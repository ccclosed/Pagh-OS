// Feature: linux-binary-compat, Property 10: clock_gettime normalizes ticks into a valid timespec or rejects bad clocks

use crate::errno::Errno;
use crate::rand_clock::{ticks_to_timespec, CLOCK_MONOTONIC, CLOCK_REALTIME};
use proptest::prelude::*;

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// Clock-id generator: the two supported ids plus arbitrary (mostly invalid) ones.
fn clock_strategy() -> impl Strategy<Value = u32> {
    prop_oneof![
        Just(CLOCK_REALTIME),
        Just(CLOCK_MONOTONIC),
        any::<u32>(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For supported clocks, `ticks_to_timespec` yields `tv_nsec ∈ [0, 1e9)` with
    /// `tv_sec * 1e9 + tv_nsec == ticks * 1e9 / tick_hz`; unsupported clock ids
    /// yield `EINVAL`.
    #[test]
    fn clock_normalizes_or_rejects(
        ticks in any::<u64>(),
        clock_id in clock_strategy(),
        tick_hz in 1u64..=u64::MAX,
    ) {
        let got = ticks_to_timespec(ticks, clock_id, tick_hz);

        let supported = clock_id == CLOCK_REALTIME || clock_id == CLOCK_MONOTONIC;
        if !supported {
            prop_assert_eq!(got, Err(Errno::EINVAL));
        } else {
            let ts = got.expect("supported clock with nonzero hz must succeed");

            // tv_nsec normalized into [0, 1e9).
            prop_assert!(ts.tv_nsec >= 0 && (ts.tv_nsec as u128) < NANOS_PER_SEC,
                "tv_nsec {} out of [0, 1e9)", ts.tv_nsec);

            // Total nanoseconds match the model computed in u128.
            let expected_total: u128 = (ticks as u128 * NANOS_PER_SEC) / tick_hz as u128;
            let got_total: u128 = ts.tv_sec as u128 * NANOS_PER_SEC + ts.tv_nsec as u128;
            prop_assert_eq!(got_total, expected_total);
        }
    }
}
