// Feature: TLS server authentication (issue #14), Property 44: the X.509
// certificate parser the verifier will feed server chains into —
//   * a test-side DER builder produces certificates with KNOWN fields, and
//     `parse_certificate` must return exactly those fields (version, serial,
//     names, validity, RSA SPKI, SAN dNSName/iPAddress, BasicConstraints);
//   * any truncation of a well-formed certificate is rejected (the outer
//     length declares the full size — a prefix can never parse), and any
//     single-byte mutation is Err-or-Ok but never a panic;
//   * two REAL-WORLD fixtures (frozen DER: ISRG Root X1 as the CA shape,
//     the current deb.debian.org leaf as the server shape) parse with the
//     expected structural properties — guard against the parser being
//     tuned only to the test builder's own encodings.

use crate::x509::{
    find_extension, parse_basic_constraints, parse_certificate, san_names, decode_asn1_time,
    San, SpkiKey, TAG_BIT_STRING, TAG_BOOLEAN, TAG_IA5_STRING, TAG_INTEGER, TAG_NULL, TAG_OCTET_STRING,
    TAG_SEQUENCE, TAG_SET, TAG_UTC_TIME, TAG_UTF8_STRING, OID_EXT_BASIC_CONSTRAINTS, OID_EXT_SAN,
    OID_RSA_ENCRYPTION,
};
use proptest::prelude::*;

#[path = "p44_fixture_root.rs"]
mod fixture_root;
#[path = "p44_fixture_leaf.rs"]
mod fixture_leaf;

use fixture_leaf::DEB_LEAF_DER_HEX;
use fixture_root::ROOT_X1_DER_HEX;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn hval(c: u8) -> u32 {
    match c {
        b'0'..=b'9' => (c - b'0') as u32,
        b'a'..=b'f' => (c - b'a' + 10) as u32,
        b'A'..=b'F' => (c - b'A' + 10) as u32,
        _ => panic!("bad hex digit"),
    }
}

fn hex(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    (0..b.len() / 2)
        .map(|i| ((hval(b[2 * i]) << 4) | hval(b[2 * i + 1])) as u8)
        .collect()
}

/// Minimal-DER TLV encoder (test side).
fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let l = content.len();
    if l < 0x80 {
        out.push(l as u8);
    } else {
        let be = l.to_be_bytes();
        let skip = be.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (8 - skip) as u8);
        out.extend_from_slice(&be[skip..]);
    }
    out.extend_from_slice(content);
    out
}

fn oid(content: &[u8]) -> Vec<u8> {
    tlv(0x06, content)
}

fn int(content: &[u8]) -> Vec<u8> {
    tlv(TAG_INTEGER, content)
}

fn utctime(s: &[u8]) -> Vec<u8> {
    tlv(TAG_UTC_TIME, s)
}

/// Name ::= SEQUENCE { SET { SEQUENCE { OID(cn 2.5.4.3), UTF8String } } }
fn name(cn: &[u8]) -> Vec<u8> {
    let cn_field = tlv(0x30, &[oid(&[0x55, 0x04, 0x03]), tlv(TAG_UTF8_STRING, cn)].concat());
    let rdn = tlv(TAG_SET, &cn_field);
    tlv(TAG_SEQUENCE, &rdn)
}

/// RSA SubjectPublicKeyInfo element wrapping (`n`, `e`).
fn rsa_spki(n: &[u8], e: &[u8]) -> Vec<u8> {
    let pk = tlv(TAG_SEQUENCE, &[int(n), int(e)].concat());
    let mut bits = vec![0x00]; // BIT STRING unused-bits byte
    bits.extend_from_slice(&pk);
    let alg = tlv(TAG_SEQUENCE, &[oid(OID_RSA_ENCRYPTION), tlv(TAG_NULL, &[])].concat());
    tlv(TAG_SEQUENCE, &[alg.as_slice(), tlv(TAG_BIT_STRING, &bits).as_slice()].concat())
}

/// One Extension element: SEQ { OID, [BOOLEAN critical], OCTET STRING value }.
fn ext(oid_v: &[u8], critical: bool, value_der: &[u8]) -> Vec<u8> {
    let mut inner = oid(oid_v);
    if critical {
        inner.extend_from_slice(&tlv(TAG_BOOLEAN, &[0xff]));
    }
    inner.extend_from_slice(&tlv(TAG_OCTET_STRING, value_der));
    tlv(TAG_SEQUENCE, &inner)
}

/// SAN extension with GeneralName entries as (tag, content) pairs.
fn san_ext(entries: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut gn = Vec::new();
    for (tag, c) in entries {
        gn.extend_from_slice(&tlv(*tag, c));
    }
    ext(OID_EXT_SAN, false, &tlv(TAG_SEQUENCE, &gn))
}

/// basicConstraints extension (critical, per RFC 5280 MUST).
fn bc_ext(is_ca: bool) -> Vec<u8> {
    let seq = if is_ca {
        tlv(TAG_SEQUENCE, &tlv(TAG_BOOLEAN, &[0xff]))
    } else {
        tlv(TAG_SEQUENCE, &[])
    };
    ext(OID_EXT_BASIC_CONSTRAINTS, true, &seq)
}

/// Build a full certificate DER with the given fields.
#[allow(clippy::too_many_arguments)]
fn build_cert(
    version: Option<u8>, // None => v1 (field absent)
    serial: &[u8],
    not_before: &[u8],
    not_after: &[u8],
    spki: &[u8],
    exts: &[Vec<u8>],
    sig: &[u8],
) -> Vec<u8> {
    let mut tbs_c = Vec::new();
    if let Some(v) = version {
        // [0] EXPLICIT INTEGER, encoded value = version - 1.
        tbs_c.extend_from_slice(&tlv(0xA0, &int(&[v - 1])));
    }
    tbs_c.extend_from_slice(&int(serial));
    tbs_c.extend_from_slice(&tlv(TAG_SEQUENCE, &[oid(OID_RSA_ENCRYPTION), tlv(TAG_NULL, &[])].concat()));
    tbs_c.extend_from_slice(&name(b"Issuer"));
    tbs_c.extend_from_slice(&tlv(TAG_SEQUENCE, &[utctime(not_before), utctime(not_after)].concat()));
    tbs_c.extend_from_slice(&name(b"Subject"));
    tbs_c.extend_from_slice(spki);
    if !exts.is_empty() {
        let mut all = Vec::new();
        for e in exts {
            all.extend_from_slice(e);
        }
        tbs_c.extend_from_slice(&tlv(0xA3, &tlv(TAG_SEQUENCE, &all)));
    }
    let tbs = tlv(TAG_SEQUENCE, &tbs_c);
    let alg = tlv(TAG_SEQUENCE, &[oid(OID_RSA_ENCRYPTION), tlv(TAG_NULL, &[])].concat());
    let mut sigbits = vec![0x00];
    sigbits.extend_from_slice(sig);
    tlv(TAG_SEQUENCE, &[
        tbs.as_slice(),
        alg.as_slice(),
        tlv(TAG_BIT_STRING, &sigbits).as_slice(),
    ].concat())
}

/// Format a UTCTime ASCII string for a civil date (test-side).
fn utc_str(y: u32, mo: u32, d: u32, h: u32, mi: u32) -> Vec<u8> {
    format!("{:02}{:02}{:02}{:02}{:02}00Z", y % 100, mo, d, h, mi).into_bytes()
}

// ---------------------------------------------------------------------------
// properties
// ---------------------------------------------------------------------------

proptest! {
    /// Whatever the builder encodes, the parser must hand back exactly.
    #[test]
    fn generated_cert_round_trips(
        serial in proptest::collection::vec(any::<u8>(), 1..8)
            .prop_filter("no sign ambiguity", |v| v[0] != 0),
        (y1, mo1, d1, h1, mi1) in (2020u32..=2040, 1u32..=12, 1u32..=28, 0u32..=23, 0u32..=59),
        (y2, mo2, d2, h2, mi2) in (2020u32..=2040, 1u32..=12, 1u32..=28, 0u32..=23, 0u32..=59),
        dns in "[a-z][a-z0-9-]{0,10}(\\.[a-z][a-z0-9-]{0,10}){1,2}",
        ip in proptest::collection::vec(any::<u8>(), 4),
        n in proptest::collection::vec(any::<u8>(), 1..24)
            .prop_filter("no sign ambiguity", |v| v[0] != 0),
        dummy_sig in proptest::collection::vec(any::<u8>(), 0..32),
    ) {
        let nb = utc_str(y1, mo1, d1, h1, mi1);
        let na = utc_str(y2, mo2, d2, h2, mi2);
        let der = build_cert(
            Some(3),
            &serial,
            &nb,
            &na,
            &rsa_spki(&n, &[0x01, 0x00, 0x01]),
            &[san_ext(&[(0x82, dns.as_bytes().to_vec()), (0x87, ip.clone())]), bc_ext(true)],
            &dummy_sig,
        );
        let (cert, rest) = parse_certificate(&der).unwrap();
        prop_assert!(rest.is_empty());

        prop_assert_eq!(cert.version, 3);
        prop_assert_eq!(cert.serial, serial.as_slice());
        // The raw signatureValue bytes come back untouched.
        prop_assert_eq!(cert.signature, dummy_sig.as_slice());
        let issuer_der = name(b"Issuer");
        let subject_der = name(b"Subject");
        prop_assert_eq!(cert.issuer, issuer_der.as_slice());
        prop_assert_eq!(cert.subject, subject_der.as_slice());
        prop_assert_eq!(cert.validity.not_before, decode_asn1_time(TAG_UTC_TIME, &nb).unwrap());
        prop_assert_eq!(cert.validity.not_after, decode_asn1_time(TAG_UTC_TIME, &na).unwrap());

        // RSA SPKI round-trip.
        match cert.spki.key {
            SpkiKey::Rsa { n: pn, e: pe } => {
                prop_assert_eq!(pn, n.as_slice());
                prop_assert_eq!(pe, &[0x01, 0x00, 0x01]);
            }
            ref k => prop_assert!(false, "expected RSA, got {k:?}"),
        }

        // SAN: exactly the generated dNSName + iPAddress (one each), and
        // nothing else of substance.
        let san = find_extension(&cert, OID_EXT_SAN).unwrap().unwrap();
        let mut dns_count = 0u32;
        let mut ip_count = 0u32;
        for entry in san_names(san).unwrap() {
            match entry.unwrap() {
                San::Dns(d) => {
                    dns_count += 1;
                    prop_assert_eq!(d, dns.as_bytes());
                }
                San::Ip(i) => {
                    ip_count += 1;
                    prop_assert_eq!(i, ip.as_slice());
                }
                San::Other => {}
            }
        }
        prop_assert_eq!(dns_count, 1);
        prop_assert_eq!(ip_count, 1);

        // BasicConstraints: CA.
        let bc = find_extension(&cert, OID_EXT_BASIC_CONSTRAINTS).unwrap().unwrap();
        prop_assert_eq!(parse_basic_constraints(bc).unwrap(), true);

        // Unknown OID lookup finds nothing.
        prop_assert_eq!(find_extension(&cert, &[0x55, 0x1d, 0x2b]), Ok(None));
    }

    /// v1 certificate (version field absent, no extensions): parses with
    /// version 1 and no SAN.
    #[test]
    fn v1_cert_has_no_extensions(
        serial in proptest::collection::vec(any::<u8>(), 1..8)
            .prop_filter("no sign ambiguity", |v| v[0] != 0),
    ) {
        let der = build_cert(
            None,
            &serial,
            b"250101000000Z",
            b"260101000000Z",
            &rsa_spki(&[0xc1], &[0x01, 0x00, 0x01]),
            &[],
            b"\xde\xad",
        );
        let (cert, rest) = parse_certificate(&der).unwrap();
        prop_assert!(rest.is_empty());
        prop_assert_eq!(cert.version, 1);
        prop_assert_eq!(find_extension(&cert, OID_EXT_SAN), Ok(None));
    }

    /// Fail-closed on corruption: truncation of ANY prefix is always an
    /// error (the outer length declares the whole size), a byte flip is
    /// Err-or-Ok but never a panic, and a surviving parse must not lose the
    /// trailing bytes invariant (rest is always inside the buffer).
    #[test]
    fn corruption_fail_closed(
        dns in "[a-z][a-z0-9-]{0,10}(\\.[a-z][a-z0-9-]{0,10}){1,2}",
        cut in 0usize..400,
        idx in 0usize..400,
        byte in any::<u8>(),
    ) {
        let der = build_cert(
            Some(3),
            &[0x11, 0x22],
            b"250101000000Z",
            b"260101000000Z",
            &rsa_spki(&[0xc1, 0x02], &[0x01, 0x00, 0x01]),
            &[san_ext(&[(0x82, dns.as_bytes().to_vec())]), bc_ext(true)],
            b"\x00\x01\x02",
        );

        // Truncation: any proper prefix must be rejected.
        let cut = cut.min(der.len() - 1);
        prop_assert!(parse_certificate(&der[..cut]).is_err());

        // Mutation: never panics.
        let mut m = der.clone();
        let i = idx % m.len();
        if m[i] != byte {
            m[i] = byte;
        }
        match parse_certificate(&m) {
            Ok((_cert, rest)) => prop_assert!(rest.len() < m.len()),
            Err(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// real-world fixtures (frozen DER)
// ---------------------------------------------------------------------------

#[test]
fn real_root_isrg_x1_parses() {
    let der = hex(ROOT_X1_DER_HEX);
    let (cert, rest) = parse_certificate(&der).unwrap();
    assert!(rest.is_empty());

    // A v3 root: critical basicConstraints CA:TRUE, RSA-4096 SPKI, no SAN.
    assert_eq!(cert.version, 3);
    let bc = find_extension(&cert, OID_EXT_BASIC_CONSTRAINTS).unwrap().unwrap();
    assert_eq!(parse_basic_constraints(bc).unwrap(), true);
    match cert.spki.key {
        SpkiKey::Rsa { n, e } => {
            // ISRG Root X1 carries a 4096-bit modulus and e = 65537.
            assert_eq!(n.len(), 512);
            assert_eq!(e, &[0x01, 0x00, 0x01]);
        }
        k => panic!("expected RSA SPKI, got {k:?}"),
    }
    assert_eq!(find_extension(&cert, OID_EXT_SAN), Ok(None));
    assert!(cert.validity.not_before < cert.validity.not_after);
}

#[test]
fn real_leaf_deb_debian_org_parses() {
    let der = hex(DEB_LEAF_DER_HEX);
    let (cert, rest) = parse_certificate(&der).unwrap();
    assert!(rest.is_empty());

    // A v3 leaf: SAN present and must carry the mirror hostname; the SPKI is
    // one of the algorithms the verifier supports (RSA or ECDSA P-256/P-384).
    assert_eq!(cert.version, 3);
    let san = find_extension(&cert, OID_EXT_SAN).unwrap().expect("leaf must carry SAN");
    let dns: Vec<Vec<u8>> = san_names(san)
        .unwrap()
        .filter_map(|r| r.ok())
        .filter_map(|s| match s {
            San::Dns(d) => Some(d.to_vec()),
            _ => None,
        })
        .collect();
    assert!(
        dns.iter().any(|d| d.as_slice() == b"deb.debian.org"),
        "SAN must contain deb.debian.org, got {dns:?}"
    );
    assert!(!matches!(cert.spki.key, SpkiKey::Unsupported));
    assert!(cert.validity.not_before < cert.validity.not_after);
}

#[test]
fn generated_v3_cert_matches_all_expected_fields() {
    // Fixed vector through the builder: pins the exact serial/SAN/BC values
    // (cheap deterministic counterpart to the randomized round-trip).
    let der = build_cert(
        Some(3),
        &[0x42],
        b"250101000000Z",
        b"260101000000Z",
        &rsa_spki(&[0xc1, 0xab], &[0x01, 0x00, 0x01]),
        &[san_ext(&[(0x82, b"mirror.example.org".to_vec()), (0x87, vec![10, 0, 2, 15])]), bc_ext(true)],
        b"\x00\x01\x02",
    );
    let (cert, rest) = parse_certificate(&der).unwrap();
    assert!(rest.is_empty());
    assert_eq!(cert.serial, &[0x42]);
    assert_eq!(cert.signature, &[0x00, 0x01, 0x02]);
    assert_eq!(cert.validity.not_before, 1_735_689_600);
    assert_eq!(cert.validity.not_after, 1_767_225_600);
    let san = find_extension(&cert, OID_EXT_SAN).unwrap().unwrap();
    let entries: Vec<San> = san_names(san).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0], San::Dns(b"mirror.example.org".as_slice()));
    assert_eq!(entries[1], San::Ip(&[10, 0, 2, 15]));
    let bc = find_extension(&cert, OID_EXT_BASIC_CONSTRAINTS).unwrap().unwrap();
    assert!(parse_basic_constraints(bc).unwrap());
    // dNSName tag is IA5 under the hood; the builder used the right one.
    assert_eq!(TAG_IA5_STRING, 0x16);
}
