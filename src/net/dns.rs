//! Pure DNS query construction and A-record response parsing.
//!
//! This is the **pure** core of the resolver (the effectful socket pump lives in
//! [`crate::net::resolve`]). Like [`crate::net::http`] it is `core` + `alloc`
//! only — no smoltcp sockets, no globals, no hardware — and deliberately speaks
//! in plain byte slices and `[u8; 4]` octets rather than any `smoltcp` wire type,
//! so it compiles unchanged into the `host-tests` crate via a `#[path]` include
//! and the parser can be property-tested on the host.
//!
//! Both functions are hardened against adversarial input: every index is bounds
//! checked, compression pointers in answers are handled defensively, and no path
//! can panic or read past the input buffer.
#![allow(dead_code)]

use alloc::vec::Vec;

/// DNS QTYPE for an IPv4 address (A) record.
pub const QTYPE_A: u16 = 1;
/// DNS QCLASS for the Internet (IN).
pub const QCLASS_IN: u16 = 1;

/// Maximum number of name "hops" we will follow while skipping a (possibly
/// compressed) DNS name. A bound prevents runaway loops on malformed pointers.
const MAX_NAME_STEPS: usize = 128;

/// Build a DNS standard query for `hostname` into `out` (cleared first).
///
/// Produces a single-Question message with `QTYPE=A`, `QCLASS=IN`, the
/// recursion-desired (RD) flag set, and transaction id `id`. The QNAME is the
/// dot-separated labels of `hostname`, each length-prefixed, terminated by the
/// root (zero) label.
///
/// Returns `false` (leaving `out` safe but unspecified) if `hostname` cannot be
/// encoded: an empty hostname, an empty label (e.g. a leading/trailing/`..` dot),
/// a label longer than 63 bytes, or a total encoded name longer than 255 bytes.
/// Pure and panic-free.
pub fn build_dns_query(id: u16, hostname: &str, out: &mut Vec<u8>) -> bool {
    out.clear();

    if hostname.is_empty() {
        return false;
    }

    // Validate + measure the encoded QNAME length before emitting anything past
    // the header, so a reject leaves no partial garbage to misinterpret.
    let mut name_len = 1usize; // the terminating root label
    for label in hostname.split('.') {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        name_len += 1 + label.len();
    }
    if name_len > 255 {
        return false;
    }

    // Header (12 bytes): ID, flags, QDCOUNT=1, ANCOUNT=NSCOUNT=ARCOUNT=0.
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&0x0100u16.to_be_bytes()); // QR=0, Opcode=0, RD=1
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT

    // QNAME: length-prefixed labels + root label.
    for label in hostname.split('.') {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);

    // QTYPE, QCLASS.
    out.extend_from_slice(&QTYPE_A.to_be_bytes());
    out.extend_from_slice(&QCLASS_IN.to_be_bytes());
    true
}

/// Parse the first A-record (IPv4) answer out of a DNS response in `buf`.
///
/// Returns `Some([a, b, c, d])` for the first answer RR whose TYPE is A and whose
/// RDATA is exactly four bytes. Returns `None` on any of: a buffer shorter than
/// the 12-byte header, a transaction-id mismatch with `expected_id`, a message
/// that is not a response (QR clear), a non-zero RCODE (e.g. NXDOMAIN), no A
/// answer present, or any malformed/truncated record. Never indexes past `buf`
/// and never panics.
pub fn parse_dns_a_response(buf: &[u8], expected_id: u16) -> Option<[u8; 4]> {
    if buf.len() < 12 {
        return None;
    }

    let id = u16::from_be_bytes([buf[0], buf[1]]);
    if id != expected_id {
        return None;
    }

    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    // QR (bit 15) must be set for a response.
    if flags & 0x8000 == 0 {
        return None;
    }
    // RCODE (low 4 bits) must be 0 (NoError); NXDOMAIN(3) and friends -> None.
    if flags & 0x000F != 0 {
        return None;
    }

    let qdcount = u16::from_be_bytes([buf[4], buf[5]]);
    let ancount = u16::from_be_bytes([buf[6], buf[7]]);

    let mut pos = 12usize;

    // Skip the question section: NAME + QTYPE(2) + QCLASS(2).
    for _ in 0..qdcount {
        pos = skip_name(buf, pos)?;
        pos = pos.checked_add(4)?;
        if pos > buf.len() {
            return None;
        }
    }

    // Walk the answer RRs looking for the first A record.
    for _ in 0..ancount {
        pos = skip_name(buf, pos)?;
        // Fixed RR fields: TYPE(2) CLASS(2) TTL(4) RDLENGTH(2) = 10 bytes.
        let fields_end = pos.checked_add(10)?;
        if fields_end > buf.len() {
            return None;
        }
        let rtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let rdlength = u16::from_be_bytes([buf[pos + 8], buf[pos + 9]]) as usize;
        let rdata = fields_end;
        let rdata_end = rdata.checked_add(rdlength)?;
        if rdata_end > buf.len() {
            return None;
        }
        if rtype == QTYPE_A && rdlength == 4 {
            return Some([buf[rdata], buf[rdata + 1], buf[rdata + 2], buf[rdata + 3]]);
        }
        pos = rdata_end;
    }

    None
}

/// Advance past a (possibly compressed) DNS name beginning at `pos`, returning
/// the index of the first byte after the name *in the record stream*.
///
/// For a name terminated by a compression pointer, that is the index just after
/// the two pointer bytes (the pointer is not followed — callers only need to skip
/// the name, never decode it). Returns `None` on any malformed input: an index
/// out of range, reserved length bits, or more than [`MAX_NAME_STEPS`] labels
/// (runaway). Never indexes past `buf`.
fn skip_name(buf: &[u8], mut pos: usize) -> Option<usize> {
    let mut steps = 0usize;
    loop {
        steps += 1;
        if steps > MAX_NAME_STEPS {
            return None;
        }
        let len = *buf.get(pos)?;
        match len & 0xC0 {
            0x00 => {
                if len == 0 {
                    // Root label: the name ends here.
                    return Some(pos + 1);
                }
                // Ordinary label: skip the length byte + the label bytes.
                pos = pos.checked_add(1 + len as usize)?;
                if pos > buf.len() {
                    return None;
                }
            }
            0xC0 => {
                // Compression pointer (two bytes); ensure the second byte exists,
                // then the name ends here in the stream.
                buf.get(pos + 1)?;
                return Some(pos + 2);
            }
            // 0x40 / 0x80: reserved label types -> malformed.
            _ => return None,
        }
    }
}

/// Parse a dotted-quad IPv4 literal (e.g. `"10.0.2.3"`) into four octets, or
/// `None` for any malformed input (wrong octet count, empty/oversized octet,
/// non-digit byte, or an octet outside `0..=255`). Pure and panic-free; lets
/// [`crate::net::resolve`] short-circuit a literal without issuing a query.
pub fn parse_ipv4_literal(s: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut idx = 0usize;
    for part in s.split('.') {
        if idx >= 4 || part.is_empty() || part.len() > 3 {
            return None;
        }
        let mut value: u16 = 0;
        for b in part.bytes() {
            if !b.is_ascii_digit() {
                return None;
            }
            value = value * 10 + (b - b'0') as u16;
        }
        if value > 255 {
            return None;
        }
        octets[idx] = value as u8;
        idx += 1;
    }
    if idx != 4 {
        return None;
    }
    Some(octets)
}
