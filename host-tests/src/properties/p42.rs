// Feature: linux-binary-compat (Phase-1 signals), Property 42: the `rt_sigframe`
// ABI round-trips and the placement/alignment contract holds for any inputs.

use crate::signal_frame::*;
use proptest::prelude::*;

fn action_strategy() -> impl Strategy<Value = SignalAction> {
    (
        prop::option::of(0x400_000u64..0x4000_0000u64), // handler (NULL = SIG_DFL)
        any::<u64>(),
        prop::option::of(0x400_000u64..0x4000_0000u64), // restorer
        any::<u64>(),
    )
        .prop_map(|(handler, flags, restorer, mask)| SignalAction {
            handler: handler.unwrap_or(0),
            flags,
            restorer: restorer.unwrap_or(0),
            mask,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// encode → decode round-trips every saved GPR and the new mask, and the
    /// frame carries pretcode/siginfo at their ABI offsets.
    #[test]
    fn frame_roundtrip(
        r8 in any::<u64>(), r9 in any::<u64>(), r10 in any::<u64>(), r11 in any::<u64>(),
        r12 in any::<u64>(), r13 in any::<u64>(), r14 in any::<u64>(), r15 in any::<u64>(),
        di in any::<u64>(), si in any::<u64>(), bp in any::<u64>(), bx in any::<u64>(),
        dx in any::<u64>(), ax in any::<u64>(), cx in any::<u64>(), sp in any::<u64>(),
        ip in 1u64..0x4000_0000u64, // NULL ip is the decode-reject sentinel
        flags in any::<u64>(),
        mask in any::<u64>(),
        restorer in any::<u64>(),
        sig in 1u64..=64u64,
        action in action_strategy(),
    ) {
        let saved = SigFrameRegs { r8, r9, r10, r11, r12, r13, r14, r15, di, si, bp, bx, dx, ax, cx, sp, ip, flags };
        let alt = SigAltStack::default();
        let mut buf = [0u8; RT_SIGFRAME_SIZE as usize];
        encode_rt_sigframe(&mut buf, restorer, sig, &saved, mask, &alt);
        let mut uc = [0u8; 304];
        uc.copy_from_slice(&buf[UC_OFFSET as usize..UC_OFFSET as usize + 304]);
        let r = decode_rt_sigframe(&uc).expect("non-null ip decodes");
        prop_assert_eq!(r.regs, saved);
        prop_assert_eq!(r.mask, mask & !UNBLOCKABLE_MASK);

        let mut w = [0u8; 8];
        w.copy_from_slice(&buf[0..8]);
        prop_assert_eq!(u64::from_le_bytes(w), restorer);
        let mut s4 = [0u8; 4];
        s4.copy_from_slice(&buf[SIGINFO_OFFSET as usize..SIGINFO_OFFSET as usize + 4]);
        prop_assert_eq!(u32::from_le_bytes(s4), sig as u32);
    }

    /// Handler-entry RSP is always ≡ 8 (mod 16) and the frame sits below the
    /// interrupted RSP (or inside the altstack when SA_ONSTACK applies).
    #[test]
    fn frame_placement_alignment(
        rsp in 0x1000u64..0x8000_0000_0000u64,
        action in action_strategy(),
        alt_sp in 0u64..0x4000_0000u64,
        alt_size in 0u64..0x100_0000u64,
        alt_flags in 0u32..8u32,
    ) {
        let alt = SigAltStack { sp: alt_sp, flags: alt_flags, size: alt_size };
        let f = frame_location(rsp, &action, &alt);
        prop_assert_eq!(f % 16, 8);
        let on_alt = action.flags & SA_ONSTACK != 0
            && alt.flags & SS_DISABLE == 0
            && alt.size >= MINSIGSTKSZ;
        if on_alt {
            prop_assert!(f >= alt.sp && f + RT_SIGFRAME_SIZE <= alt.sp + alt.size);
        } else {
            prop_assert!(f + RT_SIGFRAME_SIZE <= rsp);
        }
    }

    /// The unblockable invariant: no `sigprocmask` operation can ever leave
    /// SIGKILL/SIGSTOP blocked, for any `how` and any requested mask.
    #[test]
    fn kill_stop_never_blocked(how in 0u64..4u64, old in any::<u64>(), set in any::<u64>()) {
        match apply_mask_op(how, old, set) {
            Some(new) => prop_assert_eq!(new & UNBLOCKABLE_MASK, 0),
            None => prop_assert!(how != SIG_BLOCK && how != SIG_UNBLOCK && how != SIG_SETMASK),
        }
    }
}
