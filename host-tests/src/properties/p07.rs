// Feature: linux-binary-compat, Property 7: The fd table allocates the lowest free descriptor >= 3 and reports EBADF for absent fds

use crate::fd_alloc::{BadFd, FdSlots, TableFull, FD_TABLE_LIMIT};
use proptest::prelude::*;
use std::collections::BTreeSet;

/// Operations applied to the fd table; fds are kept in a small range so closes and
/// gets hit both occupied and absent descriptors.
#[derive(Clone, Debug)]
enum Op {
    Alloc(u32),
    Close(u32),
    Get(u32),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        any::<u32>().prop_map(Op::Alloc),
        (0u32..16).prop_map(Op::Close),
        (0u32..16).prop_map(Op::Get),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Starting with standard streams {0,1,2} occupied, `alloc` always returns the
    /// lowest free index >= 3 and marks it occupied; `close`/`get` on an absent fd
    /// reports BadFd / None leaving the table unchanged; `close` on an open fd frees
    /// it.
    #[test]
    fn fd_table_lowest_free_and_ebadf(ops in prop::collection::vec(op(), 0..60)) {
        // 0,1,2 pre-bound (dummy stored value type u32).
        let mut table: FdSlots<u32> =
            FdSlots::from_slots(vec![Some(0u32), Some(1u32), Some(2u32)]);
        // Reference model of which fds are currently occupied.
        let mut occupied: BTreeSet<u32> = BTreeSet::from([0, 1, 2]);

        for op in ops {
            match op {
                Op::Alloc(value) => {
                    // Expected: lowest free index >= 3.
                    let mut expected = 3u32;
                    while occupied.contains(&expected) {
                        expected += 1;
                    }
                    let got = table.alloc(3, value);
                    prop_assert_eq!(got, expected);
                    prop_assert!(table.get(got).is_some());
                    occupied.insert(got);
                }
                Op::Close(fd) => {
                    let len_before = table.len();
                    if occupied.contains(&fd) {
                        prop_assert_eq!(table.close(fd), Ok(()));
                        prop_assert!(table.get(fd).is_none());
                        occupied.remove(&fd);
                    } else {
                        // Absent fd: BadFd, table length unchanged, still absent.
                        prop_assert_eq!(table.close(fd), Err(BadFd));
                        prop_assert_eq!(table.len(), len_before);
                        prop_assert!(table.get(fd).is_none());
                    }
                }
                Op::Get(fd) => {
                    prop_assert_eq!(table.get(fd).is_some(), occupied.contains(&fd));
                }
            }
        }
    }
}

/// F-r3-life-1: `fcntl(F_DUPFD, arg)` used to be able to grow the descriptor
/// table to the caller-chosen index. The audit's `arg` made the slot vector's
/// next doubling step ask the fixed kernel heap for 704 MiB and the allocation
/// panic halted the machine. `try_alloc` is the table-level backstop: it must
/// refuse an index at or above the limit *before* growing or allocating.
#[test]
fn fd_table_refuses_an_index_above_the_limit() {
    let mut table: FdSlots<u32> =
        FdSlots::from_slots(vec![Some(0u32), Some(1u32), Some(2u32)]);
    // The last admissible index still works, and the table grows only to it.
    assert_eq!(table.try_alloc(1023, FD_TABLE_LIMIT, 7), Ok(1023));
    assert_eq!(table.len(), FD_TABLE_LIMIT);
    assert!(table.get(1023).is_some());
    // Fill the holes below it; now the lowest free index is the bound itself.
    for _ in 3..1023 {
        table.alloc(3, 0);
    }
    // The next free index (1024) is at the bound: EMFILE, no growth.
    assert_eq!(table.try_alloc(0, FD_TABLE_LIMIT, 8), Err(TableFull));
    assert_eq!(table.len(), FD_TABLE_LIMIT);
}

#[test]
fn huge_fdupfd_minimum_allocates_nothing() {
    let mut table: FdSlots<u32> =
        FdSlots::from_slots(vec![Some(0u32), Some(1u32), Some(2u32)]);
    // 0x2A000000 slots × 84 bytes is the audit's 704 MiB allocation. Every
    // out-of-range minimum must be refused with the table untouched.
    for min in [FD_TABLE_LIMIT, 704_643_072usize, 1usize << 31, usize::MAX] {
        assert_eq!(table.try_alloc(min, FD_TABLE_LIMIT, 1), Err(TableFull));
        assert_eq!(table.len(), 3, "a refused minimum must not grow the table");
        assert!(table.get(3).is_none());
    }
    // A legitimate minimum below the cap still allocates the lowest free slot.
    assert_eq!(table.try_alloc(0, FD_TABLE_LIMIT, 1), Ok(3));
}

/// A full table reports EMFILE rather than growing past the bound, even when
/// `min` itself is small: `lowest_free_index` may return an index at the limit.
#[test]
fn full_fd_table_reports_emfile() {
    let mut table: FdSlots<u32> =
        FdSlots::from_slots(vec![Some(0u32), Some(1u32), Some(2u32)]);
    for _ in 3..FD_TABLE_LIMIT {
        table.alloc(3, 0);
    }
    assert_eq!(table.len(), FD_TABLE_LIMIT);
    assert_eq!(table.try_alloc(0, FD_TABLE_LIMIT, 1), Err(TableFull));
    assert_eq!(table.len(), FD_TABLE_LIMIT);
    // Closing one slot makes exactly that slot reusable.
    assert_eq!(table.close(777), Ok(()));
    assert_eq!(table.try_alloc(0, FD_TABLE_LIMIT, 1), Ok(777));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any caller-supplied minimum, `try_alloc` either returns the lowest
    /// free index `>= min` below `limit` and grows exactly to it, or returns
    /// `TableFull` with the table byte-for-byte unchanged — never an
    /// unbounded growth, never a slot placed past the limit.
    #[test]
    fn bounded_alloc_is_all_or_nothing(min in 0usize..4096, limit in 1usize..2048) {
        let mut table: FdSlots<u32> =
            FdSlots::from_slots(vec![Some(0u32), Some(1u32), Some(2u32)]);
        let before = table.len();
        match table.try_alloc(min, limit, 9) {
            Ok(fd) => {
                let fd = fd as usize;
                prop_assert!(fd < limit);
                prop_assert!(fd >= min);
                prop_assert_eq!(fd, core::cmp::max(min, 3));
                prop_assert_eq!(table.len(), fd + 1);
                prop_assert_eq!(table.get(fd as u32), Some(&9));
            }
            Err(TableFull) => {
                prop_assert_eq!(table.len(), before);
                prop_assert!(min >= limit);
            }
        }
    }
}
