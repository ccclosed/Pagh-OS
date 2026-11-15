//! Root-port reset policy for the xHCI driver.
//!
//! WHY A SEPARATE, PURE MODULE: the field bug is a timing decision, not an MMIO
//! one — "reset this port now" versus "leave the PHY alone and look again" —
//! and that decision cannot be observed on QEMU (its ports enable instantly).
//! Keeping it here means the exact policy the kernel runs is driven on the host
//! against simulated ports: a port that stays in Polling, a port that enables
//! for 50 ms and falls back, a port that needs 700 ms of debounce, many failed
//! attempts (backoff/power-cycle escalation). The driver in `xhci.rs` is then a
//! thin executor: read PORTSC, call [`PortPolicy::step`], perform the returned
//! [`Step`] side effect.
//!
//! THE RULE THE OLD DRIVER BROKE. xHCI reports a USB2 port in Polling (7) or
//! RxDetect (5) while the controller is still detecting/debouncing/training a
//! device. Writing PR in that window restarts the attach sequence: PED appears
//! for a moment, the link returns to Polling, and the old loop reset again
//! 124 ms later (the field photo: `try=19 ms=124`). Linux's hub waits out the
//! connect debounce and only then resets; after a reset it gives the port up to
//! 500 ms for PRC and PED+U0, requires the enable to stay stable, and backs off
//! between attempts. This module encodes exactly those decisions:
//!
//! * Polling/RxDetect is a WAIT state, not a reset request: a port gets
//!   [`DETECT_WAIT_US`] of uninterrupted training before a reset is considered,
//!   and an enable that happens during the wait is adopted without any reset;
//! * after a reset, PRC followed by PED+U0 is awaited for [`RESET_WAIT_US`]
//!   without issuing another reset, whatever the link does in between;
//! * the enable must hold for [`STABLE_US`] before the device is addressed;
//! * a lost PED does not trigger a reset: the port goes into [`Phase::Backoff`]
//!   (250 ms → 1 s) and is then observed again first;
//! * a PP power-cycle is a last resort: only after [`POWER_CYCLE_AFTER`] failed
//!   attempts, with a 500 ms power-off and a fresh debounce afterwards;
//! * the state machine never blocks: `step` does one bounded decision and
//!   returns, so the boot menu (which calls the driver synchronously from its
//!   countdown) keeps working between steps.

// ─── Link-state constants the policy talks about ─────────────────────────────
//
// These mirror the xHCI PORTSC field encodings (xHCI 1.2 table 5-27). They live
// here so the host tests describe ports in the same terms as the driver; the
// driver's MMIO masks are separate and are applied before the observation is
// passed in.

/// Link state U0: enabled and active.
pub const PLS_U0: u8 = 0;
/// Link state U3: suspended with the device still enabled.
pub const PLS_U3: u8 = 3;
/// Link state RxDetect: USB3 receiver detection. Part of the module's public
/// vocabulary (decoders and host tests); the kernel's step function matches it
/// through `PortObservation`.
#[allow(dead_code)]
pub const PLS_RXDETECT: u8 = 5;
/// Link state Polling: USB2 detection/training; PED is not valid yet.
#[allow(dead_code)]
pub const PLS_POLLING: u8 = 7;

// ─── Timing policy (microseconds unless named) ───────────────────────────────

/// Time a detecting port gets before a reset is allowed. Linux debounces a
/// connect for ~100 ms and then resets; a PCH USB2 PHY can still be training
/// well after that, and the field loop proved 60 ms is far too little. 400 ms
/// is inside the 100–500 ms band the task asks for: long enough for the slow
/// case, short enough that a dead port does not postpone boot for a second.
pub const DETECT_WAIT_US: u64 = 400_000;

/// Time after PR/WPR to wait for PRC and a PED+U0 enable. xHCI 1.2 §4.19.1
/// gives USB2 a 50 ms reset + 10 ms recovery; slower PCH PHYs need more. During
/// this window the policy NEVER issues another reset: PRC/PED are allowed to
/// come and go, and a lost PED is handled after the window, not inside it.
pub const RESET_WAIT_US: u64 = 500_000;

/// Uninterrupted PED+U0 interval required before enumeration. A single PED read
/// is not an enable: the field log shows PED was seen and then lost, and the
/// old code accepted a 20 ms U0 glimpse. 80 ms is inside the 50–100 ms band.
pub const STABLE_US: u64 = 80_000;

/// Port power settle after raising PP (xHCI 1.2 §4.19.1: 20 ms before CCS is
/// valid). The old driver slept here too; the policy makes the wait visible.
pub const POWER_SETTLE_US: u64 = 20_000;

/// How long PP stays off during a last-resort power cycle.
pub const POWER_CYCLE_OFF_US: u64 = 500_000;

/// How often an empty port is re-examined. A port with nothing attached costs
/// one PORTSC read per interval; no resets, no power cycling.
pub const EMPTY_RETRY_US: u64 = 1_000_000;

/// How long the policy waits for a software resume to reach U0.
pub const RESUME_WAIT_US: u64 = 60_000;

/// Failed attempts before a PP power-cycle is allowed. The old driver toggled
/// PP every 4 attempts, which is exactly the kind of extra disturbance the
/// field link did not need; 16 failures at 250 ms–1 s backoff means the port has
/// had >10 s of quiet retries first.
pub const POWER_CYCLE_AFTER: u16 = 16;

/// Retry delay after a failed attempt: 250 ms, 500 ms, then 1 s (capped).
/// The field driver retried every ~124 ms; that is faster than the link can
/// recover and turns one slow device into a permanent reset loop.
pub const fn backoff_us(failures: u16) -> u64 {
    let ms = match failures {
        0 | 1 => 250,
        2 => 500,
        _ => 1_000,
    };
    ms * 1_000
}

/// One PORTSC read, decoded by the driver.
///
/// `raw` is kept because the diagnostics show the full register value: the
/// field report is a photograph, and `pre`/`post`/`ped_sc` only mean something
/// with all bits (CCS, PED, PLS, speed, PRC) intact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortObservation {
    pub ccs: bool,
    pub ped: bool,
    pub pp: bool,
    pub speed: u8,
    pub pls: u8,
    /// Port Reset Change was set in this read.
    pub prc: bool,
    pub raw: u32,
}

impl PortObservation {
    pub const fn new(
        ccs: bool,
        ped: bool,
        pp: bool,
        speed: u8,
        pls: u8,
        prc: bool,
        raw: u32,
    ) -> Self {
        Self {
            ccs,
            ped,
            pp,
            speed,
            pls,
            prc,
            raw,
        }
    }

    /// A port that is enabled and active: PED, U0 and a valid speed id.
    pub const fn enabled(&self) -> bool {
        self.ccs && self.ped && self.speed != 0 && self.pls == PLS_U0
    }

    /// Suspended but still enabled: resume is required, not a reset.
    pub const fn suspended(&self) -> bool {
        self.ccs && self.ped && self.speed != 0 && self.pls == PLS_U3
    }

    /// Still detecting/debouncing/training the device.
    #[allow(dead_code)] // public vocabulary for host tests/decoders
    pub const fn detecting(&self) -> bool {
        self.ccs && !self.ped && (self.pls == PLS_POLLING || self.pls == PLS_RXDETECT)
    }
}

/// What the driver should do after one [`PortPolicy::step`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    /// Nothing to do this poll. The policy waits for time or the next PORTSC.
    Idle,
    /// Raise PP (port power). The policy then waits [`POWER_SETTLE_US`].
    PowerOn,
    /// Drop PP for a last-resort power cycle (waits [`POWER_CYCLE_OFF_US`]).
    PowerOff,
    /// Clear stale change bits and write PR (USB2) or WPR (USB3).
    Reset { warm: bool },
    /// Port is enabled but suspended: perform a software resume.
    Resume,
    /// Port is enabled and stable: address/enumerate it at `speed`.
    Enumerate { speed: u8 },
    /// The attempt failed; diagnostics should record `reason` and retry after
    /// the policy's backoff (the next steps return [`Step::Idle`] until then).
    Failed(FailReason),
}

/// Why one port attempt ended. [`FailReason::stage`] is the string the boot
/// menu and the shell print, and the serial log names the same value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailReason {
    /// The controller never raised PRC for the reset we asked for.
    NoPrc,
    /// PRC/PED were seen (or the reset completed) but PED+U0 did not hold.
    LostAfterReset,
    /// The device disappeared during the attempt.
    Disconnected,
    /// A software resume did not reach U0.
    ResumeLost,
    /// The port enabled but enumeration (descriptors/configuration) failed.
    EnumFailed,
    /// PP did not come up after the power-on write.
    NoPower,
}

impl FailReason {
    pub const fn stage(self) -> &'static str {
        match self {
            FailReason::NoPrc => "reset-no-prc",
            FailReason::LostAfterReset => "lost-after-reset",
            FailReason::Disconnected => "no-device",
            FailReason::ResumeLost => "resume-lost",
            FailReason::EnumFailed => "enum-failed",
            FailReason::NoPower => "no-power",
        }
    }

    const fn needs_reset_before_retry(self) -> bool {
        // Enumeration failure happens on an ENABLED port: the link is fine but
        // the device/endpoint state is not, so the next attempt starts with a
        // reset. A lost enable is the opposite: observe first, reset only if
        // the link does not come back by itself (see `Phase::Backoff`).
        matches!(self, FailReason::EnumFailed)
    }
}

/// Visible phase, for diagnostics and host tests. (The timing fields are
/// private so the only way to advance the machine is [`PortPolicy::step`].)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    /// Not observed yet (or a fresh device appeared).
    Idle,
    /// Waiting out [`POWER_SETTLE_US`] after PP was raised.
    PowerSettle,
    /// Nothing attached; re-read once per [`EMPTY_RETRY_US`].
    Empty,
    /// Port is detecting/training: wait, do not reset before the deadline.
    Debounce,
    /// PR/WPR issued: wait for PRC and PED+U0 until the deadline.
    ResetWait,
    /// PED+U0 seen: require [`STABLE_US`] of uninterrupted enable.
    Stabilize,
    /// Failed attempt: quiet until the backoff deadline.
    Backoff,
    /// PP is off for a last-resort power cycle.
    PowerCycle,
    /// Software resume in progress.
    ResumeWait,
    /// Port is enabled and the driver is enumerating it (the driver reports
    /// back with [`PortPolicy::enumerated`] or [`PortPolicy::enumeration_failed`]).
    Enumerating,
}

/// The complete reset policy of one root port. `Copy`, no allocation.
#[derive(Clone, Copy)]
pub struct PortPolicy {
    phase: Phase,
    /// Absolute monotonic-microsecond deadline of the current phase.
    deadline_us: u64,
    /// Start of the current stability window / observation.
    since_us: u64,
    /// Start of the current attempt (reset or adoption), for `ms=`.
    attempt_started_us: u64,
    /// PR/WPR commands issued since boot (the menu's `try=`).
    resets: u16,
    /// Failed attempts since boot (drives backoff and the power-cycle gate).
    failures: u16,
    /// Whether the enable being enumerated came from OUR reset (the driver
    /// prints `reset-ok`) or was adopted/resumed (`adopted`).
    attempt_had_reset: bool,
    /// A reset is required before the next enumeration (enumeration failure).
    need_reset: bool,
    /// USB3 port (Supported Protocol major revision 3): reset is WPR.
    usb3: bool,
    // ── last-attempt diagnostics (per port, never shared) ───────────────────
    seen_prc: bool,
    seen_ped: bool,
    /// PED was seen with PLS=U0 and a valid speed (a real enable glimpse).
    seen_ped_u0: bool,
    /// PORTSC at the last PED+U0 sighting; before one is seen, the last
    /// transient PED sighting. `seen_ped_u0` says which value this is.
    ped_sc: u32,
    pre: u32,
    post: u32,
}

impl PortPolicy {
    pub const fn new(usb3: bool) -> Self {
        Self {
            phase: Phase::Idle,
            deadline_us: 0,
            since_us: 0,
            attempt_started_us: 0,
            resets: 0,
            failures: 0,
            attempt_had_reset: false,
            need_reset: false,
            usb3,
            seen_prc: false,
            seen_ped: false,
            seen_ped_u0: false,
            ped_sc: 0,
            pre: 0,
            post: 0,
        }
    }

    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// Reset commands issued since boot (`try=` in the menu).
    pub const fn resets(&self) -> u16 {
        self.resets
    }

    /// Failed attempts since boot (backoff/power-cycle escalation).
    pub const fn failures(&self) -> u16 {
        self.failures
    }

    pub const fn attempt_started_us(&self) -> u64 {
        self.attempt_started_us
    }

    pub const fn attempt_had_reset(&self) -> bool {
        self.attempt_had_reset
    }

    pub const fn seen_prc(&self) -> bool {
        self.seen_prc
    }

    pub const fn seen_ped(&self) -> bool {
        self.seen_ped
    }

    pub const fn seen_ped_u0(&self) -> bool {
        self.seen_ped_u0
    }

    pub const fn ped_sc(&self) -> u32 {
        self.ped_sc
    }

    pub const fn pre(&self) -> u32 {
        self.pre
    }

    pub const fn post(&self) -> u32 {
        self.post
    }

    /// Advance the machine with one PORTSC observation taken at `now_us` (any
    /// monotonic microsecond clock; the driver uses calibrated TSC). Returns the
    /// one side effect the driver must perform; all waiting is done by calling
    /// `step` again later, never by blocking.
    pub fn step(&mut self, obs: PortObservation, now_us: u64) -> Step {
        self.observe(obs);
        match self.phase {
            Phase::Idle => self.begin(obs, now_us),
            Phase::PowerSettle => {
                if now_us < self.deadline_us {
                    Step::Idle
                } else {
                    self.begin(obs, now_us)
                }
            }
            Phase::Empty => {
                if !obs.ccs {
                    if now_us >= self.deadline_us {
                        self.deadline_us = now_us.saturating_add(EMPTY_RETRY_US);
                    }
                    Step::Idle
                } else {
                    // A device appeared while nobody was looking: observe it like
                    // any other first sighting (never reset outright).
                    self.begin(obs, now_us)
                }
            }
            Phase::Debounce => {
                if !obs.ccs {
                    self.phase = Phase::Empty;
                    self.deadline_us = now_us.saturating_add(EMPTY_RETRY_US);
                    return Step::Idle;
                }
                if obs.enabled() {
                    // Self-enable during detect/training: still require the
                    // stability window. The field port produced exactly this
                    // PED+U0 glimpse before falling back, and the old 20 ms
                    // check accepted it as an enable.
                    return self.stabilize(obs, now_us);
                }
                if obs.suspended() {
                    return self.resume(now_us);
                }
                // Polling/RxDetect (or any other not-yet-enabled state): the
                // whole point of this phase is that the timer — not a reset —
                // decides when the wait is over.
                if now_us >= self.deadline_us {
                    if obs.pp {
                        self.start_reset(obs, now_us)
                    } else {
                        self.phase = Phase::PowerSettle;
                        self.deadline_us = now_us.saturating_add(POWER_SETTLE_US);
                        Step::PowerOn
                    }
                } else {
                    Step::Idle
                }
            }
            Phase::ResetWait => {
                if !obs.ccs {
                    return self.fail(obs, now_us, FailReason::Disconnected);
                }
                if obs.enabled() {
                    // Do not address the device yet: a PED glimpse is not an
                    // enable (the field port raises it and loses it again).
                    self.phase = Phase::Stabilize;
                    self.since_us = now_us;
                    self.deadline_us = now_us.saturating_add(STABLE_US);
                    self.post = obs.raw;
                    return Step::Idle;
                }
                if obs.suspended() {
                    return self.resume(now_us);
                }
                if now_us >= self.deadline_us {
                    let reason = if self.seen_prc {
                        FailReason::LostAfterReset
                    } else {
                        FailReason::NoPrc
                    };
                    return self.fail(obs, now_us, reason);
                }
                // Whatever PED/PLS do inside this window: keep waiting. PRC and
                // PED+U0 may legally appear late; resetting here is the bug.
                Step::Idle
            }
            Phase::Stabilize => {
                if obs.enabled() {
                    if now_us.saturating_sub(self.since_us) >= STABLE_US {
                        return self.enumerate(obs, now_us);
                    }
                    Step::Idle
                } else if obs.suspended() {
                    self.resume(now_us)
                } else {
                    // The enable did not hold. Do NOT reset: back off, then
                    // observe again (the port is probably back in Polling).
                    self.fail(obs, now_us, FailReason::LostAfterReset)
                }
            }
            Phase::Backoff => {
                if now_us < self.deadline_us {
                    return Step::Idle;
                }
                if !obs.ccs {
                    self.begin(obs, now_us)
                } else if self.should_power_cycle() {
                    // Last resort, after many quiet failures: cold-start the
                    // port. The following steps power it back on and wait a
                    // fresh debounce — no reset is written while PP is down.
                    self.phase = Phase::PowerCycle;
                    self.deadline_us = now_us.saturating_add(POWER_CYCLE_OFF_US);
                    self.attempt_had_reset = false;
                    Step::PowerOff
                } else {
                    self.begin(obs, now_us)
                }
            }
            Phase::PowerCycle => {
                if now_us < self.deadline_us {
                    Step::Idle
                } else {
                    self.phase = Phase::PowerSettle;
                    self.deadline_us = now_us.saturating_add(POWER_SETTLE_US);
                    Step::PowerOn
                }
            }
            Phase::ResumeWait => {
                if obs.enabled() {
                    // The resume write already waited its 20 ms; the enable
                    // still has to hold the same stability window as any other.
                    self.stabilize(obs, now_us)
                } else if now_us >= self.deadline_us {
                    self.fail(obs, now_us, FailReason::ResumeLost)
                } else {
                    Step::Idle
                }
            }
            Phase::Enumerating => {
                // The driver reports `enumerated`/`enumeration_failed`; until it
                // does, there is nothing sensible to do twice.
                Step::Idle
            }
        }
    }

    /// The driver finished enumerating the port successfully.
    pub fn enumerated(&mut self) {
        self.phase = Phase::Enumerating;
        self.need_reset = false;
    }

    /// Enumeration of an ENABLED port failed (descriptor/configuration path).
    /// The next attempt starts with a reset after the backoff.
    pub fn enumeration_failed(&mut self, obs: PortObservation, now_us: u64) -> Step {
        self.fail(obs, now_us, FailReason::EnumFailed)
    }

    /// The driver could not perform a side effect (PP did not come up, resume
    /// write failed, post-reset re-read lost the port). Records the failure and
    /// schedules the backoff exactly like an internally detected one.
    pub fn fail(&mut self, obs: PortObservation, now_us: u64, reason: FailReason) -> Step {
        self.post = obs.raw;
        self.phase = Phase::Backoff;
        self.failures = self.failures.saturating_add(1);
        self.need_reset = reason.needs_reset_before_retry();
        self.attempt_had_reset = false;
        self.deadline_us = now_us.saturating_add(backoff_us(self.failures));
        Step::Failed(reason)
    }

    // ── internals ───────────────────────────────────────────────────────────

    /// Refresh the per-attempt diagnostic snapshot from this observation.
    fn observe(&mut self, obs: PortObservation) {
        if obs.prc {
            self.seen_prc = true;
        }
        if obs.ped {
            self.seen_ped = true;
            if obs.enabled() {
                self.seen_ped_u0 = true;
                // Latest real-enable snapshot. Per-port by construction: every
                // PortPolicy is one port's own state.
                self.ped_sc = obs.raw;
            } else if !self.seen_ped_u0 {
                // Before any U0 sighting, keep the LAST transient PED snapshot
                // rather than the first: a reset-in-progress PED read is not an
                // enable, and the final value is the useful one in a photo.
                self.ped_sc = obs.raw;
            }
        }
    }

    /// First decision after Idle/Empty/Backoff/PowerSettle: adopt, resume, or
    /// start observing a still-detecting port. NEVER resets a detecting port.
    fn begin(&mut self, obs: PortObservation, now_us: u64) -> Step {
        if !obs.ccs {
            self.phase = Phase::Empty;
            self.deadline_us = now_us.saturating_add(EMPTY_RETRY_US);
            self.attempt_had_reset = false;
            return Step::Idle;
        }
        self.attempt_started_us = now_us;
        if !obs.pp {
            self.phase = Phase::PowerSettle;
            self.deadline_us = now_us.saturating_add(POWER_SETTLE_US);
            self.attempt_had_reset = false;
            return Step::PowerOn;
        }
        if obs.enabled() {
            if self.need_reset {
                return self.start_reset(obs, now_us);
            }
            // Already enabled (firmware left it up, or it recovered during
            // backoff): adopt it only after the same uninterrupted U0 window.
            return self.stabilize(obs, now_us);
        }
        if obs.suspended() {
            self.phase = Phase::ResumeWait;
            self.deadline_us = now_us.saturating_add(RESUME_WAIT_US);
            self.attempt_had_reset = false;
            return Step::Resume;
        }
        // Polling/RxDetect (or an unusable link state): give the controller its
        // detect/debounce/train window. The deadline is the only thing that can
        // turn this into a reset.
        self.phase = Phase::Debounce;
        self.deadline_us = now_us.saturating_add(DETECT_WAIT_US);
        self.attempt_had_reset = false;
        Step::Idle
    }

    fn start_reset(&mut self, obs: PortObservation, now_us: u64) -> Step {
        self.phase = Phase::ResetWait;
        self.deadline_us = now_us.saturating_add(RESET_WAIT_US);
        self.since_us = now_us;
        self.attempt_started_us = now_us;
        self.pre = obs.raw;
        self.seen_prc = false;
        self.seen_ped = false;
        self.seen_ped_u0 = false;
        self.ped_sc = 0;
        self.attempt_had_reset = true;
        self.need_reset = false;
        self.resets = self.resets.saturating_add(1);
        Step::Reset { warm: self.usb3 }
    }

    /// Start the uninterrupted PED+U0 window that must precede enumeration.
    fn stabilize(&mut self, obs: PortObservation, now_us: u64) -> Step {
        if self.attempt_started_us == 0 {
            self.attempt_started_us = now_us;
        }
        self.phase = Phase::Stabilize;
        self.since_us = now_us;
        self.deadline_us = now_us.saturating_add(STABLE_US);
        self.post = obs.raw;
        Step::Idle
    }

    fn enumerate(&mut self, obs: PortObservation, now_us: u64) -> Step {
        if self.attempt_started_us == 0 {
            self.attempt_started_us = now_us;
        }
        self.phase = Phase::Enumerating;
        self.post = obs.raw;
        Step::Enumerate { speed: obs.speed }
    }

    fn resume(&mut self, now_us: u64) -> Step {
        self.phase = Phase::ResumeWait;
        self.deadline_us = now_us.saturating_add(RESUME_WAIT_US);
        Step::Resume
    }

    fn should_power_cycle(&self) -> bool {
        self.failures >= POWER_CYCLE_AFTER && self.failures % POWER_CYCLE_AFTER == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(ccs: bool, ped: bool, pp: bool, speed: u8, pls: u8, prc: bool) -> PortObservation {
        let mut raw = 0u32;
        if ccs {
            raw |= 1;
        }
        if ped {
            raw |= 2;
        }
        if pp {
            raw |= 1 << 9;
        }
        raw |= (speed as u32 & 0xF) << 10;
        raw |= (pls as u32 & 0xF) << 5;
        if prc {
            raw |= 1 << 21;
        }
        PortObservation::new(ccs, ped, pp, speed, pls, prc, raw)
    }

    fn normal(pls: u8) -> PortObservation {
        obs(true, false, true, 0, pls, false)
    }

    #[test]
    fn polling_is_wait_not_reset() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        // Attached port in Polling: the first step starts the debounce window
        // and must not write a reset.
        assert_eq!(p.step(normal(PLS_POLLING), now), Step::Idle);
        assert_eq!(p.phase(), Phase::Debounce);
        // Every poll inside the window is still a wait — no reset at 60 ms, no
        // reset at 100/124 ms (the field loop's cadence).
        for ms in [60, 100, 124, 250, 399] {
            now = ms * 1_000;
            assert_eq!(p.step(normal(PLS_POLLING), now), Step::Idle, "ms={ms}");
        }
        // At the end of the debounce window the port still has not enabled, so
        // a reset is now the correct escalation.
        now = DETECT_WAIT_US;
        assert_eq!(
            p.step(normal(PLS_POLLING), now),
            Step::Reset { warm: false }
        );
        assert_eq!(p.resets(), 1);
        assert_eq!(p.phase(), Phase::ResetWait);
    }

    #[test]
    fn enable_during_debounce_is_adopted_without_reset() {
        let mut p = PortPolicy::new(false);
        assert_eq!(p.step(normal(PLS_POLLING), 0), Step::Idle);
        // The PHY finishes training on its own 120 ms later: adopt it after the
        // same stability window every enable needs.
        let enabled = obs(true, true, true, 3, PLS_U0, false);
        assert_eq!(p.step(enabled, 120_000), Step::Idle);
        assert_eq!(p.phase(), Phase::Stabilize);
        assert_eq!(
            p.step(enabled, 120_000 + STABLE_US),
            Step::Enumerate { speed: 3 }
        );
        assert_eq!(p.resets(), 0, "a self-enabled port must not be reset");
    }

    #[test]
    fn reset_wait_tolerates_a_transient_ped_without_re_resetting() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        assert_eq!(p.step(normal(PLS_POLLING), now), Step::Idle);
        now = DETECT_WAIT_US;
        assert_eq!(
            p.step(normal(PLS_POLLING), now),
            Step::Reset { warm: false }
        );
        // The field sequence: PED+U0+PRC appears immediately, then the link
        // falls back to Polling. The policy must wait out the full window and
        // must never issue a second reset inside it.
        let glimpse = obs(true, true, true, 3, PLS_U0, true);
        assert_eq!(p.step(glimpse, now + 1_000), Step::Idle);
        // The PED glimpse starts the stability window; when the next read shows
        // Polling again the attempt fails — but it never becomes a second reset.
        let mut failed = false;
        for ms in [10, 50, 100, 200] {
            now += ms * 1_000;
            match p.step(normal(PLS_POLLING), now) {
                Step::Idle => {}
                Step::Failed(FailReason::LostAfterReset) => {
                    failed = true;
                    break;
                }
                other => panic!("unexpected step {other:?} inside reset wait"),
            }
            assert_eq!(p.resets(), 1, "no reset inside the reset wait window");
        }
        // If the failure was not recorded inside the loop, the end of the
        // 500 ms window records it instead.
        if !failed {
            now = DETECT_WAIT_US + RESET_WAIT_US;
            assert_eq!(
                p.step(normal(PLS_POLLING), now),
                Step::Failed(FailReason::LostAfterReset)
            );
        }
        assert_eq!(p.phase(), Phase::Backoff);
        assert_eq!(p.resets(), 1);
        assert!(p.seen_prc && p.seen_ped && p.seen_ped_u0);
        assert_ne!(p.ped_sc(), 0, "the PED+U0 snapshot is kept for the log");
    }

    #[test]
    fn lost_ped_waits_the_backoff_then_observes_before_resetting() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        p.step(normal(PLS_POLLING), now);
        now = DETECT_WAIT_US;
        p.step(normal(PLS_POLLING), now);
        // Fail that attempt at the end of its window.
        now += RESET_WAIT_US;
        assert_eq!(
            p.step(normal(PLS_POLLING), now),
            Step::Failed(FailReason::NoPrc)
        );
        // Backoff is 250 ms for the first failure; inside it nothing happens.
        for delta in [1, 50, 100, 249] {
            assert_eq!(p.step(normal(PLS_POLLING), now + delta * 1_000), Step::Idle);
        }
        // At the backoff deadline the policy observes the still-Polling port and
        // gives it a fresh debounce window — it does NOT reset right away.
        now += 250_000;
        assert_eq!(p.step(normal(PLS_POLLING), now), Step::Idle);
        assert_eq!(p.phase(), Phase::Debounce);
        for delta in [100, 250, 399] {
            assert_eq!(p.step(normal(PLS_POLLING), now + delta * 1_000), Step::Idle);
            assert_eq!(p.resets(), 1);
        }
        // Only after the fresh window does the second reset happen: this is the
        // ≥650 ms spacing that stops the 124 ms reset loop.
        assert_eq!(
            p.step(normal(PLS_POLLING), now + DETECT_WAIT_US),
            Step::Reset { warm: false }
        );
        assert_eq!(p.resets(), 2);
    }

    #[test]
    fn enable_must_stay_stable_before_enumeration() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        p.step(normal(PLS_POLLING), now);
        now = DETECT_WAIT_US;
        p.step(normal(PLS_POLLING), now);
        let enabled = obs(true, true, true, 1, PLS_U0, true);
        // Reset completes: only a stability window starts.
        assert_eq!(p.step(enabled, now + 60_000), Step::Idle);
        assert_eq!(p.phase(), Phase::Stabilize);
        // A 50 ms glimpse is not enough (the old driver accepted 20 ms).
        assert_eq!(p.step(enabled, now + 60_000 + 50_000), Step::Idle);
        // PED lost before the window ends: backoff, no enumeration.
        assert_eq!(
            p.step(normal(PLS_POLLING), now + 60_000 + 60_000),
            Step::Failed(FailReason::LostAfterReset)
        );
        // And it never enumerated.
        let mut q = PortPolicy::new(false);
        q.step(normal(PLS_POLLING), 0);
        q.step(normal(PLS_POLLING), DETECT_WAIT_US);
        q.step(enabled, DETECT_WAIT_US + 60_000);
        assert_eq!(
            q.step(enabled, DETECT_WAIT_US + 60_000 + STABLE_US),
            Step::Enumerate { speed: 1 }
        );
    }

    #[test]
    fn ped_snapshot_is_per_policy_and_records_the_last_real_enable() {
        let mut hs = PortPolicy::new(false);
        let mut fs = PortPolicy::new(false);
        // Both ports see the same reset-time transient (this is what the field
        // photo shows on every port), then enable at their own speed.
        let transient = obs(true, true, true, 3, PLS_U0, true);
        let hs_enabled = obs(true, true, true, 3, PLS_U0, true);
        let fs_enabled = obs(true, true, true, 1, PLS_U0, true);
        hs.observe(transient);
        fs.observe(transient);
        hs.observe(hs_enabled);
        fs.observe(fs_enabled);
        assert_ne!(hs.ped_sc(), fs.ped_sc(), "per-port speed must be visible");
        assert!(hs.seen_ped_u0() && fs.seen_ped_u0());
        assert_eq!(hs.ped_sc() & 0xF << 10, 3 << 10);
        assert_eq!(fs.ped_sc() & 0xF << 10, 1 << 10);
    }

    #[test]
    fn transient_only_ped_is_flagged_as_not_u0() {
        let mut p = PortPolicy::new(false);
        p.observe(obs(true, true, true, 3, PLS_POLLING, false));
        assert!(p.seen_ped());
        assert!(!p.seen_ped_u0(), "PED without U0 is not an enable");
        // The last transient snapshot is kept, so a photo shows the latest
        // state rather than the first one.
        p.observe(obs(true, true, true, 1, PLS_POLLING, false));
        assert_eq!(p.ped_sc() & 0xF << 10, 1 << 10);
    }

    #[test]
    fn backoff_grows_to_one_second_and_power_cycles_only_late() {
        assert_eq!(backoff_us(0), 250_000);
        assert_eq!(backoff_us(1), 250_000);
        assert_eq!(backoff_us(2), 500_000);
        assert_eq!(backoff_us(3), 1_000_000);
        assert_eq!(backoff_us(u16::MAX), 1_000_000);

        // Drive a permanent-loss port through enough attempts: no PP toggling
        // before POWER_CYCLE_AFTER failures, and a PowerOff at exactly 16.
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let mut resets = 0u16;
        let mut power_offs = 0u16;
        let mut resets_at_first_power_off: Option<u16> = None;
        for _ in 0..60 {
            // Fast-forward until the policy wants to do something.
            let mut guard = 0u32;
            loop {
                let step = p.step(normal(PLS_POLLING), now);
                match step {
                    Step::Idle => {
                        now += 10_000;
                        guard += 1;
                        assert!(guard < 10_000, "policy stuck in Idle");
                    }
                    Step::Reset { .. } => {
                        resets += 1;
                        break;
                    }
                    Step::PowerOff => {
                        if resets_at_first_power_off.is_none() {
                            resets_at_first_power_off = Some(resets);
                        }
                        power_offs += 1;
                        break;
                    }
                    // The driver performs these side effects; the policy only
                    // needs the observation sequence, so keep time moving.
                    Step::PowerOn | Step::Resume => break,
                    Step::Enumerate { .. } => break,
                    Step::Failed(_) => break,
                }
            }
        }
        assert!(
            power_offs >= 1,
            "a long-dead port must be power-cycled once"
        );
        assert!(
            power_offs <= 4,
            "power cycling is a last resort, not a loop"
        );
        // The first power cycle happens only after the 16th failed attempt.
        let at = resets_at_first_power_off.expect("first power-off recorded");
        assert!(
            at >= POWER_CYCLE_AFTER,
            "power-cycled after only {at} reset(s)"
        );
    }

    #[test]
    fn empty_port_is_never_reset_or_power_cycled() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let empty = obs(false, false, true, 0, PLS_RXDETECT, false);
        for _ in 0..100 {
            assert_eq!(p.step(empty, now), Step::Idle);
            now += 100_000;
        }
        assert_eq!(p.resets(), 0);
        assert_eq!(p.failures(), 0);
        assert_eq!(p.phase(), Phase::Empty);
    }

    #[test]
    fn usb3_ports_use_warm_reset() {
        let mut p = PortPolicy::new(true);
        p.step(normal(PLS_POLLING), 0);
        assert_eq!(
            p.step(normal(PLS_POLLING), DETECT_WAIT_US),
            Step::Reset { warm: true }
        );
    }

    #[test]
    fn resume_is_requested_for_suspended_enabled_ports() {
        let mut p = PortPolicy::new(false);
        let suspended = obs(true, true, true, 3, PLS_U3, false);
        assert_eq!(p.step(suspended, 0), Step::Resume);
        assert_eq!(p.phase(), Phase::ResumeWait);
        // The driver's resume brings the link to U0: stability, then enumerate.
        let enabled = obs(true, true, true, 3, PLS_U0, false);
        assert_eq!(p.step(enabled, 20_000), Step::Idle);
        assert_eq!(
            p.step(enabled, 20_000 + STABLE_US),
            Step::Enumerate { speed: 3 }
        );
        assert_eq!(p.resets(), 0);
    }

    #[test]
    fn enumeration_failure_resets_after_backoff_not_immediately() {
        let mut p = PortPolicy::new(false);
        let enabled = obs(true, true, true, 3, PLS_U0, false);
        assert_eq!(p.step(enabled, 0), Step::Idle);
        assert_eq!(p.step(enabled, STABLE_US), Step::Enumerate { speed: 3 });
        assert_eq!(
            p.enumeration_failed(enabled, 5_000),
            Step::Failed(FailReason::EnumFailed)
        );
        // During the 250 ms backoff the still-enabled port must not be touched.
        assert_eq!(p.step(enabled, 100_000), Step::Idle);
        // At the deadline the policy resets before enumerating again.
        assert_eq!(
            p.step(enabled, 5_000 + 250_000),
            Step::Reset { warm: false }
        );
    }

    #[test]
    fn simulation_enable_after_700ms_debounce_is_adopted_without_reset() {
        // The user's field symptom: a USB2 port that needs a long debounce.
        // Model a port that is in Polling for `enable_after` and then enables on
        // its own. Drive the policy at 10 ms steps and record resets.
        for enable_after in [50_000u64, 200_000, 350_000] {
            let mut p = PortPolicy::new(false);
            let mut now = 0u64;
            let mut resets = 0;
            let mut enumerated = false;
            for _ in 0..2_000 {
                let enabled = now >= enable_after;
                let o = if enabled {
                    obs(true, true, true, 3, PLS_U0, true)
                } else {
                    normal(PLS_POLLING)
                };
                match p.step(o, now) {
                    Step::Reset { .. } => resets += 1,
                    Step::Enumerate { .. } => {
                        enumerated = true;
                        break;
                    }
                    Step::Failed(_) => {}
                    _ => {}
                }
                now += 10_000;
            }
            assert!(enumerated, "enable_after={enable_after}");
            assert_eq!(resets, 0, "a self-enabling port must never be reset");
        }
    }

    #[test]
    fn simulation_lost_after_reset_never_resets_faster_than_backoff() {
        // A port that always shows PED for one read after a reset and then
        // falls back to Polling. Count reset timestamps and assert the spacing
        // is at least backoff + debounce (≥650 ms), not the field's 124 ms.
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let mut reset_times: Vec<u64> = Vec::new();
        let mut ped_glimpse_until = 0u64;
        for _ in 0..20_000 {
            // 2 s of simulated time is enough for the first few attempts.
            let o = if now < ped_glimpse_until {
                obs(true, true, true, 3, PLS_U0, true)
            } else {
                normal(PLS_POLLING)
            };
            match p.step(o, now) {
                Step::Reset { .. } => {
                    reset_times.push(now);
                    ped_glimpse_until = now + 10_000;
                }
                _ => {}
            }
            now += 1_000;
            if now > 4_000_000 {
                break;
            }
        }
        assert!(
            reset_times.len() >= 2,
            "the port keeps failing, retries happen"
        );
        for pair in reset_times.windows(2) {
            let spacing = pair[1] - pair[0];
            // Minimum: 250 ms backoff + 400 ms fresh debounce. A PED glimpse
            // fails before the 500 ms reset window ends, so 650 ms is the true
            // floor — still 5x the field driver's 124 ms.
            assert!(
                spacing >= 650_000,
                "resets {pair:?} only {spacing} us apart"
            );
        }
    }
}
