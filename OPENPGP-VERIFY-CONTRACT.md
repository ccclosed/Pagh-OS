# OpenPGP verification contract — apt metadata and `.deb` digests (issue #32)

Status: **requirements / contract** (task `t20`). No code is changed by this document.
Scope of the downstream implementation: HARDENING.md checklist items 2–4 and the
negative-test half of item 5, plus the matching lines of `SECURITY.md`
("What is still not verified" / release requirements 3–5).

Sources read for this contract: issue #32; `HARDENING.md`; `SECURITY.md`;
`src/pkg/{apt,apt_index,mirror,deb,install}.rs` + `src/pkg/README.md`;
`src/net/{http_fetch,tls,tls_auth,tls_chain,x509}.rs`; `tools/gen_ca_bundle.py`;
`tools/mini_repo.py`; `tools/e2e.py`; `src/selftest_lx.rs`; `src/shell/commands.rs`.

Everything marked **[verified]** in this document was established empirically against
live Debian artifacts and/or `gpgv` 2.4.9 during this task; the exact commands and the
resulting vectors are in §12 so the implementer does not have to re-derive them.

---

## 0. Scope and non-goals

**In scope**

1. A `no_std` OpenPGP *verification-only* stack: packet parsing, signature verification,
   keyring handling. No signing, no key generation, no key management UI.
2. A committed, fingerprint-pinned Debian archive keyring with a deliberate regeneration
   procedure (the `tools/gen_ca_bundle.py` pattern applied to OpenPGP).
3. The full trust chain: `InRelease` / `Release.gpg` → SHA-256 **and** size of the
   `Packages` variant actually fetched → SHA-256 **and** size of every `.deb` taken from
   the signed index, checked **before** `deb::parse_ar`/unpacking.
4. A hard failure model with no "continue at your own risk" path, including a visible
   refusal for mirrors that carry no signatures at all.
5. The minimum negative integration test demanded by issue #32 plus the positive control
   that proves the test is not a tautology.

**Explicit non-goals** (must be named as residual gaps in the docs, not silently implied):

* No signing, no export, no key fetching at boot, no `trusted.gpg.d`-style multiple
  sources, no user-editable trust store. The trust store is a compile-time constant.
* No revocation *distribution* mechanism: a revocation is honoured only if the revocation
  signature is inside the committed keyring block (today: none of the Debian keys carries
  one — **[verified]**), so a key revoked upstream is not learned automatically. Key
  rotation is a new pagh release (§4.5).
* No CRL/OCSP analogue and **no `Valid-Until` on the `stable` suite [verified]**, so replay
  of a complete older triplet (`Release`+`Packages`+`.deb`) is not detected. What *is*
  enforced: `Valid-Until` whenever the suite carries it, and the future-dated checks of
  §6.4.
* No `Acquire-By-Hash` (`by-hash/SHA256/<hex>`) support — the plain path is still served
  by Debian **[verified]**, and it is checked against the same signed entry.
* No v3 signatures, no v5/v6 keys, no partial-length packets, no MD5/SHA-1 signature
  hashes, no RSA-PSS, no `pkg`-command (manual host/path) authentication — see §5.7.
* No change to index memory limits, resolver semantics, `.deb` decompression or the
  ext2 installer.

---

## 1. Deliverables and module layout

| File | Kind | Contents |
|---|---|---|
| `src/pkg/openpgp.rs` | **pure** (`core`+`alloc`, `#[path]`-included by `host-tests`) | packet reader; armor/dearmor + CRC24; clearsign framing + canonicalization; v4 signature packet; the v4 hash rule; RSA/EdDSA/ECDSA verification dispatch; keyring block walk (primary/UID/subkey/self-sig/binding/revocation); expiry+revocation+key-flag policy; SHA-1 (fingerprints only, §1.2) |
| `src/pkg/openpgp_keys.rs` | **generated, committed** | `DEBIAN_KEYRING: &[PinnedKey]` — the pinned Debian subset as byte arrays plus the pinned summary table (fingerprint, algorithm, size, created, expires, UID, role). Byte-for-byte the output of the generator (§4). |
| `src/pkg/openpgp_test_keys.rs` | **generated, committed**, compiled **only** under the harness features (`lx_selftest`, `lx_bigindex`, `lx_bigindex_inram`) | the deterministic *test* anchor used by the local-mirror integration tests (§4.6). Must not be referenced from any other path. |
| `src/pkg/release_file.rs` | **pure**, host-testable | `Release` / `InRelease` cleartext parsing: `Suite`, `Codename`, `Components`, `Architectures`, `Date`, `Valid-Until`, `Acquire-By-Hash`, and the `SHA256:` section → `ReleaseIndex` with exact-path lookup |
| `src/pkg/apt.rs` | kernel | the orchestration of §5, the index publication rule, `AptOpError` variants and messages (§6) |
| `src/pkg/apt_index.rs` | pure | + per-record SHA-256 (§5.4) |
| `tools/gen_debian_keyring.py` | tool | keyring generator + `--check` + `--print-keyring` (§4) |
| `tools/gen_openpgp_testkey.py`, `tools/openpgp_sign.py` | tools | deterministic test key + packet/armor writer for fixtures (§4.6, validated by P56 in §8.1) |
| `tools/mini_repo.py` | tool | signed mini-repo; the negative cases are **suites** (`tampered-index`, `tampered-deb`, `unsigned`, `untrusted`, `stale`) rather than flags — see §15, §16 |
| `tools/e2e.py` | tool | carries the assertions, but as the **existing** `local-mirror` mode: `signed-mirror` was never added, and the per-suite verdicts are printed by `src/selftest_lx.rs::run_apt_verify_checks()` (§15) |

**Purity rule (AGENTS.md "Testing philosophy"):** `openpgp.rs` and `release_file.rs` must
stay `core`(+`alloc`) only, so the properties of §8.1 run on the host against *the same
bytes the kernel compiles*. `host-tests/Cargo.toml` already carries every crate needed
(`sha2`, `rsa`, `p256`, `p384`, `ed25519-dalek`, `signature`, `const-oid`, `rand_core`) —
no new dependency is required for the verifier.

### 1.1 No new crate dependencies

The verifier is built from what is already vendored. `sha2` (SHA-256/384/512), `rsa`
0.9.9 (PKCS#1 v1.5 verify over a pre-hashed digest), `ed25519-dalek` 2.2 (verify), `p256`
0.13 / `p384` 0.13 (ECDSA verify), `signature`, `const-oid`. Adding an OpenPGP crate
(`pgp`, `sequoia-*`) or a `sha1` crate is **not** in scope: it would change the vendored
dependency set, which AGENTS.md pins deliberately. One exception is documented in §1.2.

### 1.2 SHA-1, used for fingerprint *identification* only

OpenPGP v4 fingerprints are `SHA1(0x99 || len16(packet_body) || packet_body)`. No `sha1`
crate is vendored **[verified: `vendor/` contains no `sha1`]** and the kernel needs
fingerprints in two places: (a) verifying that a pinned keyring block really hashes to the
pinned fingerprint constant, and (b) mapping a subkey packet to its fingerprint for the
issuer cross-check. Therefore `pkg::openpgp` implements SHA-1 locally (RFC 3174, ~60
lines) under these constraints:

* It is a **fingerprint derivation**, never a signature/MAC hash. The trust decision never
  rests on it: acceptance requires the *pinned constant* 20-byte fingerprint to equal the
  20-byte issuer fingerprint from the hashed subpacket, and the referenced key material
  comes from the pinned table entry for that exact fingerprint. A wrong SHA-1 can only
  cause a **refusal** (fail-closed), never an acceptance.
* The host property test must pin RFC 3174 vectors (`"abc"` →
  `a9993e364706816aba3e25717850c26c9cd0d89d`; `""` → `da39a3ee5e6b4b0d3255bfef95601890afd80709`;
  the 56-byte and 1000×`'a'` vectors) and recompute every pinned key's fingerprint from the
  committed bytes (§8.1, P55).

---

## 2. OpenPGP objects to parse in `no_std` (task item 1a)

### 2.1 Packet framing

* Old-format headers (RFC 4880 §4.2): CTB `10 tttt ll`, length types 0/1/2 (1-, 2-, 4-byte
  definite). Length type 3 (indeterminate) → **malformed**.
* New-format headers (RFC 9580 §4.2): CTB `11 tttttt`, definite lengths only (one-octet,
  two-octet, five-octet). **Partial body lengths (first-octet 224..254) → refused**
  (`cause=PartialLengthUnsupported`): the pinned keyring blocks are generated by our own
  tool with definite lengths, real Debian `Release.gpg`/`InRelease` signatures are small
  and definite **[verified]**, and refusing keeps the parser bounded (no incremental
  reassembly). This choice is a deliberate, documented narrowing — do not silently skip.
* Only these tags are consumed: **6** public key, **14** public subkey, **13** user ID,
  **2** signature. Everything else (12 trust, 61–63 marker/armor, unknown) is skipped by
  length and counted (§7). Unknown tags are *not* an error.
* Armor is handled before packet parsing (base64 body, not a packet).

### 2.2 Public-key packet (tag 6) and subkey packet (tag 14)

Body: `version(1) created(4) algo(1) algo-specific(…)[key-material]`.

* `version == 4` only; v5/v6 → `cause=UnsupportedVersion`.
* **algo 1 (RSA) / algo 3 (RSA sign-only):** `MPI n, MPI e`. Require
  `2048 ≤ n.bit_length() ≤ 8192`, `e ∈ {3, 17, 65537}`, `n` odd. Debian's keys are RSA-4096,
  `e = 65537` **[verified]**.
* **algo 22 (EdDSA):** `oid_len(1) || oid(oid_len) || MPI(point)`, where `point` is
  `0x40 || 32-byte Ed25519 public key` (the 0x40 prefix must be **stripped** before
  `VerifyingKey::from_bytes`). Two OIDs must be accepted:
  * `2b 06 01 04 01 da 47 0f 01` (1.3.6.1.4.1.11591.15.1) — the **legacy GnuPG** OID, and
    **both** Debian EdDSA keys use exactly this one **[verified]**;
  * `2b 65 70` (1.3.101.112, RFC 8410) — modern form, also accepted.
  `oid_len`/`oid` are not DER; do not try to `der_tlv` them.
  Ed448 (`2b 65 71`) → `cause=UnsupportedCurve`.
* **algo 19 (ECDSA):** `oid_len || oid || MPI(point)` with the uncompressed point
  `0x04 || X || Y`. Accept P-256 (`2a 86 48 ce 3d 03 01 07`), P-384 (`2b 81 04 00 22`),
  P-521 (`2b 81 04 00 23`). Compressed points → `cause=UnsupportedPointFormat`.
  *Note:* no key in `debian-archive-keyring` today uses ECDSA **[verified: 13×algo 1 +
  2×algo 22 only]**; the ECDSA path is required by this contract but can only be covered by
  synthetic fixtures (§8.1) — say so in the docs.
* Any other algo → the key is *parsed but unusable*; a signature that requires it is
  refused if its issuer is pinned (§3.5).
* Key ID / fingerprint: the v4 fingerprint is `SHA1(0x99 || len16(body) || body)` (§1.2).

### 2.3 User ID packet (tag 13)

Raw UTF-8 string; keep it only to (a) cross-check the pinned label of §3.4 and (b) print a
human name in diagnostics. Never used as a trust input.

### 2.4 Signature packet (tag 2), v4 only

Body: `version(1) type(1) pub_algo(1) hash_algo(1) hashed_len(2) hashed_subpackets(…)`
`unhashed_len(2) unhashed_subpackets(…) left16(2) MPIs(…)`.

Hashed subpackets parsed (unknown types skipped by length — the raw bytes are hashed
anyway, so skipping loses nothing):

| id | meaning | use |
|---|---|---|
| 2 | signature creation time | §3.4 validity window, §6.4 future-date check |
| 9 | key expiration time (seconds after `created`) | §3.4 expiry |
| 16 | issuer key ID (8 bytes) | fallback identification |
| 33 | issuer fingerprint (1 version byte + 20 bytes) | **primary** issuer identification |
| 27 | key flags (bit 0x02 = sign) | §3.3 |
| 7 | revocable | informational only |

Signature types consumed: **0x00** binary document (detached path), **0x01** canonical text
document (clearsigned path), **0x13/0x10/0x11/0x12** certifications (UID/self-sig checks),
**0x18** subkey binding, **0x19** primary-key binding, **0x20** key revocation, **0x28**
subkey revocation, **0x30** certification revocation. Other types → ignored.

Issuer identification rule: if subpacket 33 is present, it is authoritative; else fall back
to subpacket 16 compared against the low 8 bytes of the pinned fingerprints (never against
a *computed* key ID as a trust input). Both absent → `cause=NoIssuer`.

### 2.5 Hash algorithms

Required: **8 (SHA-256)** and **10 (SHA-512)**; **9 (SHA-384)** also accepted (free).
Everything else — including 1 (MD5) and 2 (SHA-1) — is **refused** when the signature's
issuer is pinned (`cause=UnsupportedHash`). Debian signs with algo 8 today **[verified]**.

### 2.6 Signature key material (MPIs)

* RSA: one MPI `S`; **left-pad to the modulus byte length** before
  `rsa::pkcs1v15::Signature::try_from`. MPIs strip leading zeros — an unpadded S is the
  single most likely "real signature fails" bug.
* EdDSA: two MPIs `R`, `S`; **left-pad each to 32 bytes** and concatenate to the RFC 8032
  `R||S` form. Real Debian signature: R = 252 bits, S = 256 bits **[verified]** (i.e. both
  need padding).
* ECDSA: two MPIs `r`, `s`; left-pad each to the curve field size (32/48/66) and use
  `Signature::from_scalars(r, s)`. OpenPGP ECDSA is **not** DER-wrapped (RFC 6637); do not
  call `from_der`.

---

## 3. Signature verification (task item 1b)

### 3.1 The v4 hash rule — exact, empirically verified

```
hashed_portion = packet_body[0 .. 6 + hashed_len]        # version..end of hashed subpackets
H = HASH( data || hashed_portion || 0x04 0xFF || u32(6 + hashed_len) )
```

`u32` is big-endian; the length field is the **length of the hashed portion** (including
the version/type/algo/hash/len bytes), *not* `hashed_len` and *not* the packet length.
`H` is then compared with the 2-byte `left16` field (mismatch → `BadSignature`, cheap
pre-filter) and used as the signature input per §3.2.

**`data` per framing:**

* **Detached** (`Release.gpg`, sig type 0x00): exactly the raw bytes of the `Release`
  fetch. No canonicalization. **[verified against live Debians `Release.gpg` for both RSA
  signatures with this exact formula]**
* **Clearsigned** (`InRelease`, sig type 0x01) — canonicalization of the cleartext
  (text between the blank line that ends the armor header and the
  `-----BEGIN PGP SIGNATURE-----` line):
  1. the line ending immediately before `-----BEGIN PGP SIGNATURE-----` is **not** part of
     the signed text (drop exactly one trailing `\n`);
  2. dash-unescape: a line starting with `- ` loses those two bytes (RFC 4880 §7.1);
  3. strip trailing `' '`, `'\t'`, `'\r'` from **every** line;
  4. join lines with `\r\n`, **no** trailing `\r\n`.
  **[verified three ways: the real trixie `InRelease` (which is byte-identical to the
  standalone `Release`), a synthetic clearsign with trailing whitespace (only the
  strip-variant matched), and a synthetic clearsign with a dash-escaped line (only the
  unescape-variant matched)]**
  The armor's `Hash:` header must name the hash algorithm(s) used by the signature packets;
  a signature whose `hash_algo` is not listed in the header → `cause=HashHeaderMismatch`.
  Sig type 0x01 on the detached path, or 0x00 in a clearsigned block, is refused as an
  inconsistent framing.

### 3.2 Per-algorithm signature check

All three verify the same 32/48/64-byte `H` — **not** the raw text:

* **RSA (1, 3):** PKCS#1 v1.5 with the DigestInfo prefix — in the vendored `rsa` 0.9.9 that
  is the *pre-hashed* entry point
  `RsaPublicKey::verify(Pkcs1v15Sign::new::<Sha256>(), &H, &sig)` **[verified signature in
  `vendor/rsa/src/key.rs:161` / `vendor/rsa/src/pkcs1v15.rs:69`]**;
  `pkcs1v15::VerifyingKey::verify` would hash its input again and is wrong here. The
  `Pkcs1v15Sign::new::<D>()` scheme also enforces `H.len() == D::output_size()`.
* **EdDSA (22):** `ed25519_dalek::Signature::from_bytes(&R||S)` then
  `VerifyingKey::verify_strict(&H, &sig)` — **the message is the digest `H`**, which is what
  OpenPGP EdDSA means. **[verified with `openssl pkeyutl -verify -rawin` against the live
  trixie-stable Ed25519 signature: rc=0 over `H`, rc=1 over the raw cleartext]**
  `verify_strict` is compatible with real signatures: interpreted in RFC 8032 order the
  live signature has canonical `S < L`, `R < p` **[verified]**.
* **ECDSA (19):** `p256::ecdsa::VerifyingKey::verify_prehash(&H, &sig)` (resp. `p384`) with
  `signature::hazmat::PrehashVerifier` in scope — the `*_prehash` entry point verifies a
  pre-computed digest **[verified: `vendor/ecdsa/src/verifying.rs:163`]**. Curve must match
  the key's OID (`cause=CurveMismatch` otherwise).
* Length discipline: `H` length must equal the digest size of the signature's hash algo
  (`cause=HashLengthMismatch`), and a RSA/ECDSA signature whose byte length ≠ the key size
  after padding is `MalformedSignature`.

### 3.3 Key usability (key flags, subkey binding)

A signature is usable only if its key is a **signing-capable key of a pinned key block**:

1. If the key packet is a **subkey** (tag 14), the block must contain a **valid 0x18 subkey
   binding signature** made by the primary key that binds `primary_fpr || subkey_fpr`
   (RFC 9580 §5.2.3.27 — the binding hash is
   `HASH(primary_body || subkey_body || hashed_portion || trailer)`, where `*_body` are the
   packet *bodies* from the version byte, **without** CTB/length headers; that
   "keys-over-keys" variant must be implemented explicitly, it is *not* §3.1). No valid
   binding → the subkey is unusable → `cause=NoSubkeyBinding`.
2. If key flags (subpacket 27) are present on the binding/self-signature, bit **0x02
   (sign)** must be set; else `cause=NotASigningKey`. (Debian: primaries `scSC`, signing
   subkeys `s` **[verified]**.)
3. The primary key must carry at least one valid **self-signature** (0x13 over a UID, or a
   direct-key 0x10) — otherwise the block is `cause=UncertifiedKey`.
4. `0x19` primary-key-binding signatures are parsed for structural validity but add no
   trust; an invalid one may reject the subkey.

### 3.4 Validity: creation, expiry, revocation, clock

* **Clock gate first.** `now = crate::arch::x86_64::linux::rtc::now_unix() as i64`; if
  `now < crate::net::tls_chain::CLOCK_FLOOR` (1_735_689_600 = 2025-01-01) the whole OpenPGP
  path is refused with `cause=ClockUnset` — an unset RTC makes every date comparison
  meaningless, exactly as in the TLS verifier. No "skip date checks" path.
* **Expiry** of the *used* key: `expires = created + subpacket9` (absent = never). Refuse if
  `now > expires` (`cause=Expired`) or if `now < created - 86400` (`cause=NotYetValid`).
  The signature's own creation time must lie inside the same window
  (`now - sig_created ≤ 86400` future tolerance, else `cause=FutureSignature`).
  **This is deliberately stricter than `gpgv`**: `gpgv` verifies and exits 0 for a
  signature from an expired key **[verified: local key with 1-day expiry, gpgv rc=0]**.
  Issue #32 and HARDENING item 4 require key *lifecycle* to be enforced, so pagh refuses.
* **Revocation**: a 0x20 signature over the primary (or 0x28 over the subkey) whose issuer
  is that key and whose signature verifies with it → the key is revoked
  (`cause=Revoked`). `0x30` certification revocations are ignored for trust (no revocation
  list is distributed anyway), but a revoked key's block must not be treated as usable.
* **Validity-window arithmetic** uses the same clamped `i64` convention as
  `src/net/x509.rs` (see its `decode_asn1_time` docs) — never wrap or saturate silently.

### 3.5 Selection and acceptance over multiple signatures

Real `stable` InRelease/Release.gpg carries **three** signature packets (two RSA-4096
subkeys + one Ed25519 primary) **[verified]** and real mirrors may carry more or fewer.
The acceptance rule is:

1. Iterate every v4 signature packet (limit §7).
2. Ignore a signature whose issuer fingerprint is **not** in the pinned table — but only
   *after* establishing the issuer fingerprint; such a signature contributes nothing and is
   counted, not fatal (mirrors legitimately sign with keys we did not pin; refusing them
   would make the live mirror unusable the day Debian adds a signer).
3. For a signature whose issuer fingerprint **is** pinned:
   * the body must verify (§3.1–§3.2) **and** the key must be usable (§3.3–§3.4);
   * any failure here is **fatal** — no "another signature was fine" escape. This mirrors
     `gpgv`, which returns 1 when one signature of a multi-signature file is bad
     **[verified]**.
4. Require **at least one** fully verified signature from a pinned key; otherwise
   `cause=NoTrustedSignature` (this includes the "signature by an unknown key only" case,
   where `gpgv` itself refuses — rc=2 for unchecked signatures **[verified]**).
5. Record the verifying fingerprint (and its UID label) for the success line
   `apt: verify OK release=… signer=<fpr>`.

The data hashed is the **same** for every signature: the whole file (detached) or the whole
canonical cleartext (clearsigned). Never re-frame per signature.

### 3.6 Armor

* `-----BEGIN PGP SIGNATURE-----` … `-----END PGP SIGNATURE-----`; headers before the first
  blank line (`Hash:`, `Version:`, `Comment:`); base64 body with `=` padding; optional
  `=`-prefixed **CRC24** trailer.
* Base64 must be strict (alphabet + padding), whitespace ignored, no trailing garbage after
  the END line (`cause=TrailingData`).
* CRC24 (init `0xB704CE`, poly `0x1864CFB`) must be checked when present; mismatch →
  `cause=CrcMismatch` **[verified: a tampered CRC makes `gpgv` fail to parse the clearsigned
  file at all]**.
* Armor is required for both paths: Debian serves **armored** `Release.gpg` **[verified —
  it is base64, not binary]**. A binary (unarmored) signature body may be accepted for the
  detached path as a convenience, but the armored form is the tested one.

---

## 4. The keyring in the repository (task item 2)

### 4.1 Form

`src/pkg/openpgp_keys.rs` (generated, committed, `#![allow(dead_code)]` header comment
naming the source and the generator) exports:

```rust
pub struct PinnedKey {
    pub label: &'static str,        // UID string, verbatim
    pub fingerprint: [u8; 20],      // v4 fingerprint of the PRIMARY key
    pub algo: u8,                   // 1 / 22 / 19
    pub bits: u16,                  // 4096 / 255 / 384…
    pub created: u32,               // unix seconds
    pub expires: Option<u32>,       // created + subpacket 9, None = never
    pub block: &'static [u8],       // bytes: primary key packet + UID(s) + self-sigs + subkeys + bindings (+ revocations)
    pub subkeys: &'static [[u8; 20]],
}
pub static DEBIAN_KEYRING: &[PinnedKey] = &[ … ];
```

* The `block` is the *exact byte range* from the primary key packet up to (not including)
  the next primary key packet in the source keyring — including the UID, its self-signature,
  every subkey and its binding signature, and any revocation signature that upstream ships.
  Nothing is re-encoded, so a reviewer can pipe the block to `gpg --list-packets`.
* The full keyring subset is emitted in `SELECTION` order, hex-formatted like
  `src/net/ca_bundle.rs::CA_BUNDLE`, and `rustfmt`-canonicalized by the generator — a plain
  regeneration must be **byte-identical** to the committed file.
* The generator additionally supports `--print-keyring <path>`, writing the same subset as a
  binary OpenPGP keyring so it can be inspected with the reference implementation
  (`gpg --list-packets`, `gpgv --keyring <path> Release.gpg Release`). This is a review aid;
  the committed artifact stays the Rust module (the `ca_bundle.rs` precedent: no runtime file
  I/O, no path resolution, no way to swap the trust store at boot).

### 4.2 Generator and pinning — `tools/gen_debian_keyring.py`

Follows `tools/gen_ca_bundle.py` exactly in spirit: *source is not trusted on its own*.

* **Source (pinned twice):** `https://deb.debian.org/debian/pool/main/d/debian-archive-keyring/debian-archive-keyring_2025.1_all.deb`,
  pinned by the .deb's own sha256
  `9ea7778e443144ca490668737a8ab22dd3e748bb99e805e22ec055abeb3c7fac` **[verified during this
  task]**; the script fails closed if the download does not match, so a *new* upstream
  release cannot silently rewrite our trust store — bumping the version is an explicit,
  reviewed commit.
* Extract `usr/share/keyrings/debian-archive-keyring.gpg` (the full archive keyring, which
  contains every key we pin).
* **Selection is BY FINGERPRINT** (computed from the packet bytes), with a cross-check of
  everything the pin does not already cover; any deviation is `SystemExit` (exit != 0):
  expected UID string, algorithm, key size, creation time, expiry, and — for a subkey — the
  existence of a valid binding signature by the pinned primary.
* A key whose fingerprint is present but whose UID/algo/size/expiry differs is a **PIN
  MISMATCH** error, never a silent pick (identical to the CN cross-check in
  `gen_ca_bundle.py`).

`SELECTION` — the initial pinned set (the keys that sign today's `stable`; every value
**[verified]**, see §12.6):

| # | label (UID) | role | fingerprint | algo | created | expires |
|---|---|---|---|---|---|---|
| 1 | `Debian Stable Release Key (13/trixie) <debian-release@lists.debian.org>` | primary (signs directly) | `41587F7DB8C774BCCF131416762F67A0B2C39DE4` | EdDSA/Ed25519 | 2025-03-24 | 2033-03-22 |
| 2 | `Debian Archive Automatic Signing Key (13/trixie) <ftpmaster@debian.org>` | primary + signing subkey `B8E5F13176D2A7A75220028078DBA3BC47EF2265` | `04B54C3CDCA79751B16BC6B5225629DF75B188BD` | RSA-4096 | 2025-03-30 | 2035-03-28 |
| 3 | `Debian Archive Automatic Signing Key (12/bookworm) <ftpmaster@debian.org>` | primary + signing subkey `4CB50190207B4758A3F73A796ED0E7B82643E131` | `B8B80B5B623EAB6AD8775C45B7C5D7D6350947F8` | RSA-4096 | 2023-01-21 | 2031-01-19 |

Key 3 signs `stable` because Debian cross-signs the transition; keep it and document why
(removing it makes the live mirror fail the day Debian drops key 1's signature).
**Documented opt-in, not pinned by default** (uncomment in `SELECTION` only when a
`*-security` suite is actually configured):
`Debian Security Archive Automatic Signing Key (13/trixie)` primary
`5E04A1E3223A19A20706E20F9904613D4CCE68C6` + subkey `89C87ACEA5DD6B8E6A7068808E9F831205B4BA95`.

Note the EdDSA keys use the **legacy** OID (§2.2) — the generator must record it and the
parser must accept both forms.

### 4.3 When the keyring is regenerated, and by whom

* Only by a human/agent running `python tools/gen_debian_keyring.py` **deliberately**, then
  reviewing the diff and committing the regenerated `src/pkg/openpgp_keys.rs` (plus the
  `--print-keyring` output).
* **Never** at build time, never at boot, never from the network by the kernel. The kernel
  contains no code that fetches keys.
* Triggers (documented in `SECURITY.md` under the trust-root policy that HARDENING item 4
  asks for): a Debian archive-key rotation (new fingerprint), an expiring pinned key, or a
  new `debian-archive-keyring` release. The *mechanism* that surfaces the trigger is the
  runtime refusal `apt: verify FAIL stage=key cause=Untrusted|Expired` naming the
  fingerprint — the operator then regenerates and ships a new pagh.
* Ship the generator's pinned .deb version + sha256 in the generated module's doc comment so
  the provenance of the committed bytes is self-describing.

### 4.4 Properties/tests for the keyring (the `gen_ca_bundle` → P48 analogue)

Host property **P55** ("pinned keyring matches the committed bytes") must assert, for every
`PinnedKey`:

1. `sha1_fingerprint(block[0..primary_packet_len]) == fingerprint`;
2. the block parses cleanly with the pure parser, contains exactly one primary key, ≥ 1 UID,
   ≥ 1 self-signature, and one subkey per `subkeys` entry;
3. each `subkeys[i]` equals the computed fingerprint of the i-th subkey packet;
4. `algo`, `bits`, `created`, `expires` equal the values parsed out of the block;
5. `label` equals the UID packet string;
6. the fingerprint set contains no key that is not in `SELECTION` (no accidental widening);
7. a byte-flipped copy of a block must **fail** the check (the property is not vacuous).

`tools/gen_debian_keyring.py --check` (re-download, regenerate, diff, exit 1 on any
difference) exists for manual/CI-optional use; it is **not** part of the four mandatory gates
because CI has no network.

### 4.5 What the runtime does with the pinned table

* Match the signature's issuer fingerprint against the table (byte equality, §3.5).
* Re-verify the block's own fingerprint with the local SHA-1 (§1.2) before using its keys.
* Use the *pinned* constants for expiry checks (§3.4), never a value recomputed from a
  potentially-mutated block — the block is a constant, but the rule keeps the trust decision
  independent of parsing.

### 4.6 The test anchor

The local-mirror integration tests need a key the kernel trusts. Design:

* `tools/gen_openpgp_testkey.py` derives a **deterministic Ed25519 keypair** from a
  documented seed (RFC 8032 is deterministic), writes
  `src/pkg/openpgp_test_keys.rs` (`TEST_KEYRING`, cfg-gated) with the same `PinnedKey` shape,
  and prints the seed/public key for the fixture tools.
* `tools/openpgp_sign.py` writes OpenPGP packets/armor from scratch (public key packet, UID,
  self-signature, signature packets) and signs per §3.1–§3.2 (Ed25519 over `H`), so
  `tools/mini_repo.py` can emit a signed mini-repo with no `gpg` on the host and no committed
  secret key.
* Validation of that writer is **required**, but never by the kernel alone:
  * host test P56 verifies the produced fixture signature with the vendored `ed25519-dalek`;
  * when `gpgv` is present, the fixture builder may additionally cross-check with
    `gpgv --keyring <test pubkey> InRelease` (advisory; not a gate);
  * `tools/mini_repo.py --break untrusted` uses a **second** test key that is *not* in
    `TEST_KEYRING`, so the "untrusted signer" negative case has a real fixture.
* Hand-rolled Python crypto is acceptable **here only** because it produces test fixtures
  and is validated by reviewed code (Rust `ed25519-dalek`) and by the reference
  implementation. The kernel-side "no ad-hoc cryptography" rule is untouched.
* Under the default build `TEST_KEYRING` does not exist; `apt` must then have no path that
  could ever trust a non-Debian key.

---

## 5. The trust chain (task item 3)

### 5.1 Update flow (order is normative)

```
1. clock gate (§3.4)                          -> ClockUnset
2. GET {base}/dists/{suite}/InRelease
     |- 200            -> clearsigned path (5.2)
     |- 404            -> GET {base}/dists/{suite}/Release.gpg  (must be 200)
     |                    GET {base}/dists/{suite}/Release      (must be 200)
     |                    detached path (5.2)
     |- other error    -> propagate (Download/Status)
     |- neither present-> Unsigned (5.5)
3. verify the signature(s) against DEBIAN_KEYRING [ + TEST_KEYRING under lx_selftest ]
4. parse the VERIFIED Release: Suite/Codename/Date/Valid-Until/SHA256 section
5. choose the index variant: prefer Packages.gz, then Packages.xz, then Packages —
   but ONLY a variant that has an entry in the signed SHA256 section;
   none listed -> NoIndexEntry
6. GET it, check SHA-256 **and** Size against the signed entry
     (mismatch => hard refusal, no fallback to another variant)
7. decompress + stream-parse (existing pipeline, unchanged limits)
8. publish INDEX atomically (5.6)
9. `apt install`: per record -> Filename + SHA256 + Size from the SAME signed record ->
   GET -> check SHA-256 **and** Size -> only then parse_ar/decompress/tar/install (5.4)
```

### 5.2 Both signed forms are equally acceptable

Debian serves `InRelease` (clearsign, sig type 0x01) and `Release.gpg` (armored detached,
sig type 0x00) with the same three signatures **[verified]**, and the InRelease cleartext is
byte-identical to the standalone `Release` **[verified]**. Rules:

* Prefer `InRelease`; fall back to `Release.gpg`+`Release` **only** on a 404 of `InRelease`.
* A **verification failure** on `InRelease` is fatal — do not fall back (that would let a
  MITM force the weaker path). Same for `Release.gpg`: a failure is fatal.
* `Release` without `Release.gpg` (or vice versa) → `Unsigned` (never "unsigned Release is
  metadata, use it anyway").
* If both are present, `InRelease` is authoritative; verifying both is optional and must not
  turn a good InRelease into a failure.

### 5.3 Release → `Packages`: SHA-256 **and** size

* The signed `Release` carries three digest sections (`MD5Sum:`, `SHA256:`, `SHA512:`-style
  blocks are possible). **Only `SHA256:` is parsed**; a 64-hex digest is required, and the
  32-hex `MD5Sum` lines must not be mistaken for SHA-256 (a parser that scans for
  `hex + size + path` across the whole file accepts MD5 — refuse that design).
* Entry format (verified): `" " + 64 hex + "  " + size + " " + path`, e.g.
  ` 42c23aac…f40a85 13332733 main/binary-amd64/Packages.gz`.
* Lookup key: the exact pool-relative path under `dists/{suite}/`, i.e.
  `{component}/binary-{arch}/Packages{,.gz,.xz}`. The fetched URL is
  `{base}/dists/{suite}/{that path}`.
* Both the digest **and** the size must match. Size mismatch is a refusal, not a warning.
* `Packages` and its compressed variants are all listed **[verified]**, so whichever variant
  the kernel prefers can be checked.
* A variant that is absent from the signed section must be skipped *before* fetching.

### 5.4 `Packages` → `.deb`: SHA-256 **and** size, before parsing

* `src/pkg/apt_index.rs` gains per-record `sha256: Option<[u8; 32]>` parsed from the stanza's
  `SHA256:` field, alongside the existing `Size:` (already parsed). It must live in both the
  recording path and the arena-backed `PkgRecordC`/`StanzeView` path so the streaming parser
  and `show`/`list` agree.
* In `apt::install`, per planned package: take `filename`, `sha256`, `size` from the **same**
  record that produced the plan entry (`index.get(pkg).or_else(get_provider)` — the current
  code already keeps them together; keep it that way);
  * empty `Filename` → refuse (today an ad-hoc `Download` error; make it a named cause);
  * missing/unparsable `SHA256` → refuse to install **that package** (`DigestUnavailable`);
    `apt show`/`list` may still display the record, but it must be visible in the output
    that the package cannot be verified (implementer's choice of wording; no silent install);
  * after `cfg.fetch(&url)` and **before** `deb::parse_ar` → compare SHA-256 and size;
    mismatch → `DigestMismatch`/`SizeMismatch` and **no unpacking, no file written**.
* Nothing else may read the fetched bytes first: the check must be the first use of
  `bytes`, before `parse_ar`, `locate_members`, `decompression_of`, `decompress_data`,
  `read_tar`, `install_data_tar`.
* The `.deb` request URL is derived from the signed `Filename`, so "a `.deb` that is not in
  the index" cannot be fetched by name. The three missing-from-index cases are therefore:
  (a) requested *name* unknown → `NotFound` (existing behaviour, keep);
  (b) record without `Filename` → `IndexMissingFilename`;
  (c) record without `SHA256` → `DigestUnavailable`.
  A `.deb` fetched through the low-level `pkg` command is **not** covered (§5.7).

### 5.5 Mirrors without signatures — visible refusal, never silent trust

* No `InRelease`, no `Release.gpg` → `AptOpError::Unsigned { url }` and the serial line
  `apt: verify FAIL stage=metadata cause=Unsigned url=<…>`.
* The shell must print a single explicit line naming the mirror and the fact that pagh
  refuses unauthenticated package metadata by design (pointing at `SECURITY.md`), e.g.
  `apt: update failed: <host> serves no signed metadata (no InRelease/Release.gpg) — refusing unauthenticated package metadata (see SECURITY.md)`.
* There is **no** flag, config key, environment variable or build feature that turns
  verification off for the `network_packages` build. The only documented way to run without
  a trust anchor is `cargo build --no-default-features`, where
  `update()`/`install()` return `NetworkDisabled` (unchanged).
* The development/e2e path keeps working by serving a *signed* mini-repo with the test
  anchor (§8.3) — the escape hatch is a real signature, not a bypass.

### 5.6 Index publication

* `INDEX` is replaced only after the Release signature, the `Packages` digest/size and the
  parse have all succeeded (`Ok(count)`).
* On **any** verification failure the INDEX is **cleared** (not left holding the previous
  boot's or previous run's index). Rationale: the index is RAM-only and rebuilt by
  `apt update` every boot, so clearing costs nothing while "keep the last verified index
  after a failed update" would let an attacker freeze the mirror view. Record this deviation
  from apt's "keep old lists" behaviour in the subsystem README.
* `install()` requires a published, verified index; with none it returns `NoIndex` (existing).

### 5.7 The manual `pkg` command

`pkg <host> <path> [port]` downloads and installs a `.deb` with **no** index, hence no
digest to check — it stays unauthenticated by construction. The contract requires:
its usage text and the README/SECURITY text to say so explicitly ("raw, unauthenticated
download; `apt` is the verified path"), so #32's closure does not read as "everything is
verified". No verification is to be bolted onto `pkg` in this scope.

---

## 6. Failure model (task item 4)

### 6.1 Principle

Every verification failure is **terminal for the operation**: no fallback transport, no
fallback file variant, no "use the unverified bytes anyway", no interactive override, no
warning-then-continue. Failures are single-line, greppable, and name the stage and cause.

### 6.2 Error variants (`src/pkg/apt.rs`)

```rust
pub enum AptOpError {
    // existing: NetworkDisabled, NoNetwork, NoIndex, NotFound, Download, Parse, IndexTooLarge, Install
    Unsigned { url: String },                              // no signed metadata on the mirror
    BadSignature { stage: &'static str, cause: &'static str }, // armor/clearsign/signature/key failure
    ClockUnset,                                            // RTC below 2025-01-01
    ReleaseExpired { until: i64 },                         // now > Valid-Until
    ReleaseFuture { field: &'static str },                 // Date/sig time far in the future
    NoIndexEntry { path: String },                         // Packages variant not in the signed Release
    IndexMismatch { path: String, cause: &'static str },   // SHA-256 or Size of Packages
    DigestUnavailable { pkg: String },                     // record has no usable SHA256
    DigestMismatch { pkg: String, cause: &'static str },   // SHA-256 or Size of a .deb
}
```

`message()` for each must be a single shell-friendly line naming the stage/cause; none may
suggest retrying with verification off. No `Debug`-only dumping of raw bytes: report
expected/actual digests truncated to a readable length.

### 6.3 Required serial diagnostics

Exactly one line per failure, greppable by the e2e harness:

```
apt: verify FAIL stage=<armor|clearsign|signature|key|clock|metadata|release|index|deb> cause=<Cause> [key=<fpr>] [path=<…>] [pkg=<name>]
apt: verify OK release=<InRelease|Release.gpg> signer=<primary-fpr> key=<used-fpr> suites=<suite>
```

and for a successful `.deb` check (debug level, one per package):
`apt: verify deb pkg=<name> sha256=<hex-prefix> ok`.

The existing markers the harnesses depend on must keep working unchanged:
`apt: Get <scheme>://<host><path> […]`, `apt: fetched … body - decompressing...`,
`apt: index ready - N packages`, `apt: [n/total] Unpacked <pkg> (N files, size)` and
`LXSELFTEST apt_e2e PASS`. New scenario markers are additive (§8.3).

### 6.4 Concrete decisions

| Condition | Result | Cause string |
|---|---|---|
| `InRelease` 404 and `Release.gpg` 404 | refuse update | `metadata/Unsigned` |
| `InRelease` present but bad armor/CRC/base64 | refuse update | `armor/CrcMismatch` etc. |
| clearsign framing malformed (no blank line, no BEGIN/END, text after END) | refuse update | `clearsign/Malformed` |
| no signature packet at all in the blob | refuse update | `signature/NoSignature` |
| all signatures by keys outside the pinned table | refuse update | `signature/NoTrustedSignature` |
| one pinned-key signature bad among good ones | refuse update | `signature/BadSignature` |
| pinned key expired / revoked / not yet valid | refuse update | `key/Expired` `key/Revoked` `key/NotYetValid` |
| signature creation time > now + 86400 | refuse update | `signature/FutureSignature` |
| RTC < 2025-01-01 | refuse update | `clock/ClockUnset` |
| `Valid-Until` present and passed | refuse update | `release/ValidUntilExpired` |
| `Date` > now + 86400 | refuse update | `release/FutureDate` |
| no index variant listed in the signed Release | refuse update | `index/NoIndexEntry` |
| `Packages` SHA-256 or Size ≠ signed entry | refuse update, **no** variant fallback | `index/HashMismatch` `index/SizeMismatch` |
| `.deb` SHA-256 or Size ≠ signed index | refuse that package, **before** any parse/unpack | `deb/HashMismatch` `deb/SizeMismatch` |
| record has no `SHA256` | refuse that package | `deb/DigestUnavailable` |
| requested name not in the index | `NotFound` (unchanged) | — |

Not enforced / documented as residual: replay of a complete old triplet (no `Valid-Until` on
`stable`), upstream revocation discovery, ECDSA coverage by a real key, `by-hash` fetch.

---

## 7. Bounds (bounded memory, no OOM abort)

| Object | Limit | Note |
|---|---|---|
| `InRelease` / `Release` body | 2 MiB | live `stable` InRelease = 140 421 B **[verified]** |
| signature packets per file | 16 | live file has 3 **[verified]** |
| armor size | 64 KiB | live `Release.gpg` = 1 760 B **[verified]** |
| armor line length | 80 | base64 64 + slack; longer → malformed |
| pinned keyring total | 64 KiB | subset of the 170 KiB upstream keyring |
| packets per pinned block | 64 | upstream block has ≤ 8 |
| RSA modulus | 2048…8192 bits | Debian: 4096 |
| MPI length | ≤ modulus/field size + 2 bytes | reject longer before allocation |
| subpacket area | ≤ 8 KiB per signature | |
| hashed subpackets | ≤ 64 entries | |

All limits produce named refusals, never panics or truncation. The parser must be
`no_std`-clean with no recursion (the ~10 MiB kernel stack in host tests does not apply on
the host; the *kernel* stack is small — iterate, do not recurse).

---

## 8. Tests (task item 5)

### 8.1 Host properties (`host-tests/src/properties/`, P50+; `#[path]`-included pure code)

Numbering continues from P49. Minimum set:

* **P50 armor/dearmor** — round-trip, strict base64, CRC24 accept/reject, trailing-data
  reject, oversized input reject; malformed inputs return errors (never panic).
* **P51 packet framing** — old/new headers, all definite length forms, bounds checks,
  partial/indeterminate refused, truncated packets refused, fuzz-style random buffers.
* **P52 v4 hash rule** — a fixture table with the *live* Debian vectors of §12.4 (recovered
  digest == computed `H`) plus a synthetic "trailer length = 6+hashed_len" vector; a wrong
  trailer length or a missing hashed portion must not verify.
* **P53 clearsign canonicalization** — the three live/synthetic vectors of §12.3: exact
  Release bytes (CRLF, no final EOL), trailing-whitespace stripping, dash-unescape;
  a mutated body must fail.
* **P54 signature verification** — for each algorithm, host-generated keys (the same crates
  already in `host-tests`): RSA-2048/4096 PKCS#1 v1.5 SHA-256/512, Ed25519 (sign with
  `ed25519-dalek` over `H`), ECDSA P-256/P-384; positive and negative (one flipped bit,
  padded-vs-unpadded MPIs, wrong curve, wrong hash algo, signature/protocol mismatch).
* **P55 pinned keyring integrity** — §4.4 items 1–7, including the "byte-flip must fail"
  control and the RFC 3174 SHA-1 vectors.
* **P56 fixture self-consistency** — `tools/openpgp_sign.py` output verifies with the
  vendored Rust verifier, and the generated `TEST_KEYRING` block matches the test key.
* **P57 `Release` parsing** — `SHA256:` section only (an `MD5Sum:`-looking line must not be
  accepted as SHA-256), size parsing, path lookup hit/miss, `Valid-Until` parse, duplicate
  entries, CRLF-tolerant line handling, `Acquire-By-Hash` ignored.
* **P58 index digests** — `apt_index` records carry the SHA-256 from the stanza; a record
  with a malformed/absent SHA256 yields `None` (never a zero digest that would "match" a
  fake); `Size` parsed; streaming and non-streaming paths agree.

Every existing test must stay green; `abi.rs`'s `supported_set_is_exact` is untouched by
this work.

### 8.2 Evidence style

The implementing task must record, for each property and each e2e scenario, the exact
command and its exit code (the repo's convention: gates in AGENTS.md must be green, and a
regression fix without a test is not done).

### 8.3 In-QEMU negative integration test (the issue's item 5)

Harness: `tools/mini_repo.py` (signed mini-repo) + `tools/e2e.py` (QEMU + serial + exit
code) + the `lx_selftest` kernel harness. New scenario `signed-mirror` runs five cases in
one boot (or five boots; implementer's choice, but all five must be asserted by the driver):

| # | Mirror state | Required kernel behaviour | Evidence |
|---|---|---|---|
| 1 | signed, correct (positive control) | `apt update` + `apt install hello-pagh` succeed, the ELF runs | `LXSELFTEST apt_sig control PASS`, existing `apt: [1/1] Unpacked hello-pagh`, `hello from apt` |
| 2 | `Packages.gz` does not match the signed `Release` | update refused, index NOT published | `apt: verify FAIL stage=index cause=HashMismatch`, `LXSELFTEST apt_sig tampered_index PASS` |
| 3 | `.deb` does not match the signed `Packages` | install refused **before unpacking**; `/mnt/usr/bin/hello-pagh` must not exist afterwards | `apt: verify FAIL stage=deb cause=HashMismatch`, `LXSELFTEST apt_sig tampered_deb PASS` |
| 4 | no `InRelease`, no `Release.gpg` | update refused, explicit user-visible message | `apt: verify FAIL stage=metadata cause=Unsigned`, `LXSELFTEST apt_sig unsigned PASS` |
| 5 | signed by a key not in the anchor set | update refused | `apt: verify FAIL stage=signature cause=NoTrustedSignature`, `LXSELFTEST apt_sig untrusted PASS` |

Requirements on the test:

* Cases 2 and 3 are the **minimum demanded by #32** and must be present verbatim in intent
  (one metadata mismatch, one payload mismatch, both refused).
* Case 3 must additionally assert the *absence* of installed files (proving "before
  unpacking"), e.g. the driver greps for the missing path in the guest via a shell command or
  the harness asserts `run_linux_binary` fails.
* Case 1 is mandatory: without it, "everything is refused" would also pass.
* The pre-existing `local-mirror` scenario (`LXSELFTEST apt_e2e PASS`, `hello from apt`) must
  stay green against the *signed* mini-repo — that is the regression guard for the change of
  the mini-repo format.
* `tools/e2e.py` must return non-zero if any of the five is missing, and record the summary
  JSON (verdict + markers + ELF sha256 + HEAD) as it does for the other modes.
* Optional (opt-in, network): the live `stable` mirror (the official HTTPS default mirror)
  verifies with the pinned keyring (`lx_livetest`), proving the stack against real Debian
  data. It must not be a CI gate (network + slow under TCG). With the OpenPGP chain in place
  a plain-HTTP mirror is integrity-protected too — the transport is not what carries the
  guarantee any more — and `NETWORK-APT.md` must say exactly that instead of the current
  "plain HTTP is unauthenticated by construction" as the last word on the subject.

### 8.4 What the implementing task must NOT do

Rename existing markers (`apt: Get`, `apt: [n/m] Unpacked`, `LXSELFTEST apt_e2e*`), change
the resolver, change decompression limits, disable the four AGENTS.md gates, or add a
"verify off" escape hatch.

---

## 9. Documentation and gates

* `SECURITY.md` — rewrite "What is still not verified" (metadata signatures and digests are
  now verified; name the residual gaps from §0 verbatim), tick release requirements 3 and 4,
  and add the trust-root policy (pinned keys, deliberate regeneration, no auto-update, what
  an expired/unknown signer looks like).
* `HARDENING.md` — tick items 2, 3 and 4 with the same honest residuals; item 5 stays
  partially open only for the TLS-MITM server gap (this contract adds the apt negative tests,
  not the rustls-refusing one).
* `src/pkg/README.md` — the trust chain, the new modules, error variants, index publication
  rule, the test anchor and the "no override" rule.
* `NETWORK-APT.md` — the "Trust status" paragraph (a plain-HTTP mirror is now
  integrity-protected by the OpenPGP chain even though the transport is unauthenticated; the
  mirror's metadata must still be signed and pinned).
* Root `README.md` limitation list + `src/shell/commands.rs` usage text: replace
  "repository metadata signatures and package digests are not verified" with the verified
  state and name the pinned-key constraint (a mirror outside the pinned set is refused).
* Version: this is a user-visible behaviour change → **MINOR bump** in the same PR (from the
  then-current version, `2.4.2` at the time of writing → `2.5.0`), including the
  `Cargo.lock` `pagh` entry (AGENTS.md). If a parallel task already bumped the minor, take
  the next free minor with the feature that lands last.
* Gates: `cargo build`, `python tools/build.py build --release`, `cargo fmt --all -- --check`,
  `python tools/check_safety.py`, `python tools/host_tests.py` — all green, no new warnings
  beyond the pre-existing 42 (38 kernel + 4 host).

---

## 10. Definition of done

An implementation satisfies this contract only if **all** of the following hold:

1. `src/pkg/openpgp.rs` + `release_file.rs` implement §1–§3 and §7 and are `#[path]`-included
   by `host-tests`; the verifier does not depend on `std`, allocation beyond `alloc`, or any
   new vendored crate.
2. `src/pkg/openpgp_keys.rs` is generated by `tools/gen_debian_keyring.py`, committed, and
   passes P55 as a byte-derived cross-check of §4.1's table (pins as in §4.2, `SELECTION`
   order, deterministic regeneration).
3. The update path implements §5.1 exactly, including "verify before publish" (§5.6) and
   "no variant fallback after a digest failure".
4. `apt install` verifies SHA-256 **and** size of every `.deb` before `deb::parse_ar`.
5. §6 holds: every refusal is terminal, greppable, and no override exists; the unsigned
   mirror refusal is visible to the user.
6. §8.1 properties pass, §8.3's five cases pass in QEMU under `tools/e2e.py`, and the
   pre-existing e2e markers are unchanged and green.
7. §9's documentation, version bump and gates are done.
8. Issue #32's own minimum is satisfied by name: a local mirror whose `Packages` does not
   match the signed `Release` and a `.deb` that does not match `Packages` are both refused.

---

## 11. Work breakdown suggestion (non-binding)

1. `pkg::openpgp` pure parser + armor/clearsign + hash rule + verify dispatch (P50–P54).
2. `pkg::openpgp_keys` + generator + P55 (and `--print-keyring`).
3. `pkg::release_file` + P57 and `apt_index` SHA-256 + P58.
4. `apt::update` trust chain + `AptOpError` + diagnostics (§5, §6) + P55–P57 wiring.
5. `apt::install` digest check (§5.4).
6. Fixture tools + signed mini-repo + `tools/e2e.py signed-mirror` (§8.3) + P56.
7. Docs, version bump, gates (§9).

Each step lands with its tests; 6 depends on 1–3 (the fixtures must be verifiable by the
pure module), and 4–5 depend on 2–3.

---

## 12. Empirical evidence appendix

Environment: `gpg`/`gpgv` 2.4.9 (libgcrypt 1.12.2), OpenSSL 3.5.8, python 3.14.7,
2026-09-17, fetched from `https://deb.debian.org/debian/dists/stable/`.

### 12.1 Real artifacts

| Object | Size | sha256 |
|---|---|---|
| `InRelease` | 140 421 B | `0584fba3…` |
| `Release` | 138 612 B | `ed56aac47e7911e65ee63aae8d67e29f20e023f840e49f8ed037043a33de138a` |
| `Release.gpg` | 1 760 B (armored) | `9f80d5c8…` |
| `debian-archive-keyring_2025.1_all.deb` | 179 244 B | `9ea7778e443144ca490668737a8ab22dd3e748bb99e805e22ec055abeb3c7fac` |

* `InRelease` is clearsigned; its cleartext body (armor header + blank line stripped, final
  line ending removed) is **byte-identical** to `Release`.
* `Release.gpg` is an **armored detached** signature (base64), not binary.
* Both carry exactly **3** signature packets: RSA-4096 `keyid 6ED0E7B82643E131` (issuer fpr
  `4CB50190207B4758A3F73A796ED0E7B82643E131`, a **subkey**), RSA-4096
  `keyid 78DBA3BC47EF2265` (issuer fpr `B8E5F13176D2A7A75220028078DBA3BC47EF2265`, a
  **subkey**), EdDSA `keyid 762F67A0B2C39DE4` (issuer fpr
  `41587F7DB8C774BCCF131416762F67A0B2C39DE4`, the primary release key). All signatures are
  v4, sigclass 0x01 (InRelease) / 0x00 (Release.gpg), hash algo 8 (SHA-256), with hashed
  subpackets {2: creation time, 33: issuer fingerprint} and unhashed {16: issuer key id}.
* `Release` fields: `Suite: stable`, `Version: 13.7`, `Codename: trixie`,
  `Date: Sat, 12 Sep 2026 07:55:41 UTC`, `Acquire-By-Hash: yes`,
  `Architectures: all amd64 …`, `Components: main contrib non-free-firmware non-free`;
  **no `Valid-Until`**. `SHA256:` section lines look like
  ` 42c23aaca08e8225ed0449360724b92531952cbe4ce3bbf189298a7108f40a85 13332733 main/binary-amd64/Packages.gz`.

### 12.2 Signature verification semantics (`gpgv`)

| Case | Result |
|---|---|
| 3 signatures, all good, full keyring | rc=0 |
| 3 signatures, the third byte-tampered, full keyring | rc=**1** (fatal even though 2 are good) |
| keyring contains only the Ed25519 key (2 signatures unchecked: "public key not found") | rc=**2** (not accepted) |
| signature by an **expired** key, verified after expiry | rc=**0** (gpgv accepts!) — pagh deliberately refuses (§3.4) |

### 12.3 Clearsign canonicalization (determined empirically)

Three independent tests, each comparing the digest recovered *from the signature* (RSA
public-key math: `S^e mod n` → PKCS#1 DigestInfo) with candidate canonicalizations:

1. Live `InRelease`: matched exactly `CRLF` + per-line trailing-whitespace strip + **no**
   final EOL (`CRLF-strip-noFinal`). `LF` variants and "final EOL included" variants did not
   match.
2. Synthetic clearsign of `"line one   \nline two\t\nline three\n"` (gpg `--clearsign`,
   `--digest-algo SHA256`): only the **strip** variant matched — so gpg's text filter strips
   trailing whitespace.
3. Synthetic clearsign of a text containing a line starting with `- `: only the
   **dash-unescaped** variant matched.

### 12.4 The v4 hash rule (determined empirically)

`H = SHA256(data || hashed_portion || 0x04 0xFF || u32(6 + hashed_len))` reproduced the
digest inside **both** RSA signatures of the live `Release.gpg` over the raw `Release`
bytes, and both RSA signatures of the live `InRelease` over the canonical cleartext.
The length field is `6 + hashed_len`, not `hashed_len`, not the packet length (a longer
brute-force over trailer lengths/variants matched only this composition).

> Pitfall: `gpg --list-packets`'s "begin of digest" **does** print the digest's leading
> 16 bits for RSA signatures, but for a locally Ed25519-signed file it prints the signature
> R instead — do not use it as an oracle for EdDSA.

### 12.5 OpenPGP EdDSA signs the digest, not the text

Public key `41587F7D…` (Ed25519, legacy OID `2b06010401da470f01`, point `0x40 || 32 bytes`),
signature `R||S` (R = 252 bits, S = 256 bits, each left-padded to 32 bytes):

* `openssl pkeyutl -verify -pubin -inkey ed.pem -rawin -in H.bin -sigfile ed.sig` → **rc=0**,
  where `H` is the 32-byte v4 digest above.
* The same command over the raw canonical cleartext (140 064 B) → **rc=1**.
* Strictness: with RFC 8032 little-endian interpretation `S < L` and `R < p` — so
  `ed25519_dalek::verify_strict` is compatible with live Debian signatures.

### 12.6 Pinned Debian keys (from `debian-archive-keyring_2025.1`)

| UID | role | fingerprint | algo | created → expires |
|---|---|---|---|---|
| `Debian Stable Release Key (13/trixie) <debian-release@lists.debian.org>` | primary | `41587F7DB8C774BCCF131416762F67A0B2C39DE4` | 22 Ed25519 (legacy OID) | 2025-03-24T18:56:21Z → 2033-03-22T18:56:21Z |
| `Debian Archive Automatic Signing Key (13/trixie) <ftpmaster@debian.org>` | subkey `B8E5F13176D2A7A75220028078DBA3BC47EF2265` | primary `04B54C3CDCA79751B16BC6B5225629DF75B188BD` | 1 RSA-4096 | 2025-03-30T12:50:29Z → 2035-03-28T12:50:29Z |
| `Debian Archive Automatic Signing Key (12/bookworm) <ftpmaster@debian.org>` | subkey `4CB50190207B4758A3F73A796ED0E7B82643E131` | primary `B8B80B5B623EAB6AD8775C45B7C5D7D6350947F8` | 1 RSA-4096 | 2023-01-21T11:44:21Z → 2031-01-19T11:44:21Z |
| `Debian Security Archive Automatic Signing Key (13/trixie) <ftpmaster@debian.org>` (opt-in) | subkey `89C87ACEA5DD6B8E6A7068808E9F831205B4BA95` | primary `5E04A1E3223A19A20706E20F9904613D4CCE68C6` | 1 RSA-4096 | 2025-03-30T12:51:41Z → 2035-03-28T12:51:41Z |

Key-algorithm census of the upstream keyring: 13 × algo 1 (RSA) + 2 × algo 22 (EdDSA); **no
ECDSA and no revocations** in any shipped keyring (`debian-archive-keyring.pgp`,
`debian-archive-removed-keys.pgp`, per-suite files) — see the coverage notes in §2.2/§0.

### 12.7 Reference commands used

```sh
# signature inventory / oracle
gpg --list-packets Release.gpg
gpgv --keyring debian-archive-keyring.gpg --status-fd 1 Release.gpg Release
gpgv --keyring debian-archive-trixie-stable.gpg Release.gpg Release   # rc=2: unchecked sigs

# fixture-side digest recovery (validates the §3.1 rule independently of gpg)
#   m = pow(S, e, n); PKCS#1 strip -> DigestInfo -> digest == H(candidate)

# clearsign canonicalization fixtures
printf 'line one   \nline two\t\nline three\n' > ws.txt
gpg --batch --faked-system-time 20260101T000000 -a --clearsign --digest-algo SHA256 -o ws.asc ws.txt

# EdDSA message semantics
openssl pkeyutl -verify -pubin -inkey ed.pem -rawin -in msg.bin -sigfile ed.sig
```

---

## 13. Trap checklist for the implementer

1. **The digest is not the file hash**: `H = HASH(data || hashed_portion || 0x04 0xFF ||
   u32(6+hashed_len))`. Getting the trailer wrong makes every real signature fail.
2. **MPIs strip leading zeros** — left-pad RSA `S` to the modulus size, EdDSA `R`/`S` to 32
   bytes, ECDSA `r`/`s` to the field size, before handing them to the crypto crates.
3. **EdDSA/Ed25519 signs `H`**, not the text (`verify_strict(&H, &sig)`).
4. **RSA must use the pre-hashed PKCS#1 v1.5 entry point**; the `VerifyingKey::verify`
   overload hashes its input again.
5. **Clearsign**: dash-unescape + strip trailing whitespace per line + CRLF + drop the final
   line ending. Detached: raw bytes. Do not unify the two paths.
6. **Armor, not binary**: `Release.gpg` is base64. Check CRC24 when present.
7. **Both Ed25519 OIDs**, and Debian uses the *legacy* one.
8. **Subkey signatures**: the signature is made by a *subkey* (two of the three live
   signatures!) whose fingerprint is the issuer fingerprint; the primary must be verified
   through the binding signature (a different hash composition: primary‖subkey).
9. **Never re-frame per signature** and never accept "one good among a bad pinned-key
   signature" — that is `gpgv`'s rc=1 case.
10. **`gpgv` accepts expired keys**; we must not (that is a deliberate strengthening, and it
    has to be documented as such).
11. **Duplicate key/UID/subkey blocks** in a keyring: `debian-archive-keyring.gpg` contains
    historical keys — select by fingerprint, and refuse a *modified* block rather than
    merging attributes from different blocks.
12. **`MD5Sum:` vs `SHA256:`** sections in `Release`: parse only `SHA256:`.
13. **Index publication order**: verify → then publish; clear on failure (§5.6).
14. **mini_repo.py must become signed** or the existing `apt_e2e`/local-mirror scenario breaks
    by design — plan that change in the same task as the verification.
15. Keep the clock gate and the `Valid-Until`/future-date checks tied to
    `CLOCK_FLOOR`/`rtc::now_unix()`, exactly as the TLS verifier does; do not invent a
    second clock policy.

---

## 14. Amendment after t21 (implementation findings)

`t21` implemented §2/§3 and verified them against **real** Debian archive keys; three
details of the original text turned out to be wrong and are corrected here. Every claim
below is proven by recovering the signed digest from an actual RSA signature
(`pow(S, e, n)` → PKCS#1 DigestInfo) and matching it against the candidate preimage, and is
covered by host property P53 (`host-tests/src/properties/p53.rs`,
`real_gnupg_key_signatures_verify`).

1. **Key signatures hash the key in the `0x99 || be16(len) || body` form.** §3.3 said
   `primary_body || subkey_body || …`, i.e. the bare packet bodies. GnuPG's
   `do_hash_public_key()` hashes a public key packet *with the same prefix the v4
   fingerprint uses* (`0x99`, two-octet body length). The correct preimages are:
   * direct key signature (0x1F) / key revocation (0x20):
     `0x99||len16||primary || hashed_portion || 0x04 0xFF || be32(6+hashed_len)`;
   * subkey binding (0x18) / subkey revocation (0x28):
     `0x99||len16||primary || 0x99||len16||subkey || hashed_portion || trailer`;
   * User ID certification (0x10–0x13):
     `0x99||len16||primary || 0xB4 || be32(uid_len) || uid || hashed_portion || trailer`.
   With the bare-body form, *every* self-signature and binding of the committed Debian keys
   fails (P53 red); with the `0x99` form all of them verify.

2. **Signature-type constants.** §2.4 listed 0x10 as the "direct key signature". Per
   RFC 4880 §5.2.1 the types are: 0x10–0x13 User ID certifications (0x13 = positive),
   0x18 subkey binding, 0x19 primary-key binding, **0x1F signature directly on a key**,
   **0x20 key revocation**, **0x28 subkey revocation**, 0x30 certification revocation. The
   Debian archive keys carry five 0x1F direct-key signatures each, so handling 0x1F is
   required for `certified()`; the revocation codes were already correct.

3. **Property numbering.** The contract's §8.1 planned P50–P58; issue #33 took P50 first, so
   this series is **P51 (armor/packet framing), P52 (signature policy), P53 (pinned Debian
   keyring + real-GnuPG interop)**, with the remaining numbers free for the trust-chain and
   e2e properties of the follow-up tasks.

Also confirmed while implementing: `Clearsigned::text` (the armor's clear text, byte-identical
to the standalone `Release`) keeps its final line ending; only `canonical_text()` drops it for
the signature computation. The apt step that parses the `SHA256:` section therefore gets the
same bytes as a detached-style fetch.

---

## 15. Amendment after t23 (negative integration test)

The E2E half of §8.3 is implemented and passing in QEMU; two fixture-construction
facts are worth recording because they are easy to get wrong on the *producing*
side (they cost a debugging cycle here, and both are now checked by the tools
themselves).

1. **A clear-signed fixture must be signed over the canonical text, not the raw
   document.** `tools/openpgp_sign.py::canonicalize()` mirrors the verifier's rule
   (dash-unescape, strip trailing whitespace per line, CRLF joins, no final line
   ending). The first attempt signed the raw `Release` bytes for `InRelease`,
   which the kernel and `gpgv` both refuse; the detached `Release.gpg` path is the
   raw-bytes one, so the two differ on purpose.

2. **The tamper cases are length-preserving.** `tampered-index` serves a
   `Packages` body of exactly the signed size with a different SHA-256 (and the
   signed `InRelease` verifies), and `tampered-deb` serves a `.deb` of exactly the
   indexed size with a different digest. That makes the *hash* the only check that
   can catch them — a size check would not — which is precisely what issue #32
   asks to be proven. (Both suites' signatures are confirmed valid by `gpgv`
   before the kernel ever sees them.)

Also implemented, as §4.6 described: the deterministic test anchor
(`tools/gen_openpgp_testkey.py` → `src/pkg/openpgp_test_keys.rs`) is compiled only
under `lx_selftest` / `lx_bigindex`, and `pkg::apt::trusted_keyring()` selects it
only there, so no other build can trust the test key. `DEBIAN_KEYRING` gained
per-key `const`s (`DEBIAN_KEY_0`…) purely so the selftest anchor table can list the
Debian keys and the test key in one static slice; the production table is
unchanged in content.

In-guest markers (asserted by `python3 tools/e2e.py local-mirror`, whose
`LXSELFTEST apt_e2e PASS` is gated on all of them):

```
LXSELFTEST apt_verify signed-suite PASS (2 packages)
LXSELFTEST apt_verify tampered-index PASS   # verify OK, then stage=index cause=HashMismatch
LXSELFTEST apt_verify tampered-deb   PASS   # stage=deb cause=HashMismatch, nothing written
LXSELFTEST apt_verify unsigned-mirror PASS  # stage=metadata cause=Unsigned
LXSELFTEST apt_verify untrusted-key  PASS   # stage=signature cause=NoTrustedSignature
LXSELFTEST apt_e2e PASS (… trust-chain checks passed)
```

### Coverage map after t23 (what is proven where)

| Refusal / property | Where it is proven |
|---|---|
| correctly signed mirror loads and installs | QEMU E2E (`stable` suite; also the live Debian mirror in the t22 run) |
| `Packages` ≠ signed `Release`, **signature valid**, same size | QEMU E2E (`tampered-index`) |
| `.deb` ≠ signed index, same size, nothing written to `/mnt` | QEMU E2E (`tampered-deb`) |
| mirror with no signatures | QEMU E2E (`unsigned`) |
| signature by an unpinned key | QEMU E2E (`untrusted`) |
| refused update leaves no index published | QEMU E2E (asserted per refusal) |
| subkey issuer → pinned primary mapping | QEMU E2E (the test key signs with its primary; the *live* t22 run exercised a Debian subkey) |
| rollback to a complete older signed triplet | QEMU E2E `stale` — **accepted**, printed as a NOTE; the gap is documented in `SECURITY.md` |
| `ClockUnset`, key expiry/revocation/not-yet-valid, future-dated signature, `Date`/`Valid-Until` parsing, `NoIndexEntry`, ECDSA | host properties P52–P54 only — end-to-end they would need a guest clock override (`-rtc base=2020-01-01`, which the E2E driver does not expose) or a Debian private key |

For the two key-lifecycle branches the E2E *tool* is ready and validated without
being wired in: `tools/gen_openpgp_testkey.py` emits an expired and a
not-yet-valid deterministic test key, P53 proves the verifier refuses each as
`Expired` / `NotYetValid` from the generator's own metadata, and neither key
appears in the harness anchor table. Connecting them to the E2E run is a small,
deliberate follow-up, kept out of the series so the verification surface stays
bounded.

---

## 16. Amendment after the external fixture cross-check

1. **Final property map.** §8.1 planned P50–P58 and §14 fixed P51–P53 for the
   verifier/keyring series; the trust-chain and `Release` work then took the next
   numbers. The implemented set is:

   | Property | File | Covers |
   |---|---|---|
   | P51 | `host-tests/src/properties/p51.rs` | armor/dearmor, CRC24, packet framing |
   | P52 | `p52.rs` | verification policy: accept, tamper, canonicalization, key validity, clock gate, signature times, ECDSA |
   | P53 | `p53.rs` | pinned Debian keyring: pins vs committed bytes, real-GnuPG interop, expiry/revocation, anchor separation |
   | P54 | `p54.rs` | `Release` parsing: `SHA256:` section only, path lookup (including a miss), `Date` / `Valid-Until` |
   | P55 | `p55.rs` | index records carry the stanza's SHA-256 |

   Forward references inside §4 and §8.1 that still say "P55"/"P56" describe the
   pre-t21 plan; the table above is what the tests actually carry.

2. **Two branches that the coverage map claimed but no test reached are now
   tested**, both noticed while replaying an *independent* fixture manifest that
   attributed them to P52:
   * `signature/FutureSignature` — both directions (a signature dated after the
     clock, and a signature predating the key that made it).
     `P52::signature_time_is_sanity_checked_against_the_clock_and_the_key`
     rewrites the creation-time subpacket of the committed fixture signature and
     asserts the pair `("signature", "FutureSignature")`. The time gates run
     *before* the digest check, so a `BadSignature` result would mean the check
     moved to the wrong layer — that is what the assertion pins.
   * OpenPGP ECDSA — the positive path had no fixture at all (no Debian key uses
     ECDSA and `tools/openpgp_sign.py` signs Ed25519/RSA only).
     `P52::ecdsa_curves_verify_the_digest_through_the_shared_backends` drives
     `verify_ecdsa_p256` / `verify_ecdsa_p384` from host-generated keys and checks
     that the digest is bound.

3. **Malformed metadata is diagnosed by the layer that detects it.** Harness
   markers must use these pairs:

   | Input | Diagnostic |
   |---|---|
   | `InRelease` without its `-----BEGIN PGP SIGNATURE-----` line, or with a header line lacking `:` | `stage=clearsign cause=Malformed` |
   | `InRelease` without its `Hash:` header | `stage=clearsign cause=HashHeaderMismatch` |
   | armor block with a missing `-----END PGP SIGNATURE-----`, a bad CRC24 or non-base64 body | `stage=armor cause=MalformedArmor` / `CrcMismatch` / `BadBase64` |

   The armor layer is parsed before the packets inside it, so a *truncated*
   signature block is an armor fault, not a clearsign-framing fault; the
   clear-signed framing check owns the message header, the armor headers and the
   presence of the signature header at a line boundary.

