//! System-wide tick and event accounting behind `/proc/stat` (issue #11, slice 2).
//!
//! The timer tick is charged to **exactly one** bucket in [`crate::task::scheduler::scheduler_tick_irq`],
//! once per interrupt, from the frame the CPU interrupted:
//!
//!   * ring 3 (`CS.RPL == 3`) -> user;
//!   * the idle task, or a CPU parked in `hlt` -> idle;
//!   * any other ring 0 frame -> system.
//!
//! The classification is done *before* the requeue/switch decision, so a tick
//! belongs to the context that was interrupted, not to the task the scheduler
//! happens to pick next. No path charges a second bucket for the same tick.
//!
//! ## Units and CPUs
//!
//! Counters are sampled tick counts. `TICK_HZ == USER_HZ == 1000`
//! (`arch/x86_64/apic.rs`; `src/vfs/procfs.rs` has the compile-time assertion),
//! so the raw values are already the numbers Linux `/proc/stat` expects. The
//! machine is single-CPU (`AGENTS.md`, invariant 3): the three counters are the
//! per-CPU set, and the renderer prints the same sums as the aggregate `cpu`
//! line and as `cpu0`, exactly as Linux does for a one-CPU machine.
//!
//! ## Snapshot consistency
//!
//! A tick updates one bucket under a seqlock ([`SEQ`]) so [`cpu_ticks`] returns
//! a snapshot whose three fields all belong to the same set of completed ticks.
//! That keeps `user + system + idle` a meaningful total; each field is monotonic
//! on its own regardless. The tick path is lock- and allocation-free: three
//! `AtomicU64` operations at 1000 Hz.
//!
//! The module is `core`-only and self-contained, so `host-tests` includes this
//! exact source (`task_stat`) and property-tests [`classify_tick`].

use core::sync::atomic::{AtomicU64, Ordering};

/// Linux `USER_HZ` / this kernel's tick rate. The renderer divides nothing:
/// one tick is one `USER_HZ` unit.
pub const USER_HZ: u64 = 1000;

/// Which bucket one timer tick belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickClass {
    /// Ring 3: the CPU was executing a user program.
    User,
    /// Ring 0, but not idle: kernel work (syscalls, drivers, the shell, …).
    System,
    /// The idle task or an `hlt` wait: no runnable work.
    Idle,
}

/// Classify one timer tick from the interrupted frame (`must be the value the
/// CPU saved, not the scheduler's opinion of the next task`).
///
/// User first: a ring-3 frame can never be the idle task or inside `hlt`
/// (`hlt` is privileged). Of the remaining ring-0 frames, the idle task and an
/// explicit `hlt` wait are the two ways this kernel says "nothing to run".
#[inline]
pub fn classify_tick(interrupted_cs: u64, idle_task: bool, in_halt: bool) -> TickClass {
    if interrupted_cs & 3 == 3 && interrupted_cs != 0 {
        TickClass::User
    } else if idle_task || in_halt {
        TickClass::Idle
    } else {
        TickClass::System
    }
}

static USER_TICKS: AtomicU64 = AtomicU64::new(0);
static SYSTEM_TICKS: AtomicU64 = AtomicU64::new(0);
static IDLE_TICKS: AtomicU64 = AtomicU64::new(0);
/// Odd while a tick is being charged, even when the counters are consistent.
static SEQ: AtomicU64 = AtomicU64::new(0);

static CONTEXT_SWITCHES: AtomicU64 = AtomicU64::new(0);
static PROCESSES: AtomicU64 = AtomicU64::new(0);
static BOOT_UNIX: AtomicU64 = AtomicU64::new(0);

/// A consistent snapshot of the three timing buckets, in ticks (`USER_HZ`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTicks {
    pub user: u64,
    pub system: u64,
    pub idle: u64,
}

/// Charge one elapsed tick to exactly one bucket. Called only by the timer
/// tick handler; atomics only, no locks, no allocation.
#[inline]
pub fn charge(class: TickClass) {
    SEQ.fetch_add(1, Ordering::Relaxed);
    match class {
        TickClass::User => USER_TICKS.fetch_add(1, Ordering::Relaxed),
        TickClass::System => SYSTEM_TICKS.fetch_add(1, Ordering::Relaxed),
        TickClass::Idle => IDLE_TICKS.fetch_add(1, Ordering::Relaxed),
    };
    // Release: the bucket increment above is visible before the reader can see
    // the even sequence number that ends the critical section.
    SEQ.fetch_add(1, Ordering::Release);
}

/// Atomically read all three buckets from the same set of completed ticks.
pub fn cpu_ticks() -> CpuTicks {
    loop {
        let seq = SEQ.load(Ordering::Acquire);
        if seq & 1 == 0 {
            let user = USER_TICKS.load(Ordering::Relaxed);
            let system = SYSTEM_TICKS.load(Ordering::Relaxed);
            let idle = IDLE_TICKS.load(Ordering::Relaxed);
            if SEQ.load(Ordering::Acquire) == seq {
                return CpuTicks { user, system, idle };
            }
        }
        // A tick preempted the reader between a field and the closing check:
        // retry. There is one CPU, so the writer cannot be running concurrently
        // with this loop — only the preemption window makes the retry reachable.
        core::hint::spin_loop();
    }
}

/// Count one context switch (a different task is about to run).
#[inline]
pub fn count_context_switch() {
    CONTEXT_SWITCHES.fetch_add(1, Ordering::Relaxed);
}

/// Number of context switches since boot.
#[inline]
pub fn context_switches() -> u64 {
    CONTEXT_SWITCHES.load(Ordering::Relaxed)
}

/// Count one newly created task. Linux's `processes` field is `total_forks`,
/// incremented for every `copy_process` (threads and kernel threads included);
/// this kernel's analogue is `scheduler::spawn`.
#[inline]
pub fn count_process() {
    PROCESSES.fetch_add(1, Ordering::Relaxed);
}

/// Number of tasks created since boot (Linux-`total_forks`-style counter).
#[inline]
pub fn processes() -> u64 {
    PROCESSES.load(Ordering::Relaxed)
}

/// Record the boot wall-clock time (Unix seconds) read from the RTC at boot.
/// `0` means the RTC was unreadable/pre-epoch; `/proc/stat` then prints
/// `btime 0`, which is the documented gap rather than a fabricated date.
pub fn set_boot_unix(secs: u64) {
    BOOT_UNIX.store(secs, Ordering::Relaxed);
}

/// Boot wall-clock time in Unix seconds, or `0` when the RTC had no valid time.
#[inline]
pub fn boot_unix() -> u64 {
    BOOT_UNIX.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_order() {
        assert_eq!(classify_tick(0x1b, false, false), TickClass::User);
        assert_eq!(classify_tick(0x1b, true, true), TickClass::User);
        assert_eq!(classify_tick(0x08, true, false), TickClass::Idle);
        assert_eq!(classify_tick(0x08, false, true), TickClass::Idle);
        assert_eq!(classify_tick(0x08, false, false), TickClass::System);
    }
}
