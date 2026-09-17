//! Pure OpenPGP *verification policy* for Debian repository metadata (issue
//! #32): trusted-key selection, subkey binding, expiry/revocation/key-flag
//! handling, and the two entry points `apt` will call — a detached signature
//! over `Release` (`Release.gpg`) and a clear-signed `InRelease`.
//!
//! `core` + `alloc` only, no globals: the trusted keyring is always an explicit
//! argument ([`PinnedKey`]), so this module can never consult a mutable trust
//! store, and the host tests exercise the same code with synthetic keyrings.
//! The contract this implements is `OPENPGP-VERIFY-CONTRACT.md` (§2, §3, §7);
//! the empirical facts behind every rule (the v4 digest trailer, clearsign
//! canonicalization, "EdDSA signs the digest", the legacy Ed25519 OID, subkey
//! issuers) are recorded there and re-verified against live Debian artifacts in
//! P52/P53.
//!
//! ## Acceptance over multiple signatures
//!
//! Real `stable` metadata carries three signatures (today: two RSA-4096
//! subkeys and one Ed25519 primary) and a mirror may add signers Debian rotation
//! introduces. The rule is therefore:
//!
//! * a signature whose issuer fingerprint is **not** in the pinned keyring is
//!   ignored (it cannot be checked, and refusing it would break the mirror the
//!   day Debian adds a signer);
//! * a signature whose issuer fingerprint **is** pinned must verify fully — a
//!   single bad signature from a pinned key is fatal even when others are good
//!   (this is `gpgv`'s own behaviour: one bad signature makes it exit 1);
//! * at least one fully verified signature is required.
//!
//! ## Deliberate strengthening over `gpgv`
//!
//! `gpgv` verifies, and exits 0 for, a signature made by an **expired** key.
//! pagh refuses: a key whose validity window has closed is not usable, and the
//! operator must regenerate the pinned keyring (`tools/gen_debian_keyring.py`)
//! deliberately. `SECURITY.md` names this as a strengthening, not as
//! compatibility.

#![allow(dead_code)]

use alloc::vec::Vec;

use super::openpgp_crypto::{self as crypto, CryptoError, HashAlgo};
use super::openpgp_packet::{self as packet, PacketError, SignaturePacket};

/// Clock floor shared with the TLS verifier: below 2025-01-01 the RTC is treated
/// as unset and *no* date comparison can be trusted. Must stay equal to
/// `net::tls_chain::CLOCK_FLOOR`; P53 asserts the two never drift apart.
pub const CLOCK_FLOOR: i64 = 1_735_689_600;

/// Tolerance for clock skew when comparing "now" against a signature or key
/// creation time (seconds). A signature dated further in the future than this is
/// refused.
pub const MAX_SKEW: i64 = 86_400;

/// One entry of the committed trust store.
///
/// The generator (`tools/gen_debian_keyring.py`) writes these as constants and
/// pins everything the trust decision needs: the primary fingerprint, the
/// subkey fingerprints, the validity window and the *exact* keyring bytes. The
/// runtime re-derives the fingerprints from `block` with the local SHA-1 and
/// refuses on any disagreement.
#[derive(Debug, Clone, Copy)]
pub struct PinnedKey {
    /// User ID string (human label for diagnostics; also cross-checked).
    pub label: &'static str,
    /// v4 fingerprint of the primary key.
    pub fingerprint: [u8; 20],
    /// Public-key algorithm id of the primary key.
    pub algo: u8,
    /// Key size in bits (RSA modulus bits, or the curve size).
    pub bits: u16,
    /// Primary key creation time (Unix seconds).
    pub created: u32,
    /// Primary key expiry (Unix seconds), `None` for a key that does not expire.
    pub expires: Option<u32>,
    /// The keyring block: this primary key, its user IDs, its certifications, its
    /// subkeys and their binding signatures.
    pub block: &'static [u8],
    /// v4 fingerprints of the subkeys that may sign.
    pub subkeys: &'static [[u8; 20]],
}

/// Why a signature was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPgpError {
    /// Armor/packet/key-block layer refused the bytes.
    Packet(PacketError),
    /// The signature did not verify (or the key material was unusable).
    Crypto(CryptoError),
    /// The RTC is below [`CLOCK_FLOOR`]: no date can be trusted, so nothing is.
    ClockUnset,
    /// The blob carried no signature packet.
    NoSignature,
    /// More than [`packet::MAX_SIGNATURES`] signature packets.
    TooManySignatures,
    /// Signature type does not match the framing (e.g. a binary signature inside
    /// a clear-signed block).
    SigTypeMismatch,
    /// A clear-signed message's `Hash:` header does not name the hash algorithm
    /// the signature uses.
    HashHeaderMismatch,
    /// Hash algorithm this verifier does not implement (MD5/SHA-1 included).
    UnsupportedHashAlgo(u8),
    /// Public-key algorithm this verifier does not implement.
    UnsupportedPubAlgo(u8),
    /// The signature's public-key algorithm does not match the key's material
    /// (e.g. an ECDSA signature packet issued by an RSA key).
    AlgoMismatch,
    /// No signature carried an issuer fingerprint or key ID.
    NoIssuer,
    /// No signature verified against a pinned key.
    NoTrustedSignature,
    /// A pinned key's signature did not verify mathematically.
    BadSignature,
    /// The committed keyring block does not hash to its pinned fingerprint.
    FingerprintMismatch,
    /// The committed keyring block disagrees with its pinned metadata
    /// (algorithm, size, creation time or expiry).
    KeyMetadataMismatch,
    /// The primary key carries no self-signature that verifies with its own key.
    UncertifiedKey,
    /// A pinned subkey has no binding signature.
    NoSubkeyBinding,
    /// A pinned subkey's binding signature does not verify.
    BadSubkeyBinding,
    /// Key flags forbid signing with this key.
    NotASigningKey,
    /// The key's validity window has closed.
    Expired,
    /// The key is not valid yet.
    NotYetValid,
    /// The key (or subkey) carries a revocation signature that verifies.
    Revoked,
    /// The signature is dated implausibly far in the future, or before the key
    /// existed.
    FutureSignature,
}

impl From<PacketError> for OpenPgpError {
    fn from(e: PacketError) -> Self {
        OpenPgpError::Packet(e)
    }
}

impl From<CryptoError> for OpenPgpError {
    fn from(e: CryptoError) -> Self {
        OpenPgpError::Crypto(e)
    }
}

/// The outcome of a successful verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    /// Primary fingerprint of the pinned key that vouched for the data.
    pub signer: [u8; 20],
    /// Fingerprint of the key that actually produced the signature (the primary
    /// key or one of its subkeys).
    pub key: [u8; 20],
    /// Human label of the pinned key.
    pub label: &'static str,
    /// Signature creation time, when the packet carried one.
    pub created: Option<u32>,
}

/// Which key of a pinned block produced (or should produce) a signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyChoice {
    Primary,
    Subkey(usize),
}

impl packet::Clearsigned<'_> {
    /// The `Hash:` armor header as OpenPGP hash algorithms.
    ///
    /// A clear-signed message must declare its hash algorithm (RFC 4880 §7.1);
    /// the signature is only accepted if the algorithm it names appears here.
    pub fn declared_hashes(&self) -> Vec<HashAlgo> {
        let mut out = Vec::new();
        for header in &self.headers {
            if header.key != b"Hash" {
                continue;
            }
            for token in header.value.split(|&b| b == b',') {
                let upper = token.to_ascii_uppercase();
                let token = trim(&upper);
                let alg = match token {
                    b"SHA256" => Some(HashAlgo::Sha256),
                    b"SHA384" => Some(HashAlgo::Sha384),
                    b"SHA512" => Some(HashAlgo::Sha512),
                    _ => None,
                };
                if let Some(alg) = alg {
                    if !out.contains(&alg) {
                        out.push(alg);
                    }
                }
            }
        }
        out
    }
}

fn trim(v: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = v.len();
    while start < end && v[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && v[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &v[start..end]
}

fn check_clock(now: i64) -> Result<(), OpenPgpError> {
    if now < CLOCK_FLOOR {
        Err(OpenPgpError::ClockUnset)
    } else {
        Ok(())
    }
}

/// Verify a detached signature (`Release.gpg`) over `data`.
///
/// `signature` may be an armored signature block or a binary packet stream (see
/// [`packet::dearmor_or_binary`]); `data` are the bytes of the `Release` file as
/// fetched, unmodified.
pub fn verify_detached(
    keyring: &[PinnedKey],
    signature: &[u8],
    data: &[u8],
    now: i64,
) -> Result<Verified, OpenPgpError> {
    check_clock(now)?;
    let binary = packet::dearmor_or_binary(signature)?;
    verify_stream(keyring, &binary, &[data], packet::SIG_BINARY, None, now)
}

/// Parse and verify a clear-signed message (`InRelease`).
///
/// Returns the parsed message (whose [`packet::Clearsigned::canonical_text`] is
/// the signed byte string and whose `text` is exactly the `Release` content the
/// caller must parse for the `SHA256:` section) together with the signer.
pub fn verify_clearsigned<'a>(
    keyring: &[PinnedKey],
    input: &'a [u8],
    now: i64,
) -> Result<(packet::Clearsigned<'a>, Verified), OpenPgpError> {
    check_clock(now)?;
    let cs = packet::parse_clearsigned(input)?;
    let declared = cs.declared_hashes();
    let canonical = cs.canonical_text();
    let binary = packet::dearmor(cs.signature)?.data;
    let verified = verify_stream(
        keyring,
        &binary,
        &[&canonical],
        packet::SIG_TEXT,
        Some(&declared),
        now,
    )?;
    Ok((cs, verified))
}

/// Verify the signature packets of a decoded blob over `parts`.
fn verify_stream(
    keyring: &[PinnedKey],
    binary: &[u8],
    parts: &[&[u8]],
    expected_sig_type: u8,
    declared_hashes: Option<&[HashAlgo]>,
    now: i64,
) -> Result<Verified, OpenPgpError> {
    let mut signatures: Vec<SignaturePacket<'_>> = Vec::new();
    for item in packet::packets(binary) {
        let pkt = item?;
        if pkt.tag != 2 {
            continue;
        }
        if signatures.len() >= packet::MAX_SIGNATURES {
            return Err(OpenPgpError::TooManySignatures);
        }
        signatures.push(packet::parse_signature(pkt.body)?);
    }
    if signatures.is_empty() {
        return Err(OpenPgpError::NoSignature);
    }

    let mut saw_unidentified = false;
    let mut winner: Option<Verified> = None;
    for sig in &signatures {
        if sig.sig_type != expected_sig_type {
            return Err(OpenPgpError::SigTypeMismatch);
        }
        // The `Hash:` header of a clear-signed message declares the hash
        // algorithms used for the WHOLE message, so every signature packet in
        // the block is checked against it.
        if let Some(declared) = declared_hashes {
            match HashAlgo::from_id(sig.hash_algo) {
                Some(alg) if declared.contains(&alg) => {}
                _ => return Err(OpenPgpError::HashHeaderMismatch),
            }
        }
        let located = match locate(keyring, sig) {
            Some(loc) => loc,
            None => {
                if sig.issuer_fpr.is_none() && sig.issuer_keyid.is_none() {
                    saw_unidentified = true;
                }
                continue;
            }
        };
        // A pinned key's signature must verify completely; any failure here is
        // fatal (never "another signature was fine"). Verification therefore
        // continues over the remaining packets even after a good one, so a
        // tampered signature appended behind a valid one cannot hide.
        let verified = verify_with_pinned(keyring, located, sig, parts, now)?;
        if winner.is_none() {
            winner = Some(verified);
        }
    }

    winner.ok_or(if saw_unidentified {
        OpenPgpError::NoIssuer
    } else {
        OpenPgpError::NoTrustedSignature
    })
}

/// Find the pinned key (and which of its keys) a signature claims to come from.
fn locate(keyring: &[PinnedKey], sig: &SignaturePacket<'_>) -> Option<(usize, KeyChoice)> {
    if let Some(fpr) = sig.issuer_fpr {
        for (i, key) in keyring.iter().enumerate() {
            if key.fingerprint == fpr {
                return Some((i, KeyChoice::Primary));
            }
            for (j, sub) in key.subkeys.iter().enumerate() {
                if *sub == fpr {
                    return Some((i, KeyChoice::Subkey(j)));
                }
            }
        }
        return None;
    }
    if let Some(id) = sig.issuer_keyid {
        // Key IDs are a *hint* only: the signature still has to verify with the
        // key the entry names, so a collision cannot widen trust.
        for (i, key) in keyring.iter().enumerate() {
            if key.fingerprint[12..] == id {
                return Some((i, KeyChoice::Primary));
            }
            for (j, sub) in key.subkeys.iter().enumerate() {
                if sub[12..] == id {
                    return Some((i, KeyChoice::Subkey(j)));
                }
            }
        }
    }
    None
}

/// Validate a committed keyring block against its pinned metadata, without
/// verifying any document signature.
///
/// This is the trust-store integrity check: the block must hash to the pinned
/// fingerprint, agree with the pinned algorithm/size/creation/expiry, carry a
/// self-certification that verifies with its own key, and have every pinned
/// subkey bound by a binding signature issued by the primary. It is what the
/// host property P53 runs against the committed Debian keyring (real GnuPG
/// signatures over RSA-4096 and Ed25519 keys) and what the apt trust chain can
/// run before relying on the keyring at all.
pub fn check_pinned_key(pinned: &PinnedKey, now: i64) -> Result<(), OpenPgpError> {
    check_clock(now)?;
    let block = packet::parse_key_block(pinned.block)?;
    validate_block(pinned, &block, now)?;
    Ok(())
}

/// Every check that binds a keyring block to its pin, plus revocation and
/// validity. Returns the primary key's certification (expiry and key flags).
fn validate_block(
    pinned: &PinnedKey,
    block: &packet::KeyBlock<'_>,
    now: i64,
) -> Result<PrimaryCert, OpenPgpError> {
    if block.primary.fingerprint() != pinned.fingerprint {
        return Err(OpenPgpError::FingerprintMismatch);
    }
    if block.primary.created != pinned.created
        || block.primary.bits() != pinned.bits
        || block.primary.algo != pinned.algo
    {
        return Err(OpenPgpError::KeyMetadataMismatch);
    }

    // The pinned label must be one of the key's own user IDs: a swapped or
    // truncated block cannot masquerade as the key the table names.
    if !block.uids.iter().any(|uid| *uid == pinned.label.as_bytes()) {
        return Err(OpenPgpError::KeyMetadataMismatch);
    }

    let primary_cert = certified(block)?;
    if pinned.expires != primary_cert.expires {
        return Err(OpenPgpError::KeyMetadataMismatch);
    }

    check_revocations(&block.revocations, &block.primary, block.primary.body, None)?;

    for want in pinned.subkeys {
        let sub = block
            .subkeys
            .iter()
            .find(|s| s.key.fingerprint() == *want)
            .ok_or(OpenPgpError::FingerprintMismatch)?;
        let binding = sub.binding.as_ref().ok_or(OpenPgpError::NoSubkeyBinding)?;
        let alg = HashAlgo::from_id(binding.hash_algo)
            .ok_or(OpenPgpError::UnsupportedHashAlgo(binding.hash_algo))?;
        let digest = crypto::key_pair_digest(
            alg,
            block.primary.body,
            sub.key.body,
            binding.hashed_portion,
        );
        let sig_bytes = signature_bytes(block.primary.material, &binding.mpis)?;
        verify_material(block.primary.material, alg, &digest, sig_bytes)
            .map_err(|_| OpenPgpError::BadSubkeyBinding)?;
        check_revocations(
            &sub.revocations,
            &block.primary,
            block.primary.body,
            Some(sub.key.body),
        )?;
    }

    if now + MAX_SKEW < pinned.created as i64 {
        return Err(OpenPgpError::NotYetValid);
    }
    if let Some(expires) = primary_cert.expires {
        if now > expires as i64 {
            return Err(OpenPgpError::Expired);
        }
    }
    Ok(primary_cert)
}

/// Check a pinned key block and verify `sig` with the selected key.
fn verify_with_pinned(
    keyring: &[PinnedKey],
    (idx, choice): (usize, KeyChoice),
    sig: &SignaturePacket<'_>,
    parts: &[&[u8]],
    now: i64,
) -> Result<Verified, OpenPgpError> {
    let pinned = &keyring[idx];
    let block = packet::parse_key_block(pinned.block)?;

    // 1. Trust-store integrity: fingerprint, metadata, self-certification,
    //    subkey bindings, revocations and the validity window of the primary.
    let primary_cert = validate_block(pinned, &block, now)?;

    // 2. Pick the key that signs: the primary itself or one of its subkeys.
    let (material, key_fpr, key_created, key_expires, key_flags) = match choice {
        KeyChoice::Primary => (
            block.primary.material,
            block.primary.fingerprint(),
            block.primary.created,
            primary_cert.expires,
            primary_cert.flags,
        ),
        KeyChoice::Subkey(j) => {
            let want = *pinned
                .subkeys
                .get(j)
                .ok_or(OpenPgpError::FingerprintMismatch)?;
            let sub = block
                .subkeys
                .iter()
                .find(|s| s.key.fingerprint() == want)
                .ok_or(OpenPgpError::FingerprintMismatch)?;
            // The binding was verified by `validate_block`; only its declared
            // validity data is needed here.
            let binding = sub.binding.as_ref().ok_or(OpenPgpError::NoSubkeyBinding)?;
            (
                sub.key.material,
                sub.key.fingerprint(),
                sub.key.created,
                expiry_of(sub.key.created, binding.key_expiry),
                binding.key_flags,
            )
        }
    };

    // 3. Key validity: key flags and the signature's own timestamp.
    if let Some(flags) = key_flags {
        if flags & packet::KEY_FLAG_SIGN == 0 {
            return Err(OpenPgpError::NotASigningKey);
        }
    }
    if let Some(expires) = key_expires {
        if now > expires as i64 {
            return Err(OpenPgpError::Expired);
        }
    }
    if now + MAX_SKEW < key_created as i64 {
        return Err(OpenPgpError::NotYetValid);
    }
    if let Some(created) = sig.created {
        let created = created as i64;
        if created > now + MAX_SKEW {
            return Err(OpenPgpError::FutureSignature);
        }
        // A signature cannot predate the key it was made with (allow skew).
        if created + MAX_SKEW < key_created as i64 {
            return Err(OpenPgpError::FutureSignature);
        }
        if let Some(expires) = key_expires {
            if created > expires as i64 + MAX_SKEW {
                return Err(OpenPgpError::Expired);
            }
        }
    }

    // 4. Verify the document signature itself. The signature packet's declared
    //    public-key algorithm must match the key material it claims to use.
    let compatible = match material {
        packet::KeyMaterial::Rsa { .. } => {
            sig.pub_algo == packet::ALGO_RSA || sig.pub_algo == packet::ALGO_RSA_SIGN
        }
        _ => sig.pub_algo == alg_of(material),
    };
    if !compatible {
        return Err(OpenPgpError::AlgoMismatch);
    }
    let alg =
        HashAlgo::from_id(sig.hash_algo).ok_or(OpenPgpError::UnsupportedHashAlgo(sig.hash_algo))?;
    let digest = crypto::v4_digest(alg, parts, sig.hashed_portion);
    if digest.first_chunk::<2>() != Some(&sig.left16) {
        return Err(OpenPgpError::BadSignature);
    }
    let sig_bytes = signature_bytes(material, &sig.mpis)?;
    verify_material(material, alg, &digest, sig_bytes)?;

    Ok(Verified {
        signer: pinned.fingerprint,
        key: key_fpr,
        label: pinned.label,
        created: sig.created,
    })
}

/// Expiry instant of a key given its creation time and the subpacket-9 value.
fn expiry_of(created: u32, expiry: Option<u32>) -> Option<u32> {
    match expiry {
        Some(secs) if secs > 0 => Some(created.saturating_add(secs)),
        _ => None,
    }
}

/// The primary key's self-certification: expiry and key flags.
struct PrimaryCert {
    expires: Option<u32>,
    flags: Option<u8>,
}

/// Require at least one certification of the primary key that verifies with the
/// primary key itself, and return the validity data it carries.
///
/// Cross-certifications by *other* keys also live in this list (Debian ships
/// several); they simply fail to verify with our primary's key and are skipped.
fn certified(block: &packet::KeyBlock<'_>) -> Result<PrimaryCert, OpenPgpError> {
    let mut best: Option<PrimaryCert> = None;
    for cert in &block.certifications {
        let Some(alg) = HashAlgo::from_id(cert.hash_algo) else {
            continue;
        };
        let Ok(sig_bytes) = signature_bytes(block.primary.material, &cert.mpis) else {
            continue;
        };
        // Direct key signature: the hash covers only the key body. A
        // certification (0x11–0x13) covers the key body plus one user ID, so
        // every user ID is tried until one pairing verifies.
        let digest: Option<Vec<u8>> = match cert.sig_type {
            packet::SIG_DIRECT_KEY => {
                let d = crypto::key_digest(alg, block.primary.body, cert.hashed_portion);
                if d.first_chunk::<2>() == Some(&cert.left16)
                    && verify_material(block.primary.material, alg, &d, sig_bytes).is_ok()
                {
                    Some(d)
                } else {
                    None
                }
            }
            packet::SIG_GENERIC_CERT..=packet::SIG_POSITIVE_CERT => {
                let mut found = None;
                for uid in &block.uids {
                    let d = crypto::certification_digest(
                        alg,
                        block.primary.body,
                        uid,
                        cert.hashed_portion,
                    );
                    if d.first_chunk::<2>() != Some(&cert.left16) {
                        continue;
                    }
                    if verify_material(block.primary.material, alg, &d, sig_bytes).is_ok() {
                        found = Some(d);
                        break;
                    }
                }
                found
            }
            _ => None,
        };
        if digest.is_none() {
            // Cross-certification by another key, or a signature this verifier
            // cannot check: not a self-certification, so it carries no weight.
            continue;
        }
        let this = PrimaryCert {
            expires: expiry_of(block.primary.created, cert.key_expiry),
            flags: cert.key_flags,
        };
        best = Some(match best {
            // Prefer the most conservative expiry when several certifications
            // verify (a later re-certification may extend it).
            Some(prev) => PrimaryCert {
                expires: match (prev.expires, this.expires) {
                    (Some(a), Some(b)) => Some(core::cmp::min(a, b)),
                    (Some(a), None) => Some(a),
                    (None, b) => b,
                },
                flags: prev.flags.or(this.flags),
            },
            None => this,
        });
    }
    best.ok_or(OpenPgpError::UncertifiedKey)
}

/// Refuse if any revocation signature verifies with the (primary) key.
///
/// The revocation has to verify — a bare, unverifiable 0x20 packet is not an
/// attack (an attacker cannot revoke a key they do not hold), but it also is not
/// a reason to refuse; the pinned block is a constant that the generator
/// reviewed, so in practice this is a tripwire for a hand-edited artifact.
fn check_revocations(
    revocations: &[SignaturePacket<'_>],
    revoker: &packet::PublicKey<'_>,
    primary_body: &[u8],
    subkey_body: Option<&[u8]>,
) -> Result<(), OpenPgpError> {
    for rev in revocations {
        let Some(alg) = HashAlgo::from_id(rev.hash_algo) else {
            continue;
        };
        // A key revocation (0x20) is hashed over the revoked key alone; a
        // subkey revocation (0x28) over the primary key and the subkey.
        let digest = match subkey_body {
            Some(sub) => crypto::key_pair_digest(alg, primary_body, sub, rev.hashed_portion),
            None => crypto::key_digest(alg, primary_body, rev.hashed_portion),
        };
        let Ok(sig_bytes) = signature_bytes(revoker.material, &rev.mpis) else {
            continue;
        };
        if verify_material(revoker.material, alg, &digest, sig_bytes).is_ok() {
            return Err(OpenPgpError::Revoked);
        }
    }
    Ok(())
}

/// Expected MPI count per public-key algorithm.
fn signature_bytes<'a>(
    material: packet::KeyMaterial<'_>,
    mpis: &'a [packet::Mpi<'a>],
) -> Result<SignatureMpis<'a>, OpenPgpError> {
    match material {
        packet::KeyMaterial::Rsa { .. } => {
            let mpi = mpis.first().ok_or(OpenPgpError::AlgoMismatch)?;
            if mpis.len() != 1 {
                return Err(OpenPgpError::AlgoMismatch);
            }
            Ok(SignatureMpis::Rsa(mpi.bytes))
        }
        packet::KeyMaterial::Ed25519 { .. } | packet::KeyMaterial::Ecdsa { .. } => {
            if mpis.len() != 2 {
                return Err(OpenPgpError::AlgoMismatch);
            }
            Ok(SignatureMpis::Pair(mpis[0].bytes, mpis[1].bytes))
        }
    }
}

/// A binding signature is always made by the primary key, so its MPI shape must
/// match the primary's algorithm — the same rule as a document signature.
/// Algorithm-shaped signature MPIs (`Copy`: an RSA signature is one MPI, a
/// EdDSA/ECDSA signature is the pair r, s).
#[derive(Clone, Copy)]
enum SignatureMpis<'a> {
    Rsa(&'a [u8]),
    Pair(&'a [u8], &'a [u8]),
}

/// Verify a signature over an already computed digest with the given key.
fn verify_material(
    material: packet::KeyMaterial<'_>,
    alg: HashAlgo,
    digest: &[u8],
    sig: SignatureMpis<'_>,
) -> Result<(), OpenPgpError> {
    match (material, sig) {
        (packet::KeyMaterial::Rsa { n, e }, SignatureMpis::Rsa(s)) => {
            crypto::verify_rsa(n, e, alg, digest, s)?;
            Ok(())
        }
        (packet::KeyMaterial::Ed25519 { point }, SignatureMpis::Pair(r, s)) => {
            crypto::verify_ed25519(point, digest, r, s)?;
            Ok(())
        }
        (packet::KeyMaterial::Ecdsa { curve, point }, SignatureMpis::Pair(r, s)) => {
            match curve {
                packet::Curve::P256 => crypto::verify_ecdsa_p256(point, digest, r, s)?,
                packet::Curve::P384 => crypto::verify_ecdsa_p384(point, digest, r, s)?,
                // P-521 has no compiled-in backend: refuse by name rather than
                // pretending the signature was bad.
                packet::Curve::P521 => {
                    return Err(OpenPgpError::Crypto(CryptoError::UnsupportedCurve))
                }
            }
            Ok(())
        }
        _ => Err(OpenPgpError::AlgoMismatch),
    }
}

/// The public-key algorithm a key material must be used with, for the check
/// against a signature packet's declared algorithm.
pub fn alg_of(material: packet::KeyMaterial<'_>) -> u8 {
    match material {
        packet::KeyMaterial::Rsa { .. } => packet::ALGO_RSA,
        packet::KeyMaterial::Ed25519 { .. } => packet::ALGO_EDDSA,
        packet::KeyMaterial::Ecdsa { .. } => packet::ALGO_ECDSA,
    }
}

/// Convenience for callers that already parsed a clear-signed message.
pub fn verify_parsed_clearsigned(
    keyring: &[PinnedKey],
    cs: &packet::Clearsigned<'_>,
    now: i64,
) -> Result<Verified, OpenPgpError> {
    check_clock(now)?;
    let declared = cs.declared_hashes();
    let canonical = cs.canonical_text();
    let binary = packet::dearmor(cs.signature)?.data;
    verify_stream(
        keyring,
        &binary,
        &[&canonical],
        packet::SIG_TEXT,
        Some(&declared),
        now,
    )
}
