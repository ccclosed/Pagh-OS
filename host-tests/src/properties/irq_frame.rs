// Feature: linux-binary-compat (issue #12, task t9), property `irq_frame`: the
// timer-tick frame adapter is byte-exact and cannot deliver a neighbouring
// signal —
//   * `is_user_frame` is exactly "CS has RPL 3" (a task interrupted inside a
//     syscall carries a kernel CS and must NOT get a frame built here);
//   * the interrupted user context read out of the frame is the CPU-pushed iret
//     material (RIP/RSP/RFLAGS) plus the 15 GPR slots — and never the `popfq`
//     word at `+0`;
//   * entering the handler mutates exactly RIP, RFLAGS, RSP, RDI, RSI, RDX and
//     preserves CS, SS, the `popfq` word and `rax` (the live user `rax` that is
//     also the `sigcontext.rax` a later `rt_sigreturn` restores);
//   * the plan's `rdi` IS the signal that was consumed, and the encoded frame's
//     `si_signo` and `sigcontext` agree with it — the off-by-one class (signal
//     N-1 delivered for a pending N) cannot survive this;
//   * the whole thing round-trips through `encode_rt_sigframe` /
//     `decode_rt_sigframe`, so an `rt_sigreturn` from the handler resumes the
//     interrupted instruction with the interrupted stack and registers.

use crate::regs::SavedRegs;
use crate::signal_frame::*;
use crate::trap_frame::*;
use proptest::prelude::*;

fn gpr_strategy() -> impl Strategy<Value = SavedRegs> {
    // `SavedRegs` has 15 fields — more than proptest's tuple strategies accept, so
    // build it from a 15-element vector in field order.
    prop::collection::vec(any::<u64>(), 15).prop_map(|v: Vec<u64>| SavedRegs {
        r15: v[0],
        r14: v[1],
        r13: v[2],
        r12: v[3],
        r11: v[4],
        r10: v[5],
        r9: v[6],
        r8: v[7],
        rbp: v[8],
        rdi: v[9],
        rsi: v[10],
        rdx: v[11],
        rcx: v[12],
        rbx: v[13],
        rax: v[14],
    })
}

fn frame_strategy() -> impl Strategy<Value = IrqFrame> {
    (any::<u64>(), gpr_strategy(), any::<u64>(), any::<u64>()).prop_map(
        |(popfq_rflags, gpr, rip, cs)| IrqFrame {
            popfq_rflags,
            gpr,
            rip,
            cs,
            rflags: 0x202,
            rsp: 0x7000_0000_0000,
            ss: 0x10,
        },
    )
}

/// Independent expectation for the interrupted user context.
fn expected_context(f: &IrqFrame) -> SigFrameRegs {
    SigFrameRegs {
        r8: f.gpr.r8,
        r9: f.gpr.r9,
        r10: f.gpr.r10,
        r11: f.gpr.r11,
        r12: f.gpr.r12,
        r13: f.gpr.r13,
        r14: f.gpr.r14,
        r15: f.gpr.r15,
        di: f.gpr.rdi,
        si: f.gpr.rsi,
        bp: f.gpr.rbp,
        bx: f.gpr.rbx,
        dx: f.gpr.rdx,
        ax: f.gpr.rax,
        cx: f.gpr.rcx,
        sp: f.rsp,
        ip: f.rip,
        flags: f.rflags,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Ring-3 classification is exactly the CS RPL (and a non-null selector).
    #[test]
    fn user_frame_is_exactly_ring3(f in frame_strategy(), cs in any::<u64>()) {
        let mut g = f;
        g.cs = cs;
        prop_assert_eq!(g.is_user_frame(), cs & 3 == 3 && cs != 0);
    }

    /// The user context comes from the GPR slots and the iret words — never from
    /// the `popfq` word at +0.
    #[test]
    fn user_context_maps_the_iret_frame(f in frame_strategy(), popfq in any::<u64>()) {
        let mut g = f;
        g.popfq_rflags = popfq;
        prop_assert_eq!(g.user_context(), expected_context(&g));
        // The scenario the whole module exists for: `gpr.rax` (absolute +120) is a
        // live user register, not the syscall frame's user-RSP slot.
        prop_assert_eq!(g.user_context().ax, g.gpr.rax);
        prop_assert_eq!(g.user_context().sp, g.rsp);
        prop_assert_eq!(g.user_context().ip, g.rip);
        prop_assert_eq!(g.user_context().flags, g.rflags);
    }

    /// Entering the handler mutates exactly the six entry fields.
    #[test]
    fn enter_handler_mutates_exactly_the_entry_fields(
        f in frame_strategy(),
        handler in 0x400_000u64..0x4000_0000u64,
        frame_addr in 0x1000u64..0x4000_0000u64,
        sig in 1u64..=64u64,
    ) {
        let before = f;
        let plan = plan_irq_delivery(frame_addr, sig, handler, USER_RFLAGS);
        let mut after = f;
        after.enter_handler(&plan);
        // Mutated:
        prop_assert_eq!(after.rip, handler);
        prop_assert_eq!(after.rsp, frame_addr);
        prop_assert_eq!(after.rflags, USER_RFLAGS);
        prop_assert_eq!(after.gpr.rdi, sig);
        prop_assert_eq!(after.gpr.rsi, frame_addr + SIGINFO_OFFSET);
        prop_assert_eq!(after.gpr.rdx, frame_addr + UC_OFFSET);
        // Preserved — `rax` and the popfq word above all:
        prop_assert_eq!(after.gpr.rax, before.gpr.rax);
        prop_assert_eq!(after.popfq_rflags, before.popfq_rflags);
        prop_assert_eq!(after.cs, before.cs);
        prop_assert_eq!(after.ss, before.ss);
        // Everything else in the GPR block:
        let (mut a, mut b) = (after.gpr, before.gpr);
        a.rdi = b.rdi; a.rsi = b.rsi; a.rdx = b.rdx;
        b.rdi = a.rdi; b.rsi = a.rsi; b.rdx = a.rdx;
        prop_assert_eq!(a, b);
    }

    /// The plan carries the signal that was consumed — the off-by-one guard.
    #[test]
    fn plan_carries_the_pending_signal(
        sig in 1u64..=64u64,
        handler in 0x400_000u64..0x4000_0000u64,
        restorer in 0x400_000u64..0x4000_0000u64,
        frame_addr in 0x1000u64..0x4000_0000u64,
        mask in any::<u64>(),
    ) {
        let plan = plan_irq_delivery(frame_addr, sig, handler, USER_RFLAGS);
        prop_assert_eq!(plan.sig, sig);
        prop_assert_eq!(plan.rdi, sig);
        // …and the encoded frame reports the same signal in `si_signo`.
        let f = IrqFrame { popfq_rflags: 0x002, gpr: SavedRegs::default(), rip: 0x401000, cs: 0x2b, rflags: 0x246, rsp: 0x7000_0000_0000, ss: 0x10 };
        let mut buf = [0u8; RT_SIGFRAME_SIZE as usize];
        encode_rt_sigframe(&mut buf, restorer, plan.sig, &f.user_context(), mask, &SigAltStack::default());
        let mut si = [0u8; 4];
        si.copy_from_slice(&buf[SIGINFO_OFFSET as usize..SIGINFO_OFFSET as usize + 4]);
        prop_assert_eq!(u32::from_le_bytes(si), sig as u32);
    }

    /// The delivered frame round-trips: `rt_sigreturn` from the handler restores
    /// exactly the interrupted context (this is what makes tick delivery a real
    /// delivery rather than a jump into the handler that cannot come back).
    #[test]
    fn tick_frame_round_trips(f in frame_strategy(), sig in 1u64..=64u64, restorer in 0x400_000u64..0x4000_0000u64, mask in any::<u64>()) {
        let mut g = f;
        g.cs = 0x2b; // ring 3
        let mut buf = [0u8; RT_SIGFRAME_SIZE as usize];
        encode_rt_sigframe(&mut buf, restorer, sig, &g.user_context(), mask, &SigAltStack::default());
        let mut uc = [0u8; 304];
        uc.copy_from_slice(&buf[UC_OFFSET as usize..UC_OFFSET as usize + 304]);
        let r = decode_rt_sigframe(&uc).expect("non-null ip decodes");
        prop_assert_eq!(r.regs, g.user_context());
        prop_assert_eq!(r.mask, mask & !UNBLOCKABLE_MASK);
    }
}
