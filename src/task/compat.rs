//! Per-`Compat_Process` compatibility state (design "Compat_Process additions").
//!
//! A Linux `Compat_Process` carries more state than a pagh-native task: its
//! file-descriptor table (R2.4/R2.6/R2.14), its program-break + anonymous-`mmap`
//! bookkeeping (R3/R4), the `FS.base` set via `arch_prctl(ARCH_SET_FS)` (R2.9),
//! its thread id returned by `set_tid_address` (R2.10/R7.2), the set of
//! already-reported unsupported syscall numbers for the at-most-once `nosys`
//! diagnostic (R12.2), and the normalized exit code (R12.3).
//!
//! This bundle hangs off the scheduler [`Tcb`](super::scheduler::Tcb) as an
//! `Option<CompatState>`: it is `None` for the existing pagh-native tasks
//! (`spawn_test_user_process`) and only `Some` once `run_linux_binary` (task 13.3)
//! populates it for a real Linux binary.
#![allow(dead_code)]

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::x86_64::linux::mem::VmRegionSet;
use crate::sync::spinlock::Spinlock;

use super::fd::FdTable;

/// All Linux-compatibility state attached to a `Compat_Process`.
#[derive(Clone)]
pub struct CompatState {
    /// The process's open file descriptors (0/1/2 pre-bound to the std streams).
    pub fds: FdTable,
    /// Program break + anonymous `mmap` region tracking.
    pub vm: Arc<Spinlock<VmRegionSet>>,
    /// `FS.base`, settable via `arch_prctl(ARCH_SET_FS)` (R2.9).
    pub fs_base: u64,
    /// The thread id reported by `set_tid_address` (R2.10).
    pub tid: u64,
    /// Parent process id used by getppid/wait4.
    pub ppid: u64,
    /// Process id shared by all threads in the group.
    pub tgid: u64,
    /// Address cleared and futex-woken at thread exit.
    pub clear_child_tid: u64,
    /// Whether exit creates a wait4-visible zombie.
    pub waitable: bool,
    /// Linux robust-futex list head and ABI structure size.
    pub robust_head: u64,
    pub robust_len: u64,
    /// The process's current working directory (absolute, default `/`). Relative
    /// `open`/`openat`/`access`/`chdir` paths resolve against this; `getcwd`
    /// reports it (Feature: linux-binary-compat).
    pub cwd: String,
    /// Distinct unsupported syscall numbers already logged, so the `nosys`
    /// diagnostic is emitted at most once per number per process (R12.2).
    pub nosys_logged: BTreeSet<u64>,
    /// Whether the controlling tty is in raw mode (ICANON cleared via TCSETS).
    pub raw_mode: bool,
    /// STAGE 16.16: whether the controlling tty still echoes typed characters
    /// (ECHO in c_lflag). A real tty echoes even in raw mode; only programs
    /// that draw their own input (nvim, full readline) clear this bit.
    pub echo: bool,
    /// The normalized exit code (low byte of the requested code), once the
    /// process has exited (R12.3).
    pub exit_code: Option<u8>,
    /// Absolute VFS path of the exec'd image, reported through
    /// `readlink("/proc/self/exe")` (libuv's uv_exepath / nvim's progpath).
    pub exe_path: String,
    /// What fds 0/1/2 were AT execve time (after the
    /// close-on-exec sweep). The spawn-time [DIAG] lines are wiped by the
    /// TUI screen clear, so the watchdog re-prints this snapshot instead.
    pub exec_stdio: [&'static str; 3],
    /// Per-process rlimit overrides set via `prlimit64`/`setrlimit` (resource
    /// id -> (soft, hard)). Resources absent here fall back to the kernel
    /// defaults (`misc::default_rlimit`); `fork`/`clone` inherit the table.
    pub rlimits: BTreeMap<u64, (u64, u64)>,
    /// The process's file-mode creation mask (`umask(2)`); applied to the
    /// permission bits of freshly created files/directories.
    pub umask: u32,
    /// Permission-bit overrides keyed by VFS inode identity (`fs_ino`, or the
    /// FNV fallback for synthetic nodes). Written by `chmod` and at creation
    /// time (default mode masked by `umask`); consulted by `stat`-family
    /// handlers when reporting `st_mode`. Forked/cloned children inherit it.
    pub mode_overrides: BTreeMap<u64, u32>,
}

impl CompatState {
    /// Build the initial compat state for a freshly launched Linux binary:
    /// the supplied descriptor table and VM bookkeeping, `FS.base` cleared to 0,
    /// the given thread id, cwd `/mnt` (the writable ext2 tree; `/` has no
    /// `create_dir`), an empty `nosys` log, and no exit
    /// code yet.
    pub fn new(fds: FdTable, vm: Arc<Spinlock<VmRegionSet>>, tid: u64) -> Self {
        Self::new_with_parent(fds, vm, tid, 1)
    }
    pub fn new_with_parent(
        fds: FdTable,
        vm: Arc<Spinlock<VmRegionSet>>,
        tid: u64,
        ppid: u64,
    ) -> Self {
        Self {
            fds,
            vm,
            fs_base: 0,
            tid,
            ppid,
            tgid: tid,
            clear_child_tid: 0,
            waitable: true,
            robust_head: 0,
            robust_len: 0,
            cwd: "/mnt".to_string(),
            nosys_logged: BTreeSet::new(),
            raw_mode: false,
            echo: true,
            exit_code: None,
            exe_path: String::new(),
            exec_stdio: ["?", "?", "?"],
            rlimits: BTreeMap::new(),
            umask: 0o022,
            mode_overrides: BTreeMap::new(),
        }
    }
}

// ─── Current-process CompatState registry ────────────────────────────────────
//
// ARCHITECTURE NOTE (the single source of truth for a RUNNING Compat_Process).
//
// The scheduler keeps NO persistent `Tcb` for the running task — it stores only
// `CURRENT_PID` and rebuilds the `Tcb` from the kernel RSP on each tick (see
// `scheduler::scheduler_tick_irq`), discarding any `Tcb.compat` field on requeue.
// Effectful syscall handlers, however, need mutable access to the *running*
// process's `CompatState` (its `FdTable`, `VmRegionSet`, `fs_base`, `tid`, and
// `nosys_logged` set). A field on the transient `Tcb` therefore cannot be the
// owner of that state.
//
// We resolve this with a global registry keyed by pid. This registry — NOT any
// field on the transient `Tcb` (the field was removed: the `Tcb` is rebuilt
// from the saved RSP on every tick and can never carry authoritative state) —
// is the authoritative home of a Compat_Process's Linux state while it runs.
// It is consulted via [`with_current_compat`].
//
// `run_linux_binary` (task 13.3) calls [`install_compat`] to register a freshly
// launched process's state; [`remove_compat`] tears it down. `exit_current`
// already removes the entry for the calling pid (wired in
// `scheduler::exit_current`), so an exiting Compat_Process drops its registry
// entry as part of termination.

/// The authoritative registry of per-process [`CompatState`], keyed by pid.
///
/// Guarded by a [`Spinlock`] (which disables interrupts while held). Handlers
/// must therefore NOT hold this lock across operations that block waiting for a
/// device interrupt (e.g. ext2/VFS disk I/O): they extract what they need under
/// the lock, release it, perform the blocking work, then re-acquire briefly to
/// commit results (the pattern used by the `io` handlers). Page-table/PMM work
/// (`brk`/`mmap`/`munmap`/`mprotect`) does not wait on interrupts, so it may run
/// inside the [`with_current_compat`] closure.
static COMPAT_STATES: Spinlock<BTreeMap<u64, CompatState>> = Spinlock::new(BTreeMap::new());
static EXITED_CHILDREN: Spinlock<BTreeMap<(u64, u64), u8>> = Spinlock::new(BTreeMap::new());

/// Register `state` as the [`CompatState`] for process `pid`, replacing any
/// previous entry. Called by `run_linux_binary` (task 13.3) when launching a
/// Compat_Process.
pub fn install_compat(pid: u64, state: CompatState) {
    COMPAT_STATES.lock().insert(pid, state);
}

/// Remove and return the [`CompatState`] for process `pid`, if any. Called when
/// a process terminates (`scheduler::exit_current`).
/// Is `pid`'s stdin currently in raw mode? The ^C kill path consults this so
/// full-screen programs that consume ^C as a key (nvim) keep their binding,
/// while canonical-mode programs (python, bash) get the classic SIGINT-ish
/// terminate.
/// Does the CURRENT task's mmap tracking contain `addr` inside a region that
/// was created with PROT_WRITE? Used by the page-fault handler to decide
/// whether a write-to-RO-page fault is a fixable mapping inconsistency.
pub fn current_addr_in_writable_mmap(addr: u64) -> bool {
    with_current_compat(|cs| {
        cs.vm
            .lock()
            .mmaps
            .iter()
            .any(|m| addr >= m.base && addr < m.base + m.pages * 4096 && m.writable)
    })
    .unwrap_or(false)
}

pub fn compat_is_raw(pid: u64) -> bool {
    COMPAT_STATES
        .lock()
        .get(&pid)
        .map(|cs| cs.raw_mode)
        .unwrap_or(false)
}

pub fn remove_compat(pid: u64) -> Option<CompatState> {
    COMPAT_STATES.lock().remove(&pid)
}
/// Whether a compat process/thread with this pid still exists (used by the
/// shell's foreground wait after `lxrun`).
pub fn compat_exists(pid: u64) -> bool {
    COMPAT_STATES.lock().contains_key(&pid)
}
pub fn finish_compat_exit(pid: u64) {
    if let Some(state) = remove_compat(pid) {
        if state.waitable && state.ppid != 0 {
            EXITED_CHILDREN
                .lock()
                .insert((state.ppid, pid), state.exit_code.unwrap_or(0));
        }
    }
}
pub fn current_ppid() -> u64 {
    with_current_compat(|s| s.ppid).unwrap_or(1)
}
fn child_matches(wanted: i64, child: u64) -> bool {
    wanted == -1 || (wanted > 0 && child == wanted as u64)
}
pub fn reap_child(parent: u64, wanted: i64) -> Option<(u64, u8)> {
    let mut z = EXITED_CHILDREN.lock();
    let k = z
        .keys()
        .find(|(p, c)| *p == parent && child_matches(wanted, *c))
        .copied()?;
    z.remove(&k).map(|v| (k.1, v))
}
pub fn has_child(parent: u64, wanted: i64) -> bool {
    if COMPAT_STATES
        .lock()
        .iter()
        .any(|(p, s)| s.ppid == parent && child_matches(wanted, *p))
    {
        return true;
    }
    EXITED_CHILDREN
        .lock()
        .keys()
        .any(|(p, c)| *p == parent && child_matches(wanted, *c))
}

/// Whether the currently-running process (per `scheduler::current_pid`) has a
/// registered [`CompatState`] — i.e. is a Linux `Compat_Process` rather than a
/// pagh-native task. The dispatcher uses this to decide precedence: a process
/// with compat state gets full Linux syscall semantics; a native task without
/// it keeps the legacy pagh-native routing.
pub fn clone_current_compat(child: u64, tls: Option<u64>, clear_child_tid: u64) -> bool {
    let parent = super::scheduler::current_pid();
    let mut states = COMPAT_STATES.lock();
    let Some(mut child_state) = states.get(&parent).cloned() else {
        return false;
    };
    child_state.tid = child;
    child_state.ppid = parent;
    child_state.waitable = false;
    child_state.clear_child_tid = clear_child_tid;
    child_state.exit_code = None;
    if let Some(base) = tls {
        child_state.fs_base = base;
    }
    states.insert(child, child_state);
    true
}

/// Clone the parent's full compat state for a forked child.
/// The child becomes its own thread-group leader (tid = tgid = child pid), is
/// waitable (a future wait4 zombie for the parent), and inherits the fd table
/// (Arc-shared pipe/socket endpoints — Linux dup semantics), cwd, fs_base,
/// raw_mode, robust-list registration, and exe_path.
pub fn fork_current_compat(child: u64, clear_child_tid: u64) -> bool {
    let parent = super::scheduler::current_pid();
    let mut states = COMPAT_STATES.lock();
    let Some(parent_state) = states.get(&parent) else {
        return false;
    };
    let parent_tgid = parent_state.tgid;
    let mut child_state = parent_state.clone();
    // Fork: the child owns a private address space (deep-copied by
    // `clone_user_space`), so give it a private copy of the mmap/brk tracking
    // rather than sharing the parent's VmRegionSet.
    {
        let vm_copy = child_state.vm.lock().clone();
        child_state.vm = Arc::new(Spinlock::new(vm_copy));
    }
    child_state.tid = child;
    child_state.tgid = child;
    child_state.ppid = parent_tgid;
    child_state.waitable = true;
    child_state.clear_child_tid = clear_child_tid;
    child_state.exit_code = None;
    states.insert(child, child_state);
    true
}
pub fn fs_base_for(pid: u64) -> Option<u64> {
    COMPAT_STATES.lock().get(&pid).map(|s| s.fs_base)
}
pub fn current_tgid() -> u64 {
    with_current_compat(|s| s.tgid).unwrap_or_else(super::scheduler::current_pid)
}
pub fn current_clear_child_tid() -> u64 {
    with_current_compat(|s| s.clear_child_tid).unwrap_or(0)
}
pub fn current_robust_list() -> (u64, u64) {
    with_current_compat(|s| (s.robust_head, s.robust_len)).unwrap_or((0, 0))
}
/// Restored during the origin/main merge: futex cleanup on thread exit needs
/// these by pid (not just for the current task).
pub fn clear_tid_for(pid: u64) -> u64 {
    COMPAT_STATES
        .lock()
        .get(&pid)
        .map(|s| s.clear_child_tid)
        .unwrap_or(0)
}
pub fn robust_for(pid: u64) -> (u64, u64) {
    COMPAT_STATES
        .lock()
        .get(&pid)
        .map(|s| (s.robust_head, s.robust_len))
        .unwrap_or((0, 0))
}
pub fn group_member_pids(tgid: u64, except: u64) -> alloc::vec::Vec<u64> {
    COMPAT_STATES
        .lock()
        .iter()
        .filter_map(|(pid, s)| (s.tgid == tgid && *pid != except).then_some(*pid))
        .collect()
}

pub fn current_has_compat() -> bool {
    let pid = super::scheduler::current_pid();
    COMPAT_STATES.lock().contains_key(&pid)
}

/// Run `f` against the currently-running process's [`CompatState`], returning
/// `Some(f(..))` when that process has registered compat state and `None`
/// otherwise (e.g. a native task, or before `install_compat`).
///
/// The `COMPAT_STATES` lock is held for the duration of `f`, so `f` must not
/// block on a device interrupt (see the [`COMPAT_STATES`] note) nor call back
/// into [`with_current_compat`] (which would deadlock on the same lock).
pub fn with_current_compat<R>(f: impl FnOnce(&mut CompatState) -> R) -> Option<R> {
    let pid = super::scheduler::current_pid();
    COMPAT_DEPTH.fetch_add(1, Ordering::Relaxed);
    let out = {
        let mut guard = COMPAT_STATES.lock();
        guard.get_mut(&pid).map(f)
    };
    COMPAT_DEPTH.fetch_sub(1, Ordering::Relaxed);
    out
}

/// Reentrance depth of [`with_current_compat`] on this (single) CPU. The
/// page-fault handler consults it: a fault raised INSIDE a
/// `with_current_compat` closure (kernel code touching user memory under the
/// lock) must not re-enter the registry — the spinlock is not reentrant and a
/// second `lock()` would deadlock the CPU.
static COMPAT_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Whether the current context is already inside [`with_current_compat`].
pub fn compat_lock_held() -> bool {
    COMPAT_DEPTH.load(Ordering::Relaxed) > 0
}
