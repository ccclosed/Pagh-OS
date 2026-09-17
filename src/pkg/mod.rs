//! Debian package handling (design components 8–10).
//!
//! This subsystem turns a downloaded `.deb` byte buffer into files installed on
//! the ext2 filesystem. It is split along the R11.6 pure-function boundary:
//!
//!   * [`deb`] — pure `ar` container enumeration, `.deb` member location,
//!     compression-suffix classification (design component 8, R9) and the
//!     effectful decompression shell (whole-buffer and streaming).
//!
//! Sibling modules [`tar`] (the ustar reader/writer, component 9), [`install`]
//! (the pure path-normalization/install model) and [`install_fs`] (the effectful
//! ext2 installer, component 10) are already in place.
//!
//! Everything in [`deb`] is `core` + `alloc` only — no hardware, no globals — so
//! the `host-tests` crate `#[path]`-includes the same source and property-tests
//! it on the host (P23).

/// Effectful `apt` package-manager front end: by-name `update`/`install`/`show`/
/// `list`/`setmirror` over the pure index ([`apt_index`]), resolver
/// ([`apt_resolve`]), `.deb` parser ([`deb`]), tar reader ([`tar`]), and ext2
/// installer ([`install_fs`]). Kernel-only (drives networking + VFS), so it lives
/// apart from the pure, host-includable modules.
pub mod apt;
/// Pure `Packages` index parser + lookup index (the read side of `apt install`).
/// `core` + `alloc` only — `#[path]`-included by `host-tests` and exercised by P30.
pub mod apt_index;
/// Pure dependency resolver layered over [`apt_index`] (the planning side of
/// `apt install`). `core` + `alloc` only; references [`apt_index`] via `super::`
/// so one source resolves in both the kernel and the host crate. Tested by P30.
pub mod apt_resolve;
pub mod deb;
pub mod install;
/// Effectful ext2 installer (`Package_Installer`, component 10). Kernel-only: it
/// drives the `VfsNode` trait, so — like `net::http_fetch` beside the pure
/// `net::http` — it lives apart from the pure, host-includable `install` module.
pub mod install_fs;
/// Pure `apt setmirror` host-argument parsing (URL-scheme prefix handling).
/// `core`-only and self-contained — `#[path]`-included by `host-tests`.
pub mod mirror;
pub mod tar;

/// Pure OpenPGP verification policy for repository metadata (issue #32):
/// trusted-key selection, subkey binding, expiry/revocation/key-flag handling and
/// the `Release.gpg`/`InRelease` entry points. `core` + `alloc` only —
/// `#[path]`-included by `host-tests` (P51–P53).
pub mod openpgp;
/// Pure OpenPGP cryptography used by [`openpgp`]: the v4 signature digest rule,
/// RSA PKCS#1 v1.5 / EdDSA / ECDSA verification over that digest, and the local
/// SHA-1 used for v4 fingerprints only (no `sha1` crate is vendored).
/// `core` + `alloc` only — `#[path]`-included by `host-tests`.
pub mod openpgp_crypto;
/// Pure OpenPGP packet layer: ASCII armor (CRC24), packet framing, public keys,
/// signature packets, keyring blocks and the `InRelease` clearsign framing.
/// `core` + `alloc` only, panic-free and bounded — `#[path]`-included by
/// `host-tests`.
pub mod openpgp_packet;

/// The GENERATED Debian keyring (`tools/gen_debian_keyring.py`): the trusted
/// keys as exact keyring byte ranges, each pinned by its v4 fingerprint,
/// algorithm, size, validity window and subkey fingerprints. Committed and
/// reviewed deliberately; the kernel never fetches or updates keys at runtime.
/// The apt trust chain consumes this table in the next PR of the series.
pub mod openpgp_keys;
