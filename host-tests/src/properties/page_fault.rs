// Feature: security hardening (2026-11-13 attack audit, F-x86a-4 backstop),
// property `page_fault`: the page-fault handler's disposition policy.
//
// The kernel part is `idt.rs::page_fault_handler`: a fault that
// `mem_sys::handle_user_page_fault` refuses used to be either "kill the ring-3
// Compat_Process" (USER_MODE) or "halt the machine" (everything else). Two
// machine kills were fixed here:
//
//   * F-x86a-4: a syscall handler that dereferences a user buffer AFTER a
//     sibling thread unmapped it faults in KERNEL mode on a user address, so the
//     otherwise-correct halt turned a ring-3 triggerable stale pointer into a
//     machine kill. `page_fault_action` adds that one backstop case.
//   * S4b: a ring-3 fault only killed a `Compat_Process`, so a userspace client
//     without compat state (native ring-3 task before `install_compat`) and any
//     future non-Linux userspace died into `halt()` along with the machine. The
//     ring-3 branch is now unconditional.
//
// This file pins the boundaries, in particular the one the fix must NOT move:
//
//   * ring-3 fault (compat OR native task)  -> kill the task, never the machine;
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
fn ring3_fault_always_kills_the_task() {
    // A compatible process is killed; the machine keeps running.
    assert_eq!(
        page_fault_action(true, 0x40_0000, USER_ADDR_MAX, true),
        PageFaultAction::KillProcess
    );
    // S4b: a native ring-3 task (no compat state) must be killed too — it used
    // to fall through to `halt()` and take the machine down.
    assert_eq!(
        page_fault_action(true, 0x40_0000, USER_ADDR_MAX, false),
        PageFaultAction::KillProcess
    );
    // The ring-3 rule never depended on the faulting address: a wild jump or a
    // kernel-address dereference from userspace still kills only the task.
    for addr in [0u64, 4095, 4096, KERNEL_ADDR, u64::MAX] {
        assert_eq!(
            page_fault_action(true, addr, USER_ADDR_MAX, false),
            PageFaultAction::KillProcess
        );
        assert_eq!(
            page_fault_action(true, addr, USER_ADDR_MAX, true),
            PageFaultAction::KillProcess
        );
    }
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

    /// Ring-3 faults always kill the faulting task for every address and every
    /// compat state (S4b: they must never halt the machine).
    #[test]
    fn ring3_faults_never_halt_the_machine(
        addr in any::<u64>(),
        compat in any::<bool>(),
    ) {
        prop_assert_eq!(
            page_fault_action(true, addr, USER_ADDR_MAX, compat),
            PageFaultAction::KillProcess
        );
    }
}