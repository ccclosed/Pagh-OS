//! Trust anchors + certificate chain building for the TLS server verifier
//! (issue #14 series) — pure `core` + `alloc`, panic-free, self-contained.
//!
//! This module turns the primitives of the previous PRs into a complete
//! path-validation decision for one server chain:
//!
//!   * [`TrustAnchor`] — a self-signed root the kernel is configured to trust,
//!     as a raw DER `Name` (byte-compared, never parsed) plus its public key;
//!   * [`verify_chain`] — builds leaf → intermediates → anchor by **byte
//!     equality** of `issuer`/`subject` DER names, checks every signature with
//!     [`super::tls_verify::verify_certificate_signature`], enforces
//!     `basicConstraints cA=TRUE` on every issuer (intermediates and roots),
//!     checks each certificate's validity window against the caller-provided
//!     `now`, and applies the **clock gate** below.
//!
//! Fail-closed rules (issue #14 is exactly about removing fail-open paths):
//!
//!   * **Clock gate**: a `now` below [`CLOCK_FLOOR`] (2025-01-01T00:00:00Z) is
//!     a hard [`ChainError::Clock`] reject. At boot the CMOS RTC may be unset
//!     (counting from 1970), and "I do not know what time it is" must refuse
//!     the handshake, never pass it: every real-world certificate on a current
//!     mirror is already inside 2025+, so treating pre-2025 clocks as unset is
//!     safe and eliminates the whole class of "RTC not set ⇒ everything
//!     validates" fail-open windows.
//!   * A chain that never reaches an anchor is a reject ([`ChainError::NoAnchor`]
//!     when nothing matches the issuer name at all); a name match whose
//!     signature does not verify is [`ChainError::Verify`] — never a pass.
//!   * Intermediate/root certificates without `basicConstraints cA=TRUE` are
//!     rejected ([`ChainError::NotCa`]) — RFC 5280 §4.2.1.9: DER omits the
//!     defaulted `FALSE`, so a *missing* extension is also a reject here.
//!   * Extra certificates in the DER after a complete path is found are
//!     ignored (RFC 8446 §4.4.2 lets servers send entries the client does not
//!     need); they are never trusted for anything, so ignoring them removes no
//!     check.
//!
//! No kernel services, no RNG, no host calls: `now` is a parameter, so the
//! kernel call site (later PR of the series) passes `rtc::now_unix() as i64`
//! and the host property test P47 drives the exact same code with synthetic
//! values.

#![allow(dead_code)] // consumed by the TlsVerifier in the next PR of the series.

use alloc::vec;
use alloc::vec::Vec;

use super::tls_verify::verify_certificate_signature;
use super::x509::{
    find_extension, parse_basic_constraints, parse_certificate, CertificateRef, DerError, SpkiKey,
    OID_EXT_BASIC_CONSTRAINTS,
};

/// Clock gate floor: 2025-01-01T00:00:00Z as Unix seconds.
///
/// A system clock below this value is treated as "not actually set" (fresh
/// UEFI/CMOS boots commonly start at the build epoch or 1970) and fails the
/// handshake closed — see the module docs.
pub const CLOCK_FLOOR: i64 = 1_735_689_600;

/// Why a chain was rejected. Every variant is a hard reject; `PartialEq` so
/// the host property tests can assert exact failure classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainError {
    /// No trust anchor matches the final issuer name in the chain.
    NoAnchor,
    /// The chain is unusable as given: a certificate failed to parse, or the
    /// path between the leaf and the anchors is broken (missing intermediate).
    IncompleteChain,
    /// A certificate in the path is past its `notAfter`.
    Expired,
    /// A certificate in the path is before its `notBefore`.
    NotYetValid,
    /// The system clock is below the floor — the time of day is unknown, so
    /// validity cannot be decided and the handshake is refused.
    Clock,
    /// An issuer certificate (intermediate or root) is not `cA=TRUE`.
    NotCa,
    /// A signature check failed, with the underlying reason from
    /// [`super::tls_verify::SigVerifyError`].
    Verify(super::tls_verify::SigVerifyError),
}

impl From<SigVerifyError> for ChainError {
    fn from(e: SigVerifyError) -> Self {
        ChainError::Verify(e)
    }
}

// Keep the alias unambiguous at the use sites below.
use super::tls_verify::SigVerifyError;

/// A self-signed root the kernel trusts: the raw DER `Name` (compared by byte
/// equality against certificate `issuer` fields) and the public key the root
/// signs with. No certificate parsing is done on trust — the anchor is a
/// configured fact, not a parsed claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustAnchor<'a> {
    /// Raw DER of the anchor's subject `Name` (same encoding certificates
    /// carry, so comparison against `CertificateRef::issuer` is a plain
    /// byte-slice compare).
    pub subject: &'a [u8],
    /// The anchor's public key (must be one the verifier supports).
    pub key: SpkiKey<'a>,
}

/// Validate one server certificate chain against the given anchors.
///
/// * `anchors` — the configured trust anchors (root CAs);
/// * `der` — the DER of the leaf certificate followed by any intermediates the
///   server sent (exactly the layout of a TLS `Certificate` message);
/// * `now` — the current Unix time in seconds (`rtc::now_unix() as i64` at
///   the kernel call site).
///
/// Returns `Ok(())` only if a complete path leaf → … → anchor exists where
/// every signature verifies against the parent key, every issuer in the path
/// (intermediates and the root, when carried as a certificate) is `cA=TRUE`,
/// every certificate in the path is within its validity window, and `now` is
/// at or above [`CLOCK_FLOOR`].
pub fn verify_chain(anchors: &[TrustAnchor<'_>], der: &[u8], now: i64) -> Result<(), ChainError> {
    // CLOCK GATE, before anything else: an unset RTC must never look like a
    // valid time for a certificate whose window happens to include it.
    if now < CLOCK_FLOOR {
        return Err(ChainError::Clock);
    }

    // Parse every certificate the peer sent. A malformed entry anywhere is a
    // reject (IncompleteChain) — the peer does not get to choose which parts
    // of its garbage we ignore.
    let mut rest = der;
    let mut certs: Vec<CertificateRef<'_>> = Vec::new();
    while !rest.is_empty() {
        let (cert, tail) = parse_certificate(rest).map_err(der_to_chain)?;
        certs.push(cert);
        rest = tail;
    }
    let leaf = match certs.first() {
        Some(c) => *c,
        None => return Err(ChainError::IncompleteChain),
    };

    // Validity of every certificate in the message is checked up front (even
    // for entries that may not end up in the path — they were sent as part of
    // the chain, and a peer sending expired chain material is not a peer this
    // verifier trusts to pass).
    for cert in &certs {
        check_validity(cert, now)?;
    }

    // Walk the path. `path_index` tracks which message entries were consumed
    // as issuers so a hostile peer cannot reuse the same entry twice (a
    // self-issued loop would otherwise let a chain "verify" against itself).
    let mut used = vec![false; certs.len()];
    used[0] = true;
    let mut current = leaf;
    loop {
        // Does an intermediate certificate in the message name itself as the
        // issuer of `current`? Try each unused candidate; a name match whose
        // signature fails is remembered and only surfaces if NO candidate
        // verifies.
        let mut name_match_err: Option<ChainError> = None;
        let mut advanced = false;
        for (i, cand) in certs.iter().enumerate() {
            if used[i] || cand.subject != current.issuer {
                continue;
            }
            // An intermediate MUST be a CA (fail closed on a missing
            // basicConstraints too — DER omits the defaulted FALSE).
            if !is_ca(cand)? {
                name_match_err = Some(ChainError::NotCa);
                continue;
            }
            match verify_certificate_signature(
                current.sig_alg,
                current.tbs,
                current.signature,
                &cand.spki.key,
            ) {
                Ok(()) => {
                    used[i] = true;
                    current = *cand;
                    advanced = true;
                    break;
                }
                Err(e) => {
                    name_match_err = Some(ChainError::Verify(e));
                }
            }
        }
        if advanced {
            continue;
        }
        if let Some(e) = name_match_err {
            return Err(e);
        }

        // No unused intermediate matches: the parent must be a trust anchor.
        let mut anchor_err: Option<ChainError> = None;
        for anchor in anchors {
            if anchor.subject != current.issuer {
                continue;
            }
            match verify_certificate_signature(
                current.sig_alg,
                current.tbs,
                current.signature,
                &anchor.key,
            ) {
                Ok(()) => return Ok(()),
                Err(e) => anchor_err = Some(ChainError::Verify(e)),
            }
        }
        if let Some(e) = anchor_err {
            return Err(e);
        }
        // Nothing names the issuer of `current`: the path is broken (missing
        // intermediate or an untrusted root).
        return Err(ChainError::NoAnchor);
    }
}

/// Check `not_before <= now <= not_after` for one certificate.
fn check_validity(cert: &CertificateRef<'_>, now: i64) -> Result<(), ChainError> {
    if now > cert.validity.not_after {
        return Err(ChainError::Expired);
    }
    if now < cert.validity.not_before {
        return Err(ChainError::NotYetValid);
    }
    Ok(())
}

/// Does this certificate carry `basicConstraints cA=TRUE`? A missing extension
/// or a parse failure is NOT a CA (fail closed).
fn is_ca(cert: &CertificateRef<'_>) -> Result<bool, ChainError> {
    match find_extension(cert, OID_EXT_BASIC_CONSTRAINTS) {
        Ok(Some(octets)) => parse_basic_constraints(octets).map_err(der_to_chain),
        Ok(None) => Ok(false),
        Err(e) => Err(der_to_chain(e)),
    }
}

/// Map a DER failure of chain material onto the chain error (malformed input
/// in the middle of a chain is an incomplete chain, not a parse report).
fn der_to_chain(e: DerError) -> ChainError {
    let _ = e;
    ChainError::IncompleteChain
}

// ───────────────────────────── self-tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A `TrustAnchor` shaped from raw bytes for dispatcher-level checks.
    #[test]
    fn clock_gate_rejects_below_floor() {
        let anchor = TrustAnchor {
            subject: &[0x30, 0x00],
            key: SpkiKey::Unsupported,
        };
        // One second before the floor: "the clock is not really set".
        assert_eq!(
            verify_chain(&[anchor], &[], CLOCK_FLOOR - 1),
            Err(ChainError::Clock)
        );
        // At the floor and above the gate passes (empty DER fails later, but
        // with IncompleteChain, never Clock).
        assert_eq!(
            verify_chain(&[anchor], &[], CLOCK_FLOOR),
            Err(ChainError::IncompleteChain)
        );
    }
}
