// Feature: linux-binary-compat, Property 6: lseek computes a non-negative absolute offset or rejects with EINVAL

use crate::errno::Errno;
use crate::io::{plan_lseek, SEEK_CUR, SEEK_END, SEEK_SET};
use proptest::prelude::*;

/// Model of the absolute-offset computation in `i128` (R2.7, R2.15).
fn model(whence: u32, cur: u64, size: u64, delta: i64) -> Result<u64, Errno> {
    let base: u64 = match whence {
        SEEK_SET => 0,
        SEEK_CUR => cur,
        SEEK_END => size,
        _ => return Err(Errno::EINVAL),
    };
    let absolute = base as i128 + delta as i128;
    if absolute < 0 || absolute > u64::MAX as i128 {
        return Err(Errno::EINVAL);
    }
    Ok(absolute as u64)
}

/// Whence generator: the three valid values plus arbitrary (mostly invalid) ones.
fn whence_strategy() -> impl Strategy<Value = u32> {
    prop_oneof![
        Just(SEEK_SET),
        Just(SEEK_CUR),
        Just(SEEK_END),
        any::<u32>(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For valid whence values `plan_lseek` returns the non-negative absolute
    /// offset; unknown whence or a negative result yields `EINVAL`.
    #[test]
    fn lseek_computes_absolute_or_einval(
        whence in whence_strategy(),
        cur in any::<u64>(),
        size in any::<u64>(),
        delta in any::<i64>(),
    ) {
        let got = plan_lseek(whence, cur, size, delta);
        let expected = model(whence, cur, size, delta);
        prop_assert_eq!(got, expected);

        // Unknown whence is always rejected.
        if whence != SEEK_SET && whence != SEEK_CUR && whence != SEEK_END {
            prop_assert_eq!(got, Err(Errno::EINVAL));
        }

        // A successful result is always a non-negative absolute offset that fits u64.
        if let Ok(abs) = got {
            let base: i128 = match whence {
                SEEK_SET => 0,
                SEEK_CUR => cur as i128,
                SEEK_END => size as i128,
                _ => unreachable!(),
            };
            prop_assert_eq!(abs as i128, base + delta as i128);
        }
    }
}
