//! Feature: linux-binary-compat (job control), property `tty_job`: the pure
//! controlling-terminal model that the kernel's `TIOCGPGRP`/`TIOCSPGRP` ioctls
//! and the stdin/console read+write paths consult.
//!
//! The properties pin the *semantics* (and the Linux ABI numbers) against
//! independent oracles, not against a restatement of the implementation:
//!
//!   * only a process group that does NOT own the terminal is ever signalled,
//!     and the signal is chosen by access kind: `SIGTTIN` for a read always,
//!     `SIGTTOU` for a write exactly when `TOSTOP` is set (termios(3));
//!   * both stop signals really are stop-class signals in `signal_frame`
//!     (`default_action == Stop`), so a terminal verdict cannot queue a signal
//!     that would not stop the group;
//!   * transferring the terminal is a total round trip (`set` then `foreground`
//!     returns what was set) and flips the verdict for exactly the two groups
//!     involved;
//!   * `decode_pgrp_arg` is the C `int` decode: the result equals the
//!     sign-extended oracle, and every negative word is `EINVAL`;
//!   * `may_set_foreground` accepts exactly {live groups, the shell's group, the
//!     current group} minus group 0, which is checked against an independent
//!     set-membership oracle;
//!   * the ioctl request numbers are the documented Linux values
//!     (`TIOCGPGRP` 0x540F / `TIOCSPGRP` 0x5410) and classification is total.

use crate::signal_frame::{default_action, DefaultAction, SIGTTIN, SIGTTOU};
use crate::tty::{
    decode_pgrp_arg, decode_winsize, encode_winsize, may_set_foreground, pgrp_request,
    surface_winsize, winsize_changed, winsize_request, PgrpRequest, TtyAccess, TtyJobControl,
    TtyVerdict, Winsize, WinsizeRequest, TIOCGPGRP, TIOCSPGRP, TIOCGWINSZ, TIOCSWINSZ,
    WINSIZE_SIZE,
};
use proptest::prelude::*;

/// Independent oracle for the two stop signals used by terminal access control
/// (signal(7) default-action column): both Stop, and distinct.
const TTIN: u64 = 21;
const TTOU: u64 = 22;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// The verdict depends on foreground ownership and access kind only:
    /// foreground ⇒ allow; background read ⇒ SIGTTIN; background write ⇒
    /// SIGTTOU iff TOSTOP.
    #[test]
    fn verdict_is_exactly_ownership_and_access_kind(
        fg in 1u64..=1_000_000,
        caller in 1u64..=1_000_000,
        tostop in any::<bool>(),
    ) {
        let tty = TtyJobControl::new(fg, tostop);
        let read = tty.access(caller, TtyAccess::Read);
        let write = tty.access(caller, TtyAccess::Write);
        if caller == fg {
            prop_assert_eq!(read, TtyVerdict::Allow);
            prop_assert_eq!(write, TtyVerdict::Allow);
            prop_assert!(tty.is_foreground(caller));
        } else {
            prop_assert_eq!(read, TtyVerdict::Background(SIGTTIN));
            prop_assert_eq!(read, TtyVerdict::Background(TTIN));
            prop_assert!(!tty.is_foreground(caller));
            if tostop {
                prop_assert_eq!(write, TtyVerdict::Background(SIGTTOU));
                prop_assert_eq!(write, TtyVerdict::Background(TTOU));
            } else {
                prop_assert_eq!(write, TtyVerdict::Allow);
            }
        }
    }

    /// The signals a terminal verdict can queue are exactly the stop-class
    /// signals of the delivery path — a terminal that queued something else
    /// would fail to stop a background group.
    #[test]
    fn queued_signals_are_stop_class(fg in 1u64..=4096, caller in 1u64..=4096) {
        let tty = TtyJobControl::new(fg, true);
        for kind in [TtyAccess::Read, TtyAccess::Write] {
            if let TtyVerdict::Background(sig) = tty.access(caller, kind) {
                prop_assert!(sig == SIGTTIN || sig == SIGTTOU);
                prop_assert_eq!(default_action(sig), DefaultAction::Stop);
            }
        }
        prop_assert_ne!(SIGTTIN, SIGTTOU);
    }

    /// Handing the terminal over is a round trip, and it changes the verdict for
    /// exactly the old and the new owner.
    #[test]
    fn set_foreground_round_trips_and_flips_only_two_groups(
        old in 1u64..=4096,
        new in 1u64..=4096,
        probe in 1u64..=4096,
        tostop in any::<bool>(),
    ) {
        let mut tty = TtyJobControl::new(old, tostop);
        prop_assert_eq!(tty.foreground(), old);
        tty.set_foreground(new);
        prop_assert_eq!(tty.foreground(), new);
        prop_assert_eq!(tty.access(new, TtyAccess::Read), TtyVerdict::Allow);
        if new != old {
            prop_assert_eq!(tty.access(old, TtyAccess::Read), TtyVerdict::Background(SIGTTIN));
        }
        if probe != new {
            prop_assert_eq!(tty.access(probe, TtyAccess::Read), TtyVerdict::Background(SIGTTIN));
        }
        // Setting the group that already owns the terminal is idempotent.
        tty.set_foreground(new);
        prop_assert_eq!(tty.foreground(), new);
    }

    /// `TOSTOP` is an independent bit: toggling it never moves the foreground
    /// group, and it only ever affects background WRITES.
    #[test]
    fn tostop_only_affects_background_writes(
        fg in 1u64..=4096,
        caller in 1u64..=4096,
        on in any::<bool>(),
    ) {
        let mut tty = TtyJobControl::new(fg, on);
        prop_assert_eq!(tty.tostop(), on);
        prop_assert_eq!(tty.foreground(), fg);
        let read_before = tty.access(caller, TtyAccess::Read);
        let write_before = tty.access(caller, TtyAccess::Write);
        tty.set_tostop(!on);
        prop_assert_eq!(tty.foreground(), fg);
        prop_assert_eq!(tty.access(caller, TtyAccess::Read), read_before);
        if caller != fg {
            if on {
                prop_assert_eq!(write_before, TtyVerdict::Background(SIGTTOU));
                prop_assert_eq!(tty.access(caller, TtyAccess::Write), TtyVerdict::Allow);
            } else {
                prop_assert_eq!(write_before, TtyVerdict::Allow);
                prop_assert_eq!(tty.access(caller, TtyAccess::Write), TtyVerdict::Background(SIGTTOU));
            }
        }
    }

    /// The `TIOCSPGRP` argument is a C `int`: the decode agrees with the
    /// sign-extended oracle, and every word with the top bit set is `EINVAL`.
    #[test]
    fn pgrp_arg_decode_is_signed_int(raw in any::<u32>()) {
        let signed = raw as i32;
        match decode_pgrp_arg(raw) {
            Ok(pgid) => {
                prop_assert!(signed >= 0);
                prop_assert_eq!(pgid, signed as i64 as u64);
                prop_assert!(pgid <= i32::MAX as u64);
            }
            Err(e) => {
                prop_assert!(signed < 0);
                prop_assert_eq!(e, crate::errno::Errno::EINVAL);
            }
        }
    }

    /// `may_set_foreground` accepts exactly the live group, the shell's group and
    /// the current group — and never group 0.
    #[test]
    fn may_set_foreground_matches_set_membership(
        target in 0u64..=64,
        shell in 0u64..=64,
        current in 0u64..=64,
        live in any::<bool>(),
    ) {
        let oracle_ok = target != 0 && (live || target == shell || target == current);
        prop_assert_eq!(may_set_foreground(target, live, shell, current).is_ok(), oracle_ok);
        if target == 0 {
            prop_assert_eq!(
                may_set_foreground(target, live, shell, current),
                Err(crate::errno::Errno::EPERM)
            );
        }
    }

    /// `pgrp_request` is total and classifies exactly the two Linux request
    /// numbers; everything else (including neighbouring tty ioctls) is `None`.
    #[test]
    fn pgrp_request_classifies_exactly_the_two_ioctls(request in any::<u64>()) {
        let oracle = match request {
            0x540F => Some(PgrpRequest::Get),
            0x5410 => Some(PgrpRequest::Set),
            _ => None,
        };
        prop_assert_eq!(pgrp_request(request), oracle);
        prop_assert_eq!(TIOCGPGRP, 0x540F);
        prop_assert_eq!(TIOCSPGRP, 0x5410);
        prop_assert_eq!(pgrp_request(0x5401), None); // TCGETS
        prop_assert_eq!(pgrp_request(0x5402), None); // TCSETS
        prop_assert_eq!(pgrp_request(0x5413), None); // TIOCGWINSZ
    }

    // ─── winsize: struct winsize / TIOCGWINSZ / TIOCSWINSZ ──────────────────

    /// `struct winsize` is four little-endian `u16`: encoding then decoding is
    /// the identity for every field vector, and the wire layout is exactly the
    /// one Linux UAPI documents (`ws_row, ws_col, ws_xpixel, ws_ypixel`).
    #[test]
    fn winsize_round_trips_and_matches_the_uapi_layout(
        rows in any::<u16>(),
        cols in any::<u16>(),
        xpixel in any::<u16>(),
        ypixel in any::<u16>(),
    ) {
        let ws = Winsize { rows, cols, xpixel, ypixel };
        let bytes = encode_winsize(&ws);
        prop_assert_eq!(decode_winsize(&bytes), Some(ws));
        prop_assert_eq!(&bytes[0..2], &rows.to_le_bytes());
        prop_assert_eq!(&bytes[2..4], &cols.to_le_bytes());
        prop_assert_eq!(&bytes[4..6], &xpixel.to_le_bytes());
        prop_assert_eq!(&bytes[6..8], &ypixel.to_le_bytes());
        prop_assert_eq!(bytes.len(), WINSIZE_SIZE);
        // Every strict prefix is a rejection, never a truncated success.
        for cut in 0..WINSIZE_SIZE {
            prop_assert_eq!(decode_winsize(&bytes[..cut]), None);
        }
    }

    /// `surface_winsize` is integer division with a one-cell floor on non-empty
    /// surfaces (a zero row/col would make TUI programs divide by zero), and the
    /// zero-input cases stay zero.
    #[test]
    fn surface_winsize_matches_the_division_oracle(
        pw in 0usize..=8192,
        ph in 0usize..=8192,
        cw in 0usize..=64,
        ch in 0usize..=64,
    ) {
        let ws = surface_winsize(pw, ph, cw, ch);
        if pw == 0 || ph == 0 || cw == 0 || ch == 0 {
            prop_assert_eq!(ws, Winsize::default());
            prop_assert!(ws.is_empty());
        } else {
            prop_assert_eq!(ws.cols as usize, (pw / cw).max(1));
            prop_assert_eq!(ws.rows as usize, (ph / ch).max(1));
            prop_assert!(!ws.is_empty());
            prop_assert_eq!(ws.xpixel as usize, core::cmp::min(pw, u16::MAX as usize));
            prop_assert_eq!(ws.ypixel as usize, core::cmp::min(ph, u16::MAX as usize));
        }
    }

    /// `winsize_request` classifies exactly the two Linux ioctls; the constants
    /// are the UAPI ones and the neighbouring termios requests are not winsize.
    #[test]
    fn winsize_request_classifies_exactly_the_two_ioctls(request in any::<u64>()) {
        prop_assert_eq!(TIOCGWINSZ, 0x5413);
        prop_assert_eq!(TIOCSWINSZ, 0x5414);
        let oracle = match request {
            0x5413 => Some(WinsizeRequest::Get),
            0x5414 => Some(WinsizeRequest::Set),
            _ => None,
        };
        prop_assert_eq!(winsize_request(request), oracle);
        prop_assert_eq!(winsize_request(0x5401), None); // TCGETS
        prop_assert_eq!(winsize_request(0x540F), None); // TIOCGPGRP
        prop_assert_eq!(winsize_request(0x5410), None); // TIOCSPGRP
    }

    /// `winsize_changed` is exactly "some field differs": SIGWINCH must fire on
    /// a real resize and stay quiet on an idempotent `TIOCSWINSZ`.
    #[test]
    fn winsize_change_is_fieldwise_inequality(
        a in (any::<u16>(), any::<u16>(), any::<u16>(), any::<u16>()),
        b in (any::<u16>(), any::<u16>(), any::<u16>(), any::<u16>()),
    ) {
        let a = Winsize { rows: a.0, cols: a.1, xpixel: a.2, ypixel: a.3 };
        let b = Winsize { rows: b.0, cols: b.1, xpixel: b.2, ypixel: b.3 };
        prop_assert_eq!(winsize_changed(&a, &b), a != b);
        prop_assert_eq!(winsize_changed(&a, &a), false);
        prop_assert_eq!(winsize_changed(&b, &b), false);
    }

    /// Two terminals are independent: moving/altering one never changes the
    /// other's foreground group or TOSTOP. This is the pure restatement of
    /// "per-session tty" — the kernel keeps one of these per console session.
    #[test]
    fn two_terminals_are_independent(
        fg_a in 1u64..=4096,
        fg_b in 1u64..=4096,
        tostop_a in any::<bool>(),
        tostop_b in any::<bool>(),
        probe in 1u64..=4096,
    ) {
        let mut a = TtyJobControl::new(fg_a, tostop_a);
        let mut b = TtyJobControl::new(fg_b, tostop_b);
        a.set_foreground(probe);
        a.set_tostop(!tostop_a);
        prop_assert_eq!(a.foreground(), probe);
        prop_assert_eq!(b.foreground(), fg_b);
        prop_assert_eq!(b.tostop(), tostop_b);
        prop_assert_eq!(a.access(probe, TtyAccess::Write), TtyVerdict::Allow);
        if probe != fg_b {
            prop_assert_eq!(
                b.access(probe, TtyAccess::Read),
                TtyVerdict::Background(SIGTTIN)
            );
        }
    }
}
