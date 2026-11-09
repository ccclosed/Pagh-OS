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
    decode_pgrp_arg, may_set_foreground, pgrp_request, PgrpRequest, TtyAccess, TtyJobControl,
    TtyVerdict, TIOCGPGRP, TIOCSPGRP,
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
}
