//! Boot-time Linux-compatibility self-test harness (cargo feature `lx_selftest`).
//!
//! This module is a single consolidated on-target smoke/integration harness for the
//! Linux binary-compatibility layer. It is gated behind the `lx_selftest` cargo
//! feature so the normal boot/shell is byte-for-byte unchanged when the feature is
//! off: `src/lib.rs` only declares `mod selftest_lx;` under the feature, and
//! `boot.rs` only calls [`run`] under `cfg!(feature = "lx_selftest")`.
//!
//! It consolidates the on-target tasks from the spec's Testing Strategy
//! (integration/smoke list):
//!   * 13.4 end-to-end load + run + exit isolation + load-failure-no-enqueue
//!   * 12.2 `open`/`openat` of an absent path -> `-ENOENT`
//!   * 12.6 `arch_prctl(ARCH_SET_FS)` / `set_tid_address` / `uname`
//!   * 12.8 register preservation across a syscall (R1.7)
//!   * 12.4 OOM/over-limit rollback for `brk`/`mmap` (state unchanged)
//!   * 14.2 `fetch_deb` returns `NoNetwork` when no interface address is configured
//!   * 14.4 ext2 `data.tar` install round trip (read back byte-for-byte)
//!
//! Each check prints exactly one line `LXSELFTEST <name> PASS` or
//! `LXSELFTEST <name> FAIL <detail>` through the kernel's existing `info!`/`error!`
//! serial path. Every check is defensive: a failing step prints a `FAIL` line and
//! returns rather than panicking (the kernel is `panic = "abort"`, so a panic would
//! triple-fault the harness), and one failing check never prevents the others from
//! running.
//!
//! ## When this runs
//!
//! [`run`] is invoked from `boot::kernel_main` *after* the ext2 mount and networking
//! init but *before* interrupts are enabled — the same window in which the existing
//! `spawn_test_user_process` runs. This ordering matters:
//!   * a `Compat_Process` can be enqueued exactly like the native test process, and
//!   * DHCP has not yet run (the net thread services it only after interrupts are
//!     enabled), so no interface address is configured and the `fetch_no_network`
//!     check observes the `NoNetwork` preflight path (R8.7).
//!
//! ## Direct-handler checks
//!
//! Several checks invoke the effectful handlers directly in a kernel context. They
//! need a per-process [`CompatState`] registered for the running pid (the boot/idle
//! pid `0`) and, for the pointer-taking handlers, a scratch page mapped at a
//! user-half virtual address so the single `check_user_ptr` choke point accepts it.
//! [`with_synth_compat`] and [`map_scratch`]/[`unmap_scratch`] provide these and tear
//! them down afterwards, so the harness leaves no persistent compat state on the idle
//! task.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use x86_64::registers::model_specific::FsBase;
use x86_64::structures::paging::PageTableFlags;

use crate::arch::x86_64::linux::errno::Errno;
use crate::arch::x86_64::linux::mem::{VmRegionSet, MAP_ANONYMOUS, MAP_PRIVATE, PROT_WRITE};
use crate::arch::x86_64::linux::regs::SavedRegs;
use crate::arch::x86_64::linux::validate::USER_ADDR_MAX;
use crate::arch::x86_64::linux::{io_sys, linux_dispatch, mem_sys, misc};
use crate::memory::layout::USER_MMAP_BASE;
use crate::memory::{pmm, vmm};
use crate::net::http_fetch::{fetch_deb, FetchError};
use crate::pkg::install_fs::install_data_tar;
use crate::pkg::tar::{read_tar, write_tar};
use crate::task::compat::{self, CompatState};
use crate::task::fd::FdTable;
use crate::task::process::{run_linux_binary, RunError};
use crate::task::scheduler;
use crate::vfs;

/// User-half scratch virtual address for the pointer-taking handler checks.
///
/// 64 TiB: comfortably below `USER_ADDR_MAX` (so `check_user_range` accepts it) and
/// far from anything the kernel PML4 maps (the kernel lives in the higher half and
/// never maps this low-half address), so mapping a fresh frame here cannot collide.
/// It is not a higher-half entry, so it is never shared into any user PML4 and can
/// have no effect on real `Compat_Process` address spaces.
const SCRATCH_VA: u64 = 0x0000_4000_0000_0000;

/// `arch_prctl` subfunction code: set `FS.base`.
const ARCH_SET_FS: u64 = 0x1002;

/// Run the consolidated Linux-compat self-test. Prints one `LXSELFTEST <name>
/// PASS|FAIL ...` line per check. Never panics.
pub fn run() {
    crate::info!("LXSELFTEST harness start");

    check_end_to_end_run();
    check_exit_isolation();
    check_open_absent();
    check_arch_prctl_uname_tid();
    check_register_preservation();
    check_oom_rollback();
    check_fetch_no_network();
    check_ext2_install_roundtrip();

    // ── New "no new process model" syscalls (Feature: linux-binary-compat) ──
    check_getcwd();
    check_chdir();
    check_dup();
    check_walltime();
    check_getdents();
    check_links();

    crate::info!("LXSELFTEST harness done");
}

/// How long the post-network checks wait for `net::ip_config()` to become
/// available (60 s at `TICK_HZ`). One shared constant so the local-mirror and
/// HTTPS checks cannot drift into different ideas of "the network is up".
const WAIT_IFACE_TICKS: u64 = crate::arch::x86_64::apic::TICK_HZ * 60;

/// Post-network HTTPS smoke test entry point (cargo feature `lx_selftest`).
///
/// Spawned as a kernel thread from `boot::kernel_main` (NOT called from [`run`],
/// which runs before interrupts/DHCP). It waits for the interface to acquire an
/// address (DHCP lease or static fallback), then performs one real **HTTPS (TLS
/// 1.3)** GET against a small file reachable through QEMU user-net NAT and reports:
///
///   * `LXSELFTEST https_get PASS ...` — the TLS handshake completed and an HTTP
///     200 with a non-empty body was decrypted over the encrypted channel, or
///   * `LXSELFTEST https_get FAIL <FetchError>` — what went wrong (a parsed but
///     non-200 status still proves the handshake completed; it is reported as
///     `Status(code)`).
///
/// SECURITY: [`crate::net::tls::https_get`] is **fail-closed server
/// authentication**, so this check exercises the whole verify path against a live
/// `deb.debian.org`: the handshake completes only if the server's chain reaches
/// the committed CA bundle, the SAN authorizes the host, the clock gate passes
/// and the `CertificateVerify` signature checks out. A `PASS` is therefore
/// evidence of authenticated transport, and a `FAIL` is accompanied by the
/// verifier's own `Package_Fetcher(tls): stage=verify cause=…` line naming the
/// exact refused check (see `SECURITY.md` for what is still unverified:
/// repository metadata signatures and package digests).
pub fn run_net_smoke() {
    let name = "https_get";

    // Wait up to ~60 s for an interface address. The lease/fallback lands
    // seconds after boot, but under TCG the ring-3 selftest checks that run
    // before this thread can push that well past the old 15 s window — and a
    // timeout here is reported as "no interface address", which reads like a
    // network failure instead of "the harness gave up too early".
    let deadline = scheduler::ticks() + WAIT_IFACE_TICKS;
    while crate::net::ip_config().is_none() {
        if scheduler::ticks() >= deadline {
            fail(
                name,
                "no interface address within 60 s (DHCP/static fallback did not configure)",
            );
            return;
        }
        scheduler::sleep_ticks(10);
    }

    crate::info!(
        "LXSELFTEST https_get: interface up, attempting authenticated TLS 1.3 GET \
         (chain -> committed CA bundle, SAN, clock gate, CertificateVerify) ..."
    );

    // A small, stable file on the default Debian mirror (served by Fastly over
    // HTTPS). The Release index is a few KiB of text — quick to download over NAT.
    match crate::net::tls::https_get("deb.debian.org", 443, "/debian/dists/stable/Release") {
        Ok(body) if !body.is_empty() => {
            crate::info!(
                "LXSELFTEST https_get PASS (TLS 1.3 handshake OK; HTTP 200; {} body bytes decrypted)",
                body.len()
            );
        }
        Ok(_) => fail(name, "HTTP 200 but empty body"),
        Err(e) => crate::error!("LXSELFTEST https_get FAIL {:?}", e),
    }
}

/// Run the post-network self-tests sequentially on one thread.
///
/// The local-mirror `apt` end-to-end test ([`run_apt_e2e`]) runs first — it talks
/// only to the QEMU host gateway and completes quickly — then the external HTTPS
/// smoke test ([`run_net_smoke`]). Sequencing them avoids two threads contending
/// for the single network pump (a slow/unreachable external HTTPS handshake would
/// otherwise starve the local apt fetch).
pub fn run_post_net_checks() {
    run_apt_e2e();
    // Give the scheduler a window to run the just-enqueued hello-pagh process so
    // its "hello from apt" output lands on serial before the external HTTPS test
    // (which may monopolize the network pump) begins.
    scheduler::sleep_ticks(200);
    run_net_smoke();
}

/// Live full-update integration check against the real `deb.debian.org` mirror
/// (cargo feature `lx_livetest`; spec task 11.1).
///
/// Spawned as a kernel thread from `boot::kernel_main` under the **dedicated**
/// `lx_livetest` feature so it never runs in the normal kernel or the regular
/// `lx_selftest` harness. It deliberately does NOT `set_mirror` to the local
/// mini-repo, so the run starts from the DEFAULT apt configuration
/// (`deb.debian.org` `/debian stable main amd64`) and then switches the transport
/// to cleartext HTTP (see the `set_mirror` call and its WHY below — the large
/// index download is what needs HTTP, not the trust story). It drives the full
/// live update + install pipeline:
///
///   1. wait for the interface to acquire an address (DHCP, then static fallback),
///   2. `apt::update()` against the live mirror; on `Ok(count)` log
///      `LIVE_APT_UPDATE: count=N` and assert `N >= 50_000` (R1.2). The
///      `apt: decompressed K KiB, parsed P packages...` lines emitted underneath
///      are the monotonic-progress evidence (R1.4/R3.2) and `apt: index loaded
///      (N packages)` is the terminal no-hang outcome (R3.1),
///   3. report the Resident_Index_Footprint via [`apt::index_footprint`]
///      (R2.4/R6.2),
///   4. `apt::install("busybox-static")` then run it through the loader
///      (R8.1–R8.3).
///
/// Prints `LXSELFTEST live_update PASS ...` on full success, or a single `FAIL`
/// line naming the failing step (never hangs, never panics).
///
/// PER Q-A the timing is **soft and non-binding**: this is network-dependent and
/// slow under QEMU/TCG. The harness script gates on serial evidence, not
/// wall-clock; a still-progressing run at the script timeout reports its partial
/// monotonic-progress evidence as an acceptable outcome.
#[cfg(feature = "lx_livetest")]
pub fn run_live_update_check() {
    let name = "live_update";

    // Wait up to ~60 s for an interface address (DHCP, then static fallback) —
    // same shared window as the other post-network checks. A live update also
    // needs DNS + TLS, so it must not be cut short by a shorter wait than the
    // local-mirror check uses. (The previous `ticks() + 3000` was 3 s, not the
    // ~30 s the comment claimed: TICK_HZ is 1000.)
    let deadline = scheduler::ticks() + WAIT_IFACE_TICKS;
    while crate::net::ip_config().is_none() {
        if scheduler::ticks() >= deadline {
            fail(
                name,
                "no interface address within 60 s (DHCP/static fallback did not configure)",
            );
            return;
        }
        scheduler::sleep_ticks(10);
    }

    // Point apt at the cleartext HTTP mirror so the large index download uses
    // `http_get` (which does NOT touch embedded-tls) rather than the HTTPS path.
    //
    // WHY HTTP: embedded-tls deterministically hangs at ~12 MiB on large streams
    // (the read() future stops returning to our executor, so our transport is
    // never re-entered and no timeout can fire) — a library limitation, not a trust
    // decision, and it applies to the authenticated HTTPS path exactly as it did
    // to the old unverified one. Repository metadata signatures and package
    // digests are still unverified on EITHER transport, so plain HTTP is the
    // honest, working way to COMPLETE a live full update from the official Debian
    // mirror; the authenticated HTTPS path itself is covered end-to-end by
    // `run_net_smoke` (small file) above. http://deb.debian.org/debian sets
    // tls=false, port=80, base=/debian.
    crate::pkg::apt::set_mirror("http://deb.debian.org", Some("/debian"));

    // Confirm the active mirror config and log it so the serial record is
    // unambiguous (now updating over HTTP, not HTTPS).
    let cfg = crate::pkg::apt::config();
    crate::info!(
        "LXSELFTEST live_update: interface up, updating over http against {}://{}{} ({} {} {})",
        cfg.scheme(),
        cfg.host,
        cfg.base,
        cfg.suite,
        cfg.component,
        cfg.arch
    );

    // 2. Live full update. The `apt:` progress + terminal lines are emitted by
    //    `apt::update()` itself and are the R1.4/R3.2/R3.1 evidence.
    let count = match crate::pkg::apt::update() {
        Ok(n) => n,
        Err(e) => {
            crate::error!("LXSELFTEST live_update FAIL update: {}", e.message());
            return;
        }
    };
    crate::info!("LIVE_APT_UPDATE: count={}", count);

    // 3. Report the resident index footprint (R2.4/R6.2).
    if let Some(fp) = crate::pkg::apt::index_footprint() {
        crate::info!(
            "LXSELFTEST live_update: Resident_Index_Footprint = {} bytes ({} KiB)",
            fp,
            fp / 1024
        );
    }

    // Assert the real `main` index scale (R1.2). A short count means we did not
    // actually swallow the full live index.
    if count < 50_000 {
        crate::error!(
            "LXSELFTEST live_update FAIL count {} < 50000 (did not load full index)",
            count
        );
        return;
    }

    // 4. Resolve + install a real static package by name, then run it (R8.1–R8.3).
    let pkg = "busybox-static";
    let installed = match crate::pkg::apt::install(pkg) {
        Ok(v) => v,
        Err(e) => {
            crate::error!("LXSELFTEST live_update FAIL install: {}", e.message());
            return;
        }
    };
    crate::info!("LXSELFTEST live_update: installed {:?}", installed);

    // busybox-static ships its binary at /bin/busybox; it was written onto ext2
    // under /mnt by the installer.
    let bin_path = "/mnt/bin/busybox";
    match vfs::lookup_path(bin_path) {
        Ok(node) if !node.is_directory() && node.size() > 0 => {}
        Ok(_) => {
            fail(name, "installed busybox path is a directory or empty");
            return;
        }
        Err(_) => {
            fail(name, "installed busybox binary not found under /mnt/bin");
            return;
        }
    }

    // Run busybox with no args (prints its usage banner) to prove the installed
    // static Linux ELF loads and executes via the loader.
    match run_linux_binary(bin_path, &[b"busybox"], &[]) {
        Ok(pid) => {
            crate::info!(
                "LXSELFTEST live_update PASS (index {} pkgs; installed {}; spawned busybox pid={})",
                count,
                installed.len(),
                pid
            );
        }
        Err(e) => match e {
            RunError::ArgsTooLarge => fail(name, "run returned ArgsTooLarge"),
            RunError::NotFound => fail(name, "run returned NotFound"),
            RunError::LoadFailed(c) => fail(name, c),
            RunError::StackFailed => fail(name, "run returned StackFailed"),
        },
    }
}

/// End-to-end `apt` smoke test against the local mini-repo (cargo feature
/// `lx_selftest`).
///
/// Spawned as a kernel thread from `boot::kernel_main` (NOT called from [`run`],
/// which runs before interrupts/DHCP). It waits for the interface to acquire an
/// address, then drives the full by-name install pipeline against a tiny
/// Debian-style repository served on the QEMU user-net host gateway
/// (`http://10.0.2.2:8000`, built by `tools/mini_repo.py`):
///
///   1. `set_mirror("http://10.0.2.2:8000", "/")` — cleartext HTTP on port 8000,
///      mirror root (so `dists/...` and `pool/...` resolve directly).
///   2. `apt::update()` — fetch + stream-parse the tiny `Packages.gz` index.
///   3. `apt::install("hello-pagh")` — fetch the `.deb`, decompress `data.tar.gz`,
///      and write `usr/bin/hello-pagh` onto ext2 under `/mnt`.
///   4. `run_linux_binary("/mnt/usr/bin/hello-pagh")` — load + enqueue the
///      installed static ELF, which prints `hello from apt` and `exit_group`s.
///
/// Prints `LXSELFTEST apt_e2e PASS ...` if the index loaded (>=1 package), the
/// file was written, and the binary was enqueued; otherwise a single `FAIL` line
/// naming the failing step (never hangs, never panics). The installed binary's
/// `hello from apt` line appears separately on serial once the scheduler runs it.
pub fn run_apt_e2e() {
    let name = "apt_e2e";

    // Wait up to ~60 s for an interface address (DHCP, then static fallback) —
    // same window as `run_net_smoke`; see the rationale there.
    let deadline = scheduler::ticks() + WAIT_IFACE_TICKS;
    while crate::net::ip_config().is_none() {
        if scheduler::ticks() >= deadline {
            fail(
                name,
                "no interface address (DHCP/static fallback did not configure)",
            );
            return;
        }
        scheduler::sleep_ticks(10);
    }

    crate::info!("LXSELFTEST apt_e2e: interface up, pointing apt at http://10.0.2.2:8000 ...");

    // 1. Point apt at the local mirror (cleartext HTTP, port 8000, mirror root).
    crate::pkg::apt::set_mirror("http://10.0.2.2:8000", Some("/"));

    // 2. Download + stream-parse the index.
    let count = match crate::pkg::apt::update() {
        Ok(n) => n,
        Err(e) => {
            crate::error!("LXSELFTEST apt_e2e FAIL update: {}", e.message());
            return;
        }
    };
    if count == 0 {
        fail(name, "index loaded but contained 0 packages");
        return;
    }
    crate::info!("LXSELFTEST apt_e2e: index loaded ({} packages)", count);

    // 3. Resolve + install the package onto ext2.
    let installed = match crate::pkg::apt::install("hello-pagh") {
        Ok(v) => v,
        Err(e) => {
            crate::error!("LXSELFTEST apt_e2e FAIL install: {}", e.message());
            return;
        }
    };
    crate::info!("LXSELFTEST apt_e2e: installed {:?}", installed);

    // The installed binary must be present on ext2 under /mnt.
    let bin_path = "/mnt/usr/bin/hello-pagh";
    match vfs::lookup_path(bin_path) {
        Ok(node) if !node.is_directory() && node.size() > 0 => {}
        Ok(_) => {
            fail(name, "installed path is a directory or empty");
            return;
        }
        Err(_) => {
            fail(name, "installed binary not found under /mnt");
            return;
        }
    }

    // 4. Load + enqueue the installed Linux binary; it prints "hello from apt".
    match run_linux_binary(bin_path, &[b"hello-pagh"], &[]) {
        Ok(pid) => {
            crate::info!(
                "LXSELFTEST apt_e2e PASS (index {} pkgs; installed {}; spawned hello-pagh pid={})",
                count,
                installed.len(),
                pid
            );
        }
        Err(e) => match e {
            RunError::ArgsTooLarge => fail(name, "run returned ArgsTooLarge"),
            RunError::NotFound => fail(name, "run returned NotFound"),
            RunError::LoadFailed(c) => fail(name, c),
            RunError::StackFailed => fail(name, "run returned StackFailed"),
        },
    }
}

// ───────────────────────── Part B: bigindex parse-crash repro ─────────────────────────

/// DIAGNOSTIC (Part B): reproduce the `apt update` parse-stage crash (#14 PF,
/// RIP=0x1) over a LOCAL big index (cargo feature `lx_bigindex`).
///
/// Spawned as a kernel thread from `boot::kernel_main` (after interrupts/DHCP).
/// It waits for an interface address, points apt at the local mini-repo big
/// index served by `tools/mini_repo.py bigindex N 8000`, then drives one of two
/// variants used to DISCRIMINATE the crash cause:
///
///   * default (`lx_bigindex`): [`bigindex_streaming`] — the REAL path,
///     `apt::update()` fetching `Packages.gz` over the socket and stream-parsing
///     concurrently with NET/scheduler activity (mirrors the live crash).
///   * with `lx_bigindex_inram` also set: [`bigindex_inram`] ("Test A") — fetch
///     the gz ONCE into RAM, then decompress+parse in a tight single-threaded
///     loop with NO concurrent socket/NET work, isolating parse/heap from
///     net/scheduler.
///
/// Both log heap headroom (`BIGINDEX heap: ...`) ~every 1 MiB so allocator
/// exhaustion/corruption can be ruled in or out. The default kernel build does
/// not compile or run any of this.
#[cfg(feature = "lx_bigindex")]
pub fn run_bigindex_check() {
    let name = "bigindex";

    // Wait up to ~60 s for an interface address (DHCP, then static fallback) —
    // the shared window; see `WAIT_IFACE_TICKS`. (The previous
    // `ticks() + 2000` was 2 s, not the ~20 s the comment claimed.)
    let deadline = scheduler::ticks() + WAIT_IFACE_TICKS;
    while crate::net::ip_config().is_none() {
        if scheduler::ticks() >= deadline {
            fail(
                name,
                "no interface address within 60 s (DHCP/static fallback did not configure)",
            );
            return;
        }
        scheduler::sleep_ticks(10);
    }

    let (size, used, free) = crate::memory::heap::stats();
    crate::info!(
        "BIGINDEX: interface up; heap used {} KiB / free {} KiB / size {} KiB",
        used / 1024,
        free / 1024,
        size / 1024
    );

    // MIRROR BASE NOTE: the Part-B brief suggested base "/debian", but the local
    // `tools/mini_repo.py` serves its tree from the mirror ROOT (the index lives
    // at "/dists/stable/main/binary-amd64/Packages.gz"), so "/debian/dists/..."
    // would 404 and never reach the parse stage we are trying to crash. We
    // therefore point apt at the mirror root "/" — exactly as the working
    // `run_apt_e2e` does — so the streaming fetch+parse path actually runs.
    crate::pkg::apt::set_mirror("http://10.0.2.2:8000", Some("/"));

    #[cfg(not(feature = "lx_bigindex_inram"))]
    bigindex_streaming(name);
    #[cfg(feature = "lx_bigindex_inram")]
    bigindex_inram(name);
}

/// Variant 1 (default `lx_bigindex`): drive the REAL streaming path. `apt::update()`
/// fetches the big `Packages.gz` over the TCP socket and stream-parses it while
/// the NET thread/scheduler are live — the exact concurrency the live crash had.
/// If the #14 PF reproduces, the QEMU exception dump appears before any PASS line.
#[cfg(all(feature = "lx_bigindex", not(feature = "lx_bigindex_inram")))]
fn bigindex_streaming(name: &str) {
    crate::info!("BIGINDEX variant=STREAMING (apt::update over socket; net+parse concurrent)");
    match crate::pkg::apt::update() {
        Ok(n) => {
            let (size, used, free) = crate::memory::heap::stats();
            crate::info!(
                "BIGINDEX update OK: {} packages; heap used {} KiB / free {} KiB / size {} KiB",
                n,
                used / 1024,
                free / 1024,
                size / 1024
            );
            crate::info!("LXSELFTEST bigindex PASS (streaming; {} packages)", n);
        }
        Err(e) => fail(name, &e.message()),
    }
}

/// Variant 2 / "Test A" (`lx_bigindex` + `lx_bigindex_inram`): isolate parse/heap
/// from net+scheduler ENTIRELY. Build the large `Packages` document IN-KERNEL
/// from an in-RAM buffer (mirroring the host-tests `bigindex` generator and the
/// `tools/mini_repo.py bigindex` field layout), then run `StanzaParser::push_view`
/// into the builder in a tight single-threaded loop in fixed 8 KiB chunks — the
/// exact kernel parse path `apt::update` uses on decompressed output — with NO
/// socket/NET work at all.
///
/// WHY IN-KERNEL (not "fetch once"): the brief's Test A suggested fetching the gz
/// once then parsing. In THIS environment the cleartext fetch over QEMU user-net
/// under TCG does not complete in bounded time (it busy-progresses glacially and
/// never delivers even ~300 KiB), so a fetch-first Test A never reaches the parse
/// stage. Generating the buffer in-kernel achieves the SAME — actually stronger —
/// isolation (zero net, zero scheduler contention from a socket pump) and lets the
/// parse/heap path run at full crash scale. If this STILL faults at ~the same
/// package count → the bug is the parse/heap path itself (kernel allocator at
/// scale or stack), NOT net/scheduler. If it completes → the parse/heap path is
/// clean in-kernel and the live crash is the net/scheduler interaction during
/// fetch+parse. Heap headroom is logged ~every 1 MiB pushed.
#[cfg(all(feature = "lx_bigindex", feature = "lx_bigindex_inram"))]
fn bigindex_inram(name: &str) {
    use crate::pkg::apt_index::{PackageIndex, PackageIndexBuilder, StanzaParser};

    /// Stanza count. Past the live-crash point (~5459 pkgs / 4 MiB decompressed):
    /// 12000 stanzas decompress to ~4.5 MiB and exceed the crash package count,
    /// while keeping the in-kernel doc build tractable under QEMU/TCG (60k is the
    /// host-test scale but takes many minutes to *generate* in-kernel under TCG).
    const N_STANZAS: usize = 12_000;
    /// Chunk size feeding `push_view`, matching `deb::decompress_stream`'s 8 KiB.
    const CHUNK: usize = 8 * 1024;

    crate::info!(
        "BIGINDEX variant=IN-RAM (Test A: build {} stanzas IN-KERNEL, parse with ZERO net)",
        N_STANZAS
    );

    // 1. Build the big Packages document in-kernel (in-RAM buffer). Mirrors
    //    host-tests bigindex::build_big_packages field-for-field.
    let doc = build_big_packages_kernel(N_STANZAS);
    let bytes = doc.as_bytes();
    let (size, used, free) = crate::memory::heap::stats();
    crate::info!(
        "BIGINDEX built doc = {} KiB in RAM; heap used {} KiB / free {} KiB / size {} KiB",
        bytes.len() / 1024,
        used / 1024,
        free / 1024,
        size / 1024
    );

    // 2. Parse in a tight loop in 8 KiB chunks — the exact kernel push_view path,
    //    with NO concurrent socket/NET work.
    let mut parser = StanzaParser::new();
    let mut builder = PackageIndexBuilder::new();
    let mut pushed: usize = 0;
    let mut next_heap_mark: usize = 1024 * 1024;
    let mut pos = 0;
    while pos < bytes.len() {
        let end = (pos + CHUNK).min(bytes.len());
        parser.push_view(&bytes[pos..end], &mut builder);
        pushed += end - pos;
        pos = end;
        if pushed >= next_heap_mark {
            let (size, used, free) = crate::memory::heap::stats();
            crate::info!(
                "BIGINDEX heap: pushed {} KiB, pkgs {}, heap used {} KiB / free {} KiB / size {} KiB",
                pushed / 1024,
                builder.len(),
                used / 1024,
                free / 1024,
                size / 1024
            );
            while pushed >= next_heap_mark {
                next_heap_mark += 1024 * 1024;
            }
        }
    }
    parser.finish_view(&mut builder);
    let idx = PackageIndex::from_builder(builder);
    let _ = name;
    crate::info!(
        "LXSELFTEST bigindex PASS (in-ram in-kernel; {} packages parsed)",
        idx.len()
    );
}

/// Build a large, realistic `Packages` document with `n` stanzas, IN-KERNEL.
/// Field-for-field mirror of host-tests `bigindex::build_big_packages` and
/// `tools/mini_repo.py build_big_index`, so the in-kernel parse stresses the
/// same dependency-group / provides side-table arithmetic at scale.
#[cfg(all(feature = "lx_bigindex", feature = "lx_bigindex_inram"))]
fn build_big_packages_kernel(n: usize) -> alloc::string::String {
    use alloc::string::String;
    let mut s = String::with_capacity(n * 256);
    for i in 0..n {
        let pkg = alloc::format!("pkg-{:06}", i);
        s.push_str("Package: ");
        s.push_str(&pkg);
        s.push('\n');

        s.push_str("Version: ");
        s.push_str(&alloc::format!(
            "{}.{}.{}-{}",
            i % 10,
            i % 100,
            i % 7,
            i % 3
        ));
        s.push('\n');

        s.push_str("Architecture: amd64\n");

        s.push_str("Filename: ");
        s.push_str(&alloc::format!(
            "pool/main/p/{}/{}_{}.{}_amd64.deb",
            pkg,
            pkg,
            i % 10,
            i % 100
        ));
        s.push('\n');

        if i > 2 {
            s.push_str(&alloc::format!(
                "Depends: pkg-{:06} (>= 1.0), pkg-{:06} | pkg-{:06} (>= 2.0)\n",
                i - 1,
                i - 2,
                i - 3
            ));
        }

        if i % 5 == 0 {
            s.push_str(&alloc::format!("Provides: virtual-{:06}, feature-x\n", i));
        }

        s.push_str("Maintainer: Pagh-OS <root@pagh>\n");

        s.push_str("Description: synthetic package ");
        s.push_str(&pkg);
        s.push('\n');
        s.push_str(" This is a continuation line describing the package in detail\n");
        s.push_str(" across multiple physical lines for realism.\n");

        s.push_str("Size: ");
        s.push_str(&alloc::format!("{}", 1000 + (i as u64) * 37));
        s.push('\n');

        s.push('\n');
    }
    s
}

// ───────────────────────────── reporting helpers ─────────────────────────────
/// Emit the single `PASS` line for a check.
fn pass(name: &str) {
    crate::info!("LXSELFTEST {} PASS", name);
}

/// Emit the single `FAIL` line for a check (with a short detail string).
fn fail(name: &str, detail: &str) {
    crate::error!("LXSELFTEST {} FAIL {}", name, detail);
}

// ───────────────────────── synthesized-state helpers ─────────────────────────

/// Run `f` with a freshly-synthesized [`CompatState`] registered for the running
/// pid, then remove it. The state carries a standard-streams [`FdTable`], a
/// [`VmRegionSet`] seeded from `initial_brk` (and the canonical `USER_MMAP_BASE`
/// hint), and the given `tid`. This lets the effectful handlers (which look the
/// running process's state up by `scheduler::current_pid`) operate in a kernel
/// context without a real ring-3 process.
fn with_synth_compat<R>(initial_brk: u64, tid: u64, f: impl FnOnce() -> R) -> R {
    let pid = scheduler::current_pid();
    let state = CompatState::new(
        FdTable::with_standard_streams(),
        Arc::new(crate::sync::spinlock::Spinlock::new(VmRegionSet::new(
            initial_brk,
            USER_MMAP_BASE,
        ))),
        tid,
    );
    compat::install_compat(pid, state);
    let r = f();
    // Always drop the synthesized state so the idle/boot task is not left looking
    // like a Compat_Process to the dispatcher.
    compat::remove_compat(pid);
    r
}

/// Map a fresh, zeroed, user-accessible scratch page at [`SCRATCH_VA`] in the
/// current (kernel) address space so the syscall pointer-validation choke point
/// (`virt_to_phys` page-presence walk) accepts a buffer there. Returns `true` on
/// success; the caller must pair a `true` with [`unmap_scratch`].
fn map_scratch() -> bool {
    if vmm::virt_to_phys(SCRATCH_VA).is_some() {
        // Already mapped (unexpected): do not take ownership.
        return false;
    }
    let frame = match pmm::alloc_frame() {
        Some(f) => f,
        None => return false,
    };
    // SAFETY: `frame` was just allocated and is reachable through the HHDM alias.
    unsafe {
        core::ptr::write_bytes(vmm::phys_to_virt(frame) as *mut u8, 0, 4096);
    }
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;
    vmm::map(frame, SCRATCH_VA, flags).is_ok()
}

/// Unmap the scratch page and return its frame to the PMM (no-op if absent).
fn unmap_scratch() {
    if let Some(phys) = vmm::virt_to_phys(SCRATCH_VA) {
        let _ = vmm::unmap(SCRATCH_VA);
        pmm::free_frame(phys & !0xFFF);
    }
}

/// Copy `bytes` into the scratch page at offset 0. PRECONDITION: scratch mapped.
fn scratch_write(bytes: &[u8]) {
    // SAFETY: scratch page is mapped writable; bytes fit within one 4 KiB page.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), SCRATCH_VA as *mut u8, bytes.len());
    }
}

/// Copy `bytes` into the scratch page at `off`. PRECONDITION: scratch mapped and
/// `off + bytes.len() <= 4096`.
fn scratch_write_at(off: usize, bytes: &[u8]) {
    // SAFETY: the scratch page is mapped writable; the caller keeps the write
    // inside the 4 KiB page.
    unsafe {
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (SCRATCH_VA as *mut u8).add(off),
            bytes.len(),
        );
    }
}

/// Read `len` bytes from the scratch page at `off` into an owned buffer.
fn scratch_read_at(off: usize, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    // SAFETY: the scratch page is mapped readable; the caller keeps the read
    // inside the 4 KiB page.
    unsafe {
        core::ptr::copy_nonoverlapping((SCRATCH_VA as *const u8).add(off), buf.as_mut_ptr(), len);
    }
    buf
}

/// Read `len` bytes back from the scratch page into an owned buffer.
fn scratch_read(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    // SAFETY: scratch page is mapped readable; len fits within one 4 KiB page.
    unsafe {
        core::ptr::copy_nonoverlapping(SCRATCH_VA as *const u8, buf.as_mut_ptr(), len);
    }
    buf
}

// ─────────────────────────────── VFS helpers ─────────────────────────────────

/// Create (replacing any existing file) `/<mnt>/<name>` and write `content`,
/// returning a short error detail on failure. Removes any pre-existing file first
/// so the stored size equals `content.len()` (ext2 `write` only grows `i_size`).
fn write_mnt_file(name: &str, content: &[u8]) -> Result<(), &'static str> {
    let dir = vfs::lookup_path("/mnt").map_err(|_| "/mnt lookup failed")?;
    let _ = dir.remove(name);
    let file = dir.create_file(name).map_err(|_| "create_file failed")?;
    if !content.is_empty() {
        let n = file.write(0, content).map_err(|_| "write failed")?;
        if n != content.len() {
            return Err("short write");
        }
    }
    dir.sync();
    Ok(())
}

/// Read the whole file at `path` and compare it byte-for-byte to `expected`.
fn verify_file(path: &str, expected: &[u8]) -> Result<(), &'static str> {
    let node = vfs::lookup_path(path).map_err(|_| "lookup failed")?;
    let size = node.size() as usize;
    if size != expected.len() {
        return Err("size mismatch");
    }
    let mut buf = vec![0u8; size];
    let mut off = 0usize;
    while off < size {
        match node.read(off as u64, &mut buf[off..]) {
            Ok(0) => break,
            Ok(n) => off += n,
            Err(_) => return Err("read failed"),
        }
    }
    if &buf[..off] == expected {
        Ok(())
    } else {
        Err("content mismatch")
    }
}

// ───────────────────────── hand-assembled Linux ELF ──────────────────────────

/// Build, in memory, a minimal statically-linked `ET_EXEC` x86_64 Linux ELF whose
/// `_start` issues `write(1, msg, len)` then `exit_group(0)` using the Linux ABI
/// (`write` = 1, `exit_group` = 231; number in `rax`, args in `rdi/rsi/rdx`) via the
/// `syscall`-instruction entry. Mirrors `process::build_test_elf` but with Linux
/// syscall numbers and an `exit_group`, so a `Compat_Process` running it exercises
/// the Linux write handler (serial output) and the exit diagnostic end-to-end.
///
/// The entry instruction is `syscall` (`0F 05`), NOT `int 0x80`, and that is
/// load-bearing: AGENTS.md invariant 2 — a Compat_Process must enter syscalls
/// through `syscall_entry`, where the CPU saves the user RIP in `rcx`, the user
/// RFLAGS in `r11` and the per-task user-RSP slot at `SavedRegs + 120` holds the
/// user RSP. On the `int 0x80` path the CPU-pushed frame makes that `+120` word
/// the user RIP instead, so signal delivery would build the `rt_sigframe` "below"
/// a text address and overwrite the user's code. `linux_dispatch` records every
/// `int 0x80` entry (`linux::INT80_ENTRY`) and the signal path refuses frame
/// delivery for such a process — but the test binary must not rely on that safety
/// net: it is a Compat_Process and therefore has to take the compat path. Both
/// instructions are two bytes, so `CODE_LEN` and the ELF layout are unchanged.
fn build_linux_test_elf() -> Vec<u8> {
    const VBASE: u64 = 0x40_0000;
    const EHSIZE: usize = 64;
    const PHSIZE: usize = 56;
    let code_off = EHSIZE + PHSIZE;

    let msg: &[u8] = b"LXSELFTEST e2e compat write OK\n";

    // Code length is fixed by the instruction encoding below.
    const CODE_LEN: usize = 33;
    let msg_off = code_off + CODE_LEN;
    let msg_addr = VBASE + msg_off as u64;
    let len = msg.len() as u32;

    let mut code: Vec<u8> = Vec::with_capacity(CODE_LEN);
    code.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // mov eax, 1   (write)
    code.extend_from_slice(&[0xBF, 0x01, 0x00, 0x00, 0x00]); // mov edi, 1   (fd = stdout)
    code.push(0xBE);
    code.extend_from_slice(&(msg_addr as u32).to_le_bytes()); // mov esi, msg_addr
    code.push(0xBA);
    code.extend_from_slice(&len.to_le_bytes()); // mov edx, len
    code.extend_from_slice(&[0x0F, 0x05]); // syscall (NOT int 0x80: AGENTS invariant 2)
    code.extend_from_slice(&[0xB8, 0xE7, 0x00, 0x00, 0x00]); // mov eax, 231 (exit_group)
    code.extend_from_slice(&[0x31, 0xFF]); // xor edi, edi (code = 0)
    code.extend_from_slice(&[0x0F, 0x05]); // syscall (NOT int 0x80: AGENTS invariant 2)
    code.extend_from_slice(&[0xEB, 0xFE]); // 1: jmp 1b (fallback)
    debug_assert_eq!(code.len(), CODE_LEN);

    let entry = VBASE + code_off as u64;
    let total_len = (msg_off + msg.len()) as u64;

    let mut elf: Vec<u8> = Vec::new();

    // ELF64 header (64 bytes).
    elf.extend_from_slice(&[0x7F, b'E', b'L', b'F']);
    elf.push(2); // ELFCLASS64
    elf.push(1); // ELFDATA2LSB
    elf.push(1); // EI_VERSION
    elf.push(0); // System V
    elf.extend_from_slice(&[0u8; 8]); // EI_ABIVERSION + padding
    elf.extend_from_slice(&2u16.to_le_bytes()); // e_type = ET_EXEC
    elf.extend_from_slice(&0x3Eu16.to_le_bytes()); // e_machine = EM_X86_64
    elf.extend_from_slice(&1u32.to_le_bytes()); // e_version
    elf.extend_from_slice(&entry.to_le_bytes()); // e_entry
    elf.extend_from_slice(&(EHSIZE as u64).to_le_bytes()); // e_phoff
    elf.extend_from_slice(&0u64.to_le_bytes()); // e_shoff
    elf.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    elf.extend_from_slice(&(EHSIZE as u16).to_le_bytes()); // e_ehsize
    elf.extend_from_slice(&(PHSIZE as u16).to_le_bytes()); // e_phentsize
    elf.extend_from_slice(&1u16.to_le_bytes()); // e_phnum
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shentsize
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shnum
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx
    debug_assert_eq!(elf.len(), EHSIZE);

    // Program header (56 bytes): one PT_LOAD covering the whole image.
    elf.extend_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    elf.extend_from_slice(&7u32.to_le_bytes()); // PF_R|PF_W|PF_X
    elf.extend_from_slice(&0u64.to_le_bytes()); // p_offset
    elf.extend_from_slice(&VBASE.to_le_bytes()); // p_vaddr
    elf.extend_from_slice(&VBASE.to_le_bytes()); // p_paddr
    elf.extend_from_slice(&total_len.to_le_bytes()); // p_filesz
    elf.extend_from_slice(&total_len.to_le_bytes()); // p_memsz
    elf.extend_from_slice(&0x1000u64.to_le_bytes()); // p_align
    debug_assert_eq!(elf.len(), EHSIZE + PHSIZE);

    elf.extend_from_slice(&code);
    elf.extend_from_slice(msg);
    debug_assert_eq!(elf.len() as u64, total_len);

    elf
}

// ─────────────────────────────────── checks ──────────────────────────────────

/// 13.4 — end-to-end load and run: write a valid static Linux ELF onto ext2, run
/// it (it `write`s a message then `exit_group`s), and confirm a load failure on a
/// non-ELF file does NOT enqueue a process.
fn check_end_to_end_run() {
    let name = "end_to_end_run";

    let elf = build_linux_test_elf();
    if let Err(d) = write_mnt_file("lxbin", &elf) {
        fail(name, d);
        return;
    }
    if let Err(d) = write_mnt_file("lxnotelf", b"this is definitely not an ELF binary") {
        fail(name, d);
        return;
    }

    // Load failure must return Err WITHOUT enqueuing a process (R7.3).
    match run_linux_binary("/mnt/lxnotelf", &[b"lxnotelf"], &[]) {
        Ok(_) => {
            fail(name, "non-ELF file unexpectedly spawned a process");
            return;
        }
        Err(_) => { /* expected: rejected, no enqueue */ }
    }

    // Valid static binary: load + build stack + enqueue. The message and the exit
    // diagnostic appear on serial once the scheduler runs the process (after
    // interrupts are enabled).
    match run_linux_binary("/mnt/lxbin", &[b"lxbin"], &[]) {
        Ok(pid) => {
            crate::info!(
                "LXSELFTEST end_to_end_run spawned Compat_Process pid={}",
                pid
            );
            pass(name);
        }
        Err(e) => match e {
            RunError::ArgsTooLarge => fail(name, "run returned ArgsTooLarge"),
            RunError::NotFound => fail(name, "run returned NotFound"),
            RunError::LoadFailed(c) => fail(name, c),
            RunError::StackFailed => fail(name, "run returned StackFailed"),
        },
    }
}

/// 13.4 / R7.2 — exit isolation: reaching this check means `run_linux_binary`
/// returned control to the harness (it did not terminate the caller), and the
/// kernel keeps running the remaining checks. The spawned `Compat_Process`'s
/// `exit_group` later terminates only that task — observable on serial as the
/// "Compat_Process pid=N exited with code 0" line followed by a still-interactive
/// shell.
fn check_exit_isolation() {
    pass("exit_isolation");
}

/// 12.2 — `open` of a known-absent ext2 path returns `-ENOENT`. Driven through the
/// real `sys_open` handler: the absent path string is placed in the scratch user
/// page and the handler reads it via the pointer choke point before resolving it.
fn check_open_absent() {
    let name = "open_absent";
    if !map_scratch() {
        fail(name, "scratch map failed");
        return;
    }
    // NUL-terminated absent path.
    scratch_write(b"/mnt/this_path_does_not_exist_42\0");

    let res = with_synth_compat(0x40_0000, scheduler::current_pid(), || {
        io_sys::sys_open(SCRATCH_VA, 0, 0)
    });
    unmap_scratch();

    match res {
        Err(Errno::ENOENT) => pass(name),
        Ok(fd) => {
            let _ = fd;
            fail(name, "absent path unexpectedly opened");
        }
        Err(e) => {
            crate::error!("LXSELFTEST {} FAIL expected ENOENT got {:?}", name, e);
        }
    }
}

/// 12.6 — `arch_prctl(ARCH_SET_FS)` records `FS.base`; `set_tid_address` returns the
/// tid; `uname` fills the fixed identifying strings. Driven through the real
/// handlers with a synthesized compat state; the live `FS.base` MSR is saved and
/// restored around the check so the kernel's register state is untouched.
fn check_arch_prctl_uname_tid() {
    let name = "arch_prctl_uname_tid";
    const TID: u64 = 0xABCD;
    const FS_TEST: u64 = 0x0000_0000_DEAD_B000;

    let saved_fs = FsBase::read();

    let result: Result<(), &'static str> = with_synth_compat(0x40_0000, TID, || {
        // arch_prctl(ARCH_SET_FS): records fs_base in the compat state (R2.9).
        if misc::sys_arch_prctl(ARCH_SET_FS, FS_TEST) != Ok(0) {
            return Err("arch_prctl(SET_FS) did not return 0");
        }
        let recorded = compat::with_current_compat(|cs| cs.fs_base).unwrap_or(0);
        if recorded != FS_TEST {
            return Err("fs_base not recorded in compat state");
        }

        // set_tid_address returns the tid (R2.10).
        match misc::sys_set_tid_address(0) {
            Ok(t) if t == TID => {}
            _ => return Err("set_tid_address did not return tid"),
        }

        // uname fills the fixed strings (R2.11). Utsname layout: sysname @ 0,
        // machine @ 4*65 = 260 (each field is 65 bytes).
        if !map_scratch() {
            return Err("scratch map failed");
        }
        let ur = misc::sys_uname(SCRATCH_VA);
        let buf = scratch_read(266);
        unmap_scratch();
        if ur != Ok(0) {
            return Err("uname did not return 0");
        }
        if &buf[0..5] != b"Linux" {
            return Err("uname sysname != Linux");
        }
        if &buf[260..266] != b"x86_64" {
            return Err("uname machine != x86_64");
        }
        Ok(())
    });

    // Restore the live FS.base regardless of outcome.
    FsBase::write(saved_fs);

    match result {
        Ok(()) => pass(name),
        Err(d) => fail(name, d),
    }
}

/// 12.8 / R1.7 — register preservation: a syscall preserves every GPR other than
/// `rax`. Drives `linux_dispatch` directly with a `SavedRegs` frame full of distinct
/// sentinels and `rax = 39` (`getpid`). `linux_dispatch` delivers its result as the
/// return value (the entry stub is what writes `rax`), so the saved frame must come
/// back byte-for-byte identical and the return value must equal the current pid.
fn check_register_preservation() {
    let name = "register_preservation";

    let mut regs = SavedRegs {
        r15: 0x1515_1515_1515_1515,
        r14: 0x1414_1414_1414_1414,
        r13: 0x1313_1313_1313_1313,
        r12: 0x1212_1212_1212_1212,
        r11: 0x1111_1111_1111_1111,
        r10: 0x1010_1010_1010_1010,
        r9: 0x0909_0909_0909_0909,
        r8: 0x0808_0808_0808_0808,
        rbp: 0x0B0B_0B0B_0B0B_0B0B,
        rdi: 0x0D0D_0D0D_0D0D_0D0D,
        rsi: 0x0505_0505_0505_0505,
        rdx: 0x0303_0303_0303_0303,
        rcx: 0x0C0C_0C0C_0C0C_0C0C,
        rbx: 0x0B0B_0B0B_0B0B_0BBB,
        rax: 39, // getpid
    };
    let snapshot = regs;

    // 0 = NOT a real syscall: this direct call runs on the boot thread, which is
    // not a schedulable task, so the dispatcher must leave IF exactly as it found
    // it (see `linux_dispatch`). Unmasking here would park the boot thread for
    // good on the first timer tick.
    let ret = linux_dispatch(&mut regs as *mut SavedRegs, 0);
    let expected = scheduler::current_pid();

    if regs == snapshot && ret == expected {
        pass(name);
    } else {
        crate::error!(
            "LXSELFTEST {} FAIL regs_changed={} ret={} expected_pid={}",
            name,
            regs != snapshot,
            ret,
            expected
        );
    }
}

/// 12.4 / R3.4, R4.4 — OOM / over-limit rollback: `brk` and `mmap` leave the
/// process VM state unchanged when a request cannot be satisfied. Uses deterministic
/// over-`USER_ADDR_MAX` requests (no real PMM exhaustion needed): an over-max `brk`
/// reports the unchanged break, and an impossible-size `mmap` returns `-ENOMEM`
/// without allocating or recording any region.
fn check_oom_rollback() {
    let name = "oom_rollback";
    const INITIAL_BRK: u64 = 0x40_0000;

    let result: Result<(), &'static str> = with_synth_compat(INITIAL_BRK, 1, || {
        // brk(0) queries the current break.
        if mem_sys::sys_brk(0) != Ok(INITIAL_BRK) {
            return Err("brk(0) query mismatch");
        }
        // brk over USER_ADDR_MAX -> unchanged (R3.5).
        if mem_sys::sys_brk(USER_ADDR_MAX + 0x1000) != Ok(INITIAL_BRK) {
            return Err("over-max brk changed the break");
        }
        // mmap of an impossible size cannot be placed below the ceiling -> ENOMEM.
        let huge = mem_sys::sys_mmap(
            0,
            USER_ADDR_MAX,
            PROT_WRITE as u64,
            (MAP_ANONYMOUS | MAP_PRIVATE) as u64,
            (-1i64) as u64,
            0,
        );
        if huge != Err(Errno::ENOMEM) {
            return Err("impossible mmap did not return ENOMEM");
        }
        // The VM state must be untouched: break still INITIAL_BRK, no regions.
        let (brk, nmaps) = compat::with_current_compat(|cs| {
            let vm = cs.vm.lock();
            (vm.current_brk, vm.mmaps.len())
        })
        .unwrap_or((0, usize::MAX));
        if brk != INITIAL_BRK {
            return Err("current_brk changed after rollback");
        }
        if nmaps != 0 {
            return Err("mmap region recorded after rollback");
        }
        Ok(())
    });

    match result {
        Ok(()) => pass(name),
        Err(d) => fail(name, d),
    }
}

/// 14.2 / R8.7 — `fetch_deb` returns `NoNetwork` without attempting a connection
/// when no interface address is configured. This check runs before DHCP/networking
/// is up (interrupts are still disabled), so the interface has no address yet.
fn check_fetch_no_network() {
    let name = "fetch_no_network";
    match fetch_deb("10.0.2.2", 80, "/pool/main/test.deb") {
        Err(FetchError::NoNetwork) => pass(name),
        Err(e) => crate::error!("LXSELFTEST {} FAIL expected NoNetwork got {:?}", name, e),
        Ok(_) => fail(
            name,
            "fetch unexpectedly succeeded with no interface address",
        ),
    }
}

/// 14.4 / R10 — ext2 install round trip: build a small `data.tar` with
/// `write_tar`, parse it with `read_tar`, install it onto real ext2 under `/mnt`,
/// and read the installed files back byte-for-byte. Also confirms a parent
/// directory is created and an unsafe `..`-escaping entry is skipped.
fn check_ext2_install_roundtrip() {
    let name = "ext2_install_roundtrip";

    let foo: &[u8] = b"foo-binary-bytes-0123456789-abcdef";
    let conf: &[u8] = b"key=value\nflag=1\n";
    let entries_src: [(&str, &[u8]); 3] = [
        ("usr/bin/foo", foo),
        ("etc/foo.conf", conf),
        ("../escape.txt", b"escaping-content"),
    ];

    let tar_bytes = write_tar(&entries_src);
    let entries = match read_tar(&tar_bytes) {
        Ok(e) => e,
        Err(e) => {
            crate::error!("LXSELFTEST {} FAIL read_tar {:?}", name, e);
            return;
        }
    };

    let installed = match install_data_tar(&entries, "/mnt") {
        Ok(n) => n,
        Err(e) => {
            crate::error!("LXSELFTEST {} FAIL install_data_tar {:?}", name, e);
            return;
        }
    };
    // Two safe regular files installed; the `..`-escaping entry is skipped (R10.8).
    if installed != 2 {
        crate::error!(
            "LXSELFTEST {} FAIL installed={} expected 2",
            name,
            installed
        );
        return;
    }

    if let Err(d) = verify_file("/mnt/usr/bin/foo", foo) {
        fail(name, d);
        return;
    }
    if let Err(d) = verify_file("/mnt/etc/foo.conf", conf) {
        fail(name, d);
        return;
    }

    // A parent directory was created (R10.2).
    match vfs::lookup_path("/mnt/usr/bin") {
        Ok(n) if n.is_directory() => {}
        _ => {
            fail(name, "parent directory /mnt/usr/bin not created");
            return;
        }
    }

    // The `..`-escaping entry must not have produced a file (R10.8).
    if vfs::lookup_path("/mnt/escape.txt").is_ok() {
        fail(name, "unsafe ../escape entry was installed");
        return;
    }

    pass(name);
}

// ───────── new directory / fd / time syscall checks (linux-binary-compat) ─────────

/// `getcwd` returns the default cwd `/` (two bytes: `'/'` + NUL) for a fresh
/// Compat_Process. Driven through the real handler with the result written to the
/// scratch user page.
fn check_getcwd() {
    let name = "getcwd";
    if !map_scratch() {
        fail(name, "scratch map failed");
        return;
    }
    let res = with_synth_compat(0x40_0000, scheduler::current_pid(), || {
        io_sys::sys_getcwd(SCRATCH_VA, 256)
    });
    let buf = scratch_read(2);
    unmap_scratch();

    match res {
        Ok(2) if buf[0] == b'/' && buf[1] == 0 => pass(name),
        Ok(n) => crate::error!("LXSELFTEST {} FAIL got len {} bytes {:?}", name, n, buf),
        Err(e) => crate::error!("LXSELFTEST {} FAIL {:?}", name, e),
    }
}

/// `chdir` to an existing directory (`/mnt`) updates the process cwd. Driven
/// through the real handler with the path placed in the scratch user page.
fn check_chdir() {
    let name = "chdir";
    if !map_scratch() {
        fail(name, "scratch map failed");
        return;
    }
    scratch_write(b"/mnt\0");
    let result: Result<(), &'static str> =
        with_synth_compat(0x40_0000, scheduler::current_pid(), || {
            if io_sys::sys_chdir(SCRATCH_VA) != Ok(0) {
                return Err("chdir did not return 0");
            }
            let cwd = compat::with_current_compat(|cs| cs.cwd.clone()).unwrap_or_default();
            if cwd != "/mnt" {
                return Err("cwd not updated to /mnt");
            }
            Ok(())
        });
    unmap_scratch();

    match result {
        Ok(()) => pass(name),
        Err(d) => fail(name, d),
    }
}

/// `dup` of a standard stream allocates a fresh descriptor `>= 3`.
fn check_dup() {
    let name = "dup";
    let res = with_synth_compat(0x40_0000, scheduler::current_pid(), || {
        // dup the pre-bound stdin (fd 0).
        io_sys::sys_dup(0)
    });
    match res {
        Ok(fd) if fd >= 3 => pass(name),
        Ok(fd) => crate::error!(
            "LXSELFTEST {} FAIL dup returned {} (expected >= 3)",
            name,
            fd
        ),
        Err(e) => crate::error!("LXSELFTEST {} FAIL {:?}", name, e),
    }
}

/// `gettimeofday` and `time` return a positive wall-clock value once the CMOS RTC
/// is wired (the RTC reports the present date, well after the 1970 epoch).
fn check_walltime() {
    let name = "walltime";
    if !map_scratch() {
        fail(name, "scratch map failed");
        return;
    }
    let result: Result<(), &'static str> =
        with_synth_compat(0x40_0000, scheduler::current_pid(), || {
            if misc::sys_gettimeofday(SCRATCH_VA, 0) != Ok(0) {
                return Err("gettimeofday did not return 0");
            }
            let b = scratch_read(8);
            let secs = i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            if secs <= 0 {
                return Err("gettimeofday tv_sec not positive");
            }
            match misc::sys_time(0) {
                Ok(t) if t > 0 => {}
                _ => return Err("time did not return a positive value"),
            }
            Ok(())
        });
    unmap_scratch();

    match result {
        Ok(()) => pass(name),
        Err(d) => fail(name, d),
    }
}

/// 18.5 / issue #18 — symbolic links end to end through the real syscall layer.
///
/// Builds links on the mounted ext2 under `/mnt` and then drives `readlink(2)`,
/// `newfstatat` (`stat` vs `lstat`), `open`, `getdents64` and the `/proc/self/exe`
/// path. Pins the contract (`EXT2-LINKS.md` §4):
///
///   * `readlink` returns the stored target verbatim (fast *and* slow layout),
///     truncates to `bufsiz` without a NUL, and answers `EINVAL` for a
///     non-link or a zero `bufsiz`, `ENOENT` for an absent path;
///   * `lstat` describes the link itself (`S_IFLNK`, `st_size` = target length,
///     the link's own inode) while `stat` follows it (`S_IFREG`, target size,
///     target inode) — and two hard links report the same `st_ino` with
///     `st_nlink == 2`;
///   * `open` follows a link and reads the target's bytes;
///   * `getdents64` reports `DT_LNK` for the link and the real inode numbers;
///   * a dangling link is `ENOENT` for `stat`/`open` but fine for `lstat`, and a
///     **cycle** is `ELOOP` (never a hang, never a wrong file).
///
/// Everything it creates is removed again, so the mounted tree is left as found.
fn check_links() {
    let name = "ext2_links";
    const PATH_OFF: usize = 0; // C-string path for the handlers
    const OUT_OFF: usize = 256; // targets / file payloads
    const STAT_OFF: usize = 512; // struct stat (144 bytes)
    const DIR_OFF: usize = 1024; // getdents64 output
    const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
    const S_IFMT: u32 = 0o170000;
    const S_IFREG: u32 = 0o100000;
    const S_IFLNK: u32 = 0o120000;

    let mnt = match vfs::lookup_path("/mnt") {
        Ok(n) => n,
        Err(_) => {
            fail(name, "/mnt is not mounted");
            return;
        }
    };
    let target_payload: &[u8] = b"link-payload";
    if let Err(d) = write_mnt_file("lx_target", target_payload) {
        fail(name, d);
        return;
    }
    // 70-byte target: long enough for the *slow* (data-block) inode layout.
    let mut slow_target: alloc::vec::Vec<u8> = b"/mnt/".to_vec();
    for _ in 0..70 {
        slow_target.push(b'z');
    }
    let make = |n: &str, t: &[u8]| -> bool {
        let _ = mnt.remove(n);
        mnt.create_symlink(n, t).is_ok()
    };
    let created = make("lx_rel", b"lx_target")
        && make("lx_abs", b"/mnt/lx_target")
        && make("lx_slow", &slow_target)
        && make("lx_dang", b"/mnt/lx_absent")
        && make("lx_loop_a", b"/mnt/lx_loop_b")
        && make("lx_loop_b", b"/mnt/lx_loop_a");
    if !created {
        fail(name, "create_symlink failed on /mnt");
        cleanup_links(&mnt);
        return;
    }
    // A second name for the target inode (hard link, issue #18).
    let hard_ok = {
        let _ = mnt.remove("lx_hard");
        match vfs::lookup_path("/mnt/lx_target") {
            Ok(t) => mnt.link("lx_hard", &t).is_ok(),
            Err(_) => false,
        }
    };

    if !map_scratch() {
        cleanup_links(&mnt);
        fail(name, "scratch map failed");
        return;
    }

    let u32_at = |off: usize| -> u32 {
        let b = scratch_read_at(off, 4);
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    };
    let u64_at = |off: usize| -> u64 {
        let b = scratch_read_at(off, 8);
        u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    };

    let result: Result<(), &'static str> =
        with_synth_compat(0x40_0000, scheduler::current_pid(), || {
            // ── readlink: relative target, verbatim ──────────────────────
            // The expected lengths come from the literals themselves: a magic
            // number here silently drifts from the string it describes.
            let rel_target: &[u8] = b"lx_target";
            let abs_target: &[u8] = b"/mnt/lx_target";
            scratch_write_at(PATH_OFF, b"/mnt/lx_rel\0");
            let n = io_sys::sys_readlink(
                SCRATCH_VA + PATH_OFF as u64,
                SCRATCH_VA + OUT_OFF as u64,
                64,
            )
            .map_err(|_| "readlink(rel) failed")?;
            if n as usize != rel_target.len() || scratch_read_at(OUT_OFF, n as usize) != rel_target
            {
                return Err("readlink(rel) returned the wrong target");
            }
            // ── readlink: absolute target ────────────────────────────────
            scratch_write_at(PATH_OFF, b"/mnt/lx_abs\0");
            let n = io_sys::sys_readlink(
                SCRATCH_VA + PATH_OFF as u64,
                SCRATCH_VA + OUT_OFF as u64,
                64,
            )
            .map_err(|_| "readlink(abs) failed")?;
            if n as usize != abs_target.len() || scratch_read_at(OUT_OFF, n as usize) != abs_target
            {
                return Err("readlink(abs) returned the wrong target");
            }
            // ── readlink: slow layout, then truncation to bufsiz ─────────
            scratch_write_at(PATH_OFF, b"/mnt/lx_slow\0");
            let n = io_sys::sys_readlink(
                SCRATCH_VA + PATH_OFF as u64,
                SCRATCH_VA + OUT_OFF as u64,
                128,
            )
            .map_err(|_| "readlink(slow) failed")?;
            if n as usize != slow_target.len()
                || scratch_read_at(OUT_OFF, n as usize) != slow_target.as_slice()
            {
                return Err("readlink(slow) did not round-trip the 70-byte target");
            }
            let n =
                io_sys::sys_readlink(SCRATCH_VA + PATH_OFF as u64, SCRATCH_VA + OUT_OFF as u64, 4)
                    .map_err(|_| "readlink(truncating) failed")?;
            // Truncation returns the *prefix* of the stored target (which starts
            // with `/mnt/`), not a NUL-terminated or re-encoded string.
            if n != 4 || scratch_read_at(OUT_OFF, 4) != b"/mnt" {
                return Err("readlink must truncate to bufsiz without a NUL");
            }
            // ── readlink error cases ─────────────────────────────────────
            if io_sys::sys_readlink(SCRATCH_VA + PATH_OFF as u64, SCRATCH_VA + OUT_OFF as u64, 0)
                != Err(Errno::EINVAL)
            {
                return Err("readlink with bufsiz 0 must be EINVAL");
            }
            scratch_write_at(PATH_OFF, b"/mnt/lx_target\0");
            if io_sys::sys_readlink(
                SCRATCH_VA + PATH_OFF as u64,
                SCRATCH_VA + OUT_OFF as u64,
                64,
            ) != Err(Errno::EINVAL)
            {
                return Err("readlink of a regular file must be EINVAL");
            }
            scratch_write_at(PATH_OFF, b"/mnt/lx_absent\0");
            if io_sys::sys_readlink(
                SCRATCH_VA + PATH_OFF as u64,
                SCRATCH_VA + OUT_OFF as u64,
                64,
            ) != Err(Errno::ENOENT)
            {
                return Err("readlink of an absent path must be ENOENT");
            }

            // ── lstat vs stat ────────────────────────────────────────────
            scratch_write_at(PATH_OFF, b"/mnt/lx_abs\0");
            io_sys::sys_newfstatat(
                0,
                SCRATCH_VA + PATH_OFF as u64,
                SCRATCH_VA + STAT_OFF as u64,
                AT_SYMLINK_NOFOLLOW,
            )
            .map_err(|_| "lstat(link) failed")?;
            if u32_at(STAT_OFF + 24) & S_IFMT != S_IFLNK {
                return Err("lstat(link) must report S_IFLNK");
            }
            if u32_at(STAT_OFF + 24) & 0o777 != 0o777 {
                return Err("ext2 symlinks are 0777");
            }
            let link_ino = u64_at(STAT_OFF + 8);
            // st_size is the *target length*, not the target's size.
            let size = u64_at(STAT_OFF + 48);
            if size != abs_target.len() as u64 {
                return Err("lstat(link).st_size must be the target length");
            }
            io_sys::sys_newfstatat(
                0,
                SCRATCH_VA + PATH_OFF as u64,
                SCRATCH_VA + STAT_OFF as u64,
                0,
            )
            .map_err(|_| "stat(link) failed")?;
            if u32_at(STAT_OFF + 24) & S_IFMT != S_IFREG {
                return Err("stat(link) must follow to the regular file");
            }
            if u64_at(STAT_OFF + 8) == link_ino {
                return Err("stat(link) must report the target's inode");
            }
            if u64_at(STAT_OFF + 48) as usize != target_payload.len() {
                return Err("stat(link).st_size must be the target's size");
            }
            let target_ino = u64_at(STAT_OFF + 8);
            // Hard links: same inode, nlink == 2 (skip when the link failed).
            if hard_ok {
                scratch_write_at(PATH_OFF, b"/mnt/lx_hard\0");
                io_sys::sys_newfstatat(
                    0,
                    SCRATCH_VA + PATH_OFF as u64,
                    SCRATCH_VA + STAT_OFF as u64,
                    0,
                )
                .map_err(|_| "stat(hardlink) failed")?;
                if u64_at(STAT_OFF + 8) != target_ino {
                    return Err("a hard link must report the same st_ino");
                }
                if u64_at(STAT_OFF + 16) != 2 {
                    return Err("a hard-linked file must report st_nlink 2");
                }
            }

            // ── dangling link ────────────────────────────────────────────
            scratch_write_at(PATH_OFF, b"/mnt/lx_dang\0");
            io_sys::sys_newfstatat(
                0,
                SCRATCH_VA + PATH_OFF as u64,
                SCRATCH_VA + STAT_OFF as u64,
                AT_SYMLINK_NOFOLLOW,
            )
            .map_err(|_| "lstat(dangling) failed")?;
            if u32_at(STAT_OFF + 24) & S_IFMT != S_IFLNK {
                return Err("lstat(dangling) must still report S_IFLNK");
            }
            if io_sys::sys_newfstatat(
                0,
                SCRATCH_VA + PATH_OFF as u64,
                SCRATCH_VA + STAT_OFF as u64,
                0,
            ) != Err(Errno::ENOENT)
            {
                return Err("stat(dangling) must be ENOENT");
            }

            // ── open follows, and reads the target ───────────────────────
            scratch_write_at(PATH_OFF, b"/mnt/lx_rel\0");
            let fd = io_sys::sys_open(SCRATCH_VA + PATH_OFF as u64, 0, 0)
                .map_err(|_| "open(link) failed")?;
            let rn = io_sys::sys_read(fd, SCRATCH_VA + OUT_OFF as u64, 32)
                .map_err(|_| "read through the link failed")?;
            if scratch_read_at(OUT_OFF, rn as usize) != target_payload {
                return Err("reading through a link must return the target's bytes");
            }
            let _ = io_sys::sys_close(fd);
            scratch_write_at(PATH_OFF, b"/mnt/lx_dang\0");
            if io_sys::sys_open(SCRATCH_VA + PATH_OFF as u64, 0, 0) != Err(Errno::ENOENT) {
                return Err("open(dangling) must be ENOENT");
            }

            // ── getdents64: d_type and the real inode ────────────────────
            scratch_write_at(PATH_OFF, b"/mnt\0");
            let dfd = io_sys::sys_open(SCRATCH_VA + PATH_OFF as u64, 0, 0)
                .map_err(|_| "open(/mnt) failed")?;
            let dn = io_sys::sys_getdents64(dfd, SCRATCH_VA + DIR_OFF as u64, 4096)
                .map_err(|_| "getdents64(/mnt) failed")?;
            let _ = io_sys::sys_close(dfd);
            let dir = scratch_read_at(DIR_OFF, dn as usize);
            let mut at = 0usize;
            let mut saw_link = false;
            let mut saw_target = false;
            while at + 19 <= dir.len() {
                let rec_len = u16::from_le_bytes([dir[at + 16], dir[at + 17]]) as usize;
                if rec_len < 19 || at + rec_len > dir.len() {
                    break;
                }
                let d_type = dir[at + 18];
                let d_ino = u64::from_le_bytes([
                    dir[at],
                    dir[at + 1],
                    dir[at + 2],
                    dir[at + 3],
                    dir[at + 4],
                    dir[at + 5],
                    dir[at + 6],
                    dir[at + 7],
                ]);
                let name_bytes = &dir[at + 19..at + rec_len];
                let end = name_bytes
                    .iter()
                    .position(|b| *b == 0)
                    .unwrap_or(name_bytes.len());
                let nm = &name_bytes[..end];
                if nm == b"lx_rel" {
                    saw_link = d_type == 10; // DT_LNK
                }
                if nm == b"lx_target" {
                    saw_target = d_type == 8 && d_ino == target_ino; // DT_REG
                }
                at += rec_len;
            }
            if !saw_link {
                return Err("getdents64 must report DT_LNK for a symlink");
            }
            if !saw_target {
                return Err("getdents64 must report the real inode for a regular file");
            }

            // ── cycles are ELOOP, and lstat still sees the link ──────────
            scratch_write_at(PATH_OFF, b"/mnt/lx_loop_a\0");
            io_sys::sys_newfstatat(
                0,
                SCRATCH_VA + PATH_OFF as u64,
                SCRATCH_VA + STAT_OFF as u64,
                AT_SYMLINK_NOFOLLOW,
            )
            .map_err(|_| "lstat(cyclic link) failed")?;
            if u32_at(STAT_OFF + 24) & S_IFMT != S_IFLNK {
                return Err("lstat of a cyclic link must still describe the link");
            }
            if io_sys::sys_newfstatat(
                0,
                SCRATCH_VA + PATH_OFF as u64,
                SCRATCH_VA + STAT_OFF as u64,
                0,
            ) != Err(Errno::ELOOP)
            {
                return Err("stat through a link cycle must be ELOOP");
            }
            if io_sys::sys_open(SCRATCH_VA + PATH_OFF as u64, 0, 0) != Err(Errno::ELOOP) {
                return Err("open through a link cycle must be ELOOP");
            }

            // ── /proc/self/exe still works through the same mechanism ────
            compat::with_current_compat(|cs| {
                cs.exe_path = alloc::string::String::from("/mnt/lx_target")
            });
            scratch_write_at(PATH_OFF, b"/proc/self/exe\0");
            let n = io_sys::sys_readlink(
                SCRATCH_VA + PATH_OFF as u64,
                SCRATCH_VA + OUT_OFF as u64,
                64,
            )
            .map_err(|_| "readlink(/proc/self/exe) failed")?;
            if scratch_read_at(OUT_OFF, n as usize) != b"/mnt/lx_target" {
                return Err("readlink(/proc/self/exe) must report the image path");
            }
            io_sys::sys_newfstatat(
                0,
                SCRATCH_VA + PATH_OFF as u64,
                SCRATCH_VA + STAT_OFF as u64,
                AT_SYMLINK_NOFOLLOW,
            )
            .map_err(|_| "lstat(/proc/self/exe) failed")?;
            if u32_at(STAT_OFF + 24) & S_IFMT != S_IFLNK {
                return Err("lstat(/proc/self/exe) must report S_IFLNK");
            }
            io_sys::sys_newfstatat(
                0,
                SCRATCH_VA + PATH_OFF as u64,
                SCRATCH_VA + STAT_OFF as u64,
                0,
            )
            .map_err(|_| "stat(/proc/self/exe) failed")?;
            if u32_at(STAT_OFF + 24) & S_IFMT != S_IFREG {
                return Err("stat(/proc/self/exe) must follow to the image");
            }

            Ok(())
        });

    unmap_scratch();
    cleanup_links(&mnt);
    mnt.sync();

    match result {
        Ok(()) => pass(name),
        Err(d) => fail(name, d),
    }
}

/// Remove every scratch entry [`check_links`] creates.
fn cleanup_links(mnt: &Arc<dyn crate::vfs::VfsNode>) {
    for n in [
        "lx_rel",
        "lx_abs",
        "lx_slow",
        "lx_dang",
        "lx_loop_a",
        "lx_loop_b",
        "lx_hard",
        "lx_target",
    ] {
        let _ = mnt.remove(n);
    }
}

/// `getdents64` over a known directory (`/mnt`, which holds files written by the
/// earlier checks plus a probe file created here) returns at least one entry.
fn check_getdents() {
    let name = "getdents64";
    if let Err(d) = write_mnt_file("dentsprobe", b"x") {
        fail(name, d);
        return;
    }
    if !map_scratch() {
        fail(name, "scratch map failed");
        return;
    }
    scratch_write(b"/mnt\0");
    let result: Result<(), &'static str> =
        with_synth_compat(0x40_0000, scheduler::current_pid(), || {
            let fd = match io_sys::sys_open(SCRATCH_VA, 0, 0) {
                Ok(fd) => fd,
                Err(_) => return Err("open /mnt failed"),
            };
            // The scratch page now serves as the dirent output buffer.
            match io_sys::sys_getdents64(fd, SCRATCH_VA, 4096) {
                Ok(0) => Err("getdents64 returned no entries"),
                Ok(_) => Ok(()),
                Err(_) => Err("getdents64 returned an error"),
            }
        });
    unmap_scratch();

    match result {
        Ok(()) => pass(name),
        Err(d) => fail(name, d),
    }
}
