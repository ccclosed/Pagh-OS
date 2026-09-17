//! Pure OpenPGP *packet* layer: ASCII armor, packet framing, public keys,
//! signatures and the `Release.gpg`/`InRelease` clearsign framing (issue #32).
//!
//! `core` + `alloc` only, no globals, no hardware — the kernel compiles this
//! source for the bare-metal target and `host-tests` `#[path]`-includes it, so
//! the properties P51/P52 run against the same bytes the kernel will parse.
//!
//! Everything here is **panic-free and bounded**: every read is bounds-checked,
//! every length is capped ([`MAX_ARMOR_BYTES`], [`MAX_PACKETS`],
//! [`MAX_PACKET_BODY`], [`MAX_SIGNATURES`], [`MAX_KEY_BLOCK_BYTES`]), and
//! malformed input produces an [`PacketError`] rather than a panic. That matters
//! twice over: `panic = "abort"` kills the machine, and this parser eats bytes
//! served by a mirror we do not trust yet.
//!
//! Only the packet tags the verifier needs are interpreted (6 public key, 14
//! public subkey, 13 user ID, 2 signature); every other tag is skipped by
//! length. Partial body lengths and indeterminate lengths are **refused** rather
//! than reassembled — real Debian signatures and our pinned keyring blocks use
//! definite lengths only, and refusing keeps the parser free of an incremental
//! buffering path that would need its own memory bound.
//!
//! Parsing is deliberately tolerant of *unsupported* algorithms (they are
//! recorded and refused by the policy layer) but strict about *framing*: a
//! truncated packet, a v5/v6 key, or a signature packet with trailing bytes is
//! an error, never a silent skip — the pinned keyring and the mirror's signature
//! blob are both attacker-adjacent inputs.

#![allow(dead_code)]

use alloc::vec::Vec;

/// Maximum armored blob accepted (real `Release.gpg` is ~1.7 KiB).
pub const MAX_ARMOR_BYTES: usize = 64 * 1024;
/// Maximum armor line length (base64 payload 64 + `\r` + slack).
pub const MAX_ARMOR_LINE: usize = 80;
/// Maximum packets walked in one blob.
pub const MAX_PACKETS: usize = 256;
/// Maximum single packet body.
pub const MAX_PACKET_BODY: usize = 64 * 1024;
/// Maximum signature packets in one signed blob (live Debian `stable` has 3).
pub const MAX_SIGNATURES: usize = 16;
/// Maximum subpackets parsed from one signature area.
pub const MAX_SUBPACKETS: usize = 64;
/// Maximum bytes of a pinned keyring block.
pub const MAX_KEY_BLOCK_BYTES: usize = 96 * 1024;
/// Maximum clear-signed body accepted (live `stable` `InRelease` is ~140 KiB).
pub const MAX_CLEARSIGN_BYTES: usize = 4 * 1024 * 1024;

/// Parsing/refusal cause for armor, packets and key blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketError {
    /// The blob exceeds [`MAX_ARMOR_BYTES`] (or a bound named in the variant).
    TooLarge,
    /// No `-----BEGIN PGP SIGNATURE-----` line was found.
    ArmorMissing,
    /// An armor line exceeded [`MAX_ARMOR_LINE`].
    ArmorLineTooLong,
    /// Armor structure is broken (no blank line after the headers, no END line,
    /// unexpected characters).
    ArmorMalformed,
    /// Base64 payload is not valid base64 (bad alphabet or padding).
    ArmorBase64,
    /// The CRC24 trailer does not match the decoded payload.
    ArmorCrc,
    /// Bytes follow `-----END PGP SIGNATURE-----` that are not whitespace.
    ArmorTrailingData,
    /// The blob does not start with a packet header octet.
    NotAPacket,
    /// A packet header/body runs past the end of the buffer.
    Truncated,
    /// Partial body lengths are not supported (deliberate: see the module docs).
    PartialLength,
    /// Old-format indeterminate length (length type 3) is not supported.
    IndeterminateLength,
    /// More than [`MAX_PACKETS`] packets.
    TooManyPackets,
    /// A packet body exceeds [`MAX_PACKET_BODY`].
    PacketTooLarge,
    /// Packet/key/signature version other than 4.
    UnsupportedVersion(u8),
    /// Public-key algorithm this verifier does not implement.
    UnsupportedAlgo(u8),
    /// Key material the parser cannot interpret (wrong MPI count, bad OID,
    /// wrong point encoding, key material longer than the algorithm allows).
    MalformedKey,
    /// Signature packet structurally invalid (truncated subpacket, leftover
    /// bytes, too many MPIs).
    MalformedSignature,
    /// More than [`MAX_SIGNATURES`] signature packets.
    TooManySignatures,
    /// The blob carried no signature packet at all.
    NoSignaturePacket,
    /// A signature subpacket area is malformed or has too many entries.
    MalformedSubpackets,
    /// The keyring block is not one primary key followed by its own material.
    KeyBlockMalformed,
    /// The keyring block exceeds [`MAX_KEY_BLOCK_BYTES`].
    KeyBlockTooLarge,
    /// No public-key packet at the start of a keyring block.
    MissingPrimaryKey,
    /// Clear-signed framing is malformed.
    ClearsignMalformed,
}

/// A parsed armor header (`Key: value`, ASCII keys as in RFC 9580 §6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmorHeader {
    /// Header name, e.g. `Hash`.
    pub key: Vec<u8>,
    /// Header value, e.g. `SHA256`.
    pub value: Vec<u8>,
}

/// One parsed packet: its tag plus its body (header stripped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    /// OpenPGP packet tag (6 public key, 14 public subkey, 13 user ID, 2 signature).
    pub tag: u8,
    /// Packet body, borrowed from the input.
    pub body: &'a [u8],
}

/// Iterator over the packets of a binary OpenPGP blob.
pub struct Packets<'a> {
    buf: &'a [u8],
    pos: usize,
    count: usize,
    failed: bool,
}

impl<'a> Iterator for Packets<'a> {
    type Item = Result<Packet<'a>, PacketError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos >= self.buf.len() {
            return None;
        }
        if self.count >= MAX_PACKETS {
            self.failed = true;
            return Some(Err(PacketError::TooManyPackets));
        }
        match read_packet(self.buf, &mut self.pos) {
            Ok(pkt) => {
                self.count += 1;
                Some(Ok(pkt))
            }
            Err(e) => {
                self.failed = true;
                Some(Err(e))
            }
        }
    }
}

/// Walk the packets of a binary OpenPGP blob.
pub fn packets(buf: &[u8]) -> Packets<'_> {
    Packets {
        buf,
        pos: 0,
        count: 0,
        failed: false,
    }
}

fn read_packet<'a>(buf: &'a [u8], pos: &mut usize) -> Result<Packet<'a>, PacketError> {
    let ctb = *buf.get(*pos).ok_or(PacketError::Truncated)?;
    if ctb & 0x80 == 0 {
        return Err(PacketError::NotAPacket);
    }
    *pos += 1;

    let (tag, len) = if ctb & 0x40 != 0 {
        // New-format header (RFC 9580 §4.2).
        let tag = ctb & 0x3F;
        let first = *buf.get(*pos).ok_or(PacketError::Truncated)?;
        *pos += 1;
        let len = match first {
            0..=191 => first as usize,
            192..=223 => {
                let second = *buf.get(*pos).ok_or(PacketError::Truncated)?;
                *pos += 1;
                ((first as usize - 192) << 8) + second as usize + 192
            }
            224..=254 => return Err(PacketError::PartialLength),
            _ => {
                let bytes = take(buf, pos, 4)?;
                u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
            }
        };
        (tag, len)
    } else {
        // Old-format header (RFC 4880 §4.2).
        let tag = (ctb >> 2) & 0x0F;
        let len = match ctb & 0x03 {
            0 => take(buf, pos, 1)?[0] as usize,
            1 => {
                let b = take(buf, pos, 2)?;
                u16::from_be_bytes([b[0], b[1]]) as usize
            }
            2 => {
                let b = take(buf, pos, 4)?;
                u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize
            }
            _ => return Err(PacketError::IndeterminateLength),
        };
        (tag, len)
    };

    if len > MAX_PACKET_BODY {
        return Err(PacketError::PacketTooLarge);
    }
    let body = take(buf, pos, len)?;
    Ok(Packet { tag, body })
}

fn take<'a>(buf: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], PacketError> {
    let end = pos.checked_add(n).ok_or(PacketError::Truncated)?;
    let slice = buf.get(*pos..end).ok_or(PacketError::Truncated)?;
    *pos = end;
    Ok(slice)
}

fn read_u8(buf: &[u8], pos: &mut usize) -> Result<u8, PacketError> {
    Ok(take(buf, pos, 1)?[0])
}

fn read_u16(buf: &[u8], pos: &mut usize) -> Result<u16, PacketError> {
    let b = take(buf, pos, 2)?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32, PacketError> {
    let b = take(buf, pos, 4)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

// ── ASCII armor ──────────────────────────────────────────────────────────────

/// CRC-24 used by OpenPGP armor (RFC 9580 §6.1), initial value `0xB704CE`.
pub fn crc24(data: &[u8]) -> [u8; 3] {
    let mut crc: u32 = 0x00B7_04CE;
    for &b in data {
        crc ^= (b as u32) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x0100_0000 != 0 {
                crc ^= 0x0186_4CFB;
            }
        }
    }
    let v = crc & 0x00FF_FFFF;
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

fn trim_ascii(line: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = line.len();
    while start < end && line[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && line[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &line[start..end]
}

fn is_blank(line: &[u8]) -> bool {
    trim_ascii(line).is_empty()
}

fn b64_value(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Strict base64 decode for armored payloads (`=` padding honoured, any other
/// byte rejected).
fn base64_decode(body: &[u8], out: &mut Vec<u8>) -> Result<(), PacketError> {
    let mut acc: u32 = 0;
    let mut n: u32 = 0;
    let mut pad: u32 = 0;
    for &b in body {
        if b == b'=' {
            pad += 1;
            if pad > 2 {
                return Err(PacketError::ArmorBase64);
            }
            continue;
        }
        if pad > 0 {
            // Padding must be the tail of the payload.
            return Err(PacketError::ArmorBase64);
        }
        let v = b64_value(b).ok_or(PacketError::ArmorBase64)?;
        acc = (acc << 6) | v as u32;
        n += 1;
        if n == 4 {
            out.push((acc >> 16) as u8);
            out.push((acc >> 8) as u8);
            out.push(acc as u8);
            acc = 0;
            n = 0;
        }
    }
    match (n, pad) {
        (0, 0) => {}
        (2, 2) => out.push((acc >> 4) as u8),
        (3, 1) => {
            out.push((acc >> 10) as u8);
            out.push((acc >> 2) as u8);
        }
        _ => return Err(PacketError::ArmorBase64),
    }
    Ok(())
}

/// The result of de-armoring: the binary packet stream plus the armor headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Armored {
    /// Headers found before the blank line (`Hash:`, `Version:`, `Comment:` …).
    pub headers: Vec<ArmorHeader>,
    /// Decoded binary payload.
    pub data: Vec<u8>,
}

/// Decode one `-----BEGIN PGP SIGNATURE-----` armor block.
///
/// The CRC24 trailer is verified when present (GnuPG refuses a bad one, and a
/// corrupted blob is indistinguishable from an edited one).
pub fn dearmor(input: &[u8]) -> Result<Armored, PacketError> {
    if input.len() > MAX_ARMOR_BYTES {
        return Err(PacketError::TooLarge);
    }

    const BEGIN: &[u8] = b"-----BEGIN PGP SIGNATURE-----";
    const END: &[u8] = b"-----END PGP SIGNATURE-----";

    let mut headers: Vec<ArmorHeader> = Vec::new();
    let mut payload: Vec<u8> = Vec::new();
    let mut crc: Option<[u8; 3]> = None;
    let mut state = 0u8; // 0 = seeking BEGIN, 1 = headers, 2 = payload, 3 = done
    let mut saw_end = false;

    for line in input.split(|&b| b == b'\n') {
        if line.len() > MAX_ARMOR_LINE {
            return Err(PacketError::ArmorLineTooLong);
        }
        let line = trim_ascii(line);
        match state {
            0 => {
                if line == BEGIN {
                    state = 1;
                } else if !line.is_empty() {
                    return Err(PacketError::ArmorMissing);
                }
            }
            1 => {
                if line.is_empty() {
                    state = 2;
                    continue;
                }
                if line == BEGIN {
                    return Err(PacketError::ArmorMalformed);
                }
                let colon = line
                    .iter()
                    .position(|&b| b == b':')
                    .ok_or(PacketError::ArmorMalformed)?;
                let key = trim_ascii(&line[..colon]);
                let value = trim_ascii(&line[colon + 1..]);
                if key.is_empty() {
                    return Err(PacketError::ArmorMalformed);
                }
                headers.push(ArmorHeader {
                    key: key.to_vec(),
                    value: value.to_vec(),
                });
            }
            2 => {
                if line == END {
                    state = 3;
                    saw_end = true;
                    continue;
                }
                if line.is_empty() {
                    continue;
                }
                if line[0] == b'=' {
                    if crc.is_some() {
                        return Err(PacketError::ArmorMalformed);
                    }
                    if line.len() != 5 {
                        return Err(PacketError::ArmorMalformed);
                    }
                    let mut raw: Vec<u8> = Vec::with_capacity(3);
                    base64_decode(&line[1..], &mut raw)?;
                    if raw.len() != 3 {
                        return Err(PacketError::ArmorMalformed);
                    }
                    crc = Some([raw[0], raw[1], raw[2]]);
                    continue;
                }
                base64_decode(line, &mut payload)?;
            }
            _ => {
                if !line.is_empty() {
                    return Err(PacketError::ArmorTrailingData);
                }
            }
        }
    }

    if !saw_end {
        return Err(if state == 0 {
            PacketError::ArmorMissing
        } else {
            PacketError::ArmorMalformed
        });
    }
    if let Some(expected) = crc {
        if crc24(&payload) != expected {
            return Err(PacketError::ArmorCrc);
        }
    }
    Ok(Armored {
        headers,
        data: payload,
    })
}

/// Decode a signature blob that is *either* armored or already a binary packet
/// stream. Debian serves `Release.gpg` armored; accepting the binary form costs
/// nothing and keeps `gpg --output -` style inputs working.
pub fn dearmor_or_binary(input: &[u8]) -> Result<Vec<u8>, PacketError> {
    let first = input.iter().find(|b| !b.is_ascii_whitespace());
    match first {
        None => Err(PacketError::ArmorMissing),
        Some(&b) if b & 0x80 != 0 => {
            if input.len() > MAX_ARMOR_BYTES {
                return Err(PacketError::TooLarge);
            }
            Ok(input.to_vec())
        }
        Some(_) => Ok(dearmor(input)?.data),
    }
}

// ── MPIs ─────────────────────────────────────────────────────────────────────

/// One multiprecision integer as transmitted by OpenPGP: a big-endian magnitude
/// whose leading zero octets are stripped (so consumers must left-pad).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mpi<'a> {
    /// Declared bit length.
    pub bits: u16,
    /// Magnitude without leading zero octets.
    pub bytes: &'a [u8],
}

fn read_mpi<'a>(buf: &'a [u8], pos: &mut usize) -> Result<Mpi<'a>, PacketError> {
    let bits = read_u16(buf, pos)?;
    let nbytes = (bits as usize + 7) / 8;
    // An 8192-bit MPI is the largest the algorithm tables allow; anything bigger
    // is malformed for our purposes and refused before it can be sized.
    if nbytes > 1024 {
        return Err(PacketError::MalformedKey);
    }
    let raw = take(buf, pos, nbytes)?;
    let mut i = 0;
    while i < raw.len() && raw[i] == 0 {
        i += 1;
    }
    Ok(Mpi {
        bits,
        bytes: &raw[i..],
    })
}

// ── Public keys ──────────────────────────────────────────────────────────────

/// Named curves the ECDSA path supports (OpenPGP ECDSA public-key algo 19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Curve {
    /// NIST P-256 (`1.2.840.10045.3.1.7`).
    P256,
    /// NIST P-384 (`1.3.132.0.34`).
    P384,
    /// NIST P-521 (`1.3.132.0.35`).
    P521,
}

/// Public-key algorithm identifiers the verifier knows (RFC 9580 §9.1).
pub const ALGO_RSA: u8 = 1;
/// RSA (sign-only) — same key material layout as [`ALGO_RSA`].
pub const ALGO_RSA_SIGN: u8 = 3;
/// ECDSA with a named curve.
pub const ALGO_ECDSA: u8 = 19;
/// EdDSA (Ed25519 is the only leg implemented).
pub const ALGO_EDDSA: u8 = 22;

/// Legacy GnuPG Ed25519 OID (`1.3.6.1.4.1.11591.15.1`). Both Debian release
/// keys use this form, *not* the RFC 8410 one.
pub const OID_ED25519_LEGACY: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0xDA, 0x47, 0x0F, 0x01];
/// RFC 8410 Ed25519 OID (`1.3.101.112`).
pub const OID_ED25519: &[u8] = &[0x2B, 0x65, 0x70];
/// RFC 8410 Ed448 OID (`1.3.101.113`) — recognised so it can be refused by name.
pub const OID_ED448: &[u8] = &[0x2B, 0x65, 0x71];
/// NIST P-256 OID.
pub const OID_CURVE_P256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
/// NIST P-384 OID.
pub const OID_CURVE_P384: &[u8] = &[0x2B, 0x81, 0x04, 0x00, 0x22];
/// NIST P-521 OID.
pub const OID_CURVE_P521: &[u8] = &[0x2B, 0x81, 0x04, 0x00, 0x23];

/// Public key material of a v4 key/subkey packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMaterial<'a> {
    /// RSA: modulus and public exponent.
    Rsa {
        /// Modulus MPI magnitude.
        n: &'a [u8],
        /// Public exponent MPI magnitude.
        e: &'a [u8],
    },
    /// Ed25519: the 32-octet public key (the MPI's `0x40` prefix removed).
    Ed25519 {
        /// Ed25519 public key.
        point: &'a [u8],
    },
    /// ECDSA over a named curve with an uncompressed point (`0x04 || X || Y`).
    Ecdsa {
        /// Named curve.
        curve: Curve,
        /// Uncompressed SEC1 point, including the leading `0x04`.
        point: &'a [u8],
    },
}

/// A parsed v4 public key or subkey packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicKey<'a> {
    /// Key creation time (Unix seconds).
    pub created: u32,
    /// Public-key algorithm id.
    pub algo: u8,
    /// Parsed key material.
    pub material: KeyMaterial<'a>,
    /// The packet body (fed to the fingerprint and to certification hashes).
    pub body: &'a [u8],
}

impl PublicKey<'_> {
    /// The v4 fingerprint of this key/subkey packet.
    pub fn fingerprint(&self) -> [u8; 20] {
        super::openpgp_crypto::v4_fingerprint(self.body)
    }

    /// Key size in bits as reported in diagnostics (RSA modulus bits, or the
    /// curve size for the elliptic-curve algorithms).
    pub fn bits(&self) -> u16 {
        match self.material {
            KeyMaterial::Rsa { n, .. } => bit_len(n) as u16,
            KeyMaterial::Ed25519 { .. } => 255,
            KeyMaterial::Ecdsa { curve, .. } => match curve {
                Curve::P256 => 256,
                Curve::P384 => 384,
                Curve::P521 => 521,
            },
        }
    }
}

fn bit_len(bytes: &[u8]) -> usize {
    let mut i = 0;
    while i < bytes.len() && bytes[i] == 0 {
        i += 1;
    }
    let b = &bytes[i..];
    match b.first() {
        None => 0,
        Some(&first) => (b.len() - 1) * 8 + (8 - first.leading_zeros() as usize),
    }
}

fn read_oid<'a>(buf: &'a [u8], pos: &mut usize) -> Result<&'a [u8], PacketError> {
    let len = read_u8(buf, pos)? as usize;
    if len == 0 || len > 32 {
        return Err(PacketError::MalformedKey);
    }
    take(buf, pos, len)
}

/// Parse a v4 public-key (tag 6) or public-subkey (tag 14) packet body.
///
/// Unsupported algorithms are reported as [`PacketError::UnsupportedAlgo`] *with
/// the key body still available to the caller's diagnostics*; the verifier turns
/// that into a refusal only for keys it was asked to use.
pub fn parse_public_key(body: &[u8]) -> Result<PublicKey<'_>, PacketError> {
    if body.len() > MAX_PACKET_BODY {
        return Err(PacketError::PacketTooLarge);
    }
    let mut pos = 0usize;
    let version = read_u8(body, &mut pos)?;
    if version != 4 {
        return Err(PacketError::UnsupportedVersion(version));
    }
    let created = read_u32(body, &mut pos)?;
    let algo = read_u8(body, &mut pos)?;

    let material = match algo {
        ALGO_RSA | ALGO_RSA_SIGN => {
            let n = read_mpi(body, &mut pos)?;
            let e = read_mpi(body, &mut pos)?;
            if pos != body.len() {
                return Err(PacketError::MalformedKey);
            }
            if n.bytes.is_empty() || e.bytes.is_empty() {
                return Err(PacketError::MalformedKey);
            }
            KeyMaterial::Rsa {
                n: n.bytes,
                e: e.bytes,
            }
        }
        ALGO_EDDSA => {
            let oid = read_oid(body, &mut pos)?;
            let point = read_mpi(body, &mut pos)?;
            if pos != body.len() {
                return Err(PacketError::MalformedKey);
            }
            if oid == OID_ED448 {
                return Err(PacketError::UnsupportedAlgo(algo));
            }
            if oid != OID_ED25519 && oid != OID_ED25519_LEGACY {
                return Err(PacketError::MalformedKey);
            }
            // The MPI carries the 0x40-prefixed point; `ed25519-dalek` wants the
            // bare 32 octets.
            let raw = point.bytes;
            if raw.len() != 33 || raw[0] != 0x40 {
                return Err(PacketError::MalformedKey);
            }
            KeyMaterial::Ed25519 { point: &raw[1..] }
        }
        ALGO_ECDSA => {
            let oid = read_oid(body, &mut pos)?;
            let point = read_mpi(body, &mut pos)?;
            if pos != body.len() {
                return Err(PacketError::MalformedKey);
            }
            let curve = if oid == OID_CURVE_P256 {
                Curve::P256
            } else if oid == OID_CURVE_P384 {
                Curve::P384
            } else if oid == OID_CURVE_P521 {
                Curve::P521
            } else {
                return Err(PacketError::MalformedKey);
            };
            let want = match curve {
                Curve::P256 => 65,
                Curve::P384 => 97,
                Curve::P521 => 133,
            };
            let raw = point.bytes;
            // Uncompressed points only; a compressed point would need a square
            // root the verifier has no business computing.
            if raw.len() != want || raw[0] != 0x04 {
                return Err(PacketError::MalformedKey);
            }
            KeyMaterial::Ecdsa { curve, point: raw }
        }
        other => return Err(PacketError::UnsupportedAlgo(other)),
    };

    Ok(PublicKey {
        created,
        algo,
        material,
        body,
    })
}

// ── Signatures ───────────────────────────────────────────────────────────────

/// Signature types the verifier distinguishes (RFC 9580 §5.2.1).
pub const SIG_BINARY: u8 = 0x00;
/// Canonical text document signature (clearsigned `InRelease`).
pub const SIG_TEXT: u8 = 0x01;
/// Generic User ID certification (0x10; 0x11–0x13 are the stronger levels).
pub const SIG_GENERIC_CERT: u8 = 0x10;
/// Positive User ID certification (0x13) — the level GnuPG writes for self-sigs.
pub const SIG_POSITIVE_CERT: u8 = 0x13;
/// Signature directly on a key (RFC 4880 §5.2.1) — hashed over the key packet
/// body alone. This is the type GnuPG writes for "direct key" metadata, and it
/// is distinct from the User ID certifications below.
pub const SIG_DIRECT_KEY: u8 = 0x1F;
/// Subkey binding signature.
pub const SIG_SUBKEY_BINDING: u8 = 0x18;
/// Primary key binding signature.
pub const SIG_PRIMARY_KEY_BINDING: u8 = 0x19;
/// Key revocation signature.
pub const SIG_KEY_REVOCATION: u8 = 0x20;
/// Subkey revocation signature.
pub const SIG_SUBKEY_REVOCATION: u8 = 0x28;

/// Key-flag subpacket bit meaning "this key may sign" (RFC 9580 §5.2.3.29).
pub const KEY_FLAG_SIGN: u8 = 0x02;

/// A parsed v4 signature packet, with the hashed area kept verbatim (it is part
/// of the signed data, so it must be hashed exactly as received).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignaturePacket<'a> {
    /// The packet body (version octet through the last MPI).
    pub body: &'a [u8],
    /// Signature type.
    pub sig_type: u8,
    /// Public-key algorithm.
    pub pub_algo: u8,
    /// Hash algorithm octet (validated by the policy layer).
    pub hash_algo: u8,
    /// Version..end-of-hashed-subpackets, i.e. the second element of the digest.
    pub hashed_portion: &'a [u8],
    /// Subpacket 2 — signature creation time.
    pub created: Option<u32>,
    /// Subpacket 9 — key expiration, in seconds after the key's creation time.
    pub key_expiry: Option<u32>,
    /// Subpacket 16 — issuer key ID (hashed or unhashed area).
    pub issuer_keyid: Option<[u8; 8]>,
    /// Subpacket 33 — issuer fingerprint (v4 keys).
    pub issuer_fpr: Option<[u8; 20]>,
    /// Subpacket 27 — key flags (first octet).
    pub key_flags: Option<u8>,
    /// Leftmost 16 bits of the signed digest (a cheap pre-filter).
    pub left16: [u8; 2],
    /// Signature MPIs (one for RSA, two for EdDSA/ECDSA).
    pub mpis: Vec<Mpi<'a>>,
}

fn parse_subpackets(area: &[u8], sig: &mut SignaturePacket<'_>) -> Result<(), PacketError> {
    let mut pos = 0usize;
    let mut count = 0usize;
    while pos < area.len() {
        count += 1;
        if count > MAX_SUBPACKETS {
            return Err(PacketError::MalformedSubpackets);
        }
        let first = area[pos];
        pos += 1;
        let len = match first {
            0..=191 => first as usize,
            192..=254 => {
                let second = *area.get(pos).ok_or(PacketError::MalformedSubpackets)?;
                pos += 1;
                ((first as usize - 192) << 8) + second as usize + 192
            }
            _ => {
                let b = take(area, &mut pos, 4)?;
                u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize
            }
        };
        if len == 0 {
            return Err(PacketError::MalformedSubpackets);
        }
        let data = take(area, &mut pos, len)?;
        let ty = data[0];
        let payload = &data[1..];
        match ty {
            2 => {
                if payload.len() == 4 {
                    sig.created = Some(u32::from_be_bytes([
                        payload[0], payload[1], payload[2], payload[3],
                    ]));
                }
            }
            9 => {
                if payload.len() == 4 {
                    sig.key_expiry = Some(u32::from_be_bytes([
                        payload[0], payload[1], payload[2], payload[3],
                    ]));
                }
            }
            16 => {
                if payload.len() == 8 {
                    let mut id = [0u8; 8];
                    id.copy_from_slice(payload);
                    sig.issuer_keyid = Some(id);
                }
            }
            27 => {
                if !payload.is_empty() {
                    sig.key_flags = Some(payload[0]);
                }
            }
            33 => {
                if payload.len() == 21 && payload[0] == 4 {
                    let mut fpr = [0u8; 20];
                    fpr.copy_from_slice(&payload[1..]);
                    sig.issuer_fpr = Some(fpr);
                }
            }
            // Every other subpacket stays unknown but is still hashed: the
            // hashed area is hashed verbatim, so ignoring a subpacket's meaning
            // never weakens the signature.
            _ => {}
        }
    }
    Ok(())
}

/// Parse a v4 signature packet body.
pub fn parse_signature(body: &[u8]) -> Result<SignaturePacket<'_>, PacketError> {
    if body.len() > MAX_PACKET_BODY {
        return Err(PacketError::PacketTooLarge);
    }
    let mut pos = 0usize;
    let version = read_u8(body, &mut pos)?;
    if version != 4 {
        return Err(PacketError::UnsupportedVersion(version));
    }
    let sig_type = read_u8(body, &mut pos)?;
    let pub_algo = read_u8(body, &mut pos)?;
    let hash_algo = read_u8(body, &mut pos)?;
    let hashed_len = read_u16(body, &mut pos)? as usize;
    let hashed_area = take(body, &mut pos, hashed_len)?;
    let hashed_portion = &body[..pos];
    let unhashed_len = read_u16(body, &mut pos)? as usize;
    let unhashed_area = take(body, &mut pos, unhashed_len)?;
    let left = take(body, &mut pos, 2)?;

    let mut sig = SignaturePacket {
        body,
        sig_type,
        pub_algo,
        hash_algo,
        hashed_portion,
        created: None,
        key_expiry: None,
        issuer_keyid: None,
        issuer_fpr: None,
        key_flags: None,
        left16: [left[0], left[1]],
        mpis: Vec::new(),
    };
    parse_subpackets(hashed_area, &mut sig)?;
    // The unhashed area may legitimately repeat the issuer id; a value found in
    // the hashed area wins (it is the authenticated one).
    let mut unhashed = SignaturePacket {
        body: &[],
        sig_type: 0,
        pub_algo: 0,
        hash_algo: 0,
        hashed_portion: &[],
        created: None,
        key_expiry: None,
        issuer_keyid: None,
        issuer_fpr: None,
        key_flags: None,
        left16: [0, 0],
        mpis: Vec::new(),
    };
    parse_subpackets(unhashed_area, &mut unhashed)?;
    if sig.issuer_keyid.is_none() {
        sig.issuer_keyid = unhashed.issuer_keyid;
    }

    // MPI count is algorithm-dependent; accept up to two and require the packet
    // to be fully consumed.
    while pos < body.len() {
        if sig.mpis.len() >= 2 {
            return Err(PacketError::MalformedSignature);
        }
        sig.mpis.push(read_mpi(body, &mut pos)?);
    }
    if sig.mpis.is_empty() {
        return Err(PacketError::MalformedSignature);
    }
    Ok(sig)
}

// ── Keyring blocks ───────────────────────────────────────────────────────────

/// One subkey of a keyring block plus its binding signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subkey<'a> {
    /// The subkey packet.
    pub key: PublicKey<'a>,
    /// The signature that binds the subkey to the primary key (0x18).
    pub binding: Option<SignaturePacket<'a>>,
    /// Subkey revocation signatures (0x28).
    pub revocations: Vec<SignaturePacket<'a>>,
}

/// One primary key with its user IDs, certifications, subkeys and revocations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBlock<'a> {
    /// The primary key packet.
    pub primary: PublicKey<'a>,
    /// User ID packet bodies.
    pub uids: Vec<&'a [u8]>,
    /// Certification signatures (0x10–0x13); self-signatures and cross-signatures
    /// by other keys both land here and are told apart by verifying them.
    pub certifications: Vec<SignaturePacket<'a>>,
    /// Subkeys with their bindings.
    pub subkeys: Vec<Subkey<'a>>,
    /// Primary key revocation signatures (0x20).
    pub revocations: Vec<SignaturePacket<'a>>,
}

/// Walk a pinned keyring block into its primary key, certifications, subkeys and
/// revocations.
///
/// Tolerant by design: packets this verifier does not use (trust packets, other
/// keys' cross-certifications, primary-key-binding signatures) are ignored, and
/// an *unsupported algorithm* is left for the policy layer to judge. Structural
/// damage — a truncated packet, a second primary key, a malformed signature —
/// is an error: the block is a compile-time constant of the trust store, so
/// anything unexpected in it is a generated-artifact bug and must fail closed.
pub fn parse_key_block(block: &[u8]) -> Result<KeyBlock<'_>, PacketError> {
    if block.len() > MAX_KEY_BLOCK_BYTES {
        return Err(PacketError::KeyBlockTooLarge);
    }
    let mut primary: Option<PublicKey<'_>> = None;
    let mut uids: Vec<&[u8]> = Vec::new();
    let mut certifications: Vec<SignaturePacket<'_>> = Vec::new();
    let mut subkeys: Vec<Subkey<'_>> = Vec::new();
    let mut revocations: Vec<SignaturePacket<'_>> = Vec::new();

    for item in packets(block) {
        let pkt = item?;
        match pkt.tag {
            6 => {
                if primary.is_some() {
                    return Err(PacketError::KeyBlockMalformed);
                }
                primary = Some(parse_public_key(pkt.body)?);
            }
            13 => uids.push(pkt.body),
            14 => {
                if primary.is_none() {
                    return Err(PacketError::KeyBlockMalformed);
                }
                subkeys.push(Subkey {
                    key: parse_public_key(pkt.body)?,
                    binding: None,
                    revocations: Vec::new(),
                });
            }
            2 => {
                let sig = parse_signature(pkt.body)?;
                match sig.sig_type {
                    // User ID certifications (0x10–0x13) and signatures directly
                    // on the key (0x1F): both can carry the primary key's flags
                    // and expiry and are told apart when they are verified.
                    SIG_GENERIC_CERT..=SIG_POSITIVE_CERT | SIG_DIRECT_KEY => {
                        if subkeys.is_empty() {
                            certifications.push(sig);
                        }
                    }
                    SIG_SUBKEY_BINDING => {
                        if let Some(sub) = subkeys.last_mut() {
                            if sub.binding.is_none() {
                                sub.binding = Some(sig);
                            }
                        }
                    }
                    SIG_KEY_REVOCATION => revocations.push(sig),
                    SIG_SUBKEY_REVOCATION => {
                        if let Some(sub) = subkeys.last_mut() {
                            sub.revocations.push(sig);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let primary = primary.ok_or(PacketError::MissingPrimaryKey)?;
    Ok(KeyBlock {
        primary,
        uids,
        certifications,
        subkeys,
        revocations,
    })
}

// ── Clear-signed messages (`InRelease`) ──────────────────────────────────────

/// A parsed `-----BEGIN PGP SIGNED MESSAGE-----` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clearsigned<'a> {
    /// Headers before the blank line (`Hash: SHA256`, `Comment:` …).
    pub headers: Vec<ArmorHeader>,
    /// The clear text exactly as it appears in the armor, **including** the line
    /// ending that separates it from the signature block (so for Debian's
    /// `InRelease` this is byte-identical to the standalone `Release` file, which
    /// is what the apt trust chain parses). Armor line endings, dash escapes and
    /// trailing whitespace are untouched.
    pub text: &'a [u8],
    /// The armored signature block that follows the clear text.
    pub signature: &'a [u8],
}

impl Clearsigned<'_> {
    /// The signed byte string: the clear text after dash-unescaping, with every
    /// line's trailing whitespace removed, lines joined by CRLF and **no** line
    /// ending before the signature block (RFC 4880 §7.1).
    ///
    /// This is exactly what GnuPG hashes — verified against live Debian
    /// `InRelease` and against synthetic fixtures with trailing whitespace and a
    /// dash-escaped line in P52.
    pub fn canonical_text(&self) -> Vec<u8> {
        // The line ending before the signature header is not part of the signed
        // text (RFC 4880 §7.1); the armor always writes one.
        let text = if self.text.ends_with(b"\r\n") {
            &self.text[..self.text.len() - 2]
        } else if self.text.ends_with(b"\n") {
            &self.text[..self.text.len() - 1]
        } else {
            self.text
        };
        let mut lines: Vec<&[u8]> = Vec::new();
        for line in text.split(|&b| b == b'\n') {
            let line = if line.starts_with(b"- ") {
                &line[2..]
            } else {
                line
            };
            let mut end = line.len();
            while end > 0 && matches!(line[end - 1], b' ' | b'\t' | b'\r') {
                end -= 1;
            }
            lines.push(&line[..end]);
        }
        let mut out: Vec<u8> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(line);
        }
        out
    }
}

/// Parse a clear-signed message: headers, clear text and the trailing armored
/// signature block.
pub fn parse_clearsigned(input: &[u8]) -> Result<Clearsigned<'_>, PacketError> {
    if input.len() > MAX_CLEARSIGN_BYTES {
        return Err(PacketError::TooLarge);
    }
    const BEGIN_MSG: &[u8] = b"-----BEGIN PGP SIGNED MESSAGE-----";
    const BEGIN_SIG: &[u8] = b"-----BEGIN PGP SIGNATURE-----";

    // Locate the message header, allowing leading blank lines.
    let mut offset = 0usize;
    let begin_at = loop {
        if offset >= input.len() {
            return Err(PacketError::ArmorMissing);
        }
        let line_end = input[offset..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| offset + i)
            .unwrap_or(input.len());
        let line = trim_ascii(&input[offset..line_end]);
        if line == BEGIN_MSG {
            break line_end;
        }
        if !line.is_empty() {
            return Err(PacketError::ArmorMissing);
        }
        offset = line_end + 1;
    };

    // Headers run until the first empty line.
    let mut pos = begin_at + 1;
    let mut headers: Vec<ArmorHeader> = Vec::new();
    let body_start = loop {
        if pos >= input.len() {
            return Err(PacketError::ClearsignMalformed);
        }
        let line_end = input[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| pos + i)
            .unwrap_or(input.len());
        let line = trim_ascii(&input[pos..line_end]);
        if line.is_empty() {
            break line_end + 1;
        }
        let colon = line
            .iter()
            .position(|&b| b == b':')
            .ok_or(PacketError::ClearsignMalformed)?;
        let key = trim_ascii(&line[..colon]);
        let value = trim_ascii(&line[colon + 1..]);
        if key.is_empty() {
            return Err(PacketError::ClearsignMalformed);
        }
        headers.push(ArmorHeader {
            key: key.to_vec(),
            value: value.to_vec(),
        });
        pos = line_end + 1;
    };

    // Find the signature block: it must start at a line boundary.
    let mut text_end = None;
    let mut sig_start = None;
    let mut scan = body_start;
    while scan < input.len() {
        let line_end = input[scan..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| scan + i)
            .unwrap_or(input.len());
        if trim_ascii(&input[scan..line_end]) == BEGIN_SIG {
            text_end = Some(scan);
            sig_start = Some(scan);
            break;
        }
        scan = line_end + 1;
    }
    let (text_end, sig_start) = match (text_end, sig_start) {
        (Some(t), Some(s)) => (t, s),
        _ => return Err(PacketError::ClearsignMalformed),
    };

    // The clear text keeps its framing exactly as armored (including the line
    // ending before the signature header); `canonical_text` removes that ending
    // when it computes what was signed.
    Ok(Clearsigned {
        headers,
        text: &input[body_start..text_end],
        signature: &input[sig_start..],
    })
}
