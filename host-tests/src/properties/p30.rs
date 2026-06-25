// Feature: networking, Property 30: DNS query building round-trips through the
// A-record response parser, and the parser is panic-free / bounds-safe on
// arbitrary input (including name-compression pointers and NXDOMAIN).

use crate::dns::{build_dns_query, parse_dns_a_response, QCLASS_IN, QTYPE_A};
use proptest::prelude::*;

/// Build a minimal DNS response that echoes the question from `query` and
/// appends a single A answer (`addr`) whose NAME is a compression pointer back
/// to the question at offset 12. This mirrors what a real resolver returns and
/// exercises the parser's pointer handling.
fn build_a_response(query: &[u8], addr: [u8; 4]) -> Vec<u8> {
    let mut r = query.to_vec();
    // Flip header into a response: set QR + RD + RA, RCODE=0, ANCOUNT=1.
    r[2] = 0x81; // QR=1, RD=1
    r[3] = 0x80; // RA=1, RCODE=0
    r[6] = 0x00;
    r[7] = 0x01; // ANCOUNT = 1

    // Answer: NAME = pointer to offset 12 (the question's QNAME).
    r.push(0xC0);
    r.push(0x0C);
    r.extend_from_slice(&QTYPE_A.to_be_bytes()); // TYPE = A
    r.extend_from_slice(&QCLASS_IN.to_be_bytes()); // CLASS = IN
    r.extend_from_slice(&60u32.to_be_bytes()); // TTL
    r.extend_from_slice(&4u16.to_be_bytes()); // RDLENGTH = 4
    r.extend_from_slice(&addr); // RDATA
    r
}

/// A hostname strategy: 1..4 DNS labels of 1..=20 ASCII letters/digits.
fn hostname_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec("[a-z0-9]{1,20}", 1..4).prop_map(|labels| labels.join("."))
}

#[test]
fn rejects_empty_and_malformed_hostnames() {
    let mut out = Vec::new();
    // Empty hostname, empty label (leading/trailing/double dot) -> reject.
    assert!(!build_dns_query(1, "", &mut out));
    assert!(!build_dns_query(1, ".", &mut out));
    assert!(!build_dns_query(1, "a..b", &mut out));
    assert!(!build_dns_query(1, "deb.debian.org.", &mut out));
    // Label > 63 bytes -> reject.
    let long = "a".repeat(64);
    assert!(!build_dns_query(1, &long, &mut out));
}

#[test]
fn parser_is_bounds_safe_on_truncation_and_pointers() {
    // A well-formed response truncated at every prefix must never panic.
    let mut q = Vec::new();
    assert!(build_dns_query(0x1234, "deb.debian.org", &mut q));
    let resp = build_a_response(&q, [151, 101, 0, 1]);
    for len in 0..resp.len() {
        let _ = parse_dns_a_response(&resp[..len], 0x1234);
    }

    // A pointer that loops to itself must terminate (bounded) and return None,
    // not hang or panic.
    let mut loopy = q.clone();
    loopy[2] = 0x81;
    loopy[3] = 0x80;
    loopy[6] = 0x00;
    loopy[7] = 0x01;
    let here = loopy.len();
    loopy.push(0xC0);
    loopy.push((here & 0xFF) as u8); // points at itself
    assert_eq!(parse_dns_a_response(&loopy, 0x1234), None);

    // NXDOMAIN (RCODE=3) -> None even if an answer is present.
    let mut nx = build_a_response(&q, [1, 2, 3, 4]);
    nx[3] = 0x83; // RA=1, RCODE=3 (NXDOMAIN)
    assert_eq!(parse_dns_a_response(&nx, 0x1234), None);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any valid hostname, transaction id, and address, a query built by
    /// `build_dns_query` and turned into an A response parses back to exactly
    /// that address. A wrong expected id, a non-response, or arbitrary garbage
    /// returns `None` without panicking.
    #[test]
    fn dns_query_response_round_trip(
        host in hostname_strategy(),
        id in any::<u16>(),
        addr in any::<[u8; 4]>(),
        garbage in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut q = Vec::new();
        prop_assert!(build_dns_query(id, &host, &mut q));

        // Query header sanity: id, RD set, QDCOUNT=1, ANCOUNT=0.
        prop_assert_eq!(u16::from_be_bytes([q[0], q[1]]), id);
        prop_assert_eq!(q[2] & 0x01, 0x01); // RD bit
        prop_assert_eq!(u16::from_be_bytes([q[4], q[5]]), 1); // QDCOUNT
        prop_assert_eq!(u16::from_be_bytes([q[6], q[7]]), 0); // ANCOUNT

        // Correct round trip.
        let resp = build_a_response(&q, addr);
        prop_assert_eq!(parse_dns_a_response(&resp, id), Some(addr));

        // Wrong expected id -> None.
        prop_assert_eq!(parse_dns_a_response(&resp, id.wrapping_add(1)), None);

        // The query itself is not a response (QR clear) -> None.
        prop_assert_eq!(parse_dns_a_response(&q, id), None);

        // Arbitrary garbage never panics (result is ignored).
        let _ = parse_dns_a_response(&garbage, id);
    }
}
