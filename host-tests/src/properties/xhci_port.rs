// Feature: xHCI root-port bring-up policy (2.22.7 field iterations, v2)
//
// THE FIELD STORY THESE PROPERTIES PIN DOWN. On the real machine (Intel
// 8086:7a60, 26 ports) the first 2.22.7 fix stopped a reset every ~124 ms, but
// the port still lost the enable it had just waited for:
//   `ccs=0 ped=0 pp=0 pls=4 stage=power-cycle try=16 ms=159
//    pre=0x6e1 post=0x6e1 prc=1 ped_seen=1 ped_u0=1 ped_sc=0x00200603`
// Two things were wrong beyond timing:
//   * the PORTSC writes preserved the raw read value except for the change
//     bits, so they carried PED (bit 1) — write 1 to PED DISABLES the port.
//     Acknowledge-the-reset on an enabled port therefore turned it off;
//   * the PP power-cycle escalation could leave the port unpowered (pp=0,
//     ccs=0), and its 20 ms settle is below the USB2 power/debounce budget.
// The policy was rewritten to follow Linux `hub.c`/`xhci-hub.c`: neutral PORTSC
// writes (`port_state_to_neutral`), `hub_port_debounce` (25 ms steps, 100 ms
// stable, 2 s budget), `hub_port_wait_reset` (10 ms/200 ms polls, 800 ms per
// write, 5 writes), a 100 ms power settle, no power-cycle at all, and ≥1 s
// between attempts. QEMU enables ports instantly, so these properties simulate
// the PCH port the kernel has to drive.
//
// Every assertion below drives the exact `src/drivers/xhci_port.rs` the kernel
// compiles (included verbatim in this crate).

use crate::xhci_port::{
    backoff_us, port_state_to_neutral, FailReason, Phase, PortObservation, PortPolicy, Step,
    HUB_DEBOUNCE_STEP_US, HUB_DEBOUNCE_TIMEOUT_US, HUB_LONG_RESET_TIME_US, HUB_RESET_TIMEOUT_US,
    HUB_SHORT_RESET_TIME_US, PLS_POLLING, PLS_RXDETECT, PLS_U0, PORT_RESET_TRIES,
    POWER_SETTLE_US, RETRY_BACKOFF_US, STABLE_US,
};
use proptest::prelude::*;

/// Build a raw PORTSC word with the same bit layout the driver decodes (xHCI
/// 1.2 table 5-27): CCS bit 0, PED bit 1, PR bit 4, PLS bits 5-8, PP bit 9,
/// speed bits 10-13, CSC bit 17, PRC bit 21, WPR bit 31.
fn raw_portsc(ccs: bool, ped: bool, pp: bool, speed: u8, pls: u8, prc: bool, csc: bool, pr: bool) -> u32 {
    let mut raw = 0u32;
    if ccs {
        raw |= 1 << 0;
    }
    if ped {
        raw |= 1 << 1;
    }
    if pr {
        raw |= 1 << 4;
    }
    raw |= (pls as u32 & 0xF) << 5;
    if pp {
        raw |= 1 << 9;
    }
    raw |= (speed as u32 & 0xF) << 10;
    if csc {
        raw |= 1 << 17;
    }
    if prc {
        raw |= 1 << 21;
    }
    raw
}

fn observation(
    ccs: bool,
    ped: bool,
    pp: bool,
    speed: u8,
    pls: u8,
    prc: bool,
    csc: bool,
    pr: bool,
) -> PortObservation {
    PortObservation::new(
        ccs,
        ped,
        pp,
        speed,
        pls,
        prc,
        csc,
        pr,
        raw_portsc(ccs, ped, pp, speed, pls, prc, csc, pr),
    )
}

/// An attached port that has not enabled yet: CCS, no PED, PP on.
fn detecting(pls: u8) -> PortObservation {
    observation(true, false, true, 0, pls, false, false, false)
}

/// An attached port that is enabled at U0 with `speed`.
fn enabled(speed: u8) -> PortObservation {
    observation(true, true, true, speed, PLS_U0, true, false, false)
}

/// The port as Linux's first even and then `hub_port_wait_reset` sees it.
fn in_reset() -> PortObservation {
    observation(true, false, true, 0, PLS_POLLING, true, false, true)
}

fn reset_warm(step: Step) -> Option<bool> {
    match step {
        Step::Reset { warm } => Some(warm),
        _ => None,
    }
}

/// Drive a connected, still-detecting port through `hub_port_debounce` and the
/// first reset write. Returns the time of the write (all times in µs).
fn drive_to_first_reset(p: &mut PortPolicy) -> u64 {
    let mut now = 0u64;
    assert_eq!(p.step(detecting(PLS_POLLING), now), Step::Idle);
    for _ in 0..4 {
        now += HUB_DEBOUNCE_STEP_US;
        let step = p.step(detecting(PLS_POLLING), now);
        if matches!(step, Step::Reset { .. }) {
            assert_eq!(step, Step::Reset { warm: false });
            return now;
        }
        assert_eq!(step, Step::Idle);
    }
    panic!("debounce did not produce a reset after 100 ms of stable connection");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// The root cause of the second field round: a PORTSC write must never
    /// carry PED (write-1-to-disable), PR/WPR (write-1-to-reset), LWS, or a
    /// write-1-to-clear change bit. Linux's `xhci_port_state_to_neutral` keeps
    /// only RO and RWS bits; so must ours.
    #[test]
    fn neutral_portsc_write_drops_every_write_one_to_clear_bit(raw in any::<u32>()) {
        let neutral = port_state_to_neutral(raw);
        // Write-1-to-clear / write-1-to-set / reserved bits.
        let dangerous: u32 = (1 << 1)   // PED: write 1 disables the port
            | (1 << 2)                  // RsvdZ
            | (1 << 4)                  // PR: write 1 initiates a reset
            | (1 << 16)                 // LWS
            | (1 << 17)                 // CSC
            | (1 << 18)                 // PEC
            | (1 << 19)                 // WRC
            | (1 << 20)                 // OCC
            | (1 << 21)                 // PRC
            | (1 << 22)                 // PLC
            | (1 << 23)                 // CEC
            | (1 << 24)                 // RsvdZ
            | (1 << 28)
            | (1 << 29)
            | (1 << 31);                // WPR
        prop_assert_eq!(neutral & dangerous, 0, "neutral write carried {:08x}", neutral & dangerous);
        // Read-only (CCS, bit 3, speed, bit 30) and read/write-neutral (PLS, PP,
        // bits 14-15, 25-27) bits must survive.
        let ro: u32 = (1 << 0) | (1 << 3) | (0xF << 10) | (1 << 30);
        let rws: u32 = (0xF << 5) | (1 << 9) | (0x3 << 14) | (0x7 << 25);
        prop_assert_eq!(neutral & (ro | rws), raw & (ro | rws));
        // A reset write is neutral | PR: still no PED.
        let reset = neutral | (1 << 4);
        prop_assert_eq!(reset & (1 << 1), 0);
        // Neutralising an already-neutral value changes nothing.
        prop_assert_eq!(port_state_to_neutral(neutral), neutral);
    }

    /// Linux `hub_port_debounce`: no PR before 100 ms of unchanged connection,
    /// and then exactly one reset (not one per poll).
    #[test]
    fn reset_waits_for_100ms_of_stable_connection(
        pls in prop_oneof![Just(PLS_POLLING), Just(PLS_RXDETECT)],
    ) {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        prop_assert_eq!(p.step(detecting(pls), now), Step::Idle);
        // 75 ms of stable connection is not enough: Linux waits 100 ms.
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            prop_assert_eq!(p.step(detecting(pls), now), Step::Idle);
            prop_assert_eq!(p.resets(), 0);
        }
        // The fourth 25 ms sample completes the window: one reset, not one per
        // poll.
        now += HUB_DEBOUNCE_STEP_US;
        prop_assert_eq!(p.step(detecting(pls), now), Step::Reset { warm: false });
        prop_assert_eq!(p.resets(), 1);
        prop_assert_eq!(p.phase(), Phase::ResetWait);
    }

    /// A new CSC edge (a reconnect) restarts the 100 ms stable timer, because
    /// the sticky CSC bit is deliberately not cleared before the reset.
    #[test]
    fn csc_edge_restarts_the_stable_timer(
        after_samples in 1usize..4,
        speed in 1u8..=4,
    ) {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let poll = detecting(PLS_POLLING);
        prop_assert_eq!(p.step(poll, now), Step::Idle);
        for _ in 0..after_samples {
            now += HUB_DEBOUNCE_STEP_US;
            prop_assert_eq!(p.step(poll, now), Step::Idle);
        }
        // Connect-change sample: the stable time restarts here.
        now += HUB_DEBOUNCE_STEP_US;
        let with_csc = observation(true, false, true, 0, PLS_POLLING, false, true, false);
        prop_assert_eq!(p.step(with_csc, now), Step::Idle);
        prop_assert_eq!(p.resets(), 0);
        // It cannot have enabled: a reset comes only after another full 100 ms.
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            prop_assert_eq!(p.step(poll, now), Step::Idle);
            prop_assert_eq!(p.resets(), 0);
        }
        // The port self-enables during the restarted window: adopt, no PR.
        let en = enabled(speed);
        now += HUB_DEBOUNCE_STEP_US;
        prop_assert_eq!(p.step(en, now), Step::Idle);
        prop_assert_eq!(p.phase(), Phase::Stabilize);
        prop_assert_eq!(p.step(en, now + STABLE_US), Step::Enumerate { speed });
        prop_assert_eq!(p.resets(), 0);
    }

    /// A port that enables on its own after a stable connection is adopted
    /// without any PR, and only after the enable holds for `STABLE_US`.
    #[test]
    fn self_enabled_port_is_adopted_without_reset(speed in 1u8..=4) {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        prop_assert_eq!(p.step(detecting(PLS_POLLING), now), Step::Idle);
        // Become enabled exactly when the debounce completes.
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            prop_assert_eq!(p.step(detecting(PLS_POLLING), now), Step::Idle);
        }
        now += HUB_DEBOUNCE_STEP_US;
        let en = enabled(speed);
        prop_assert_eq!(p.step(en, now), Step::Idle);
        prop_assert_eq!(p.phase(), Phase::Stabilize);
        prop_assert_eq!(p.step(en, now + STABLE_US - 1), Step::Idle);
        prop_assert_eq!(p.step(en, now + STABLE_US), Step::Enumerate { speed });
        prop_assert_eq!(p.resets(), 0);
    }

    /// Linux `hub_port_wait_reset`: success is PR clear + connection, then PED;
    /// no second reset is written while a reset wait is running.
    #[test]
    fn reset_wait_uses_linux_poll_spacing_and_checks_ped(
        speed in 1u8..=4,
        pr_polls in 0usize..3,
    ) {
        let mut p = PortPolicy::new(false);
        let reset_at = drive_to_first_reset(&mut p);
        let mut now = reset_at;
        // The controller keeps PR set for a few short polls.
        for _ in 0..pr_polls {
            now += HUB_SHORT_RESET_TIME_US;
            prop_assert_eq!(p.step(in_reset(), now), Step::Idle);
            prop_assert_eq!(p.resets(), 1, "no second reset inside a wait");
        }
        // PR clears, connection is still there, and the port is enabled at U0.
        // The wait is polled every 200 ms after the first two short polls, so
        // feed the success sample when the next 200 ms poll is due.
        let done = enabled(speed);
        let success_at = now + HUB_LONG_RESET_TIME_US;
        prop_assert_eq!(p.step(done, success_at), Step::Idle);
        prop_assert_eq!(p.phase(), Phase::Stabilize);
        prop_assert_eq!(p.resets(), 1);
        prop_assert_eq!(
            p.step(done, success_at + STABLE_US),
            Step::Enumerate { speed }
        );
    }

    /// PR clear + connection but no enable is Linux's -EBUSY: the reset is
    /// written again, at most `PORT_RESET_TRIES` times for one attempt.
    #[test]
    fn reset_without_enable_is_retried_at_most_five_times(
        speed in 1u8..=4,
    ) {
        let _ = speed; // the port never reports a speed without PED
        let mut p = PortPolicy::new(false);
        let mut now = drive_to_first_reset(&mut p);
        let no_enable = observation(true, false, true, 0, PLS_POLLING, true, false, false);
        for expected in 2..=PORT_RESET_TRIES {
            now += HUB_SHORT_RESET_TIME_US;
            prop_assert_eq!(p.step(no_enable, now), Step::Reset { warm: false });
            prop_assert_eq!(p.resets(), expected as u16);
            prop_assert_eq!(p.reset_tries(), expected);
        }
        now += HUB_SHORT_RESET_TIME_US;
        prop_assert_eq!(p.step(no_enable, now), Step::Failed(FailReason::ResetTimeout));
        prop_assert_eq!(p.phase(), Phase::Backoff);
    }

    /// A reset the controller never completes is given `HUB_RESET_TIMEOUT` per
    /// write, and every write is retried only after that window (not per poll).
    #[test]
    fn a_stuck_reset_is_abandoned_after_800ms(extra in 0u64..40) {
        let mut p = PortPolicy::new(false);
        let reset_at = drive_to_first_reset(&mut p);
        let mut now = reset_at;
        let mut next_reset = None;
        for _ in 0..(100 + extra) {
            now += 10_000;
            if let Step::Reset { warm: false } = p.step(in_reset(), now) {
                next_reset = Some(now);
                break;
            }
        }
        let retried = next_reset.expect("a stuck reset must be retried");
        prop_assert!(
            retried - reset_at >= HUB_RESET_TIMEOUT_US,
            "retried after only {} us",
            retried - reset_at
        );
        prop_assert_eq!(p.reset_tries(), 2);
    }

    /// After a lost PED the policy pauses ≥1 s, then debounces again before any
    /// new reset — never the field's 124 ms loop.
    #[test]
    fn lost_ped_backs_off_and_reobserves_before_resetting(
        speed in 1u8..=4,
        glimpse_us in 1_000u64..STABLE_US,
    ) {
        let mut p = PortPolicy::new(false);
        let reset_at = drive_to_first_reset(&mut p);
        let t = reset_at;
        let en = enabled(speed);
        prop_assert_eq!(p.step(en, t + 20_000), Step::Idle);
        let lost_at = t + 20_000 + glimpse_us;
        prop_assert_eq!(
            p.step(detecting(PLS_POLLING), lost_at),
            Step::Failed(FailReason::LostAfterReset)
        );
        let failed_at = lost_at;
        // Inside the 1 s backoff nothing happens.
        for delta in [0u64, 1, 100, 500, 999] {
            prop_assert_eq!(
                p.step(detecting(PLS_POLLING), failed_at + delta * 1_000),
                Step::Idle
            );
        }
        // At the deadline the port is observed (debounce), not reset.
        let resume_at = failed_at + RETRY_BACKOFF_US;
        prop_assert_eq!(p.step(detecting(PLS_POLLING), resume_at), Step::Idle);
        prop_assert_eq!(p.phase(), Phase::Debounce);
        let resets_before = p.resets();
        let mut now = resume_at;
        for _ in 0..3 {
            now += HUB_DEBOUNCE_STEP_US;
            prop_assert_eq!(p.step(detecting(PLS_POLLING), now), Step::Idle);
            prop_assert_eq!(p.resets(), resets_before);
        }
        now += HUB_DEBOUNCE_STEP_US;
        prop_assert_eq!(p.step(detecting(PLS_POLLING), now), Step::Reset { warm: false });
        prop_assert!(now - failed_at >= RETRY_BACKOFF_US);
    }

    /// PP is only ever raised (never cycled), the power settle is 100 ms, and a
    /// port that never gets attached stays observed rather than reset.
    #[test]
    fn power_is_raised_once_and_never_cycled(settle_fraction in 1u64..99) {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let unpowered = observation(true, false, false, 0, PLS_POLLING, false, false, false);
        prop_assert_eq!(p.step(unpowered, now), Step::PowerOn);
        prop_assert_eq!(p.phase(), Phase::PowerSettle);
        // Nothing is written during the settle.
        let inside = now + settle_fraction * POWER_SETTLE_US / 100;
        prop_assert_eq!(p.step(unpowered, inside), Step::Idle);
        // At the deadline a still-unpowered port gets the write again.
        prop_assert_eq!(p.step(unpowered, now + POWER_SETTLE_US), Step::PowerOn);
        // Powered but empty: observed, never reset, and PP is never touched
        // again (there is no PowerOff/PowerCycle step any more).
        now += POWER_SETTLE_US;
        let empty = observation(false, false, true, 0, PLS_RXDETECT, false, false, false);
        prop_assert_eq!(p.step(empty, now + POWER_SETTLE_US), Step::Idle);
        prop_assert_eq!(p.phase(), Phase::Empty);
        for _ in 0..50 {
            now += 1_000_000;
            prop_assert_eq!(p.step(empty, now), Step::Idle);
        }
        prop_assert_eq!(p.resets(), 0);
        prop_assert_eq!(p.failures(), 0);
    }

    /// A connection that keeps bouncing is never reset; the debounce gives up
    /// after Linux's 2 s budget and records a failure.
    #[test]
    fn flapping_connection_times_out_without_a_reset(_start in any::<bool>()) {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let mut ccs = true;
        prop_assert_eq!(
            p.step(
                observation(ccs, false, true, 0, PLS_POLLING, false, false, false),
                now
            ),
            Step::Idle
        );
        // Toggle every 25 ms sample: stable_time never reaches 100 ms.
        let mut failed = false;
        for _ in 0..80 {
            ccs = !ccs;
            now += HUB_DEBOUNCE_STEP_US;
            match p.step(
                observation(ccs, false, true, 0, PLS_POLLING, false, false, false),
                now,
            ) {
                Step::Idle => {}
                Step::Failed(FailReason::DebounceTimeout) => {
                    failed = true;
                    break;
                }
                other => panic!("unexpected step {other:?}"),
            }
            prop_assert_eq!(p.resets(), 0);
        }
        prop_assert!(failed, "a flapping port must time out");
        prop_assert!(now >= HUB_DEBOUNCE_TIMEOUT_US);
        prop_assert_eq!(p.resets(), 0);
    }

    /// Once the port is enabled and stable, enumeration failure resets only
    /// after the same ≥1 s backoff, never immediately on an enabled port.
    #[test]
    fn enumeration_failure_resets_after_the_backoff(speed in 1u8..=4) {
        let mut p = PortPolicy::new(false);
        let en = enabled(speed);
        prop_assert_eq!(p.step(en, 0), Step::Idle);
        prop_assert_eq!(p.step(en, STABLE_US), Step::Enumerate { speed });
        prop_assert_eq!(
            p.enumeration_failed(en, 5_000),
            Step::Failed(FailReason::EnumFailed)
        );
        prop_assert_eq!(p.step(en, 5_000 + RETRY_BACKOFF_US - 1), Step::Idle);
        prop_assert_eq!(
            p.step(en, 5_000 + RETRY_BACKOFF_US),
            Step::Reset { warm: false }
        );
    }

    /// USB3 ports (Supported Protocol major revision 3) use WPR, USB2 use PR.
    #[test]
    fn usb3_ports_use_warm_reset(pls in prop_oneof![Just(PLS_POLLING), Just(PLS_RXDETECT)]) {
        let mut p = PortPolicy::new(true);
        let mut now = 0u64;
        prop_assert_eq!(p.step(detecting(pls), now), Step::Idle);
        let mut saw = None;
        for _ in 0..10 {
            now += HUB_DEBOUNCE_STEP_US;
            if let Some(warm) = reset_warm(p.step(detecting(pls), now)) {
                saw = Some(warm);
                break;
            }
        }
        prop_assert_eq!(saw, Some(true));
    }

    /// `ped_sc` is per port and records the last real PED+U0 sighting; the
    /// `seen_ped_u0` flag distinguishes it from a transient PED read.
    #[test]
    fn ped_snapshot_is_the_last_u0_enable_and_per_port(
        speed_a in 1u8..=4,
        speed_b in 1u8..=4,
    ) {
        let mut a = PortPolicy::new(false);
        let mut b = PortPolicy::new(false);
        let transient = observation(true, true, true, 3, PLS_POLLING, true, false, false);
        prop_assert_eq!(a.step(transient, 0), Step::Idle);
        prop_assert_eq!(b.step(transient, 0), Step::Idle);
        prop_assert!(!a.seen_ped_u0(), "PED without U0 is not an enable");
        let en_a = enabled(speed_a);
        let en_b = enabled(speed_b);
        prop_assert_eq!(a.step(en_a, 10_000), Step::Idle);
        prop_assert_eq!(b.step(en_b, 10_000), Step::Idle);
        prop_assert!(a.seen_ped_u0() && b.seen_ped_u0());
        prop_assert_eq!((a.ped_sc() >> 10) & 0xF, speed_a as u32);
        prop_assert_eq!((b.ped_sc() >> 10) & 0xF, speed_b as u32);
        prop_assert_eq!(a.ped_sc() & 1, 1, "CCS is still in the per-port snapshot");
        if speed_a != speed_b {
            prop_assert_ne!(a.ped_sc(), b.ped_sc());
        }
    }

    /// Backoff is a flat ≥1 s floor; no attempt count can overflow into a busy
    /// retry loop.
    #[test]
    fn backoff_is_a_flat_one_second_floor(failures in 0u16..2_000) {
        prop_assert_eq!(backoff_us(failures), RETRY_BACKOFF_US);
        prop_assert!(RETRY_BACKOFF_US >= 1_000_000);
    }
}

/// The exact second field photo, simulated: a port that is connected, never
/// enables while it is debounced, and only produces PED+U0 for a moment after
/// each reset before falling back to Polling. The policy must not hammer it:
/// at most five resets per attempt, ≥1 s between attempts, and a fresh 100 ms
/// stable debounce before every burst.
#[test]
fn field_machine_lost_after_reset_retries_in_bursts() {
    let mut p = PortPolicy::new(false);
    let mut now = 0u64;
    let mut reset_times: Vec<u64> = Vec::new();
    let mut glimpse_until = 0u64;
    let mut last_fail = None::<u64>;
    let mut reset_burst = 0u16;
    while now < 30_000_000 {
        let o = if now < glimpse_until {
            enabled(1) // the field's ped_sc=0x00200603: FS speed id 1 at U0
        } else {
            detecting(PLS_POLLING)
        };
        match p.step(o, now) {
            Step::Reset { .. } => {
                if let Some(at) = last_fail.take() {
                    assert!(now - at >= RETRY_BACKOFF_US);
                }
                reset_burst += 1;
                assert!(reset_burst <= PORT_RESET_TRIES as u16);
                reset_times.push(now);
                glimpse_until = now + 30_000;
            }
            Step::Failed(_) => {
                last_fail = Some(now);
                reset_burst = 0;
            }
            _ => {}
        }
        now += 1_000;
    }
    assert!(reset_times.len() >= PORT_RESET_TRIES as usize, "the port keeps retrying");
    // Every reset is either inside a 5-write burst (≈10-20 ms apart) or a new
    // attempt (≥1 s + debounce after the last failure). The old loop's 124 ms
    // cadence cannot occur: no reset is 100..999 ms after another.
    for pair in reset_times.windows(2) {
        let spacing = pair[1] - pair[0];
        assert!(
            spacing <= 200_000 || spacing >= 1_000_000,
            "resets {pair:?} only {spacing} us apart"
        );
    }
}

/// A stable connected port that only enables itself after 700 ms (the slow PCH
/// PHY) is reset after the 100 ms debounce (Linux behaviour) and the later
/// self-enable is still adopted once PED+U0 holds for 200 ms — the late enable
/// is not lost to a reset loop.
#[test]
fn slow_self_enabling_port_is_adopted_after_the_reset() {
    let mut p = PortPolicy::new(false);
    let mut now = 0u64;
    let mut resets = 0;
    let mut enumerated = false;
    while now < 5_000_000 {
        let o = if now >= 700_000 {
            enabled(3)
        } else {
            detecting(PLS_POLLING)
        };
        match p.step(o, now) {
            Step::Reset { .. } => resets += 1,
            Step::Enumerate { speed } => {
                assert_eq!(speed, 3);
                enumerated = true;
                break;
            }
            _ => {}
        }
        now += 10_000;
    }
    assert!(enumerated, "the port never enumerated");
    assert!(resets >= 1, "Linux resets a connected, non-enabled port after 100 ms");
    assert!(
        resets <= 2 * PORT_RESET_TRIES,
        "the port must not be reset over and over (resets={resets})"
    );
}

/// An empty port is re-examined once per second and never reset.
#[test]
fn empty_port_retry_interval_is_a_second() {
    let mut p = PortPolicy::new(false);
    let empty = observation(false, false, true, 0, PLS_RXDETECT, false, false, false);
    assert_eq!(p.step(empty, 0), Step::Idle);
    assert_eq!(p.phase(), Phase::Empty);
    assert_eq!(p.step(empty, 1_000_000 - 1), Step::Idle);
    assert_eq!(p.step(empty, 1_000_000), Step::Idle);
    assert_eq!(p.resets(), 0);
}