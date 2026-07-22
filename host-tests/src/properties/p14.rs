// Feature: linux-binary-compat, Property 14: protection flags map to writable/no-execute bits consistently

use crate::mem::{prot_to_flags, PROT_EXEC, PROT_WRITE};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any protection bitmask, `prot_to_flags` sets the writable bit iff
    /// `PROT_WRITE` is present and the no-execute bit iff `PROT_EXEC` is absent.
    #[test]
    fn prot_maps_to_writable_and_nx(prot in any::<u32>()) {
        let (writable, nx) = prot_to_flags(prot);
        prop_assert_eq!(writable, prot & PROT_WRITE != 0);
        prop_assert_eq!(nx, prot & PROT_EXEC == 0);
    }
}
