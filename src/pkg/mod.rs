//! Debian package handling (design components 8–10).
//!
//! This subsystem turns a downloaded `.deb` byte buffer into files installed on
//! the ext2 filesystem. It is split along the R11.6 pure-function boundary:
//!
//!   * [`deb`] — pure `ar` container enumeration, `.deb` member location, and
//!     compression-suffix classification (design component 8, R9). The effectful
//!     decompression shell is added to the same module by a later task.
//!
//! Later tasks add `tar` (the ustar reader/writer, component 9) and `install`
//! (the ext2 installer + path normalization, component 10) as sibling modules.
//!
//! Everything in [`deb`] is `core` + `alloc` only — no hardware, no globals — so
//! the `host-tests` crate `#[path]`-includes the same source and property-tests
//! it on the host (P23).

/// Pure `Packages` index parser + lookup index (the read side of `apt install`).
/// `core` + `alloc` only — `#[path]`-included by `host-tests` and exercised by P30.
pub mod apt_index;
/// Pure dependency resolver layered over [`apt_index`] (the planning side of
/// `apt install`). `core` + `alloc` only; references [`apt_index`] via `super::`
/// so one source resolves in both the kernel and the host crate. Tested by P30.
pub mod apt_resolve;
/// Effectful `apt` package-manager front end: by-name `update`/`install`/`show`/
/// `list`/`setmirror` over the pure index ([`apt_index`]), resolver
/// ([`apt_resolve`]), `.deb` parser ([`deb`]), tar reader ([`tar`]), and ext2
/// installer ([`install_fs`]). Kernel-only (drives networking + VFS), so it lives
/// apart from the pure, host-includable modules.
pub mod apt;
pub mod deb;
pub mod install;
/// Pure `apt setmirror` host-argument parsing (URL-scheme prefix handling).
/// `core`-only and self-contained — `#[path]`-included by `host-tests`.
pub mod mirror;
/// Effectful ext2 installer (`Package_Installer`, component 10). Kernel-only: it
/// drives the `VfsNode` trait, so — like `net::http_fetch` beside the pure
/// `net::http` — it lives apart from the pure, host-includable `install` module.
pub mod install_fs;
pub mod tar;
