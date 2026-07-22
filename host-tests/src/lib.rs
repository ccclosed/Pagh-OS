//! Host-side property-test harness for the `pagh` kernel.
//!
//! This crate hosts `proptest`-based property tests that run on the HOST (see the
//! crate-level `Cargo.toml` for why this lives outside the kernel workspace). The
//! kernel itself is `#![no_std]` and built for a bare-metal target, so its property
//! tests live here against pure logic extracted from the bug fixes.
//!
//! Later tasks fill this in:
//!   * Property 1/2 (DMA share/unshare + cross-page round trip)  — task 11.2/11.3
//!   * Property 3   (context-frame interchangeability)            — task 13.2
//!   * Property 4/5 (PMM reserved + alloc/free conservation)      — task 10.2/10.3
//!   * Property 6/7/8 (ext2 capacity sizing / round trip / reject)— task 14.3-14.5
//!   * Property 9   (/dev/serial byte fidelity)                   — task 9.2
//!
//! For now this module only proves the proptest harness is wired up and discovered
//! by `cargo test`.

// The `alloc` crate is available in the host `std` sysroot; declaring it here lets
// the `#[path]`-included kernel modules that are `core` + `alloc` (e.g. `stack`)
// resolve `alloc::vec::Vec` identically to the kernel's crate-root `extern crate alloc;`.
extern crate alloc;

// ---------------------------------------------------------------------------
// Shared kernel pure logic
// ---------------------------------------------------------------------------
//
// Pure, `core`-only kernel modules are included here directly via `#[path]` so the
// host property tests exercise the SAME source the kernel compiles (R11.6) — no
// copy to drift out of sync. Each later task wires in its module the same way.
#[path = "../../src/arch/x86_64/linux/errno.rs"]
pub mod errno;

#[path = "../../src/arch/x86_64/linux/stat.rs"]
pub mod stat;

#[path = "../../src/arch/x86_64/linux/validate.rs"]
pub mod validate;

// `io` references the errno model via `super::errno`. Declared here as a crate-root
// sibling of `errno` so that relative path resolves identically to the kernel's
// `crate::arch::x86_64::linux::{errno, io}` siblings (task 2.1; tests in P5/P6).
#[path = "../../src/arch/x86_64/linux/io.rs"]
pub mod io;

#[path = "../../src/arch/x86_64/linux/abi.rs"]
pub mod abi;

#[path = "../../src/arch/x86_64/linux/rand_clock.rs"]
pub mod rand_clock;

// `stack` is `core` + `alloc` only (uses `alloc::vec::Vec`). The host crate links
// `std`, which provides the `alloc` crate via the `extern crate alloc;` below, so
// the exact same kernel source compiles and runs under proptest (task 5.4; P17–P19).
#[path = "../../src/task/stack.rs"]
pub mod stack;

// Pure ELF classifier + static-PIE bias selection (task 5.1). Lives in
// `src/vfs/elf_classify.rs` precisely so it carries no kernel/paging deps and is
// host-testable (R11.6). Tests in P15/P16 (tasks 5.2/5.3).
#[path = "../../src/vfs/elf_classify.rs"]
pub mod elf_classify;

// `http` is `core` + `alloc` only (pure GET building + response-head parsing);
// included so P20/P21 exercise the same source the kernel compiles (task 7.1).
#[path = "../../src/net/http.rs"]
pub mod http;

// `dns` is `core` + `alloc` only (pure DNS query building + A-record response
// parsing, speaking plain byte slices / `[u8; 4]` — no smoltcp types); included
// so the DNS parser properties exercise the same source the kernel compiles
// (R11.6). The effectful `resolve` socket pump stays in `net/mod.rs`.
#[path = "../../src/net/dns.rs"]
pub mod dns;

// `deb` is `core` + `alloc` only (pure `ar` enumeration + `.deb` member location
// + compression classification); included so P23 exercises the same source the
// kernel compiles (task 8.1). It is self-contained — no sibling `pkg` deps — so
// the standalone `#[path]` include resolves with no extra wiring.
#[path = "../../src/pkg/deb.rs"]
pub mod deb;

// `tar` is the pure ustar reader/writer (component 9). Like `deb` it is `core` +
// `alloc` only and self-contained (no sibling `pkg` deps), so the standalone
// `#[path]` include resolves with no extra wiring (task 8.5; tests in P24/P25).
#[path = "../../src/pkg/tar.rs"]
pub mod tar;

// `install` is the pure install-path normalization + install model (component 10).
// It references `TarEntry`/`TarType` via `super::tar`, which here resolves to the
// crate-root `tar` module declared just above (in the kernel it resolves to
// `crate::pkg::tar`, the sibling of `crate::pkg::install`) — one source compiles in
// both crates with no shim (task 8.8; test in P26, task 8.9).
#[path = "../../src/pkg/install.rs"]
pub mod install;

// `apt_index` is the pure `Packages` index parser + lookup index (the read side
// of `apt install`). `core` + `alloc` only and self-contained (no sibling deps),
// so the standalone `#[path]` include resolves with no extra wiring (P30).
#[path = "../../src/pkg/apt_index.rs"]
pub mod apt_index;

// `apt_resolve` is the pure dependency resolver. It references the index types
// via `super::apt_index`, which here resolves to the crate-root `apt_index`
// declared just above (in the kernel it resolves to `crate::pkg::apt_index`, the
// sibling of `crate::pkg::apt_resolve`) — one source compiles in both crates with
// no shim, exactly as `install` uses `super::tar` (P30).
#[path = "../../src/pkg/apt_resolve.rs"]
pub mod apt_resolve;

// `mirror` is the pure `apt setmirror` host-argument parser (URL-scheme prefix
// handling). `core`-only and self-contained, so the standalone `#[path]` include
// resolves with no extra wiring. Its inline `#[cfg(test)]` unit tests run here
// under `cargo test` (HTTPS/TLS feature) and exercise the same source the kernel
// compiles (R11.6).
#[path = "../../src/pkg/mirror.rs"]
pub mod mirror;

// `diag` uses `alloc::collections::BTreeSet`; the `extern crate alloc;` declared
// at the top of this crate brings the sysroot allocator into scope for it.
#[path = "../../src/arch/x86_64/linux/diag.rs"]
pub mod diag;

// `mem` references the errno model via `super::errno` and `USER_ADDR_MAX` via
// `super::validate`. Declared here as a crate-root sibling of both so those relative
// paths resolve identically to the kernel's `crate::arch::x86_64::linux::{errno,
// validate, mem}` siblings (task 3.1; tests in P11/P12/P13/P14).
#[path = "../../src/arch/x86_64/linux/mem.rs"]
pub mod mem;

// `dirent` is the pure `getdents64` record packer (Feature: linux-binary-compat).
// `core` + `alloc` only and self-contained, so the standalone `#[path]` include
// resolves with no extra wiring (P32).
#[path = "../../src/arch/x86_64/linux/dirent.rs"]
pub mod dirent;

// `timeconv` is the pure time math (BCD decode, civil-date->unix-seconds, timeval
// encoding) backing the wall-clock syscalls and the CMOS RTC reader. `core`-only
// and self-contained, so the standalone `#[path]` include resolves with no extra
// wiring (P32).
#[path = "../../src/arch/x86_64/linux/timeconv.rs"]
pub mod timeconv;

// `fd_alloc` is the pure, dependency-free file-descriptor bookkeeping (lowest free
// index >= 3 + EBADF semantics) extracted from the per-process fd table (task 11.1).
// The kernel-facing `task::fd::FdTable` embeds `Arc<dyn VfsNode>`, which cannot be
// compiled on the host, so only this `core` + `alloc` seam is shared here for
// Property 7 (R11.6; test in P7, task 12.0).
#[path = "../../src/task/fd_alloc.rs"]
pub mod fd_alloc;

// ---------------------------------------------------------------------------
// Property-test modules (P1..P28)
// ---------------------------------------------------------------------------
//
// Pre-created as placeholders so each later test task only edits its own file.
// They are `#[cfg(test)]` so they compile only under `cargo test`.
#[cfg(test)]
mod properties {
    mod p01;
    mod p02;
    mod p03;
    mod p04;
    mod p05;
    mod p06;
    mod p07;
    mod p08;
    mod p09;
    mod p10;
    mod p11;
    mod p12;
    mod p13;
    mod p14;
    mod p15;
    mod p16;
    mod p17;
    mod p18;
    mod p19;
    mod p20;
    mod p21;
    mod p22;
    mod p23;
    mod p24;
    mod p25;
    mod p26;
    mod p27;
    mod p28;
    mod p29;
    // Authoritative xz/zstd fixtures consumed by p29 (generated by gen_fixtures.py).
    mod p29_fixtures;
    mod p30;
    mod p31;
    mod p32;
    mod p33;
    mod p34;
    mod p35;
    mod p36;
    mod p37;
    mod p38;
    mod p39;
    mod p40;
    mod p41;
}

// PHASE 0 diagnostic: large-scale (60k stanza) apt-index repro harness for the
// `apt update` parse-stage crash. Left in place (clearly marked) for the fix.
#[cfg(test)]
mod bigindex;

#[cfg(test)]
mod scaffolding {
    use proptest::prelude::*;

    proptest! {
        /// Smoke test: confirms the `proptest!` harness compiles, links `std`, and
        /// runs on the host. `u8::wrapping_add` is associative-with-zero, a trivially
        /// true property that exercises generator + shrinking machinery end to end.
        #[test]
        fn proptest_harness_is_wired(x in any::<u8>()) {
            prop_assert_eq!(x.wrapping_add(0), x);
        }
    }
}
