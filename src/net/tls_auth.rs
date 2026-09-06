//! One-shot TLS 1.3 server authentication (issue #14 series) — the decision
//! layer that ties the previous PRs into a single "may I talk to this peer?"
//! answer. Pure `core` + `alloc`, panic-free, self-contained: no kernel
//! services, no RNG, no embedded-tls types. The kernel call site
//! (`net::tls`) adapts the handshake's borrowed certificate entries and the
//! negotiated `SignatureScheme` onto the small own types below, and the host
//! property P49 drives the exact same source with synthetic chains.
//!
//! Three decisions, all fail-closed, all mandatory, in this order:
//!
//!   1. **Chain** — [`tls_chain::verify_chain`] links the server's
//!      certificates to the configured trust anchors (byte-equal names, every
//!      signature verified, `cA=TRUE` on every issuer, validity windows plus
//!      the 2025 clock gate against the caller-provided `now`).
//!   2. **Hostname** — RFC 6125 authorization of the connection target
//!      against the LEAF's SAN entries only: dNSName entries through
//!      [`hostname::hostname_matches`] for a DNS target, iPAddress entries
//!      through [`hostname::ip_matches`] for an IP-literal target. There is
//!      deliberately NO CommonName fallback (CN matching was deprecated by
//!      CA/B Forum in 2017; every current mirror certificate carries SAN) and
//!      NO cross-type matching (an IP host never matches a dNSName entry and
//!      vice versa). A missing or malformed SAN extension is a reject, not a
//!      pass; a missing host (`None`) is a reject too — a verifier without a
//!      name to check has nothing to authorize.
//!   3. **Leaf key export** — the TLS 1.3 `CertificateVerify` message (a
//!      signature over the handshake transcript, sent AFTER the certificate)
//!      must be checked with the LEAF's public key. [`authenticate_server`]
//!      copies the leaf SPKI into an owned [`LeafKey`]; a leaf whose
//!      algorithm is outside the supported surface fails the handshake here
//!      rather than at the signature step (same fail-closed surface as
//!      [`super::x509::SpkiKey::Unsupported`]).
//!
//! The `CertificateVerify` check itself lives here too:
//! [`certificate_verify_message`] builds the exact signed message
//! (RFC 8446 §4.4.3: 64 spaces + context string + transcript hash) and
//! [`verify_certificate_verify`] dispatches on the negotiated
//! [`Tls13Scheme`]. Unlike the X.509 chain signatures — where RSA-PSS is
//! rejected outright (parameters-driven OID, see `tls_verify` module docs) —
//! TLS 1.3 REQUIRES RSA `CertificateVerify` signatures to be PSS
//! (`rsa_pss_rsae_*`, RFC 8446 §4.2.3): the scheme is fully determined by the
//! negotiated identifier (hash = suffix, salt length = hash length, trailer
//! fixed), so there is no parameter surface to downgrade. A chain signed
//! PKCS1-v1_5 with an RSA leaf therefore verifies its chain in PKCS1-v1_5 and
//! its `CertificateVerify` in PSS — that combination is what real Debian
//! mirrors serve.

#![allow(dead_code)] // consumed by net::tls in this very PR; kept explicit.

use alloc::vec;
use alloc::vec::Vec;

use super::hostname::{hostname_matches, ip_matches, is_ip_literal, parse_ipv4_literal};
use super::tls_chain::{verify_chain, ChainError, TrustAnchor};
use super::tls_verify::{verify_certificate_verify_signature, SigVerifyError, TLS13_CTX_SERVER_CV};
use super::x509::{find_extension, parse_certificate, san_names, SpkiKey, OID_EXT_SAN};

// The scheme enum is defined in `tls_verify` next to the backends it
// dispatches to; re-exported here so callers see one TLS-auth surface.
pub use super::tls_verify::Tls13Scheme;

/// The TLS 1.3 `CertificateVerify` signature schemes the verifier accepts
/// (RFC 8446 §4.2.3). Own enum — the pure layer never imports embedded-tls
/// types; the kernel call site maps the wire values 1:1.
///
/// Deliberately absent: `rsa_pkcs1_*` (banned for TLS 1.3 CertificateVerify —
/// their presence in a handshake is a downgrade marker), `Ed448`,
/// `EcdsaSecp521r1Sha512`, PSS-with-PSS-key (`rsa_pss_pss_*` — the SPKI is
/// `rsassaPss`, an algorithm family the X.509 layer rejects anyway).
//
// Tls13Scheme itself is defined in tls_verify next to the backends it
// dispatches to.

/// The leaf certificate's public key, copied into owned memory.
///
/// `verify_certificate` receives the certificate entries as borrows of the
/// handshake's record buffers; the `CertificateVerify` step happens later in
/// the same handshake, and the verifier struct outlives neither — but the
/// borrow checker cannot see that. Copying the (small) key material into
/// owned bytes at decision time keeps the verifier's state self-owned and
/// lets the pure layer run on the host without borrowed-state juggling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafKey {
    /// rsaEncryption: modulus and exponent (big-endian, sign-stripped).
    Rsa { n: Vec<u8>, e: Vec<u8> },
    /// EC P-256 uncompressed point (65 B).
    EcP256 { point: Vec<u8> },
    /// EC P-384 uncompressed point (97 B).
    EcP384 { point: Vec<u8> },
    /// Ed25519 raw 32-byte public key.
    Ed25519 { key: Vec<u8> },
}

impl LeafKey {
    /// Copy a parsed SPKI key into owned bytes; `None` for keys outside the
    /// supported surface (the handshake must fail, not defer to a later step).
    pub fn from_spki(key: &SpkiKey<'_>) -> Option<LeafKey> {
        match *key {
            SpkiKey::Rsa { n, e } => Some(LeafKey::Rsa {
                n: n.to_vec(),
                e: e.to_vec(),
            }),
            SpkiKey::EcP256 { point } => Some(LeafKey::EcP256 {
                point: point.to_vec(),
            }),
            SpkiKey::EcP384 { point } => Some(LeafKey::EcP384 {
                point: point.to_vec(),
            }),
            SpkiKey::Ed25519 { key } => Some(LeafKey::Ed25519 { key: key.to_vec() }),
            SpkiKey::Unsupported => None,
        }
    }

    /// Borrow back as a [`SpkiKey`] (for the signature backends, which speak
    /// the borrowed form).
    pub fn as_spki(&self) -> SpkiKey<'_> {
        match self {
            LeafKey::Rsa { n, e } => SpkiKey::Rsa { n, e },
            LeafKey::EcP256 { point } => SpkiKey::EcP256 { point },
            LeafKey::EcP384 { point } => SpkiKey::EcP384 { point },
            LeafKey::Ed25519 { key } => SpkiKey::Ed25519 { key },
        }
    }
}

/// What `verify_certificate` hands to the `CertificateVerify` step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAuth {
    /// The verified leaf's public key — the only key allowed to sign the
    /// handshake's `CertificateVerify` message.
    pub leaf_key: LeafKey,
}

/// Why the server failed authentication. Every variant is a hard reject.
/// `PartialEq`/`Eq` so the host property tests can assert exact classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// The certificate chain did not validate (wraps the chain builder's
    /// exact error class — [`ChainError::Clock`] here means the RTC was
    /// unset, exactly as it does at the chain layer).
    Chain(ChainError),
    /// No connection-target hostname was configured — nothing to authorize
    /// the certificate against, so the handshake is refused.
    NoHostname,
    /// The certificate does not authorize the connection target: no SAN
    /// extension, a malformed one, or no dNSName/iPAddress entry matching
    /// the host. Includes the missing-CN-fallback policy (see module docs).
    HostnameMismatch,
    /// The leaf's key algorithm is outside the supported verification
    /// surface, so the `CertificateVerify` step could never succeed.
    UnsupportedLeafKey,
}

/// Authenticate a TLS 1.3 server: chain validation + hostname authorization
/// + leaf-key export, in one fail-closed decision.
///
/// * `entries` — the raw DER of each certificate in the server's
///   `Certificate` message, leaf first (exactly the entry order the peer
///   sent; non-X.509 entries make the caller refuse before this layer).
/// * `host` — the connection target as configured (`Some` for DNS names and
///   IPv4 literals alike; [`None`] is a hard [`AuthError::NoHostname`]).
/// * `anchors` — the configured trust roots (raw subject `Name` DER + key).
/// * `now` — current Unix seconds (`rtc::now_unix() as i64` at the kernel
///   call site; the clock gate inside [`verify_chain`] applies regardless).
///
/// On success the leaf's key is returned for the `CertificateVerify` step;
/// the caller MUST still run that step (a valid chain with a bogus
/// `CertificateVerify` is an incomplete handshake, not a pass).
pub fn authenticate_server(
    entries: &[&[u8]],
    host: Option<&str>,
    anchors: &[TrustAnchor<'_>],
    now: i64,
) -> Result<ServerAuth, AuthError> {
    // Empty entry list: nothing to authenticate (the chain layer would also
    // reject, but be explicit — an empty message is its own failure class).
    let first = match entries.first() {
        Some(e) => e,
        None => return Err(AuthError::Chain(ChainError::IncompleteChain)),
    };

    // 1. CHAIN. Concatenate the entries into the layout verify_chain expects
    // (leaf followed by intermediates — the TLS Certificate message layout)
    // and let the chain builder make every decision about signatures, CA
    // flags, validity and the clock.
    let mut der = Vec::with_capacity(entries.iter().map(|e| e.len()).sum());
    for entry in entries {
        der.extend_from_slice(entry);
    }
    verify_chain(anchors, &der, now).map_err(AuthError::Chain)?;

    // 2. HOSTNAME. The leaf (first entry) must authorize the connection
    // target through its SAN entries — never through the CommonName.
    let host = match host {
        Some(h) => h,
        None => return Err(AuthError::NoHostname),
    };
    authorize_hostname(first, host.as_bytes())?;

    // 3. LEAF KEY. Copy the verified leaf's public key for the
    // CertificateVerify step; an unsupported algorithm fails here.
    let (leaf, _) =
        parse_certificate(first).map_err(|_| AuthError::Chain(ChainError::IncompleteChain))?;
    let leaf_key = LeafKey::from_spki(&leaf.spki.key).ok_or(AuthError::UnsupportedLeafKey)?;

    Ok(ServerAuth { leaf_key })
}

/// RFC 6125 authorization of `host` against the leaf's SAN entries.
///
/// An IP-literal host is matched ONLY against `iPAddress` entries and a DNS
/// host ONLY against `dNSName` entries (no cross-type matching, no CN
/// fallback); a missing or unparseable SAN extension is a mismatch. The
/// return is boolean rather than an error because the caller has exactly one
/// policy for "not authorized": reject the handshake.
fn authorize_hostname(leaf_der: &[u8], host: &[u8]) -> Result<(), AuthError> {
    let (leaf, _) =
        parse_certificate(leaf_der).map_err(|_| AuthError::Chain(ChainError::IncompleteChain))?;
    // Collect the SAN GeneralNames; a missing extension or a malformed one
    // both leave `entries` empty below → HostnameMismatch (fail closed).
    let mut entries: Vec<super::x509::San<'_>> = Vec::new();
    if let Ok(Some(octets)) = find_extension(&leaf, OID_EXT_SAN) {
        if let Ok(iter) = san_names(octets) {
            // A malformed individual GeneralName stops the walk: the peer
            // sent broken authorization material and does not get to have
            // the well-formed prefix of it considered.
            for item in iter {
                match item {
                    Ok(san) => entries.push(san),
                    Err(_) => {
                        entries.clear();
                        break;
                    }
                }
            }
        }
    }

    let matched = if is_ip_literal(host) {
        // An IPv4 target compares as raw octets against iPAddress entries.
        // (An IPv6 target cannot be resolved by this stack's DNS path yet;
        // `parse_ipv4_literal` returns None and the match fails closed.)
        match parse_ipv4_literal(host) {
            Some(octets) => entries.iter().any(|san| match san {
                super::x509::San::Ip(raw) => ip_matches(&octets, raw),
                _ => false,
            }),
            None => false,
        }
    } else {
        entries.iter().any(|san| match san {
            super::x509::San::Dns(name) => hostname_matches(host, name),
            _ => false,
        })
    };

    if matched {
        Ok(())
    } else {
        Err(AuthError::HostnameMismatch)
    }
}

/// Build the exact message a TLS 1.3 server signs in its `CertificateVerify`
/// (RFC 8446 §4.4.3): 64 spaces, the context string
/// ([`TLS13_CTX_SERVER_CV`], NUL-terminated) and the transcript hash.
pub fn certificate_verify_message(transcript_hash: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(64 + TLS13_CTX_SERVER_CV.len() + transcript_hash.len());
    msg.extend_from_slice(&[0x20u8; 64]);
    msg.extend_from_slice(TLS13_CTX_SERVER_CV);
    msg.extend_from_slice(transcript_hash);
    msg
}

/// Verify the handshake's `CertificateVerify` against the authenticated
/// leaf key: the transcript-hash length must match the negotiated scheme,
/// and the signature must verify over the exact RFC 8446 §4.4.3 message.
pub fn verify_certificate_verify(
    scheme: Tls13Scheme,
    transcript_hash: &[u8],
    signature: &[u8],
    key: &LeafKey,
) -> Result<(), SigVerifyError> {
    if transcript_hash.len() != scheme.hash_len() {
        return Err(SigVerifyError::MalformedSignature);
    }
    let msg = certificate_verify_message(transcript_hash);
    verify_certificate_verify_signature(scheme, &msg, signature, &key.as_spki())
}

// ───────────────────────────── self-tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 8446 §4.4.3 message layout: 64 spaces + context string +
    /// transcript hash, built byte-exactly.
    #[test]
    fn certificate_verify_message_layout() {
        let hash = [0xabu8; 32];
        let msg = certificate_verify_message(&hash);
        assert_eq!(msg.len(), 64 + 34 + 32);
        assert!(msg[..64].iter().all(|&b| b == 0x20));
        assert_eq!(&msg[64..98], b"TLS 1.3, server CertificateVerify\0");
        assert_eq!(&msg[98..], &hash[..]);
    }

    /// A transcript hash of the wrong length for the scheme is rejected
    /// before any cryptography runs.
    #[test]
    fn hash_length_gated_by_scheme() {
        let key = LeafKey::Ed25519 { key: vec![0u8; 32] };
        assert_eq!(
            verify_certificate_verify(
                Tls13Scheme::Ed25519,
                &[0u8; 48], // P-384-width hash for a SHA-256 scheme
                &[0u8; 64],
                &key,
            ),
            Err(SigVerifyError::MalformedSignature)
        );
    }
}
