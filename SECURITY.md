# Security policy

pagh is an educational kernel, not a production operating system.

## Safe defaults

The development build enables outbound package downloads through the default `network_packages` Cargo feature, which activates the network transport; `cargo build --no-default-features` produces a fail-closed build with no outbound package transport.

**TLS peer authentication is implemented (issue #14, PRs #22–#28 of the series):** an HTTPS fetch validates the server certificate chain down to the committed CA bundle (`src/net/ca_bundle.rs`, roots pinned by sha256 at generation time), authorizes the connection host against the leaf's SAN entries (RFC 6125, no CommonName fallback), enforces the validity windows, refuses to validate at all when the system clock is unset (below 2025-01-01), and checks the handshake `CertificateVerify` signature against the verified leaf key. A failed check aborts the handshake.

What is still **not** verified: Debian repository metadata signatures (no OpenPGP path) and complete per-package digest verification. Treat package *contents* from an untrusted mirror as unauthenticated and do not execute them.

## Reporting

Report memory-safety, privilege-boundary, filesystem-integrity, parser, or package-supply-chain issues privately to the repository maintainers. Include a minimal reproducer, build profile, QEMU version, and serial log.

## Release requirements

A security-capable package manager requires all of the following before the demo feature can become a default:

1. ~~CSPRNG-backed TLS randomness.~~ **Done** — RDSEED/RDRAND, fail-closed when unavailable.
2. ~~Certificate-chain, hostname, and expiry validation.~~ **Done** — `net::tls_auth` / `net::tls_chain` / `net::ca_bundle`, driven by the handshake verifier in `net::tls`.
3. Signed repository metadata verification.
4. Digest verification of `Packages` and every `.deb` before parsing or installation.
5. Negative integration tests for MITM, corrupted metadata, digest mismatch, path traversal, malformed archives, and malformed ELF files.
