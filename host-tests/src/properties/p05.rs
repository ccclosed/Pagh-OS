// Feature: linux-binary-compat, Property 5: read clamps to EOF and advances the offset by bytes copied

use crate::io::plan_read;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any `(size, off, count)`, `plan_read` copies
    /// `min(count, size.saturating_sub(off))` and advances the offset by exactly
    /// that many bytes.
    #[test]
    fn read_clamps_to_eof_and_advances(
        size in any::<u64>(),
        off in any::<u64>(),
        count in any::<u64>(),
    ) {
        let (copied, new_off) = plan_read(size, off, count);

        let expected_copied = core::cmp::min(count, size.saturating_sub(off));
        prop_assert_eq!(copied, expected_copied);
        prop_assert_eq!(new_off, off + copied);

        // The offset never advances past EOF when the read started within the file.
        if off <= size {
            prop_assert!(new_off <= size);
        } else {
            // Reading at or beyond EOF copies nothing and leaves the offset put.
            prop_assert_eq!(copied, 0u64);
            prop_assert_eq!(new_off, off);
        }
    }
}
