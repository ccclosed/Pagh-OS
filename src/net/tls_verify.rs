//! Signature verification for the X.509 certificate verifier (issue #14
//! series) — pure `core` + `alloc`, panic-free, self-contained.
//!
//! `verify_certificate_signature` dispatches on the certificate's
//! `tbsCertificate.signature` AlgorithmIdentifier OID (validated to equal the
//! outer field by `x509::parse_certificate`) and checks the raw
//! `signatureValue` bytes (from [`CertificateRef::signature`]) over the exact
//! `tbs` bytes against the ISSUER's public key ([`SpkiKey`] from the issuer
//! certificate's SPKI). No kernel services, no RNG, no host calls.
//!
//! Supported surface (each mapping is fixed, no algorithm agility beyond it):
//!   * `sha256WithRSAEncryption` / `sha384WithRSAEncryption` /
//!     `sha512WithRSAEncryption` — RSASSA-PKCS1-v1_5 over an RSA SPKI;
//!   * `ecdsa-with-SHA256` — ECDSA (P-256) over an `ecPublicKey/prime256v1` SPKI;
//!   * `ecdsa-with-SHA384` — ECDSA (P-384) over an `ecPublicKey/secp384r1` SPKI;
//!   * `Ed25519` — pure Ed25519 over a raw 32-byte SPKI key.
//!
//! Fail-closed rules:
//!   * RSA-PSS certificates (`rsaEncryption`-family PSS OID) are explicitly
//!     REJECTED: the scheme lives in the AlgorithmIdentifier parameters, and
//!     mis-parsed PSS parameters are a classic downgrade vector. Real Debian
//!     mirror chains are PKCS1-v1_5 or ECDSA, so availability is unaffected;
//!     PSS support is a documented follow-up, not a lax fallback.
//!   * RSA moduli below 2048 bits are rejected (`MalformedKey`) regardless of
//!     signature validity (NIST SP 800-57 / CA/B minimum).
//!   * Any key/signature shape mismatch between the algorithm OID and the
//!     SPKI key type is a hard error (`KeyTypeMismatch`), never a lax try.

#![allow(dead_code)] // consumed by the chain verifier in the next PR of the series.

use super::x509::SpkiKey;
use sha2::{Digest, Sha256, Sha384, Sha512};

// ── AlgorithmIdentifier OIDs (content bytes, DER-encoded) ──────────────

/// `1.2.840.113549.1.1.11` sha256WithRSAEncryption.
pub const OID_SHA256_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];
/// `1.2.840.113549.1.1.12` sha384WithRSAEncryption.
pub const OID_SHA384_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c];
/// `1.2.840.113549.1.1.13` sha512WithRSAEncryption.
pub const OID_SHA512_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d];
/// `1.2.840.113549.1.1.10` RSASSA-PSS — explicitly REJECTED, see module docs.
pub const OID_RSA_PSS: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0a];
/// `1.2.840.10045.4.3.2` ecdsa-with-SHA256.
pub const OID_ECDSA_WITH_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
/// `1.2.840.10045.4.3.3` ecdsa-with-SHA384.
pub const OID_ECDSA_WITH_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03];
/// `1.3.101.112` Ed25519.
pub const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];

/// Minimum RSA modulus we accept, in bits (NIST SP 800-57 / CA/B Forum).
const RSA_MIN_MODULUS_BITS: usize = 2048;

/// Actual bit length of a big-endian modulus. The byte length overstates it
/// by up to 8 bits when the top byte is small — a 256-byte modulus starting
/// `0x01` is only 2041 bits, below the documented floor. The X.509 parser
/// sign-strips INTEGERs, so `n` should never START with `0x00`, but this
/// stays correct even for such caller-supplied input.
fn modulus_bits(n: &[u8]) -> usize {
    match n.split_first() {
        None => 0,
        Some((&first, rest)) => rest.len() * 8 + (8 - first.leading_zeros() as usize),
    }
}

/// Why a certificate signature check refused to pass. Every variant is a
/// hard reject — the chain is untrusted until a check returns `Ok(())`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigVerifyError {
    /// Signature algorithm OID outside the supported surface (including the
    /// explicitly rejected RSA-PSS).
    UnsupportedAlg,
    /// SPKI key type does not fit the signature algorithm (RSA sig + EC key…).
    KeyTypeMismatch,
    /// SPKI key material malformed for the algorithm (short RSA modulus,
    /// bad EC point encoding, wrong Ed25519 key length).
    MalformedKey,
    /// Signature blob malformed for the scheme (lengths, DER shape).
    MalformedSignature,
    /// Cryptographic verification failed — the bytes do not authenticate.
    VerifyFailed,
}

/// Verify the signature of one certificate against the ISSUER's public key.
///
/// * `sig_alg` — the `tbsCertificate.signature` AlgorithmIdentifier OID
///   (content bytes as decoded by the X.509 parser);
/// * `tbs` — the exact raw `TBSCertificate` element bytes the signature is
///   computed over ([`CertificateRef::tbs`]);
/// * `signature` — the raw `signatureValue` bytes ([`CertificateRef::signature`]);
/// * `key` — the issuer certificate's parsed SPKI key.
pub fn verify_certificate_signature(
    sig_alg: &[u8],
    tbs: &[u8],
    signature: &[u8],
    key: &SpkiKey<'_>,
) -> Result<(), SigVerifyError> {
    if oid_is(sig_alg, OID_RSA_PSS) {
        // Explicit reject (see module docs): parameters-driven scheme.
        return Err(SigVerifyError::UnsupportedAlg);
    }
    if oid_is(sig_alg, OID_SHA256_WITH_RSA) {
        return verify_rsa_pkcs1::<Sha256>(tbs, signature, key);
    }
    if oid_is(sig_alg, OID_SHA384_WITH_RSA) {
        return verify_rsa_pkcs1::<Sha384>(tbs, signature, key);
    }
    if oid_is(sig_alg, OID_SHA512_WITH_RSA) {
        return verify_rsa_pkcs1::<Sha512>(tbs, signature, key);
    }
    if oid_is(sig_alg, OID_ECDSA_WITH_SHA256) {
        return match key {
            SpkiKey::EcP256 { point } => verify_ecdsa_p256(point, tbs, signature),
            _ => Err(SigVerifyError::KeyTypeMismatch),
        };
    }
    if oid_is(sig_alg, OID_ECDSA_WITH_SHA384) {
        return match key {
            SpkiKey::EcP384 { point } => verify_ecdsa_p384(point, tbs, signature),
            _ => Err(SigVerifyError::KeyTypeMismatch),
        };
    }
    if oid_is(sig_alg, OID_ED25519) {
        return match key {
            SpkiKey::Ed25519 { key: pk } => verify_ed25519(pk, tbs, signature),
            _ => Err(SigVerifyError::KeyTypeMismatch),
        };
    }
    Err(SigVerifyError::UnsupportedAlg)
}

/// Compare an OID against a known content-byte constant (`x509::oid_is`
/// semantics: the decoded OID content of the AlgorithmIdentifier).
fn oid_is(oid: &[u8], known: &[u8]) -> bool {
    oid == known
}

/// RSASSA-PKCS1-v1_5 verification for one digest width.
fn verify_rsa_pkcs1<D>(
    tbs: &[u8],
    signature: &[u8],
    key: &SpkiKey<'_>,
) -> Result<(), SigVerifyError>
where
    D: Digest + const_oid::AssociatedOid,
{
    let SpkiKey::Rsa { n, e } = key else {
        return Err(SigVerifyError::KeyTypeMismatch);
    };
    // Empty/oversized-INTEGER encodings are already rejected by the parser;
    // a leading zero byte (positive-INTEGER padding) is legal and harmless
    // for from_bytes_be. The floor is the ACTUAL bit length, not the byte
    // length (see `modulus_bits`): a 256-byte modulus with a small top byte
    // is below 2048 bits and must be rejected.
    if modulus_bits(n) < RSA_MIN_MODULUS_BITS {
        return Err(SigVerifyError::MalformedKey);
    }
    let modulus = rsa::BigUint::from_bytes_be(n);
    let exponent = rsa::BigUint::from_bytes_be(e);
    let public =
        rsa::RsaPublicKey::new(modulus, exponent).map_err(|_| SigVerifyError::MalformedKey)?;
    let verifying = rsa::pkcs1v15::VerifyingKey::<D>::new(public);
    let sig = rsa::pkcs1v15::Signature::try_from(signature)
        .map_err(|_| SigVerifyError::MalformedSignature)?;
    use rsa::signature::Verifier;
    verifying
        .verify(tbs, &sig)
        .map_err(|_| SigVerifyError::VerifyFailed)
}

/// ECDSA (P-256) verification. The certificate carries a DER-encoded
/// `ECDSA-Sig-Value` (r,s); the SPKI point is the uncompressed 0x04 form the
/// parser validated.
fn verify_ecdsa_p256(point: &[u8], tbs: &[u8], signature: &[u8]) -> Result<(), SigVerifyError> {
    let verifying = p256::ecdsa::VerifyingKey::from_sec1_bytes(point)
        .map_err(|_| SigVerifyError::MalformedKey)?;
    let sig = p256::ecdsa::Signature::from_der(signature)
        .map_err(|_| SigVerifyError::MalformedSignature)?;
    use signature::Verifier;
    verifying
        .verify(tbs, &sig)
        .map_err(|_| SigVerifyError::VerifyFailed)
}

/// ECDSA (P-384) verification — same shape as P-256.
fn verify_ecdsa_p384(point: &[u8], tbs: &[u8], signature: &[u8]) -> Result<(), SigVerifyError> {
    let verifying = p384::ecdsa::VerifyingKey::from_sec1_bytes(point)
        .map_err(|_| SigVerifyError::MalformedKey)?;
    let sig = p384::ecdsa::Signature::from_der(signature)
        .map_err(|_| SigVerifyError::MalformedSignature)?;
    use signature::Verifier;
    verifying
        .verify(tbs, &sig)
        .map_err(|_| SigVerifyError::VerifyFailed)
}

/// Pure Ed25519 verification (RFC 8410): 32-byte key, 64-byte signature,
/// signed over the TBS bytes directly (no digest OID involved).
fn verify_ed25519(pk: &[u8], tbs: &[u8], signature: &[u8]) -> Result<(), SigVerifyError> {
    let pk: [u8; 32] = <[u8; 32]>::try_from(pk).map_err(|_| SigVerifyError::MalformedKey)?;
    let verifying =
        ed25519_dalek::VerifyingKey::from_bytes(&pk).map_err(|_| SigVerifyError::MalformedKey)?;
    let sig = ed25519_dalek::Signature::from_slice(signature)
        .map_err(|_| SigVerifyError::MalformedSignature)?;
    use signature::Verifier;
    verifying
        .verify(tbs, &sig)
        .map_err(|_| SigVerifyError::VerifyFailed)
}

// ───────────────────────────── self-tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid-looking RSA SPKI shape for dispatch tests (short modulus —
    /// must be rejected by the size gate before any math).
    #[test]
    fn short_rsa_modulus_rejected() {
        let key = SpkiKey::Rsa {
            n: &[0xc1; 16], // 127 bits — far below the 2048-bit gate
            e: &[0x01, 0x00, 0x01],
        };
        assert_eq!(
            verify_certificate_signature(OID_SHA256_WITH_RSA, &[0u8; 8], &[0u8; 32], &key),
            Err(SigVerifyError::MalformedKey)
        );
    }

    /// The modulus floor is the ACTUAL bit length, not the byte length: a
    /// 256-byte modulus with a small top byte (2041..2047 bits) is below the
    /// documented 2048-bit floor and must be rejected, while an exactly
    /// 2048-bit modulus passes the gate (and only then fails cryptographically).
    #[test]
    fn modulus_floor_is_bit_exact() {
        // 256 bytes, top byte 0x01 → 2041 bits: BELOW the floor.
        let below = SpkiKey::Rsa {
            n: &[0x01; 256],
            e: &[0x01, 0x00, 0x01],
        };
        assert_eq!(
            verify_certificate_signature(OID_SHA256_WITH_RSA, &[0u8; 8], &[0u8; 256], &below),
            Err(SigVerifyError::MalformedKey)
        );
        // 255 bytes, top byte 0xFF → 2040 bits: BELOW the floor (byte-length
        // alone would have flagged this one correctly; bit-length agrees).
        let short = SpkiKey::Rsa {
            n: &[0xff; 255],
            e: &[0x01, 0x00, 0x01],
        };
        assert_eq!(
            verify_certificate_signature(OID_SHA256_WITH_RSA, &[0u8; 8], &[0u8; 256], &short),
            Err(SigVerifyError::MalformedKey)
        );
        // Exactly 2048 bits, odd (a key rsa would even construct): the gate
        // passes and the failure moves to the cryptographic step (bogus
        // signature) — anything but MalformedKey here.
        let mut n_exact = [0x01u8; 256];
        n_exact[0] = 0x80; // top bit set → exactly 2048 bits; last byte 0x01 → odd
        let exact = SpkiKey::Rsa {
            n: &n_exact,
            e: &[0x01, 0x00, 0x01],
        };
        assert_ne!(
            verify_certificate_signature(OID_SHA256_WITH_RSA, &[0u8; 8], &[0u8; 256], &exact),
            Err(SigVerifyError::MalformedKey)
        );
    }

    /// OID → key-type mismatch is a hard error, never a lax try.
    #[test]
    fn key_type_mismatches() {
        let ec = SpkiKey::EcP256 { point: &[0u8; 65] };
        assert_eq!(
            verify_certificate_signature(OID_SHA256_WITH_RSA, &[0u8; 8], &[0u8; 32], &ec),
            Err(SigVerifyError::KeyTypeMismatch)
        );
        let rsa = SpkiKey::Rsa {
            n: &[0xc1; 256],
            e: &[0x01, 0x00, 0x01],
        };
        assert_eq!(
            verify_certificate_signature(OID_ECDSA_WITH_SHA256, &[0u8; 8], &[0u8; 8], &rsa),
            Err(SigVerifyError::KeyTypeMismatch)
        );
        assert_eq!(
            verify_certificate_signature(OID_ED25519, &[0u8; 8], &[0u8; 64], &rsa),
            Err(SigVerifyError::KeyTypeMismatch)
        );
    }

    /// RSA-PSS and unknown OIDs are rejected without touching the key.
    #[test]
    fn unsupported_algorithms() {
        let key = SpkiKey::Rsa {
            n: &[0xc1; 256],
            e: &[0x01, 0x00, 0x01],
        };
        assert_eq!(
            verify_certificate_signature(OID_RSA_PSS, &[0u8; 8], &[0u8; 32], &key),
            Err(SigVerifyError::UnsupportedAlg)
        );
        assert_eq!(
            verify_certificate_signature(&[0x2a, 0x85, 0x00], &[0u8; 8], &[0u8; 32], &key),
            Err(SigVerifyError::UnsupportedAlg)
        );
        assert_eq!(
            verify_certificate_signature(&[], &[0u8; 8], &[0u8; 32], &key),
            Err(SigVerifyError::UnsupportedAlg)
        );
    }

    /// Signature-blob shape errors for Ed25519: key must be 32 bytes and the
    /// signature exactly 64.
    #[test]
    fn ed25519_shape_errors() {
        let key = SpkiKey::Ed25519 { key: &[0u8; 31] };
        assert_eq!(
            verify_certificate_signature(OID_ED25519, &[0u8; 8], &[0u8; 64], &key),
            Err(SigVerifyError::MalformedKey)
        );
        let key = SpkiKey::Ed25519 { key: &[0u8; 32] };
        assert_eq!(
            verify_certificate_signature(OID_ED25519, &[0u8; 8], &[0u8; 63], &key),
            Err(SigVerifyError::MalformedSignature)
        );
    }
}
