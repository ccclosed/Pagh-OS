// Feature: USB HID on real hardware (2.22.7 field fix)
// Properties `usb_hid_proto` / descriptor walk: the pure policy behind the HID
// driver.
//
// THE FIELD FAILURE THIS PINS DOWN. On the real machine the E-Signal mouse
// (04d9:a09f) answered SET_PROTOCOL with report protocol (the driver asked for
// wValue=1 while its comment said boot) and sent a 7-byte report. The driver's
// mouse buffer was hard-coded to 4 bytes, so the xHCI controller reported a
// babble (CC 3) and the old code disabled the endpoint until reboot — a mouse
// that never worked. The same report would then have been parsed as the boot
// `[buttons, dx, dy]` layout, i.e. bytes 1-2 are X_low/X_high, not a signed
// delta pair.
//
// These tests drive the exact two modules the kernel compiles:
//   * `usb_hid_proto::transfer_len` — buffer sizing from `wMaxPacketSize`, with
//     the invariant "never shorter than one packet" (>= one full report);
//   * `usb_hid_proto::mouse_report` / `keyboard_report` — boot layouts and the
//     documented report-protocol fallbacks (7-byte 16-bit E-Signal layout,
//     8-bit axes), including the ambiguity boundary;
//   * `usb_desc::parse_hid_interfaces` — composite devices with several HID
//     interfaces, alternate settings, malformed/truncated descriptors.

use crate::usb_desc::{parse_hid_interfaces, HidIface};
use crate::usb_hid_proto::{
    bindable, clamp_delta, class_of, keyboard_report, minimum_report_len, mouse_report,
    recovery_action, transfer_len, HidClass, RecoveryAction, ReportMode, BOOT_KEYBOARD_REPORT,
    BOOT_MOUSE_REPORT, MAX_RECOVERIES, MAX_REPORT, PROTOCOL_KEYBOARD, PROTOCOL_MOUSE,
    SUBCLASS_BOOT,
};
use proptest::prelude::*;

// ─── transfer buffer sizing ─────────────────────────────────────────────────

proptest! {
    /// The buffer must be at least one whole packet (the babble guard) and is
    /// clamped to the driver's parseable maximum.
    #[test]
    fn usb_transfer_len_never_shorter_than_a_packet(max_packet in any::<u16>()) {
        let len = transfer_len(max_packet);
        // Bits 11-12 are the high-speed multiplier, not bytes.
        let mps = (max_packet & 0x07FF) as usize;
        prop_assert!((BOOT_KEYBOARD_REPORT..=MAX_REPORT).contains(&len));
        if mps <= MAX_REPORT {
            prop_assert!(len >= mps, "len {} < wMaxPacketSize {}", len, mps);
        } else {
            prop_assert_eq!(len, MAX_REPORT);
        }
    }
}

/// The exact field regression: a 7-byte report against `wMaxPacketSize = 4`
/// (the old hard-coded mouse length) must now fit.
#[test]
fn usb_transfer_len_fits_the_field_mouse_7_byte_report() {
    assert_eq!(transfer_len(4), 8);
    assert!(transfer_len(4) >= 7);
    assert_eq!(transfer_len(7), 8);
    assert_eq!(transfer_len(8), 8);
    assert_eq!(transfer_len(16), 16);
    assert_eq!(transfer_len(64), 64);
    // High-speed multiplier bits do not inflate the byte length.
    assert_eq!(transfer_len(0x1808), 8);
    // Oversized declarations are clamped (and logged by the caller).
    assert_eq!(transfer_len(1024), MAX_REPORT);
    assert_eq!(transfer_len(0), BOOT_KEYBOARD_REPORT);
}

// ─── delta clamping ─────────────────────────────────────────────────────────

proptest! {
    /// A 16-bit report delta is clamped into the PS/2 packet's 8-bit range
    /// instead of wrapping (the old `as i8` cast turned +1000 into -24).
    #[test]
    fn usb_clamp_delta_keeps_range_and_direction(v in any::<i16>()) {
        let c = clamp_delta(v);
        prop_assert!((i8::MIN..=i8::MAX).contains(&c));
        if (i8::MIN as i16..=i8::MAX as i16).contains(&v) {
            prop_assert_eq!(c, v as i8);
        } else if v > 0 {
            prop_assert_eq!(c, i8::MAX);
        } else {
            prop_assert_eq!(c, i8::MIN);
        }
    }
}

// ─── mouse report layouts ───────────────────────────────────────────────────

proptest! {
    /// Boot mouse: exactly `[buttons, dx, dy]`, deltas are i8.
    #[test]
    fn usb_mouse_boot_layout(b0 in any::<u8>(), b1 in any::<u8>(), b2 in any::<u8>()) {
        let r = mouse_report(&[b0, b1, b2], ReportMode::Boot).expect("3-byte boot report");
        prop_assert_eq!(r.buttons, b0 & 0x07);
        prop_assert_eq!(r.dx, b1 as i8 as i16);
        prop_assert_eq!(r.dy, b2 as i8 as i16);
        prop_assert_eq!(r.wheel, 0);
        prop_assert_eq!(r.pan, 0);
    }

    /// 7-byte report protocol: 16-bit little-endian axes are the field
    /// E-Signal/Holtek layout.
    #[test]
    fn usb_mouse_report_protocol_16bit(
        b0 in any::<u8>(), xlo in any::<u8>(), xhi in any::<u8>(),
        ylo in any::<u8>(), yhi in any::<u8>(), w in any::<u8>(), p in any::<u8>(),
    ) {
        let report = [b0, xlo, xhi, ylo, yhi, w, p];
        let r = mouse_report(&report, ReportMode::Report).expect("7-byte report");
        prop_assert_eq!(r.buttons, b0 & 0x07);
        prop_assert_eq!(r.dx, i16::from_le_bytes([xlo, xhi]));
        prop_assert_eq!(r.dy, i16::from_le_bytes([ylo, yhi]));
        prop_assert_eq!(r.wheel, w as i8);
        prop_assert_eq!(r.pan, p as i8);
        // The bytes that are a 16-bit X are NOT the boot dx/dy bytes: the old
        // blind boot parse would have read xlo as dx and xhi as dy.
        let boot = mouse_report(&report, ReportMode::Boot).expect("boot parse still possible");
        prop_assert_eq!(boot.dx, xlo as i8 as i16);
        prop_assert_eq!(boot.dy, xhi as i8 as i16);
    }

    /// 3-6 byte report protocol reports are decoded as 8-bit axes; a 4th byte,
    /// when present, is the wheel. This is the documented best-effort boundary.
    #[test]
    fn usb_mouse_report_protocol_short(
        b0 in any::<u8>(), b1 in any::<u8>(), b2 in any::<u8>(), b3 in any::<u8>(),
    ) {
        for len in 3..=4usize {
            let report = [b0, b1, b2, b3];
            let r = mouse_report(&report[..len], ReportMode::Report)
                .expect("3-4 byte report protocol report");
            prop_assert_eq!(r.buttons, b0 & 0x07);
            prop_assert_eq!(r.dx, b1 as i8 as i16);
            prop_assert_eq!(r.dy, b2 as i8 as i16);
            prop_assert_eq!(r.wheel, if len >= 4 { b3 as i8 } else { 0 });
            prop_assert_eq!(r.pan, 0);
        }
    }
}

/// No mouse layout can be decoded from fewer than 3 bytes, and that is the only
/// rejection: a 3-byte report is valid in both modes.
#[test]
fn usb_mouse_short_reports_are_rejected() {
    assert!(mouse_report(&[], ReportMode::Boot).is_none());
    assert!(mouse_report(&[0], ReportMode::Boot).is_none());
    assert!(mouse_report(&[0, 1], ReportMode::Report).is_none());
    assert!(mouse_report(&[0, 1, 2], ReportMode::Boot).is_some());
    assert!(mouse_report(&[0, 1, 2], ReportMode::Report).is_some());
}

// ─── keyboard report ────────────────────────────────────────────────────────

proptest! {
    /// Usage 0 (no key) and 1 (ErrorRollOver) are not presses; everything else
    /// is passed through verbatim, and shorter-than-boot reports are rejected
    /// instead of read out of bounds.
    #[test]
    fn usb_keyboard_report_layout(
        mods in any::<u8>(),
        bytes in proptest::collection::vec(any::<u8>(), 6..=12),
    ) {
        let mut report = std::vec![mods, 0u8];
        report.extend_from_slice(&bytes);
        let (m, keys) = keyboard_report(&report).expect(">= 8 bytes");
        prop_assert_eq!(m, mods);
        for i in 0..6 {
            let raw = report[2 + i];
            prop_assert_eq!(keys[i], if raw <= 1 { 0 } else { raw });
        }
        prop_assert!(keyboard_report(&report[..7]).is_none());
    }
}

/// Minimum report length per interface/mode: boot keyboard 8, boot mouse 3,
/// report-protocol mouse 3 (layouts below that cannot be decoded), and an
/// unsupported interface can never produce feedable input.
#[test]
fn usb_minimum_report_len_matrix() {
    assert_eq!(
        minimum_report_len(SUBCLASS_BOOT, PROTOCOL_KEYBOARD, ReportMode::Boot),
        BOOT_KEYBOARD_REPORT
    );
    assert_eq!(
        minimum_report_len(SUBCLASS_BOOT, PROTOCOL_KEYBOARD, ReportMode::Report),
        BOOT_KEYBOARD_REPORT
    );
    assert_eq!(
        minimum_report_len(SUBCLASS_BOOT, PROTOCOL_MOUSE, ReportMode::Boot),
        BOOT_MOUSE_REPORT
    );
    assert_eq!(
        minimum_report_len(SUBCLASS_BOOT, PROTOCOL_MOUSE, ReportMode::Report),
        3
    );
    assert_eq!(
        minimum_report_len(0, PROTOCOL_MOUSE, ReportMode::Report),
        usize::MAX
    );
}

// ─── class policy + recovery policy ─────────────────────────────────────────

#[test]
fn usb_class_policy_matrix() {
    assert_eq!(class_of(SUBCLASS_BOOT, PROTOCOL_KEYBOARD), HidClass::Keyboard);
    assert_eq!(class_of(SUBCLASS_BOOT, PROTOCOL_MOUSE), HidClass::Mouse);
    // protocol 0 is "no protocol" (consumer/system/vendor collections).
    assert_eq!(class_of(SUBCLASS_BOOT, 0), HidClass::Unsupported);
    // subclass 0 = not a boot interface.
    assert_eq!(class_of(0, PROTOCOL_KEYBOARD), HidClass::Unsupported);
    assert!(bindable(SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
    assert!(bindable(SUBCLASS_BOOT, PROTOCOL_MOUSE));
    assert!(!bindable(SUBCLASS_BOOT, 0));
    assert!(!bindable(2, PROTOCOL_MOUSE));
}

proptest! {
    /// The retry bound is exact: the first `MAX_RECOVERIES` errors reset the
    /// endpoint, the next one disables it (no endless reset storm).
    #[test]
    fn usb_recovery_action_bound(previous in any::<u8>()) {
        let expected = if previous < MAX_RECOVERIES {
            RecoveryAction::ResetEndpoint
        } else {
            RecoveryAction::Disable
        };
        prop_assert_eq!(recovery_action(previous), expected);
    }
}

// ─── configuration-descriptor walk ──────────────────────────────────────────

fn iface_desc(number: u8, alt: u8, class: u8, subclass: u8, protocol: u8) -> std::vec::Vec<u8> {
    std::vec![9, 4, number, alt, 0, class, subclass, protocol, 0]
}

fn ep_desc(addr: u8, attrs: u8, max_packet: u16, interval: u8) -> std::vec::Vec<u8> {
    let [lo, hi] = max_packet.to_le_bytes();
    std::vec![7, 5, addr, attrs, lo, hi, interval]
}

fn config_desc(total: u16) -> std::vec::Vec<u8> {
    let [lo, hi] = total.to_le_bytes();
    std::vec![9, 2, lo, hi, 0, 7, 0x80, 0x32, 1]
}

/// A three-interface combo device in the shape of the field hardware: if0 boot
/// keyboard (8-byte reports), if1 HID consumer control (protocol 0), if2 boot
/// mouse (7-byte report). All three parse; only two are bindable.
#[test]
fn usb_desc_parses_composite_hid() {
    let mut body = std::vec::Vec::new();
    body.extend(iface_desc(0, 0, 3, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
    body.extend(ep_desc(0x81, 0x03, 8, 10));
    body.extend(iface_desc(1, 0, 3, SUBCLASS_BOOT, 0));
    body.extend(ep_desc(0x82, 0x03, 8, 10));
    body.extend(iface_desc(2, 0, 3, SUBCLASS_BOOT, PROTOCOL_MOUSE));
    body.extend(ep_desc(0x83, 0x03, 7, 4));
    let mut conf = config_desc(9 + body.len() as u16);
    conf.extend(body);
    conf[5] = 7; // bConfigurationValue

    let ifaces = parse_hid_interfaces(&conf);
    assert_eq!(ifaces.len(), 3, "every HID interface must be found");
    assert_eq!(
        ifaces[0],
        HidIface {
            config_value: 7,
            interface: 0,
            subclass: SUBCLASS_BOOT,
            protocol: PROTOCOL_KEYBOARD,
            ep_addr: 0x81,
            max_packet: 8,
            interval: 10,
        }
    );
    assert_eq!(ifaces[1].interface, 1);
    assert_eq!(ifaces[1].protocol, 0);
    assert_eq!(ifaces[2].ep_addr, 0x83);
    assert_eq!(ifaces[2].max_packet, 7);

    let bound: std::vec::Vec<_> = ifaces
        .iter()
        .filter(|i| bindable(i.subclass, i.protocol))
        .collect();
    assert_eq!(bound.len(), 2, "consumer control is not a boot keyboard/mouse");
    assert_eq!(bound[0].interface, 0);
    assert_eq!(bound[1].interface, 2);
}

/// Alternate settings belong to the same interface number and must not yield a
/// second endpoint: the driver never selects an alternate setting.
#[test]
fn usb_desc_ignores_alternate_settings_and_duplicate_endpoints() {
    let mut body = std::vec::Vec::new();
    body.extend(iface_desc(0, 0, 3, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
    // Two interrupt-IN endpoints on the default setting: only the first is used.
    body.extend(ep_desc(0x81, 0x03, 8, 10));
    body.extend(ep_desc(0x82, 0x03, 8, 10));
    // Alternate setting 1 of the same interface: ignored entirely.
    body.extend(iface_desc(0, 1, 3, SUBCLASS_BOOT, PROTOCOL_MOUSE));
    body.extend(ep_desc(0x84, 0x03, 8, 10));
    let mut conf = config_desc(9 + body.len() as u16);
    conf.extend(body);

    let ifaces = parse_hid_interfaces(&conf);
    assert_eq!(ifaces.len(), 1);
    assert_eq!(ifaces[0].interface, 0);
    assert_eq!(ifaces[0].protocol, PROTOCOL_KEYBOARD);
    assert_eq!(ifaces[0].ep_addr, 0x81);
}

/// Non-HID interfaces, HID interfaces without interrupt-IN endpoints, and bulk
/// endpoints are all skipped; the walk stays aligned through unknown class
/// descriptors (e.g. HID report descriptors of type 0x22).
#[test]
fn usb_desc_skips_non_input_interfaces() {
    let mut body = std::vec::Vec::new();
    // Non-HID interface (mass storage), endpoint 0x01 is interrupt but class != 3.
    body.extend(iface_desc(0, 0, 8, 6, 0x50));
    body.extend(ep_desc(0x01, 0x03, 8, 10));
    // HID interface whose only endpoint is an interrupt OUT.
    body.extend(iface_desc(1, 0, 3, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
    body.extend(ep_desc(0x02, 0x03, 8, 10));
    // HID interface whose only endpoint is bulk IN.
    body.extend(iface_desc(2, 0, 3, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
    body.extend(ep_desc(0x83, 0x02, 8, 10));
    // HID interface with a class descriptor (report descriptor type 0x22),
    // then an interrupt-IN endpoint: both must be traversed correctly.
    body.extend(iface_desc(3, 0, 3, SUBCLASS_BOOT, PROTOCOL_MOUSE));
    body.extend(std::vec![9, 0x21, 0x11, 0x01, 0, 1, 0x22, 0x40, 0]);
    body.extend(ep_desc(0x84, 0x03, 8, 10));
    let mut conf = config_desc(9 + body.len() as u16);
    conf.extend(body);

    let ifaces = parse_hid_interfaces(&conf);
    assert_eq!(ifaces.len(), 1);
    assert_eq!(ifaces[0].interface, 3);
    assert_eq!(ifaces[0].ep_addr, 0x84);
}

proptest! {
    /// Arbitrary bytes are a liar's descriptor: the walk must never panic, must
    /// terminate, and every interface it does return must be a HID interface
    /// with an IN-direction endpoint.
    #[test]
    fn usb_desc_is_total_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let ifaces = parse_hid_interfaces(&bytes);
        for iface in &ifaces {
            prop_assert_eq!(iface.ep_addr & 0x80, 0x80, "only IN endpoints are bound");
        }
    }

    /// Truncating a valid descriptor never panics and never invents an interface
    /// the full descriptor did not contain (a prefix property: the returned
    /// vector is a prefix of the full parse).
    #[test]
    fn usb_desc_truncation_is_a_prefix(
        n_interfaces in 1usize..=4,
        max_packet in 1u16..=64,
        cut in 0usize..512,
    ) {
        let mut body = std::vec::Vec::new();
        for n in 0..n_interfaces {
            body.extend(iface_desc(n as u8, 0, 3, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
            body.extend(ep_desc(0x81 + (n as u8 & 0x0F), 0x03, max_packet, 10));
        }
        let mut conf = config_desc(9 + body.len() as u16);
        conf.extend(body);
        let full = parse_hid_interfaces(&conf);
        let cut = cut.min(conf.len());
        let partial = parse_hid_interfaces(&conf[..cut]);
        prop_assert!(partial.len() <= full.len());
        for (i, iface) in partial.iter().enumerate() {
            prop_assert_eq!(iface, &full[i], "truncated parse diverged at {}", i);
        }
    }
}

/// Length fields that cannot be followed (zero, one, or past the end) stop the
/// walk instead of looping or reading out of bounds.
#[test]
fn usb_desc_rejects_unwalkable_lengths() {
    assert!(parse_hid_interfaces(&[]).is_empty());
    assert!(parse_hid_interfaces(&[0, 4, 0]).is_empty());
    assert!(parse_hid_interfaces(&[1, 4, 0]).is_empty());
    // Descriptor claims 200 bytes inside a 9-byte buffer.
    assert!(parse_hid_interfaces(&[200, 4, 0, 0, 0, 3, 1, 1, 0]).is_empty());
}