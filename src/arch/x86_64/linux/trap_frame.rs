//! Pure mirror of the frame `task::switch::irq32_stub` leaves on the kernel stack,
//! plus the byte-level plan for delivering a signal from the timer-tick return
//! path.
//!
//! ## Why this module exists (and why the `SavedRegs` cast is forbidden)
//!
//! The tick handler receives `current_rsp`, the RSP right after `irq32_stub`
//! pushed its frame:
//!
//! ```text
//!   [rsp+0]    RFLAGS consumed by `popfq` (0x002: IF stays masked until `iretq`)
//!   [+8..+127] 15 GPRs in SavedRegs field order: r15@+8 … rax@+120
//!   [+128]     RIP     (the interrupted instruction)
//!   [+136]     CS      (RPL 3 = the task was in ring 3 when it was interrupted)
//!   [+144]     RFLAGS  (the interrupted user RFLAGS)
//!   [+152]     RSP     (the interrupted user RSP)
//!   [+160]     SS
//! ```
//!
//! The word at `+120` is `rax` — NOT the per-task user-RSP slot of the syscall
//! frame, which lives at `SavedRegs + 120` (i.e. `+128` here). Casting
//! `(current_rsp + 8) as *mut SavedRegs` and calling the syscall-path delivery
//! would therefore read the user's `rax` as a user stack pointer, write the frame
//! base into `rax`, and rewrite `rcx`/`r11` — which `iretq`, unlike `sysretq`,
//! never consumes: the handler would not run and the `rt_sigframe` would land on
//! whatever address `rax` happened to hold. This module is the typed, asserted
//! alternative; the two adapters (syscall frame vs IRQ frame) share only the
//! pure `signal_frame` encoder.
//!
//! ## Contract with `src/test.rs`
//!
//! `scheduler_layout_tests` asserts the same layout byte-for-byte with sentinel
//! values (word indices 0 = popfq, 10 = rdi, 16 = RIP, 19 = RSP); the `const`
//! assertions at the bottom of this file pin the SAME offsets against the real
//! `SavedRegs` type, so a reordering of either side stops the build.
#![allow(dead_code)]

use super::regs::SavedRegs;
use super::signal_frame::{SigFrameRegs, SIGINFO_OFFSET, UC_OFFSET};

/// The complete 21-word frame `irq32_stub` builds (168 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IrqFrame {
    /// `+0`: the RFLAGS word the restore path consumes with `popfq`. It stays
    /// `0x002` (IF masked) for the whole restore tail — `iretq` restores the real
    /// user IF from [`IrqFrame::rflags`]. Never set IF here: the window between
    /// `popfq` and `iretq` must not be interruptible.
    pub popfq_rflags: u64,
    /// `+8 .. +127`: the 15 GPRs, in `SavedRegs` order (`r15` lowest, `rax` at
    /// absolute `+120`).
    pub gpr: SavedRegs,
    /// `+128`: the interrupted RIP.
    pub rip: u64,
    /// `+136`: the interrupted CS.
    pub cs: u64,
    /// `+144`: the interrupted RFLAGS.
    pub rflags: u64,
    /// `+152`: the interrupted RSP.
    pub rsp: u64,
    /// `+160`: the interrupted SS.
    pub ss: u64,
}

impl IrqFrame {
    /// Was the task interrupted in ring 3? Only then does the frame carry a user
    /// context that an `rt_sigframe` may be built from: for a task preempted
    /// inside a syscall the CS is a kernel selector and the user context lives in
    /// the syscall entry frame up-stack, reachable only through that syscall's
    /// return epilogue (or its `EINTR` checks).
    #[inline]
    pub fn is_user_frame(&self) -> bool {
        self.cs & 3 == 3 && self.cs != 0
    }

    /// The user context this frame describes, in the shape
    /// [`super::signal_frame::encode_rt_sigframe`] consumes.
    pub fn user_context(&self) -> SigFrameRegs {
        SigFrameRegs {
            r8: self.gpr.r8,
            r9: self.gpr.r9,
            r10: self.gpr.r10,
            r11: self.gpr.r11,
            r12: self.gpr.r12,
            r13: self.gpr.r13,
            r14: self.gpr.r14,
            r15: self.gpr.r15,
            di: self.gpr.rdi,
            si: self.gpr.rsi,
            bp: self.gpr.rbp,
            bx: self.gpr.rbx,
            dx: self.gpr.rdx,
            ax: self.gpr.rax,
            cx: self.gpr.rcx,
            sp: self.rsp,
            ip: self.rip,
            flags: self.rflags,
        }
    }

    /// Enter `handler` at the next `iretq`: the exact in-place mutation set.
    ///
    /// `rax` (the live user `rax`, which is also the `sigcontext.rax` the
    /// `rt_sigreturn` will restore), `cs`, `ss` and the `popfq` word are NOT
    /// touched — the handler runs in ring 3, on the user stack, with a clean
    /// user RFLAGS, and the interrupted context is reachable only through the
    /// `rt_sigframe` written at `frame_addr`.
    pub fn enter_handler(&mut self, plan: &IrqDelivery) {
        self.rip = plan.rip;
        self.rflags = plan.rflags;
        self.rsp = plan.rsp;
        self.gpr.rdi = plan.rdi;
        self.gpr.rsi = plan.rsi;
        self.gpr.rdx = plan.rdx;
    }
}

/// Where the handler must be entered, and with which argument registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqDelivery {
    /// `sa_handler` — the `iretq` target.
    pub rip: u64,
    /// Clean user RFLAGS (`IF` set, reserved bit 1 set).
    pub rflags: u64,
    /// The `rt_sigframe` base (≡ 8 mod 16, `pretcode` at its first word).
    pub rsp: u64,
    /// `signo`.
    pub rdi: u64,
    /// `&siginfo`.
    pub rsi: u64,
    /// `&ucontext`.
    pub rdx: u64,
    /// The signal this delivery carries — kept explicitly so the handler-entry
    /// plan can be checked against the pending bit's signal (the off-by-one class:
    /// delivering signal N-1 for a pending N).
    pub sig: u64,
}

/// Build the delivery plan for `sig` handled by `handler`, whose `rt_sigframe`
/// starts at `frame_addr`.
pub fn plan_irq_delivery(frame_addr: u64, sig: u64, handler: u64, user_rflags: u64) -> IrqDelivery {
    IrqDelivery {
        rip: handler,
        rflags: user_rflags,
        rsp: frame_addr,
        rdi: sig,
        rsi: frame_addr + SIGINFO_OFFSET,
        rdx: frame_addr + UC_OFFSET,
        sig,
    }
}

// ─── Page-fault disposition policy (F-x86a-4 backstop) ───────────────────────

/// What the page-fault handler must do with a fault the demand-paging path
/// could not service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageFaultAction {
    /// Terminate only the faulting Compat_Process; the machine keeps running.
    KillProcess,
    /// Dump the faulting frame and halt: a genuine kernel fault must stay fatal.
    Halt,
}

/// Decide the disposition of a page fault `mem_sys::handle_user_page_fault`
/// refused.
///
/// `user_mode` mirrors `PageFaultErrorCode::USER_MODE` (the faulting CS was ring
/// 3), `fault_addr` is CR2, `user_addr_max` the exclusive user-half bound and
/// `current_is_compat` says whether the CPU is running a Linux compat process.
///
/// Ring-3 faults keep their historical rule: a compat process is killed, any
/// other task takes the fatal path. The case this policy adds is a **ring-0
/// fault on a user address**: a syscall handler read or wrote a user buffer that
/// a sibling `CLONE_VM` thread unmapped after the entry-time validation
/// (F-x86a-4). That is never worth a machine halt — kill the process that
/// triggered it. A ring-0 fault on a *kernel* address, by contrast, is a real
/// kernel bug (the LIEDTKE case the handler exists for) and still halts, so the
/// backstop cannot silently swallow one.
pub fn page_fault_action(
    user_mode: bool,
    fault_addr: u64,
    user_addr_max: u64,
    current_is_compat: bool,
) -> PageFaultAction {
    // The first page is never mappable in this kernel (`mmap(0,...)` is
    // rejected), so a ring-0 fault at address ~0 is a kernel NULL-pointer bug,
    // not a user-buffer race: it must stay fatal, not be blamed on a process.
    const MIN_USER_FAULT_ADDR: u64 = 4096;
    let on_user_address = (MIN_USER_FAULT_ADDR..user_addr_max).contains(&fault_addr);
    if (user_mode || on_user_address) && current_is_compat {
        PageFaultAction::KillProcess
    } else {
        PageFaultAction::Halt
    }
}

// ─── CPU-exception disposition policy (F-r3-arch-1) ─────────────────────────

/// What a synchronous CPU-exception handler (#DE/#DB/#UD/#OF/#BR/#NM/#AC/#MF/#XM)
/// must do when the exception is not one the kernel itself expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrapAction {
    /// Terminate only the faulting Compat_Process; the machine keeps running.
    KillProcess,
    /// Dump what the handler logged and halt: an exception in ring 0 is a
    /// genuine kernel bug (or hardware failure) and must stay fatal.
    Halt,
}

/// Decide the disposition of a synchronous CPU exception.
///
/// F-r3-arch-1 (round-3 audit, high): only `#GP` and `#PF` implemented the
/// "a user-mode exception is a userspace bug, not a machine fault" rule; every
/// other user-reachable vector ended in `halt()`, so `1/0`, `ud2` or a
/// ring-3 `popfq` with TF set parked the whole machine from an unprivileged
/// process. The rule here is the same one `#GP`/`#PF` use: if the faulting
/// context is ring 3 (or the saved RIP is a user address) **and** the current
/// task is a Linux Compat_Process, kill only that process; otherwise halt.
///
/// `user_mode` is derived from the saved CS RPL; `fault_rip < user_addr_max`
/// is the same user-address check the `#GP`/`#PF` paths apply. Both are needed:
/// a kernel-mode exception on a user RIP (e.g. a future emulation path) should
/// not take the machine down, while an exception in ring 0 on a kernel RIP is
/// exactly the case that must remain fatal. Only a compat task can be killed;
/// a native kernel task has no Compat_Process to terminate, so it stays fatal.
pub fn exception_action(
    user_mode: bool,
    fault_rip: u64,
    user_addr_max: u64,
    current_is_compat: bool,
) -> TrapAction {
    let on_user_rip = fault_rip < user_addr_max;
    if current_is_compat && (user_mode || on_user_rip) {
        TrapAction::KillProcess
    } else {
        TrapAction::Halt
    }
}

// ─── Layout assertions (the `src/test.rs` contract, in type form) ─────────────

const _: () = {
    use core::mem::{offset_of, size_of};
    // The whole frame: 1 popfq word + 15 GPRs + RIP/CS/RFLAGS/RSP/SS.
    assert!(size_of::<IrqFrame>() == 21 * 8);
    assert!(offset_of!(IrqFrame, popfq_rflags) == 0);
    assert!(offset_of!(IrqFrame, gpr) == 8);
    assert!(offset_of!(IrqFrame, rip) == 128);
    assert!(offset_of!(IrqFrame, cs) == 136);
    assert!(offset_of!(IrqFrame, rflags) == 144);
    assert!(offset_of!(IrqFrame, rsp) == 152);
    assert!(offset_of!(IrqFrame, ss) == 160);
    // `SavedRegs` field offsets inside the GPR block. `rax` at absolute +120 is
    // the trap this module exists for: it is NOT the syscall frame's user-RSP
    // slot (which is at `SavedRegs + 120`).
    assert!(size_of::<SavedRegs>() == 15 * 8);
    assert!(offset_of!(SavedRegs, r15) == 0);
    assert!(offset_of!(SavedRegs, rax) == 112);
    assert!(offset_of!(SavedRegs, rdi) == 72);
};
