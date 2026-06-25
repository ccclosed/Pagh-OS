// Feature: linux-binary-compat, Property 11: brk transitions follow the program-break rules

use crate::mem::{plan_brk, BrkOutcome};
use crate::validate::USER_ADDR_MAX;
use proptest::prelude::*;

const PAGE_SIZE: u64 = 4096;

#[inline]
fn page_down(addr: u64) -> u64 {
    addr & !(PAGE_SIZE - 1)
}

#[inline]
fn page_up(addr: u64) -> u64 {
    page_down(addr.saturating_add(PAGE_SIZE - 1))
}

/// Strategy biased to hit every branch: values both below and around
/// `USER_ADDR_MAX`, plus a few well above it.
fn addr() -> impl Strategy<Value = u64> {
    prop_oneof![
        0u64..=(USER_ADDR_MAX + 8192),
        any::<u64>(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any `(initial, current, requested)`, `plan_brk` returns:
    ///   * `Unchanged(current)` when `requested == 0`, `requested >= MAX`, or
    ///     `requested < initial`,
    ///   * `Shrink(requested)` when `initial <= requested <= current`,
    ///   * `Grow{ new_brk = requested, map span = [page_down(current),
    ///     page_up(requested)) }` when `current < requested < MAX`.
    #[test]
    fn brk_transitions_follow_the_rules(
        initial in addr(),
        current in addr(),
        requested in addr(),
    ) {
        let expected = if requested == 0
            || requested >= USER_ADDR_MAX
            || requested < initial
        {
            BrkOutcome::Unchanged(current)
        } else if requested <= current {
            BrkOutcome::Shrink(requested)
        } else {
            BrkOutcome::Grow {
                new_brk: requested,
                map_from: page_down(current),
                map_to: page_up(requested),
            }
        };

        prop_assert_eq!(plan_brk(initial, current, requested), expected);
    }
}
