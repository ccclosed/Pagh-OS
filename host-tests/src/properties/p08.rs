// Feature: linux-binary-compat, Property 8: struct stat encoding preserves size and mode

use crate::stat::{encode_stat, S_IFREG};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any `(size, mode)`, `encode_stat` places `size` in `st_size` and `mode`
    /// in `st_mode`, and the accessors recover the inputs.
    #[test]
    fn stat_round_trips_size_and_mode(size in any::<u64>(), perms in 0u32..0o7777) {
        // Mode carries the regular-file type bits plus permission bits.
        let mode = S_IFREG | perms;

        let stat = encode_stat(size, mode);

        // st_size holds the (i64-reinterpreted) size.
        prop_assert_eq!(stat.stat_size(), size as i64);
        // st_mode holds the mode verbatim, including S_IFREG.
        prop_assert_eq!(stat.stat_mode(), mode);
        prop_assert_ne!(stat.stat_mode() & S_IFREG, 0);
    }
}
