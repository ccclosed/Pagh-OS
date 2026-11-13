// Feature: security hardening (2026-11-13 attack audit, F-x86a-4 backstop),
// property `page_fault`: the page-fault handler's disposition policy.
//
// The kernel part is `idt.rs::page_fault_handler`: a fault that
// `mem_sys::handle_user_page_fault` refuses used to be either "kill the ring-3
// Compat_Process" (USER_MODE) or "halt the machine" (everything else). A
// syscall handler that dereferences a user buffer AFTER a sibling thread
// unmapped it (F-x86a-4) faults in KERNEL mode on a user address, so the
// otherwise-correct halt turned a ring-3 triggerable stale pointer into a
// machine kill. `trap_frame::page_fault_action` adds that one case — and this
// file pins the boundaries, in particular the one the fix must NOT move:
//
//   * ring-3 fault + compat process          -> kill the process (historical);
//   * ring-3 fault + non-compat task         -> fatal (historical);
//   * ring-0 fault on a USER address (>= the first mappable page, < the user
//     half) + compat process                -> kill the process, NOT the machine;
//   * ring-0 fault on a KERNEL address      -> fatal, ALWAYS: a real kernel bug
//     (including a NULL~0 dereference) must stay visible;
//   * ring-0 fault on the first page/NULL   -> fatal: `mmap(0, ...)` is
//     rejected, so this is a kernel bug, not a user buffer.

use crate::trap_frame::{page_fault_action, PageFaultAction};
use crate::validate::USER_ADDR_MAX;
use proptest::prelude::*;

/// A representative kernel-image address (higher half).
const KERNEL_ADDR: u64 = 0xffff_ffff_8000_0000;

#[test]
fn ring3_fault_keeps_the_historical_rule() {
    // A compatible process is killed; the machine keeps running.
    assert_eq!(
        page_fault_action(true, 0x40_0000, USER_ADDR_MAX, true),
        PageFaultAction::KillProcess
    );
    // A native task (no compat state) still takes the fatal path.
    assert_eq!(
        page_fault_action(true, 0x40_0000, USER_ADDR_MAX, false),
        PageFaultAction::Halt
    );
    // The ring-3 rule never depended on the faulting address.
    assert_eq!(
        page_fault_action(true, 0, USER_ADDR_MAX, true),
        PageFaultAction::KillProcess
    );
}

#[test]
fn ring0_user_address_fault_kills_only_the_process() {
    // F-x86a-4: the stale user pointer case. The machine must survive.
    assert_eq!(
        page_fault_action(false, 0x40_0000, USER_ADDR_MAX, true),
        PageFaultAction::KillProcess
    );
    // The last user page is covered (exclusive upper bound).
    assert_eq!(
        page_fault_action(false, USER_ADDR_MAX - 1, USER_ADDR_MAX, true),
        PageFaultAction::KillProcess
    );
    // No compat process to blame -> nothing to kill -> fatal.
    assert_eq!(
        page_fault_action(false, 0x40_0000, USER_ADDR_MAX, false),
        PageFaultAction::Halt
    );
}

#[test]
fn ring0_kernel_address_fault_stays_fatal() {
    // The Lead's invariant: the backstop must not swallow genuine kernel bugs.
    assert_eq!(
        page_fault_action(false, KERNEL_ADDR, USER_ADDR_MAX, true),
        PageFaultAction::Halt
    );
    // Boundary: USER_ADDR_MAX and above are kernel addresses.
    assert_eq!(
        page_fault_action(false, USER_ADDR_MAX, USER_ADDR_MAX, true),
        PageFaultAction::Halt
    );
    // The first page is never mappable, so a NULL~0 ring-0 fault is a kernel
    // bug (it must not be blamed on, and survived by killing, the process).
    assert_eq!(
        page_fault_action(false, 0, USER_ADDR_MAX, true),
        PageFaultAction::Halt
    );
    assert_eq!(
        page_fault_action(false, 4095, USER_ADDR_MAX, true),
        PageFaultAction::Halt
    );
    assert_eq!(
        page_fault_action(false, 4096, USER_ADDR_MAX, true),
        PageFaultAction::KillProcess
    );
}

proptest! {
    /// Any ring-0 fault on a kernel address is fatal, regardless of the process.
    #[test]
    fn ring0_kernel_address_faults_are_always_fatal(
        addr in (USER_ADDR_MAX..=u64::MAX),
        compat in any::<bool>(),
    ) {
        prop_assert_eq!(
            page_fault_action(false, addr, USER_ADDR_MAX, compat),
            PageFaultAction::Halt
        );
    }

    /// A ring-0 fault on a real user address kills iff a compat process is
    /// running; it never halts a machine that has a process to blame.
    #[test]
    fn ring0_user_address_faults_follow_compat_state(
        addr in (4096u64..USER_ADDR_MAX),
        compat in any::<bool>(),
    ) {
        let expected = if compat {
            PageFaultAction::KillProcess
        } else {
            PageFaultAction::Halt
        };
        prop_assert_eq!(
            page_fault_action(false, addr, USER_ADDR_MAX, compat),
            expected
        );
    }

    /// Ring-3 faults keep their historical rule for every address.
    #[test]
    fn ring3_faults_depend_only_on_compat_state(addr in any::<u64>()) {
        prop_assert_eq!(
            page_fault_action(true, addr, USER_ADDR_MAX, true),
            PageFaultAction::KillProcess
        );
        prop_assert_eq!(
            page_fault_action(true, addr, USER_ADDR_MAX, false),
            PageFaultAction::Halt
        );
    }
}