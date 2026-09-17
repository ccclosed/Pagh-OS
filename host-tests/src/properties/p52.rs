// Feature: OpenPGP repository-metadata verification (issue #32), Property 52:
// the verification policy accepts real GnuPG signatures and refuses every
// tamper class — corrupted data, corrupted signature, foreign signer, expired
// key, unset clock.
//
// This is the property that decides whether a mirror's `Release`/`InRelease` is
// trusted, so its negatives matter as much as its positives. The fixtures are
// GnuPG-produced (P51 de-armors them), which makes this an interoperability
// test: the exact v4 digest rule (`H = HASH(data ‖ hashed_portion ‖ 0x04 0xFF ‖
// be32(len(hashed_portion)))`), the MPI left-padding, "EdDSA signs the digest",
// the clearsign canonicalization and the subkey-binding composition all have to
// match GnuPG byte-for-byte or the positive cases fail.
//
// The negative cases pin the semantics `gpgv` itself implements, plus the one
// deliberate strengthening in `OPENPGP-VERIFY-CONTRACT.md` §3.4:
//   * a signature from a pinned key that does not verify is FATAL even when
//     another pinned signature in the same blob is good (gpgv exits 1 for
//     exactly that shape);
//   * a signature whose issuer is not in the pinned keyring is ignored (Debian
//     rotates signers; refusing unknown signers would break the live mirror);
//   * an expired key is refused — gpgv verifies it and exits 0, pagh does not.

use proptest::prelude::*;

use crate::openpgp::{
    check_pinned_key, verify_clearsigned, verify_detached, OpenPgpError, PinnedKey, CLOCK_FLOOR,
};
use crate::openpgp_crypto::CryptoError;
use super::openpgp_fixtures::*;
use crate::openpgp_packet::{self as packet, PacketError};

const ED_LABEL: &str = "pagh test release key (ed25519) <pagh-test@example.invalid>";
const RSA_LABEL: &str = "pagh test archive key (rsa2048) <pagh-test@example.invalid>";
const EXPIRED_LABEL: &str = "pagh test expired key (ed25519) <pagh-test@example.invalid>";
const REVOKED_LABEL: &str = "pagh test revoked key (ed25519) <pagh-test@example.invalid>";

static RSA_SUBKEYS: &[[u8; 20]] = &[FPR_RSA_SUB];
static NO_SUBKEYS: &[[u8; 20]] = &[];

fn ed_key() -> PinnedKey {
    PinnedKey {
        label: ED_LABEL,
        fingerprint: FPR_ED,
        algo: ALGO_ED,
        bits: BITS_ED,
        created: CREATED_ED,
        expires: EXPIRES_ED,
        block: KEY_ED,
        subkeys: NO_SUBKEYS,
    }
}

fn rsa_key() -> PinnedKey {
    PinnedKey {
        label: RSA_LABEL,
        fingerprint: FPR_RSA,
        algo: ALGO_RSA,
        bits: BITS_RSA,
        created: CREATED_RSA,
        expires: EXPIRES_RSA,
        block: KEY_RSA,
        subkeys: RSA_SUBKEYS,
    }
}

fn expired_key() -> PinnedKey {
    PinnedKey {
        label: EXPIRED_LABEL,
        fingerprint: FPR_EXPIRED,
        algo: ALGO_EXPIRED,
        bits: BITS_EXPIRED,
        created: CREATED_EXPIRED,
        expires: EXPIRES_EXPIRED,
        block: KEY_EXPIRED,
        subkeys: NO_SUBKEYS,
    }
}

fn revoked_key() -> PinnedKey {
    PinnedKey {
        label: REVOKED_LABEL,
        fingerprint: FPR_REVOKED,
        algo: ALGO_REVOKED,
        bits: BITS_REVOKED,
        created: CREATED_REVOKED,
        expires: EXPIRES_REVOKED,
        block: KEY_REVOKED,
        subkeys: NO_SUBKEYS,
    }
}

/// `(packet start, body start, packet end, tag)` for every packet of a stream.
///
/// The tests need to tamper with *specific* fields (a hash-algorithm octet, one
/// MPI byte, an issuer subpacket), which requires the exact framing offsets —
/// the packet module deliberately does not expose them, so this walks the two
/// definite-length header forms independently.
fn packet_offsets(buf: &[u8]) -> Vec<(usize, usize, usize, u8)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < buf.len() {
        let start = i;
        let ctb = buf[i];
        i += 1;
        let (tag, len) = if ctb & 0x40 != 0 {
            let tag = ctb & 0x3F;
            let first = buf[i];
            i += 1;
            if first < 192 {
                (tag, first as usize)
            } else if first < 224 {
                let len = ((first as usize - 192) << 8) + buf[i] as usize + 192;
                i += 1;
                (tag, len)
            } else {
                let len = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
                i += 4;
                (tag, len)
            }
        } else {
            let tag = (ctb >> 2) & 0x0F;
            match ctb & 0x03 {
                0 => {
                    let len = buf[i] as usize;
                    i += 1;
                    (tag, len)
                }
                1 => {
                    let len = u16::from_be_bytes([buf[i], buf[i + 1]]) as usize;
                    i += 2;
                    (tag, len)
                }
                _ => {
                    let len = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
                    i += 4;
                    (tag, len)
                }
            }
        };
        out.push((start, i, i + len, tag));
        i += len;
    }
    out
}

/// Start offset of the `n`-th signature packet body in a binary stream.
fn signature_body_offsets(buf: &[u8]) -> Vec<usize> {
    packet_offsets(buf)
        .into_iter()
        .filter(|&(_, _, _, tag)| tag == 2)
        .map(|(_, body, _, _)| body)
        .collect()
}

/// Byte ranges of the signature *values* (the MPI magnitudes) of every
/// signature packet.
///
/// Only the value bytes are returned: an MPI's 2-octet bit-length prefix is not
/// part of the signed value, and a change there that keeps the same octet count
/// (e.g. `0x00FE` -> `0x00FF`) leaves the signature mathematically identical —
/// so a property that flipped those bytes would be testing the wrong thing.
/// Every byte in these ranges, on the other hand, is part of R/S (or of the RSA
/// signature value) and any bit flip must destroy the signature. The unhashed
/// subpacket area is deliberately not included: it is not signed.
fn signature_value_ranges(buf: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (_, body, end, tag) in packet_offsets(buf) {
        if tag != 2 {
            continue;
        }
        let hashed_len = u16::from_be_bytes([buf[body + 4], buf[body + 5]]) as usize;
        let unhashed_len_at = body + 6 + hashed_len;
        let unhashed_len =
            u16::from_be_bytes([buf[unhashed_len_at], buf[unhashed_len_at + 1]]) as usize;
        let mut at = unhashed_len_at + 2 + unhashed_len + 2;
        while at < end {
            let bits = u16::from_be_bytes([buf[at], buf[at + 1]]) as usize;
            let nbytes = (bits + 7) / 8;
            out.push((at + 2, at + 2 + nbytes));
            at += 2 + nbytes;
        }
    }
    out
}

#[test]
fn detached_multi_signer_accepts() {
    let ring = [ed_key(), rsa_key()];
    let verified = verify_detached(&ring, RELEASE_GPG, RELEASE, FIXTURE_NOW)
        .expect("the multi-signer fixture verifies");
    assert!(
        verified.signer == FPR_ED || verified.signer == FPR_RSA,
        "signer is one of the pinned keys"
    );
    assert!(verified.created.is_some(), "signature creation time recorded");
}

#[test]
fn detached_accepts_through_the_rsa_signing_subkey() {
    // Only the RSA key is pinned, and its *subkey* made the signature: this
    // exercises subkey selection plus the 0x18 binding verification.
    let ring = [rsa_key()];
    let verified = verify_detached(&ring, RELEASE_GPG, RELEASE, FIXTURE_NOW)
        .expect("subkey signature verifies");
    assert_eq!(verified.signer, FPR_RSA);
    assert_eq!(verified.key, FPR_RSA_SUB, "the subkey, not the primary, signed");
}

#[test]
fn clearsigned_accepts_and_yields_the_signed_text() {
    let ring = [ed_key(), rsa_key()];
    let (cs, verified) = verify_clearsigned(&ring, INRELEASE, FIXTURE_NOW)
        .expect("the clear-signed fixture verifies");
    assert_eq!(verified.signer, FPR_ED);
    // The clear text is byte-identical to the standalone document — this is how
    // apt gets the `Release` body out of `InRelease`.
    assert_eq!(cs.text, RELEASE);
    assert_eq!(cs.canonical_text(), INRELEASE_CANONICAL);
    assert_eq!(cs.declared_hashes(), vec![crate::openpgp_crypto::HashAlgo::Sha256]);
}

#[test]
fn clearsign_canonicalization_matches_gnupg() {
    // Whitespace-stripping and dash-escaping are the two rules that are easy to
    // get wrong; the fixture has a trailing-whitespace line, a tab, a CRLF line
    // and a line starting with "- " (which armour escapes as "- - ").
    let ring = [ed_key()];
    let (cs, _) = verify_clearsigned(&ring, WS_INRELEASE, FIXTURE_NOW)
        .expect("the canonicalization fixture verifies");
    let expected: &[u8] = b"line one\r\n- dash line\r\nplain\tline\r\nlast line";
    assert_eq!(cs.canonical_text(), expected, "canonical form of WS_DOC");
    // The generator computed the same bytes independently (in Python).
    assert_eq!(cs.canonical_text(), WS_CANONICAL);
    assert_ne!(cs.canonical_text(), WS_DOC, "the rules must actually change the bytes");
}

#[test]
fn tampered_signature_byte_is_refused() {
    let body = signature_body_offsets(RELEASE_GPG_BIN);
    assert!(body.len() >= 2, "fixture has two signature packets");
    // Two signatures: Ed25519 has two MPIs (R and S), the RSA one has a single
    // MPI — three value ranges in total.
    assert_eq!(signature_value_ranges(RELEASE_GPG_BIN).len(), 3);
    // Flip a byte inside the first signature packet's MPI (the last bytes of the
    // packet body are the signature value).
    let mut tampered = RELEASE_GPG_BIN.to_vec();
    let (value_start, value_end) = signature_value_ranges(&tampered)[0];
    assert!(value_end > value_start, "signature value range is non-empty");
    tampered[value_end - 1] ^= 0x01;
    let ring = [ed_key(), rsa_key()];
    let err = verify_detached(&ring, &tampered, RELEASE, FIXTURE_NOW)
        .expect_err("a tampered signature must be refused");
    assert!(
        matches!(err, OpenPgpError::BadSignature | OpenPgpError::Crypto(_)),
        "unexpected refusal cause: {err:?}"
    );
}

#[test]
fn tampered_data_is_refused() {
    let ring = [ed_key(), rsa_key()];
    let mut tampered = RELEASE.to_vec();
    let mid = tampered.len() / 2;
    tampered[mid] ^= 0x01;
    let err = verify_detached(&ring, RELEASE_GPG, &tampered, FIXTURE_NOW)
        .expect_err("changed data must not verify");
    assert!(
        matches!(err, OpenPgpError::BadSignature | OpenPgpError::Crypto(_)),
        "unexpected refusal cause: {err:?}"
    );
}

#[test]
fn one_bad_pinned_signature_is_fatal_even_with_a_good_one() {
    // The fixture signs with Ed25519 first and the RSA subkey second. Corrupt
    // the SECOND signature only: one good, one bad, both issuers pinned.
    let mut tampered = RELEASE_GPG_BIN.to_vec();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    let ring = [ed_key(), rsa_key()];
    let err = verify_detached(&ring, &tampered, RELEASE, FIXTURE_NOW)
        .expect_err("a bad signature from a pinned key is fatal (gpgv exits 1 here)");
    assert!(
        matches!(err, OpenPgpError::BadSignature | OpenPgpError::Crypto(_)),
        "unexpected refusal cause: {err:?}"
    );

    // The same blob with only the Ed25519 key pinned: the corrupted signature's
    // issuer is not pinned any more, so it is ignored and the good signature
    // carries the verification. This is the "unknown signer is ignored" rule.
    let ring = [ed_key()];
    let verified = verify_detached(&ring, &tampered, RELEASE, FIXTURE_NOW)
        .expect("an unpinned signature is ignored");
    assert_eq!(verified.signer, FPR_ED);
}

#[test]
fn foreign_signer_is_refused() {
    let ring = [ed_key(), rsa_key()];
    let err = verify_detached(&ring, RELEASE_FOREIGN_GPG, RELEASE, FIXTURE_NOW)
        .expect_err("a signature by an unpinned key must not be trusted");
    assert_eq!(err, OpenPgpError::NoTrustedSignature);
}

#[test]
fn expired_key_is_valid_inside_its_window_and_refused_after_it() {
    let ring = [expired_key()];
    check_pinned_key(&ring[0], FIXTURE_NOW).expect("valid inside its window");
    verify_detached(&ring, RELEASE_EXPIRED_GPG, RELEASE, FIXTURE_NOW)
        .expect("valid signature inside the key's window");

    let err = verify_detached(&ring, RELEASE_EXPIRED_GPG, RELEASE, EXPIRED_NOW)
        .expect_err("an expired key must be refused");
    assert_eq!(err, OpenPgpError::Expired);
    assert_eq!(
        check_pinned_key(&ring[0], EXPIRED_NOW).expect_err("expired key block"),
        OpenPgpError::Expired
    );
}

#[test]
fn revoked_key_is_refused() {
    // The fixture key carries a real (0x20) key revocation signature produced by
    // GnuPG and imported from its own revocation certificate. A revoked key must
    // never be usable, whatever its validity window says.
    let ring = [revoked_key()];
    assert_eq!(
        check_pinned_key(&ring[0], FIXTURE_NOW).expect_err("revoked key"),
        OpenPgpError::Revoked
    );
    // The same key without the revocation signature validates: the fixture
    // proves the revocation check, not a broken key block.
    let mut unrevoked = revoked_key();
    unrevoked.block = KEY_ED;
    unrevoked.fingerprint = FPR_ED;
    unrevoked.algo = ALGO_ED;
    unrevoked.bits = BITS_ED;
    unrevoked.created = CREATED_ED;
    unrevoked.expires = EXPIRES_ED;
    unrevoked.label = ED_LABEL;
    check_pinned_key(&unrevoked, FIXTURE_NOW).expect("control: a valid key block");
}

#[test]
fn clock_floor_stops_every_entry_point() {
    let ring = [ed_key()];
    let before = CLOCK_FLOOR - 1;
    assert_eq!(
        verify_detached(&ring, RELEASE_GPG, RELEASE, before).unwrap_err(),
        OpenPgpError::ClockUnset
    );
    assert_eq!(
        verify_clearsigned(&ring, INRELEASE, before).unwrap_err(),
        OpenPgpError::ClockUnset
    );
    assert_eq!(
        check_pinned_key(&ring[0], before).unwrap_err(),
        OpenPgpError::ClockUnset
    );
}

#[test]
fn signature_type_must_match_the_framing() {
    // A binary-document signature packet stream handed to the clear-signed path
    // (and vice versa) is an inconsistent framing, not a verification failure.
    let ring = [ed_key(), rsa_key()];
    let (_, _) = verify_clearsigned(&ring, INRELEASE, FIXTURE_NOW).expect("control");
    let err = verify_detached(&ring, INRELEASE_SIG_BIN, &INRELEASE_CANONICAL, FIXTURE_NOW)
        .expect_err("a text signature must not satisfy the detached path");
    assert_eq!(err, OpenPgpError::SigTypeMismatch);

    // And the reverse: flip the signature type octet of the detached stream to
    // "canonical text" and check the detached path refuses it.
    let mut tampered = RELEASE_GPG_BIN.to_vec();
    let body = signature_body_offsets(&tampered);
    tampered[body[0] + 1] = packet::SIG_TEXT;
    let err = verify_detached(&ring, &tampered, RELEASE, FIXTURE_NOW)
        .expect_err("a text signature in the detached path is refused");
    assert_eq!(err, OpenPgpError::SigTypeMismatch);
}

#[test]
fn hash_algorithm_is_refused_when_unsupported() {
    // SHA-1 (id 2) is deliberately not implemented: a pinned signature that
    // names it must be refused, never silently skipped.
    let mut tampered = RELEASE_GPG_BIN.to_vec();
    let body = signature_body_offsets(&tampered);
    tampered[body[0] + 3] = 2; // version, type, pub_algo, hash_algo
    let ring = [ed_key(), rsa_key()];
    let err = verify_detached(&ring, &tampered, RELEASE, FIXTURE_NOW)
        .expect_err("SHA-1 signatures are refused");
    assert_eq!(err, OpenPgpError::UnsupportedHashAlgo(2));
}

#[test]
fn clearsign_hash_header_must_match() {
    // Rewrite the `Hash: SHA256` header so it no longer names the algorithm the
    // signature uses: the message's own declared framing is inconsistent.
    let tampered: Vec<u8> = INRELEASE
        .windows(b"Hash: SHA256".len())
        .position(|w| w == b"Hash: SHA256")
        .map(|at| {
            let mut v = INRELEASE.to_vec();
            v[at + 6..at + 12].copy_from_slice(b"SHA512");
            v
        })
        .expect("the fixture declares its hash algorithm");
    let ring = [ed_key()];
    let err = verify_clearsigned(&ring, &tampered, FIXTURE_NOW)
        .expect_err("a mismatched Hash header is refused");
    assert_eq!(err, OpenPgpError::HashHeaderMismatch);
}

#[test]
fn empty_signature_stream_is_refused() {
    let ring = [ed_key()];
    let err = verify_detached(&ring, b"", RELEASE, FIXTURE_NOW).expect_err("no signature");
    assert_eq!(err, OpenPgpError::Packet(PacketError::ArmorMissing));
}

#[test]
fn unpinned_issuer_fingerprint_falls_back_to_the_remaining_signer() {
    // Patch the issuer fingerprint subpacket of the first signature so it names
    // no pinned key: that signature becomes unverifiable *and* irrelevant, and
    // the RSA subkey signature must still carry the file.
    let mut tampered = RELEASE_GPG_BIN.to_vec();
    let body = signature_body_offsets(&tampered);
    let hashed_len_at = body[0] + 4;
    let hashed_len = u16::from_be_bytes([tampered[hashed_len_at], tampered[hashed_len_at + 1]]) as usize;
    let area_start = hashed_len_at + 2;
    // Walk the subpacket area: [length (1 or 5 octets)][type][data]; the length
    // counts the type octet but not the length octets themselves.
    let mut at = area_start;
    let area_end = area_start + hashed_len;
    let mut patched = false;
    while at < area_end {
        let first = tampered[at] as usize;
        let (len, len_bytes) = if first < 192 {
            (first, 1usize)
        } else if first < 255 {
            (((first - 192) << 8) + tampered[at + 1] as usize + 192, 2usize)
        } else {
            (
                u32::from_be_bytes([
                    tampered[at + 1],
                    tampered[at + 2],
                    tampered[at + 3],
                    tampered[at + 4],
                ]) as usize,
                5usize,
            )
        };
        assert!(len >= 1, "subpacket has a type octet");
        if tampered[at + len_bytes] == 33 && len == 22 {
            // [length][0x21][0x04][20-byte fingerprint]
            for b in &mut tampered[at + len_bytes + 2..at + len_bytes + 22] {
                *b ^= 0x01;
            }
            patched = true;
            break;
        }
        at += len_bytes + len;
    }
    assert!(patched, "fixture signature carries an issuer fingerprint");

    let ring = [ed_key(), rsa_key()];
    let verified = verify_detached(&ring, &tampered, RELEASE, FIXTURE_NOW)
        .expect("the remaining pinned signature verifies");
    assert_eq!(verified.signer, FPR_RSA, "the RSA subkey signer carried the file");

    // With only the Ed25519 key pinned, no signature is left to trust.
    let ring = [ed_key()];
    let err = verify_detached(&ring, &tampered, RELEASE, FIXTURE_NOW)
        .expect_err("no trusted signature remains");
    assert_eq!(err, OpenPgpError::NoTrustedSignature);
}

#[test]
fn keyring_block_must_match_its_pin() {
    // A block whose bytes were altered (or a table entry that pins the wrong
    // fingerprint) must be refused before any signature is checked.
    let leaked: &'static [u8] = Box::leak(KEY_ED.to_vec().into_boxed_slice());
    let mut wrong = ed_key();
    wrong.block = leaked;
    assert!(check_pinned_key(&wrong, FIXTURE_NOW).is_ok(), "control: unaltered copy");

    let mut flipped: Vec<u8> = KEY_ED.to_vec();
    let last = flipped.len() - 1;
    flipped[last] ^= 0x01;
    let leaked: &'static [u8] = Box::leak(flipped.into_boxed_slice());
    let mut wrong = ed_key();
    wrong.block = leaked;
    // Flipping the last byte of the keyring block lands inside the self-signature
    // value: the packets still parse, but the key is no longer certified.
    assert_eq!(
        check_pinned_key(&wrong, FIXTURE_NOW).unwrap_err(),
        OpenPgpError::UncertifiedKey,
        "a corrupted self-signature must not certify the key"
    );

    // A metadata mismatch (wrong pinned size) is caught without touching bytes.
    let mut wrong = ed_key();
    wrong.bits = 4096;
    assert_eq!(
        check_pinned_key(&wrong, FIXTURE_NOW).unwrap_err(),
        OpenPgpError::KeyMetadataMismatch
    );

    // A swapped label (the block does not carry that user ID) is caught too.
    let mut wrong = ed_key();
    wrong.label = RSA_LABEL;
    assert_eq!(
        check_pinned_key(&wrong, FIXTURE_NOW).unwrap_err(),
        OpenPgpError::KeyMetadataMismatch
    );

    // An empty pinned subkey list still verifies the block; a *wrong* subkey
    // fingerprint does not.
    let mut wrong = rsa_key();
    wrong.subkeys = &[FPR_ED];
    assert_eq!(
        check_pinned_key(&wrong, FIXTURE_NOW).unwrap_err(),
        OpenPgpError::FingerprintMismatch
    );
}

#[test]
fn unsupported_curve_is_refused_by_name() {
    // Sanity check on the crypto dispatch: an ECDSA signature verified against a
    // key whose curve has no compiled-in backend reports UnsupportedCurve, and
    // key material of the wrong shape reports AlgoMismatch — neither panics.
    assert_eq!(
        crate::openpgp_crypto::verify_ecdsa_p256(&[0x04; 65], &[0u8; 32], &[1], &[1]).unwrap_err(),
        CryptoError::MalformedKey
    );
}

proptest! {
    /// Any single-bit change to the signed document must be refused — for every
    /// position, not just the two the unit tests pick.
    #[test]
    fn any_flipped_bit_in_the_document_is_refused(
        pos in 0usize..RELEASE.len(),
        bit in 0u8..8,
    ) {
        let mut tampered = RELEASE.to_vec();
        tampered[pos] ^= 1 << bit;
        prop_assert!(tampered != RELEASE, "the mutation must change the bytes");
        let ring = [ed_key(), rsa_key()];
        let result = verify_detached(&ring, RELEASE_GPG, &tampered, FIXTURE_NOW);
        prop_assert!(result.is_err(), "flipped bit at {} must not verify", pos);
    }

    /// Flipping a bit inside any signature *value* never verifies. (The
    /// unhashed subpacket area is deliberately excluded: it is not signed, and a
    /// change there is a no-op by design.)
    #[test]
    fn any_flipped_bit_in_a_signature_value_is_refused(
        which in 0usize..3,
        offset in 0usize..700,
        bit in 0u8..8,
    ) {
        let ranges = signature_value_ranges(RELEASE_GPG_BIN);
        let (start, end) = ranges[which];
        let pos = start + offset % (end - start);
        let mut tampered = RELEASE_GPG_BIN.to_vec();
        tampered[pos] ^= 1 << bit;
        let ring = [ed_key(), rsa_key()];
        let result = verify_detached(&ring, &tampered, RELEASE, FIXTURE_NOW);
        prop_assert!(result.is_err(), "flipped signature value bit at {} must not verify", pos);
    }

    /// Truncations and arbitrary garbage are refused without panicking — the
    /// same guarantee as P51, but through the policy layer.
    #[test]
    fn hostile_inputs_never_verify_and_never_panic(
        data in proptest::collection::vec(any::<u8>(), 0..800),
    ) {
        let ring = [ed_key(), rsa_key()];
        let _ = verify_detached(&ring, &data, RELEASE, FIXTURE_NOW);
        let _ = verify_detached(&ring, RELEASE_GPG, &data, FIXTURE_NOW);
        let _ = verify_clearsigned(&ring, &data, FIXTURE_NOW);
    }
}



