// Feature: linux-binary-compat, Property 16: static-PIE bias keeps every segment in the user half and shifts the entry

use crate::elf_classify::{choose_bias, PIE_BASE, USER_ADDR_MAX};
use proptest::prelude::*;

const PAGE_SIZE: u64 = 4096;

#[inline]
fn page_up(x: u64) -> Option<u64> {
    x.checked_add(PAGE_SIZE - 1).map(|v| v & !(PAGE_SIZE - 1))
}

fn max_vaddr_end() -> impl Strategy<Value = u64> {
    prop_oneof![
        0u64..USER_ADDR_MAX,
        any::<u64>(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// `choose_bias` yields a 4096-aligned bias with
    /// `bias + page_up(max_vaddr_end) < USER_ADDR_MAX`, or `None` when none fits.
    #[test]
    fn bias_keeps_segments_in_user_half(max in max_vaddr_end()) {
        let got = choose_bias(max);

        // Independent reference using the deterministic PIE base.
        let expected = page_up(max)
            .and_then(|end| PIE_BASE.checked_add(end))
            .filter(|&top| top < USER_ADDR_MAX)
            .map(|_| PIE_BASE);
        prop_assert_eq!(got, expected);

        if let Some(bias) = got {
            prop_assert_eq!(bias % PAGE_SIZE, 0);
            let end = page_up(max).expect("page_up must succeed when bias is chosen");
            let top = bias.checked_add(end).expect("bias+end must not overflow");
            prop_assert!(top < USER_ADDR_MAX);
        }
    }
}
