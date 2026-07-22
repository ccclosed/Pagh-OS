# Hardening implementation

This branch implements the safe foundation of the four-stage review plan.

## Implemented

- Reproducible pinned Rust toolchain.
- Native Linux/Bash `build.sh`, `run.sh`, and `setup-linux.sh` scripts.
- Cross-platform Python build/link/stage/run driver and Makefile.
- GitHub Actions for formatting, debug/release kernel builds, host property tests, artifacts, and security policy checks.
- Network package installation is feature-gated: the development build enables it through the default `network_packages` feature (which activates the historical `insecure_network_demo` transport and prints an explicit trust warning); `cargo build --no-default-features` remains fail-closed.
- Hardware-backed, fail-closed entropy API using RDSEED/RDRAND.
- Linux `getrandom` returns `EAGAIN` when secure entropy is unavailable.
- TLS demo refuses to start without hardware entropy.
- User-pointer validation now requires `USER_ACCESSIBLE` at every page-table level, not merely a present mapping.
- A narrow page-flag inspection API and a CI check for undocumented unsafe in trust-boundary modules.
- Security policy and explicit production-release requirements.

- Embedded offline mini-Rust with `cargo`, `rustc`, `rust`, and `rustup` commands.
- Static Rust/musl userspace build helper and sample application.
- `nano+` full-screen editor with journal-safe truncation, undo/redo, search and goto.

## Deliberately not represented as complete

Certificate validation and signed Debian repository verification are not implemented. The development build enables the network transport by default for the hobby/QEMU workflow, but apt prints a prominent trust warning before any network operation, and `cargo build --no-default-features` produces a fail-closed build with no outbound package transport.

Before enabling networking by default, implement and test:

1. CA-chain, hostname, validity-period, and signature verification in TLS.
2. Trusted-key verification of Debian `InRelease`/`Release.gpg`.
3. SHA-256 binding from trusted Release metadata to `Packages`, and from `Packages` to every `.deb`.
4. Revocation/update policy for trust roots.
5. MITM and corrupted-artifact integration tests.

These items require choosing and reviewing a no-std X.509/OpenPGP trust stack; they must not be approximated with ad-hoc cryptography.
