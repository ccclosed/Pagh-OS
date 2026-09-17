//! Signal delivery machinery — the kernel-only glue between the pure frame ABI
//! ([`super::signal_frame`]), the pure `kill(2)` argument/errno model
//! ([`super::kill`]), the per-process signal state in [`crate::task::compat`],
//! and the syscall entry path.
//!
//! ## Where signals are delivered
//!
//! Phase 1 delivers at exactly ONE point: the tail of
//! [`super::linux_dispatch`], after the syscall's `Result` has been folded
//! but before the entry stub writes the return value and unwinds the
//! `SavedRegs` frame. A pending, unblocked signal with a user handler gets an
//! `rt_sigframe` built on the user stack and the saved registers rewritten so
//! `sysretq` lands in the handler; a default-terminate signal kills the
//! process right there. Blocking kernel waits are interrupted through the
//! `EINTR` checks the wait loops perform with [`has_deliverable_current`].
//!
//! Not yet wired (tracked by issue #12): timer-tick delivery (a CPU-bound loop
//! with no syscalls still never sees a signal) and SIGSTOP/SIGCONT scheduling.
//!
//! ## Sending: `kill(2)`, `tgkill` and the group broadcast
//!
//! [`send_signal`] queues onto ONE thread. [`sys_kill`] (`kill`, nr 62) decodes
//! the `pid_t`/`int` arguments, classifies the target (`pid > 0`, `0`, `-1`,
//! `-pgid`) via [`super::kill`], resolves it to a SNAPSHOT of pids and queues the
//! signal once per addressed thread group (`kill::pick_group_target`), which is
//! what Linux's shared group-pending list achieves. Errno decisions (EINVAL
//! before any lookup, `ESRCH` for `INT_MIN` and empty groups, `EPERM`
//! structurally unreachable under the single-uid model) are documented on
//! `kill::kill_args`.
//!
//! ## Path assumption: compat ⇒ `syscall_entry`
//!
//! Signal delivery only ever runs for Compat_Processes (native tasks have no
//! signal state), and a Compat_Process enters syscalls exclusively through
//! the `syscall`-instruction stub. On that path the per-task user-RSP slot at
//! `SavedRegs + 120` holds the live user RSP (the same slot `execve` and
//! `clone` already read/write), the saved `rcx` is the user RIP and the saved
//! `r11` is the user RFLAGS. An `int 0x80`-entering compat process would find
//! a different meaning in those slots — the CPU-pushed user RIP at `+120` — so
//! the frame is never interpreted blind. Two guards enforce this, in order:
//!
//!   1. **Exact**: `linux_dispatch` records every `int 0x80` entry on the
//!      process (`linux::INT80_ENTRY`, `compat::current_int80_entry`). A
//!      process observed on that path never gets a frame: the signal is
//!      consumed, one `[signal] ... refused: int 0x80 entry` diagnostic is
//!      printed, and a default-fatal signal is executed through
//!      [`force_terminate_group`] instead of being written into user memory.
//!   2. **Last resort**: [`frame_carries_syscall_view`] rejects any frame whose
//!      saved `r11` lacks RFLAGS bit 1 (architecturally always set) — a `syscall`
//!      entry cannot produce such a frame, so an entry path nobody has thought
//!      of yet degrades to "signal consumed, nothing written" rather than to a
//!      corrupted user address space.
//!
//! ## Register contract recap
//!
//! * Delivery: `RSP=frame`, `RIP=sa_handler`, `RDI=signo`, `RSI=&siginfo`,
//!   `RDX=&ucontext`, `RAX=syscall result`, clean user RFLAGS. `sigcontext`
//!   stores `cx = ip` and `r11 = flags` (the `syscall` clobber — identical to
//!   Linux's syscall-return delivery view).
//! * `rt_sigreturn`: mcontext GPRs are restored into the `SavedRegs` frame;
//!   `rcx ← sigcontext.ip` (sysret RIP), `r11 ← sigcontext.flags`, the slot
//!   `+120 ← sigcontext.sp`, and the function's return value becomes the
//!   restored `rax` (the stub writes the dispatcher's return into the `rax`
//!   slot after we return).
#![allow(dead_code)]

use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use super::check_user_ptr;
use super::errno::Errno;
use super::regs::SavedRegs;
use super::signal_frame::{
    decode_rt_sigframe, default_action, encode_rt_sigframe, frame_location, is_user_handler,
    DefaultAction, RestoredFrame, RT_SIGFRAME_SIZE, SIGINFO_OFFSET, SIGKILL, SIG_DFL, SIG_IGN,
    UC_OFFSET, UNBLOCKABLE_MASK, USER_RFLAGS,
};
use super::trap_frame::{plan_irq_delivery, IrqFrame};

use crate::task::compat;
use crate::task::scheduler;

/// Conservative count of enqueued pending signals across all processes. The
/// delivery check on EVERY syscall return is `load(==0) → skip` — a pure
/// atomic read; the value may overcount (cleared pendings, exited processes)
/// but never undercount an actually-deliverable signal, so the optimistic
/// skip is always safe.
static PENDING_APPROX: AtomicU64 = AtomicU64::new(0);

// ─── Sending ─────────────────────────────────────────────────────────────────

/// Queue signal `sig` for process `pid` (single-thread primitive):
///
/// * `sig == 0`: existence probe (`kill(pid, 0)` semantics).
/// * `SIGKILL`: immediate outside-kill — the target's compat state is torn
///   down with exit code `128 + 9` recorded for `wait4`, and the task is
///   marked exiting (it notices at its next tick/yield). A PARKED (stopped)
///   task is removed from `STOPPED_TASKS` and reaped with its own cr3 (see
///   `scheduler::request_exit`).
/// * everything else: appended to the target's pending bitset and delivered
///   at its next syscall return (or by the `EINTR` checks of its blocking
///   waits).
///
/// ## Stop/continue magic happens HERE, at generation time
///
/// Linux splits the two halves of job control (`prepare_signal`): the *stop* is
/// done as a signal action at delivery, but the *continue* is done immediately
/// when `SIGCONT` is generated — "regardless of blocking, ignoring, or
/// handling" — because a stopped task has no execution context that could
/// observe a pending bit. This function therefore performs, for the target's
/// THREAD GROUP:
///
/// * `SIGCONT`: resume every parked member ([`scheduler::resume_stopped`]) and
///   discard all pending stop-class bits (POSIX 2.4.1). Unconditional — a
///   blocked or `SIG_IGN`ed `SIGCONT` still resumes; only the signal's own
///   delivery follows the normal mask/disposition rules afterwards.
/// * a stop-class signal (`SIGSTOP`/`SIGTSTP`/`SIGTTIN`/`SIGTTOU`): discard a
///   pending `SIGCONT` (POSIX: generating a stop signal discards it), and if the
///   group is ALREADY stopped, consume the signal without queueing it — otherwise
///   the resume would immediately re-stop the group.
///
/// Pending signals are per-thread: `pid` is addressed exactly. Group-addressed
/// calls (`kill(0|-1|-pgid)`) go through [`sys_kill`], which resolves the group
/// to a single thread per group before calling this (Linux queues a group signal
/// on the group's shared pending list, so the handler runs once per group).
pub fn send_signal(pid: u64, sig: u64) -> Result<(), Errno> {
    if sig == 0 {
        return if compat::compat_exists(pid) {
            Ok(())
        } else {
            Err(Errno::ESRCH)
        };
    }
    if sig > super::signal_frame::SIGNAL_COUNT as u64 {
        return Err(Errno::EINVAL);
    }
    if !compat::compat_exists(pid) {
        return Err(Errno::ESRCH);
    }
    if sig == SIGKILL {
        force_terminate_group(pid, SIGKILL);
        return Ok(());
    }
    // ── generation-time stop/continue effects (see the doc above) ───────────
    let tgid = compat::tgid_of(pid);
    if sig == super::signal_frame::SIGCONT {
        resume_group(tgid);
    } else if super::signal_frame::is_stop_signal(sig) {
        for member in compat::group_pids(tgid) {
            compat::clear_cont_pending(member);
        }
        if group_is_stopped(tgid) {
            crate::info!(
                "[signal] pid={} sig={} ignored: thread group {} is already stopped",
                pid,
                sig,
                tgid
            );
            return Ok(());
        }
    }
    if !compat::set_pending(pid, sig) {
        return Err(Errno::ESRCH);
    }
    PENDING_APPROX.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Is any member of thread group `tgid` parked in the stopped state?
fn group_is_stopped(tgid: u64) -> bool {
    compat::group_pids(tgid)
        .into_iter()
        .any(scheduler::is_stopped)
}

/// `SIGCONT` generation for thread group `tgid`: discard every pending
/// stop-class bit and put every parked member back into rotation.
///
/// One registry snapshot, then per-member scheduler calls — `COMPAT_STATES` is
/// never held across them (AGENTS.md invariant 4).
fn resume_group(tgid: u64) {
    for member in compat::group_pids(tgid) {
        compat::clear_stop_pending(member);
        // Snapshot the parked state BEFORE resuming: only a member that really was
        // parked produces a WCONTINUED report (a SIGCONT to a running process just
        // discards stop bits).
        let was_parked = scheduler::is_stopped(member);
        if scheduler::resume_stopped(member) && was_parked {
            compat::note_child_continued(member);
            // The stuck-syscall clock restarts: the paused interval must not count
            // as "stuck in syscall" for the watchdog (see `watchdog_tick`).
            super::inflight_refresh(member);
            crate::info!("[signal] pid={} resumed by SIGCONT", member);
        }
    }
}

/// Apply a DELIVERED stop-class signal whose disposition is `SIG_DFL`: the whole
/// thread group stops (Linux group stop).
///
/// The receiver parks at THIS delivery point — the pending bit has already been
/// consumed by the caller — while members whose frames are already saved are
/// moved out of rotation immediately. `mark_stop_requested` makes the next
/// requeue decision (this yield, or a tick that fires first) move the frame into
/// `STOPPED_TASKS`; we never return to ring 3 with a stop pending.
fn begin_group_stop(sig: u64) {
    let pid = scheduler::current_pid();
    let tgid = compat::tgid_of(pid);
    let siblings: alloc::vec::Vec<u64> = compat::group_pids(tgid)
        .into_iter()
        .filter(|m| *m != pid)
        .collect();
    let parked = scheduler::stop_ready_pids(&siblings);
    compat::note_child_stopped(pid, sig);
    crate::info!(
        "[signal] pid={} sig={} stopping thread group {} ({} sibling(s) parked now, receiver parks at this return point)",
        pid,
        sig,
        tgid,
        parked
    );
    scheduler::mark_stop_requested(pid);
}

/// Stop-signal delivery at a syscall return: mark the group stopped, then park the
/// receiver by yielding — the requeue decision inside `yield_switch` sees the stop
/// request and parks this frame instead of rotating it.
fn stop_current_group(sig: u64) {
    begin_group_stop(sig);
    scheduler::yield_current();
}

/// Outside-kill of the thread group `pid` belongs to, recording exit status
/// `128 + sig` for `wait4` (a plain `request_exit` would drop the compat
/// state without ever inserting the zombie entry).
fn force_terminate_group(pid: u64, sig: u64) {
    let tgid = compat::tgid_of(pid);
    for member in compat::group_member_pids(tgid, pid) {
        scheduler::request_exit(member);
    }
    let code = (128u64 + sig) as u8;
    compat::set_exit_code_of(pid, code);
    // Removes the compat state and inserts the wait4 zombie with the code
    // recorded above.
    compat::finish_compat_exit(pid);
    // Now a no-op removal + the actual mark-exiting.
    scheduler::request_exit(pid);
}

// ─── kill(2) ─────────────────────────────────────────────────────────────────

/// The uid of every process in pagh's single-user model (`misc::sys_getid`
/// returns 0 for every caller) — the CAP_KILL-equivalent that makes `EPERM`
/// structurally unreachable in [`deliver_to_targets`].
const SINGLE_UID: u32 = 0;

/// `kill(2)` (62): send `sig` to a process, a process group, or every process
/// the caller may signal.
///
/// The pure half lives in [`super::kill`]: argument decoding (both parameters
/// are C `int`s, so they are truncated to 32 bits and sign-extended — `kill(-pgid)`
/// depends on it), target classification, and the errno matrix. See
/// `kill::kill_args` for why each error is returned where it is: `EINVAL` for an
/// invalid signal BEFORE any target lookup, `ESRCH` for `INT_MIN` and for a
/// group with no live member, `EPERM` structurally unreachable under the
/// single-uid model.
///
/// Locking (AGENTS.md invariant 4): [`resolve_kill_targets`] takes one short
/// registry snapshot and releases it; the sends happen afterwards, never while
/// `COMPAT_STATES` is held (each `send_signal` re-enters the registry itself).
pub fn sys_kill(pid_raw: u64, sig_raw: u64) -> Result<u64, Errno> {
    let (target, sig) = super::kill::kill_args(
        super::kill::decode_pid(pid_raw),
        super::kill::decode_sig(sig_raw),
    )?;
    let targets = resolve_kill_targets(target);
    deliver_to_targets(&targets, sig)
}

/// Resolve a classified `kill(2)` target into the exact pids a signal is queued
/// on: ONE thread per addressed thread group (`kill::pick_group_target`), not
/// every member — the equivalent of Linux queueing on the group's shared pending
/// list, so a handler installed by a multi-threaded process runs once.
///
/// An empty result means "no such process/group" and becomes `ESRCH` in
/// [`deliver_to_targets`]. All registry iteration happens inside the snapshot
/// helpers here; nothing is delivered while their lock is held.
pub fn resolve_kill_targets(target: super::kill::KillTarget) -> alloc::vec::Vec<u64> {
    use super::kill::KillTarget;
    match target {
        // Positive pid: addressed exactly (Phase-1 per-thread semantics — the
        // module doc records this deviation from Linux's group delivery). No
        // registry walk: existence is checked by `send_signal` itself.
        KillTarget::Pid(pid) => alloc::vec![pid],
        // pid == 0: the caller's own process group. In pagh a process group IS a
        // thread group (`sys_getpgid` reports the tgid, `sys_setpgid` is a
        // documented no-op), so its members are the pids with that tgid.
        KillTarget::OwnGroup => {
            let tgid = compat::current_tgid();
            super::kill::pick_group_target(&compat::group_pids(tgid), tgid)
                .into_iter()
                .collect()
        }
        // pid < -1: the process group `-pid`.
        KillTarget::Group(pgid) => super::kill::pick_group_target(&compat::group_pids(pgid), pgid)
            .into_iter()
            .collect(),
        // pid == -1: every signalable process except the caller's whole thread
        // group and pid 1 (`kill::INIT_PID`; man7 kill(2) NOTES). One snapshot,
        // then grouped by tgid so each process still receives exactly one copy.
        KillTarget::All => {
            let own = compat::current_tgid();
            let mut groups: alloc::collections::BTreeMap<u64, alloc::vec::Vec<u64>> =
                alloc::collections::BTreeMap::new();
            for (pid, tgid) in compat::compat_pid_tgid_snapshot() {
                if tgid == own || pid == super::kill::INIT_PID {
                    continue;
                }
                groups.entry(tgid).or_default().push(pid);
            }
            groups
                .iter()
                .filter_map(|(tgid, members)| super::kill::pick_group_target(members, *tgid))
                .collect()
        }
    }
}

/// Send `sig` to every resolved target and fold the per-target results into the
/// single `kill(2)` return value.
///
/// Permission policy: pagh runs a single uid ([`SINGLE_UID`]) for both sender and
/// target, which is the CAP_KILL-equivalent, so every target is permitted and
/// `EPERM` cannot be returned today. It is still a real call to
/// `kill::kill_permits` rather than a comment, so a future credential model has
/// exactly one place to change.
///
/// Errno folding (Linux: a group send succeeds when at least one target was
/// reached — `__kill_pgrp_info`): `Ok(0)` as soon as one delivery succeeded,
/// otherwise the last per-target error. For a target that existed when the
/// snapshot was taken, `ESRCH` is the only failure `send_signal` can produce (the
/// target exited in between).
fn deliver_to_targets(targets: &[u64], sig: u64) -> Result<u64, Errno> {
    if targets.is_empty() {
        return Err(Errno::ESRCH);
    }
    let permitted: alloc::vec::Vec<u64> = targets
        .iter()
        .copied()
        .filter(|_| super::kill::kill_permits(SINGLE_UID, SINGLE_UID))
        .collect();
    if permitted.is_empty() {
        return Err(Errno::EPERM);
    }
    let mut delivered = 0usize;
    let mut last_err = Errno::ESRCH;
    for pid in permitted {
        match send_signal(pid, sig) {
            Ok(()) => delivered += 1,
            Err(e) => last_err = e,
        }
    }
    if delivered == 0 {
        Err(last_err)
    } else {
        Ok(0)
    }
}

// ─── Delivery ────────────────────────────────────────────────────────────────

/// Does the saved-register frame carry the `syscall`-entry meaning the delivery
/// contract requires?
///
/// On the `syscall` path the CPU stores the user RFLAGS in `r11`, and RFLAGS bit
/// 1 is architecturally always set; on any other entry the slot holds an ordinary
/// user GPR. A frame failing this check therefore cannot have come from
/// `syscall`, and the `+120` word in it is NOT the user RSP — writing an
/// `rt_sigframe` "below it" would land in whatever that value points at (for
/// `int 0x80`: the user's own code page).
///
/// This is the last-resort half of the invariant-2 guard: the exact half is the
/// per-process `int 0x80` observation (`compat::current_int80_entry`), checked
/// first. Cheap, no lock, and cannot reject a genuine `syscall` frame.
fn frame_carries_syscall_view(regs: &SavedRegs) -> bool {
    regs.r11 & 0x2 != 0
}

/// Deliver ONE pending signal for the current process, if any is deliverable
/// (pending ∧ ¬blocked), at the syscall-return point. Called from the
/// [`super::linux_dispatch`] epilogue with `result` = the syscall's folded
/// return value (stored into the frame's saved `rax`, so a handler that later
/// returns via `rt_sigreturn` resumes with the correct syscall result).
///
/// Default actions without a user handler:
///   * `Term` → the process exits with status `128 + sig` (diverges, never
///     returns — mirrors `tgkill`'s Phase-0 fatal semantics);
///   * `Ignore`/`Stop`/`Cont` → dropped (the scheduler has no stop/continue yet
///     — see issue #12 tasks t8/t9; this is the only delivery point until the
///     timer-tick path lands, so a CPU-bound loop with no syscalls still never
///     sees a signal).
///
/// Frame placement failure (region below RSP not mapped — a guard page or a
/// nearly-exhausted stack) is treated as `SIGSEGV`: the process exits with
/// `128 + 11` (139). Linux would grow the stack; our stack-growth fault path
/// is not reachable from here yet.
///
/// The signal is ALWAYS consumed before any of the refusal paths below (the
/// pending bit is cleared and `PENDING_APPROX` decremented), so a refused
/// delivery can never spin: it is either converted into the signal's default
/// action or dropped with a diagnostic.
pub fn deliver_one_pending_syscall(regs: &mut SavedRegs, result: u64) {
    if PENDING_APPROX.load(Ordering::Relaxed) == 0 {
        return;
    }
    let Some((sig, action, old_blocked, altstack)) = compat::pick_pending_signal() else {
        return;
    };
    PENDING_APPROX.fetch_sub(1, Ordering::Relaxed);

    if action.handler == SIG_IGN {
        return;
    }
    // Invariant-2 guard, exact half: this process entered a syscall through
    // `int 0x80` at least once, so its saved registers do not mean what the
    // frame contract above requires. Never write an `rt_sigframe` for it; run the
    // default action instead and say so once per process.
    if compat::current_int80_entry() {
        if compat::note_int80_refused() {
            crate::warn!(
                "[signal] pid={} sig={} refused: int 0x80 entry path \
                 (AGENTS.md invariant 2) - no rt_sigframe written",
                scheduler::current_pid(),
                sig
            );
        }
        if action.handler == SIG_DFL && default_action(sig) == DefaultAction::Term {
            force_terminate_group(scheduler::current_pid(), sig);
        }
        return;
    }
    if action.handler == SIG_DFL {
        match default_action(sig) {
            DefaultAction::Term => {
                crate::info!(
                    "[signal] pid={} sig={} default-terminate (no handler)",
                    scheduler::current_pid(),
                    sig
                );
                // Diverges: exits the thread group with 128 + sig.
                super::misc::sys_exit_group(128 + sig);
            }
            DefaultAction::Stop => {
                // Stop is an ACTION (Linux: "the actual stopping ... is done as a
                // signal action for SIG_DFL"): park this task and its siblings. Does
                // not return until SIGCONT resumes us; the pending bit was already
                // consumed above, so the stop is not re-observed.
                stop_current_group(sig);
                return;
            }
            _ => return,
        }
    }

    // Invariant-2 guard, last resort (see the module doc): a frame that cannot
    // have come from `syscall` must not be interpreted as one.
    if !frame_carries_syscall_view(regs) {
        crate::warn!(
            "[signal] pid={} sig={} refused: saved RFLAGS={:#x} lacks bit 1 - \
             not a syscall-entry frame, no rt_sigframe written",
            scheduler::current_pid(),
            sig,
            regs.r11
        );
        return;
    }

    // User handler: build the frame below the interrupted RSP.
    let user_rsp = unsafe { ((regs as *mut SavedRegs as *const u64).add(15)).read() };
    let frame = frame_location(user_rsp, &action, &altstack);
    if check_user_ptr(frame, RT_SIGFRAME_SIZE).is_err() {
        crate::warn!(
            "[signal] pid={} sig={} frame {:#x} unmapped -> SIGSEGV exit",
            scheduler::current_pid(),
            sig,
            frame
        );
        super::misc::sys_exit_group(128 + crate::arch::x86_64::linux::signal_frame::SIGSEGV);
    }

    let saved = crate::arch::x86_64::linux::signal_frame::SigFrameRegs {
        r8: regs.r8,
        r9: regs.r9,
        r10: regs.r10,
        r11: regs.r11,
        r12: regs.r12,
        r13: regs.r13,
        r14: regs.r14,
        r15: regs.r15,
        di: regs.rdi,
        si: regs.rsi,
        bp: regs.rbp,
        bx: regs.rbx,
        dx: regs.rdx,
        ax: result,
        cx: regs.rcx,
        sp: user_rsp,
        ip: regs.rcx,
        flags: regs.r11,
    };
    let newmask = (old_blocked | action.mask) & !UNBLOCKABLE_MASK;
    let mut buf = [0u8; RT_SIGFRAME_SIZE as usize];
    encode_rt_sigframe(&mut buf, action.restorer, sig, &saved, newmask, &altstack);
    // SAFETY: the whole frame range was validated mapped+user-accessible by
    // `check_user_ptr` above, and this task exclusively owns its user stack
    // region while running.
    unsafe {
        ptr::copy_nonoverlapping(buf.as_ptr(), frame as *mut u8, buf.len());
        // New user RSP for the syscall-exit stub: the frame base.
        ((regs as *mut SavedRegs as *mut u64).add(15)).write(frame);
    }
    regs.rdi = sig;
    regs.rsi = frame + SIGINFO_OFFSET;
    regs.rdx = frame + UC_OFFSET;
    regs.rcx = action.handler; // sysretq RIP target
    regs.r11 = USER_RFLAGS;
    regs.rax = result;
    compat::block_during_handler(action.mask);
    crate::info!(
        "[signal] pid={} delivered sig={} handler={:#x} frame={:#x} (restorer={:#x})",
        scheduler::current_pid(),
        sig,
        action.handler,
        frame,
        action.restorer
    );
}

// ─── Timer-tick delivery ─────────────────────────────────────────────────────

/// What the timer-tick return path did with the interrupted task.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TickAction {
    /// Nothing to do: no deliverable signal, or the interrupted frame is a kernel
    /// frame whose user context is not here (the syscall return epilogue, or the
    /// `EINTR` checks of the blocking wait loops, will deliver it).
    None,
    /// An `rt_sigframe` was written and the iret frame rewritten in place; the
    /// caller must requeue the task with this frame.
    Delivered,
    /// The signal's default action stopped the task: the caller's requeue decision
    /// must park it (the stop request is already marked).
    Park,
    /// The task was terminated (a fatal default action, or an `rt_sigframe` that
    /// could not be placed): the caller must drop it — never requeue it — and reap
    /// it with its own address space.
    Kill,
}

/// Handle one pending signal for the CURRENT task at the timer-tick return point,
/// where `cur_rsp` is the frame `irq32_stub` just saved (see
/// [`super::trap_frame`]).
///
/// ## Why this is a separate path from [`deliver_one_pending_syscall`]
///
/// The two frames are different: at `cur_rsp` the word at `+120` is the
/// interrupted `rax` and the interrupted user context lives in the CPU-pushed
/// iret words, so the syscall-path code (which rewrites `rcx`/`r11` for `sysretq`
/// and the per-task user-RSP slot) would corrupt the task. Here the delivery
/// rewrites the iret frame: `RIP` = handler, `RSP` = frame base, `RFLAGS` =
/// [`USER_RFLAGS`], `RDI/RSI/RDX` = signo/`&siginfo`/`&ucontext`; `CS`, `SS`,
/// `rax` and the `popfq` word stay untouched.
///
/// ## Order of the checks (all of them load-bearing)
///
///   1. **Fatal action first** (any frame): a `SIG_DFL` default-terminate signal
///      needs no user frame, and the task must die whether it was in ring 3 or
///      inside a syscall. Terminates WITHOUT diverging — `sys_exit_group` must
///      never be called from IRQ context.
///   2. **Stop action next** (any frame): a stop parks the task at this tick's
///      requeue decision; unlike the syscall path it must NOT yield from IRQ
///      context.
///   3. **Ring-3 check BEFORE the pick**: a kernel frame has no user context here,
///      and consuming the bit would lose the signal forever (nothing re-queues a
///      signal). This is the one ordering rule the whole path depends on.
///   4. Then (and only then) consume the bit and build the frame. The picked
///      signal is checked against the peeked one and the frame is built from the
///      value that was actually taken, so the delivered signal can never be a
///      neighbour of the pending bit (the off-by-one class this code was written
///      next to).
pub fn tick_action(cur_rsp: u64) -> TickAction {
    if cur_rsp == 0 || PENDING_APPROX.load(Ordering::Relaxed) == 0 {
        return TickAction::None;
    }
    let pid = scheduler::current_pid();
    if !compat::compat_exists(pid) {
        return TickAction::None;
    }
    // SAFETY: the only caller is `scheduler_tick_irq`, which passes its own
    // `current_rsp` — the frame `irq32_stub` pushed on the CURRENT task's kernel
    // stack. That frame is 168 bytes, mapped and unaliased for the duration of
    // this call (each task owns a private kernel stack, and the tick runs with
    // interrupts masked, so nothing can preempt or alias it).
    let frame = unsafe { &mut *(cur_rsp as *mut IrqFrame) };

    // (1) What would the pending signal do? Peek WITHOUT consuming.
    let Some((sig, action, _blocked, _alt)) = compat::peek_deliverable_signal() else {
        return TickAction::None;
    };

    if action.handler == SIG_DFL && default_action(sig) == DefaultAction::Term {
        let _ = compat::pick_pending_signal();
        PENDING_APPROX.fetch_sub(1, Ordering::Relaxed);
        crate::info!(
            "[signal] pid={} sig={} tick-delivered default-terminate (no handler)",
            pid,
            sig
        );
        // Records 128 + sig for wait4 and marks the task exiting; the caller drops
        // it. Diverging here (exit_group) would be a halt loop inside the IRQ.
        force_terminate_group(pid, sig);
        return TickAction::Kill;
    }
    if action.handler == SIG_DFL && default_action(sig) == DefaultAction::Stop {
        let _ = compat::pick_pending_signal();
        PENDING_APPROX.fetch_sub(1, Ordering::Relaxed);
        begin_group_stop(sig);
        return TickAction::Park;
    }
    // (3) A kernel frame: no user context here, so consume NOTHING.
    if !frame.is_user_frame() {
        return TickAction::None;
    }
    if action.handler == SIG_IGN {
        let _ = compat::pick_pending_signal();
        PENDING_APPROX.fetch_sub(1, Ordering::Relaxed);
        return TickAction::None;
    }

    // (4) Consume the bit and deliver to the user handler.
    let Some((taken, action, old_blocked, altstack)) = compat::pick_pending_signal() else {
        return TickAction::None;
    };
    PENDING_APPROX.fetch_sub(1, Ordering::Relaxed);
    if taken != sig {
        // Impossible while interrupts are masked (nothing else may touch this
        // process's pending set), but if it ever happens the frame must be built
        // from the signal that was ACTUALLY taken — never from a neighbour.
        crate::error!(
            "[signal] pid={} tick delivery: peeked sig={} but picked sig={} - delivering the picked one",
            pid,
            sig,
            taken
        );
    }
    let user_rsp = frame.rsp;
    let frame_addr = frame_location(user_rsp, &action, &altstack);
    if check_user_ptr(frame_addr, RT_SIGFRAME_SIZE).is_err() {
        crate::warn!(
            "[signal] pid={} sig={} tick delivery: frame {:#x} unmapped -> SIGSEGV exit",
            pid,
            taken,
            frame_addr
        );
        force_terminate_group(pid, super::signal_frame::SIGSEGV);
        return TickAction::Kill;
    }
    let saved = frame.user_context();
    let newmask = (old_blocked | action.mask) & !UNBLOCKABLE_MASK;
    let mut buf = [0u8; RT_SIGFRAME_SIZE as usize];
    encode_rt_sigframe(&mut buf, action.restorer, taken, &saved, newmask, &altstack);
    // SAFETY: `check_user_ptr` above proved the whole RT_SIGFRAME_SIZE region is
    // mapped and user-accessible, and this task exclusively owns its user stack
    // while it is the running task (the tick runs on its kernel stack).
    unsafe {
        ptr::copy_nonoverlapping(buf.as_ptr(), frame_addr as *mut u8, buf.len());
    }
    frame.enter_handler(&plan_irq_delivery(
        frame_addr,
        taken,
        action.handler,
        USER_RFLAGS,
    ));
    compat::block_during_handler(action.mask);
    crate::info!(
        "[signal] pid={} tick-delivered sig={} handler={:#x} frame={:#x} (interrupted rip={:#x})",
        pid,
        taken,
        action.handler,
        frame_addr,
        saved.ip
    );
    TickAction::Delivered
}

// ─── rt_sigreturn ────────────────────────────────────────────────────────────

/// `rt_sigreturn` (15): restore the interrupted context from the
/// `rt_sigframe` the restorer is standing on.
///
/// The glibc restorer runs with `RSP = frame + 8` (`pretcode` was popped by
/// the handler's `ret`), which is exactly where `ucontext` starts — so the
/// entry user RSP (the `+120` slot) IS the ucontext address.
///
/// Returns the saved `rax`; [`super::linux_dispatch`]'s caller (the entry
/// stub) writes this into the saved `rax` slot, which is how the restored
/// `rax` reaches ring 3 despite the normal return-value plumbing.
pub fn sys_rt_sigreturn(regs: &mut SavedRegs) -> Result<u64, Errno> {
    let user_rsp = unsafe { ((regs as *mut SavedRegs as *const u64).add(15)).read() };
    check_user_ptr(user_rsp, 304)?;
    let mut uc = [0u8; 304];
    // SAFETY: range validated above; the frame was written by us (or the user
    // forged it — Linux trusts it likewise; a null-IP frame is rejected below).
    unsafe {
        ptr::copy_nonoverlapping(user_rsp as *const u8, uc.as_mut_ptr(), uc.len());
    }
    let Some(RestoredFrame { regs: fr, mask }) = decode_rt_sigframe(&uc) else {
        crate::warn!("[signal] rt_sigreturn: null-IP frame at {:#x}", user_rsp);
        return Err(Errno::EINVAL);
    };
    regs.r8 = fr.r8;
    regs.r9 = fr.r9;
    regs.r10 = fr.r10;
    regs.r12 = fr.r12;
    regs.r13 = fr.r13;
    regs.r14 = fr.r14;
    regs.r15 = fr.r15;
    regs.rdi = fr.di;
    regs.rsi = fr.si;
    regs.rbp = fr.bp;
    regs.rbx = fr.bx;
    regs.rdx = fr.dx;
    regs.rcx = fr.ip; // sysretq RIP
    regs.r11 = fr.flags; // sysretq RFLAGS
                         // SAFETY: the per-task user-RSP slot is this task's own kernel-stack
                         // storage (same slot execve/clone rewrite).
    unsafe {
        ((regs as *mut SavedRegs as *mut u64).add(15)).write(fr.sp);
    }
    compat::with_current_compat(|cs| cs.sig_blocked = mask);
    Ok(fr.ax)
}

// ─── Wait-loop introspection (Phase 2 consumes these) ────────────────────────

/// Whether the CURRENT process has a signal that would be delivered at the
/// next syscall return. Blocking syscall wait loops call this to convert a
/// delivered signal into an `-EINTR` return instead of sleeping through it.
pub fn has_deliverable_current() -> bool {
    if PENDING_APPROX.load(Ordering::Relaxed) == 0 {
        return false;
    }
    compat::with_current_compat(|cs| cs.sig_pending & !cs.sig_blocked != 0).unwrap_or(false)
}

/// Does the CURRENT process have a USER handler installed for `sig`?
/// `None` when there is no compat state (native task).
pub fn current_has_handler(sig: u64) -> Option<bool> {
    compat::current_action(sig).map(|a| is_user_handler(&a))
}

/// Disposition snapshot for `sig` in the CURRENT process (used by
/// `rt_sigaction`'s oldact read-back).
pub fn current_action_clone(
    sig: u64,
) -> Option<crate::arch::x86_64::linux::signal_frame::SignalAction> {
    compat::current_action(sig)
}
