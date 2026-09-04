//! Signal delivery machinery (Phase 1) — the kernel-only glue between the
//! pure frame ABI ([`super::signal_frame`]), the per-process signal state in
//! [`crate::task::compat`], and the syscall entry path.
//!
//! ## Where signals are delivered
//!
//! Phase 1 delivers at exactly ONE point: the tail of
//! [`super::linux_dispatch`], after the syscall's `Result` has been folded
//! but before the entry stub writes the return value and unwinds the
//! `SavedRegs` frame. A pending, unblocked signal with a user handler gets an
//! `rt_sigframe` built on the user stack and the saved registers rewritten so
//! `sysretq` lands in the handler; a default-terminate signal kills the
//! process right there. Signals do NOT yet interrupt blocking kernel waits
//! (no `EINTR`) — that is Phase 2.
//!
//! ## Path assumption: compat ⇒ `syscall_entry`
//!
//! Signal delivery only ever runs for Compat_Processes (native tasks have no
//! signal state), and a Compat_Process enters syscalls exclusively through
//! the `syscall`-instruction stub. On that path the per-task user-RSP slot at
//! `SavedRegs + 120` holds the live user RSP (the same slot `execve` and
//! `clone` already read/write), the saved `rcx` is the user RIP and the saved
//! `r11` is the user RFLAGS. An `int 0x80`-entering compat process would find
//! a different meaning in those slots; no known user exists (glibc always
//! uses `syscall`), and this module refuses to deliver when the decoded
//! frame fails its sanity checks.
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
    UC_OFFSET, UNBLOCKABLE_MASK,
};

use crate::task::compat;
use crate::task::scheduler;

/// Clean x86_64 user RFLAGS for handler entry: reserved bit 1 set, IF set,
/// all system flags clear. Linux sanitizes the interrupted flags the same way
/// when delivering a signal.
const USER_RFLAGS: u64 = 0x202;

/// Conservative count of enqueued pending signals across all processes. The
/// delivery check on EVERY syscall return is `load(==0) → skip` — a pure
/// atomic read; the value may overcount (cleared pendings, exited processes)
/// but never undercount an actually-deliverable signal, so the optimistic
/// skip is always safe.
static PENDING_APPROX: AtomicU64 = AtomicU64::new(0);

// ─── Sending ─────────────────────────────────────────────────────────────────

/// Queue signal `sig` for process `pid` (Phase 1 semantics):
///
/// * `sig == 0`: existence probe (`kill(pid, 0)` semantics).
/// * `SIGKILL`: immediate outside-kill — the target's compat state is torn
///   down with exit code `128 + 9` recorded for `wait4`, and the task is
///   marked exiting (it notices at its next tick/yield).
/// * everything else: appended to the target's pending bitset and delivered
///   at its next syscall return (or Phase-2 wait wake-up).
///
/// Pending signals are per-thread: `pid` is addressed exactly (no
/// group-wide broadcast yet — Phase 2, together with `kill(2)`).
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
    if !compat::set_pending(pid, sig) {
        return Err(Errno::ESRCH);
    }
    PENDING_APPROX.fetch_add(1, Ordering::Relaxed);
    Ok(())
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

// ─── Delivery ────────────────────────────────────────────────────────────────

/// Deliver ONE pending signal for the current process, if any is deliverable
/// (pending ∧ ¬blocked), at the syscall-return point. Called from the
/// [`super::linux_dispatch`] epilogue with `result` = the syscall's folded
/// return value (stored into the frame's saved `rax`, so a handler that later
/// returns via `rt_sigreturn` resumes with the correct syscall result).
///
/// Default actions without a user handler:
///   * `Term` → the process exits with status `128 + sig` (diverges, never
///     returns — mirrors `tgkill`'s Phase-0 fatal semantics);
///   * `Ignore`/`Stop`/`Cont` → dropped (the scheduler has no stop/continue
///     yet; Phase 1 logs nothing for these — they are the signal numbers the
///     Phase-0 stubs already ignored silently).
///
/// Frame placement failure (region below RSP not mapped — a guard page or a
/// nearly-exhausted stack) is treated as `SIGSEGV`: the process exits with
/// `128 + 11` (139). Linux would grow the stack; our stack-growth fault path
/// is not reachable from here yet.
pub fn deliver_one_pending(regs: &mut SavedRegs, result: u64) {
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
            _ => return,
        }
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
