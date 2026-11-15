// Round-3 audit F-r3-lead-1 — host properties for the address-space teardown
// accounting (`src/task/as_refs.rs`).
//
// `execve` replaces the calling task's address space and then may free the old
// PML4 frame tree. That is only safe when no other LIVE task still runs on it.
// The kernel keeps live tasks in three registries and the old
// `drop_exclusive_user_space` only looked at two (ready queue + pending
// reaps), so a sibling thread parked by SIGSTOP/SIGTSTP — out of the ready
// queue until SIGCONT — was invisible and its shared page tables were freed
// under it. The fix consults all three; these properties pin the policy the
// kernel now applies:
//
//   * a reference in ANY registry blocks the free, whichever registry it is;
//   * the asking/reaping pid never counts itself;
//   * `None` means genuinely exclusive, so "never free without proof" is the
//     direction that can only leak, never UAF.

use crate::as_refs::{referenced_by, referencing_pid, AsRef, Registry};
use proptest::prelude::*;

fn reg_of(registry: Registry) -> u8 {
    match registry {
        Registry::Ready => 0,
        Registry::Stopped => 1,
        Registry::PendingReap => 2,
    }
}

/// Reference model: the first task, scanning ready -> stopped -> pending in
/// order, that references `cr3` and is not `exclude`.
fn model_referenced_by(
    cr3: u64,
    exclude: u64,
    ready: &[AsRef],
    stopped: &[AsRef],
    pending: &[AsRef],
) -> Option<(u64, u8)> {
    for (tasks, id) in [(ready, 0u8), (stopped, 1u8), (pending, 2u8)] {
        if let Some(t) = tasks.iter().find(|t| t.pid != exclude && t.cr3 == cr3) {
            return Some((t.pid, id));
        }
    }
    None
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// The combined policy is exactly "first non-excluded reference in
    /// ready -> stopped -> pending order"; every registry — including the
    /// SIGSTOP-parked one the buggy path ignored — blocks the teardown.
    #[test]
    fn every_registry_blocks_and_order_is_stable(
        cr3 in 0u64..8,
        exclude in 0u64..6,
        ready in prop::collection::vec(
            (0u64..6, 0u64..8).prop_map(|(pid, cr3)| AsRef { pid, cr3 }), 0..6),
        stopped in prop::collection::vec(
            (0u64..6, 0u64..8).prop_map(|(pid, cr3)| AsRef { pid, cr3 }), 0..6),
        pending in prop::collection::vec(
            (0u64..6, 0u64..8).prop_map(|(pid, cr3)| AsRef { pid, cr3 }), 0..6),
    ) {
        let got = referenced_by(cr3, exclude, &ready, &stopped, &pending)
            .map(|(pid, registry)| (pid, reg_of(registry)));
        prop_assert_eq!(got, model_referenced_by(cr3, exclude, &ready, &stopped, &pending));
    }

    /// A reference that exists ONLY in the stopped registry (a SIGSTOP-parked
    /// sibling) is enough to forbid the free — the exact case that used to
    /// free the page tables. The same holds for a pending reap, which would
    /// otherwise double-free the PML4.
    #[test]
    fn stopped_or_pending_reference_alone_forbids_teardown(
        cr3 in 1u64..0x1000,
        sibling in 2u64..32,
        parked_first in any::<bool>(),
    ) {
        let stopped = if parked_first { vec![AsRef { pid: sibling, cr3 }] } else { vec![] };
        let pending = if parked_first { vec![] } else { vec![AsRef { pid: sibling, cr3 }] };
        prop_assert_eq!(
            referenced_by(cr3, 1, &[], &stopped, &pending),
            Some((sibling, if parked_first { Registry::Stopped } else { Registry::PendingReap }))
        );
    }

    /// `referencing_pid` in one registry ignores the asking task and other
    /// address spaces, and reports the first matching live task.
    #[test]
    fn referencing_pid_filters_self_and_foreign_spaces(
        cr3 in 0u64..8,
        exclude in 0u64..6,
        tasks in prop::collection::vec(
            (0u64..6, 0u64..8).prop_map(|(pid, cr3)| AsRef { pid, cr3 }), 0..8),
    ) {
        let got = referencing_pid(cr3, exclude, tasks.iter().copied());
        let want = tasks.iter().find(|t| t.pid != exclude && t.cr3 == cr3).map(|t| t.pid);
        prop_assert_eq!(got, want);
    }
}

/// The audit scenario in one literal case: a stopped sibling keeps the space.
#[test]
fn stopped_sibling_keeps_the_address_space_alive() {
    let old_cr3 = 0x2128_1000u64;
    let me = 6u64;
    let sibling = 7u64;
    // Only the SIGSTOP-parked sibling references the old space.
    let ready: Vec<AsRef> = vec![];
    let stopped = vec![AsRef {
        pid: sibling,
        cr3: old_cr3,
    }];
    let pending: Vec<AsRef> = vec![];
    assert_eq!(
        referenced_by(old_cr3, me, &ready, &stopped, &pending),
        Some((sibling, Registry::Stopped))
    );
    // Excluding the sibling's own pid (the exit reaper's view) frees it.
    assert_eq!(referenced_by(old_cr3, sibling, &ready, &stopped, &pending), None);
    // A different address space is never impacted.
    assert_eq!(
        referenced_by(0xdeaf_0000, me, &ready, &stopped, &pending),
        None
    );
}