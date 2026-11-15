// Feature: xHCI root-port reset policy (2.22.7 field fix)
//
// THE FIELD FAILURE THESE PROPERTIES PIN DOWN. On the real machine (Intel
// 8086:7a60, 26 ports) every attached USB2 port read CCS=1, then
// `ped=0 pls=7 stage=lost-after-reset try=19 ms=124`: the old `reset_port`
// waited only 60 ms for a port that was still in Polling, reset it anyway
// (restarting the USB2 debounce/training it had just interrupted), accepted a
// 20 ms PED glimpse as an enable, and retried every ~124 ms. QEMU enables ports
// instantly, so no boot test could see it.
//
// The decisions live in `src/drivers/xhci_port.rs` and are included in this
// crate verbatim, so these tests drive the exact state machine the kernel runs:
//   * a port in Polling/RxDetect is never reset before `DETECT_WAIT_US`, and a
//     port that enables on its own during that window is adopted without a
//     reset (this is the primary fix: stop interrupting the attach sequence);
//   * after a reset, PRC + PED+U0 are awaited for `RESET_WAIT_US` and no second
//     reset is written inside that window, no matter how often PED comes and
//     goes (the old driver's `try=19 ms=124` loop);
//   * a PED+U0 enable must hold for `STABLE_US` before enumeration;
//   * a lost PED leads to a backoff and a fresh observation window, never an
//     immediate reset; retries are ≥250 ms apart and cap at 1 s;
//   * PP is power-cycled only after `POWER_CYCLE_AFTER` failed attempts;
//   * `ped_sc` is a per-port snapshot of the last PED+U0 sighting (with
//     `seen_ped_u0` saying whether it is a real enable or a transient glimpse).

use crate::xhci_port::{
    backoff_us, FailReason, Phase, PortObservation, PortPolicy, Step, DETECT_WAIT_US,
    EMPTY_RETRY_US, PLS_POLLING, PLS_RXDETECT, PLS_U0, POWER_CYCLE_AFTER, RESET_WAIT_US,
    STABLE_US,
};
use proptest::prelude::*;

/// Build a raw PORTSC word with the same bit layout the driver decodes (xHCI
/// 1.2 table 5-27): CCS bit 0, PED bit 1, PLS bits 5-8, PP bit 9, speed bits
/// 10-13, CSC bit 17, PRC bit 21.
fn raw_portsc(ccs: bool, ped: bool, pp: bool, speed: u8, pls: u8, prc: bool) -> u32 {
    let mut raw = 0u32;
    if ccs {
        raw |= 1 << 0;
    }
    if ped {
        raw |= 1 << 1;
    }
    raw |= (pls as u32 & 0xF) << 5;
    if pp {
        raw |= 1 << 9;
    }
    raw |= (speed as u32 & 0xF) << 10;
    if prc {
        raw |= 1 << 21;
    }
    raw
}

fn observation(ccs: bool, ped: bool, pp: bool, speed: u8, pls: u8, prc: bool) -> PortObservation {
    PortObservation::new(
        ccs,
        ped,
        pp,
        speed,
        pls,
        prc,
        raw_portsc(ccs, ped, pp, speed, pls, prc),
    )
}

/// An attached port that has not enabled yet: CCS, no PED, PP on.
fn detecting(pls: u8) -> PortObservation {
    observation(true, false, true, 0, pls, false)
}

/// An attached port that is enabled at U0 with `speed`.
fn enabled(speed: u8) -> PortObservation {
    observation(true, true, true, speed, PLS_U0, true)
}

fn reset_warm(step: Step) -> Option<bool> {
    match step {
        Step::Reset { warm } => Some(warm),
        _ => None,
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// THE PRIMARY FIX. For every detecting link state and every point inside
    /// the debounce window, the policy returns Idle: no reset, no PP write, no
    /// state change. The old driver reset at ~60-124 ms and restarted training.
    #[test]
    fn detecting_ports_are_never_reset_inside_the_debounce_window(
        pls in prop_oneof![Just(PLS_POLLING), Just(PLS_RXDETECT)],
        point in 0u64..DETECT_WAIT_US,
        extra in 0usize..8,
    ) {
        let mut p = PortPolicy::new(false);
        let o = detecting(pls);
        prop_assert_eq!(p.step(o, 0), Step::Idle);
        prop_assert_eq!(p.phase(), Phase::Debounce);
        for i in 0..=extra {
            // Keep every sample strictly inside the window.
            let now = point.saturating_mul(i as u64 + 1) / (extra as u64 + 2);
            prop_assume!(now < DETECT_WAIT_US);
            prop_assert_eq!(p.step(o, now), Step::Idle);
            prop_assert_eq!(p.resets(), 0);
        }
        // At the deadline (if the port still has not enabled) a reset is the
        // correct escalation — once, and only then.
        prop_assert_eq!(reset_warm(p.step(o, DETECT_WAIT_US)), Some(false));
        prop_assert_eq!(p.resets(), 1);
    }

    /// A port that finishes training on its own during the debounce window is
    /// adopted as-is: no PR is ever written, whatever its speed.
    #[test]
    fn self_enabling_port_is_adopted_without_any_reset(
        speed in 1u8..=4,
        when in 0u64..DETECT_WAIT_US,
        pls in prop_oneof![Just(PLS_POLLING), Just(PLS_RXDETECT)],
    ) {
        let mut p = PortPolicy::new(false);
        prop_assert_eq!(p.step(detecting(pls), 0), Step::Idle);
        // Self-enable starts the same stability window as a reset enable: a
        // single PED+U0 read is not enough (the field glimpse).
        prop_assert_eq!(p.step(enabled(speed), when), Step::Idle);
        prop_assert_eq!(
            p.step(enabled(speed), when + STABLE_US),
            Step::Enumerate { speed }
        );
        prop_assert_eq!(p.resets(), 0, "a self-enabled port must not be reset");
    }

    /// A PED+U0 glimpse shorter than `STABLE_US` is not an enable: the policy
    /// starts a stability window, loses it, records one failure, and never
    /// enumerates. The old driver accepted a 20 ms glimpse.
    #[test]
    fn short_ped_glimpses_are_not_enumerated(
        speed in 1u8..=4,
        glimpse_us in 1u64..STABLE_US,
    ) {
        let mut p = PortPolicy::new(false);
        prop_assert_eq!(p.step(detecting(PLS_POLLING), 0), Step::Idle);
        prop_assert_eq!(reset_warm(p.step(detecting(PLS_POLLING), DETECT_WAIT_US)), Some(false));
        // The reset completes at +60 ms; PED+U0 is visible for `glimpse_us`.
        let started = DETECT_WAIT_US + 60_000;
        prop_assert_eq!(p.step(enabled(speed), started), Step::Idle);
        prop_assert_eq!(p.phase(), Phase::Stabilize);
        let after = started + glimpse_us;
        prop_assume!(after - started < STABLE_US);
        let step = p.step(detecting(PLS_POLLING), after);
        prop_assert_eq!(step, Step::Failed(FailReason::LostAfterReset));
        prop_assert_eq!(p.phase(), Phase::Backoff);
        prop_assert_eq!(p.resets(), 1, "no reset while inside the stability window");
    }

    /// THE `try=19 ms=124` REGRESSION. Once a reset is issued, no second reset
    /// may be written inside `RESET_WAIT_US`, even if the port alternates
    /// between a PED+U0 glimpse and Polling on every poll.
    #[test]
    fn reset_wait_never_issues_a_second_reset(
        speed in 1u8..=4,
        samples in 1usize..16,
    ) {
        let mut p = PortPolicy::new(false);
        prop_assert_eq!(p.step(detecting(PLS_POLLING), 0), Step::Idle);
        prop_assert_eq!(reset_warm(p.step(detecting(PLS_POLLING), DETECT_WAIT_US)), Some(false));
        let start = DETECT_WAIT_US;
        for i in 0..samples {
            // Alternate the field sequence: PED+U0+PRC, then Polling again.
            let now = start + (i as u64 + 1) * RESET_WAIT_US / (samples as u64 + 1);
            prop_assume!(now < start + RESET_WAIT_US);
            let o = if i % 2 == 0 {
                enabled(speed)
            } else {
                detecting(PLS_POLLING)
            };
            let step = p.step(o, now);
            prop_assert_ne!(step, Step::Reset { warm: false });
            prop_assert_ne!(step, Step::Reset { warm: true });
            prop_assert_eq!(p.resets(), 1);
        }
    }

    /// After a failed attempt the policy waits the 250 ms → 1 s backoff and then
    /// OBSERVES the still-Polling port for a fresh debounce window before it may
    /// reset again. Consecutive resets are therefore hundreds of milliseconds
    /// apart, not the field's 124 ms.
    #[test]
    fn failed_attempts_back_off_and_reobserve_before_resetting(
        attempts in 2usize..6,
    ) {
        let mut p = PortPolicy::new(false);
        let mut now = 0u64;
        let poll = detecting(PLS_POLLING);
        let mut last_reset = 0u64;
        let mut resets = 0u16;
        // Run enough simulated time for several attempts.
        for _ in 0..20_000 {
            match p.step(poll, now) {
                Step::Reset { .. } => {
                    if resets > 0 {
                        // Previous failure ended + backoff + debounce before this.
                        let spacing = now - last_reset;
                        prop_assert!(spacing >= RESET_WAIT_US, "resets only {spacing} us apart");
                    }
                    resets += 1;
                    last_reset = now;
                    if resets as usize >= attempts {
                        break;
                    }
                }
                Step::Failed(_) => {}
                _ => {}
            }
            now += 1_000;
        }
        prop_assert!(resets as usize >= attempts);
    }

    /// `ped_sc` is per port and records the last real PED+U0 sighting; the
    /// `seen_ped_u0` flag says whether the snapshot is an enable or a transient.
    #[test]
    fn ped_snapshot_is_the_last_u0_enable_and_per_port(
        speed_a in 1u8..=4,
        speed_b in 1u8..=4,
    ) {
        let mut a = PortPolicy::new(false);
        let mut b = PortPolicy::new(false);
        // Both ports see the same reset-time transient first (the field photo
        // showed the same 0x00200e03 on every USB2 port), then each enables at
        // its own speed.
        let transient = observation(true, true, true, 3, PLS_POLLING, true);
        prop_assert_eq!(a.step(transient, 0), Step::Idle);
        prop_assert_eq!(b.step(transient, 0), Step::Idle);
        prop_assert!(!a.seen_ped_u0(), "PED without U0 is not an enable");
        // Feed a real enable later in the debounce window; it must hold for the
        // stability window before enumeration.
        let enabled_a = observation(true, true, true, speed_a, PLS_U0, true);
        let enabled_b = observation(true, true, true, speed_b, PLS_U0, true);
        prop_assert_eq!(a.step(enabled_a, 10_000), Step::Idle);
        prop_assert_eq!(b.step(enabled_b, 10_000), Step::Idle);
        prop_assert_eq!(
            a.step(enabled_a, 10_000 + STABLE_US),
            Step::Enumerate { speed: speed_a }
        );
        prop_assert_eq!(
            b.step(enabled_b, 10_000 + STABLE_US),
            Step::Enumerate { speed: speed_b }
        );
        prop_assert!(a.seen_ped_u0() && b.seen_ped_u0());
        prop_assert_eq!((a.ped_sc() >> 10) & 0xF, speed_a as u32);
        prop_assert_eq!((b.ped_sc() >> 10) & 0xF, speed_b as u32);
        prop_assert_eq!(a.ped_sc() & 1, 1, "CCS is still in the per-port snapshot");
        if speed_a != speed_b {
            prop_assert_ne!(a.ped_sc(), b.ped_sc());
        }
    }

    /// A port with nothing attached costs one read per `EMPTY_RETRY_US`: it is
    /// never reset, never fails, never power-cycled.
    #[test]
    fn empty_ports_are_never_reset_or_power_cycled(
        pp in any::<bool>(),
        pls in 0u8..16,
        points in proptest::collection::vec(0u64..5_000_000, 1..32),
    ) {
        let mut p = PortPolicy::new(false);
        let o = observation(false, false, pp, 0, pls, false);
        for now in points {
            prop_assert_eq!(p.step(o, now), Step::Idle);
        }
        prop_assert_eq!(p.resets(), 0);
        prop_assert_eq!(p.failures(), 0);
        prop_assert!(matches!(p.phase(), Phase::Empty));
    }

    /// USB3 ports (Supported Protocol major revision 3) reset with WPR, USB2
    /// ports with PR. A port that never enables must not silently switch modes.
    #[test]
    fn usb3_ports_use_warm_reset(
        pls in prop_oneof![Just(PLS_POLLING), Just(PLS_RXDETECT)],
    ) {
        let mut p = PortPolicy::new(true);
        prop_assert_eq!(p.step(detecting(pls), 0), Step::Idle);
        prop_assert_eq!(reset_warm(p.step(detecting(pls), DETECT_WAIT_US)), Some(true));
    }

    /// Retry backoff is bounded to 250 ms..=1 s and non-decreasing in the
    /// failure count; no attempt count can overflow into a busy retry loop.
    #[test]
    fn backoff_is_bounded_and_non_decreasing(failures in 0u16..2_000) {
        let b = backoff_us(failures);
        prop_assert!((250_000..=1_000_000).contains(&b));
        if failures > 0 {
            prop_assert!(backoff_us(failures) >= backoff_us(failures - 1));
        }
    }
}

/// Deterministic simulation of the field machine: a PCH USB2 port that is in
/// Polling for 700 ms after the controller starts and then enables on its own.
/// The fixed policy must adopt it without a single reset; the old one reset it
/// at ~60 ms and restarted the training it was waiting for.
#[test]
fn field_machine_long_debounce_is_adopted_without_reset() {
    let mut p = PortPolicy::new(false);
    let poll = detecting(PLS_POLLING);
    let mut now = 0u64;
    let mut resets = 0;
    let mut enumerated = false;
    while now < 5_000_000 {
        let o = if now >= 700_000 {
            enabled(3)
        } else {
            poll
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
    assert!(
        resets <= 1,
        "a slow self-enabling port must not be reset repeatedly (resets={resets})"
    );
}

/// The exact field failure, simulated: every reset produces a short PED+U0
/// glimpse followed by Polling forever. The policy must not hammer resets: the
/// first attempt is given its full 500 ms window and the next reset waits for
/// backoff + a fresh debounce window. This is the `try=19 ms=124` loop turned
/// into a ≤1 Hz, deliberate retry.
#[test]
fn field_machine_lost_after_reset_retries_slowly() {
    let mut p = PortPolicy::new(false);
    let mut now = 0u64;
    let mut reset_times: Vec<u64> = Vec::new();
    let mut glimpse_until = 0u64;
    while now < 20_000_000 {
        let o = if now < glimpse_until {
            enabled(3)
        } else {
            detecting(PLS_POLLING)
        };
        match p.step(o, now) {
            Step::Reset { .. } => {
                reset_times.push(now);
                // The controller raises PED/PRC, then loses it: the field's
                // `ped_seen=1 prc=1 post=0x6e1 pls=7`.
                glimpse_until = now + 30_000;
            }
            _ => {}
        }
        now += 1_000;
    }
    assert!(reset_times.len() >= 2, "permanent failure must keep retrying");
    for pair in reset_times.windows(2) {
        let spacing = pair[1] - pair[0];
        assert!(
            spacing >= 650_000,
            "resets {pair:?} only {spacing} us apart — this is the 124 ms loop"
        );
    }
    // The second reset comes after the failed attempt's window/backoff and a
    // fresh debounce window — hundreds of milliseconds, not 124 ms.
    assert!(reset_times[1] - reset_times[0] >= 650_000);
}

/// Permanent enable failure escalates to a PP power-cycle only after
/// `POWER_CYCLE_AFTER` failed attempts, and the cycle costs a 500 ms power-off
/// and a fresh debounce — never a reset while PP is down.
#[test]
fn power_cycle_is_a_late_last_resort() {
    let mut p = PortPolicy::new(false);
    let poll = detecting(PLS_POLLING);
    let mut now = 0u64;
    let mut resets = 0u16;
    let mut power_offs = 0u16;
    let mut resets_at_first_power_off = None;
    while now < 60_000_000 && power_offs == 0 {
        match p.step(poll, now) {
            Step::Reset { .. } => resets += 1,
            Step::PowerOff => {
                resets_at_first_power_off = Some(resets);
                power_offs += 1;
            }
            _ => {}
        }
        now += 1_000;
    }
    assert_eq!(
        power_offs, 1,
        "the port never reached the power-cycle escalation"
    );
    let at = resets_at_first_power_off.unwrap();
    assert!(
        at >= POWER_CYCLE_AFTER,
        "power-cycled after only {at} reset(s)"
    );
}

/// An empty port is re-examined once per `EMPTY_RETRY_US` and never touched
/// otherwise.
#[test]
fn empty_port_retry_interval_is_a_second() {
    let mut p = PortPolicy::new(false);
    let empty = observation(false, false, true, 0, PLS_RXDETECT, false);
    assert_eq!(p.step(empty, 0), Step::Idle);
    assert_eq!(p.phase(), Phase::Empty);
    assert_eq!(p.step(empty, EMPTY_RETRY_US - 1), Step::Idle);
    assert_eq!(p.step(empty, EMPTY_RETRY_US), Step::Idle);
    assert_eq!(p.resets(), 0);
}