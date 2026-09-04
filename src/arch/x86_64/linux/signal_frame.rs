//! Pure x86_64 Linux signal ABI: signal numbers, disposition records, sigset
//! algebra, default-action classification, and the `rt_sigframe`
//! encode/decode used by signal delivery (`super::signal`) and
//! `rt_sigreturn`.
//!
//! This module is deliberately `core`-only and free of kernel dependencies
//! (R11.6): it is `#[path]`-included by `host-tests` (`p42`), so the property
//! tests exercise the EXACT bytes the kernel writes into ring-3 stacks.
//!
//! ## Frame layout contract (x86_64 Linux, `arch/x86/kernel/signal.c`)
//!
//! A delivered signal builds, on the user stack below the interrupted RSP:
//!
//! ```text
//! frame+0    : pretcode  = sa_restorer (glibc's sigreturn trampoline)
//! frame+8    : struct ucontext uc        (304 bytes)
//! frame+312  : struct siginfo info       (128 bytes, zeros + si_signo/si_code)
//! total      : RT_SIGFRAME_SIZE = 440
//! ```
//!
//! The interrupted GPR state is stored in `uc.uc_mcontext` (a
//! `sigcontext_64`, 256 bytes at `frame+48`), and the signal mask to restore
//! on `rt_sigreturn` in `uc.uc_sigmask` (`frame+304`).
//!
//! Handler-entry register contract (matching Linux on the syscall-return
//! path): `RSP = frame`, `RIP = sa_handler`, `RDI = signo`, `RSI = &info`,
//! `RDX = &uc`. `rcx`/`r11` at delivery hold the interrupted user RIP /
//! RFLAGS (the `syscall` instruction's own clobber), so `sigcontext.cx` and
//! `sigcontext.ip` carry the same value — identical to what Linux shows.
//!
//! ## Return-to-frame contract for `rt_sigreturn`
//!
//! The restorer runs with `RSP = frame + 8` (the `ret` popped `pretcode`), so
//! `rt_sigreturn` finds `uc` exactly at the entry RSP: mcontext at `+40`,
//! sigmask at `+296`.
#![allow(dead_code)]

/// Highest signal number the table covers (Linux `_NSIG/64-bit` is 64).
pub const SIGNAL_COUNT: usize = 64;

// ─── Signal numbers (x86_64 Linux) ───────────────────────────────────────────

pub const SIGHUP: u64 = 1;
pub const SIGINT: u64 = 2;
pub const SIGQUIT: u64 = 3;
pub const SIGILL: u64 = 4;
pub const SIGTRAP: u64 = 5;
pub const SIGABRT: u64 = 6;
pub const SIGBUS: u64 = 7;
pub const SIGFPE: u64 = 8;
pub const SIGKILL: u64 = 9;
pub const SIGSEGV: u64 = 11;
pub const SIGPIPE: u64 = 13;
pub const SIGALRM: u64 = 14;
pub const SIGCHLD: u64 = 17;
pub const SIGCONT: u64 = 18;
pub const SIGSTOP: u64 = 19;
pub const SIGTSTP: u64 = 20;
pub const SIGTTIN: u64 = 21;
pub const SIGTTOU: u64 = 22;
pub const SIGURG: u64 = 23;
pub const SIGWINCH: u64 = 28;

/// `sigaction(2)`: reset the disposition to `SIG_DFL`.
pub const SIG_DFL: u64 = 0;
/// `sigaction(2)`: ignore the signal.
pub const SIG_IGN: u64 = 1;

// ─── `sa_flags` bits ─────────────────────────────────────────────────────────

/// Run the handler on the alternate signal stack (`sigaltstack(2)`).
pub const SA_ONSTACK: u64 = 0x0800_0000;
/// `sa_restorer` is valid (glibc always sets this on x86_64).
pub const SA_RESTORER: u64 = 0x0400_0000;
/// Handler takes `(signo, &siginfo, &ucontext)` — we pass all three either way.
pub const SA_SIGINFO: u64 = 0x0000_0004;

// ─── `sigprocmask` `how` values ──────────────────────────────────────────────

pub const SIG_BLOCK: u64 = 0;
pub const SIG_UNBLOCK: u64 = 1;
pub const SIG_SETMASK: u64 = 2;

// ─── `sigaltstack` ───────────────────────────────────────────────────────────

/// Minimum alternate-stack size we accept (`MINSIGSTKSZ` on Linux x86_64).
pub const MINSIGSTKSZ: u64 = 2048;
/// `stack_t.ss_flags`: the alternate stack is disabled.
pub const SS_DISABLE: u32 = 2;

// ─── sigset algebra (u64 bitset, bit N == signal N) ──────────────────────────

/// Bit mask for signal `sig` (1..=64); `sig == 0` yields 0 (no signal).
#[inline]
pub const fn sigbit(sig: u64) -> u64 {
    if sig == 0 || sig > SIGNAL_COUNT as u64 {
        0
    } else {
        1u64 << (sig - 1)
    }
}

/// Kernel-enforced unblockable signals: `SIGKILL` and `SIGSTOP`.
pub const UNBLOCKABLE_MASK: u64 = sigbit(SIGKILL) | sigbit(SIGSTOP);

/// Apply a `sigprocmask` operation. `SIGKILL`/`SIGSTOP` bits can never end up
/// blocked; an unknown `how` is `None` (caller maps to `EINVAL`).
pub fn apply_mask_op(how: u64, old: u64, set: u64) -> Option<u64> {
    let mut new = match how {
        SIG_BLOCK => old | set,
        SIG_UNBLOCK => old & !set,
        SIG_SETMASK => set,
        _ => return None,
    };
    new &= !UNBLOCKABLE_MASK;
    Some(new)
}

// ─── Default actions (`signal(7)` table) ─────────────────────────────────────

/// What the kernel does with a signal whose disposition is `SIG_DFL`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DefaultAction {
    /// Terminate the process (exit status `128 + sig`).
    Term,
    /// Ignore the signal.
    Ignore,
    /// Stop the process (Phase 1 logs and drops — the scheduler has no stop).
    Stop,
    /// Continue the process if stopped, else ignore.
    Cont,
}

/// Default action for signals 1..=31 plus the common RT numbers; everything
/// beyond 34 defaults to Term (Linux: real-time signals default-terminate).
pub fn default_action(sig: u64) -> DefaultAction {
    match sig {
        SIGCHLD | SIGURG | SIGWINCH => DefaultAction::Ignore,
        SIGCONT => DefaultAction::Cont,
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => DefaultAction::Stop,
        _ => DefaultAction::Term,
    }
}

/// Is this signal's default action process termination? Used by `tgkill` to
/// keep its "fatal signal to self without a handler exits the group" behavior.
pub fn default_is_fatal(sig: u64) -> bool {
    default_action(sig) == DefaultAction::Term
}

// ─── Disposition + alternate stack records ───────────────────────────────────

/// Kernel-side copy of one disposition (`struct kernel_sigaction`, 32-byte
/// user ABI when `sigsetsize == 8`: handler, flags, restorer, mask).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignalAction {
    /// `SIG_DFL` (0), `SIG_IGN` (1), or the user handler virtual address.
    pub handler: u64,
    /// `sa_flags` (`SA_RESTORER`, `SA_ONSTACK`, ...).
    pub flags: u64,
    /// `sa_restorer` — the sigreturn trampoline glibc provides.
    pub restorer: u64,
    /// Mask to block while the handler runs (`sa_mask`, low 64 bits).
    pub mask: u64,
}

impl Default for SignalAction {
    fn default() -> Self {
        SignalAction {
            handler: SIG_DFL,
            flags: 0,
            restorer: 0,
            mask: 0,
        }
    }
}

/// Does this disposition run user code (neither `SIG_DFL` nor `SIG_IGN`)?
pub fn is_user_handler(a: &SignalAction) -> bool {
    a.handler != SIG_DFL && a.handler != SIG_IGN
}

/// Kernel-side `stack_t` (24-byte user ABI: sp, flags(+pad), size).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SigAltStack {
    /// Base of the alternate stack region.
    pub sp: u64,
    /// `SS_DISABLE` or 0 (enabled).
    pub flags: u32,
    /// Region size in bytes.
    pub size: u64,
}

impl Default for SigAltStack {
    fn default() -> Self {
        SigAltStack {
            sp: 0,
            flags: SS_DISABLE,
            size: 0,
        }
    }
}

// ─── Frame geometry ──────────────────────────────────────────────────────────

/// Offset of `ucontext` within the frame (after `pretcode`).
pub const UC_OFFSET: u64 = 8;
/// Offset of `sigcontext_64` (mcontext) within `ucontext`
/// (`uc_flags` 8 + `uc_link` 8 + `uc_stack` 24).
pub const MCONTEXT_OFFSET: u64 = UC_OFFSET + 40;
/// Offset of `uc_sigmask` within `ucontext` (after the 256-byte mcontext).
pub const SIGMASK_OFFSET: u64 = UC_OFFSET + 40 + 256;
/// Offset of `siginfo` within the frame (after the 304-byte ucontext).
pub const SIGINFO_OFFSET: u64 = UC_OFFSET + 304;
/// Total frame size: pretcode(8) + ucontext(304) + siginfo(128).
pub const RT_SIGFRAME_SIZE: u64 = 440;

/// The GPR snapshot stored in / restored from the frame's `sigcontext`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigFrameRegs {
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub di: u64,
    pub si: u64,
    pub bp: u64,
    pub bx: u64,
    pub dx: u64,
    pub ax: u64,
    pub cx: u64,
    pub sp: u64,
    pub ip: u64,
    pub flags: u64,
}

impl SigFrameRegs {
    fn put(&self, buf: &mut [u8; 256]) {
        let fields = [
            self.r8, self.r9, self.r10, self.r11, self.r12, self.r13, self.r14, self.r15, self.di,
            self.si, self.bp, self.bx, self.dx, self.ax, self.cx, self.sp, self.ip, self.flags,
        ];
        for (i, v) in fields.iter().enumerate() {
            buf[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        // cs/gs/fs/ss (u16s at 144..152) and err/trapno/oldmask/cr2 are left
        // zero — the kernel-side reader restores only GPRs/IP/FLAGS/SP.
    }

    fn get(buf: &[u8; 256]) -> SigFrameRegs {
        let mut f = [0u64; 18];
        for (i, slot) in f.iter_mut().enumerate() {
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[i * 8..i * 8 + 8]);
            *slot = u64::from_le_bytes(b);
        }
        SigFrameRegs {
            r8: f[0],
            r9: f[1],
            r10: f[2],
            r11: f[3],
            r12: f[4],
            r13: f[5],
            r14: f[6],
            r15: f[7],
            di: f[8],
            si: f[9],
            bp: f[10],
            bx: f[11],
            dx: f[12],
            ax: f[13],
            cx: f[14],
            sp: f[15],
            ip: f[16],
            flags: f[17],
        }
    }
}

/// Where the kernel places the frame below the interrupted RSP.
///
/// The address is ≡ 8 (mod 16): at handler entry `RSP = frame` looks exactly
/// like a normal `call`-pushed return address from the SysV alignment
/// standpoint, so handler prologues using `movaps` keep their ABI guarantee.
/// `SA_ONSTACK` with an enabled, large-enough alternate stack places the frame
/// at the TOP of that stack instead (still ≡ 8 mod 16).
pub fn frame_location(user_rsp: u64, action: &SignalAction, alt: &SigAltStack) -> u64 {
    let base =
        if action.flags & SA_ONSTACK != 0 && alt.flags & SS_DISABLE == 0 && alt.size >= MINSIGSTKSZ
        {
            alt.sp + alt.size
        } else {
            user_rsp
        };
    ((base.saturating_sub(RT_SIGFRAME_SIZE)) & !0xF) - 8
}

/// Encode a complete `rt_sigframe` into `out` (exactly `RT_SIGFRAME_SIZE`
/// bytes). `restorer` becomes `pretcode`; `newmask` is the mask `rt_sigreturn`
/// restores (the pre-delivery mask ORed with the action's `sa_mask`).
pub fn encode_rt_sigframe(
    out: &mut [u8; RT_SIGFRAME_SIZE as usize],
    restorer: u64,
    sig: u64,
    saved: &SigFrameRegs,
    newmask: u64,
    alt: &SigAltStack,
) {
    *out = [0u8; RT_SIGFRAME_SIZE as usize];
    // pretcode
    out[0..8].copy_from_slice(&restorer.to_le_bytes());
    // ucontext: uc_flags = 0, uc_link = NULL, uc_stack = the CURRENT altstack
    // state (Linux reports the active one; SS_DISABLE when unset).
    // uc_stack = { ss_sp @ uc+16, ss_flags @ uc+24, ss_size @ uc+28 }.
    out[UC_OFFSET as usize + 24..UC_OFFSET as usize + 28].copy_from_slice(&alt.flags.to_le_bytes());
    out[UC_OFFSET as usize + 28..UC_OFFSET as usize + 36].copy_from_slice(&alt.size.to_le_bytes());
    // mcontext
    let mut mc = [0u8; 256];
    saved.put(&mut mc);
    out[MCONTEXT_OFFSET as usize..MCONTEXT_OFFSET as usize + 256].copy_from_slice(&mc);
    // uc_sigmask
    out[SIGMASK_OFFSET as usize..SIGMASK_OFFSET as usize + 8]
        .copy_from_slice(&newmask.to_le_bytes());
    // siginfo: si_signo, si_errno=0, si_code=SI_KERNEL (0x80); rest zeros.
    out[SIGINFO_OFFSET as usize..SIGINFO_OFFSET as usize + 4]
        .copy_from_slice(&(sig as u32).to_le_bytes());
    out[SIGINFO_OFFSET as usize + 8..SIGINFO_OFFSET as usize + 12]
        .copy_from_slice(&0x80u32.to_le_bytes());
}

/// Result of decoding a frame on `rt_sigreturn`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestoredFrame {
    /// Full GPR snapshot (restore all except `rcx`/`r11` — see below).
    pub regs: SigFrameRegs,
    /// Signal mask to reinstate (the pre-delivery mask).
    pub mask: u64,
}

/// Decode the frame at `uc_addr` from the 304 bytes starting there
/// (`ucontext`): mcontext at `+40`, sigmask at `+296`.
///
/// Returns `None` when the mcontext RIP is NULL — a sanity guard against
/// `rt_sigreturn` misuse (there is no kernel-side way to authenticate the
/// frame; Linux trusts it likewise).
pub fn decode_rt_sigframe(uc: &[u8; 304]) -> Option<RestoredFrame> {
    let mut mc = [0u8; 256];
    mc.copy_from_slice(
        &uc[MCONTEXT_OFFSET as usize - UC_OFFSET as usize
            ..MCONTEXT_OFFSET as usize - UC_OFFSET as usize + 256],
    );
    let regs = SigFrameRegs::get(&mc);
    if regs.ip == 0 {
        return None;
    }
    let mut m = [0u8; 8];
    m.copy_from_slice(&uc[296..304]);
    Some(RestoredFrame {
        regs,
        mask: u64::from_le_bytes(m) & !UNBLOCKABLE_MASK,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigbit_basics() {
        assert_eq!(sigbit(1), 1);
        assert_eq!(sigbit(SIGINT), 0b10); // bit N-1 for signal N
        assert_eq!(sigbit(0), 0);
        assert_eq!(sigbit(65), 0);
        assert_eq!(UNBLOCKABLE_MASK, sigbit(9) | sigbit(19));
    }

    #[test]
    fn mask_ops_always_leave_kill_stop_unblocked() {
        let m = apply_mask_op(SIG_SETMASK, 0, u64::MAX).unwrap();
        assert_eq!(m & UNBLOCKABLE_MASK, 0);
        let m = apply_mask_op(SIG_BLOCK, 0, u64::MAX).unwrap();
        assert_eq!(m & UNBLOCKABLE_MASK, 0);
        assert_eq!(apply_mask_op(99, 0, 1), None);
    }

    #[test]
    fn default_action_table() {
        assert_eq!(default_action(SIGINT), DefaultAction::Term);
        assert_eq!(default_action(SIGKILL), DefaultAction::Term);
        assert_eq!(default_action(SIGSEGV), DefaultAction::Term);
        assert_eq!(default_action(SIGCHLD), DefaultAction::Ignore);
        assert_eq!(default_action(SIGWINCH), DefaultAction::Ignore);
        assert_eq!(default_action(SIGSTOP), DefaultAction::Stop);
        assert_eq!(default_action(SIGCONT), DefaultAction::Cont);
        assert_eq!(default_action(42), DefaultAction::Term);
    }

    #[test]
    fn frame_location_keeps_sysv_alignment() {
        let a = SignalAction::default();
        let d = SigAltStack::default();
        for rsp in [0x7FFF_F000u64, 0x7FFF_F008, 0x7FFF_E010, 0x1000_0000] {
            let f = frame_location(rsp, &a, &d);
            assert_eq!(f % 16, 8, "rsp={rsp:#x} frame={f:#x}");
            assert!(f + RT_SIGFRAME_SIZE <= rsp);
        }
    }

    #[test]
    fn frame_location_uses_enabled_altstack() {
        let mut a = SignalAction::default();
        a.flags = SA_ONSTACK;
        let mut alt = SigAltStack::default();
        // Disabled → falls back to the regular stack.
        assert_eq!(
            frame_location(0x8000, &a, &alt),
            frame_location(0x8000, &SignalAction::default(), &alt)
        );
        alt.flags = 0;
        alt.sp = 0x10_0000;
        alt.size = MINSIGSTKSZ;
        let f = frame_location(0x8000, &a, &alt);
        assert!(f >= 0x10_0000 && f + RT_SIGFRAME_SIZE <= 0x10_0000 + MINSIGSTKSZ);
        assert_eq!(f % 16, 8);
        // Too-small altstack is ignored.
        alt.size = 64;
        assert_eq!(
            frame_location(0x8000, &a, &alt),
            frame_location(0x8000, &SignalAction::default(), &alt)
        );
    }

    #[test]
    fn roundtrip_encode_decode() {
        let saved = SigFrameRegs {
            r8: 0x88,
            r9: 0x99,
            r10: 0xAA,
            r11: 0x202,
            r12: 1,
            r13: 2,
            r14: 3,
            r15: 4,
            di: 0x1234,
            si: 0x5678,
            bp: 0x7FFF_F000,
            bx: 42,
            dx: 7,
            ax: 0xDEAD,
            cx: 0x401000,
            sp: 0x7FFF_F100,
            ip: 0x4020F0,
            flags: 0x246,
        };
        let alt = SigAltStack::default();
        let mut buf = [0u8; RT_SIGFRAME_SIZE as usize];
        encode_rt_sigframe(&mut buf, 0x401500, SIGINT, &saved, 0b1010, &alt);
        let mut uc = [0u8; 304];
        uc.copy_from_slice(&buf[UC_OFFSET as usize..UC_OFFSET as usize + 304]);
        let r = decode_rt_sigframe(&uc).expect("valid frame decodes");
        assert_eq!(r.regs, saved);
        assert_eq!(r.mask, 0b1010);
        // pretcode == sa_restorer
        let mut p = [0u8; 8];
        p.copy_from_slice(&buf[0..8]);
        assert_eq!(u64::from_le_bytes(p), 0x401500);
        // siginfo: si_signo = SIGINT, si_code = SI_KERNEL
        let mut s = [0u8; 4];
        s.copy_from_slice(&buf[SIGINFO_OFFSET as usize..SIGINFO_OFFSET as usize + 4]);
        assert_eq!(u32::from_le_bytes(s), SIGINT as u32);
        s.copy_from_slice(&buf[SIGINFO_OFFSET as usize + 8..SIGINFO_OFFSET as usize + 12]);
        assert_eq!(u32::from_le_bytes(s), 0x80);
    }

    #[test]
    fn decode_rejects_null_ip() {
        let mut uc = [0u8; 304];
        // All-zero mcontext → ip == 0 → rejected.
        assert!(decode_rt_sigframe(&uc).is_none());
    }
}
