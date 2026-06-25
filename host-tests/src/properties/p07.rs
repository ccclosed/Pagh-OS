// Feature: linux-binary-compat, Property 7: The fd table allocates the lowest free descriptor >= 3 and reports EBADF for absent fds

use crate::fd_alloc::{BadFd, FdSlots};
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
