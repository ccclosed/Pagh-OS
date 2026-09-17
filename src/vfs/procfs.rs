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
//!     is never held across a `with_current_compat` call.
//!   * **`/proc/self` is invisible to a task without `CompatState`** (a kernel
//!     thread, the boot selftest): `lookup` returns `NotFound` → `ENOENT` rather
//!     than inventing a process (`docs/procfs.md` §5.5).
//!   * **Unknown paths are `NotFound`** — there is no wildcard, so `/proc/1`,
//!     `/proc/stat` (deferred), `/proc/self/fd` etc. all return `ENOENT`.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::x86_64::apic::TICK_HZ;
use crate::arch::x86_64::cpuid;
use crate::arch::x86_64::linux::signal_frame::{SIG_DFL, SIG_IGN};
use crate::memory::layout::{PAGE_SIZE, USER_STACK_PAGES, USER_STACK_TOP};
use crate::memory::pmm;
use crate::sync::spinlock::Spinlock;
use crate::task::{compat, scheduler};

use super::elf::LoadSegment;
use super::procfs_format::{self as fmt, ProcKind};
use super::{VfsError, VfsNode, VfsResult};

/// The `/proc` subtree root, ready for `vfs::mount_at("/proc", …)`.
pub fn root() -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: "proc",
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
}

struct ProcDir {
    name: &'static str,
    ino: u64,
    kind: DirKind,
}

/// A fresh `/proc/self` node (the children are re-created per lookup so each one
/// renders for the process that is asking).
fn self_dir() -> Arc<dyn VfsNode> {
    Arc::new(ProcDir {
        name: "self",
        ino: fmt::PROC_SELF_DIR_INO,
        kind: DirKind::SelfDir,
    })
}

impl VfsNode for ProcDir {
    fn name(&self) -> &str {
        self.name
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
            DirKind::Root => {
                out.push(self_dir());
                for e in fmt::PROC_FILES.iter().copied() {
                    out.push(file_for(e));
                }
            }
            DirKind::SelfDir => {
                for e in fmt::PROC_SELF_FILES.iter().copied() {
                    out.push(file_for(e));
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
        }
    }
}

// ─── files ──────────────────────────────────────────────────────────────────

fn file_for(e: fmt::ProcEntry) -> Arc<dyn VfsNode> {
    Arc::new(ProcFile {
        name: e.name,
        ino: e.ino,
        kind: e.kind,
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
            ProcKind::SelfCmdline => {
                compat::with_current_compat(|cs| cs.cmdline.clone()).unwrap_or_default()
            }
            ProcKind::SelfStatus => self.render_status(),
            ProcKind::SelfMaps => self.render_maps(),
            // The `exe` node is a symlink, not a readable file.
            ProcKind::SelfExe => Vec::new(),
        }
    }

    /// Render into the session buffer (idempotent).
    fn refresh(&self) {
        let fresh = self.render();
        *self.buf.lock() = Some(fresh);
    }

    fn render_status(&self) -> Vec<u8> {
        let pid = scheduler::current_pid();

        /// Everything `/proc/self/status` needs, copied out from under the
        /// registry lock so rendering and the thread-count query happen outside.
        struct Snap {
            name: Vec<u8>,
            umask: u32,
            tgid: u64,
            ppid: u64,
            fd_size: u64,
            sig_pending: u64,
            sig_blocked: u64,
            sig_ign: u64,
            sig_cgt: u64,
            vm: fmt::VmKb,
        }

        let snap = compat::with_current_compat(|cs| {
            let argv0: &[u8] = match cs.cmdline.iter().position(|b| *b == 0) {
                Some(end) => &cs.cmdline[..end],
                None => &cs.cmdline[..],
            };
            let name = fmt::task_name(&cs.exe_path, argv0);

            // Disposition masks: `SIG_IGN` for ignored signals, "handler
            // installed" for everything that is neither default nor ignore.
            let mut sig_ign = 0u64;
            let mut sig_cgt = 0u64;
            for (i, action) in cs.sig.lock().handlers.iter().enumerate() {
                if action.handler == SIG_IGN {
                    sig_ign |= 1u64 << i;
                } else if action.handler != SIG_DFL {
                    sig_cgt |= 1u64 << i;
                }
            }

            let vm = {
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
                fmt::VmKb {
                    size: image_kb + heap_kb + stack_kb + mmap_kb,
                    rss: rss_kb + stack_kb,
                    data: heap_kb,
                    stk: stack_kb,
                    exe: image_kb,
                    lib: 0,
                }
            };

            Snap {
                name,
                umask: cs.umask,
                tgid: cs.tgid,
                ppid: cs.ppid,
                fd_size: cs.fds.capacity() as u64,
                sig_pending: cs.sig_pending,
                sig_blocked: cs.sig_blocked,
                sig_ign,
                sig_cgt,
                vm,
            }
        });

        // A kernel thread has no compat state: the caller cannot reach this node
        // (see `lookup`), but stay defensive and render nothing rather than a
        // half-invented process.
        let Some(s) = snap else {
            return Vec::new();
        };

        // Threads = the calling thread plus every other thread of the group.
        // Queried outside the registry lock (`group_member_pids` takes it).
        let threads = compat::group_member_pids(s.tgid, pid).len() as u64 + 1;

        fmt::format_status(&fmt::StatusInputs {
            name: &s.name,
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
            vm: s.vm,
        })
    }

    fn render_maps(&self) -> Vec<u8> {
        struct Snap {
            exe_path: String,
            interp_path: String,
            image_segments: Vec<LoadSegment>,
            interp_segments: Vec<LoadSegment>,
            initial_brk: u64,
            current_brk: u64,
            mmaps: Vec<crate::arch::x86_64::linux::mem::MmapRegion>,
        }

        let snap = compat::with_current_compat(|cs| {
            let guard = cs.vm.lock();
            Snap {
                exe_path: cs.exe_path.clone(),
                interp_path: cs.interp_path.clone(),
                image_segments: cs.image_segments.clone(),
                interp_segments: cs.interp_segments.clone(),
                initial_brk: guard.initial_brk,
                current_brk: guard.current_brk,
                mmaps: guard.mmaps.clone(),
            }
        });
        let Some(s) = snap else {
            return Vec::new();
        };

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
}

/// One `/proc/self/maps` line for a loaded `PT_LOAD` segment.
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
