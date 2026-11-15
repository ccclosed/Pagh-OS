//! Pure address-space (CR3) reference accounting for teardown decisions.
//!
//! A user PML4 frame may be freed only while NO live task can still run on it:
//! freeing it under a task that later wakes up is a use-after-free of page
//! tables, and every page-table page it contained can already have been
//! recycled into an unrelated mapping (round-3 audit F-r3-lead-1).
//!
//! The kernel's scheduler keeps live tasks in three separate registries, and a
//! teardown path must consult ALL of them:
//!
//!   * [`Registry::Ready`] — the ready queue. Note that this scheduler has no
//!     sleeping TCB state: a task blocked in `nanosleep`/`futex`/`read`/`wait4`
//!     yields cooperatively and stays RUNNABLE in this queue, so a "sleeping
//!     sibling" is found here too.
//!   * [`Registry::Stopped`] — tasks parked by SIGSTOP/SIGTSTP. Their saved
//!     frame is OUT of the ready queue until SIGCONT, and this is the state
//!     that made `execve`-time `drop_exclusive_user_space` free the shared
//!     address space of a stopped sibling thread.
//!   * [`Registry::PendingReap`] — tasks dropped by a tick whose resources are
//!     reclaimed by a later tick. Their CR3 is still owned and will be freed
//!     by the reaper, so a second free from another path is a double free.
//!
//! This module is `core`-only and dependency-free, so `host-tests` includes the
//! exact same source with `#[path]` and property-tests the policy the kernel
//! runs (R11.6).

#![allow(dead_code)]

/// One live task registration: its pid and the physical PML4 it runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsRef {
    /// Task id.
    pub pid: u64,
    /// Physical address of the task's user PML4 (`Tcb::cr3`).
    pub cr3: u64,
}

/// Which scheduler registry a blocking reference was found in. Reported for
/// the log line that explains why an address space was not freed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registry {
    /// Ready queue (includes cooperatively blocked syscalls).
    Ready,
    /// `STOPPED_TASKS` (SIGSTOP/SIGTSTP parked frames).
    Stopped,
    /// `PENDING_REAPS` (dropped, not yet reclaimed).
    PendingReap,
}

impl Registry {
    /// Short name for kernel log lines.
    pub fn name(self) -> &'static str {
        match self {
            Registry::Ready => "ready",
            Registry::Stopped => "stopped",
            Registry::PendingReap => "pending-reap",
        }
    }
}

/// First registration in ONE registry snapshot that references `cr3` and is not
/// `exclude_pid`.
///
/// `exclude_pid` is the task asking (the exec'ing task is not in any registry,
/// but the reaper IS in `PENDING_REAPS` with its own pid and must not count
/// itself).
pub fn referencing_pid<I>(cr3: u64, exclude_pid: u64, tasks: I) -> Option<u64>
where
    I: IntoIterator<Item = AsRef>,
{
    tasks
        .into_iter()
        .find(|t| t.pid != exclude_pid && t.cr3 == cr3)
        .map(|t| t.pid)
}

/// The complete teardown policy over all three registry snapshots: the first
/// task (and the registry it came from) that still references `cr3`.
///
/// A result of `None` means the address space is exclusively owned by the
/// caller and may be freed. Registries are scanned in the order the kernel
/// takes their locks (`READY_QUEUE` → `STOPPED_TASKS` → `PENDING_REAPS`); the
/// kernel applies [`referencing_pid`] one lock at a time, and this combined
/// form exists so the host property test can pin the full policy.
pub fn referenced_by(
    cr3: u64,
    exclude_pid: u64,
    ready: &[AsRef],
    stopped: &[AsRef],
    pending: &[AsRef],
) -> Option<(u64, Registry)> {
    for (tasks, registry) in [
        (ready, Registry::Ready),
        (stopped, Registry::Stopped),
        (pending, Registry::PendingReap),
    ] {
        if let Some(pid) = referencing_pid(cr3, exclude_pid, tasks.iter().copied()) {
            return Some((pid, registry));
        }
    }
    None
}
