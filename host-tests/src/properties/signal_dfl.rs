// Feature: linux-binary-compat (issue #12), property `signal_dfl`: which
// `SIG_DFL` dispositions have NOTHING to do in a handler.
//
// Regression behind this property (release 2.24.0 stop-gate): the tick delivery
// path handled `DefaultAction::{Term,Stop,Ignore}` but not `Cont`, so a pending
// SIGCONT raised by `bg` (job control) reached the "build rt_sigframe" branch
// with `handler == SIG_DFL == 0`; returning "from the handler" then set RIP=0
// and the process died with a user PAGE FAULT at address 0. The compositor
// (`/mnt/pd`) was killed that way right after `bg`, and every later PDP/1
// client handshake timed out — the reported e2e terminal failure.
//
// The pure half of the fix is `default_action_is_silent`: it must classify
// exactly the signals whose default action performs no user-visible delivery
// (`Ignore` and `Cont`) and must never classify a terminating or stopping
// signal as silent.

use crate::signal_frame::*;
use proptest::prelude::*;

/// Independent oracle from signal(7)'s default-action column: the signals whose
/// default action is "ignore" or "continue" and therefore never need a frame.
const SILENT_CLASS: [u64; 4] = [SIGCONT, SIGCHLD, SIGURG, SIGWINCH];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// For every signal number, "silent" is exactly `Ignore | Cont`, and never a
    /// Term/Stop signal — a Term/Stop signal routed into the silent branch would
    /// silently break termination and job control.
    #[test]
    fn silent_is_exactly_ignore_or_cont(sig in 0u64..=64) {
        let expects = matches!(
            default_action(sig),
            DefaultAction::Ignore | DefaultAction::Cont
        );
        prop_assert_eq!(default_action_is_silent(sig), expects);
        if default_action_is_silent(sig) {
            prop_assert!(default_action(sig) != DefaultAction::Term);
            prop_assert!(default_action(sig) != DefaultAction::Stop);
        }
    }

    /// The 1..=31 silent set is exactly the independent list above: adding a
    /// signal there without the table (or the reverse) is a delivery-path
    /// disagreement.
    #[test]
    fn silent_set_matches_signal_7(sig in 1u64..=31) {
        prop_assert_eq!(
            default_action_is_silent(sig),
            SILENT_CLASS.contains(&sig),
            "signal {}", sig
        );
    }
}

/// SIGCONT must be silent even though it is not "ignored": its continue effect
/// already happened when the signal was GENERATED (`resume_group`), so the
/// pending bit is only something to consume. This assertion is the direct
/// regression guard for the `bg` crash — dropping `Cont` from the predicate
/// re-introduces a frame at handler 0.
#[test]
fn sigcont_is_consumed_without_a_frame() {
    assert_eq!(default_action(SIGCONT), DefaultAction::Cont);
    assert!(default_action_is_silent(SIGCONT));
    // And the signals that DO have delivery work are never silent.
    for sig in [SIGINT, SIGKILL, SIGSEGV, SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU] {
        assert!(
            !default_action_is_silent(sig),
            "signal {sig} must reach the delivery path"
        );
    }
}
