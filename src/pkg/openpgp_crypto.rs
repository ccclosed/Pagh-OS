//! Cryptographic primitives for the pure OpenPGP verifier (issue #32).
//!
//! This is the only place in the OpenPGP stack that touches a crypto crate, and
//! it is `core` + `alloc` only: the kernel compiles this exact source for the
//! bare-metal target, while `host-tests` `#[path]`-includes it and exercises it
//! on the host (P51–P53). No new dependency is introduced — the verifier reuses
//! the `sha2`/`rsa`/`ed25519-dalek`/`p256`/`p384` crates the tree already pins
//! for the TLS certificate verifier (`net::tls_verify`).
//!
//! ## What is signed
//!
//! An OpenPGP v4 signature is computed over
//!
//! ```text
//! H = HASH( signed_data || hashed_portion || 0x04 0xFF || be32(len(hashed_portion)) )
//! ```
//!
//! where `hashed_portion` is the signature packet body from the version octet
//! through the end of the hashed subpackets (RFC 4880 §5.2.4 / RFC 9580 §5.2.4;
//! the trailer length is the length of that whole portion, *not* the length of
//! the subpacket area alone — getting this wrong makes every real signature
//! fail, and it is verified against live Debian metadata in P52).
//!
//! `H` — never the raw document — is what each algorithm consumes:
//!
//! * **RSA** (public-key algo 1/3): PKCS#1 v1.5 with the DigestInfo prefix, via
//!   the pre-hashed `RsaPublicKey::verify(Pkcs1v15Sign::new::<D>(), &H, &sig)`
//!   entry point. Two traps: the signature MPI must be **left-padded to the
//!   modulus byte length** (`rsa` rejects `sig_len != modulus size`), and the
//!   `pkcs1v15::VerifyingKey::verify` helper must NOT be used — it would hash the
//!   digest a second time.
//! * **EdDSA** (algo 22, Ed25519): OpenPGP EdDSA signs the *digest*, so this is
//!   `VerifyingKey::verify_strict(&H, &sig)` — not a signature over the document.
//!   R and S arrive as MPIs and are left-padded to the 32-octet RFC 8032 form.
//! * **ECDSA** (algo 19): ECDSA over `H` with the curve named by the key's OID;
//!   the r/s MPIs are left-padded to the field size (32/48/66 octets).
//!
//! ## SHA-1 is for fingerprints only
//!
//! No `sha1` crate is vendored, and OpenPGP v4 **fingerprints** are
//! `SHA1(0x99 || be16(len(body)) || body)`, needed to tie a key packet to a
//! *pinned* 20-octet fingerprint and to match a signature's issuer fingerprint.
//! [`sha1`] therefore implements RFC 3174 locally. It is never a signature or
//! MAC hash and no trust decision depends on its collision resistance: a wrong
//! implementation can only make a valid key look absent (a refusal), never make a
//! foreign key acceptable, because acceptance compares the **pinned constant**
//! against the bytes carried in the signature.

#![allow(dead_code)]

use alloc::vec::Vec;

use rsa::traits::PublicKeyParts;
use sha2::{Digest, Sha256, Sha384, Sha512};

/// Minimum RSA modulus accepted (bits).
pub const RSA_MIN_MODULUS_BITS: usize = 2048;
/// Maximum RSA modulus: `rsa::RsaPublicKey::MAX_SIZE` is 4096 bits, so anything
/// larger cannot be verified by the pinned crate and is refused by name rather
/// than failing later with an opaque error. Real Debian archive keys are RSA-4096.
pub const RSA_MAX_MODULUS_BITS: usize = 4096;

/// Signature-verification failure. Kept narrow on purpose: callers map it onto
/// the user-visible refusal codes of `pkg::openpgp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    /// The RSA modulus is below [`RSA_MIN_MODULUS_BITS`].
    KeyTooSmall,
    /// The RSA modulus exceeds [`RSA_MAX_MODULUS_BITS`].
    KeyTooLarge,
    /// Key material the backend rejected (bad point, non-invertible/exponent
    /// range, malformed encoding).
    MalformedKey,
    /// Signature MPIs longer than the key size, or the wrong count for the algo.
    MalformedSignature,
    /// Mathematically valid encoding, wrong signature (or wrong key).
    VerifyFailed,
    /// Curve OID not supported by the compiled-in backends.
    UnsupportedCurve,
}

/// Hash algorithm identifiers (RFC 9580 §9.4) supported by the verifier.
///
/// SHA-256 is what Debian archive keys use today; SHA-384/512 are accepted
/// because they cost nothing and appear in the wild. MD5 (1) and SHA-1 (2) are
/// deliberately absent: a signature that names them is refused, never skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    /// id 8 — SHA-256.
    Sha256,
    /// id 9 — SHA-384.
    Sha384,
    /// id 10 — SHA-512.
    Sha512,
}

impl HashAlgo {
    /// Map an OpenPGP hash-algorithm octet to a supported algorithm.
    pub fn from_id(id: u8) -> Option<HashAlgo> {
        match id {
            8 => Some(HashAlgo::Sha256),
            9 => Some(HashAlgo::Sha384),
            10 => Some(HashAlgo::Sha512),
            _ => None,
        }
    }

    /// The OpenPGP hash-algorithm octet (feeds the `Hash:` armor header check).
    pub fn id(self) -> u8 {
        match self {
            HashAlgo::Sha256 => 8,
            HashAlgo::Sha384 => 9,
            HashAlgo::Sha512 => 10,
        }
    }

    /// Digest length in octets.
    pub fn size(self) -> usize {
        match self {
            HashAlgo::Sha256 => 32,
            HashAlgo::Sha384 => 48,
            HashAlgo::Sha512 => 64,
        }
    }
}

/// SHA-256 of `data`.
///
/// The apt trust chain uses this for the digests the signed `Release` declares:
/// the `Packages` body (`verify_index_body`) and every downloaded `.deb`
/// (`verify_package_body`). It lives here so the kernel's repository-metadata
/// path has exactly one SHA-256 implementation, the same one the signature
/// digests use.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    out
}

/// Compute the v4 signature digest `H` over `parts || hashed_portion || trailer`.
///
/// `parts` is the signed data split into the pieces the caller already owns (a
/// document body, or a key packet body plus a user-ID packet for a
/// certification) so the verifier never allocates a concatenated copy of a
/// multi-hundred-KiB `Release`.
pub fn v4_digest(alg: HashAlgo, parts: &[&[u8]], hashed_portion: &[u8]) -> Vec<u8> {
    // be32 length of the hashed portion; the parser bounds it far below 2^32.
    let len = (hashed_portion.len() as u32).to_be_bytes();
    let trailer = [0x04u8, 0xFF];
    match alg {
        HashAlgo::Sha256 => {
            let mut h = Sha256::new();
            for p in parts {
                h.update(p);
            }
            h.update(hashed_portion);
            h.update(trailer);
            h.update(len);
            h.finalize().to_vec()
        }
        HashAlgo::Sha384 => {
            let mut h = Sha384::new();
            for p in parts {
                h.update(p);
            }
            h.update(hashed_portion);
            h.update(trailer);
            h.update(len);
            h.finalize().to_vec()
        }
        HashAlgo::Sha512 => {
            let mut h = Sha512::new();
            for p in parts {
                h.update(p);
            }
            h.update(hashed_portion);
            h.update(trailer);
            h.update(len);
            h.finalize().to_vec()
        }
    }
}

/// The CTB GnuPG hashes a public key/subkey with: `0x99 || be16(len) || body`.
///
/// This is the same form the v4 *fingerprint* uses, and it is what
/// `do_hash_public_key()` feeds the hash context for key signatures,
/// certifications and subkey bindings (verified against Debian's real archive
/// keys in P53 — using the bare packet body instead makes every one of them
/// fail). Note it is NOT the on-the-wire packet header: the length is always
/// two octets, whatever the packet's own encoding was.
pub const KEY_HASH_HEADER: u8 = 0x99;

/// The legacy GnuPG `user ID packet` framing byte used by certification hashes:
/// an old-format CTB for tag 13 followed by a **four**-octet length.
pub const USERID_HASH_HEADER: u8 = 0xB4;

/// Append `key_body` in the hashed form (`0x99 || be16(len) || body`).
fn push_hashed_key(out: &mut Vec<u8>, key_body: &[u8]) {
    out.push(KEY_HASH_HEADER);
    out.extend_from_slice(&(key_body.len() as u16).to_be_bytes());
    out.extend_from_slice(key_body);
}

/// Digest of a signature made *directly over a key* (type 0x1F) or of a key
/// revocation (0x20): `HASH(0x99||len||key_body || hashed_portion || trailer)`.
pub fn key_digest(alg: HashAlgo, key_body: &[u8], hashed_portion: &[u8]) -> Vec<u8> {
    let mut pre: Vec<u8> = Vec::with_capacity(key_body.len() + 3);
    push_hashed_key(&mut pre, key_body);
    v4_digest(alg, &[&pre], hashed_portion)
}

/// Digest of a subkey binding (0x18) or subkey revocation (0x28):
/// `HASH(key || subkey || hashed_portion || trailer)`, both in hashed form.
pub fn key_pair_digest(
    alg: HashAlgo,
    primary_body: &[u8],
    subkey_body: &[u8],
    hashed_portion: &[u8],
) -> Vec<u8> {
    let mut pre: Vec<u8> = Vec::with_capacity(primary_body.len() + subkey_body.len() + 6);
    push_hashed_key(&mut pre, primary_body);
    push_hashed_key(&mut pre, subkey_body);
    v4_digest(alg, &[&pre], hashed_portion)
}

/// Digest of a User ID certification (0x10–0x13):
/// `HASH(key || 0xB4 || be32(uid_len) || uid || hashed_portion || trailer)`.
pub fn certification_digest(
    alg: HashAlgo,
    key_body: &[u8],
    uid: &[u8],
    hashed_portion: &[u8],
) -> Vec<u8> {
    let mut pre: Vec<u8> = Vec::with_capacity(key_body.len() + uid.len() + 8);
    push_hashed_key(&mut pre, key_body);
    pre.push(USERID_HASH_HEADER);
    pre.extend_from_slice(&(uid.len() as u32).to_be_bytes());
    pre.extend_from_slice(uid);
    v4_digest(alg, &[&pre], hashed_portion)
}

/// Number of significant bits in a big-endian magnitude without leading zeros.
fn bit_len(bytes: &[u8]) -> usize {
    let trimmed = trim_zeros(bytes);
    match trimmed.first() {
        None => 0,
        Some(&first) => {
            let leading = first.leading_zeros() as usize;
            (trimmed.len() - 1) * 8 + (8 - leading)
        }
    }
}

/// Strip leading zero octets (MPIs are transmitted without them, but a redundant
/// zero must not change the value or the byte size the signature is padded to).
fn trim_zeros(bytes: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < bytes.len() && bytes[i] == 0 {
        i += 1;
    }
    &bytes[i..]
}

/// Left-pad `value` with zeros to `len` octets; `None` if it does not fit.
fn left_pad(value: &[u8], len: usize) -> Option<Vec<u8>> {
    let v = trim_zeros(value);
    if v.len() > len {
        return None;
    }
    let mut out = alloc::vec![0u8; len];
    out[len - v.len()..].copy_from_slice(v);
    Some(out)
}

/// RSA PKCS#1 v1.5 verification over the pre-computed digest `H`.
///
/// `sig` is the raw MPI magnitude; it is left-padded to the modulus length
/// because `rsa` compares the signature length against the key size.
pub fn verify_rsa(
    n: &[u8],
    e: &[u8],
    alg: HashAlgo,
    digest: &[u8],
    sig: &[u8],
) -> Result<(), CryptoError> {
    let bits = bit_len(n);
    if bits < RSA_MIN_MODULUS_BITS {
        return Err(CryptoError::KeyTooSmall);
    }
    if bits > RSA_MAX_MODULUS_BITS {
        return Err(CryptoError::KeyTooLarge);
    }
    if digest.len() != alg.size() {
        return Err(CryptoError::MalformedSignature);
    }
    let modulus = rsa::BigUint::from_bytes_be(trim_zeros(n));
    let exponent = rsa::BigUint::from_bytes_be(trim_zeros(e));
    let public =
        rsa::RsaPublicKey::new(modulus, exponent).map_err(|_| CryptoError::MalformedKey)?;
    let k = public.size();
    let sig = left_pad(sig, k).ok_or(CryptoError::MalformedSignature)?;
    let scheme = match alg {
        HashAlgo::Sha256 => rsa::Pkcs1v15Sign::new::<Sha256>(),
        HashAlgo::Sha384 => rsa::Pkcs1v15Sign::new::<Sha384>(),
        HashAlgo::Sha512 => rsa::Pkcs1v15Sign::new::<Sha512>(),
    };
    public
        .verify(scheme, digest, &sig)
        .map_err(|_| CryptoError::VerifyFailed)
}

/// Ed25519 (OpenPGP algo 22) verification. `point` is the 32-octet public key
/// (the 0x40 prefix of the MPI is stripped by the caller); the message signed is
/// the digest `H`.
pub fn verify_ed25519(point: &[u8], digest: &[u8], r: &[u8], s: &[u8]) -> Result<(), CryptoError> {
    let key: [u8; 32] = <[u8; 32]>::try_from(point).map_err(|_| CryptoError::MalformedKey)?;
    let verifying =
        ed25519_dalek::VerifyingKey::from_bytes(&key).map_err(|_| CryptoError::MalformedKey)?;
    let r = left_pad(r, 32).ok_or(CryptoError::MalformedSignature)?;
    let s = left_pad(s, 32).ok_or(CryptoError::MalformedSignature)?;
    let mut sig = [0u8; 64];
    sig[..32].copy_from_slice(&r);
    sig[32..].copy_from_slice(&s);
    let sig = ed25519_dalek::Signature::from_bytes(&sig);
    // `verify_strict` additionally rejects non-canonical S and small-order
    // keys. Real GnuPG Ed25519 signatures are canonical (verified against live
    // Debian metadata in P52), so the stricter check costs no interoperability.
    verifying
        .verify_strict(digest, &sig)
        .map_err(|_| CryptoError::VerifyFailed)
}

/// ECDSA (P-256) verification over the digest `H`.
pub fn verify_ecdsa_p256(
    point: &[u8],
    digest: &[u8],
    r: &[u8],
    s: &[u8],
) -> Result<(), CryptoError> {
    use signature::hazmat::PrehashVerifier;
    let verifying =
        p256::ecdsa::VerifyingKey::from_sec1_bytes(point).map_err(|_| CryptoError::MalformedKey)?;
    let r = left_pad(r, 32).ok_or(CryptoError::MalformedSignature)?;
    let s = left_pad(s, 32).ok_or(CryptoError::MalformedSignature)?;
    let mut raw = [0u8; 64];
    raw[..32].copy_from_slice(&r);
    raw[32..].copy_from_slice(&s);
    let sig =
        p256::ecdsa::Signature::from_slice(&raw).map_err(|_| CryptoError::MalformedSignature)?;
    // `verify_prehash` truncates/zero-extends the digest to the field size
    // internally (`ecdsa::hazmat::bits2field`), which is exactly the ECDSA
    // bits2int rule for byte-aligned curves.
    verifying
        .verify_prehash(digest, &sig)
        .map_err(|_| CryptoError::VerifyFailed)
}

/// ECDSA (P-384) verification over the digest `H` — same shape as P-256.
pub fn verify_ecdsa_p384(
    point: &[u8],
    digest: &[u8],
    r: &[u8],
    s: &[u8],
) -> Result<(), CryptoError> {
    use signature::hazmat::PrehashVerifier;
    let verifying =
        p384::ecdsa::VerifyingKey::from_sec1_bytes(point).map_err(|_| CryptoError::MalformedKey)?;
    let r = left_pad(r, 48).ok_or(CryptoError::MalformedSignature)?;
    let s = left_pad(s, 48).ok_or(CryptoError::MalformedSignature)?;
    let mut raw = [0u8; 96];
    raw[..48].copy_from_slice(&r);
    raw[48..].copy_from_slice(&s);
    let sig =
        p384::ecdsa::Signature::from_slice(&raw).map_err(|_| CryptoError::MalformedSignature)?;
    verifying
        .verify_prehash(digest, &sig)
        .map_err(|_| CryptoError::VerifyFailed)
}

// ── SHA-1 (fingerprints only — see the module docs) ───────────────────────────

const SHA1_INIT: [u32; 5] = [
    0x6745_2301,
    0xEFCD_AB89,
    0x98BA_DCFE,
    0x1032_5476,
    0xC3D2_E1F0,
];

/// Streaming SHA-1 (RFC 3174). Used for OpenPGP v4 fingerprints only.
#[derive(Clone)]
pub struct Sha1 {
    h: [u32; 5],
    block: [u8; 64],
    used: usize,
    total: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha1 {
    /// A fresh SHA-1 state.
    pub fn new() -> Sha1 {
        Sha1 {
            h: SHA1_INIT,
            block: [0u8; 64],
            used: 0,
            total: 0,
        }
    }

    /// Absorb `data`.
    pub fn update(&mut self, data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        let mut rest = data;

        if self.used > 0 {
            let need = 64 - self.used;
            let take = core::cmp::min(need, rest.len());
            self.block[self.used..self.used + take].copy_from_slice(&rest[..take]);
            self.used += take;
            rest = &rest[take..];
            if self.used == 64 {
                let block = self.block;
                self.compress(&block);
                self.used = 0;
            }
        }

        while rest.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&rest[..64]);
            self.compress(&block);
            rest = &rest[64..];
        }

        if !rest.is_empty() {
            self.block[..rest.len()].copy_from_slice(rest);
            self.used = rest.len();
        }
    }

    /// Finish and return the 20-octet digest.
    pub fn finish(mut self) -> [u8; 20] {
        let bits = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.used != 56 {
            self.update(&[0x00]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 20];
        for (i, word) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) =
            (self.h[0], self.h[1], self.h[2], self.h[3], self.h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(b);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
        self.h[4] = self.h[4].wrapping_add(e);
    }
}

/// One-shot SHA-1.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(data);
    h.finish()
}

/// OpenPGP v4 fingerprint of a public key / subkey packet body:
/// `SHA1(0x99 || be16(len(body)) || body)` (RFC 9580 §5.5.4).
pub fn v4_fingerprint(body: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(&[0x99]);
    h.update(&(body.len() as u16).to_be_bytes());
    h.update(body);
    h.finish()
}
