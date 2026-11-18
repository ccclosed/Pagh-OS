// Feature: security hardening (round-3 attack audit, F-r3-arch-1), property
// `exception_trap`: the disposition policy for synchronous CPU exceptions.
//
// The kernel part is `idt.rs`: every user-reachable exception except #GP/#PF
// used to end in `halt()`, so a single ring-3 `1/0`, `ud2` or `popfq` with
// RFLAGS.TF parked the whole machine from an unprivileged process.
// `trap_frame::exception_action` applies the #GP/#PF rule to all of them and
// this file pins the boundaries:
//
//   * ring-3 exception -> always kill the faulting task: ring 3 IS user code,
//     so there is always a task to reap (`scheduler::exit_current` drops a
//     compat process and a native ring-3 task alike). Requiring compat state
//     here was the S4b defect: a userspace client without a `CompatState`
//     (native ring-3 task before `install_compat`) turned its own crash into a
//     machine kill;
//   * ring-0 exception on a USER address + compat process -> kill the process,
//     NOT the machine (a kernel-mode emulation path faulting on a user RIP is
//     still a user problem);
//   * ring-0 exception on a KERNEL address -> fatal, ALWAYS: a real kernel bug
//     must stay visible;
//   * ring-0 + user RIP on a native task -> fatal: nothing to kill.

use crate::trap_frame::{exception_action, TrapAction};
use crate::validate::USER_ADDR_MAX;
use proptest::prelude::*;

/// A representative kernel-image address (higher half).
const KERNEL_ADDR: u64 = 0xffff_ffff_8000_0000;

#[test]
fn ring3_exception_always_kills_the_task() {
    // A compat process is killed; the machine keeps running, whatever the RIP.
    for rip in [0u64, 0x40_0000, USER_ADDR_MAX - 1] {
        assert_eq!(
            exception_action(true, rip, USER_ADDR_MAX, true),
            TrapAction::KillProcess
        );
    }
    // S4b: a native ring-3 task (no compat state) has a task to kill too — it
    // must never be able to halt the machine by crashing.
    for rip in [0u64, 0x40_0000, KERNEL_ADDR] {
        assert_eq!(
            exception_action(true, rip, USER_ADDR_MAX, false),
            TrapAction::KillProcess
        );
    }
}

#[test]
fn ring0_user_rip_exception_kills_only_the_process() {
    // A kernel-mode handler faulting while pointed at user code is a user bug.
    assert_eq!(
        exception_action(false, 0x40_0000, USER_ADDR_MAX, true),
        TrapAction::KillProcess
    );
    // The last user-address byte is covered (exclusive upper bound).
    assert_eq!(
        exception_action(false, USER_ADDR_MAX - 1, USER_ADDR_MAX, true),
        TrapAction::KillProcess
    );
    // No compat process to blame -> nothing to kill -> fatal.
    assert_eq!(
        exception_action(false, 0x40_0000, USER_ADDR_MAX, false),
        TrapAction::Halt
    );
}

#[test]
fn ring0_kernel_rip_exception_stays_fatal() {
    // The fix must never swallow a genuine kernel bug.
    assert_eq!(
        exception_action(false, KERNEL_ADDR, USER_ADDR_MAX, true),
        TrapAction::Halt
    );
    // The exclusive user bound itself is a kernel address.
    assert_eq!(
        exception_action(false, USER_ADDR_MAX, USER_ADDR_MAX, true),
        TrapAction::Halt
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any address, the outcome is determined only by the documented
    /// conditions: ring-3 faults always kill, ring-0 faults kill only a compat
    /// process whose RIP is a user address, everything else is fatal.
    #[test]
    fn exception_policy_is_total_and_fail_safe(
        user_mode in any::<bool>(),
        compat in any::<bool>(),
        addr in any::<u64>(),
    ) {
        let action = exception_action(user_mode, addr, USER_ADDR_MAX, compat);
        let user_rip = addr < USER_ADDR_MAX;
        if user_mode {
            prop_assert_eq!(action, TrapAction::KillProcess);
        } else if compat && user_rip {
            prop_assert_eq!(action, TrapAction::KillProcess);
        } else {
            prop_assert_eq!(action, TrapAction::Halt);
        }
    }

    /// A ring-3 fault can never halt the machine, no matter what state the
    /// task carries: that was the S4b machine-kill.
    #[test]
    fn ring3_faults_are_never_fatal(addr in any::<u64>(), compat in any::<bool>()) {
        prop_assert_eq!(
            exception_action(true, addr, USER_ADDR_MAX, compat),
            TrapAction::KillProcess
        );
    }
}