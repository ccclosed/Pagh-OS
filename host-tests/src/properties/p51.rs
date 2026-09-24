// Feature: OpenPGP repository-metadata verification (issue #32), Property 51:
// the armor/packet layer parses only well-formed, bounded input and NEVER
// panics, whatever a mirror sends.
//
// The bytes this code eats come from a host we have not authenticated yet: the
// signature blob may be truncated mid-packet, base64-broken, carry a wrong CRC,
// use a packet form this verifier refuses (partial body lengths), or be plain
// garbage. `panic = "abort"` means a panic here kills the machine, and the
// caller (the apt trust chain) has no way to catch it — so the parser must be
// total over arbitrary input, and every bound must produce a named refusal.
//
// Covered here:
//   * the committed GnuPG fixtures de-armor, and every fixture key's v4
//     fingerprint recomputes to the pinned constant (the fingerprint code the
//     trust decision leans on is exercised by P53 against the Debian keyring,
//     and here against the fixtures' own bytes);
//   * CRC24 is checked when present (a flipped CRC character is refused);
//   * truncation at any point of a real armored blob is refused;
//   * the packet framing handles both header formats, and refuses partial /
//     indeterminate / oversized / truncated framing by name;
//   * the packet-count and armor-size bounds trip as errors, not as memory
//     growth;
//   * RFC 3174 SHA-1 vectors hold (the fingerprint hash is an implementation we
//     own — no `sha1` crate is vendored — so its test vectors are pinned here);
//   * a randomized fuzz sweep over arbitrary bytes and over truncations of the
//     real fixtures returns errors without panicking.

use proptest::prelude::*;

use crate::openpgp_crypto::{sha1, v4_fingerprint};
use super::openpgp_fixtures::*;
use crate::openpgp_packet::{self as packet, PacketError};

/// The `=`-prefixed CRC24 line of an armored blob, as a byte range.
fn crc_line_range(armored: &[u8]) -> Option<(usize, usize)> {
    let mut start = 0usize;
    while start < armored.len() {
        let end = armored[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| start + i)
            .unwrap_or(armored.len());
        if armored[start] == b'=' {
            return Some((start, end));
        }
        start = end + 1;
    }
    None
}

#[test]
fn fixture_keyring_blocks_parse_and_match_their_fingerprints() {
    for (name, block, fpr) in [
        ("KEY_ED", KEY_ED, FPR_ED),
        ("KEY_RSA", KEY_RSA, FPR_RSA),
        ("KEY_EXPIRED", KEY_EXPIRED, FPR_EXPIRED),
        ("KEY_FOREIGN", KEY_FOREIGN, FPR_FOREIGN),
    ] {
        let parsed = packet::parse_key_block(block).expect("fixture block parses");
        let computed = parsed.primary.fingerprint();
        assert_eq!(computed, fpr, "{name}: primary fingerprint");
        // Recompute through the public one-shot helper as well: a drift between
        // the two paths would mean the fingerprint rule is duplicated wrongly.
        assert_eq!(v4_fingerprint(parsed.primary.body), fpr, "{name}: v4_fingerprint");
        assert!(!parsed.uids.is_empty(), "{name}: has a user ID");
    }
    let rsa = packet::parse_key_block(KEY_RSA).expect("RSA block parses");
    assert_eq!(rsa.subkeys.len(), 1, "RSA fixture has exactly one subkey");
    assert_eq!(rsa.subkeys[0].key.fingerprint(), FPR_RSA_SUB);
}

#[test]
fn armor_decodes_and_the_crc_holds() {
    let armored = packet::dearmor(RELEASE_GPG).expect("fixture armor decodes");
    assert_eq!(armored.data, RELEASE_GPG_BIN, "decoded bytes match the fixture");
    // Three packets: two signatures (Ed25519 + RSA) is what the fixture signs.
    let count = packet::packets(&armored.data)
        .filter(|p| matches!(p, Ok(pkt) if pkt.tag == 2))
        .count();
    assert_eq!(count, 2, "two signature packets in the multi-signer fixture");
}

#[test]
fn armor_crc_mismatch_is_refused() {
    let (start, end) = crc_line_range(RELEASE_GPG).expect("fixture has a CRC24 line");
    let mut tampered = RELEASE_GPG.to_vec();
    // Flip one base64 character of the CRC: still valid armor, wrong checksum.
    let c = tampered[start + 1];
    tampered[start + 1] = if c == b'A' { b'B' } else { b'A' };
    assert!(end > start);
    let err = packet::dearmor(&tampered).expect_err("bad CRC is refused");
    assert_eq!(err, PacketError::ArmorCrc);
}

#[test]
fn armor_truncation_is_refused() {
    // Any prefix that cuts into the END marker is refused. (A prefix that stops
    // exactly at the end of the END line is complete armor and may parse: the
    // trailing newline is not required.)
    const END: &[u8] = b"-----END PGP SIGNATURE-----";
    let end_at = RELEASE_GPG
        .windows(END.len())
        .position(|w| w == END)
        .expect("fixture carries an END line");
    for cut in [8usize, 30, 120, RELEASE_GPG.len() - 40, end_at + END.len() - 1] {
        let err = packet::dearmor(&RELEASE_GPG[..cut]);
        assert!(err.is_err(), "truncation at {cut} must not parse");
    }
    // Trailing non-whitespace after the END line is refused too.
    let mut with_junk = RELEASE_GPG.to_vec();
    with_junk.extend_from_slice(b"junk");
    assert_eq!(
        packet::dearmor(&with_junk).expect_err("trailing junk"),
        PacketError::ArmorTrailingData
    );
}

#[test]
fn armor_size_bound_is_named() {
    let oversized = vec![b'A'; packet::MAX_ARMOR_BYTES + 1];
    assert_eq!(
        packet::dearmor(&oversized).expect_err("oversized armor"),
        PacketError::TooLarge
    );
}

#[test]
fn packet_framing_edge_cases() {
    // Old format, 1-octet length, tag 2, body [1, 2, 3].
    let old = [0x88u8, 3, 1, 2, 3];
    let packets: Vec<_> = packet::packets(&old).collect();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].as_ref().unwrap().tag, 2);
    assert_eq!(packets[0].as_ref().unwrap().body, &[1, 2, 3]);

    // New format with a 5-octet length (255 marker) and a 2-byte body.
    let new5 = [0xC6u8, 0xFF, 0, 0, 0, 2, 0xAA, 0xBB];
    let packets: Vec<_> = packet::packets(&new5).collect();
    assert_eq!(packets[0].as_ref().unwrap().body, &[0xAA, 0xBB]);

    // New format with a two-octet length (192..=223 marker). The marker
    // encodes lengths 192..=8383: `(first-192)<<8 | second + 192`.
    let mut new2 = vec![0xC6u8, 192, 0]; // length 192
    new2.extend(std::iter::repeat(0xAA).take(192));
    let packets: Vec<_> = packet::packets(&new2).collect();
    assert_eq!(packets[0].as_ref().unwrap().body.len(), 192);
    // A two-octet length that runs past the end of the buffer is truncated.
    assert_eq!(
        packet::packets(&[0xC6, 192, 1]).next().unwrap().unwrap_err(),
        PacketError::Truncated
    );

    // A length octet that is not a CTB.
    assert_eq!(
        packet::packets(&[0x00, 0x01]).next().unwrap().unwrap_err(),
        PacketError::NotAPacket
    );
    // Partial body length (224..=254) is refused rather than reassembled.
    assert_eq!(
        packet::packets(&[0xC6, 224]).next().unwrap().unwrap_err(),
        PacketError::PartialLength
    );
    // Old-format indeterminate length (type 3).
    assert_eq!(
        packet::packets(&[0x8B, 0x01]).next().unwrap().unwrap_err(),
        PacketError::IndeterminateLength
    );
    // Body runs past the end of the buffer.
    assert_eq!(
        packet::packets(&[0xC6, 10, 1]).next().unwrap().unwrap_err(),
        PacketError::Truncated
    );
}

#[test]
fn packet_count_bound_trips() {
    // 300 empty packets of tag 0: the iterator must stop with TooManyPackets
    // rather than walk an unbounded stream.
    let mut buf = Vec::new();
    for _ in 0..(packet::MAX_PACKETS + 44) {
        buf.extend_from_slice(&[0x80, 0x00]);
    }
    let mut saw_bound = false;
    for item in packet::packets(&buf) {
        if item == Err(PacketError::TooManyPackets) {
            saw_bound = true;
            break;
        }
    }
    assert!(saw_bound, "packet-count bound must trip");
}

#[test]
fn sha1_rfc3174_vectors() {
    assert_eq!(
        sha1(b""),
        [
            0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
            0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09
        ]
    );
    assert_eq!(
        sha1(b"abc"),
        [
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
            0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d
        ]
    );
    assert_eq!(
        sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        [
            0x84, 0x98, 0x3e, 0x44, 0x1c, 0x3b, 0xd2, 0x6e, 0xba, 0xae, 0x4a, 0xa1, 0xf9, 0x51,
            0x29, 0xe5, 0xe5, 0x46, 0x70, 0xf1
        ]
    );
    // 1,000,000 × 'a' also covers multi-block streaming.
    let million = vec![b'a'; 1_000_000];
    assert_eq!(
        sha1(&million),
        [
            0x34, 0xaa, 0x97, 0x3c, 0xd4, 0xc4, 0xda, 0xa4, 0xf6, 0x1e, 0xeb, 0x2b, 0xdb, 0xad,
            0x27, 0x31, 0x65, 0x34, 0x01, 0x6f
        ]
    );
}

proptest! {
    /// Arbitrary bytes must never panic any entry point of the packet layer.
    #[test]
    fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..600)) {
        let _ = packet::dearmor(&data);
        let _ = packet::dearmor_or_binary(&data);
        let _ = packet::parse_clearsigned(&data);
        let _ = packet::parse_public_key(&data);
        let _ = packet::parse_signature(&data);
        let _ = packet::parse_key_block(&data);
        let mut walked = 0usize;
        for item in packet::packets(&data) {
            walked += 1;
            if item.is_err() {
                break;
            }
            prop_assert!(walked <= packet::MAX_PACKETS);
        }
    }

    /// Every truncation of a real armored blob is refused unless it still
    /// carries the complete END marker — and then it must decode to exactly the
    /// fixture payload, never to something else.
    #[test]
    fn truncated_real_armor_is_refused(cut in 0usize..RELEASE_GPG.len()) {
        const END: &[u8] = b"-----END PGP SIGNATURE-----";
        let prefix = &RELEASE_GPG[..cut];
        let end_present = prefix
            .windows(END.len())
            .any(|w| w == END);
        match packet::dearmor(prefix) {
            Ok(decoded) => {
                prop_assert!(end_present, "armor decoded without an END marker at {}", cut);
                prop_assert_eq!(&decoded.data, RELEASE_GPG_BIN, "payload differs at {}", cut);
            }
            Err(_) => {}
        }
    }

    /// Flipping one bit inside the *armored payload* never yields a silently
    /// accepted signature blob: either the CRC catches it or the packet layer
    /// does. (`dearmor_or_binary` is what the verifier uses.)
    #[test]
    fn single_bit_flip_in_armor_is_never_ok(
        pos in 0usize..RELEASE_GPG.len(),
        bit in 0u8..8,
    ) {
        let mut tampered = RELEASE_GPG.to_vec();
        tampered[pos] ^= 1 << bit;
        let result = packet::dearmor_or_binary(&tampered);
        // A flip inside whitespace/line-structure bytes may legitimately still
        // decode to the same payload; anything else must carry an error or the
        // identical payload. It must never produce a *different* payload that
        // decodes without an error.
        if let Ok(data) = result {
            prop_assert!(data == RELEASE_GPG_BIN, "a flipped armor byte changed the payload");
        }
    }
}
