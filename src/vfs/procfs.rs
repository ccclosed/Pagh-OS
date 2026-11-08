//! Synthetic `/proc` (issue #11; contract: `docs/procfs.md`).
//!
//! `/proc` is a VFS subtree whose files have no backing store: their text is
//! rendered from kernel state when a program reads them. The design decisions
//! that matter are all in `docs/procfs.md`; the short version:
//!
//!   * **Lazy, per-instance content.** Nothing is rendered at mount time. A node
//!     instance is created by `lookup()`/`open()` and renders its text on the
//!     first `size()`/`read()`. A read from offset 0 re-renders, so a program that
//!     samples `/proc/uptime` or `/proc/meminfo` sees current values, while a
//!     multi-chunk reader of one file keeps a single render and therefore never
//!     splices two different renders together (`docs/procfs.md` §3.2).
//!   * **`size()` is the rendered length**, not Linux's zero: this kernel's read
//!     path derives EOF from `size()` (`plan_read`), so a zero-sized procfs file
//!     would read as instantly empty (DEVIATION-1).
//!   * **Lock order is state-then-node.** Rendering (`COMPAT_STATES`, PMM,
//!     scheduler) happens before the node's own spinlock is taken; the node lock
//!     is never held across a `with_pid_compat` call.
//!   * **`/proc/self` is invisible to a task without `CompatState`** (a kernel
//!     thread, the boot selftest): `lookup` returns `NotFound` → `ENOENT` rather
//!     than inventing a process (`docs/procfs.md` §5.5).
//!   * **Unknown paths are `NotFound`** — there is no wildcard. A numeric
//!     component is resolved *only* when that pid is currently registered in
//!     `COMPAT_STATES`, so `/proc/<dead-pid>` is `ENOENT`; `/proc/self/fd`,
//!     `/proc/loadavg`, `/proc/version` and friends stay `ENOENT` too.
//!
//! Slices:
//!   * 1 (2.5.0): `/proc/{cpuinfo,meminfo,uptime}` and
//!     `/proc/self/{exe,cmdline,status,maps}`;
//!   * 2 (2.9.0): `/proc/stat` from real tick accounting;
//!   * 3 (2.10.0): `/proc/version`, `/proc/filesystems`, `/proc/loadavg`,
//!     `/proc/sys/kernel/{ostype,osrelease,hostname}` and the per-pid tree
//!     `/proc/<pid>/{status,stat,cmdline,maps}`, plus
//!     `/proc/self/{stat,statm}`.
//!
//! The per-pid files snapshot the TARGET's state (not the reader's) under
//! `COMPAT_STATES`, release the registry lock, and only then format the text.
//! That is the whole point of the `*Snap` structs below: `COMPAT_STATES` is a
//! non-reentrant spinlock and must never be held across a render (AGENTS.md
//! invariant 4), while a cpu-bound target may be concurrently changing its
//! `VmRegionSet`.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::x86_64::apic::TICK_HZ;
use crate::arch::x86_64::cpuid;
use crate::arch::x86_64::linux::signal_frame::{SIG_DFL, SIG_IGN};
use crate::memory::layout::{PAGE_SIZE, USER_STACK_PAGES, USER_STACK_TOP};
use crate::memory::pmm;
use crate::sync::spinlock::Spinlock;
use crate::task::compat::{self, CompatState};
use crate::task::{scheduler, stat};

use super::elf::LoadSegment;
use super::procfs_format::{self as fmt, ProcKind};
use super::{VfsError, VfsNode, VfsResult};

// `/proc/stat` reports USER_HZ units; the counters are raw ticks, so the two
// rates must be the same constant or every number would silently be wrong.
const _: () = assert!(TICK_HZ == stat::USER_HZ);

/// The real crate version (`Cargo.toml` is the single source), the same value
/// `provision.rs` writes to `/mnt/etc/pagh-release`.
const KERNEL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `/proc` subtree root, ready for `vfs::mount_at("/proc", …)`.
pub fn root() -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: String::from("proc"),
        ino: fmt::PROC_ROOT_DIR_INO,
        kind: DirKind::Root,
    })
}

// ─── directories ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum DirKind {
    /// `/proc`.
    Root,
    /// `/proc/self`.
    SelfDir,
    /// `/proc/sys`.
    SysDir,
    /// `/proc/sys/kernel`.
    SysKernelDir,
    /// `/proc/<pid>`, carrying the target pid.
    Pid(u64),
}

struct ProcDir {
    /// `"proc"`, `"self"`, or the decimal pid. Owned because a pid directory's
    /// name is dynamic (`VfsNode::name` returns `&str`).
    name: String,
    ino: u64,
    kind: DirKind,
}

/// A fresh `/proc/self` node (the children are re-created per lookup so each one
/// renders for the process that is asking).
fn self_dir() -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: String::from("self"),
        ino: fmt::PROC_SELF_DIR_INO,
        kind: DirKind::SelfDir,
    })
}

/// `/proc/sys` — the read-only sysctl namespace (slice 3).
fn sys_dir() -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: String::from("sys"),
        ino: fmt::PROC_SYS_DIR_INO,
        kind: DirKind::SysDir,
    })
}

/// `/proc/sys/kernel`.
fn sys_kernel_dir() -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: String::from("kernel"),
        ino: fmt::PROC_SYS_KERNEL_DIR_INO,
        kind: DirKind::SysKernelDir,
    })
}

/// A fresh `/proc/<pid>` node. Only created after the pid was observed live;
/// `lookup` re-checks that on every child access, so a cached directory cannot
/// outlive its process.
fn pid_dir(pid: u64) -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: format!("{}", pid),
        ino: fmt::pid_dir_ino(pid),
        kind: DirKind::Pid(pid),
    })
}

impl VfsNode for ProcDir {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_directory(&self) -> bool {
        true
    }

    fn fs_ino(&self) -> u64 {
        self.ino
    }

    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        let mut out: Vec<Arc<dyn VfsNode>> = Vec::new();
        match self.kind {
            // `readdir` lists the namespace, not the visible-at-this-instant
            // subset: `self` is listed even for a caller that cannot resolve it
            // (Linux behaves the same way for a pid directory that disappeared).
            // The numeric pids ARE a live snapshot: one short registry lock.
            DirKind::Root => {
                out.push(self_dir());
                out.push(sys_dir());
                for (pid, _tgid) in compat::compat_pid_tgid_snapshot() {
                    out.push(pid_dir(pid));
                }
                for e in fmt::PROC_FILES.iter().copied() {
                    out.push(file_for(e));
                }
            }
            DirKind::SelfDir => {
                for e in fmt::PROC_SELF_FILES.iter().copied() {
                    out.push(file_for(e));
                }
            }
            DirKind::SysDir => {
                out.push(sys_kernel_dir());
            }
            DirKind::SysKernelDir => {
                for e in fmt::PROC_SYS_FILES.iter().copied() {
                    out.push(file_for(e));
                }
            }
            DirKind::Pid(pid) => {
                for e in fmt::PROC_PID_FILES.iter().copied() {
                    out.push(pid_file(pid, e));
                }
            }
        }
        Ok(out)
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        match self.kind {
            DirKind::Root => {
                if name == "self" {
                    if compat::current_has_compat() {
                        return Ok(self_dir());
                    }
                    // No process of our own: nothing to describe.
                    return Err(VfsError::NotFound);
                }
                if name == "sys" {
                    return Ok(sys_dir());
                }
                // Numeric components are the per-pid tree. The liveness check is
                // what makes this a namespace rather than a wildcard: a dead (or
                // never-used) pid is ENOENT, never an empty file.
                if let Some(pid) = fmt::parse_pid_component(name) {
                    return if compat::compat_exists(pid) {
                        Ok(pid_dir(pid))
                    } else {
                        Err(VfsError::NotFound)
                    };
                }
                fmt::proc_file(name).map(file_for).ok_or(VfsError::NotFound)
            }
            DirKind::SelfDir => {
                let e = fmt::proc_self_file(name).ok_or(VfsError::NotFound)?;
                if e.kind == ProcKind::SelfExe && current_exe().is_none() {
                    // No image to point at (`readlink` used to answer ENOENT in
                    // exactly this case).
                    return Err(VfsError::NotFound);
                }
                Ok(file_for(e))
            }
            DirKind::SysDir => {
                if name == "kernel" {
                    Ok(sys_kernel_dir())
                } else {
                    Err(VfsError::NotFound)
                }
            }
            DirKind::SysKernelDir => fmt::proc_sys_file(name)
                .map(file_for)
                .ok_or(VfsError::NotFound),
            DirKind::Pid(pid) => {
                if !compat::compat_exists(pid) {
                    return Err(VfsError::NotFound);
                }
                fmt::proc_pid_file(name)
                    .map(|e| pid_file(pid, e))
                    .ok_or(VfsError::NotFound)
            }
        }
    }
}

// ─── files ──────────────────────────────────────────────────────────────────

fn file_for(e: fmt::ProcEntry) -> Arc<dyn VfsNode> {
    Arc::new(ProcFile {
        name: e.name,
        ino: e.ino,
        kind: e.kind,
        pid: 0,
        buf: Spinlock::new(None),
    })
}

fn pid_file(pid: u64, e: fmt::ProcPidEntry) -> Arc<dyn VfsNode> {
    Arc::new(ProcFile {
        name: e.name,
        ino: fmt::pid_file_ino(pid, e.slot),
        kind: e.kind,
        pid,
        buf: Spinlock::new(None),
    })
}

/// The current process's image path, when it has one.
fn current_exe() -> Option<String> {
    compat::with_current_compat(|cs| cs.exe_path.clone()).filter(|p| !p.is_empty())
}

struct ProcFile {
    name: &'static str,
    ino: u64,
    kind: ProcKind,
    /// Target of a `ProcKind::Pid*` node; ignored by the `Self*`/static kinds.
    pid: u64,
    /// The render in use for the current reading pass. Filled on demand; a
    /// zero-offset read refreshes it (see the module docs).
    buf: Spinlock<Option<Vec<u8>>>,
}

impl ProcFile {
    /// Render the file's current text. Called with **no** node lock held, and it
    /// takes no node lock itself, so it may freely reach into kernel state.
    fn render(&self) -> Vec<u8> {
        match self.kind {
            ProcKind::CpuInfo => fmt::format_cpuinfo(&[cpuid::read()]),
            ProcKind::MemInfo => fmt::format_meminfo(&fmt::MemInfoKb {
                total_kb: pmm::total_frames() as u64 * PAGE_SIZE / 1024,
                free_kb: pmm::free_frames() as u64 * PAGE_SIZE / 1024,
            }),
            ProcKind::Uptime => fmt::format_uptime(scheduler::ticks(), TICK_HZ),
            // Counters only: no wall-clock read and no arithmetic in the
            // renderer (`docs/procfs.md` §4.8). `procs_blocked` is a real zero:
            // this scheduler has no blocked state (blocking waits yield and stay
            // runnable), so there is nothing to count.
            ProcKind::Stat => {
                let cpu = stat::cpu_ticks();
                fmt::format_stat(&fmt::StatInputs {
                    user: cpu.user,
                    system: cpu.system,
                    idle: cpu.idle,
                    ctxt: stat::context_switches(),
                    btime: stat::boot_unix(),
                    processes: stat::processes(),
                    procs_running: scheduler::ready_count(),
                    procs_blocked: 0,
                })
            }
            ProcKind::SelfCmdline => self.render_cmdline(scheduler::current_pid()),
            ProcKind::SelfStatus => self.render_status(scheduler::current_pid()),
            ProcKind::SelfMaps => self.render_maps(scheduler::current_pid()),
            ProcKind::SelfStat => self.render_stat(scheduler::current_pid()),
            ProcKind::SelfStatm => self.render_statm(scheduler::current_pid()),
            ProcKind::PidCmdline => self.render_cmdline(self.pid),
            ProcKind::PidStatus => self.render_status(self.pid),
            ProcKind::PidMaps => self.render_maps(self.pid),
            ProcKind::PidStat => self.render_stat(self.pid),
            // The real crate version, never a fabricated Linux release string.
            ProcKind::Version => fmt::format_version(KERNEL_VERSION),
            ProcKind::FileSystems => fmt::format_filesystems(&fmt::FILESYSTEMS),
            ProcKind::LoadAvg => {
                let (runnable, total, last_pid) = scheduler::loadavg_counts();
                fmt::format_loadavg(&fmt::LoadAvg {
                    runnable,
                    total,
                    last_pid,
                })
            }
            ProcKind::SysOstype => fmt::format_sys_ostype(),
            ProcKind::SysOsrelease => fmt::format_sys_osrelease(KERNEL_VERSION),
            ProcKind::SysHostname => fmt::format_sys_hostname(),
            // The `exe` node is a symlink, not a readable file.
            ProcKind::SelfExe => Vec::new(),
        }
    }

    /// Render into the session buffer (idempotent).
    fn refresh(&self) {
        let fresh = self.render();
        *self.buf.lock() = Some(fresh);
    }

    /// `/proc/<pid>/cmdline` is the argv snapshot `CompatState` already holds in
    /// Linux's NUL-separated form (`format_cmdline`). Copy it out under the
    /// registry lock, format nothing.
    fn render_cmdline(&self, pid: u64) -> Vec<u8> {
        compat::with_pid_compat(pid, |cs| cs.cmdline.clone()).unwrap_or_default()
    }

    fn render_status(&self, pid: u64) -> Vec<u8> {
        let Some(s) = compat::with_pid_compat(pid, |cs| StatusSnap::from(cs)) else {
            // The target exited between lookup and render: an empty file is the
            // only truthful answer a VFS node can give (the path itself is gone).
            return Vec::new();
        };
        // Threads = the target thread plus every other thread of its group.
        // Queried outside the registry lock (`group_member_pids` takes it).
        let threads = compat::group_member_pids(s.tgid, pid).len() as u64 + 1;

        fmt::format_status(&fmt::StatusInputs {
            name: &s.name,
            state: pid_state(pid),
            umask: s.umask,
            pid,
            tgid: s.tgid,
            ppid: s.ppid,
            threads,
            fd_size: s.fd_size,
            sig_pending: s.sig_pending,
            sig_blocked: s.sig_blocked,
            sig_ign: s.sig_ign,
            sig_cgt: s.sig_cgt,
            uids: s.uids,
            gids: s.gids,
            vm: s.vm,
        })
    }

    fn render_stat(&self, pid: u64) -> Vec<u8> {
        let Some(s) = compat::with_pid_compat(pid, |cs| StatusSnap::from(cs)) else {
            return Vec::new();
        };
        let threads = compat::group_member_pids(s.tgid, pid).len() as u64 + 1;
        let stack_lo = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE;

        fmt::format_pid_stat(&fmt::PidStatInputs {
            name: &s.name,
            pid,
            state: pid_state(pid),
            ppid: s.ppid,
            // One process group per thread group (pagh does not model sessions).
            pgrp: s.tgid,
            session: s.tgid,
            threads,
            sig_blocked: s.sig_blocked,
            sig_ign: s.sig_ign,
            sig_cgt: s.sig_cgt,
            vm: s.vm,
            start_brk: s.current_brk,
            startstack: stack_lo,
            exit_code: s.exit_code,
        })
    }

    fn render_statm(&self, pid: u64) -> Vec<u8> {
        match compat::with_pid_compat(pid, |cs| StatusSnap::from(cs)) {
            Some(s) => fmt::format_statm(&s.vm),
            None => Vec::new(),
        }
    }

    fn render_maps(&self, pid: u64) -> Vec<u8> {
        let Some(s) = compat::with_pid_compat(pid, |cs| MapsSnap::from(cs)) else {
            return Vec::new();
        };
        maps_text(&s)
    }
}

/// Is the target task currently parked by a stop signal? This kernel tracks
/// exactly two observable states for `/proc` purposes: `R` (running/ready) and
/// `T` (stopped; `scheduler::STOPPED_TASKS`). Sleeping tasks are not modelled.
fn pid_state(pid: u64) -> u8 {
    if scheduler::is_stopped(pid) {
        b'T'
    } else {
        b'R'
    }
}

/// Everything `/proc/<pid>/{status,stat,statm}` needs, copied out from under the
/// registry lock so formatting never holds it (AGENTS.md invariant 4).
struct StatusSnap {
    name: Vec<u8>,
    umask: u32,
    tgid: u64,
    ppid: u64,
    fd_size: u64,
    sig_pending: u64,
    sig_blocked: u64,
    sig_ign: u64,
    sig_cgt: u64,
    uids: [u32; 4],
    gids: [u32; 4],
    vm: fmt::VmKb,
    /// Current program break (`/proc/<pid>/stat` field 47).
    current_brk: u64,
    /// Recorded exit code (`/proc/<pid>/stat` field 52), 0 while alive.
    exit_code: i64,
}

impl StatusSnap {
    fn from(cs: &CompatState) -> Self {
        let argv0: &[u8] = match cs.cmdline.iter().position(|b| *b == 0) {
            Some(end) => &cs.cmdline[..end],
            None => &cs.cmdline[..],
        };
        let name = fmt::task_name(&cs.exe_path, argv0);

        // Disposition masks: `SIG_IGN` for ignored signals, "handler installed"
        // for everything that is neither default nor ignore.
        let mut sig_ign = 0u64;
        let mut sig_cgt = 0u64;
        for (i, action) in cs.sig.lock().handlers.iter().enumerate() {
            if action.handler == SIG_IGN {
                sig_ign |= 1u64 << i;
            } else if action.handler != SIG_DFL {
                sig_cgt |= 1u64 << i;
            }
        }

        let (vm, current_brk) = vm_kb(cs);

        StatusSnap {
            name,
            umask: cs.umask,
            tgid: cs.tgid,
            ppid: cs.ppid,
            fd_size: cs.fds.capacity() as u64,
            sig_pending: cs.sig_pending,
            sig_blocked: cs.sig_blocked,
            sig_ign,
            sig_cgt,
            // Linux's four Uid/Gid columns are real, effective, saved and
            // filesystem. pagh has no separate fsuid/fsgid, so the last column
            // repeats the effective id.
            uids: [cs.creds.uid, cs.creds.euid, cs.creds.suid, cs.creds.euid],
            gids: [cs.creds.gid, cs.creds.egid, cs.creds.sgid, cs.creds.egid],
            vm,
            current_brk,
            exit_code: cs.exit_code.map(|c| c as i64).unwrap_or(0),
        }
    }
}

/// `/proc/<pid>/maps` inputs, snapshotted under the registry lock.
struct MapsSnap {
    exe_path: String,
    interp_path: String,
    image_segments: Vec<LoadSegment>,
    interp_segments: Vec<LoadSegment>,
    initial_brk: u64,
    current_brk: u64,
    mmaps: Vec<crate::arch::x86_64::linux::mem::MmapRegion>,
}

impl MapsSnap {
    fn from(cs: &CompatState) -> Self {
        let guard = cs.vm.lock();
        MapsSnap {
            exe_path: cs.exe_path.clone(),
            interp_path: cs.interp_path.clone(),
            image_segments: cs.image_segments.clone(),
            interp_segments: cs.interp_segments.clone(),
            initial_brk: guard.initial_brk,
            current_brk: guard.current_brk,
            mmaps: guard.mmaps.clone(),
        }
    }
}

/// Render a `/proc/<pid>/maps` snapshot (shared by `self` and pid nodes).
fn maps_text(s: &MapsSnap) -> Vec<u8> {
    let mut regions: Vec<fmt::MapsRegion> = Vec::new();
    for seg in &s.image_segments {
        regions.push(seg_region(seg, Some(s.exe_path.as_str())));
    }
    for seg in &s.interp_segments {
        regions.push(seg_region(seg, Some(s.interp_path.as_str())));
    }

    let heap_lo = page_down(s.initial_brk);
    let heap_hi = page_up(s.current_brk);
    if heap_hi > heap_lo {
        regions.push(fmt::MapsRegion {
            start: heap_lo,
            end: heap_hi,
            prot: fmt::PROT_READ | fmt::PROT_WRITE,
            file_offset: 0,
            dev_major: 0,
            dev_minor: 0,
            ino: 0,
            path: Some("[heap]"),
        });
    }

    for m in &s.mmaps {
        regions.push(fmt::MapsRegion {
            start: m.base,
            end: m.base.saturating_add(m.pages * PAGE_SIZE),
            prot: m.prot,
            file_offset: 0,
            dev_major: 0,
            dev_minor: 0,
            ino: 0,
            // A file-backed `mmap` has no recorded pathname in
            // `VmRegionSet` (docs/procfs.md DEVIATION-7).
            path: None,
        });
    }

    let stack_lo = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE;
    regions.push(fmt::MapsRegion {
        start: stack_lo,
        end: USER_STACK_TOP,
        prot: fmt::PROT_READ | fmt::PROT_WRITE,
        file_offset: 0,
        dev_major: 0,
        dev_minor: 0,
        ino: 0,
        path: Some("[stack]"),
    });

    fmt::format_maps(&regions)
}

/// One `/proc/<pid>/maps` line for a loaded `PT_LOAD` segment.
fn seg_region<'a>(seg: &LoadSegment, path: Option<&'a str>) -> fmt::MapsRegion<'a> {
    fmt::MapsRegion {
        start: seg.start,
        end: seg.end,
        prot: prot_bits(seg.prot),
        file_offset: seg.file_offset,
        dev_major: 0,
        dev_minor: 0,
        ino: 0,
        path,
    }
}

/// ELF `p_flags` (`PF_R`/`PF_W`/`PF_X`) → Linux `PROT_*` bits.
fn prot_bits(p_flags: u32) -> u32 {
    let mut prot = 0;
    if p_flags & 4 != 0 {
        prot |= fmt::PROT_READ;
    }
    if p_flags & 2 != 0 {
        prot |= fmt::PROT_WRITE;
    }
    if p_flags & 1 != 0 {
        prot |= fmt::PROT_EXEC;
    }
    prot
}

/// Page-rounded Vm* accounting for the loaded images, in KiB.
fn image_kb(image: &[LoadSegment], interp: &[LoadSegment]) -> (u64, u64) {
    let mut total = 0u64;
    for seg in image.iter().chain(interp.iter()) {
        total += seg.end.saturating_sub(seg.start);
    }
    (total / 1024, total / 1024)
}

/// Snapshot the target's Vm* accounting and current break under one `vm` lock.
///
/// `size` is image + heap + stack + anonymous mappings; `rss` counts only the
/// eagerly backed parts (image + stack), exactly as `/proc/self/status` has
/// reported since slice 1 (`docs/procfs.md` §4.5.1, DEVIATION-5).
fn vm_kb(cs: &CompatState) -> (fmt::VmKb, u64) {
    let guard = cs.vm.lock();
    let heap_lo = page_down(guard.initial_brk);
    let heap_hi = page_up(guard.current_brk);
    let heap_kb = if heap_hi > heap_lo {
        (heap_hi - heap_lo) / 1024
    } else {
        0
    };
    let stack_kb = USER_STACK_PAGES * PAGE_SIZE / 1024;
    let mmap_kb: u64 = guard.mmaps.iter().map(|m| m.pages * PAGE_SIZE / 1024).sum();
    let (image_kb, rss_kb) = image_kb(&cs.image_segments, &cs.interp_segments);
    (
        fmt::VmKb {
            size: image_kb + heap_kb + stack_kb + mmap_kb,
            rss: rss_kb + stack_kb,
            data: heap_kb,
            stk: stack_kb,
            exe: image_kb,
            lib: 0,
        },
        guard.current_brk,
    )
}

fn page_down(addr: u64) -> u64 {
    addr & !(PAGE_SIZE - 1)
}

fn page_up(addr: u64) -> u64 {
    page_down(addr.saturating_add(PAGE_SIZE - 1))
}

impl VfsNode for ProcFile {
    fn name(&self) -> &str {
        self.name
    }

    fn is_directory(&self) -> bool {
        false
    }

    fn fs_ino(&self) -> u64 {
        self.ino
    }

    fn is_symlink(&self) -> bool {
        self.kind == ProcKind::SelfExe
    }

    fn read_link(&self) -> Option<String> {
        if self.kind == ProcKind::SelfExe {
            current_exe()
        } else {
            None
        }
    }

    fn size(&self) -> u64 {
        // Rendered on first touch; afterwards the session buffer is reused so
        // `size()` and `read()` inside one syscall agree.
        if self.buf.lock().is_none() {
            self.refresh();
        }
        self.buf
            .lock()
            .as_ref()
            .map(|b| b.len() as u64)
            .unwrap_or(0)
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if self.kind == ProcKind::SelfExe {
            // A symlink has no readable content; `open` follows it instead.
            return Err(VfsError::NotSupported);
        }
        if offset == 0 {
            // Start of a reading pass: show current state.
            self.refresh();
        } else if self.buf.lock().is_none() {
            self.refresh();
        }

        let guard = self.buf.lock();
        let data = match guard.as_ref() {
            Some(d) => d,
            None => return Ok(0),
        };
        let off = offset as usize;
        if off >= data.len() {
            return Ok(0);
        }
        let n = core::cmp::min(buf.len(), data.len() - off);
        buf[..n].copy_from_slice(&data[off..off + n]);
        Ok(n)
    }

    // write/truncate/create/remove keep the trait defaults (`NotSupported`): the
    // syscall layer maps that to `EACCES` for a read-only synthetic file.
    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        Err(VfsError::NotSupported)
    }
}
