//! Pure `kill(2)` argument decoding, target classification and the errno
//! matrix — the decisions `signal::sys_kill` makes before it touches the
//! compat registry or the scheduler.
//!
//! Deliberately `core`-only and dependency-free (R11.6): it is `#[path]`-included
//! by `host-tests` (property `kill_target`), so the classification every errno is
//! derived from is exercised on the host for the WHOLE `i64` input space, not
//! just the paths a guest run happens to hit.
//!
//! ## Why the arguments need decoding at all
//!
//! `kill`'s prototype is `int kill(pid_t pid, int sig)` — both arguments are
//! 32-bit. On x86_64 the dispatcher reads the full 64-bit registers
//! (`linux::abi::marshal_args`), so the upper halves carry whatever the caller's
//! codegen left there. Linux truncates the C arguments, and so must we:
//! `pid = (raw as u32) as i32`, `sig = (raw as u32) as i32`. Skipping that step
//! misreads a negative `sig` (`0xFFFF_FFFF` as `int` = -1) as a huge positive
//! number, and a negative `pid` (`kill(-pgid)`) as a huge positive pid — the
//! exact bug class the `kill_target` property locks down.
//!
//! ## Errno matrix (man7 kill(2) + `kill_something_info`)
//!
//! Resolution order matters: signal validity is checked FIRST, then the target
//! is resolved, then permission.
//!
//! | argument | meaning | result |
//! |---|---|---|
//! | `sig == 0` | existence probe, no signal is sent | `Ok` if a target exists |
//! | `sig < 0` or `sig > 64` (`_NSIG-1`) | invalid signal | `EINVAL` (before any target lookup) |
//! | `pid > 0` | that process (thread) | `ESRCH` if it has no compat state |
//! | `pid == 0` | every process in the caller's process group | `ESRCH` if the group is empty |
//! | `pid == -1` | every process the caller may signal, except its own thread group and pid 1 | `ESRCH` if there is no candidate at all |
//! | `pid < -1` | every process in process group `-pid` | `ESRCH` if the group is empty |
//! | `pid == INT_MIN` | `-INT_MIN` overflows, so Linux returns | `ESRCH` (NOT `EINVAL`) |
//! | any | no permission to signal ANY target | `EPERM` |
//!
//! `EPERM` is structurally unreachable in pagh: there is a single uid model
//! (`misc::sys_getid` returns 0 for every process, i.e. the CAP_KILL-equivalent),
//! so every target is permitted. The decision is nevertheless a real function
//! ([`kill_permits`]) that the handler calls, so a future uid/credential model
//! has exactly one place to change instead of a silently missing check.
//!
//! ## Process groups in pagh
//!
//! `misc::sys_getpgid` reports `compat::current_tgid()` and `sys_setpgid` is a
//! documented no-op, so the modeled process group id of a process IS its thread
//! group id. `kill(-pgid)` therefore selects the registered compat threads whose
//! `tgid == pgid` — the same set the current shell/job-control flow produces
//! (`setpgid(0,0)` / `setpgid(child, 0)` both mean "your own group").
//! Moving a process into ANOTHER group is not modeled; if that ever changes,
//! only the group lookup in `signal::resolve_kill_targets` needs updating.
//!
//! ## What counts as "existing"
//!
//! Existence is the compat registry (`compat::compat_exists`). A child that has
//! already exited lives there only as a `wait4` record, so `kill(pid, 0)` on it
//! reports `ESRCH` where Linux would still accept the pid while the zombie
//! exists. Explicitly accepted deviation: `kill(pid, 0)` answers "running and
//! signalable", not "pid slot occupied" (which is the question a shell probing a
//! job actually asks). Native/kernel tasks are never signalable for the same
//! reason — they have no compat state.
#![allow(dead_code)]

use super::errno::Errno;

/// Highest valid signal number (`_NSIG - 1` on Linux x86_64).
pub const NSIG_MAX: i64 = 64;
/// `INT_MIN`: `kill(INT_MIN, sig)` underflows `-pid` in Linux and returns
/// `ESRCH` (`kill_something_info` guards this case explicitly).
pub const INT_MIN: i64 = i32::MIN as i64;
/// `init` (pid 1) is excluded from the `kill(-1, ...)` broadcast (man7 kill(2)
/// NOTES: "the only signals that can be sent to process ID 1 ... are those for
/// which init has explicitly installed signal handlers").
pub const INIT_PID: u64 = 1;

/// `pid_t pid` as the kernel sees it: truncate to 32 bits, then sign-extend.
#[inline]
pub const fn decode_pid(raw: u64) -> i64 {
    (raw as u32) as i32 as i64
}

/// `int sig` as the kernel sees it: truncate to 32 bits, then sign-extend.
#[inline]
pub const fn decode_sig(raw: u64) -> i64 {
    (raw as u32) as i32 as i64
}

/// What `kill(pid, sig)` addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillTarget {
    /// `pid > 0`: exactly this process/thread id.
    Pid(u64),
    /// `pid == 0`: the calling process's own process group.
    OwnGroup,
    /// `pid == -1`: every signalable process except the caller's thread group.
    All,
    /// `pid < -1`: the process group with id `-pid`.
    Group(u64),
}

/// Validate `sig` and classify `pid` (Linux: signal validity first, then the
/// target; `INT_MIN` is an `ESRCH`, see [`INT_MIN`]).
pub fn kill_args(pid: i64, sig: i64) -> Result<(KillTarget, u64), Errno> {
    if !kill_sig_valid(sig) {
        return Err(Errno::EINVAL);
    }
    let target = kill_target(pid)?;
    Ok((target, sig as u64))
}

/// Is this a signal number `kill(2)` accepts? `0` is the existence probe and is
/// valid; everything outside `0..=_NSIG-1` (including negative `int`s) is
/// `EINVAL`.
#[inline]
pub fn kill_sig_valid(sig: i64) -> bool {
    sig >= 0 && sig <= NSIG_MAX
}

/// Classify a `pid_t` already sign-extended to `i64`.
pub fn kill_target(pid: i64) -> Result<KillTarget, Errno> {
    match pid {
        INT_MIN => Err(Errno::ESRCH),
        0 => Ok(KillTarget::OwnGroup),
        -1 => Ok(KillTarget::All),
        p if p > 0 => Ok(KillTarget::Pid(p as u64)),
        p => Ok(KillTarget::Group((-p) as u64)),
    }
}

/// May a process with `sender_uid` signal a target with `target_uid`?
///
/// pagh runs a single root-ish uid (`sys_getid()` returns 0 for every process),
/// which is the kernel's CAP_KILL-equivalent, so every target is permitted and
/// `EPERM` is unreachable. Kept as a function (rather than a comment) so a real
/// credential model lands here.
#[inline]
pub fn kill_permits(sender_uid: u32, target_uid: u32) -> bool {
    sender_uid == 0 || sender_uid == target_uid
}

/// Pick the ONE thread a group-wide signal is queued on.
///
/// Linux queues a group signal on the thread group's shared pending list, so the
/// handler of a multi-threaded process runs ONCE, not once per thread. pagh's
/// pending set is per-thread, so the group path must pick a single thread:
/// the group leader (`tgid`, deterministic and the thread a `kill(-pgid)` caller
/// usually means) when it is still registered, else the lowest live member
/// (`members` arrives in ascending pid order from the registry's `BTreeMap`).
///
/// `None` means the group has no live member → `ESRCH` at the call site.
pub fn pick_group_target(members: &[u64], tgid: u64) -> Option<u64> {
    if members.contains(&tgid) {
        return Some(tgid);
    }
    members.iter().copied().min()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_truncates_to_32_bits_then_sign_extends() {
        assert_eq!(decode_pid(0x1_0000_0005), 5);
        // -5 as an `int` lives in the low 32 bits; the upper half is ignored.
        assert_eq!(decode_pid(0xFFFF_FFFF_FFFF_FFFB), -5);
        assert_eq!(decode_pid(0xFFFF_FFFB), -5);
        assert_eq!(decode_pid(0xFFFF_FFFF_8000_0000), INT_MIN);
    }

    #[test]
    fn sig_decoding_rejects_negative_ints() {
        assert_eq!(decode_sig(0xFFFF_FFFF), -1);
        assert!(!kill_sig_valid(decode_sig(0xFFFF_FFFF)));
        assert!(kill_sig_valid(0));
        assert!(kill_sig_valid(64));
        assert!(!kill_sig_valid(65));
    }

    #[test]
    fn target_classification_matches_linux() {
        assert_eq!(kill_target(7), Ok(KillTarget::Pid(7)));
        assert_eq!(kill_target(0), Ok(KillTarget::OwnGroup));
        assert_eq!(kill_target(-1), Ok(KillTarget::All));
        assert_eq!(kill_target(-7), Ok(KillTarget::Group(7)));
        assert_eq!(kill_target(INT_MIN), Err(Errno::ESRCH));
    }

    #[test]
    fn invalid_sig_wins_over_target_lookup() {
        // Linux validates the signal before resolving the pid, so a bogus pid
        // together with a bogus signal reports EINVAL (not ESRCH).
        assert_eq!(kill_args(0xDEAD, 65), Err(Errno::EINVAL));
        assert_eq!(kill_args(0xDEAD, 0), Ok((KillTarget::Pid(0xDEAD), 0)));
    }

    #[test]
    fn group_target_prefers_leader_then_lowest() {
        assert_eq!(pick_group_target(&[3, 5, 9], 5), Some(5));
        assert_eq!(pick_group_target(&[3, 5, 9], 4), Some(3));
        assert_eq!(pick_group_target(&[], 4), None);
    }

    #[test]
    fn single_uid_permits_everything() {
        assert!(kill_permits(0, 0));
        assert!(kill_permits(0, 1000));
        assert!(!kill_permits(1000, 1001));
        assert!(kill_permits(1000, 1000));
    }
}
