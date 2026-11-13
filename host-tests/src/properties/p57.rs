// Feature: security hardening (2026-11-13 attack audit follow-up), property
// `socket_io_cap`: the `sendto`/`recvfrom` payload length policy.
//
// The kernel used to validate the user buffer with a capped length
// (`len.min(1 MiB)`) and then allocate/copy the RAW u64 `len`
// (`src/arch/x86_64/linux/mod.rs`). A process needed only 1 MiB of mapped pages
// to make the kernel request an arbitrary up-to-4-GiB allocation — an
// allocation-abort/OOM DoS of the same class as the crafted-ext2 `i_size`
// allocations (F-x86v-1), but reachable through one socket syscall.
//
// `abi::socket_payload_len` is now the single decision point:
//   * `sendto`: count > MAX_SOCKET_IO -> EMSGSIZE, before any allocation (a
//     silently truncated datagram would be a correctness bug);
//   * `recvfrom`: the count is the buffer capacity, so it is truncated to the
//     cap; the kernel validates/allocates/copies exactly that many bytes;
//   * everything the caller can ask for is bounded by the cap, and a small
//     count is returned unchanged (no behaviour change for normal users).

use crate::abi::{socket_payload_len, MAX_SOCKET_IO};
use crate::errno::Errno;
use proptest::prelude::*;

#[test]
fn sendto_refuses_oversized_datagrams() {
    // The audit's trigger: a 4 GiB-minus-one datagram with only 1 MiB mapped.
    assert_eq!(
        socket_payload_len(u32::MAX as u64, true),
        Err(Errno::EMSGSIZE)
    );
    assert_eq!(socket_payload_len(u64::MAX, true), Err(Errno::EMSGSIZE));
    // Exactly the cap is still a valid datagram.
    assert_eq!(socket_payload_len(MAX_SOCKET_IO, true), Ok(MAX_SOCKET_IO));
    assert_eq!(
        socket_payload_len(MAX_SOCKET_IO + 1, true),
        Err(Errno::EMSGSIZE)
    );
    // Small payloads are unchanged.
    assert_eq!(socket_payload_len(0, true), Ok(0));
    assert_eq!(socket_payload_len(1472, true), Ok(1472));
}

#[test]
fn recvfrom_truncates_the_capacity_to_the_cap() {
    // An oversized buffer capacity is legal on Linux; the kernel must never
    // allocate or copy more than it validated.
    assert_eq!(socket_payload_len(u32::MAX as u64, false), Ok(MAX_SOCKET_IO));
    assert_eq!(socket_payload_len(u64::MAX, false), Ok(MAX_SOCKET_IO));
    assert_eq!(socket_payload_len(MAX_SOCKET_IO, false), Ok(MAX_SOCKET_IO));
    assert_eq!(socket_payload_len(MAX_SOCKET_IO + 1, false), Ok(MAX_SOCKET_IO));
    assert_eq!(socket_payload_len(0, false), Ok(0));
    assert_eq!(socket_payload_len(65535, false), Ok(65535));
}

proptest! {
    /// Whatever the caller asks for, the kernel never touches more than the cap.
    #[test]
    fn effective_length_never_exceeds_the_cap(count in any::<u64>(), send in any::<bool>()) {
        match socket_payload_len(count, send) {
            Ok(n) => prop_assert!(n <= MAX_SOCKET_IO),
            // The only error case is an oversized sendto.
            Err(e) => {
                prop_assert!(send);
                prop_assert_eq!(e, Errno::EMSGSIZE);
                prop_assert!(count > MAX_SOCKET_IO);
            }
        }
    }

    /// A count already within the cap is returned unchanged on both paths.
    #[test]
    fn in_cap_counts_are_unchanged(count in 0u64..=MAX_SOCKET_IO, send in any::<bool>()) {
        prop_assert_eq!(socket_payload_len(count, send), Ok(count));
    }

    /// recvfrom never fails: its count is a capacity, not a datagram length.
    #[test]
    fn recvfrom_never_rejects(count in any::<u64>()) {
        prop_assert!(socket_payload_len(count, false).is_ok());
    }
}