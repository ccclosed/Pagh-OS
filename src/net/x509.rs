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
fn civil_to_unix(year: i64, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> u64 {
    let days = days_from_civil(year, month, day);
    (days * 86_400 + (hour as i64) * 3600 + (minute as i64) * 60 + (second as i64)) as u64
}

// ───────────────────────────── X.509 times ─────────────────────────────

/// Decode an X.509 `Validity` time into Unix seconds (UTC assumed, matching
/// `rtc::now_unix`).
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
pub fn decode_asn1_time(tag: u8, s: &[u8]) -> Result<u64, DerError> {
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
            Ok((-631_152_000i64) as u64)
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
}
