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

use crate::arch::x86_64::linux::mem::VmRegionSet;
use crate::sync::spinlock::Spinlock;

use super::fd::FdTable;

/// All Linux-compatibility state attached to a `Compat_Process`.
pub struct CompatState {
    /// The process's open file descriptors (0/1/2 pre-bound to the std streams).
    pub fds: FdTable,
    /// Program break + anonymous `mmap` region tracking.
    pub vm: VmRegionSet,
    /// `FS.base`, settable via `arch_prctl(ARCH_SET_FS)` (R2.9).
    pub fs_base: u64,
    /// The thread id reported by `set_tid_address` (R2.10).
    pub tid: u64,
    /// The process's current working directory (absolute, default `/`). Relative
    /// `open`/`openat`/`access`/`chdir` paths resolve against this; `getcwd`
    /// reports it (Feature: linux-binary-compat).
    pub cwd: String,
    /// Distinct unsupported syscall numbers already logged, so the `nosys`
    /// diagnostic is emitted at most once per number per process (R12.2).
    pub nosys_logged: BTreeSet<u64>,
    /// The normalized exit code (low byte of the requested code), once the
    /// process has exited (R12.3).
    pub exit_code: Option<u8>,
}

impl CompatState {
    /// Build the initial compat state for a freshly launched Linux binary:
    /// the supplied descriptor table and VM bookkeeping, `FS.base` cleared to 0,
    /// the given thread id, the root cwd `/`, an empty `nosys` log, and no exit
    /// code yet.
    pub fn new(fds: FdTable, vm: VmRegionSet, tid: u64) -> Self {
        Self {
            fds,
            vm,
            fs_base: 0,
            tid,
            cwd: "/".to_string(),
            nosys_logged: BTreeSet::new(),
            exit_code: None,
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
// We resolve this with a global registry keyed by pid. This registry — NOT the
// `Option<CompatState>` field still present on `Tcb` — is the authoritative home
// of a Compat_Process's Linux state while it runs. The `Tcb.compat` field is left
// in place (it is harmless and `None` for every native task) but is not used by
// the dispatcher/handlers; the registry is consulted instead via
// [`with_current_compat`].
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

/// Register `state` as the [`CompatState`] for process `pid`, replacing any
/// previous entry. Called by `run_linux_binary` (task 13.3) when launching a
/// Compat_Process.
pub fn install_compat(pid: u64, state: CompatState) {
    COMPAT_STATES.lock().insert(pid, state);
}

/// Remove and return the [`CompatState`] for process `pid`, if any. Called when
/// a process terminates (`scheduler::exit_current`).
pub fn remove_compat(pid: u64) -> Option<CompatState> {
    COMPAT_STATES.lock().remove(&pid)
}

/// Whether the currently-running process (per `scheduler::current_pid`) has a
/// registered [`CompatState`] — i.e. is a Linux `Compat_Process` rather than a
/// pagh-native task. The dispatcher uses this to decide precedence: a process
/// with compat state gets full Linux syscall semantics; a native task without
/// it keeps the legacy pagh-native routing.
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
    let mut guard = COMPAT_STATES.lock();
    guard.get_mut(&pid).map(f)
}
