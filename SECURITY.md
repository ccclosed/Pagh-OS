# Security policy

pagh is an educational kernel, not a production operating system.

## Safe defaults

The development build enables outbound package downloads through the default `network_packages` Cargo feature, which activates the historical `insecure_network_demo` HTTP/TLS transport; `cargo build --no-default-features` produces a fail-closed build with no outbound package transport. The transport does **not** authenticate TLS peers or Debian repository metadata and must only be used with an isolated QEMU network or a mirror you trust.

Do not use `insecure_network_demo` on an untrusted network and do not execute packages obtained through it.

## Reporting

Report memory-safety, privilege-boundary, filesystem-integrity, parser, or package-supply-chain issues privately to the repository maintainers. Include a minimal reproducer, build profile, QEMU version, and serial log.

## Release requirements

A security-capable package manager requires all of the following before the demo feature can become a default:

1. CSPRNG-backed TLS randomness.
2. Certificate-chain, hostname, and expiry validation.
3. Signed repository metadata verification.
4. Digest verification of `Packages` and every `.deb` before parsing or installation.
5. Negative integration tests for MITM, corrupted metadata, digest mismatch, path traversal, malformed archives, and malformed ELF files.
