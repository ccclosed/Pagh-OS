# Security policy

pagh is an educational kernel, not a production operating system.

## Safe defaults

The development build enables outbound package downloads through the default `network_packages` Cargo feature, which activates the network transport; `cargo build --no-default-features` produces a fail-closed build with no outbound package transport.

**TLS peer authentication is implemented (issue #14, PRs #22–#29 of the series):** an HTTPS fetch validates the server certificate chain down to the committed CA bundle (`src/net/ca_bundle.rs`, roots pinned by sha256 at generation time), authorizes the connection host against the leaf's SAN entries (RFC 6125, no CommonName fallback), enforces the validity windows, refuses to validate at all when the system clock is unset (below 2025-01-01), and checks the handshake `CertificateVerify` signature against the verified leaf key. A failed check aborts the handshake — there is no fallback that keeps the connection, and the verifier's `error!` diagnostics (`Package_Fetcher(tls): stage=verify cause=…`) name the refused check.

**A handshake that skips the certificate is refused too.** RFC 8446 §4.4.2.4 requires a client to reject a server `Finished` that was not preceded by `Certificate` and `CertificateVerify`. Upstream `embedded-tls` did not enforce that, so a peer could omit both messages, compute a valid `Finished` (an active MITM that terminates the key exchange itself can), and reach application data with the verifier never invoked — a fail-open path that no amount of verifier logic could close. This tree therefore carries a local patch to `vendor/embedded-tls/src/connection.rs` that enforces the ordering (PSK handshakes, which legitimately send no certificate, are exempt), and `src/net/tls.rs` independently requires its authentication counter to advance across the handshake before a single application byte moves. Both are deliberate: the patch is the protocol-correct fix, the counter keeps the guarantee if the vendored crate is ever refreshed.

### Trust is limited to the pinned roots

The bundle trusts exactly four self-signed roots: **ISRG Root X1**, **ISRG Root X2**, **GTS Root R1**, and **GTS Root R4** (each selected by a sha256 pin in `tools/gen_ca_bundle.py`). Consequences:

* An HTTPS mirror whose chain does not reach one of those four roots is **refused** (`ChainError::NoAnchor`), even if the certificate is otherwise perfectly valid and issued by a widely trusted CA. Adding a root means deliberately re-running the generator with a reviewed pin and committing the regenerated `src/net/ca_bundle.rs`.
* Plain-HTTP mirrors (`apt setmirror http://…`) are **unauthenticated by construction**: cleartext, no certificate to check. Metadata/digest verification below is what would have to carry such a path, and it is not implemented. It is no longer the live default: `deb.debian.org` is reached over HTTPS, large indexes included, and the `lx_livetest` harness refuses to run on a cleartext config (issue #19).
* Certificate **revocation is not checked** (no CRL, no OCSP, no stapling): a certificate that its issuer has revoked but that is still inside its validity window is accepted.

### What is still not verified

Debian repository metadata signatures (no OpenPGP path) and complete per-package digest verification. Treat package *contents* from an untrusted mirror as unauthenticated and do not execute them.

## Reporting

Report memory-safety, privilege-boundary, filesystem-integrity, parser, or package-supply-chain issues privately to the repository maintainers. Include a minimal reproducer, build profile, QEMU version, and serial log.

## Release requirements

A security-capable package manager requires all of the following before the demo feature can become a default:

1. ~~CSPRNG-backed TLS randomness.~~ **Done** — RDSEED/RDRAND, fail-closed when unavailable.
2. ~~Certificate-chain, hostname, and expiry validation.~~ **Done, with two named gaps** — `net::tls_auth` / `net::tls_chain` / `net::ca_bundle`, driven by the handshake verifier in `net::tls`, fail-closed, and the "no certificate at all" bypass is closed in `vendor/embedded-tls` + the `VERIFIED_HANDSHAKES` gate. Not covered: **revocation** (no CRL/OCSP/stapling) and **root agility** (trust is the four pinned roots above; any other issuer is refused). Both are documented in "Safe defaults" rather than implied by "Done".
3. Signed repository metadata verification.
4. Digest verification of `Packages` and every `.deb` before parsing or installation.
5. Negative integration tests for MITM, corrupted metadata, digest mismatch, path traversal, malformed archives, and malformed ELF files. **Not yet done.** In particular the certificate-omission MITM now rejected by the `vendor/embedded-tls` patch has no automated regression test: it needs a TLS server that deliberately skips `Certificate` (rustls will not), so it is currently covered by code review plus the kernel-side `VERIFIED_HANDSHAKES` gate rather than by a test.
