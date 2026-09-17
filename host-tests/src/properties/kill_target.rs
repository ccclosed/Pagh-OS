// Feature: linux-binary-compat (issue #12), property `kill_target`: the `kill(2)`
// argument model is total and matches an independent oracle over the whole input
// space —
//    * both C `int` arguments are truncated to 32 bits and sign-extended, so
//      `kill(-pgid, sig)` and a negative (invalid) `sig` are never misread as
//      huge positive numbers;
//    * the target classification (`pid > 0`, `0`, `-1`, `-pgid`) and the errno
//      matrix match man7 kill(2) / `kill_something_info`: `EINVAL` for an
//      invalid signal BEFORE any target lookup, `ESRCH` for `INT_MIN`;
//    * `pid == INT_MIN` is the documented `ESRCH` (not `EINVAL`) quirk;
//    * the group pick is never a non-member and is deterministic — the group's
//      leader when it is live, else the lowest live member, so a group signal is
//      queued ONCE per group (Linux's shared pending list semantics) instead of
//      once per thread.

use crate::errno::Errno;
use crate::kill::*;
use proptest::prelude::*;

/// Independent oracle for [`kill_args`], written straight from the man page and
/// `kernel/signal.c::kill_something_info` — deliberately not derived from the
/// implementation under test.
fn oracle_args(pid: i64, sig: i64) -> Result<(KillTarget, u64), Errno> {
    // `if (sig && !valid_sig(sig)) return -EINVAL;` — validity first.
    if sig < 0 || sig > 64 {
        return Err(Errno::EINVAL);
    }
    // `if (pid == INT_MIN) return -ESRCH;` — the -pid overflow guard.
    if pid == i32::MIN as i64 {
        return Err(Errno::ESRCH);
    }
    let target = if pid == 0 {
        KillTarget::OwnGroup
    } else if pid == -1 {
        KillTarget::All
    } else if pid > 0 {
        KillTarget::Pid(pid as u64)
    } else {
        KillTarget::Group((-pid) as u64)
    };
    Ok((target, sig as u64))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// Every `(pid, sig)` pair classifies exactly as the oracle says.
    #[test]
    fn kill_args_matches_the_oracle(pid in any::<i64>(), sig in any::<i64>()) {
        prop_assert_eq!(kill_args(pid, sig), oracle_args(pid, sig));
    }

    /// Both raw register values decode by 32-bit truncation + sign extension,
    /// for every bit pattern the kernel can be handed.
    #[test]
    fn arguments_truncate_to_32_bits(raw in any::<u64>()) {
        prop_assert_eq!(decode_pid(raw), (raw as u32) as i32 as i64);
        prop_assert_eq!(decode_sig(raw), (raw as u32) as i32 as i64);
        prop_assert_eq!(kill_sig_valid(decode_sig(raw)), (raw as u32) <= 64);
    }

    /// An invalid signal is `EINVAL` no matter what the pid says — the check
    /// happens before the target is resolved (so `kill(0xDEAD, 65)` is EINVAL,
    /// not ESRCH).
    #[test]
    fn invalid_signal_wins_over_target_lookup(pid in any::<i64>(), sig in any::<i64>()) {
        prop_assume!(sig < 0 || sig > 64);
        prop_assert_eq!(kill_args(pid, sig), Err(Errno::EINVAL));
    }

    /// The group pick is either the group's own id or the lowest member, and it
    /// is always a live member (never an id outside the group snapshot).
    #[test]
    fn group_pick_is_a_member_and_deterministic(
        mut members in prop::collection::vec(1u64..1 << 30, 0..16),
        tgid in 1u64..1 << 30,
    ) {
        members.sort_unstable();
        members.dedup();
        let picked = pick_group_target(&members, tgid);
        prop_assert_eq!(picked.is_some(), !members.is_empty());
        if let Some(p) = picked {
            prop_assert!(members.contains(&p), "picked {} not among {:?}", p, members);
            if members.contains(&tgid) {
                prop_assert_eq!(p, tgid);
            } else {
                prop_assert_eq!(p, members[0]);
            }
            prop_assert_eq!(pick_group_target(&members, tgid), Some(p));
        }
    }

    /// The permission policy is reflexive and single-uid-permissive: uid 0 (every
    /// process in pagh) may signal anyone, and a nonzero uid may always signal
    /// itself.
    #[test]
    fn permission_policy_is_reflexive(sender in any::<u32>(), target in any::<u32>()) {
        prop_assert!(kill_permits(sender, sender));
        prop_assert_eq!(kill_permits(0, target), true);
    }
}
