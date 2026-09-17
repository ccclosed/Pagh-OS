// Feature: linux-binary-compat (issue #12, task t8), property `signal_stop`: the
// pure half of job control —
//   * the `wait(2)` status encodings agree with the Linux `W*` macros for every
//     signal number and exit code, and the three report kinds can never be
//     confused with each other (a stopped report, a continued report and a
//     normal exit are mutually exclusive);
//   * `is_stop_signal` is exactly "the default action is Stop", so the delivery
//     path and the generation path cannot disagree about which signals stop a
//     process;
//   * the POSIX stop/continue flushes are exact set operations: generating
//     SIGCONT discards EVERY stop-class signal and nothing else, generating a
//     stop signal discards SIGCONT and nothing else, and both are idempotent.

use crate::signal_frame::*;
use proptest::prelude::*;

/// Independent oracle for the stop-class set, written from signal(7)'s default
/// action column (SIGSTOP 19, SIGTSTP 20, SIGTTIN 21, SIGTTOU 22).
const STOP_CLASS: [u64; 4] = [19, 20, 21, 22];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// `is_stop_signal` agrees with the default-action table for every signal
    /// number, and with the independent list above.
    #[test]
    fn stop_class_is_exactly_the_stop_default_action(sig in 0u64..=64) {
        prop_assert_eq!(is_stop_signal(sig), default_action(sig) == DefaultAction::Stop);
        prop_assert_eq!(is_stop_signal(sig), STOP_CLASS.contains(&sig));
    }

    /// Generating SIGCONT discards every pending stop-class bit and preserves the
    /// rest; applying it twice changes nothing (idempotent).
    #[test]
    fn cont_generation_flushes_exactly_the_stop_class(pending in any::<u64>()) {
        let flushed = flush_stop_bits(pending);
        prop_assert_eq!(flushed & SIG_KERNEL_STOP_MASK, 0);
        prop_assert_eq!(flushed & !SIG_KERNEL_STOP_MASK, pending & !SIG_KERNEL_STOP_MASK);
        for sig in STOP_CLASS {
            prop_assert_eq!(flushed & sigbit(sig), 0, "stop bit {} survived SIGCONT", sig);
        }
        prop_assert_eq!(flush_stop_bits(flushed), flushed);
    }

    /// Generating a stop signal discards a pending SIGCONT and nothing else.
    #[test]
    fn stop_generation_flushes_exactly_sigcont(pending in any::<u64>()) {
        let flushed = flush_cont_bits(pending);
        prop_assert_eq!(flushed & sigbit(SIGCONT), 0);
        prop_assert_eq!(flushed & !sigbit(SIGCONT), pending & !sigbit(SIGCONT));
        prop_assert_eq!(flush_cont_bits(flushed), flushed);
    }

    /// A stopped report carries the stop signal in the high byte and satisfies
    /// `WIFSTOPPED` (`status & 0xff == 0x7f`); `WSTOPSIG` recovers the signal.
    #[test]
    fn stopped_status_round_trips(sig in 1u64..=64) {
        let s = wait_status_stopped(sig);
        prop_assert!(wait_status_is_stopped(s));
        prop_assert!(!wait_status_is_continued(s));
        prop_assert_eq!((s >> 8) as u64, sig);
    }

    /// The three report kinds are mutually exclusive: an exit status is never
    /// mistaken for a stop or a continue, and vice versa.
    #[test]
    fn report_kinds_never_collide(code in any::<u8>(), sig in 1u64..=64) {
        let exit = wait_status_exited(code);
        let stop = wait_status_stopped(sig);
        prop_assert!(!wait_status_is_stopped(exit));
        prop_assert!(!wait_status_is_continued(exit));
        prop_assert!(wait_status_is_continued(WAIT_STATUS_CONTINUED));
        prop_assert!(!wait_status_is_stopped(WAIT_STATUS_CONTINUED));
        prop_assert!(!wait_status_is_continued(stop));
        prop_assert_eq!(exit, (code as u32) << 8);
    }
}
