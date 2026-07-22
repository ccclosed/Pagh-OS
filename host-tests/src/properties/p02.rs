// Feature: linux-binary-compat, Property 2: User-pointer range validation never accepts an out-of-range buffer

use crate::validate::{check_user_range, spanned_pages, PtrCheck, USER_ADDR_MAX};
use proptest::prelude::*;

const PAGE_SIZE: u64 = 4096;

/// Independent model of the range check (R1.5).
fn model_ok(start: u64, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    if start >= USER_ADDR_MAX {
        return false;
    }
    match start.checked_add(len) {
        Some(end) => end <= USER_ADDR_MAX,
        None => false,
    }
}

/// A `start` generator that exercises the boundary region around `USER_ADDR_MAX`
/// as well as the full `u64` range.
fn boundary_start() -> impl Strategy<Value = u64> {
    prop_oneof![
        any::<u64>(),
        (0u64..(8 * PAGE_SIZE)).prop_map(|d| USER_ADDR_MAX.saturating_sub(d)),
        (0u64..(8 * PAGE_SIZE)).prop_map(|d| USER_ADDR_MAX.saturating_add(d)),
        0u64..(16 * PAGE_SIZE),
    ]
}

/// A `len` generator: small lengths (so the spanned-page set stays materializable)
/// plus large/extreme values that trigger the overflow and over-`MAX` branches.
fn len_strategy() -> impl Strategy<Value = u64> {
    prop_oneof![
        // Small lengths: the page enumeration is fully checked for these.
        0u64..(64 * PAGE_SIZE),
        // Extreme lengths: exercise overflow / end-past-MAX rejection only.
        Just(USER_ADDR_MAX),
        Just(USER_ADDR_MAX + 1),
        Just(u64::MAX),
        (USER_ADDR_MAX / 2)..u64::MAX,
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// `check_user_range` returns `Ok` exactly when the buffer is empty or wholly
    /// within the user half; and for a `len > 0` whose page span is small enough to
    /// materialize, the spanned page bases are exactly `first_page ..= last_page`
    /// stepping by 4096 (empty for `len == 0`).
    #[test]
    fn range_validation_matches_model(start in boundary_start(), len in len_strategy()) {
        let got = check_user_range(start, len);
        let expected = if model_ok(start, len) { PtrCheck::Ok } else { PtrCheck::Efault };
        prop_assert_eq!(got, expected,
            "check_user_range({}, {}) disagreed with the model", start, len);

        // Page enumeration.
        if len == 0 {
            let pages: alloc::vec::Vec<u64> = spanned_pages(start, len).collect();
            prop_assert!(pages.is_empty(), "len==0 must yield no spanned pages");
        } else {
            let first_page = start & !(PAGE_SIZE - 1);
            let last_addr = start.saturating_add(len - 1);
            let last_page = last_addr & !(PAGE_SIZE - 1);
            let page_count = (last_page - first_page) / PAGE_SIZE + 1;

            // Only materialize and compare when the span is small. Large spans (from
            // the extreme-length generator) would require billions of entries; their
            // validation is already covered by the model check above.
            if page_count <= 4096 {
                let pages: alloc::vec::Vec<u64> = spanned_pages(start, len).collect();

                let mut expected_pages = alloc::vec::Vec::new();
                let mut p = first_page;
                loop {
                    expected_pages.push(p);
                    if p == last_page {
                        break;
                    }
                    p += PAGE_SIZE;
                }

                prop_assert_eq!(pages, expected_pages,
                    "spanned_pages({}, {}) did not match first..=last step 4096", start, len);
            }
        }
    }
}
