//! Pure controlling-terminal job-control model (Linux tty semantics).
//!
//! WHY THIS EXISTS. The kernel console is pagh's controlling terminal: every
//! `Compat_Process` starts with stdin/stdout/stderr pre-bound to it
//! (`FdTable::with_standard_streams`). A terminal has exactly ONE foreground
//! process group. The group that owns the terminal reads and writes freely; a
//! process in any OTHER group that tries to read gets `SIGTTIN`, and — when the
//! terminal's `TOSTOP` flag is set — one that tries to write gets `SIGTTOU`
//! (POSIX 11.1.4 "Terminal Access Control", `termios(3)`). `tcgetpgrp`/
//! `tcsetpgrp` are `TIOCGPGRP`/`TIOCSPGRP` on that same terminal.
//!
//! This module is the pure decision: [`TtyJobControl`] holds the foreground
//! pgid + the `TOSTOP` bit, [`TtyJobControl::access`] classifies an access, and
//! [`decode_pgrp_arg`]/[`may_set_foreground`] are the `TIOCSPGRP` input rules.
//! It owns no kernel state and no lock, so `host-tests` includes this exact
//! source via `#[path]` (properties in `properties/tty_job.rs`). The kernel-side
//! state lives in `src/drivers/tty.rs` (the `Spinlock`, the shell's own group,
//! and the call into the signal path); `io_sys` applies the verdict to the two
//! ioctls and to the stdin/console read/write paths.
//!
//! DEVIATIONS, stated rather than implied:
//!
//! * pagh has no sessions. `tcsetpgrp`'s "the group must be in the caller's
//!   session" rule is approximated by [`may_set_foreground`] as "the target
//!   group must be live, or be the terminal's current group, or the kernel
//!   shell's own" — anything else is `EPERM`, the errno Linux returns for a
//!   group outside the session.
//! * A background access does not restart the interrupted syscall after
//!   `SIGCONT`: the caller returns `EINTR`, and the stop itself is performed by
//!   the ordinary signal-delivery path at syscall return. Linux restarts the
//!   read/write when the signal's disposition is `SIG_DFL` and returns `EINTR`
//!   only when a handler ran.
//! * `TOSTOP` is tracked and honoured, but pagh defaults it ON (Linux defaults
//!   it off), so a background job's first console write stops it without the
//!   program having asked. `tcsetattr` clears/sets it like any other flag, and
//!   `tcgetattr` reports the real state.

use super::errno::Errno;
use super::signal_frame::{SIGTTIN, SIGTTOU};

/// `TIOCGPGRP`: read the terminal's foreground process group (`tcgetpgrp`).
pub const TIOCGPGRP: u64 = 0x540F;
/// `TIOCSPGRP`: set the terminal's foreground process group (`tcsetpgrp`).
pub const TIOCSPGRP: u64 = 0x5410;
/// `TOSTOP`, the `c_lflag` bit that makes background writes generate `SIGTTOU`
/// (`termios(3)`; Linux value, same bit position as every other `c_lflag`).
pub const TOSTOP: u32 = 0x0000_0100;

/// Which terminal operation is being attempted by a compat process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TtyAccess {
    /// A read from the controlling terminal (stdin / console).
    Read,
    /// A write to the controlling terminal (stdout / stderr).
    Write,
}

/// What the terminal says about an access by a process in group `caller`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TtyVerdict {
    /// The caller owns the terminal: the operation proceeds.
    Allow,
    /// The caller is in a background process group: queue this stop-class
    /// signal on that group and fail the call (`EINTR`).
    Background(u64),
}

/// The terminal's foreground process group + the `TOSTOP` write policy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TtyJobControl {
    fg_pgid: u64,
    tostop: bool,
}

impl TtyJobControl {
    /// A terminal owned by `fg_pgid`, stopping background writes iff `tostop`.
    pub const fn new(fg_pgid: u64, tostop: bool) -> Self {
        Self { fg_pgid, tostop }
    }

    /// The process group that currently owns the terminal (`tcgetpgrp`).
    pub const fn foreground(&self) -> u64 {
        self.fg_pgid
    }

    /// Hand the terminal to `pgid` (`tcsetpgrp`). Input validation is the
    /// caller's job — see [`decode_pgrp_arg`] and [`may_set_foreground`].
    pub fn set_foreground(&mut self, pgid: u64) {
        self.fg_pgid = pgid;
    }

    /// Whether background writes generate `SIGTTOU` (`TOSTOP`).
    pub const fn tostop(&self) -> bool {
        self.tostop
    }

    /// Apply a `TCSETS` `c_lflag` image's `TOSTOP` bit.
    pub fn set_tostop(&mut self, on: bool) {
        self.tostop = on;
    }

    /// `true` when `pgid` owns the terminal.
    pub const fn is_foreground(&self, pgid: u64) -> bool {
        self.fg_pgid == pgid
    }

    /// Classify an access from process group `caller`.
    ///
    /// A read from a background group always stops it (`SIGTTIN`, any terminal
    /// configuration). A write stops it (`SIGTTOU`) only under `TOSTOP`;
    /// otherwise background output is delivered normally.
    pub const fn access(&self, caller: u64, kind: TtyAccess) -> TtyVerdict {
        if self.fg_pgid == caller {
            return TtyVerdict::Allow;
        }
        match kind {
            TtyAccess::Read => TtyVerdict::Background(SIGTTIN),
            TtyAccess::Write => {
                if self.tostop {
                    TtyVerdict::Background(SIGTTOU)
                } else {
                    TtyVerdict::Allow
                }
            }
        }
    }
}

/// The two ioctl requests that address the foreground group.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PgrpRequest {
    /// `TIOCGPGRP` — read it (`tcgetpgrp`).
    Get,
    /// `TIOCSPGRP` — set it (`tcsetpgrp`).
    Set,
}

/// Classify `request`: `Some(Get)`/`Some(Set)` for the two foreground-group
/// ioctls, `None` for anything else (the caller's `ENOTTY` case).
pub const fn pgrp_request(request: u64) -> Option<PgrpRequest> {
    if request == TIOCGPGRP {
        Some(PgrpRequest::Get)
    } else if request == TIOCSPGRP {
        Some(PgrpRequest::Set)
    } else {
        None
    }
}

/// Decode the `int` name read from the `TIOCSPGRP` user argument.
///
/// The word is a C `int`, so the high bit is a sign bit: a negative pgid is
/// `EINVAL` (Linux's `tiocspgrp` rejects `pgrp < 0` before any lookup). Zero
/// decodes fine but can never name a group; [`may_set_foreground`] refuses it.
pub const fn decode_pgrp_arg(raw: u32) -> Result<u64, Errno> {
    if (raw as i32) < 0 {
        Err(Errno::EINVAL)
    } else {
        Ok(raw as u64)
    }
}

/// May `target` become the terminal's foreground group?
///
/// * `live` — at least one registered process has `target` as its pgid.
/// * `shell_pgid` — the kernel shell's own group, the terminal's default owner.
/// * `current_fg` — the group the terminal is on right now (idempotent set).
///
/// `Ok(())` for a live group, the shell's group, or the current group; `EPERM`
/// otherwise (pagh's approximation of Linux's session check) and for the
/// meaningless group 0.
pub fn may_set_foreground(
    target: u64,
    live: bool,
    shell_pgid: u64,
    current_fg: u64,
) -> Result<(), Errno> {
    if target == 0 {
        return Err(Errno::EPERM);
    }
    if live || target == shell_pgid || target == current_fg {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_is_never_signalled() {
        let tty = TtyJobControl::new(7, true);
        assert_eq!(tty.access(7, TtyAccess::Read), TtyVerdict::Allow);
        assert_eq!(tty.access(7, TtyAccess::Write), TtyVerdict::Allow);
        assert_eq!(
            tty.access(8, TtyAccess::Read),
            TtyVerdict::Background(SIGTTIN)
        );
        assert_eq!(
            tty.access(8, TtyAccess::Write),
            TtyVerdict::Background(SIGTTOU)
        );
    }

    #[test]
    fn tostop_gates_only_background_writes() {
        let tty = TtyJobControl::new(3, false);
        assert_eq!(tty.access(9, TtyAccess::Write), TtyVerdict::Allow);
        assert_eq!(
            tty.access(9, TtyAccess::Read),
            TtyVerdict::Background(SIGTTIN)
        );
    }

    #[test]
    fn negative_pgrp_is_einval_and_zero_is_not_a_group() {
        assert_eq!(decode_pgrp_arg(0xFFFF_FFFF), Err(Errno::EINVAL));
        assert_eq!(decode_pgrp_arg(0x8000_0000), Err(Errno::EINVAL));
        assert_eq!(decode_pgrp_arg(0), Ok(0));
        assert_eq!(decode_pgrp_arg(4), Ok(4));
        assert_eq!(may_set_foreground(0, true, 1, 2), Err(Errno::EPERM));
        assert_eq!(may_set_foreground(5, true, 1, 2), Ok(()));
        assert_eq!(may_set_foreground(1, false, 1, 2), Ok(()));
        assert_eq!(may_set_foreground(2, false, 1, 2), Ok(()));
        assert_eq!(may_set_foreground(5, false, 1, 2), Err(Errno::EPERM));
    }

    #[test]
    fn request_constants_are_the_linux_ones() {
        assert_eq!(TIOCGPGRP, 0x540F);
        assert_eq!(TIOCSPGRP, 0x5410);
        assert_eq!(pgrp_request(TIOCGPGRP), Some(PgrpRequest::Get));
        assert_eq!(pgrp_request(TIOCSPGRP), Some(PgrpRequest::Set));
        assert_eq!(pgrp_request(0x5401), None);
        assert_eq!(pgrp_request(0x5413), None);
    }
}
