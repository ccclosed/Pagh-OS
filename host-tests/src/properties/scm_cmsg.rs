// Feature: AF_UNIX SCM_RIGHTS fd passing (desktop B, `docs/desktop-b.md` §6),
// property `scm_cmsg`: the ancillary-data layout, the send-side decoder and the
// receive-side delivery plan.
//
// The effectful half (fd tables, user memory, `mmap(MAP_SHARED)`) is exercised
// in-QEMU by the `desktop-b::SCM_RIGHTS` routines. This file pins the pure
// decisions those routines rely on:
//   * `decode_send_control` is `__scm_send` for the `SCM_RIGHTS` subset: a
//     malformed cmsg (bad `cmsg_len`, short header, foreign SOL_SOCKET type)
//     must be rejected, several cmsgs are concatenated, and no input — however
//     adversarial — may yield more than `SCM_MAX_FD` descriptors or panic;
//   * `plan_rights` is the Linux-observed `recvmsg` truncation rule: never
//     claim more `msg_controllen` than the caller offered, deliver as many fds
//     as fit, and flag `MSG_CTRUNC` exactly when some were left out;
//   * `encode_rights`/`decode_send_control` round-trip, so a delivery plan that
//     says "one cmsg with N fds" is parseable by the same cmsg(3) loop the
//     userspace compositor uses.

use crate::cmsg;
use proptest::prelude::*;

proptest! {
    /// Encoding a `SCM_RIGHTS` fd list and decoding it back yields the same
    /// list — for every legal length, including negative `int` values (the
    /// decoder reports them as-is; the kernel layer maps them to `EBADF`).
    #[test]
    fn encode_decode_roundtrip(fds in proptest::collection::vec(any::<i32>(), 0..=cmsg::SCM_MAX_FD)) {
        let mut buf = vec![0u8; cmsg::cmsg_space(4 * fds.len())];
        let len = cmsg::encode_rights(&mut buf, &fds);
        prop_assert_eq!(len, Some(cmsg::cmsg_len(4 * fds.len())));
        prop_assert_eq!(cmsg::decode_send_control(&buf), Ok(fds));
    }

    /// `recvmsg` delivery plans obey the caller's buffer and never invent fds:
    /// `deliver <= count`, `deliver <= capacity`-implied, `used <= capacity`,
    /// and `MSG_CTRUNC` is set exactly when `deliver < count`.
    #[test]
    fn rights_plan_is_bounded_and_exact(
        count in 0usize..=cmsg::SCM_MAX_FD,
        capacity in 0usize..=4096usize,
    ) {
        let plan = cmsg::plan_rights(count, capacity);
        prop_assert!(plan.deliver <= count);
        prop_assert!(plan.used <= capacity);
        prop_assert_eq!(plan.truncated, plan.deliver < count);
        if count == 0 {
            prop_assert_eq!((plan.deliver, plan.cmsg_len, plan.used, plan.truncated),
                            (0, 0, 0, false));
        } else if capacity < cmsg::cmsg_len(4) {
            prop_assert_eq!((plan.deliver, plan.cmsg_len, plan.used, plan.truncated),
                            (0, 0, 0, true));
        } else {
            let fit = (capacity - cmsg::CMSGHDR_SIZE) / 4;
            prop_assert_eq!(plan.deliver, core::cmp::min(count, fit));
            prop_assert_eq!(plan.cmsg_len, cmsg::cmsg_len(4 * plan.deliver));
            // `cmsg_len` (без паддинга) обязан влезть в предложенную ёмкость.
            prop_assert!(plan.cmsg_len <= capacity);
            if plan.truncated {
                // Linux помечает весь предоставленный буфер использованным.
                prop_assert_eq!(plan.used, capacity);
            } else {
                prop_assert_eq!(
                    plan.used,
                    core::cmp::min(capacity, cmsg::cmsg_space(4 * plan.deliver))
                );
            }
        }
    }

    /// Arbitrary/adversarial control bytes never panic and never yield more
    /// than `SCM_MAX_FD`: the decoder is the kernel's trust boundary for what
    /// an untrusted process puts in `msg_control`.
    #[test]
    fn decode_arbitrary_control_is_total(bytes in proptest::collection::vec(any::<u8>(), 0..=4096)) {
        match cmsg::decode_send_control(&bytes) {
            Ok(fds) => prop_assert!(fds.len() <= cmsg::SCM_MAX_FD),
            Err(_) => {}
        }
    }
}

/// A hand-built control buffer must be decoded exactly as the C `for_each_cmsghdr`
/// loop walks it: `cmsg_len` is unaligned, the next header starts at
/// `CMSG_ALIGN(cmsg_len)`, and the head of the buffer may be a foreign level.
#[test]
fn decode_walks_aligned_cmsgs_and_skips_foreign_levels() {
    // foreign level (IPPROTO_TCP) cmsg with 4 payload bytes, then SCM_RIGHTS.
    let mut buf = vec![0u8; cmsg::cmsg_space(4) + cmsg::cmsg_space(4)];
    buf[0..8].copy_from_slice(&(cmsg::cmsg_len(4) as u64).to_ne_bytes());
    buf[8..12].copy_from_slice(&6u32.to_ne_bytes());
    buf[12..16].copy_from_slice(&1u32.to_ne_bytes());
    let second = cmsg::cmsg_space(4);
    buf[second..second + 8]
        .copy_from_slice(&(cmsg::cmsg_len(4) as u64).to_ne_bytes());
    buf[second + 8..second + 12].copy_from_slice(&(cmsg::SOL_SOCKET as u32).to_ne_bytes());
    buf[second + 12..second + 16].copy_from_slice(&(cmsg::SCM_RIGHTS as u32).to_ne_bytes());
    buf[second + 16..second + 20].copy_from_slice(&42i32.to_ne_bytes());
    assert_eq!(cmsg::decode_send_control(&buf), Ok(vec![42]));
}

/// Linux-observed truncation vectors (probe on the host kernel): control
/// capacities 0..=19 deliver nothing, 20..=23 deliver the first fd of three,
/// 24..=27 the first two, and 28+ all three with `CMSG_SPACE(12) = 32` used.
#[test]
fn plan_rights_matches_linux_probe_vectors() {
    let expected = [
        (0usize, 0usize, 0usize, true),
        (8, 0, 0, true),
        (16, 0, 0, true),
        (19, 0, 0, true),
        (20, 1, 20, true),
        (23, 1, 23, true),
        (24, 2, 24, true),
        (27, 2, 27, true),
        (28, 3, 28, false),
        (31, 3, 31, false),
        (32, 3, 32, false),
        (256, 3, 32, false),
    ];
    for (capacity, deliver, used, truncated) in expected {
        let plan = cmsg::plan_rights(3, capacity);
        assert_eq!(
            (plan.deliver, plan.used, plan.truncated),
            (deliver, used, truncated),
            "capacity {capacity}"
        );
        assert_eq!(plan.cmsg_len, if deliver == 0 { 0 } else { 16 + 4 * deliver });
    }
}