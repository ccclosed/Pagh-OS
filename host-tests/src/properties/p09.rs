// Feature: linux-binary-compat, Property 9: getrandom fills exactly n bytes when n <= buflen and rejects otherwise

use crate::errno::Errno;
use crate::rand_clock::getrandom_plan;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// `getrandom_plan(buflen, n) == Ok(n)` iff `n <= buflen`, else `Err(EINVAL)`.
    #[test]
    fn getrandom_fills_or_rejects(buflen in any::<u64>(), n in any::<u64>()) {
        let got = getrandom_plan(buflen, n);
        if n <= buflen {
            prop_assert_eq!(got, Ok(n));
        } else {
            prop_assert_eq!(got, Err(Errno::EINVAL));
        }
    }
}
