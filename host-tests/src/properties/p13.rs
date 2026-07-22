// Feature: linux-binary-compat, Property 13: munmap planning unmaps exactly the covered pages or rejects

use crate::errno::Errno;
use crate::mem::{plan_munmap, MmapRegion, MunmapPlan};
use proptest::prelude::*;

const PAGE_SIZE: u64 = 4096;

/// Reference for "is this 4 KiB page covered by some region".
fn region_contains_page(r: &MmapRegion, page: u64) -> bool {
    match r.pages.checked_mul(PAGE_SIZE).and_then(|span| r.base.checked_add(span)) {
        Some(end) => page >= r.base && page < end,
        None => false,
    }
}

/// Independent reference implementation of the planner's decision.
fn expected(base: u64, len: u64, regions: &[MmapRegion]) -> MunmapPlan {
    if base & (PAGE_SIZE - 1) != 0 || len == 0 {
        return MunmapPlan::Reject(Errno::EINVAL);
    }
    let pages = match len.checked_add(PAGE_SIZE - 1) {
        Some(n) => n / PAGE_SIZE,
        None => return MunmapPlan::Reject(Errno::EINVAL),
    };
    for i in 0..pages {
        let page = match i.checked_mul(PAGE_SIZE).and_then(|off| base.checked_add(off)) {
            Some(p) => p,
            None => return MunmapPlan::Reject(Errno::EINVAL),
        };
        if !regions.iter().any(|r| region_contains_page(r, page)) {
            return MunmapPlan::Reject(Errno::EINVAL);
        }
    }
    MunmapPlan::Unmap { base, pages }
}

fn region() -> impl Strategy<Value = MmapRegion> {
    ((0u64..256).prop_map(|n| n * PAGE_SIZE), 1u64..8, any::<bool>(), any::<bool>())
        .prop_map(|(base, pages, writable, nx)| MmapRegion { base, pages, writable, nx })
}

/// A base that is page-aligned most of the time, occasionally misaligned, kept in
/// the same small address window as the generated regions so coverage is frequent.
fn base() -> impl Strategy<Value = u64> {
    prop_oneof![
        (0u64..256).prop_map(|n| n * PAGE_SIZE),
        (0u64..256 * PAGE_SIZE),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// `plan_munmap` returns `Unmap{ pages }` when `base` is page-aligned and every
    /// referenced page lies within a previously-mapped region; otherwise it returns
    /// `Reject(EINVAL)`. The region set is never mutated (the planner takes it by
    /// shared reference).
    #[test]
    fn munmap_plan_matches_spec(
        regions in prop::collection::vec(region(), 0..6),
        base in base(),
        len in 0u64..(16 * PAGE_SIZE),
    ) {
        let before = regions.clone();
        let got = plan_munmap(base, len, &regions);

        prop_assert_eq!(got, expected(base, len, &regions));
        // Purity: the region set is unchanged.
        prop_assert_eq!(&regions, &before);
    }
}
