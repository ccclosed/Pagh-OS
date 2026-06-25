// Feature: linux-binary-compat, Property 27: the nosys diagnostic is emitted at most once per distinct number per process

use crate::diag::should_log_nosys;
use alloc::collections::BTreeSet;
use proptest::prelude::*;
use std::collections::HashSet;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any sequence of unsupported syscall numbers, `should_log_nosys` returns
    /// `true` exactly once per distinct number (on its first occurrence) and
    /// `false` for every subsequent occurrence of that number.
    #[test]
    fn nosys_logged_once_per_distinct_number(
        // Small domain so repeats occur frequently within a sequence.
        numbers in prop::collection::vec(0u64..24, 0..100),
    ) {
        let mut seen: BTreeSet<u64> = BTreeSet::new();
        let mut reference: HashSet<u64> = HashSet::new();

        for nr in numbers {
            // The reference is `true` only on the first sighting of `nr`.
            let expected_first = reference.insert(nr);
            prop_assert_eq!(should_log_nosys(&mut seen, nr), expected_first);
        }
    }
}
