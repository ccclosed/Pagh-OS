// Feature: linux-binary-compat, Property 12: mmap planning sizes, aligns, and protects anonymous regions or rejects invalid requests

use crate::errno::Errno;
use crate::mem::{plan_mmap, MmapPlan, MAP_ANONYMOUS, MAP_PRIVATE, PROT_EXEC, PROT_WRITE};
use proptest::prelude::*;

const PAGE_SIZE: u64 = 4096;

/// Page-aligned hint base (the planner assumes a pre-aligned hint and passes it
/// through unchanged).
fn aligned_base() -> impl Strategy<Value = u64> {
    (0u64..0x10_0000).prop_map(|n| n * PAGE_SIZE)
}

/// Either the one valid flag combination or an arbitrary bitmask.
fn flags() -> impl Strategy<Value = u32> {
    prop_oneof![Just(MAP_ANONYMOUS | MAP_PRIVATE), any::<u32>()]
}

/// Either the only accepted fd (`-1`) or an arbitrary fd.
fn fd() -> impl Strategy<Value = i64> {
    prop_oneof![Just(-1i64), any::<i64>()]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// `plan_mmap` rejects with `EINVAL` when `len == 0`, the flags are not exactly
    /// `MAP_ANONYMOUS | MAP_PRIVATE`, `fd != -1`, or the page count overflows;
    /// otherwise it maps `ceil(len/4096)` pages at the aligned hint base with
    /// `writable == (PROT_WRITE set)` and `nx == (PROT_EXEC clear)`.
    #[test]
    fn mmap_plan_matches_spec(
        len in any::<u64>(),
        prot in any::<u32>(),
        flags in flags(),
        fd in fd(),
        base in aligned_base(),
    ) {
        let got = plan_mmap(len, prot, flags, fd, base);

        let expected = if len == 0 || flags != (MAP_ANONYMOUS | MAP_PRIVATE) || fd != -1 {
            MmapPlan::Reject(Errno::EINVAL)
        } else {
            match len.checked_add(PAGE_SIZE - 1) {
                None => MmapPlan::Reject(Errno::EINVAL),
                Some(n) => MmapPlan::Map {
                    base,
                    pages: n / PAGE_SIZE,
                    writable: prot & PROT_WRITE != 0,
                    nx: prot & PROT_EXEC == 0,
                },
            }
        };

        prop_assert_eq!(got, expected);

        // Cross-check the accepted-case invariants directly.
        if let MmapPlan::Map { base, pages, writable, nx } = got {
            prop_assert_eq!(base % PAGE_SIZE, 0);
            prop_assert!(pages >= 1);
            prop_assert_eq!(writable, prot & PROT_WRITE != 0);
            prop_assert_eq!(nx, prot & PROT_EXEC == 0);
        }
    }
}
