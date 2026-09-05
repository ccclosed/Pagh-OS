// Feature: TLS server authentication (issue #14), Property 43: the minimal DER
// reader + X.509 time decoder the certificate verifier sits on —
//   * `der_tlv` splits the exact tag/content/rest of any well-formed TLV, and
//     `der_children` walks the siblings of a constructed element in order;
//   * the reader is fail-closed on adversarial input: any truncation or
//     single-byte mutation is either rejected with `Err` or decodes without
//     panicking (it runs on untrusted network bytes inside `panic = "abort"`);
//   * non-DER length encodings (indefinite, non-minimal long form) are always
//     rejected, for every small length value;
//   * `decode_asn1_time` agrees with the INDEPENDENTLY tested civil→unix
//     conversion in `timeconv` (P32) across the whole representable range, and
//     rejects impossible dates/shapes.

use crate::timeconv::civil_to_unix;
use crate::x509::{
    decode_asn1_time, der_children, der_expect, der_tlv, DerError, TAG_BOOLEAN, TAG_GENERALIZED_TIME,
    TAG_INTEGER, TAG_SEQUENCE, TAG_UTC_TIME,
};
use proptest::prelude::*;

/// Test-side encoder: a TLV with proper minimal DER length.
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

/// Days in month (proleptic Gregorian) — test-side validity filter, kept
/// independent of the module under test.
fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// TLV structure
// ---------------------------------------------------------------------------

proptest! {
    /// Round-trip: whatever the content, the reader hands back the exact
    /// tag, the exact content, and an empty rest.
    #[test]
    fn tlv_round_trip(content in proptest::collection::vec(any::<u8>(), 0..300)) {
        let enc = tlv(TAG_SEQUENCE, &content);
        let (tag, got, rest) = der_tlv(&enc).unwrap();
        prop_assert_eq!(tag, TAG_SEQUENCE);
        prop_assert_eq!(got, content.as_slice());
        prop_assert!(rest.is_empty());
    }

    /// Sibling walk: N concatenated TLVs are yielded in order, and the
    /// iterator consumes exactly all of them.
    #[test]
    fn children_walk_in_order(
        parts in proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 0..60), 1..12,
        ),
    ) {
        // Encode each part as its own INTEGER element and concatenate.
        let mut whole = Vec::new();
        for p in &parts {
            whole.extend_from_slice(&tlv(TAG_INTEGER, p));
        }
        let collected: Result<Vec<(u8, Vec<u8>)>, DerError> = der_children(&whole)
            .map(|r| r.map(|(t, c)| (t, c.to_vec())))
            .collect();
        let collected = collected.unwrap();
        prop_assert_eq!(collected.len(), parts.len());
        for ((tag, content), p) in collected.iter().zip(&parts) {
            prop_assert_eq!(*tag, TAG_INTEGER);
            prop_assert_eq!(content, p);
        }
    }

    /// Fail-closed against truncation: a valid TLV cut at ANY prefix is either
    /// fully parseable only if the cut kept everything, or an `Err` — never a
    /// panic, and never an Ok with wrong content.
    #[test]
    fn truncation_is_rejected_or_exact(
        content in proptest::collection::vec(any::<u8>(), 0..300),
        cut in 0usize..320,
    ) {
        let enc = tlv(TAG_SEQUENCE, &content);
        let prefix = &enc[..cut.min(enc.len())];
        match der_tlv(prefix) {
            Ok((tag, got, rest)) => {
                // Only the uncut encoding can parse; when it does, it is exact.
                prop_assert_eq!(prefix.len(), enc.len());
                prop_assert_eq!(tag, TAG_SEQUENCE);
                prop_assert_eq!(got, content.as_slice());
                prop_assert!(rest.is_empty());
            }
            Err(_) => prop_assert!(cut < enc.len()),
        }
    }

    /// Fail-closed against corruption: flipping bytes (tag, length, or
    /// content) of a valid TLV never panics; Ok results stay self-consistent.
    #[test]
    fn mutation_never_panics(
        content in proptest::collection::vec(any::<u8>(), 1..64),
        idx in 0usize..80,
        byte in any::<u8>(),
    ) {
        let mut enc = tlv(TAG_SEQUENCE, &content);
        let i = idx % enc.len();
        if enc[i] != byte {
            enc[i] = byte;
        }
        match der_tlv(&enc) {
            Ok((_tag, got, _rest)) => {
                // A surviving parse must not read past the buffer.
                prop_assert!(got.len() <= content.len() + 8);
            }
            Err(_) => {}
        }
        // Random garbage through the children iterator: Ok/Err only.
        for r in der_children(&enc) {
            let _ = r;
        }
    }

    /// Non-minimal long-form lengths are rejected for EVERY small value:
    /// `[tag, 0x81, len]` claims a 1-byte length for len < 0x80 — not DER.
    #[test]
    fn non_minimal_long_form_always_rejected(len in 0u8..0x80) {
        let enc = [TAG_SEQUENCE, 0x81, len];
        prop_assert_eq!(der_tlv(&enc), Err(DerError::BadLength));
    }

    /// Random bytes through the reader: Ok/Err only, never a panic.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let _ = der_tlv(&bytes);
        let _ = der_expect(&bytes, TAG_BOOLEAN);
        for r in der_children(&bytes) {
            let _ = r;
        }
    }
}

// ---------------------------------------------------------------------------
// X.509 time decoding
// ---------------------------------------------------------------------------

/// Encode `(y, mo, d, h, mi, s)` in the UTCTime shape for years 1950..=2049.
fn utc_time_str(y: u32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Vec<u8> {
    format!("{:02}{:02}{:02}{:02}{:02}{:02}Z", y % 100, mo, d, h, mi, s).into_bytes()
}

/// Encode the GeneralizedTime shape for years 1000..=9999.
fn gen_time_str(y: u32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Vec<u8> {
    format!("{:04}{:02}{:02}{:02}{:02}{:02}Z", y, mo, d, h, mi, s).into_bytes()
}

/// Test-side RFC 5280 year pivot for a UTCTime string (mirrors the decoder).
fn py_of(enc: &[u8]) -> i64 {
    let yy = (enc[0] - b'0') as u32 * 10 + (enc[1] - b'0') as u32;
    if yy >= 50 { 1900 + yy as i64 } else { 2000 + yy as i64 }
}

proptest! {
    /// UTCTime decode agrees with the independently tested `timeconv`
    /// civil→unix conversion across the whole UTCTime year window, including
    /// the pre-epoch negative range and the RFC 5280 pivot year 50.
    #[test]
    fn utctime_matches_independent_conversion(
        (y, mo, d) in (1950u32..=2049, 1u32..=12, 1u32..=31)
            .prop_filter("existing date", |(y, mo, d)| *d <= days_in_month(*y as i64, *mo)),
        h in 0u32..=23,
        mi in 0u32..=59,
        s in 0u32..=60,
    ) {
        let enc = utc_time_str(y, mo, d, h, mi, s);
        let got = decode_asn1_time(TAG_UTC_TIME, &enc).unwrap();
        let want = civil_to_unix(y as i64, mo, d, h, mi, s);
        prop_assert_eq!(got, want);
    }

    /// GeneralizedTime decode likewise agrees, over a four-digit year window
    /// well outside UTCTime's reach.
    #[test]
    fn generalized_time_matches_independent_conversion(
        (y, mo, d) in (1000u32..=9999, 1u32..=12, 1u32..=31)
            .prop_filter("existing date", |(y, mo, d)| *d <= days_in_month(*y as i64, *mo)),
        h in 0u32..=23,
        mi in 0u32..=59,
        s in 0u32..=60,
    ) {
        let enc = gen_time_str(y, mo, d, h, mi, s);
        let got = decode_asn1_time(TAG_GENERALIZED_TIME, &enc).unwrap();
        let want = civil_to_unix(y as i64, mo, d, h, mi, s);
        prop_assert_eq!(got, want);
    }

    /// Malformed/mutated time strings never panic and never invent a date:
    /// a digit flip either keeps the shape — and then the decode must equal
    /// the independent conversion of whatever date the string now names
    /// (test-side parse + RFC 5280 year pivot as the oracle) — or is rejected.
    #[test]
    fn time_mutation_never_panics_or_invents(
        (y, mo, d) in (1950u32..=2049, 1u32..=12, 1u32..=28)
            .prop_filter("existing date", |(y, mo, d)| *d <= days_in_month(*y as i64, *mo)),
        idx in 0usize..13,
        digit in 0u8..10,
    ) {
        let mut enc = utc_time_str(y, mo, d, 12, 34, 56);
        let i = idx % enc.len();
        if enc[i].is_ascii_digit() {
            enc[i] = b'0' + digit;
        }
        match decode_asn1_time(TAG_UTC_TIME, &enc) {
            Ok(got) => {
                // Oracle: parse the mutated string test-side (the year pivot
                // mirrors RFC 5280) and require agreement.
                let two = |s: &[u8]| (s[0] - b'0') as u32 * 10 + (s[1] - b'0') as u32;
                let yy = two(&enc[0..2]);
                let py = if yy >= 50 { 1900 + yy as i64 } else { 2000 + yy as i64 };
                let want = civil_to_unix(
                    py,
                    two(&enc[2..4]) as u32,
                    two(&enc[4..6]) as u32,
                    two(&enc[6..8]) as u32,
                    two(&enc[8..10]) as u32,
                    two(&enc[10..12]) as u32,
                );
                prop_assert_eq!(got, want);
            }
            // Rejection is legal exactly when the mutated digits name a date
            // the test-side calendar check also calls impossible (or a time
            // of day out of range) — i.e. the decoder invents nothing. The
            // year cannot be invalid (any 2-digit year maps to 1900..=2099),
            // so rejection must come from the rest.
            Err(DerError::BadTime) => {
                let two = |s: &[u8]| (s[0] - b'0') as u32 * 10 + (s[1] - b'0') as u32;
                let invalid = two(&enc[2..4]) == 0
                    || two(&enc[2..4]) > 12
                    || two(&enc[4..6]) == 0
                    || two(&enc[4..6]) > days_in_month(py_of(&enc), two(&enc[2..4]))
                    || two(&enc[6..8]) > 23
                    || two(&enc[8..10]) > 59
                    || two(&enc[10..12]) > 60;
                prop_assert!(invalid, "rejected a real date: {enc:?}");
            }
            Err(e) => prop_assert!(false, "unexpected error {e:?} for {enc:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Fixed vectors (cheap, exact)
// ---------------------------------------------------------------------------

#[test]
fn known_vectors() {
    assert_eq!(
        decode_asn1_time(TAG_UTC_TIME, b"250101000000Z"),
        Ok(1_735_689_600)
    );
    assert_eq!(
        decode_asn1_time(TAG_GENERALIZED_TIME, b"20250101000000Z"),
        Ok(1_735_689_600)
    );
    // Pivot years: 49 -> 2049, 50 -> 1950 (pre-epoch).
    assert_eq!(
        decode_asn1_time(TAG_UTC_TIME, b"490101000000Z"),
        Ok(2_493_072_000)
    );
    assert_eq!(
        decode_asn1_time(TAG_UTC_TIME, b"500101000000Z"),
        Ok(-631_152_000)
    );
}

#[test]
fn impossible_dates_rejected() {
    assert_eq!(decode_asn1_time(TAG_UTC_TIME, b"230230120000Z"), Err(DerError::BadTime));
    assert_eq!(decode_asn1_time(TAG_UTC_TIME, b"230101240000Z"), Err(DerError::BadTime));
    assert_eq!(decode_asn1_time(TAG_UTC_TIME, b"2301010000Z"), Err(DerError::BadTime));
    assert_eq!(decode_asn1_time(TAG_UTC_TIME, b"230101000000+01"), Err(DerError::BadTime));
    assert_eq!(
        decode_asn1_time(TAG_GENERALIZED_TIME, b"20250101000000.123Z"),
        Err(DerError::BadTime)
    );
    assert_eq!(decode_asn1_time(TAG_SEQUENCE, b"230101000000Z"), Err(DerError::BadTime));
    assert_eq!(decode_asn1_time(TAG_UTC_TIME, b"2X0101000000Z"), Err(DerError::BadTime));
}

#[test]
fn fixed_length_encodings() {
    // Indefinite length (BER) is not DER.
    assert_eq!(der_tlv(&[0x30, 0x80]), Err(DerError::BadLength));
    // Long form for a short-form value.
    assert_eq!(der_tlv(&[0x30, 0x81, 0x05]), Err(DerError::BadLength));
    // Leading zero in a long-form length.
    assert_eq!(der_tlv(&[0x30, 0x82, 0x00, 0x80]), Err(DerError::BadLength));
    // Truncated length bytes.
    assert_eq!(der_tlv(&[0x30, 0x82, 0x01]), Err(DerError::Truncated));
    // Content runs past the buffer.
    assert_eq!(der_tlv(&[0x30, 0x05, 0x01, 0x02]), Err(DerError::Truncated));
    // High-tag-number form is outside the supported subset.
    assert_eq!(der_tlv(&[0x5f, 0x1d, 0x00]), Err(DerError::BadLength));
    // Empty input.
    assert_eq!(der_tlv(&[]), Err(DerError::Truncated));
}

#[test]
fn nested_walk() {
    // SEQUENCE { SEQUENCE { INTEGER 1 }, INTEGER 7 } followed by a sibling
    // BOOLEAN: checks both children iteration and rest-handoff.
    let inner = tlv(TAG_INTEGER, &[0x01]);
    let mut outer_content = tlv(TAG_SEQUENCE, &inner);
    outer_content.extend_from_slice(&tlv(TAG_INTEGER, &[0x07]));
    let mut whole = tlv(TAG_SEQUENCE, &outer_content);
    whole.extend_from_slice(&tlv(TAG_BOOLEAN, &[0xff]));

    let (tag, content, rest) = der_tlv(&whole).unwrap();
    assert_eq!(tag, TAG_SEQUENCE);
    assert_eq!(content, outer_content.as_slice());
    assert_eq!(rest, tlv(TAG_BOOLEAN, &[0xff]).as_slice());

    let children: Vec<(u8, Vec<u8>)> = der_children(content)
        .map(|r| r.map(|(t, c)| (t, c.to_vec())))
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0].0, TAG_SEQUENCE);
    assert_eq!(children[1].0, TAG_INTEGER);
    assert_eq!(children[1].1, vec![0x07]);
}
