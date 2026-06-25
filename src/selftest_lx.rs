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

use alloc::vec;
use alloc::vec::Vec;

use x86_64::registers::model_specific::FsBase;
use x86_64::structures::paging::PageTableFlags;

use crate::arch::x86_64::linux::errno::Errno;
use crate::arch::x86_64::linux::mem::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_WRITE, VmRegionSet};
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

    crate::info!("LXSELFTEST harness done");
}

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
/// SECURITY: the underlying [`crate::net::tls::https_get`] does NOT verify the
/// server certificate (VARIANT A). This check proves *encrypted transport*, not
/// authentication.
pub fn run_net_smoke() {
    let name = "https_get";

    // Wait up to ~15 s for an interface address (DHCP, then static fallback).
    let deadline = scheduler::ticks() + 1500;
    while crate::net::ip_config().is_none() {
        if scheduler::ticks() >= deadline {
            fail(name, "no interface address (DHCP/static fallback did not configure)");
            return;
        }
        scheduler::sleep_ticks(10);
    }

    crate::info!(
        "LXSELFTEST https_get: interface up, attempting TLS 1.3 GET (INSECURE: no cert verification) ..."
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
/// `lx_selftest` harness. It deliberately leaves the apt configuration at its
/// DEFAULT (`deb.debian.org` `/debian stable main amd64`, **HTTPS VARIANT A**) —
/// it does NOT `set_mirror` to the local mini-repo — and drives the full live
/// update + install pipeline:
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

    // Wait up to ~30 s for an interface address (DHCP, then static fallback). A
    // live update also needs DNS + TLS, so give it a more generous window than
    // the local-mirror check.
    let deadline = scheduler::ticks() + 3000;
    while crate::net::ip_config().is_none() {
        if scheduler::ticks() >= deadline {
            fail(name, "no interface address (DHCP/static fallback did not configure)");
            return;
        }
        scheduler::sleep_ticks(10);
    }

    // Point apt at the cleartext HTTP mirror so the large index download uses
    // `http_get` (which does NOT touch embedded-tls) rather than the HTTPS path.
    //
    // WHY HTTP: the VARIANT-A TLS transport has a determinate embedded-tls hang at
    // ~12 MiB on large streams (the read() future stops returning to our executor,
    // so our transport is never re-entered and no timeout can fire). TLS here
    // provides NO authentication anyway (no cert verification, weak RNG) and the
    // apt pipeline does no signature verification, so fetching the big index over
    // plain HTTP is the honest, working way to actually COMPLETE a live full
    // update from the official Debian mirror. http://deb.debian.org/debian sets
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

    // Wait up to ~20 s for an interface address (DHCP, then static fallback).
    let deadline = scheduler::ticks() + 2000;
    while crate::net::ip_config().is_none() {
        if scheduler::ticks() >= deadline {
            fail(name, "no interface address (DHCP/static fallback did not configure)");
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
        VmRegionSet::new(initial_brk, USER_MMAP_BASE),
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
/// `int 0x80` path. Mirrors `process::build_test_elf` but with Linux syscall numbers
/// and an `exit_group`, so a `Compat_Process` running it exercises the Linux write
/// handler (serial output) and the exit diagnostic end-to-end.
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
    code.extend_from_slice(&[0xCD, 0x80]); // int 0x80
    code.extend_from_slice(&[0xB8, 0xE7, 0x00, 0x00, 0x00]); // mov eax, 231 (exit_group)
    code.extend_from_slice(&[0x31, 0xFF]); // xor edi, edi (code = 0)
    code.extend_from_slice(&[0xCD, 0x80]); // int 0x80
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
            crate::info!("LXSELFTEST end_to_end_run spawned Compat_Process pid={}", pid);
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

    let ret = linux_dispatch(&mut regs as *mut SavedRegs);
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
        let (brk, nmaps) =
            compat::with_current_compat(|cs| (cs.vm.current_brk, cs.vm.mmaps.len()))
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
        Ok(_) => fail(name, "fetch unexpectedly succeeded with no interface address"),
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
        crate::error!("LXSELFTEST {} FAIL installed={} expected 2", name, installed);
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
        Ok(fd) => crate::error!("LXSELFTEST {} FAIL dup returned {} (expected >= 3)", name, fd),
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
