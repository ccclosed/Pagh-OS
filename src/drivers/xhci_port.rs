//! Root-port bring-up/reset policy for the xHCI driver, shaped like Linux's
//! `hub.c`/`xhci-hub.c`.
//!
//! WHY A SEPARATE, PURE MODULE: the field bug is a timing and write-ordering
//! decision, not an MMIO one, and QEMU enables its ports instantly, so no boot
//! test can observe it. Keeping the decision here lets the host tests drive the
//! exact state machine the kernel runs against simulated PCH ports (a link that
//! keeps dropping to Polling, a connect that bounces, a reset the controller
//! never completes, five failed resets in a row).
//!
//! LESSONS FROM THE TWO FIELD ROUNDS (Intel 8086:7a60, 26 ports):
//!
//! 1. The first iteration stopped the ~124 ms reset storm but still lost the
//!    enable. Its PORTSC writes took the raw read value, masked only the
//!    write-1-to-clear *change* bits and wrote the result back. That write still
//!    carried `PED` (bit 1), which is itself write-1-to-clear ("write 1 to
//!    disable the port"): clearing PRC/CSC on the just-enabled port therefore
//!    DISABLED it, and the following read reported the field's `post=0x6e1`
//!    (Polling, PED=0). It also carried `PR`/`WPR`, so a reset write could
//!    re-trigger a reset. Linux never does this: every PORTSC write goes through
//!    `xhci_port_state_to_neutral()`, which keeps only read-only bits (CCS,
//!    speed) and read/write bits whose 0 means "leave alone" (PLS, PP), and then
//!    ORs in the one bit it wants. [`port_state_to_neutral`] is that function.
//! 2. The reset/change-bit order must be Linux's: write PR (USB2) or WPR (USB3)
//!    on a neutral value, wait for the port to leave reset with a connection,
//!    check PED, and only then clear C_PORT_RESET (PRC). Never clear PRC/CSC in
//!    the same write that issues a reset.
//! 3. The timing numbers are Linux's, not invented: `HUB_DEBOUNCE_STEP` 25 ms,
//!    `HUB_DEBOUNCE_STABLE` 100 ms of uninterrupted connection before the first
//!    reset, `HUB_DEBOUNCE_TIMEOUT` 2000 ms total, `PORT_RESET_TRIES` 5,
//!    `HUB_SHORT_RESET_TIME` 10 ms then `HUB_LONG_RESET_TIME` 200 ms per wait,
//!    `HUB_RESET_TIMEOUT` 800 ms per reset, and a 100 ms power settle after PP.
//!    The 2 s is a *maximum* debounce budget, not a quiet period: Linux resets a
//!    connected port after 100 ms of a stable connect, and so does this policy.
//! 4. Linux never power-cycles a root port for an enable failure. The old
//!    `PowerOff`/`PowerCycle` escalation is gone: a port is only ever powered
//!    ON here, and a port that cannot be powered on is retried after the
//!    backoff rather than left unpowered.
//! 5. After a failed enable there is a ≥1 s pause, then a fresh debounce, so a
//!    broken port produces a deliberate retry every few seconds, not the field's
//!    reset every 124 ms.
//!
//! The driver in `xhci.rs` is a thin executor: read PORTSC, decode it into
//! [`PortObservation`], call [`PortPolicy::step`], perform the returned
//! [`Step`]. All waiting is a deadline compared on later polls, so the boot
//! menu's countdown never blocks on a USB2 PHY.

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
/// Link state RxDetect: USB3 receiver detection.
#[allow(dead_code)]
pub const PLS_RXDETECT: u8 = 5;
/// Link state Polling: USB2 detection/training; PED is not valid yet.
#[allow(dead_code)]
pub const PLS_POLLING: u8 = 7;

// ─── Timing policy (microseconds unless named) ───────────────────────────────
//
// Every constant below is Linux's, with the file/constant it comes from named,
// because "the field port does not enable" is not a reason to invent a number.

/// `hub.c` HUB_DEBOUNCE_STEP: debounce samples are 25 ms apart.
pub const HUB_DEBOUNCE_STEP_US: u64 = 25_000;
/// `hub.c` HUB_DEBOUNCE_STABLE: the connection must be unchanged for this long
/// before the port counts as connected. USB 2.0 §7.1.7.1 asks for ≥100 ms.
pub const HUB_DEBOUNCE_STABLE_US: u64 = 100_000;
/// `hub.c` HUB_DEBOUNCE_TIMEOUT: if the connection never stays stable this long,
/// the debounce gives up (a bouncing/flapping port, not a device).
pub const HUB_DEBOUNCE_TIMEOUT_US: u64 = 2_000_000;
/// `hub.c` HUB_SHORT_RESET_TIME: first two reset-status polls are 10 ms apart.
pub const HUB_SHORT_RESET_TIME_US: u64 = 10_000;
/// `hub.c` HUB_LONG_RESET_TIME: after two short polls, poll every 200 ms.
pub const HUB_LONG_RESET_TIME_US: u64 = 200_000;
/// `hub.c` HUB_RESET_TIMEOUT: one reset attempt is abandoned after this long.
pub const HUB_RESET_TIMEOUT_US: u64 = 800_000;
/// `hub.c` PORT_RESET_TRIES: resets written for one bring-up before giving up.
pub const PORT_RESET_TRIES: u8 = 5;

/// Power settle after PP is raised. USB 2.0 §7.1.7.1 requires 100 ms between
/// power-on and the first reset; Linux's root hub additionally debounces the
/// connection for `HUB_DEBOUNCE_STABLE` before resetting. This policy keeps the
/// 100 ms power settle and then the 100 ms stable-connect debounce, i.e. the
/// conservative sum of both requirements (the old 20 ms was only the xHCI
/// descriptor's `bPwrOn2PwrGood` value and skipped debounce entirely).
pub const POWER_SETTLE_US: u64 = 100_000;

/// Uninterrupted PED+U0 interval required before enumeration. A single PED read
/// is not an enable: the field log shows PED+U0 was seen and then lost. Linux
/// only checks PED once; this driver additionally proves the enable held so a
/// reset-time glimpse cannot be enumerated (and, with the neutral-write fix, is
/// not disabled by our own change-bit clear either).
pub const STABLE_US: u64 = 200_000;

/// How often an empty port is re-examined. A port with nothing attached costs
/// one PORTSC read per interval; no resets, no power writes.
pub const EMPTY_RETRY_US: u64 = 1_000_000;

/// How long the policy waits for a software resume to reach U0.
pub const RESUME_WAIT_US: u64 = 60_000;

/// Pause after a failed attempt (lost PED, reset timeout, enumeration failure).
/// Linux relies on the hub workqueue's retry pacing; a cooperative polled driver
/// needs an explicit quiet period so a dead port cannot become a reset loop.
/// ≥1 s is also what the second field round asked for after a lost PED.
pub const RETRY_BACKOFF_US: u64 = 1_000_000;

/// Retry delay after a failed attempt. Kept as a function because callers log it
/// and the host tests pin the floor; it is deliberately flat, not "faster at
/// first": every retry now costs a full debounce plus up to five resets anyway.
pub const fn backoff_us(_failures: u16) -> u64 {
    RETRY_BACKOFF_US
}

// ─── PORTSC write helper (the bug from the second field round) ───────────────

/// PORTSC bits that are read-only status or read/write-with-0-meaning-neutral
/// (Linux `XHCI_PORT_RO` / `XHCI_PORT_RWS` in `xhci-hub.c`):
///
/// * RO: CCS (0), bit 3, speed id (10-13), bit 30;
/// * RWS: PLS (5-8), PP (9), bits 14-15, bits 25-27.
///
/// Everything else is write-1-to-set / write-1-to-clear / reserved-zero and
/// must be written as 0. In particular **PED (bit 1) is write-1-to-disable**:
/// preserving a read PED value in a PORTSC write disabled the port that the
/// field log showed enabled for a moment. PR (bit 4), WPR (31), LWS (16) and the
/// change bits (17-23) are also cleared by this mask.
pub fn port_state_to_neutral(raw: u32) -> u32 {
    const RO: u32 = (1 << 0) | (1 << 3) | (0xf << 10) | (1 << 30);
    const RWS: u32 = (0xf << 5) | (1 << 9) | (0x3 << 14) | (0x7 << 25);
    (raw & RO) | (raw & RWS)
}

/// One PORTSC read, decoded by the driver.
///
/// `raw` is kept because the diagnostics show the full register value: the
/// field report is a photograph, and `pre`/`post`/`ped_sc` only mean something
/// with all bits (CCS, PED, PLS, speed, PR, PRC, CSC) intact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortObservation {
    pub ccs: bool,
    pub ped: bool,
    pub pp: bool,
    pub speed: u8,
    pub pls: u8,
    /// Port Reset Change was set in this read (sticky, write-1-to-clear).
    pub prc: bool,
    /// Connect Status Change was set in this read (sticky, write-1-to-clear).
    pub csc: bool,
    /// Port Reset (bit 4): the controller is still performing a reset.
    pub pr: bool,
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
        csc: bool,
        pr: bool,
        raw: u32,
    ) -> Self {
        Self {
            ccs,
            ped,
            pp,
            speed,
            pls,
            prc,
            csc,
            pr,
            raw,
        }
    }

    /// A port that is enabled and active: PED, U0, a valid speed id, and not
    /// still inside a reset.
    pub const fn enabled(&self) -> bool {
        self.ccs && self.ped && self.speed != 0 && self.pls == PLS_U0 && !self.pr
    }

    /// Suspended but still enabled: resume is required, not a reset.
    pub const fn suspended(&self) -> bool {
        self.ccs && self.ped && self.speed != 0 && self.pls == PLS_U3 && !self.pr
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
    /// Raise PP (port power) and read it back. The policy then waits
    /// [`POWER_SETTLE_US`] before debouncing the connection.
    PowerOn,
    /// Write PR (USB2) or WPR (USB3) on a neutral PORTSC value. The policy then
    /// waits for the reset to complete (PR clear + CCS) and PED, up to
    /// [`HUB_RESET_TIMEOUT_US`] per write and [`PORT_RESET_TRIES`] writes.
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
    /// The reset never left the port enabled within its 800 ms window(s).
    ResetTimeout,
    /// Reset completed, but PED+U0 did not hold for [`STABLE_US`].
    LostAfterReset,
    /// The device disappeared during the attempt.
    Disconnected,
    /// The connection never stayed stable for [`HUB_DEBOUNCE_STABLE_US`].
    DebounceTimeout,
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
            FailReason::ResetTimeout => "reset-timeout",
            FailReason::LostAfterReset => "lost-after-reset",
            FailReason::Disconnected => "no-device",
            FailReason::DebounceTimeout => "debounce-timeout",
            FailReason::ResumeLost => "resume-lost",
            FailReason::EnumFailed => "enum-failed",
            FailReason::NoPower => "no-power",
        }
    }

    const fn needs_reset_before_retry(self) -> bool {
        // Enumeration failure happens on an ENABLED port: the link is fine but
        // the device/endpoint state is not, so the next attempt starts with a
        // reset. A lost enable is the opposite: debounce and reset again from
        // scratch (Linux's hub workqueue does the same after a failed reset).
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
    /// Sampling CCS every [`HUB_DEBOUNCE_STEP_US`] until the connection has
    /// been unchanged for [`HUB_DEBOUNCE_STABLE_US`] (Linux hub_port_debounce).
    Debounce,
    /// PR/WPR issued: wait for PR clear + CCS, then PED, up to
    /// [`HUB_RESET_TIMEOUT_US`] per write (Linux hub_port_wait_reset).
    ResetWait,
    /// PED+U0 seen: require [`STABLE_US`] of uninterrupted enable.
    Stabilize,
    /// Failed attempt: quiet until the backoff deadline.
    Backoff,
    /// Software resume in progress.
    ResumeWait,
    /// Port is enabled and the driver is enumerating it (the driver reports
    /// back with [`PortPolicy::enumerated`] or [`PortPolicy::enumeration_failed`]).
    Enumerating,
}

/// The complete bring-up policy of one root port. `Copy`, no allocation.
#[derive(Clone, Copy)]
pub struct PortPolicy {
    phase: Phase,
    /// Absolute monotonic-microsecond deadline of the current phase.
    deadline_us: u64,
    /// Start of the current stability window / observation.
    since_us: u64,
    /// Start of the current attempt (reset sequence or adoption), for `ms=`.
    attempt_started_us: u64,
    /// PR/WPR commands issued since boot (the menu's `try=`).
    resets: u16,
    /// Failed attempts since boot (drives the backoff / diagnostics).
    failures: u16,
    /// Whether the enable being enumerated came from OUR reset (the driver
    /// prints `reset-ok`) or was adopted/resumed (`adopted`).
    attempt_had_reset: bool,
    /// A reset is required before the next enumeration (enumeration failure).
    need_reset: bool,
    /// USB3 port (Supported Protocol major revision 3): reset is WPR.
    usb3: bool,
    // ── hub_port_debounce state ─────────────────────────────────────────────
    /// Start of the current debounce window (real-time timeout base).
    debounce_started_us: u64,
    /// When the next 25 ms debounce sample is due.
    debounce_next_us: u64,
    /// Total debounce time elapsed (Linux `total_time`), for the log.
    debounce_total_us: u64,
    /// Unchanged-connection time accumulated in 25 ms steps (Linux
    /// `stable_time`), for the log and the decision.
    debounce_stable_us: u64,
    /// Whether a previous sample has established the sampled connection state.
    debounce_have_conn: bool,
    /// The connection state the stable timer is counting.
    debounce_conn: bool,
    /// CSC as it was at the previous sample, so a *new* connect change resets
    /// the stable timer even though the sticky bit is not cleared before reset.
    debounce_last_csc: bool,
    // ── hub_port_wait_reset state ───────────────────────────────────────────
    /// Reset writes in the current attempt (1..=[`PORT_RESET_TRIES`]).
    reset_tries: u8,
    /// When the current reset write happened (poll schedule base).
    reset_wait_started_us: u64,
    /// When the next reset-status poll is due.
    reset_wait_next_us: u64,
    /// Current poll spacing: 10 ms for the first two, then 200 ms.
    reset_wait_delay_us: u64,
    /// Reset-status polls taken for the current write (Linux `delay_time`).
    reset_wait_samples: u8,
    // ── last-attempt diagnostics (per port, never shared) ───────────────────
    seen_prc: bool,
    seen_csc: bool,
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
            debounce_started_us: 0,
            debounce_next_us: 0,
            debounce_total_us: 0,
            debounce_stable_us: 0,
            debounce_have_conn: false,
            debounce_conn: false,
            debounce_last_csc: false,
            reset_tries: 0,
            reset_wait_started_us: 0,
            reset_wait_next_us: 0,
            reset_wait_delay_us: HUB_SHORT_RESET_TIME_US,
            reset_wait_samples: 0,
            seen_prc: false,
            seen_csc: false,
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

    /// Reset writes in the CURRENT attempt (Linux `PORT_RESET_TRIES` walk).
    pub const fn reset_tries(&self) -> u8 {
        self.reset_tries
    }

    /// Failed attempts since boot.
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

    pub const fn seen_csc(&self) -> bool {
        self.seen_csc
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

    /// Debounce progress (`total_time` in Linux's log line), microseconds.
    pub const fn debounce_total_us(&self) -> u64 {
        self.debounce_total_us
    }

    /// Debounce progress (`stable_time` in Linux's log line), microseconds.
    pub const fn debounce_stable_us(&self) -> u64 {
        self.debounce_stable_us
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
                if !obs.pp {
                    // Never leave a port unpowered: PP is only ever raised here.
                    self.begin(obs, now_us)
                } else if !obs.ccs {
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
            Phase::Debounce => self.debounce_step(obs, now_us),
            Phase::ResetWait => self.reset_wait_step(obs, now_us),
            Phase::Stabilize => {
                if obs.enabled() {
                    if now_us.saturating_sub(self.since_us) >= STABLE_US {
                        return self.enumerate(obs, now_us);
                    }
                    Step::Idle
                } else if obs.suspended() {
                    self.resume(now_us)
                } else {
                    // The enable did not hold. Do NOT reset here: back off for
                    // ≥1 s, then debounce and (only if still not enabled) reset
                    // again. This is the field's lost-PED path.
                    self.fail(obs, now_us, FailReason::LostAfterReset)
                }
            }
            Phase::Backoff => {
                if now_us < self.deadline_us {
                    return Step::Idle;
                }
                // Never power-cycle: re-observe and let the normal path raise
                // PP again only if it was actually lost.
                self.begin(obs, now_us)
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
        self.reset_tries = 0;
        self.deadline_us = now_us.saturating_add(backoff_us(self.failures));
        Step::Failed(reason)
    }

    // ── internals ───────────────────────────────────────────────────────────

    /// Refresh the per-attempt diagnostic snapshot from this observation.
    fn observe(&mut self, obs: PortObservation) {
        if obs.prc {
            self.seen_prc = true;
        }
        if obs.csc {
            self.seen_csc = true;
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

    /// First decision after Idle/Empty/Backoff/PowerSettle: power on if needed,
    /// adopt, resume, or start debouncing a detecting port. NEVER resets a
    /// port before its connect has been stable for [`HUB_DEBOUNCE_STABLE_US`].
    fn begin(&mut self, obs: PortObservation, now_us: u64) -> Step {
        // Power the port first, even with nothing attached: Linux powers every
        // root-hub port at init, and an unpowered port reports no connection at
        // all, so a keyboard on a port that HCRST left unpowered would otherwise
        // stay invisible forever.
        if !obs.pp {
            self.phase = Phase::PowerSettle;
            self.deadline_us = now_us.saturating_add(POWER_SETTLE_US);
            self.attempt_had_reset = false;
            return Step::PowerOn;
        }
        if !obs.ccs {
            self.phase = Phase::Empty;
            self.deadline_us = now_us.saturating_add(EMPTY_RETRY_US);
            self.attempt_had_reset = false;
            return Step::Idle;
        }
        if obs.enabled() {
            if self.need_reset {
                return self.start_reset(obs, now_us);
            }
            // Already enabled (firmware left it up, or it recovered during the
            // backoff): adopt it only after the same uninterrupted U0 window.
            return self.stabilize(obs, now_us);
        }
        if obs.suspended() {
            self.phase = Phase::ResumeWait;
            self.deadline_us = now_us.saturating_add(RESUME_WAIT_US);
            self.attempt_had_reset = false;
            return Step::Resume;
        }
        // Attached but not enabled: Linux hub_port_debounce first (25 ms steps,
        // 100 ms stable, 2 s budget), and only then a reset.
        self.start_debounce(obs, now_us);
        Step::Idle
    }

    fn start_debounce(&mut self, obs: PortObservation, now_us: u64) {
        self.phase = Phase::Debounce;
        self.debounce_started_us = now_us;
        self.debounce_next_us = now_us.saturating_add(HUB_DEBOUNCE_STEP_US);
        self.debounce_total_us = 0;
        self.debounce_stable_us = 0;
        // The observation that started the debounce is Linux's first
        // `usb_hub_port_status()` read: it establishes `connection`, so a steady
        // port completes HUB_DEBOUNCE_STABLE 100 ms after this point, not 125.
        self.debounce_have_conn = true;
        self.debounce_conn = obs.ccs;
        self.debounce_last_csc = obs.csc;
        self.since_us = now_us;
    }

    /// Linux `hub_port_debounce`: sample every 25 ms; the connection must be
    /// unchanged for 100 ms. A newly-set CSC or a CCS change restarts the timer.
    /// When the connection is stable, decide: adopt an enable, resume a suspend,
    /// or (only now) reset the still-disabled port.
    fn debounce_step(&mut self, obs: PortObservation, now_us: u64) -> Step {
        if now_us < self.debounce_next_us {
            return Step::Idle;
        }
        self.debounce_next_us = now_us.saturating_add(HUB_DEBOUNCE_STEP_US);
        self.debounce_total_us = now_us.saturating_sub(self.debounce_started_us);

        let csc_edge = obs.csc && !self.debounce_last_csc;
        self.debounce_last_csc = obs.csc;
        let conn = obs.ccs;
        if !self.debounce_have_conn {
            self.debounce_have_conn = true;
            self.debounce_conn = conn;
            self.debounce_stable_us = 0;
        } else if !csc_edge && conn == self.debounce_conn {
            self.debounce_stable_us = self
                .debounce_stable_us
                .saturating_add(HUB_DEBOUNCE_STEP_US)
                .min(HUB_DEBOUNCE_STABLE_US);
        } else {
            self.debounce_stable_us = 0;
            self.debounce_conn = conn;
        }

        if self.debounce_stable_us >= HUB_DEBOUNCE_STABLE_US {
            if !conn {
                // A stable *disconnection* (Linux debounces both directions).
                self.phase = Phase::Empty;
                self.deadline_us = now_us.saturating_add(EMPTY_RETRY_US);
                self.attempt_had_reset = false;
                return Step::Idle;
            }
            if obs.enabled() {
                // Self-enabled during detect/training: adopt without any PR,
                // but still require the stability window (the field port
                // produced exactly this PED+U0 glimpse before falling back).
                return self.stabilize(obs, now_us);
            }
            if obs.suspended() {
                return self.resume(now_us);
            }
            return self.start_reset(obs, now_us);
        }

        if now_us.saturating_sub(self.debounce_started_us) >= HUB_DEBOUNCE_TIMEOUT_US {
            return self.fail(obs, now_us, FailReason::DebounceTimeout);
        }
        Step::Idle
    }

    fn start_reset(&mut self, obs: PortObservation, now_us: u64) -> Step {
        // A retry inside one attempt (Linux's outer PORT_RESET_TRIES loop) keeps
        // the attempt clock and the pre-reset snapshot from the first write.
        if self.phase != Phase::ResetWait || self.reset_tries == 0 {
            self.attempt_started_us = now_us;
            self.reset_tries = 0;
            self.pre = obs.raw;
            self.seen_prc = false;
            self.seen_csc = false;
            self.seen_ped = false;
            self.seen_ped_u0 = false;
            self.ped_sc = 0;
        }
        self.reset_tries = self.reset_tries.saturating_add(1);
        self.phase = Phase::ResetWait;
        self.attempt_had_reset = true;
        self.need_reset = false;
        self.resets = self.resets.saturating_add(1);
        self.reset_wait_started_us = now_us;
        self.reset_wait_next_us = now_us.saturating_add(HUB_SHORT_RESET_TIME_US);
        self.reset_wait_delay_us = HUB_SHORT_RESET_TIME_US;
        self.reset_wait_samples = 0;
        Step::Reset { warm: self.usb3 }
    }

    /// Linux `hub_port_wait_reset`: poll every 10 ms twice, then every 200 ms,
    /// for up to 800 ms. Success is PR clear AND connection; then PED must be
    /// set, otherwise the reset is retried (up to five writes per attempt).
    fn reset_wait_step(&mut self, obs: PortObservation, now_us: u64) -> Step {
        if now_us < self.reset_wait_next_us {
            return Step::Idle;
        }
        self.reset_wait_samples = self.reset_wait_samples.saturating_add(1);
        let elapsed = now_us.saturating_sub(self.reset_wait_started_us);

        if !obs.pr && obs.ccs {
            if obs.enabled() {
                // Reset completed and the port is enabled at U0 with a speed.
                self.phase = Phase::Stabilize;
                self.since_us = now_us;
                self.deadline_us = now_us.saturating_add(STABLE_US);
                self.post = obs.raw;
                return Step::Idle;
            }
            // PR clear + connected but no usable enable: Linux returns -EBUSY
            // and writes the reset again.
            return self.reset_retry_or_fail(obs, now_us, FailReason::ResetTimeout);
        }

        if elapsed >= HUB_RESET_TIMEOUT_US {
            let reason = if obs.ccs {
                FailReason::ResetTimeout
            } else {
                FailReason::Disconnected
            };
            return self.reset_retry_or_fail(obs, now_us, reason);
        }

        // Two short polls, then the long spacing (hub.c HUB_LONG_RESET_TIME).
        if self.reset_wait_samples >= 2 {
            self.reset_wait_delay_us = HUB_LONG_RESET_TIME_US;
        }
        self.reset_wait_next_us = now_us.saturating_add(self.reset_wait_delay_us);
        Step::Idle
    }

    fn reset_retry_or_fail(
        &mut self,
        obs: PortObservation,
        now_us: u64,
        reason: FailReason,
    ) -> Step {
        if self.reset_tries < PORT_RESET_TRIES {
            return self.start_reset(obs, now_us);
        }
        self.fail(obs, now_us, reason)
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
        // `stabilize` is the adopt/resume path, so `attempt_had_reset` is
        // already false here; the reset path enters Stabilize directly and keeps
        // the attempt's "reset-ok" provenance.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(
        ccs: bool,
        ped: bool,
        pp: bool,
        speed: u8,
        pls: u8,
        prc: bool,
        csc: bool,
        pr: bool,
    ) -> PortObservation {
        let mut raw = 0u32;
        if ccs {
            raw |= 1 << 0;
        }
        if ped {
            raw |= 1 << 1;
        }
        if pp {
            raw |= 1 << 9;
        }
        raw |= (speed as u32 & 0xF) << 10;
        raw |= (pls as u32 & 0xF) << 5;
        if prc {
            raw |= 1 << 21;
        }
        if csc {
            raw |= 1 << 17;
        }
        if pr {
            raw |= 1 << 4;
        }
        PortObservation::new(ccs, ped, pp, speed, pls, prc, csc, pr, raw)
    }

    fn normal(pls: u8) -> PortObservation {
        obs(true, false, true, 0, pls, false, false, false)
    }

    fn enabled(speed: u8) -> PortObservation {
        obs(true, true, true, speed, PLS_U0, false, false, false)
    }

    /// Drive a detecting port through a full, stable debounce (four counted
    /// samples at 25 ms after the first establishing sample).
    fn stable_debounce(p: &mut PortPolicy, now: &mut u64, pls: u8) {
        assert_eq!(p.step(normal(pls), *now), Step::Idle);
        for _ in 0..5 {
            *now += HUB_DEBOUNCE_STEP_US;
            let step = p.step(normal(pls), *now);
            if !matches!(step, Step::Idle) {
                return;
            }
        }
    }

    #[test]
    fn neutral_write_never_carries_ped_pr_or_change_bits() {
        // A raw PORTSC from the field: CCS+PED+PP+speed=3+U0+PRC+CSC+PR+WPR.
        let mut raw = 0u32;
        raw |= 1 << 0; // CCS
        raw |= 1 << 1; // PED (write 1 disables!)
        raw |= 1 << 3;
        raw |= 0xF << 5; // PLS
        raw |= 1 << 9; // PP
        raw |= 3 << 10; // speed
        raw |= 1 << 16; // LWS
        raw |= 1 << 17; // CSC
        raw |= 1 << 21; // PRC
        raw |= 1 << 31; // WPR
        raw |= 1 << 4; // PR
        let neutral = port_state_to_neutral(raw);
        assert_eq!(neutral & (1 << 1), 0, "PED must never be written back");
        assert_eq!(neutral & (1 << 4), 0, "PR must not be re-issued");
        assert_eq!(neutral & (1 << 31), 0, "WPR must not be re-issued");
        assert_eq!(neutral & (1 << 16), 0, "LWS must not be set implicitly");
        assert_eq!(neutral & (1 << 17), 0, "CSC is write-1-to-clear");
        assert_eq!(neutral & (1 << 21), 0, "PRC is write-1-to-clear");
        // Read-only status / neutral write bits survive.
        assert_eq!(neutral & 1, 1, "CCS is read-only");
        assert_eq!(neutral & (0xF << 10), 3 << 10, "speed is read-only");
        assert_eq!(neutral & (0xF << 5), 0xF << 5, "PLS is neutral with LWS=0");
        assert_eq!(neutral & (1 << 9), 1 << 9, "PP survives");
        // The reset write is neutral | PR: a reset must not disable the port.
        let reset = neutral | (1 << 4);
        assert_eq!(reset & (1 << 1), 0);
    }

    #[test]
    fn debounce_needs_100ms_of_unchanged_connection() {
        let mut p = PortPolicy::new(false);
        let poll = normal(PLS_POLLING);
        let mut now = 0u64;
        assert_eq!(p.step(poll, now), Step::Idle);
        assert_eq!(p.phase(), Phase::Debounce);
        // Samples at 25/50/75 ms: still inside the 100 ms stable window.
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            assert_eq!(p.step(poll, now), Step::Idle);
            assert_eq!(p.resets(), 0);
        }
        // Fourth counted sample completes 100 ms of stable connection, and only
        // then is PR written (Linux hub_port_debounce → hub_port_reset).
        now += HUB_DEBOUNCE_STEP_US;
        assert_eq!(p.step(poll, now), Step::Reset { warm: false });
        assert_eq!(p.resets(), 1);
        assert_eq!(p.reset_tries(), 1);
    }

    #[test]
    fn a_new_csc_edge_restarts_the_debounce_timer() {
        let mut p = PortPolicy::new(false);
        let poll = normal(PLS_POLLING);
        let mut now = 0u64;
        assert_eq!(p.step(poll, now), Step::Idle);
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            assert_eq!(p.step(poll, now), Step::Idle);
        }
        // Connect change at 100 ms: the stable timer restarts, so the reset that
        // was due now does not happen.
        now += HUB_DEBOUNCE_STEP_US;
        let with_csc = obs(true, false, true, 0, PLS_POLLING, false, true, false);
        assert_eq!(p.step(with_csc, now), Step::Idle);
        assert_eq!(p.resets(), 0);
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            assert_eq!(p.step(poll, now), Step::Idle);
            assert_eq!(p.resets(), 0);
        }
        now += HUB_DEBOUNCE_STEP_US;
        assert_eq!(p.step(poll, now), Step::Reset { warm: false });
    }

    #[test]
    fn connection_that_never_settles_times_out_after_2s() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        // CCS toggles on every sample: stable_time never reaches 100 ms.
        let mut ccs = true;
        // 80 samples cover exactly the 2 s debounce budget.
        for _ in 0..80 {
            assert_eq!(
                p.step(
                    obs(ccs, false, true, 0, PLS_POLLING, false, false, false),
                    now
                ),
                Step::Idle,
                "a bouncing port is never reset"
            );
            ccs = !ccs;
            now += HUB_DEBOUNCE_STEP_US;
        }
        assert_eq!(now, HUB_DEBOUNCE_TIMEOUT_US);
        let step = p.step(
            obs(ccs, false, true, 0, PLS_POLLING, false, false, false),
            now,
        );
        assert_eq!(step, Step::Failed(FailReason::DebounceTimeout));
        assert_eq!(p.resets(), 0);
    }

    #[test]
    fn self_enabled_port_is_adopted_without_any_reset() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        assert_eq!(p.step(normal(PLS_POLLING), now), Step::Idle);
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            assert_eq!(p.step(normal(PLS_POLLING), now), Step::Idle);
        }
        // The PHY finishes training on its own before the debounce completes.
        now += HUB_DEBOUNCE_STEP_US;
        let en = enabled(3);
        assert_eq!(p.step(en, now), Step::Idle); // starts the 200 ms stability window
        assert_eq!(p.phase(), Phase::Stabilize);
        assert_eq!(p.step(en, now + STABLE_US - 1), Step::Idle);
        assert_eq!(p.step(en, now + STABLE_US), Step::Enumerate { speed: 3 });
        assert_eq!(p.resets(), 0, "a self-enabled port must not be reset");
    }

    #[test]
    fn reset_wait_follows_linux_polling_and_never_re_resets_inside_one_wait() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        stable_debounce(&mut p, &mut now, PLS_POLLING);
        // `stable_debounce` stopped as soon as a Reset step appeared.
        assert_eq!(p.resets(), 1);
        // First status poll is 10 ms later and still sees PR set.
        let in_reset = obs(true, false, true, 0, PLS_POLLING, true, false, true);
        assert_eq!(p.step(in_reset, now + 5_000), Step::Idle);
        assert_eq!(p.step(in_reset, now + HUB_SHORT_RESET_TIME_US), Step::Idle);
        // Second poll 20 ms after the write: PR clear, connection, PED+U0 at FS.
        let done = obs(true, true, true, 1, PLS_U0, true, false, false);
        assert_eq!(p.step(done, now + 2 * HUB_SHORT_RESET_TIME_US), Step::Idle);
        assert_eq!(p.phase(), Phase::Stabilize);
        assert_eq!(p.resets(), 1, "no second reset inside the wait");
        // The enable must hold for the full 200 ms before enumeration.
        let t = now + 2 * HUB_SHORT_RESET_TIME_US;
        assert_eq!(p.step(done, t + STABLE_US - 1), Step::Idle);
        assert_eq!(p.step(done, t + STABLE_US), Step::Enumerate { speed: 1 });
        assert_eq!(p.resets(), 1);
    }

    #[test]
    fn reset_without_enable_is_written_again_up_to_five_times() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        stable_debounce(&mut p, &mut now, PLS_POLLING);
        assert_eq!(p.resets(), 1);
        // PR clears and CCS stays, but PED never comes: Linux returns -EBUSY and
        // writes the reset again, until PORT_RESET_TRIES is exhausted.
        let no_enable = obs(true, false, true, 0, PLS_POLLING, true, false, false);
        for expected in 2..=PORT_RESET_TRIES {
            now += HUB_SHORT_RESET_TIME_US;
            assert_eq!(
                p.step(no_enable, now),
                Step::Reset { warm: false },
                "reset write #{expected}"
            );
            assert_eq!(p.resets(), expected as u16);
            assert_eq!(p.reset_tries(), expected);
        }
        // The fifth wait also ends without an enable: the attempt fails.
        now += HUB_SHORT_RESET_TIME_US;
        assert_eq!(
            p.step(no_enable, now),
            Step::Failed(FailReason::ResetTimeout)
        );
        assert_eq!(p.phase(), Phase::Backoff);
        assert_eq!(p.failures(), 1);
    }

    #[test]
    fn a_reset_that_never_completes_waits_800ms_per_try() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        stable_debounce(&mut p, &mut now, PLS_POLLING);
        let in_reset = obs(true, false, true, 0, PLS_POLLING, true, false, true);
        let reset_at = now;
        // 10 ms, 20 ms, then 200 ms spacing; at 820 ms the wait times out and
        // the reset is written again.
        let mut retries = 0;
        for _ in 0..120 {
            now += 10_000;
            match p.step(in_reset, now) {
                Step::Reset { .. } => {
                    retries += 1;
                    break;
                }
                Step::Idle => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(retries, 1);
        assert!(
            now - reset_at >= HUB_RESET_TIMEOUT_US,
            "first wait lasted only {} us",
            now - reset_at
        );
    }

    #[test]
    fn lost_ped_backs_off_one_second_then_debounces_before_reset() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        stable_debounce(&mut p, &mut now, PLS_POLLING);
        let t = now;
        let en = enabled(1);
        assert_eq!(p.step(en, t + 20_000), Step::Idle);
        // The field's glimpse: PED+U0 for less than 200 ms, then Polling.
        assert_eq!(
            p.step(normal(PLS_POLLING), t + 20_000 + 100_000),
            Step::Failed(FailReason::LostAfterReset)
        );
        assert_eq!(p.phase(), Phase::Backoff);
        // No reset for a full second.
        for delta in [1u64, 250, 500, 999] {
            assert_eq!(
                p.step(normal(PLS_POLLING), t + 20_000 + 100_000 + delta * 1_000),
                Step::Idle
            );
        }
        let after = t + 20_000 + 100_000 + RETRY_BACKOFF_US;
        // At the backoff deadline the port is observed again (debounce), not
        // reset immediately.
        assert_eq!(p.step(normal(PLS_POLLING), after), Step::Idle);
        assert_eq!(p.phase(), Phase::Debounce);
        let resets_before = p.resets();
        let mut t2 = after;
        for _ in 0..3 {
            t2 += HUB_DEBOUNCE_STEP_US;
            assert_eq!(p.step(normal(PLS_POLLING), t2), Step::Idle);
            assert_eq!(p.resets(), resets_before);
        }
        t2 += HUB_DEBOUNCE_STEP_US;
        assert_eq!(p.step(normal(PLS_POLLING), t2), Step::Reset { warm: false });
        assert_eq!(p.resets(), resets_before + 1);
    }

    #[test]
    fn power_on_is_retried_and_never_powered_off() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let unpowered = obs(true, false, false, 0, PLS_POLLING, false, false, false);
        assert_eq!(p.step(unpowered, now), Step::PowerOn);
        assert_eq!(p.phase(), Phase::PowerSettle);
        // Nothing happens during the 100 ms power settle.
        for delta in [1u64, 50, 99] {
            assert_eq!(p.step(unpowered, now + delta * 1_000), Step::Idle);
        }
        // Still unpowered at the deadline: ask again (the driver read it back).
        assert_eq!(p.step(unpowered, now + POWER_SETTLE_US), Step::PowerOn);
        // Powered but empty: an empty port is just observed, never reset.
        now += POWER_SETTLE_US;
        let powered_empty = obs(false, false, true, 0, PLS_POLLING, false, false, false);
        assert_eq!(p.step(powered_empty, now + POWER_SETTLE_US), Step::Idle);
        assert_eq!(p.phase(), Phase::Empty);
        assert_eq!(p.resets(), 0);
        assert_eq!(p.failures(), 0);
        // Powering on a port is idempotent: PP=1 never yields another PowerOn.
        for _ in 0..10 {
            now += 1_000_000;
            assert_eq!(p.step(powered_empty, now), Step::Idle);
        }
    }

    #[test]
    fn no_power_cycle_ever_after_many_failures() {
        // Drive a permanently failing attached port through many attempts and
        // assert the only power-related step is PowerOn (never off), and that
        // no attempt is ever made with PP missing. (The enum has no PowerOff
        // step any more; this pins the behaviour the field bug asked for.)
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let poll = normal(PLS_POLLING);
        let mut power_ons = 0;
        let mut resets_in_attempt = 0u16;
        let mut attempts = 0u16;
        let mut last_fail: Option<u64> = None;
        for _ in 0..400_000 {
            match p.step(poll, now) {
                Step::PowerOn => power_ons += 1,
                Step::Reset { .. } => {
                    if let Some(failed_at) = last_fail.take() {
                        assert!(
                            now - failed_at >= RETRY_BACKOFF_US,
                            "attempt restarted only {} us after failure",
                            now - failed_at
                        );
                    }
                    resets_in_attempt += 1;
                    assert!(
                        resets_in_attempt <= PORT_RESET_TRIES as u16,
                        "more than PORT_RESET_TRIES resets in one attempt"
                    );
                }
                Step::Failed(_) => {
                    attempts += 1;
                    resets_in_attempt = 0;
                    last_fail = Some(now);
                }
                Step::Idle => {}
                other => panic!("unexpected {other:?}"),
            }
            now += 1_000;
        }
        assert!(attempts >= 5, "the port keeps retrying (slowly)");
        assert_eq!(power_ons, 0, "PP=1 must never be toggled off/on");
    }

    #[test]
    fn empty_port_is_never_reset_or_power_cycled() {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let empty = obs(false, false, true, 0, PLS_RXDETECT, false, false, false);
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
        let mut now = 0u64;
        stable_debounce(&mut p, &mut now, PLS_POLLING);
        // `stable_debounce` returns on the Reset step; re-drive cleanly.
        let mut q = PortPolicy::new(true);
        let mut t = 0u64;
        assert_eq!(q.step(normal(PLS_POLLING), t), Step::Idle);
        let mut last = Step::Idle;
        for _ in 0..5 {
            t += HUB_DEBOUNCE_STEP_US;
            last = q.step(normal(PLS_POLLING), t);
            if matches!(last, Step::Reset { .. }) {
                break;
            }
        }
        assert_eq!(last, Step::Reset { warm: true });
    }

    #[test]
    fn resume_is_requested_for_suspended_enabled_ports() {
        let mut p = PortPolicy::new(false);
        let suspended = obs(true, true, true, 3, PLS_U3, false, false, false);
        assert_eq!(p.step(suspended, 0), Step::Resume);
        assert_eq!(p.phase(), Phase::ResumeWait);
        let en = enabled(3);
        assert_eq!(p.step(en, 20_000), Step::Idle);
        assert_eq!(p.step(en, 20_000 + STABLE_US), Step::Enumerate { speed: 3 });
        assert_eq!(p.resets(), 0);
    }

    #[test]
    fn enumeration_failure_resets_after_backoff_not_immediately() {
        let mut p = PortPolicy::new(false);
        let en = enabled(3);
        assert_eq!(p.step(en, 0), Step::Idle);
        assert_eq!(p.step(en, STABLE_US), Step::Enumerate { speed: 3 });
        assert_eq!(
            p.enumeration_failed(en, 5_000),
            Step::Failed(FailReason::EnumFailed)
        );
        // During the 1 s backoff the still-enabled port must not be touched.
        assert_eq!(p.step(en, 100_000), Step::Idle);
        // At the deadline the policy resets before enumerating again.
        assert_eq!(
            p.step(en, 5_000 + RETRY_BACKOFF_US),
            Step::Reset { warm: false }
        );
    }

    #[test]
    fn ped_snapshot_is_per_policy_and_records_the_last_real_enable() {
        let mut hs = PortPolicy::new(false);
        let mut fs = PortPolicy::new(false);
        let transient = obs(true, true, true, 3, PLS_POLLING, true, false, false);
        let hs_enabled = enabled(3);
        let fs_enabled = enabled(1);
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
        p.observe(obs(true, true, true, 3, PLS_POLLING, false, false, false));
        assert!(p.seen_ped());
        assert!(!p.seen_ped_u0(), "PED without U0 is not an enable");
        p.observe(obs(true, true, true, 1, PLS_POLLING, false, false, false));
        assert_eq!(p.ped_sc() & 0xF << 10, 1 << 10);
    }

    #[test]
    fn backoff_is_flat_one_second() {
        assert_eq!(backoff_us(0), RETRY_BACKOFF_US);
        assert_eq!(backoff_us(1), RETRY_BACKOFF_US);
        assert_eq!(backoff_us(u16::MAX), RETRY_BACKOFF_US);
    }
}
