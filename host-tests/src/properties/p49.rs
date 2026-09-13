// Feature: TLS server authentication (issue #14), Property 49: the one-shot
// server-authentication decision layer (`tls_auth`) accepts exactly a chain
// that validates AND authorizes the connection target — and exports a leaf
// key that actually verifies the TLS 1.3 `CertificateVerify` signature.
//
// P47 already proves the chain builder's exact accept/reject classes; P49
// treats `verify_chain` as a component and covers what this layer ADDS:
//
//   * hostname authorization: exact/case-insensitive dNSName match, wildcard
//     SAN (one label), iPAddress match for an IPv4-literal host;
//   * fail-closed hostname classes: wrong host, deeper-than-wildcard host,
//     IP-host vs DNS-only SAN (no cross-type matching), missing SAN
//     extension (no CN fallback), missing host (`None` → NoHostname);
//   * chain failures surface untouched: Expired, Clock below the floor;
//   * extra unneeded entries in the message stay ignored (Ok);
//   * CertificateVerify round-trips: ECDSA P-256, Ed25519 and RSA-PSS-2048
//     signatures over the exact RFC 8446 §4.4.3 message verify against the
//     exported leaf key; tampered bytes, wrong keys, scheme/key mismatches
//     and wrong transcript-hash lengths all reject.

use crate::tls_auth::{
    authenticate_server, certificate_verify_message, verify_certificate_verify, AuthError,
    LeafKey, Tls13Scheme,
};
use crate::tls_chain::{ChainError, CLOCK_FLOOR, TrustAnchor};
use crate::x509::SpkiKey;
use proptest::prelude::*;
use signature::{RandomizedSigner, SignatureEncoding, Signer};

use super::chain_der::{
    basic_constraints, build_cert, extension, name, san_dns, san_ip, san_wildcard, NA, NB,
    NOW, OID_BASIC_CONSTRAINTS, OID_SAN,
};
use super::det_rng::rng_from;

/// One leaf certificate signed directly by a generated self-signed root
/// (the minimal chain this layer's hostname/leaf-key logic needs; the full
/// path-building classes are P47's subject).
struct AuthChain {
    leaf_der: Vec<u8>,
    root_der: Vec<u8>,
    anchor_name: Vec<u8>,
    anchor_point: Vec<u8>,
    leaf_point: Vec<u8>,
    leaf_sk: p256::ecdsa::SigningKey,
}

/// Build a root + leaf with the given leaf SAN extension element (`None` →
/// no extensions field at all, the missing-SAN fail-closed case).
fn build_auth_chain(seed: u64, leaf_san: Option<Vec<u8>>) -> AuthChain {
    let mut rng = rng_from(seed);
    let sk_root = p256::ecdsa::SigningKey::random(&mut rng);
    let sk_leaf = p256::ecdsa::SigningKey::random(&mut rng);

    let root_name = name(b"Auth Root CA");
    let leaf_name = name(b"leaf.auth.invalid");
    let root_point = sk_root.verifying_key().to_encoded_point(false);
    let leaf_point = sk_leaf.verifying_key().to_encoded_point(false);

    let root_der = build_cert(
        0x01,
        &root_name,
        &root_name,
        root_point.as_bytes(),
        NB,
        NA,
        &[extension(OID_BASIC_CONSTRAINTS, true, &basic_constraints(true))],
        &sk_root,
    );
    let leaf_exts: &[Vec<u8>] = match &leaf_san {
        Some(san) => &[extension(OID_SAN, false, san)],
        None => &[],
    };
    let leaf_der = build_cert(
        0x02,
        &leaf_name,
        &root_name,
        leaf_point.as_bytes(),
        NB,
        NA,
        leaf_exts,
        &sk_root, // the ISSUER signs the leaf
    );

    AuthChain {
        leaf_der,
        root_der,
        anchor_name: root_name,
        anchor_point: root_point.as_bytes().to_vec(),
        leaf_point: leaf_point.as_bytes().to_vec(),
        leaf_sk: sk_leaf,
    }
}

/// The trust anchor matching a generated chain.
fn anchor_of(chain: &AuthChain) -> TrustAnchor<'_> {
    TrustAnchor {
        subject: &chain.anchor_name,
        key: SpkiKey::EcP256 {
            point: &chain.anchor_point,
        },
    }
}

proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(8))]

    /// A chain that validates AND whose SAN authorizes the host is accepted,
    /// and the exported leaf key is exactly the leaf's public key. Covers
    /// exact dNSName (case-insensitive), wildcard SAN (one label), and
    /// iPAddress SAN vs an IPv4-literal host. Extra unneeded entries in the
    /// message stay ignored (RFC 8446).
    #[test]
    fn good_chain_authorizes_host(
        seed in any::<u64>(),
        variant in 0u8..3,
    ) {
        let (san, host): (Vec<u8>, &str) = match variant {
            0 => (san_dns(b"pagh.auth"), "pagh.auth"),
            1 => (san_wildcard(b"*.auth.test"), "a.auth.test"),
            _ => (san_ip(&[10, 0, 2, 15]), "10.0.2.15"),
        };
        let chain = build_auth_chain(seed, Some(san));
        let anchor = anchor_of(&chain);

        // Exact accept, and the case-insensitive form of the same host.
        let entries = [chain.leaf_der.as_slice()];
        let expected_point = chain.leaf_point.clone();
        let auth = authenticate_server(&entries, Some(host), &[anchor], NOW).unwrap();
        assert_eq!(auth.leaf_key, LeafKey::EcP256 { point: expected_point.clone() });

        let upper = host.to_ascii_uppercase();
        let auth2 = authenticate_server(&entries, Some(&upper), &[anchor], NOW).unwrap();
        assert_eq!(auth2.leaf_key, LeafKey::EcP256 { point: expected_point });

        // Extra unneeded entries (the root appended) stay ignored.
        let entries_extra = [chain.leaf_der.as_slice(), chain.root_der.as_slice()];
        prop_assert!(authenticate_server(&entries_extra, Some(host), &[anchor], NOW).is_ok());
    }

    /// Every fail-closed class this layer adds on top of chain validation:
    /// wrong host, deeper-than-wildcard host, IP-host vs DNS-only SAN, a
    /// leaf with NO SAN extension (CN fallback deliberately absent), a
    /// missing host, an expired leaf and the clock floor.
    #[test]
    fn hostname_and_chain_fail_closed(seed in any::<u64>()) {
        let good = build_auth_chain(seed, Some(san_dns(b"pagh.auth")));
        let anchor = anchor_of(&good);
        let entries = [good.leaf_der.as_slice()];

        // Wrong host — a valid chain does not authorize an unrelated name.
        prop_assert_eq!(
            authenticate_server(&entries, Some("other.invalid"), &[anchor], NOW),
            Err(AuthError::HostnameMismatch)
        );

        // Hostname missing entirely: nothing to authorize against.
        let static_entries: [&[u8]; 1] = [good.leaf_der.as_slice()];
        let anchors_static = [anchor];
        prop_assert_eq!(
            authenticate_server(&static_entries, None, &anchors_static, NOW),
            Err(AuthError::NoHostname)
        );

        // Wildcard matches exactly one label, never two.
        let wild = build_auth_chain(seed ^ 1, Some(san_wildcard(b"*.auth.test")));
        let deep = [wild.leaf_der.as_slice()];
        prop_assert_eq!(
            authenticate_server(&deep, Some("b.a.auth.test"), &[anchor_of(&wild)], NOW),
            Err(AuthError::HostnameMismatch)
        );

        // No cross-type matching: a DNS host against an IP-only SAN (and an
        // IP host against a DNS-only SAN) is a mismatch.
        let ip_leaf = build_auth_chain(seed ^ 2, Some(san_ip(&[10, 0, 2, 15])));
        let ip_entries = [ip_leaf.leaf_der.as_slice()];
        prop_assert_eq!(
            authenticate_server(&ip_entries, Some("pagh.auth"), &[anchor_of(&ip_leaf)], NOW),
            Err(AuthError::HostnameMismatch)
        );
        prop_assert_eq!(
            authenticate_server(&entries, Some("10.0.2.15"), &[anchor], NOW),
            Err(AuthError::HostnameMismatch)
        );

        // Missing SAN extension: the certificate does not authorize ANY host
        // — the deprecated CommonName fallback is deliberately not applied.
        let no_san = build_auth_chain(seed ^ 3, None);
        let no_san_entries = [no_san.leaf_der.as_slice()];
        prop_assert_eq!(
            authenticate_server(&no_san_entries, Some("leaf.auth.invalid"), &[anchor_of(&no_san)], NOW),
            Err(AuthError::HostnameMismatch)
        );

        // Chain failures surface untouched: expired leaf, unset clock.
        let expired_der = {
            let mut rng = rng_from(seed ^ 4);
            let sk_root = p256::ecdsa::SigningKey::random(&mut rng);
            let sk_leaf = p256::ecdsa::SigningKey::random(&mut rng);
            let root_name = name(b"Auth Root CA");
            let leaf_name = name(b"leaf.auth.invalid");
            let root_point = sk_root.verifying_key().to_encoded_point(false);
            let leaf_point = sk_leaf.verifying_key().to_encoded_point(false);
            let _ = root_point;
            build_cert(0x02, &leaf_name, &root_name, leaf_point.as_bytes(), NB, b"240101000000Z",
                &[extension(OID_SAN, false, &san_dns(b"pagh.auth"))], &sk_root)
        };
        prop_assert_eq!(
            authenticate_server(&[expired_der.as_slice()], Some("pagh.auth"), &[anchor], NOW),
            Err(AuthError::Chain(ChainError::Expired))
        );
        prop_assert_eq!(
            authenticate_server(&entries, Some("pagh.auth"), &[anchor], CLOCK_FLOOR - 1),
            Err(AuthError::Chain(ChainError::Clock))
        );
    }

    /// The exported leaf key verifies the TLS 1.3 `CertificateVerify`
    /// signature for ECDSA P-256 and Ed25519 leaves; any tampering, a wrong
    /// key, a scheme/key mismatch or a wrong transcript-hash length rejects.
    #[test]
    fn certificate_verify_roundtrip(seed in any::<u64>()) {
        let chain = build_auth_chain(seed, Some(san_dns(b"pagh.auth")));
        let anchor = anchor_of(&chain);
        let entries = [chain.leaf_der.as_slice()];
        let auth = authenticate_server(&entries, Some("pagh.auth"), &[anchor], NOW).unwrap();

        // P-256: sign the exact message with the leaf key → Ok.
        let hash = [0xabu8; 32];
        let msg = certificate_verify_message(&hash);
        let good_sig: p256::ecdsa::DerSignature = chain.leaf_sk.sign(&msg);
        prop_assert_eq!(
            verify_certificate_verify(Tls13Scheme::EcdsaSecp256r1Sha256, &hash, good_sig.as_ref(), &auth.leaf_key),
            Ok(())
        );

        // Tampered transcript hash OR tampered signature → reject (either as
        // a verification failure or a malformed blob; never a pass).
        let mut bad_hash = hash;
        bad_hash[0] ^= 0x01;
        prop_assert!(
            verify_certificate_verify(Tls13Scheme::EcdsaSecp256r1Sha256, &bad_hash, good_sig.as_ref(), &auth.leaf_key).is_err()
        );
        let mut bad_sig = good_sig.as_ref().to_vec();
        let last = bad_sig.len() - 1;
        bad_sig[last] ^= 0x01;
        prop_assert!(
            verify_certificate_verify(Tls13Scheme::EcdsaSecp256r1Sha256, &hash, &bad_sig, &auth.leaf_key).is_err()
        );

        // A different leaf key does not verify the same signature.
        let mut rng = rng_from(seed ^ 0x5eed);
        let stranger = p256::ecdsa::SigningKey::random(&mut rng);
        let stranger_point = stranger.verifying_key().to_encoded_point(false).as_bytes().to_vec();
        let stranger_key = LeafKey::EcP256 { point: stranger_point };
        prop_assert!(
            verify_certificate_verify(Tls13Scheme::EcdsaSecp256r1Sha256, &hash, good_sig.as_ref(), &stranger_key).is_err()
        );

        // Scheme/key mismatch and wrong transcript-hash length reject before
        // any cryptography.
        prop_assert_eq!(
            verify_certificate_verify(Tls13Scheme::RsaPssRsaeSha256, &hash, good_sig.as_ref(), &auth.leaf_key),
            Err(crate::tls_verify::SigVerifyError::KeyTypeMismatch)
        );
        prop_assert_eq!(
            verify_certificate_verify(Tls13Scheme::EcdsaSecp384r1Sha384, &hash, good_sig.as_ref(), &auth.leaf_key),
            Err(crate::tls_verify::SigVerifyError::MalformedSignature)
        );

        // Ed25519 leaf key round-trips the same message through its scheme.
        let ed = ed25519_dalek::SigningKey::generate(&mut rng);
        let ed_leaf = LeafKey::Ed25519 { key: ed.verifying_key().as_bytes().to_vec() };
        let ed_sig = ed.sign(&msg);
        prop_assert_eq!(
            verify_certificate_verify(Tls13Scheme::Ed25519, &hash, &ed_sig.to_vec(), &ed_leaf),
            Ok(())
        );
    }

    /// RSA leaves verify their `CertificateVerify` through RSASSA-PSS (the
    /// scheme TLS 1.3 mandates for RSA keys): a real 2048-bit round-trip
    /// passes, tampering rejects, and the 2048-bit modulus floor is enforced
    /// exactly as on the X.509 path.
    #[test]
    fn rsa_pss_certificate_verify(seed in any::<u64>()) {
        let mut rng = rng_from(seed);
        let sk = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let signing = rsa::pss::SigningKey::<sha2::Sha256>::new(sk.clone());
        let msg = certificate_verify_message(&[0x42u8; 32]);
        let sig = signing.sign_with_rng(&mut rng, &msg);
        let sig_bytes = sig.to_vec();
        let (n, e) = {
            use rsa::traits::PublicKeyParts;
            (sk.n().to_bytes_be(), sk.e().to_bytes_be())
        };
        let key = LeafKey::Rsa { n, e };

        prop_assert_eq!(
            verify_certificate_verify(Tls13Scheme::RsaPssRsaeSha256, &[0x42u8; 32], &sig_bytes, &key),
            Ok(())
        );

        // Tampered signature bytes → reject (never a pass).
        let mut bad = sig_bytes.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0x01;
        prop_assert!(
            verify_certificate_verify(Tls13Scheme::RsaPssRsaeSha256, &[0x42u8; 32], &bad, &key).is_err()
        );

        // Below the 2048-bit floor: MalformedKey before any math.
        let short = rsa::RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let (n_short, e_short) = {
            use rsa::traits::PublicKeyParts;
            (short.n().to_bytes_be(), short.e().to_bytes_be())
        };
        prop_assert_eq!(
            verify_certificate_verify(Tls13Scheme::RsaPssRsaeSha256, &[0x42u8; 32], &sig_bytes, &LeafKey::Rsa { n: n_short, e: e_short }),
            Err(crate::tls_verify::SigVerifyError::MalformedKey)
        );
    }
}
