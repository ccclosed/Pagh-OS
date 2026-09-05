//! Minimal DER (ASN.1) reader + X.509 time decoding for the TLS certificate
//! verifier (issue #14 — TLS server authentication).
//!
//! This is the foundation the certificate verifier (later PRs in the issue #14
//! series) is built on: a deliberately tiny, fail-closed DER subset covering
//! exactly the shapes a TLS 1.3 server certificate chain actually carries —
//! SEQUENCE/SET containers, INTEGER/OID/strings, context tags, and the two
//! time types X.509 `Validity` uses.
//!
//! Design constraints (match the repo's pure-module convention, R11.6):
//!
//!   * **Pure `core`-only, self-contained** — no `alloc`, no sibling deps, no
//!     kernel/arch references, so this exact source is `#[path]`-included in
//!     `host-tests` and exercised by property P43 against adversarial input.
//!     The calendar math is a re-implementation of Howard Hinnant's
//!     `days_from_civil` (see also `arch/x86_64/linux/timeconv.rs`, which
//!     cannot be referenced across the subtree without breaking the standalone
//!     host include; the host property cross-checks the two).
//!   * **Fail-closed and panic-free**: every malformed input returns
//!     [`Err(DerError)`]; there is no indexing without bounds checks, no
//!     `unwrap`, no panic path. A parser that runs on untrusted network bytes
//!     inside a `panic = "abort"` kernel must never panic.
//!   * **DER, not BER**: indefinite lengths and non-minimal length encodings
//!     are rejected. Certificates are DER per RFC 5280 §4.1, and strictness
//!     here is free attack surface removal.
//!   * **No high-tag-number form** (tags ≥ 31) and no multi-byte tag parsing:
//!     nothing in an X.509 certificate path uses them; encountering one is a
//!     hard error rather than a half-supported code path.

#![allow(dead_code)] // grows over the issue #14 series; the verifier consumes it later.

// ───────────────────────────── tags ─────────────────────────────

/// Universal tag: BOOLEAN (`0x01`).
pub const TAG_BOOLEAN: u8 = 0x01;
/// Universal tag: INTEGER (`0x02`).
pub const TAG_INTEGER: u8 = 0x02;
/// Universal tag: BIT STRING (`0x03`).
pub const TAG_BIT_STRING: u8 = 0x03;
/// Universal tag: OCTET STRING (`0x04`).
pub const TAG_OCTET_STRING: u8 = 0x04;
/// Universal tag: NULL (`0x05`).
pub const TAG_NULL: u8 = 0x05;
/// Universal tag: OBJECT IDENTIFIER (`0x06`).
pub const TAG_OID: u8 = 0x06;
/// Universal tag: UTF8String (`0x0c`).
pub const TAG_UTF8_STRING: u8 = 0x0c;
/// Universal tag: PrintableString (`0x13`).
pub const TAG_PRINTABLE_STRING: u8 = 0x13;
/// Universal tag: IA5String (`0x16`).
pub const TAG_IA5_STRING: u8 = 0x16;
/// Universal tag: UTCTime (`0x17`).
pub const TAG_UTC_TIME: u8 = 0x17;
/// Universal tag: GeneralizedTime (`0x18`).
pub const TAG_GENERALIZED_TIME: u8 = 0x18;
/// Universal tag: SEQUENCE (`0x30`).
pub const TAG_SEQUENCE: u8 = 0x30;
/// Universal tag: SET (`0x31`).
pub const TAG_SET: u8 = 0x31;

/// Tag-class mask: constructed context-specific tags (`[n]`) carry `0x80`.
///
/// X.509 wraps optional certificate fields in context tags — `[0]` around the
/// explicit version, `[3]` around the extensions block — so the verifier needs
/// to recognise the class, not the universal value.
pub const CLASS_CONTEXT: u8 = 0x80;

/// Lower tag-number bits of a tag byte (the high 3 bits are class + constructed).
#[inline]
pub fn tag_number(tag: u8) -> u8 {
    tag & 0x1f
}

// ───────────────────────────── errors ─────────────────────────────

/// Everything the DER subset can reject. `PartialEq` so properties can assert
/// exact failure classes; `Copy` so it flows through small `Result`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerError {
    /// Input ended inside a tag, length, or content.
    Truncated,
    /// Length encoding is not DER: indefinite length, non-minimal long form,
    /// or an over-long (> 4-byte) length the subset does not support.
    BadLength,
    /// A different tag was required at this position.
    UnexpectedTag {
        /// Tag the caller required.
        expected: u8,
        /// Tag actually present.
        got: u8,
    },
    /// The time string is not a Z-suffixed UTCTime/GeneralizedTime of the
    /// exact RFC 5280 shape, or the calendar date it names does not exist.
    BadTime,
    /// A structural violation of the X.509 shapes themselves (wrong field
    /// order, impossible version, malformed key material) — the DER is fine
    /// but it is not a certificate the verifier accepts.
    BadCert,
}

impl core::fmt::Display for DerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            DerError::Truncated => f.write_str("der: truncated input"),
            DerError::BadLength => f.write_str("der: non-DER length encoding"),
            DerError::UnexpectedTag { expected, got } => {
                write!(f, "der: expected tag {expected:#04x}, got {got:#04x}")
            }
            DerError::BadTime => f.write_str("der: malformed ASN.1 time"),
            DerError::BadCert => f.write_str("der: not a well-formed certificate"),
        }
    }
}

// ───────────────────────────── TLV reading ─────────────────────────────

/// Parse one DER TLV at the start of `buf`.
///
/// Returns `(tag, content, rest)`: the tag byte, the exactly-sized content
/// slice, and everything after the element (which the caller may keep parsing
/// as sibling elements). Empty `buf` or any encoding violation is an error —
/// this function never panics on arbitrary bytes.
pub fn der_tlv(buf: &[u8]) -> Result<(u8, &[u8], &[u8]), DerError> {
    let tag = *buf.first().ok_or(DerError::Truncated)?;
    // Only the low-tag-number form (numbers 0..=30) is supported; the
    // high-tag-number form (first tag byte ending in 0x1f) is rejected
    // outright — see the module docs.
    if tag & 0x1f == 0x1f {
        return Err(DerError::BadLength);
    }
    let mut pos = 1usize;
    let len = read_len(buf, &mut pos)?;
    let end = pos
        .checked_add(len)
        .filter(|&end| end <= buf.len())
        .ok_or(DerError::Truncated)?;
    Ok((tag, &buf[pos..end], &buf[end..]))
}

/// Read the DER length starting at `*pos`, advancing `*pos` past it.
///
/// Enforces full DER strictness:
///   * short form for lengths < 0x80,
///   * long form in 1..=4 length bytes, no leading zero byte, and only when
///     the value actually requires it (`len >= 0x80`),
///   * no indefinite length (`0x80`).
fn read_len(buf: &[u8], pos: &mut usize) -> Result<usize, DerError> {
    let first = *buf.get(*pos).ok_or(DerError::Truncated)?;
    *pos += 1;
    if first < 0x80 {
        return Ok(first as usize);
    }
    if first == 0x80 {
        // Indefinite length is BER, not DER.
        return Err(DerError::BadLength);
    }
    let n = (first & 0x7f) as usize;
    if n > 4 {
        return Err(DerError::BadLength);
    }
    if buf.len() < *pos + n {
        return Err(DerError::Truncated);
    }
    let mut len = 0usize;
    for &b in &buf[*pos..*pos + n] {
        len = (len << 8) | b as usize;
    }
    let first_len_byte = buf[*pos];
    *pos += n;
    // DER minimality: no leading zero length byte, and the long form must not
    // be used when the short form would do.
    if first_len_byte == 0 || len < 0x80 {
        return Err(DerError::BadLength);
    }
    Ok(len)
}

/// Expect an element with exactly `tag` at the start of `buf`.
///
/// Returns the content slice and the bytes after the element — the common
/// "SEQUENCE-of" walk pattern of the certificate parser.
pub fn der_expect(buf: &[u8], tag: u8) -> Result<(&[u8], &[u8]), DerError> {
    let (got, content, rest) = der_tlv(buf)?;
    if got != tag {
        return Err(DerError::UnexpectedTag { expected: tag, got });
    }
    Ok((content, rest))
}

/// Like [`der_tlv`] but returns the WHOLE element (tag + header + content)
/// plus the rest — needed where a slice of the element itself must outlive
/// the walk (certificate `Name`s, the `TBSCertificate` signature input).
pub fn der_element(buf: &[u8]) -> Result<(&[u8], &[u8]), DerError> {
    let (_tag, _content, rest) = der_tlv(buf)?;
    let whole = &buf[..buf.len() - rest.len()];
    Ok((whole, rest))
}

/// Iterator over the sibling elements of a constructed element's content.
///
/// Stops at the first malformed element (returning `None` afterwards keeps
/// `for r in .. { r? }` ergonomic at the call site) — the certificate parser
/// owns the decision to fail closed on it.
pub struct DerChildren<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for DerChildren<'a> {
    type Item = Result<(u8, &'a [u8]), DerError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        match der_tlv(self.rest) {
            Ok((tag, content, rest)) => {
                self.rest = rest;
                Some(Ok((tag, content)))
            }
            Err(e) => {
                self.rest = &[];
                Some(Err(e))
            }
        }
    }
}

/// Iterate the children of a constructed element's `content` (e.g. the inner
/// bytes of a `SEQUENCE`).
pub fn der_children(content: &[u8]) -> DerChildren<'_> {
    DerChildren { rest: content }
}

// ───────────────────────────── calendar math ─────────────────────────────

/// Days since 1970-01-01 for the proleptic-Gregorian `(y, m, d)`.
///
/// Howard Hinnant's `days_from_civil` (same algorithm as
/// `arch/x86_64/linux/timeconv.rs::days_from_civil`; duplicated because this
/// module must stay subtree-local for the standalone host-tests include).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64; // [0, 399]
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Days in `month` of `year` (proleptic Gregorian). `month` must be in
/// `1..=12`; anything else yields 0 (which the time decoder treats as invalid).
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Convert a validated civil breakdown to Unix seconds (UTC assumed).
///
/// Signed: dates before 1970 yield negatives. The certificate verifier MUST
/// compare against `rtc::now_unix() as i64` — clamping or wrapping this into
/// an unsigned type would silently flip comparison directions (a pre-epoch
/// `not_after` wrapped into `u64` would look like "never expires").
fn civil_to_unix(year: i64, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
    days_from_civil(year, month, day) * 86_400
        + (hour as i64) * 3600
        + (minute as i64) * 60
        + (second as i64)
}

// ───────────────────────────── X.509 times ─────────────────────────────

/// Decode an X.509 `Validity` time into Unix seconds (UTC assumed).
///
/// Returns a **signed** count: pre-epoch instants (UTCTime reaches back to
/// 1950) stay exact negatives, so validity comparisons keep their direction
/// (`not_before <= now <= not_after` against `rtc::now_unix() as i64`).
///
/// Accepts exactly the RFC 5280 §4.1.2.5 shapes — this is fail-closed on
/// purpose: fractional seconds and timezone offsets are not produced by
/// conforming CAs, so anything but the canonical forms is a hard error.
///
///   * `UTCTime` (`tag == 0x17`): `"YYMMDDHHMMSSZ"` (13 ASCII bytes). The
///     two-digit year maps `YY >= 50 → 19YY`, else `20YY`.
///   * `GeneralizedTime` (`tag == 0x18`): `"YYYYMMDDHHMMSSZ"` (15 ASCII
///     bytes), four-digit year.
///
/// The calendar date is fully validated (month, day-of-month, and time-of-day
/// ranges), so e.g. `"230230120000Z"` (Feb 30) is rejected, not wrapped.
pub fn decode_asn1_time(tag: u8, s: &[u8]) -> Result<i64, DerError> {
    // ASCII digit helper: '0'..='9' only — DER times carry no sign or space.
    fn digit(b: u8) -> Option<u32> {
        if b.is_ascii_digit() {
            Some((b - b'0') as u32)
        } else {
            None
        }
    }
    // Two decimal digits -> value.
    fn two(s: &[u8]) -> Option<u32> {
        Some(digit(*s.first()?)? * 10 + digit(*s.get(1)?)?)
    }

    let (year, rest) = match tag {
        TAG_UTC_TIME => {
            if s.len() != 13 {
                return Err(DerError::BadTime);
            }
            let yy = two(&s[0..2]).ok_or(DerError::BadTime)?;
            // RFC 5280 §4.1.2.5.1: YY >= 50 is 19YY, else 20YY.
            let year = if yy >= 50 {
                1900 + yy as i64
            } else {
                2000 + yy as i64
            };
            (year, &s[2..])
        }
        TAG_GENERALIZED_TIME => {
            if s.len() != 15 {
                return Err(DerError::BadTime);
            }
            let y = digit(s[0]).ok_or(DerError::BadTime)? as i64 * 1000
                + digit(s[1]).ok_or(DerError::BadTime)? as i64 * 100
                + digit(s[2]).ok_or(DerError::BadTime)? as i64 * 10
                + digit(s[3]).ok_or(DerError::BadTime)? as i64;
            (y, &s[4..])
        }
        _ => return Err(DerError::BadTime),
    };

    // Remaining shape for both types: MMDDHHMMSSZ (11 bytes).
    if rest.len() != 11 || rest[10] != b'Z' {
        return Err(DerError::BadTime);
    }
    let month = two(&rest[0..2]).ok_or(DerError::BadTime)?;
    let day = two(&rest[2..4]).ok_or(DerError::BadTime)?;
    let hour = two(&rest[4..6]).ok_or(DerError::BadTime)?;
    let minute = two(&rest[6..8]).ok_or(DerError::BadTime)?;
    let second = two(&rest[8..10]).ok_or(DerError::BadTime)?;

    // Full calendar validation — fail closed on dates that do not exist.
    if month == 0 || day == 0 || day > days_in_month(year, month) {
        return Err(DerError::BadTime);
    }
    if hour > 23 || minute > 59 || second > 60 {
        // second == 60 tolerates the leap-second convention some CAs emit.
        return Err(DerError::BadTime);
    }

    Ok(civil_to_unix(year, month, day, hour, minute, second))
}

// ─────────────────────── X.509 certificate parsing ───────────────────────
//
// Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm,
//                            signatureValue BIT STRING }
//
// Everything below borrows from the input DER — no allocation, so parsed
// certificates live inside the handshake's existing buffers. The verifier
// (later PRs) uses: `validity` against the RTC, `spki` for chain signature
// checks, `issuer`/`subject` raw-DER equality for chain name matching, and
// `tbs` as the bytes the certificate's own signature is computed over.

/// AlgorithmIdentifier OIDs, as raw DER OID *content* bytes (comparison is a
/// plain byte slice compare — no OID re-encoding needed).
/// rsaEncryption (1.2.840.113549.1.1.1).
pub const OID_RSA_ENCRYPTION: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
/// ecPublicKey (1.2.840.10045.2.1).
pub const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
/// prime256v1 / secp256r1 (1.2.840.10045.3.1.7).
pub const OID_PRIME256V1: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
/// secp384r1 (1.3.132.0.34).
pub const OID_SECP384R1: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];
/// Ed25519 (1.3.101.112).
pub const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];

/// Extension OIDs (2.5.29.x arc).
/// subjectAltName (2.5.29.17).
pub const OID_EXT_SAN: &[u8] = &[0x55, 0x1d, 0x11];
/// basicConstraints (2.5.29.19).
pub const OID_EXT_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];

/// Bytewise OID comparison helper — reads better at the call sites than `==`.
#[inline]
pub fn oid_is(oid: &[u8], known: &[u8]) -> bool {
    oid == known
}

/// Strip the leading unused-bits byte of a BIT STRING content.
///
/// Key material must occupy whole bytes, so a non-zero unused-bits count is
/// rejected (fail closed) rather than rounded down.
pub fn bit_string_bytes(content: &[u8]) -> Result<&[u8], DerError> {
    let first = *content.first().ok_or(DerError::Truncated)?;
    if first != 0 {
        return Err(DerError::BadCert);
    }
    Ok(&content[1..])
}

/// Strip the DER sign-padding byte of a non-negative INTEGER (`0x00` prefix
/// when the high bit is set). Minimal DER allows at most one, so at most one
/// is stripped.
pub fn integer_bytes(content: &[u8]) -> Result<&[u8], DerError> {
    if content.is_empty() {
        return Err(DerError::Truncated);
    }
    if content.len() > 1 && content[0] == 0 {
        return Ok(&content[1..]);
    }
    Ok(content)
}

/// `Validity ::= SEQUENCE { notBefore Time, notAfter Time }` — decoded to
/// Unix seconds (UTC assumed). Signed: pre-epoch instants stay exact
/// negatives, so the verifier's `not_before <= now <= not_after` comparison
/// against `rtc::now_unix() as i64` keeps its direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validity {
    /// Earliest instant the certificate is valid.
    pub not_before: i64,
    /// Latest instant the certificate is valid.
    pub not_after: i64,
}

/// The public key carried by a `SubjectPublicKeyInfo`, restricted to the
/// algorithms the verifier can actually check. Anything else — including
/// well-formed-but-unknown algorithms — lands in [`SpkiKey::Unsupported`]
/// and fails the handshake later, never silently passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpkiKey<'a> {
    /// rsaEncryption: modulus (big-endian, sign-stripped) and exponent.
    Rsa { n: &'a [u8], e: &'a [u8] },
    /// ecPublicKey on prime256v1: uncompressed point (`0x04 || X || Y`, 65 B).
    EcP256 { point: &'a [u8] },
    /// ecPublicKey on secp384r1: uncompressed point (97 B).
    EcP384 { point: &'a [u8] },
    /// Ed25519: raw 32-byte public key.
    Ed25519 { key: &'a [u8] },
    /// A supported-surface gap: algorithm or curve not in the list above.
    Unsupported,
}

/// `SubjectPublicKeyInfo ::= SEQUENCE { algorithm, subjectPublicKey }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpkiRef<'a> {
    /// The AlgorithmIdentifier OID content (`alg`).
    pub alg: &'a [u8],
    /// The parsed key (curve / key type resolved from `alg` + parameters).
    pub key: SpkiKey<'a>,
}

/// A parsed X.509 certificate, borrowing from the DER buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CertificateRef<'a> {
    /// Raw DER of the whole `TBSCertificate` element — the exact bytes the
    /// certificate's own signature is computed over.
    pub tbs: &'a [u8],
    /// Serial number, sign-stripped big-endian (raw bytes; uniqueness is the
    /// CA's problem, not the verifier's).
    pub serial: &'a [u8],
    /// `tbsCertificate.signature` AlgorithmIdentifier OID content.
    pub sig_alg: &'a [u8],
    /// Issuer `Name` — raw DER element (byte equality is the chain matcher).
    pub issuer: &'a [u8],
    /// Subject `Name` — raw DER element.
    pub subject: &'a [u8],
    /// Decoded validity window.
    pub validity: Validity,
    /// Subject public key info.
    pub spki: SpkiRef<'a>,
    /// RFC 5280 version number: 1 (absent field), 2, or 3.
    pub version: u8,
}

/// Parse one `Certificate` at the start of `der`.
///
/// Returns the parsed certificate (borrowing from `der`) and the bytes after
/// the certificate element. `signatureAlgorithm` / `signatureValue` are
/// required to be present and well-formed but are not decoded here — the
/// verifier checks the signature over [`CertificateRef::tbs`] in a later PR.
pub fn parse_certificate(der: &[u8]) -> Result<(CertificateRef<'_>, &[u8]), DerError> {
    let (cert_content, rest) = der_expect(der, TAG_SEQUENCE)?;
    let (tbs_el, tail) = der_element(cert_content)?;
    let cert = parse_tbs(tbs_el)?;
    // signatureAlgorithm (SEQUENCE) and signatureValue (BIT STRING) must be
    // present: a certificate that ends after the TBS is not a certificate.
    let (_alg, tail) = der_expect(tail, TAG_SEQUENCE)?;
    let (_sig, tail) = der_expect(tail, TAG_BIT_STRING)?;
    Ok((cert, tail))
}

/// Parse the `TBSCertificate` element (tag + header included — `tbs` is kept
/// verbatim as the signature input).
fn parse_tbs(tbs: &[u8]) -> Result<CertificateRef<'_>, DerError> {
    let (content, tail) = der_expect(tbs, TAG_SEQUENCE)?;
    if !tail.is_empty() {
        return Err(DerError::BadCert);
    }
    let mut cur = content;

    // version [0] EXPLICIT INTEGER DEFAULT v1 — absent means v1.
    let mut version = 1u8;
    if let Ok((tag, inner, rest)) = der_tlv(cur) {
        if tag == 0xA0 {
            let (v, _) = der_expect(inner, TAG_INTEGER)?;
            version = version_from_der(v)?;
            cur = rest;
        }
    }

    // serialNumber INTEGER.
    let (serial_el, rest) = der_element(cur)?;
    let (serial, _) = der_expect(serial_el, TAG_INTEGER)?;
    let serial = integer_bytes(serial)?;
    cur = rest;

    // signature AlgorithmIdentifier — keep the OID; parameters ignored.
    let (sig_el, rest) = der_element(cur)?;
    let (sig_content, _) = der_expect(sig_el, TAG_SEQUENCE)?;
    let (sig_alg, _) = der_expect(sig_content, TAG_OID)?;
    cur = rest;

    // issuer Name — raw DER element for chain name matching.
    let (issuer, rest) = der_element(cur)?;
    expect_tag(issuer, TAG_SEQUENCE)?;
    cur = rest;

    // validity SEQUENCE { notBefore, notAfter }.
    let (validity_el, rest) = der_element(cur)?;
    let (v_content, _) = der_expect(validity_el, TAG_SEQUENCE)?;
    let mut v = der_children(v_content);
    let (t1, c1) = v.next().ok_or(DerError::BadCert)??;
    let (t2, c2) = v.next().ok_or(DerError::BadCert)??;
    let validity = Validity {
        not_before: decode_asn1_time(t1, c1)?,
        not_after: decode_asn1_time(t2, c2)?,
    };
    cur = rest;

    // subject Name.
    let (subject, rest) = der_element(cur)?;
    expect_tag(subject, TAG_SEQUENCE)?;
    cur = rest;

    // subjectPublicKeyInfo.
    let (spki_el, _rest) = der_element(cur)?;
    let spki = parse_spki(spki_el)?;

    Ok(CertificateRef {
        tbs,
        serial,
        sig_alg,
        issuer,
        subject,
        validity,
        spki,
        version,
    })
    // Optional issuerUniqueID [1] / subjectUniqueID [2] / extensions [3] are
    // NOT consumed here: `find_extension` re-walks `tbs` on demand, keeping
    // this parser allocation-free and the hot path short.
}

/// Require `buf` to start with exactly `tag` (whole-element check).
fn expect_tag(buf: &[u8], tag: u8) -> Result<(), DerError> {
    let (got, _, _) = der_tlv(buf)?;
    if got != tag {
        return Err(DerError::UnexpectedTag { expected: tag, got });
    }
    Ok(())
}

/// Map the `[0]`-wrapped version INTEGER to the RFC 5280 version number.
/// Encoded values are 0..=2 for v1..=v3; anything else fails closed.
fn version_from_der(content: &[u8]) -> Result<u8, DerError> {
    match integer_bytes(content)? {
        [0x00] => Ok(1),
        [0x01] => Ok(2),
        [0x02] => Ok(3),
        _ => Err(DerError::BadCert),
    }
}

/// Parse a `SubjectPublicKeyInfo` element.
fn parse_spki(spki_el: &[u8]) -> Result<SpkiRef<'_>, DerError> {
    let (content, _) = der_expect(spki_el, TAG_SEQUENCE)?;
    // alg_el must be the WHOLE algorithm element (der_element, not the
    // content-returning der_expect) — its inner OID is parsed next.
    let (alg_el, key_el) = der_element(content)?;
    let (alg_content, _) = der_expect(alg_el, TAG_SEQUENCE)?;
    let (alg_oid, params) = der_expect(alg_content, TAG_OID)?;
    let (bits_content, _) = der_expect(key_el, TAG_BIT_STRING)?;
    let raw = bit_string_bytes(bits_content)?;

    let key = if oid_is(alg_oid, OID_RSA_ENCRYPTION) {
        // RSAPublicKey ::= SEQUENCE { modulus INTEGER, publicExponent INTEGER }
        let (pk_content, _) = der_expect(raw, TAG_SEQUENCE)?;
        let mut it = der_children(pk_content);
        let (t, c) = it.next().ok_or(DerError::BadCert)??;
        if t != TAG_INTEGER {
            return Err(DerError::UnexpectedTag {
                expected: TAG_INTEGER,
                got: t,
            });
        }
        let n = integer_bytes(c)?;
        let (t, c) = it.next().ok_or(DerError::BadCert)??;
        if t != TAG_INTEGER {
            return Err(DerError::UnexpectedTag {
                expected: TAG_INTEGER,
                got: t,
            });
        }
        let e = integer_bytes(c)?;
        if n.is_empty() || e.is_empty() {
            return Err(DerError::BadCert);
        }
        SpkiKey::Rsa { n, e }
    } else if oid_is(alg_oid, OID_EC_PUBLIC_KEY) {
        // parameters carry the namedCurve OID.
        let (curve, _) = der_expect(params, TAG_OID)?;
        if oid_is(curve, OID_PRIME256V1) {
            if raw.len() != 65 || raw[0] != 0x04 {
                return Err(DerError::BadCert);
            }
            SpkiKey::EcP256 { point: raw }
        } else if oid_is(curve, OID_SECP384R1) {
            if raw.len() != 97 || raw[0] != 0x04 {
                return Err(DerError::BadCert);
            }
            SpkiKey::EcP384 { point: raw }
        } else {
            SpkiKey::Unsupported
        }
    } else if oid_is(alg_oid, OID_ED25519) {
        if raw.len() != 32 {
            return Err(DerError::BadCert);
        }
        SpkiKey::Ed25519 { key: raw }
    } else {
        SpkiKey::Unsupported
    };

    Ok(SpkiRef { alg: alg_oid, key })
}

/// Look up an extension by OID in a parsed certificate.
///
/// Returns the *content* of the `extnValue` OCTET STRING (e.g. for SAN the
/// DER `GeneralNames`, for basicConstraints the DER `SEQUENCE`). Re-walks
/// [`CertificateRef::tbs`] on demand — the walk is cheap and the borrowed
/// result needs no storage in the certificate struct.
pub fn find_extension<'a>(
    cert: &CertificateRef<'a>,
    oid: &[u8],
) -> Result<Option<&'a [u8]>, DerError> {
    let (tbs_content, _) = der_expect(cert.tbs, TAG_SEQUENCE)?;
    let mut cur = tbs_content;

    // Skip version if present ([0] EXPLICIT, tag 0xA0).
    if let Ok((0xA0, _inner, rest)) = der_tlv(cur) {
        cur = rest;
    }
    // Skip the six REQUIRED fields: serial, signature, issuer, validity,
    // subject, subjectPublicKeyInfo. A missing one is a BadCert — it could
    // not have parsed in `parse_tbs`.
    for _ in 0..6 {
        let (_el, rest) = der_element(cur)?;
        cur = rest;
    }

    // Optional uniqueIDs then extensions; an EMPTY tail means the certificate
    // ends after the SPKI (common for v1) — no extensions, not an error.
    if cur.is_empty() {
        return Ok(None);
    }
    while !cur.is_empty() {
        let (tag, _c, rest) = der_tlv(cur)?;
        if tag == 0x81 || tag == 0x82 {
            cur = rest;
        } else {
            break;
        }
    }
    if cur.is_empty() {
        return Ok(None);
    }
    let (tag, inner, _rest) = der_tlv(cur)?;
    if tag != 0xA3 {
        return Ok(None);
    }
    let (ext_seq, _) = der_expect(inner, TAG_SEQUENCE)?;
    for ext in der_children(ext_seq) {
        let (tag, ext_content) = ext?;
        if tag != TAG_SEQUENCE {
            return Err(DerError::UnexpectedTag {
                expected: TAG_SEQUENCE,
                got: tag,
            });
        }
        let (ext_oid, rest) = der_expect(ext_content, TAG_OID)?;
        if oid_is(ext_oid, oid) {
            // critical BOOLEAN DEFAULT FALSE is optional.
            let mut r = rest;
            if let Ok((TAG_BOOLEAN, _c, rest2)) = der_tlv(r) {
                let _ = _c;
                r = rest2;
            }
            let (octet, _) = der_expect(r, TAG_OCTET_STRING)?;
            return Ok(Some(octet));
        }
    }
    Ok(None)
}

/// One `GeneralName` of a `subjectAltName` extension. Everything the verifier
/// cannot match (rfc822, URI, otherName, ...) collapses to [`San::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum San<'a> {
    /// dNSName `[2]` IA5String — ASCII hostname content.
    Dns(&'a [u8]),
    /// iPAddress `[7]` OCTET STRING — 4 (IPv4) or 16 (IPv6) bytes.
    Ip(&'a [u8]),
    /// Any other GeneralName — ignored by hostname verification.
    Other,
}

/// Iterator over the `GeneralNames` of a SAN extension value.
pub struct SanIter<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for SanIter<'a> {
    type Item = Result<San<'a>, DerError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        Some(match der_tlv(self.rest) {
            Ok((tag, content, rest)) => {
                self.rest = rest;
                match tag {
                    // [2] dNSName (context, primitive) / [7] iPAddress.
                    0x82 => Ok(San::Dns(content)),
                    0x87 => Ok(San::Ip(content)),
                    _ => Ok(San::Other),
                }
            }
            Err(e) => {
                self.rest = &[];
                Err(e)
            }
        })
    }
}

/// Iterate the `GeneralNames` of a SAN `extnValue` OCTET STRING content.
///
/// The value is a DER `SEQUENCE OF GeneralName`, so this descends into the
/// sequence first; a malformed container is an error, not an empty iterator.
pub fn san_names(san_octets: &[u8]) -> Result<SanIter<'_>, DerError> {
    let (content, tail) = der_expect(san_octets, TAG_SEQUENCE)?;
    if !tail.is_empty() {
        return Err(DerError::BadCert);
    }
    Ok(SanIter { rest: content })
}

/// Parse a `basicConstraints` extnValue OCTET STRING content:
/// `SEQUENCE { cA BOOLEAN DEFAULT FALSE, pathLenConstraint INTEGER OPTIONAL }`.
///
/// Returns `is_ca`. DER omits a defaulted FALSE, so an empty sequence is
/// `false`; anything but a leading BOOLEAN (when present) fails closed.
pub fn parse_basic_constraints(der: &[u8]) -> Result<bool, DerError> {
    let (content, _) = der_expect(der, TAG_SEQUENCE)?;
    if content.is_empty() {
        return Ok(false);
    }
    let (tag, c) = der_tlv(content).map(|(t, c, _)| (t, c))?;
    if tag != TAG_BOOLEAN {
        return Err(DerError::UnexpectedTag {
            expected: TAG_BOOLEAN,
            got: tag,
        });
    }
    // DER: FALSE is 0x00, TRUE is 0xFF; anything else is not DER.
    match c {
        [0x00] => Ok(false),
        [0xff] => Ok(true),
        _ => Err(DerError::BadCert),
    }
}

// ───────────────────────────── self-tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a TLV with proper minimal DER length (test-side helper).
    fn tlv(tag: u8, content: &[u8]) -> alloc::vec::Vec<u8> {
        let mut out = alloc::vec::Vec::new();
        out.push(tag);
        let l = content.len();
        if l < 0x80 {
            out.push(l as u8);
        } else {
            let bytes = l.to_be_bytes();
            let skip = bytes.iter().take_while(|&&b| b == 0).count();
            out.push(0x80 | (8 - skip) as u8);
            out.extend_from_slice(&bytes[skip..]);
        }
        out.extend_from_slice(content);
        out
    }

    #[test]
    fn known_vector_2025_epoch() {
        // 2025-01-01T00:00:00Z == 1735689600.
        assert_eq!(
            decode_asn1_time(TAG_UTC_TIME, b"250101000000Z"),
            Ok(1_735_689_600)
        );
        assert_eq!(
            decode_asn1_time(TAG_GENERALIZED_TIME, b"20250101000000Z"),
            Ok(1_735_689_600)
        );
    }

    #[test]
    fn utctime_year_pivot() {
        // RFC 5280: YY >= 50 -> 19YY, else 20YY. 2049-01-01 == 2493072000.
        assert_eq!(
            decode_asn1_time(TAG_UTC_TIME, b"490101000000Z"),
            Ok(2_493_072_000)
        );
        // 1950-01-01 is before the epoch; the conversion is exact (negative
        // days_from_civil) and lands at the known pre-epoch value.
        assert_eq!(
            decode_asn1_time(TAG_UTC_TIME, b"500101000000Z"),
            Ok(-631_152_000)
        );
    }

    #[test]
    fn rejects_impossible_dates_and_shapes() {
        // Feb 30 does not exist.
        assert_eq!(
            decode_asn1_time(TAG_UTC_TIME, b"230230120000Z"),
            Err(DerError::BadTime)
        );
        // Hour 24.
        assert_eq!(
            decode_asn1_time(TAG_UTC_TIME, b"230101240000Z"),
            Err(DerError::BadTime)
        );
        // Missing seconds (12-char shape).
        assert_eq!(
            decode_asn1_time(TAG_UTC_TIME, b"2301010000Z"),
            Err(DerError::BadTime)
        );
        // No Z suffix.
        assert_eq!(
            decode_asn1_time(TAG_UTC_TIME, b"230101000000+01"),
            Err(DerError::BadTime)
        );
        // Fractional GeneralizedTime is rejected (not the canonical shape).
        assert_eq!(
            decode_asn1_time(TAG_GENERALIZED_TIME, b"20250101000000.123Z"),
            Err(DerError::BadTime)
        );
        // Wrong tag class.
        assert_eq!(
            decode_asn1_time(TAG_SEQUENCE, b"230101000000Z"),
            Err(DerError::BadTime)
        );
        // Non-digit garbage.
        assert_eq!(
            decode_asn1_time(TAG_UTC_TIME, b"2X0101000000Z"),
            Err(DerError::BadTime)
        );
    }

    #[test]
    fn rejects_non_minimal_lengths() {
        // Tag then indefinite length (BER, not DER).
        assert_eq!(der_tlv(&[0x30, 0x80]), Err(DerError::BadLength));
        // Long form for a value that fits the short form.
        assert_eq!(der_tlv(&[0x30, 0x81, 0x05]), Err(DerError::BadLength));
        // Leading zero in the length.
        assert_eq!(der_tlv(&[0x30, 0x82, 0x00, 0x80]), Err(DerError::BadLength));
        // Truncated length bytes.
        assert_eq!(der_tlv(&[0x30, 0x82, 0x01]), Err(DerError::Truncated));
        // Content longer than the buffer.
        assert_eq!(der_tlv(&[0x30, 0x05, 0x01, 0x02]), Err(DerError::Truncated));
        // High-tag-number form unsupported.
        assert_eq!(der_tlv(&[0x5f, 0x1d, 0x00]), Err(DerError::BadLength));
        // Empty input.
        assert_eq!(der_tlv(&[]), Err(DerError::Truncated));
    }

    #[test]
    fn tlv_and_children_walk() {
        // SEQUENCE { SEQUENCE { INTEGER 1 } INTEGER 7 } + trailing sibling.
        let inner = tlv(TAG_INTEGER, &[0x01]);
        let outer_content = [
            tlv(TAG_SEQUENCE, &inner).as_slice(),
            &tlv(TAG_INTEGER, &[0x07])[..],
        ]
        .concat();
        let mut whole = tlv(TAG_SEQUENCE, &outer_content);
        whole.extend_from_slice(&tlv(TAG_BOOLEAN, &[0xff]));

        let (tag, content, rest) = der_tlv(&whole).unwrap();
        assert_eq!(tag, TAG_SEQUENCE);
        assert_eq!(content, outer_content.as_slice());
        assert_eq!(rest, tlv(TAG_BOOLEAN, &[0xff]).as_slice());

        let children: alloc::vec::Vec<(u8, &[u8])> =
            der_children(content).map(Result::unwrap).collect();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].0, TAG_SEQUENCE);
        assert_eq!(children[1].0, TAG_INTEGER);
        assert_eq!(children[1].1, &[0x07]);
    }

    /// Minimal v1 certificate: empty names, RSA SPKI, no extensions.
    /// Exercises the whole `parse_certificate` walk (P44 builds on this with
    /// generated certificates and real-world fixtures).
    #[test]
    fn parse_minimal_v1_certificate() {
        // BIT STRING content = 0x00 unused-bits + DER RSAPublicKey.
        let rsa_pk = tlv(
            TAG_SEQUENCE,
            &[
                tlv(TAG_INTEGER, &[0x00, 0xc1]).as_slice(), // n (sign-padded 0xc1)
                tlv(TAG_INTEGER, &[0x01, 0x00, 0x01]).as_slice(), // e = 65537
            ]
            .concat(),
        );
        let mut bits = alloc::vec::Vec::new();
        bits.push(0x00);
        bits.extend_from_slice(&rsa_pk);
        let spki = tlv(
            TAG_SEQUENCE,
            &[
                tlv(
                    TAG_SEQUENCE,
                    &[
                        tlv(TAG_OID, OID_RSA_ENCRYPTION).as_slice(),
                        tlv(TAG_NULL, &[]).as_slice(),
                    ]
                    .concat(),
                )
                .as_slice(),
                tlv(TAG_BIT_STRING, &bits).as_slice(),
            ]
            .concat(),
        );
        let name = tlv(TAG_SEQUENCE, &[]);
        let tbs = tlv(
            TAG_SEQUENCE,
            &[
                tlv(TAG_INTEGER, &[0x2a]).as_slice(), // serial 42
                tlv(
                    TAG_SEQUENCE,
                    &[
                        tlv(TAG_OID, OID_RSA_ENCRYPTION).as_slice(),
                        tlv(TAG_NULL, &[]).as_slice(),
                    ]
                    .concat(),
                )
                .as_slice(),
                name.as_slice(), // issuer
                tlv(
                    TAG_SEQUENCE,
                    &[
                        tlv(TAG_UTC_TIME, b"250101000000Z").as_slice(),
                        tlv(TAG_UTC_TIME, b"260101000000Z").as_slice(),
                    ]
                    .concat(),
                )
                .as_slice(),
                name.as_slice(), // subject
                spki.as_slice(),
            ]
            .concat(),
        );
        let cert_der = tlv(
            TAG_SEQUENCE,
            &[
                tbs.as_slice(),
                tlv(
                    TAG_SEQUENCE,
                    &[
                        tlv(TAG_OID, OID_RSA_ENCRYPTION).as_slice(),
                        tlv(TAG_NULL, &[]).as_slice(),
                    ]
                    .concat(),
                )
                .as_slice(),
                tlv(TAG_BIT_STRING, &[0x00, 0xde, 0xad]).as_slice(), // dummy signature
            ]
            .concat(),
        );

        let (cert, rest) = parse_certificate(&cert_der).unwrap();
        assert!(rest.is_empty());
        assert_eq!(cert.version, 1);
        assert_eq!(cert.serial, &[0x2a]);
        assert!(oid_is(cert.sig_alg, OID_RSA_ENCRYPTION));
        assert_eq!(cert.issuer, name.as_slice());
        assert_eq!(cert.subject, name.as_slice());
        assert_eq!(cert.validity.not_before, 1_735_689_600);
        assert_eq!(cert.validity.not_after, 1_767_225_600);
        match cert.spki.key {
            SpkiKey::Rsa { n, e } => {
                // Sign padding stripped: n = 0xc1, e = 65537 big-endian.
                assert_eq!(n, &[0xc1]);
                assert_eq!(e, &[0x01, 0x00, 0x01]);
            }
            _ => panic!("expected RSA SPKI"),
        }
        // No extensions field at all.
        assert_eq!(find_extension(&cert, OID_EXT_SAN), Ok(None));

        // Every truncation fails closed.
        for cut in 0..cert_der.len() {
            assert!(parse_certificate(&cert_der[..cut]).is_err());
        }
    }
}
