# Security policy

pagh is an educational kernel, not a production operating system.

## Safe defaults

The development build enables outbound package downloads through the default `network_packages` Cargo feature, which activates the network transport; `cargo build --no-default-features` produces a fail-closed build with no outbound package transport.

**TLS peer authentication is implemented (issue #14, PRs #22–#29 of the series):** an HTTPS fetch validates the server certificate chain down to the committed CA bundle (`src/net/ca_bundle.rs`, roots pinned by sha256 at generation time), authorizes the connection host against the leaf's SAN entries (RFC 6125, no CommonName fallback), enforces the validity windows, refuses to validate at all when the system clock is unset (below 2025-01-01), and checks the handshake `CertificateVerify` signature against the verified leaf key. A failed check aborts the handshake — there is no fallback that keeps the connection, and the verifier's `error!` diagnostics (`Package_Fetcher(tls): stage=verify cause=…`) name the refused check.

**A handshake that skips the certificate is refused too.** RFC 8446 §4.4.2.4 requires a client to reject a server `Finished` that was not preceded by `Certificate` and `CertificateVerify`. Upstream `embedded-tls` did not enforce that, so a peer could omit both messages, compute a valid `Finished` (an active MITM that terminates the key exchange itself can), and reach application data with the verifier never invoked — a fail-open path that no amount of verifier logic could close. This tree therefore carries a local patch to `vendor/embedded-tls/src/connection.rs` that enforces the ordering (PSK handshakes, which legitimately send no certificate, are exempt), and `src/net/tls.rs` independently requires its authentication counter to advance across the handshake before a single application byte moves. Both are deliberate: the patch is the protocol-correct fix, the counter keeps the guarantee if the vendored crate is ever refreshed.

### Trust is limited to the pinned roots

The bundle trusts exactly four self-signed roots: **ISRG Root X1**, **ISRG Root X2**, **GTS Root R1**, and **GTS Root R4** (each selected by a sha256 pin in `tools/gen_ca_bundle.py`). Consequences:

* An HTTPS mirror whose chain does not reach one of those four roots is **refused** (`ChainError::NoAnchor`), even if the certificate is otherwise perfectly valid and issued by a widely trusted CA. Adding a root means deliberately re-running the generator with a reviewed pin and committing the regenerated `src/net/ca_bundle.rs`.
* Plain-HTTP mirrors (`apt setmirror http://…`) are **unauthenticated at the transport layer**: cleartext, no certificate to check. What carries that path instead is the OpenPGP metadata chain below — the signature over `InRelease`/`Release.gpg`, the SHA-256 of `Packages`, and the SHA-256 of every `.deb` — so a cleartext mirror can no longer substitute metadata or payloads, although it can still observe and deny service.
* Certificate **revocation is not checked** (no CRL, no OCSP, no stapling): a certificate that its issuer has revoked but that is still inside its validity window is accepted.

### Repository metadata is verified against pinned Debian keys

`apt update` refuses to use anything the signature chain does not cover (issue #32):

* `dists/<suite>/InRelease` is verified as a clear-signed OpenPGP message, or — only when it is absent — `Release.gpg` against `Release`, against the keyring committed in `src/pkg/openpgp_keys.rs`: the Ed25519 *Debian Stable Release Key (13/trixie)* and the RSA-4096 archive keys *13/trixie* and *12/bookworm*, each pinned by its v4 fingerprint, its signing subkeys bound by verified key binding signatures, with expiry, revocation and key flags enforced and the 2025 clock gate applied.
* The `SHA256:` entry of the verified `Release` is the authority for the `Packages` variant that follows: the downloaded body must match both digest **and** size before it is decompressed or parsed.
* Every downloaded `.deb` must match the `SHA256:`/`Size:` of the stanza that named its `Filename:` — checked after the download and **before** the payload is parsed, decompressed or written.
* A mirror that serves no signatures is **refused with a visible error**, and there is no flag, feature or environment variable that turns verification off. The only unverified build is `cargo build --no-default-features`, where `apt update`/`install` return `NetworkDisabled`.

Deliberate strengthening over `gpgv`: a signature made by an **expired** key is refused (`gpgv` verifies it and exits 0), and partial-length packets are refused rather than reassembled.

### What is still not verified

* **Revocation distribution:** a revocation is honoured only if the revocation signature is inside the committed keyring block. There is no CRL/OCSP-equivalent refresh, so a key revoked upstream is learned only by shipping a regenerated keyring.
* **Replay of a complete older triplet** (`Release` + `Packages` + `.deb`) is **not detected** on suites without `Valid-Until` — which includes `stable`; only suites that carry `Valid-Until` (e.g. `security`) are covered by that check, plus the future-dated-signature checks. The E2E harness *demonstrates* this rather than leaving it implied: the local mirror's `stale` suite is a complete, correctly signed older revision (older `Date`, older package version), `apt update` accepts it, and the run prints `LXSELFTEST apt_verify stale-rollback NOTE (accepted: 1.0 -> 0.9 …)`. Closing it needs either `Valid-Until` on every suite, persistent "highest release seen" state (none exists: the index is RAM-only), or an authenticated freshness channel — none of which is in scope here.
* **Which refusal paths are proven where.** The QEMU E2E harness proves, against the real `apt` pipeline: a correctly signed mirror loads and installs (positive control), and four refusals — a `Packages` body that does not match the signed `Release` **while its signature verifies**, a `.deb` that does not match the signed index (nothing written to `/mnt`), a mirror with no signatures, and a mirror signed by an unpinned key; each refusal also leaves no index published. Refusals that need a guest clock or a Debian signing key are covered by **host properties only, not end-to-end**, and are not claimed as E2E: `ClockUnset` (P52), key expiry/revocation/not-yet-valid (P52/P53), future-dated signatures and `Date`/`Valid-Until` parsing (P52/P54), `NoIndexEntry` (P54), and the ECDSA path (P52, synthetic keys — no Debian archive key uses ECDSA today).
* **TLS revocation** is still unchecked (no CRL/OCSP/stapling) and trust is limited to the four pinned roots.
* **`pkg <host> <path>`** (the manual, index-free `.deb` downloader) remains unauthenticated by construction; `apt` is the verified path.
* **`Acquire-By-Hash`** is not used: pagh fetches the plain index path, which is checked against the same signed entry.
* The ECDSA verification path exists but no real Debian key exercises it yet (all current archive keys are RSA or Ed25519), so it is covered only by synthetic host tests.

## Reporting

Report memory-safety, privilege-boundary, filesystem-integrity, parser, or package-supply-chain issues privately to the repository maintainers. Include a minimal reproducer, build profile, QEMU version, and serial log.

## Release requirements

A security-capable package manager requires all of the following before the demo feature can become a default:

1. ~~CSPRNG-backed TLS randomness.~~ **Done** — RDSEED/RDRAND, fail-closed when unavailable.
2. ~~Certificate-chain, hostname, and expiry validation.~~ **Done, with two named gaps** — `net::tls_auth` / `net::tls_chain` / `net::ca_bundle`, driven by the handshake verifier in `net::tls`, fail-closed, and the "no certificate at all" bypass is closed in `vendor/embedded-tls` + the `VERIFIED_HANDSHAKES` gate. Not covered: **revocation** (no CRL/OCSP/stapling) and **root agility** (trust is the four pinned roots above; any other issuer is refused). Both are documented in "Safe defaults" rather than implied by "Done".
3. ~~Signed repository metadata verification.~~ **Done** — the OpenPGP chain above, pinned by fingerprint. Not covered: revocation distribution and replay of a whole old triplet (both named in "What is still not verified").
4. ~~Digest verification of `Packages` and every `.deb` before parsing or installation.~~ **Done** — digest **and size**, checked before decompression/parsing and before any unpacking.
5. Negative integration tests for MITM, corrupted metadata, digest mismatch, path traversal, malformed archives, and malformed ELF files. **Partly done.** The apt side is now proven in-QEMU by one reproducible command (`python3 tools/e2e.py local-mirror`, whose `LXSELFTEST apt_e2e PASS` is gated on it): a correctly signed local mirror loads and installs, while a **valid signature over a `Packages` file that was replaced afterwards** (equal size, different SHA-256), a **tampered `.deb`** (equal size, different SHA-256, nothing written), a mirror with **no signatures at all**, and a mirror signed by an **unpinned key** are each refused with a named diagnostic (`apt: verify FAIL stage=… cause=…`). The signed test mirror uses a deterministic test key whose anchor is compiled only into the `lx_selftest`/`lx_bigindex` harness builds (`src/pkg/openpgp_test_keys.rs`); a production build cannot trust it. Still missing from this item: the certificate-omission MITM regression test (needs a TLS server that deliberately skips `Certificate`, which rustls will not do) and the path-traversal/malformed-archive cases, which remain covered by host properties and code review.
