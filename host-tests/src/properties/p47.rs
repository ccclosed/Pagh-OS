// Feature: TLS server authentication (issue #14), Property 47: the chain
// builder (`tls_chain`) accepts exactly a complete, correctly signed path to
// a trust anchor — and fails closed on every broken variant.
//
// Synthetic chains are generated on the host with ECDSA P-256 keypairs
// (deterministic `DetRng` seed from proptest, same generator as P46):
// root (self-signed) → intermediate (cA=TRUE) → leaf (SAN dNSName). The
// certificates are hand-encoded DER of exactly the shapes
// `x509::parse_certificate` accepts, signed with the ISSUER's key, so the
// property exercises the same code path a real Debian mirror chain takes.
//
// Covered classes (each a hard reject when broken):
//   * good chain (leaf+intermediate+anchor, and leaf-only to the anchor) → Ok;
//   * extra unused intermediate in the DER → still Ok (RFC 8446 servers may
//     send unneeded entries; unused entries are never trusted);
//   * missing intermediate → NoAnchor;
//   * expired leaf / not-yet-valid leaf → Expired / NotYetValid;
//   * issuer without basicConstraints cA=TRUE → NotCa;
//   * anchor with the right NAME but the WRONG KEY → Verify(VerifyFailed);
//   * clock below the 2025 floor → Clock (checked before anything else).

use crate::tls_chain::{ChainError, CLOCK_FLOOR, TrustAnchor, verify_chain};
use crate::tls_verify::SigVerifyError;
use crate::x509::SpkiKey;
use proptest::prelude::*;
use signature::Signer;

use super::det_rng::rng_from;

// ─────────────────────── DER encoding helpers ───────────────────────

/// Encode one DER TLV with a minimal-length definite length.
fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
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

/// A `Name` with a single CN RDN (byte-compared by the chain builder, so the
/// EXACT same encoding must be reused as a child's issuer and a parent's
/// subject — this helper guarantees that by construction).
fn name(cn: &[u8]) -> Vec<u8> {
    // Name ::= SEQUENCE { SET { SEQUENCE { OID 2.5.4.3, UTF8String cn } } }
    let rdn_set = tlv(
        0x31,
        &tlv(
            0x30,
            &[
                tlv(0x06, &[0x55, 0x04, 0x03]).as_slice(),
                tlv(0x0c, cn).as_slice(),
            ]
            .concat(),
        ),
    );
    tlv(0x30, &rdn_set)
}

/// ECDSA-with-SHA256 `AlgorithmIdentifier` element (no parameters).
fn ecdsa_sig_alg() -> Vec<u8> {
    tlv(
        0x30,
        &tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02]),
    )
}

/// P-256 SPKI element from an uncompressed 65-byte point.
fn spki_p256(point: &[u8]) -> Vec<u8> {
    let mut bits = vec![0x00u8];
    bits.extend_from_slice(point);
    tlv(
        0x30,
        &[
            tlv(
                0x30,
                &[
                    tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]).as_slice(),
                    tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]).as_slice(),
                ]
                .concat(),
            )
            .as_slice(),
            tlv(0x03, &bits).as_slice(),
        ]
        .concat(),
    )
}

/// `Validity ::= SEQUENCE { UTCTime, UTCTime }` (RFC 5280 shapes).
fn validity(not_before: &[u8], not_after: &[u8]) -> Vec<u8> {
    tlv(
        0x30,
        &[
            tlv(0x17, not_before).as_slice(),
            tlv(0x17, not_after).as_slice(),
        ]
        .concat(),
    )
}

/// `Extension ::= SEQUENCE { OID, critical BOOLEAN?, OCTET STRING }`.
fn extension(oid: &[u8], critical: bool, value_der: &[u8]) -> Vec<u8> {
    let mut fields = vec![tlv(0x06, oid)];
    if critical {
        fields.push(tlv(0x01, &[0xff]));
    }
    fields.push(tlv(0x04, value_der));
    tlv(0x30, &fields.concat())
}

/// basicConstraints with `cA` as given (a CA cert MUST carry it explicitly).
fn basic_constraints(ca: bool) -> Vec<u8> {
    if ca {
        tlv(0x30, &tlv(0x01, &[0xff]))
    } else {
        tlv(0x30, &tlv(0x01, &[0x00]))
    }
}

/// subjectAltName with one dNSName entry.
fn san_dns(host: &[u8]) -> Vec<u8> {
    tlv(0x30, &tlv(0x82, host))
}

/// Build the TBSCertificate for one chain level and return the complete
/// signed certificate DER. `subject`/`issuer` are pre-encoded `Name` DERs;
/// `exts` are the raw Extension elements (empty → no extensions field);
/// `signer` is the ISSUER's key.
fn build_cert(
    serial: u8,
    subject: &[u8],
    issuer: &[u8],
    point: &[u8],
    not_before: &[u8],
    not_after: &[u8],
    exts: &[Vec<u8>],
    signer: &p256::ecdsa::SigningKey,
) -> Vec<u8> {
    let sig_alg = ecdsa_sig_alg();
    let mut fields = vec![
        // [0] EXPLICIT version v3.
        tlv(0xa0, &tlv(0x02, &[0x02])),
        tlv(0x02, &[serial]),
        sig_alg.clone(),
        issuer.to_vec(),
        validity(not_before, not_after),
        subject.to_vec(),
        spki_p256(point),
    ];
    if !exts.is_empty() {
        let ext_seq: Vec<u8> = exts.concat();
        fields.push(tlv(0xa3, &tlv(0x30, &ext_seq)));
    }
    let tbs = tlv(0x30, &fields.concat());
    // ECDSA signature over the EXACT TBS element bytes, DER-encoded r,s.
    let sig: p256::ecdsa::DerSignature = signer.sign(&tbs);
    // BIT STRING content: 0x00 unused-bits byte || raw DER signature blob.
    let mut sig_bits = vec![0x00u8];
    sig_bits.extend_from_slice(sig.as_ref());
    tlv(
        0x30,
        &[
            tbs.as_slice(),
            sig_alg.as_slice(),
            tlv(0x03, &sig_bits).as_slice(),
        ]
        .concat(),
    )
}

/// A fully-built three-level chain (root/intermediate/leaf).
struct TestChain {
    der: Vec<u8>,               // leaf || intermediate (what a TLS server sends)
    root_der: Vec<u8>,          // self-signed root (only used as an "extra" cert)
    anchor: (Vec<u8>, Vec<u8>), // (subject DER, uncompressed key point)
}

/// Generate a deterministic three-level P-256 chain. `leaf_not_before` /
/// `leaf_not_after` override the leaf validity; `inter_ca` decides whether
/// the intermediate carries cA=TRUE (broken-variant hooks).
fn build_chain(
    seed: u64,
    not_before: &[u8],
    not_after: &[u8],
    leaf_not_before: Option<&[u8]>,
    leaf_not_after: Option<&[u8]>,
    inter_ca: bool,
) -> TestChain {
    let mut rng = rng_from(seed);
    let sk_root = p256::ecdsa::SigningKey::random(&mut rng);
    let sk_inter = p256::ecdsa::SigningKey::random(&mut rng);
    let sk_leaf = p256::ecdsa::SigningKey::random(&mut rng);

    let root_name = name(b"Test Root CA");
    let inter_name = name(b"Test Intermediate CA");
    let leaf_name = name(b"pagh-test.invalid");

    let root_point = sk_root.verifying_key().to_encoded_point(false);
    let inter_point = sk_inter.verifying_key().to_encoded_point(false);
    let leaf_point = sk_leaf.verifying_key().to_encoded_point(false);

    // Root: self-signed, cA=TRUE.
    let root_der = build_cert(
        0x01,
        &root_name,
        &root_name,
        root_point.as_bytes(),
        not_before,
        not_after,
        &[extension(&[0x55, 0x1d, 0x13], true, &basic_constraints(true))],
        &sk_root,
    );

    // Intermediate: signed by the root, cA=TRUE only when requested.
    let inter_ca_ext = if inter_ca {
        extension(&[0x55, 0x1d, 0x13], true, &basic_constraints(true))
    } else {
        extension(&[0x55, 0x1d, 0x13], true, &basic_constraints(false))
    };
    let inter_der = build_cert(
        0x02,
        &inter_name,
        &root_name,
        inter_point.as_bytes(),
        not_before,
        not_after,
        &[inter_ca_ext],
        &sk_root,
    );

    // Leaf: signed by the intermediate, SAN dNSName, not a CA.
    let leaf_der = build_cert(
        0x03,
        &leaf_name,
        &inter_name,
        leaf_point.as_bytes(),
        leaf_not_before.unwrap_or(not_before),
        leaf_not_after.unwrap_or(not_after),
        &[extension(&[0x55, 0x1d, 0x11], false, &san_dns(b"pagh.test"))],
        &sk_inter,
    );

    let mut der = leaf_der;
    der.extend_from_slice(&inter_der);

    TestChain {
        der,
        root_der,
        anchor: (root_name.clone(), root_point.as_bytes().to_vec()),
    }
}

/// The trust anchor for a generated chain (subject DER + P-256 key).
fn anchor_of(chain: &TestChain) -> TrustAnchor<'_> {
    TrustAnchor {
        subject: &chain.anchor.0,
        key: SpkiKey::EcP256 {
            point: &chain.anchor.1,
        },
    }
}

/// "Now" comfortably inside every default validity window (2025-06).
const NOW: i64 = 1_750_000_000;
/// Default validity: 2025-01-01 .. 2035-01-01 (UTCTime).
const NB: &[u8] = b"250101000000Z";
const NA: &[u8] = b"350101000000Z";

proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(8))]

    /// A good two-level path (leaf → intermediate → anchor) verifies, with
    /// AND without the extra self-signed root appended (RFC 8446 extra
    /// entries), and a leaf signed directly by the anchor also verifies.
    #[test]
    fn good_chain_accepts(seed in any::<u64>(), with_extra in any::<bool>()) {
        let chain = build_chain(seed, NB, NA, None, None, true);
        let anchor = anchor_of(&chain);

        let mut der_with_extra = chain.der.clone();
        der_with_extra.extend_from_slice(&chain.root_der);
        let input = if with_extra { der_with_extra } else { chain.der.clone() };
        prop_assert_eq!(verify_chain(&[anchor], &input, NOW), Ok(()));

        // Leaf-only path straight to the anchor: re-issue the leaf with the
        // root as its direct issuer (fresh deterministic keypairs, leaf
        // signed by the root key).
        let mut rng = rng_from(seed ^ 0xdeadbeef);
        let sk_root = p256::ecdsa::SigningKey::random(&mut rng);
        let sk_leaf = p256::ecdsa::SigningKey::random(&mut rng);
        let root_name = name(b"Direct Root CA");
        let leaf_name = name(b"direct.paghsuite");
        let root_point = sk_root.verifying_key().to_encoded_point(false);
        let leaf_point = sk_leaf.verifying_key().to_encoded_point(false);
        let leaf_der = build_cert(
            0x02, &leaf_name, &root_name, leaf_point.as_bytes(), NB, NA,
            &[extension(&[0x55, 0x1d, 0x11], false, &san_dns(b"direct.paghsuite"))], &sk_root,
        );
        let direct_anchor = TrustAnchor {
            subject: &root_name,
            key: SpkiKey::EcP256 { point: root_point.as_bytes() },
        };
        prop_assert_eq!(verify_chain(&[direct_anchor], &leaf_der, NOW), Ok(()));
    }

    /// Broken chains fail closed with the EXACT error class: missing
    /// intermediate → NoAnchor; expired / not-yet-valid leaf → Expired /
    /// NotYetValid; issuer without cA=TRUE → NotCa.
    #[test]
    fn broken_chain_classes(seed in any::<u64>()) {
        // Missing intermediate: only the leaf is sent, the anchor is the
        // root. The leaf ends where the second certificate begins — found
        // via parse_certificate's tail (bytes AFTER the certificate).
        let chain = build_chain(seed, NB, NA, None, None, true);
        let anchor = anchor_of(&chain);
        let (_cert, rest) = crate::x509::parse_certificate(&chain.der).unwrap();
        let leaf_len = chain.der.len() - rest.len();
        let leaf_only = chain.der[..leaf_len].to_vec();
        prop_assert_eq!(
            verify_chain(&[anchor], &leaf_only, NOW),
            Err(ChainError::NoAnchor)
        );

        // Expired leaf (notAfter 2024 < NOW).
        let expired = build_chain(seed, NB, NA, None, Some(b"240101000000Z"), true);
        prop_assert_eq!(
            verify_chain(&[anchor_of(&expired)], &expired.der, NOW),
            Err(ChainError::Expired)
        );

        // Not-yet-valid leaf (notBefore 2030 > NOW).
        let future = build_chain(seed, NB, NA, Some(b"300101000000Z"), None, true);
        prop_assert_eq!(
            verify_chain(&[anchor_of(&future)], &future.der, NOW),
            Err(ChainError::NotYetValid)
        );

        // Intermediate without cA=TRUE: the signature is fine, the issuer
        // simply is not a CA — fail closed.
        let no_ca = build_chain(seed, NB, NA, None, None, false);
        prop_assert_eq!(
            verify_chain(&[anchor_of(&no_ca)], &no_ca.der, NOW),
            Err(ChainError::NotCa)
        );
    }

    /// A hostile anchor (correct subject NAME, wrong KEY) can never satisfy
    /// the path: the signature check fails, surfaced as Verify(VerifyFailed).
    #[test]
    fn wrong_anchor_key_rejected(seed in any::<u64>()) {
        let chain = build_chain(seed, NB, NA, None, None, true);
        let mut rng = rng_from(seed ^ 0x5eed);
        let attacker = p256::ecdsa::SigningKey::random(&mut rng);
        let attacker_point = attacker.verifying_key().to_encoded_point(false);
        let fake_anchor = TrustAnchor {
            subject: &chain.anchor.0, // same NAME...
            key: SpkiKey::EcP256 { point: attacker_point.as_bytes() }, // ...wrong KEY
        };
        prop_assert_eq!(
            verify_chain(&[fake_anchor], &chain.der, NOW),
            Err(ChainError::Verify(SigVerifyError::VerifyFailed))
        );
    }

    /// The clock gate fires BEFORE any parsing: below the 2025 floor the
    /// answer is always Clock, even for garbage DER, and the leaf's own
    /// validity window being wide open does not rescue it.
    #[test]
    fn clock_gate_fires_first(
        seed in any::<u64>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let chain = build_chain(seed, NB, NA, None, None, true);
        let anchor = anchor_of(&chain);
        prop_assert_eq!(
            verify_chain(&[anchor], &chain.der, CLOCK_FLOOR - 1),
            Err(ChainError::Clock)
        );
        // Garbage input still hits the gate first (Clock, not IncompleteChain).
        let empty_anchors: [TrustAnchor<'static>; 0] = [];
        prop_assert_eq!(
            verify_chain(&empty_anchors, &garbage, CLOCK_FLOOR - 1),
            Err(ChainError::Clock)
        );
        prop_assert_eq!(
            verify_chain(&empty_anchors, &garbage, NOW),
            Err(ChainError::IncompleteChain)
        );
    }
}
