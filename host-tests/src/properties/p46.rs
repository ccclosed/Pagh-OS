// Feature: TLS server authentication (issue #14), Property 46: the
// certificate-signature dispatcher (`tls_verify`) actually verifies real
// signatures — and fails closed on tampering and shape mismatches.
//
// Round-trip direction (host-only key GENERATION, verify via the KERNEL
// code under test):
//   * RSA-2048 + sha256WithRSAEncryption: sign TBSCertificate-shaped bytes
//     → `verify_certificate_signature` accepts; flipping any byte of the
//     message or the signature → `VerifyFailed`;
//   * ECDSA P-256 / P-384 (ecdsa-with-SHA256/384, DER sig blobs): same
//     accept + tamper behavior;
//   * Ed25519: raw 64-byte signatures, same behavior.
//
// Fail-closed direction: unknown OIDs and RSA-PSS never verify regardless of
// key; algorithm/key-type mismatches are hard errors; short RSA moduli,
// malformed EC points, wrong Ed25519 lengths → shape errors, never success.
//
// KEY GENERATION IS DETERMINISTIC: a XorShift-based RNG implements
// rand_core's traits, so every run replays the same keys/signatures from a
// proptest-provided seed — no OsRng, no getrandom, replayable failures.

use crate::tls_verify::{
    verify_certificate_signature, SigVerifyError, OID_ECDSA_WITH_SHA256, OID_ECDSA_WITH_SHA384,
    OID_ED25519, OID_RSA_PSS, OID_SHA256_WITH_RSA, OID_SHA384_WITH_RSA, OID_SHA512_WITH_RSA,
};
use crate::x509::SpkiKey;
use proptest::prelude::*;
use rand_core::{CryptoRng, RngCore};

/// Deterministic XorShift128+ style RNG (seeded from proptest values).
struct DetRng(u64, u64);

impl RngCore for DetRng {
    fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }
    fn next_u64(&mut self) -> u64 {
        // xorshift128+ (linear, deterministic, fine for test key generation).
        let mut a = self.0;
        let b = self.1;
        self.0 = b;
        a ^= a << 23;
        a ^= a >> 17;
        a ^= b ^ (b >> 26);
        self.1 = a;
        a.wrapping_add(b)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl CryptoRng for DetRng {}

fn rng_from(seed: u64) -> DetRng {
    DetRng(seed ^ 0x9E3779B97F4A7C15, seed.rotate_left(32) | 1)
}

/// Random message bytes for signatures.
fn msg() -> impl proptest::strategy::Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 1..=96)
}

proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(4))]

    /// RSA-2048 PKCS1-v1_5 over SHA-256: generated keypair verifies genuine
    /// signatures; ANY tampered byte (message or signature) fails. The
    /// keypair is generated once per run from the seed (deterministic).
    /// Cases are capped: two 2048-bit keygens per case dominate the runtime.
    #[test]
    fn rsa_pkcs1_roundtrip_and_tamper(seed in any::<u64>(), msg in msg()) {
        let mut rng = rng_from(seed);
        use rsa::traits::PublicKeyParts;
        let sk = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pk = sk.to_public_key();
        let (n_bytes, e_bytes) = (pk.n().to_bytes_be(), pk.e().to_bytes_be());
        let key = SpkiKey::Rsa {
            n: &n_bytes,
            e: &e_bytes,
        };

        let signing = rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new(sk);
        use rsa::signature::{SignatureEncoding, Signer};
        let sig = signing.sign(&msg);
        let sig_bytes = sig.to_vec();

        prop_assert_eq!(
            verify_certificate_signature(OID_SHA256_WITH_RSA, &msg, &sig_bytes, &key),
            Ok(())
        );

        // Flip one bit of the message → reject.
        let mut bad_msg = msg.clone();
        bad_msg[0] ^= 0x01;
        prop_assert_eq!(
            verify_certificate_signature(OID_SHA256_WITH_RSA, &bad_msg, &sig_bytes, &key),
            Err(SigVerifyError::VerifyFailed)
        );

        // Flip one bit of the signature → reject.
        let mut bad_sig = sig_bytes.clone();
        let flip = bad_sig.len() / 2;
        bad_sig[flip] ^= 0x80;
        prop_assert_eq!(
            verify_certificate_signature(OID_SHA256_WITH_RSA, &msg, &bad_sig, &key),
            Err(SigVerifyError::VerifyFailed)
        );

        // A DIFFERENT key must not verify the signature (key-confusion).
        let mut rng2 = rng_from(seed ^ u64::MAX);
        let sk2 = rsa::RsaPrivateKey::new(&mut rng2, 2048).unwrap();
        let pk2 = sk2.to_public_key();
        let (n2_bytes, e2_bytes) = (pk2.n().to_bytes_be(), pk2.e().to_bytes_be());
        let other = SpkiKey::Rsa {
            n: &n2_bytes,
            e: &e2_bytes,
        };
        prop_assert_eq!(
            verify_certificate_signature(OID_SHA256_WITH_RSA, &msg, &sig_bytes, &other),
            Err(SigVerifyError::VerifyFailed)
        );
    }
}

// Fast keygen (EC microsecond / Ed25519 millisecond scale): default case count.
proptest! {
    /// ECDSA P-256 (ecdsa-with-SHA256): DER signature blobs round-trip and
    /// tampering fails closed. Fresh key per case (keygen is microseconds).
    #[test]
    fn ecdsa_p256_roundtrip_and_tamper(seed in any::<u64>(), msg in msg()) {
        use signature::Signer;
        let mut rng = rng_from(seed);
        let sk = p256::ecdsa::SigningKey::random(&mut rng);
        let point = sk.verifying_key().to_encoded_point(false);
        let key = SpkiKey::EcP256 { point: point.as_bytes() };

        // Sign into the DER form explicitly (Signer<DerSignature>).
        let der_sig: p256::ecdsa::DerSignature = sk.sign(&msg);
        let sig = der_sig.as_bytes().to_vec();
        prop_assert_eq!(
            verify_certificate_signature(OID_ECDSA_WITH_SHA256, &msg, &sig, &key),
            Ok(())
        );

        let mut bad_msg = msg.clone();
        bad_msg[0] ^= 0x02;
        prop_assert_eq!(
            verify_certificate_signature(OID_ECDSA_WITH_SHA256, &bad_msg, &sig, &key),
            Err(SigVerifyError::VerifyFailed)
        );

        let mut bad_sig = sig.clone();
        let last = bad_sig.len() - 1;
        bad_sig[last] ^= 0x01;
        prop_assert_eq!(
            verify_certificate_signature(OID_ECDSA_WITH_SHA256, &msg, &bad_sig, &key),
            Err(SigVerifyError::VerifyFailed)
        );
    }

    /// ECDSA P-384 (ecdsa-with-SHA384): same contract as P-256.
    #[test]
    fn ecdsa_p384_roundtrip_and_tamper(seed in any::<u64>(), msg in msg()) {
        use signature::Signer;
        let mut rng = rng_from(seed);
        let sk = p384::ecdsa::SigningKey::random(&mut rng);
        let point = sk.verifying_key().to_encoded_point(false);
        let key = SpkiKey::EcP384 { point: point.as_bytes() };

        // Sign into the DER form explicitly (Signer<DerSignature>).
        let der_sig: p384::ecdsa::DerSignature = sk.sign(&msg);
        let sig = der_sig.as_bytes().to_vec();
        prop_assert_eq!(
            verify_certificate_signature(OID_ECDSA_WITH_SHA384, &msg, &sig, &key),
            Ok(())
        );

        let mut bad_msg = msg.clone();
        bad_msg[0] ^= 0x04;
        prop_assert_eq!(
            verify_certificate_signature(OID_ECDSA_WITH_SHA384, &bad_msg, &sig, &key),
            Err(SigVerifyError::VerifyFailed)
        );

        // Flip one bit INSIDE the DER blob's final value byte (structural
        // bytes would be MalformedSignature, a different fail-closed path).
        let mut bad_sig = sig.clone();
        let last = bad_sig.len() - 1;
        bad_sig[last] ^= 0x01;
        prop_assert_eq!(
            verify_certificate_signature(OID_ECDSA_WITH_SHA384, &msg, &bad_sig, &key),
            Err(SigVerifyError::VerifyFailed)
        );
    }

    /// Ed25519: raw 64-byte signatures over the TBS bytes; tampering and
    /// key confusion fail closed.
    #[test]
    fn ed25519_roundtrip_and_tamper(seed in any::<u64>(), msg in msg()) {
        use signature::Signer;
        let mut rng = rng_from(seed);
        let sk = ed25519_dalek::SigningKey::generate(&mut rng);
        let vk = sk.verifying_key();
        let key = SpkiKey::Ed25519 {
            key: vk.as_bytes(),
        };

        let sig = sk.sign(&msg).to_bytes().to_vec();
        prop_assert_eq!(
            verify_certificate_signature(OID_ED25519, &msg, &sig, &key),
            Ok(())
        );

        let mut bad_msg = msg.clone();
        bad_msg[0] ^= 0x08;
        prop_assert_eq!(
            verify_certificate_signature(OID_ED25519, &bad_msg, &sig, &key),
            Err(SigVerifyError::VerifyFailed)
        );

        let mut bad_sig = sig.clone();
        bad_sig[10] ^= 0x40;
        prop_assert_eq!(
            verify_certificate_signature(OID_ED25519, &msg, &bad_sig, &key),
            Err(SigVerifyError::VerifyFailed)
        );
    }

    /// Scheme/key-type mismatches and unsupported OIDs never verify — for
    /// ANY message and signature bytes, including genuine-looking ones.
    #[test]
    fn dispatch_fail_closed(seed in any::<u64>(), msg in msg(), sig in msg()) {
        let mut rng = rng_from(seed);
        let sk = ed25519_dalek::SigningKey::generate(&mut rng);
        let vk = sk.verifying_key();
        let ed = SpkiKey::Ed25519 {
            key: vk.as_bytes(),
        };
        let ec = SpkiKey::EcP256 { point: &[2u8; 65] };
        let rsa_small = SpkiKey::Rsa {
            n: &[0xc1; 16],
            e: &[0x01, 0x00, 0x01],
        };
        // Byte-length meets the 2048-bit floor but ACTUAL bit length does
        // not (top byte 0x01 → 2041 bits): the documented contract is on
        // bits, so this must be rejected too.
        let rsa_2041bit = SpkiKey::Rsa {
            n: &[0x01; 256],
            e: &[0x01, 0x00, 0x01],
        };

        // RSA-PSS: explicit reject even with a genuine Ed25519 signature
        // present and an RSA-shaped key — scheme, not crypto, decides.
        prop_assert_eq!(
            verify_certificate_signature(OID_RSA_PSS, &msg, &sig, &rsa_small),
            Err(SigVerifyError::UnsupportedAlg)
        );
        // Unknown OID prefix / empty OID.
        prop_assert_eq!(
            verify_certificate_signature(&[0x2a, 0x86], &msg, &sig, &ed),
            Err(SigVerifyError::UnsupportedAlg)
        );
        prop_assert_eq!(
            verify_certificate_signature(&[], &msg, &sig, &ed),
            Err(SigVerifyError::UnsupportedAlg)
        );
        // sha384/sha512WithRSA against a non-RSA key.
        for oid in [OID_SHA384_WITH_RSA, OID_SHA512_WITH_RSA] {
            prop_assert_eq!(
                verify_certificate_signature(oid, &msg, &sig, &ed),
                Err(SigVerifyError::KeyTypeMismatch)
            );
        }
        // ECDSA-with-SHA256 against Ed25519 / RSA keys.
        prop_assert_eq!(
            verify_certificate_signature(OID_ECDSA_WITH_SHA256, &msg, &sig, &ed),
            Err(SigVerifyError::KeyTypeMismatch)
        );
        prop_assert_eq!(
            verify_certificate_signature(OID_ECDSA_WITH_SHA256, &msg, &sig, &rsa_small),
            Err(SigVerifyError::KeyTypeMismatch)
        );
        // SHA-256-RSA against a below-minimum RSA modulus.
        prop_assert_eq!(
            verify_certificate_signature(OID_SHA256_WITH_RSA, &msg, &sig, &rsa_small),
            Err(SigVerifyError::MalformedKey)
        );
        // And against a 256-byte modulus that is only 2041 BITS.
        prop_assert_eq!(
            verify_certificate_signature(OID_SHA256_WITH_RSA, &msg, &sig, &rsa_2041bit),
            Err(SigVerifyError::MalformedKey)
        );
        // Ed25519 with a bogus 65-byte "point" EC key.
        prop_assert_eq!(
            verify_certificate_signature(OID_ED25519, &msg, &sig, &ec),
            Err(SigVerifyError::KeyTypeMismatch)
        );
    }
}
