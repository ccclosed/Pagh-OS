//! Pure USB HID report policy: buffer sizing, report layouts, recovery bound.
//!
//! WHY THIS MODULE EXISTS. The driver used to hard-code "keyboard = 8 bytes,
//! mouse = 4 bytes" and to parse every mouse report as the 3-byte boot layout.
//! The field E-Signal mouse (04d9:a09f) answers in report protocol with a 7-byte
//! report, so a 4-byte transfer buffer made the xHCI controller report a babble
//! and the old code then disabled the endpoint until reboot. The decisions that
//! fix it — how big the transfer buffer must be, which layout a report has, how
//! many times a halted endpoint may be reset — are pure and live here so the
//! host tests exercise the same source the kernel compiles.

/// `bInterfaceSubClass` value that means "boot interface" (HID 1.11 §4.2).
pub const SUBCLASS_BOOT: u8 = 1;
/// `bInterfaceProtocol` 1 = keyboard (HID 1.11 §4.3).
pub const PROTOCOL_KEYBOARD: u8 = 1;
/// `bInterfaceProtocol` 2 = mouse (HID 1.11 §4.3).
pub const PROTOCOL_MOUSE: u8 = 2;

/// Fixed boot-keyboard report size (HID 1.11 appendix B.1).
pub const BOOT_KEYBOARD_REPORT: usize = 8;
/// Fixed boot-mouse report size (HID 1.11 appendix B.2).
pub const BOOT_MOUSE_REPORT: usize = 3;

/// Largest interrupt-IN transfer this driver arms.
///
/// A full-speed HID endpoint may declare up to 64 bytes. Report-protocol HID
/// devices can declare more, but a keyboard/mouse report that large is not a
/// layout this driver can parse; the buffer is clamped and the clamp is logged,
/// so an unusually large endpoint is visible instead of producing a silent
/// babble loop.
pub const MAX_REPORT: usize = 64;

/// How many Reset Endpoint attempts one HID endpoint gets before the driver
/// gives up on it. Chosen to survive the one-off babble/data-buffer-error a
/// report-protocol device produces when its report is longer than the buffer
/// (the bug this fixes) without an endless reset storm on a truly dead device.
pub const MAX_RECOVERIES: u8 = 3;

/// Which report parser an interface needs, from its subclass/protocol bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HidClass {
    Keyboard,
    Mouse,
    /// Vendor/consumer collections and `protocol = 0` interfaces: a boot
    /// keyboard or mouse they are not, and the driver has no layout for them.
    Unsupported,
}

/// Which report layout the device is sending, decided by SET_PROTOCOL's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportMode {
    /// SET_PROTOCOL(0) succeeded: fixed 8-byte keyboard / 3-byte mouse layout.
    Boot,
    /// The device refused SET_PROTOCOL(0); it sends its own report descriptor's
    /// layout. Parsing is best effort and documented in `usb_hid.rs`.
    Report,
}

impl ReportMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            ReportMode::Boot => "boot",
            ReportMode::Report => "report",
        }
    }
}

/// Whether an interface is a boot keyboard/mouse this driver can parse.
///
/// Boot *class* is a requirement even for a device that ends up in report
/// protocol: a protocol-0 vendor collection has no keyboard/mouse semantics to
/// apply, and binding its endpoint would burn a DMA buffer and a ring to feed
/// `usb_hid` reports it must ignore anyway.
pub fn bindable(subclass: u8, protocol: u8) -> bool {
    class_of(subclass, protocol) != HidClass::Unsupported
}

/// Classify an interface by `bInterfaceSubClass`/`bInterfaceProtocol`.
pub fn class_of(subclass: u8, protocol: u8) -> HidClass {
    if subclass != SUBCLASS_BOOT {
        return HidClass::Unsupported;
    }
    match protocol {
        PROTOCOL_KEYBOARD => HidClass::Keyboard,
        PROTOCOL_MOUSE => HidClass::Mouse,
        _ => HidClass::Unsupported,
    }
}

/// Size of the interrupt-IN transfer buffer for an endpoint.
///
/// THE INVARIANT: the requested length must be at least the endpoint's
/// `wMaxPacketSize`, because one interrupt transfer may legitimately deliver a
/// full packet. A device sending more than the requested length makes the
/// controller report a babble (CC 3) and halt the endpoint — that is exactly how
/// the 4-byte mouse buffer killed the field mouse's 7-byte report. The lower
/// bound of 8 keeps the boot-keyboard report (8 bytes) whole even when a broken
/// descriptor declares a smaller packet size.
pub fn transfer_len(max_packet: u16) -> usize {
    // Bits 11-12 of wMaxPacketSize are the high-speed multiplier, not bytes.
    let mps = (max_packet & 0x07FF) as usize;
    mps.max(BOOT_KEYBOARD_REPORT).min(MAX_REPORT)
}

/// Minimum report length that can be decoded for this interface.
///
/// Boot keyboard reports are fixed at 8 bytes; a boot mouse report is 3. In
/// report protocol the layout is unknown, but no mouse layout can be decoded
/// from fewer than 3 bytes and the keyboard path needs the 8-byte boot-like
/// shape (best effort, see `usb_hid.rs`).
pub fn minimum_report_len(subclass: u8, protocol: u8, mode: ReportMode) -> usize {
    match (class_of(subclass, protocol), mode) {
        (HidClass::Keyboard, _) => BOOT_KEYBOARD_REPORT,
        (HidClass::Mouse, ReportMode::Boot) => BOOT_MOUSE_REPORT,
        (HidClass::Mouse, ReportMode::Report) => 3,
        (HidClass::Unsupported, _) => usize::MAX,
    }
}

/// One decoded relative-pointer report.
///
/// `dx`/`dy` are in HID sign convention: positive `dy` points down the screen.
/// The translator flips Y for the PS/2 path; keeping the raw value here makes
/// the layout tests independent of that consumer convention.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseReport {
    pub buttons: u8,
    pub dx: i16,
    pub dy: i16,
    /// Vertical wheel detents (report protocol only; the PS/2 path here has no
    /// wheel byte, so this is diagnostics-only for now).
    pub wheel: i8,
    /// Horizontal pan (report protocol only, diagnostics-only).
    pub pan: i8,
}

/// Decode one mouse report according to the mode the interface is in.
///
/// Boot mode is the fixed 3-byte `[buttons, dx, dy]`. In report mode the exact
/// layout is device-specific and a report descriptor parser is out of scope; the
/// driver supports the two shapes seen in the field:
///
/// * **7 bytes and longer** — the E-Signal/Holtek-style report
///   `[buttons, x_lo, x_hi, y_lo, y_hi, wheel, pan]` with 16-bit little-endian
///   X/Y (the actual device this fix was written for: 04d9:a09f);
/// * **3-6 bytes** — 8-bit axes: `[buttons, dx, dy, (wheel), ...]`. A 5/6-byte
///   16-bit layout is indistinguishable without the report descriptor and is
///   documented as unsupported rather than guessed.
pub fn mouse_report(report: &[u8], mode: ReportMode) -> Option<MouseReport> {
    if report.len() < 3 {
        return None;
    }
    let buttons = report[0] & 0x07;
    match mode {
        ReportMode::Boot => Some(MouseReport {
            buttons,
            dx: report[1] as i8 as i16,
            dy: report[2] as i8 as i16,
            wheel: 0,
            pan: 0,
        }),
        ReportMode::Report => {
            if report.len() >= 7 {
                Some(MouseReport {
                    buttons,
                    dx: i16::from_le_bytes([report[1], report[2]]),
                    dy: i16::from_le_bytes([report[3], report[4]]),
                    wheel: report[5] as i8,
                    pan: report[6] as i8,
                })
            } else {
                Some(MouseReport {
                    buttons,
                    dx: report[1] as i8 as i16,
                    dy: report[2] as i8 as i16,
                    wheel: if report.len() >= 4 {
                        report[3] as i8
                    } else {
                        0
                    },
                    pan: 0,
                })
            }
        }
    }
}

/// Decode one boot keyboard report: `(modifiers, six held usages)`.
///
/// Boot report byte 1 is reserved, bytes 2-7 are usages; `0` (no key) and `1`
/// (ErrorRollOver) are not presses. Report-protocol keyboards are parsed with
/// the same shape (best effort, documented); an interface whose report does not
/// even reach 8 bytes is rejected here rather than read out of bounds.
pub fn keyboard_report(report: &[u8]) -> Option<(u8, [u8; 6])> {
    if report.len() < BOOT_KEYBOARD_REPORT {
        return None;
    }
    let mut held = [0u8; 6];
    for (i, slot) in held.iter_mut().enumerate() {
        let usage = report[2 + i];
        *slot = if usage <= 1 { 0 } else { usage };
    }
    Some((report[0], held))
}

/// Clamp a relative delta to the 8-bit range the PS/2 packet carries.
///
/// A 16-bit report can ask for +1000 in one poll; the PS/2 packet is 9-bit, and
/// the old code narrowed with `as i8` (wrapping +1000 to -24, i.e. the cursor
/// jumping backwards). Clamping keeps the direction and the maximum speed.
pub fn clamp_delta(v: i16) -> i8 {
    v.clamp(i8::MIN as i16, i8::MAX as i16) as i8
}

/// What the driver should do after an endpoint reported a transfer error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Issue xHCI Reset Endpoint and re-arm the transfer next poll.
    ResetEndpoint,
    /// The retry budget is spent: stop polling this endpoint.
    Disable,
}

/// Recovery policy for one endpoint, given how many resets it already had.
pub fn recovery_action(previous_attempts: u8) -> RecoveryAction {
    if previous_attempts < MAX_RECOVERIES {
        RecoveryAction::ResetEndpoint
    } else {
        RecoveryAction::Disable
    }
}
