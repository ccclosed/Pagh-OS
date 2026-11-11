//! USB HID boot protocol → the kernel's existing input path.
//!
//! WHY A TRANSLATOR AND NOT A SECOND INPUT STACK. The shell, the line editor,
//! the `^C`/`^Z` latches, `lxrun`'s stdin and every compat process all consume
//! **PS/2 set-1 scancodes** off one ring buffer (`drivers::ps2_kbd`). A USB
//! keyboard reports HID usage codes in an 8-byte boot report. Rather than teach
//! every consumer about a second source, this module translates the report into
//! exactly the byte stream the 8042 would have produced — including the `0xE0`
//! prefix for extended keys and the high-bit-set break codes — and pushes it into
//! the same ring. Nothing downstream knows the difference, which is also why
//! `^C` from a USB keyboard stops a `lxrun`ed program like it always did.
//!
//! Boot protocol, not report protocol: a boot keyboard always sends the fixed
//! 8-byte report `[modifiers, reserved, key0..key5]` and a boot mouse the 3-byte
//! `[buttons, dx, dy]`, so no report-descriptor parser is needed (and none is
//! implemented). Devices default to boot protocol on the interface; the driver
//! does not have to send SET_PROTOCOL for the common case, though the
//! descriptors' protocol byte is logged so a report-protocol-only device is
//! identifiable.
//!
//! STATE. A boot report is a *snapshot* of the keys currently held, not a stream
//! of events: the translator keeps the previously seen set to derive make/break
//! codes, and the modifier byte is diffed the same way.

use crate::drivers::{ps2_kbd, ps2_mouse};
use crate::sync::spinlock::Spinlock;
use core::sync::atomic::{AtomicU64, Ordering};

/// USB HID usage ids → PS/2 set-1 scancodes (without the `0xE0` prefix).
///
/// Sparse table by usage id; `0` means "no mapping" (media keys, power, and the
/// vendor-defined ranges a boot keyboard should not be sending anyway).
struct KeyMap {
    usage: u8,
    code: u8,
}

/// Prefixed entries carry the `0xE0` extension byte the PS/2 stream uses.
const fn k(usage: u8, code: u8) -> KeyMap {
    KeyMap { usage, code }
}

static MAP: [KeyMap; 102] = [
    k(0x04, 0x1E), // a
    k(0x05, 0x30), // b
    k(0x06, 0x2E), // c
    k(0x07, 0x20), // d
    k(0x08, 0x12), // e
    k(0x09, 0x21), // f
    k(0x0A, 0x22), // g
    k(0x0B, 0x23), // h
    k(0x0C, 0x17), // i
    k(0x0D, 0x24), // j
    k(0x0E, 0x25), // k
    k(0x0F, 0x26), // l
    k(0x10, 0x32), // m
    k(0x11, 0x31), // n
    k(0x12, 0x18), // o
    k(0x13, 0x19), // p
    k(0x14, 0x10), // q
    k(0x15, 0x13), // r
    k(0x16, 0x1F), // s
    k(0x17, 0x14), // t
    k(0x18, 0x16), // u
    k(0x19, 0x2F), // v
    k(0x1A, 0x11), // w
    k(0x1B, 0x2D), // x
    k(0x1C, 0x15), // y
    k(0x1D, 0x2C), // z
    k(0x1E, 0x02), // 1
    k(0x1F, 0x03), // 2
    k(0x20, 0x04), // 3
    k(0x21, 0x05), // 4
    k(0x22, 0x06), // 5
    k(0x23, 0x07), // 6
    k(0x24, 0x08), // 7
    k(0x25, 0x09), // 8
    k(0x26, 0x0A), // 9
    k(0x27, 0x0B), // 0
    k(0x28, 0x1C), // Enter
    k(0x29, 0x01), // Escape
    k(0x2A, 0x0E), // Backspace
    k(0x2B, 0x0F), // Tab
    k(0x2C, 0x39), // Space
    k(0x2D, 0x0C), // -
    k(0x2E, 0x0D), // =
    k(0x2F, 0x1A), // [
    k(0x30, 0x1B), // ]
    k(0x31, 0x2B), // backslash
    k(0x32, 0x2B), // (non-US # — same physical key)
    k(0x33, 0x27), // ;
    k(0x34, 0x28), // '
    k(0x35, 0x29), // `
    k(0x36, 0x33), // ,
    k(0x37, 0x34), // .
    k(0x38, 0x35), // /
    k(0x39, 0x3A), // CapsLock
    k(0x3A, 0x3B), // F1
    k(0x3B, 0x3C),
    k(0x3C, 0x3D),
    k(0x3D, 0x3E),
    k(0x3E, 0x3F),
    k(0x3F, 0x40),
    k(0x40, 0x41),
    k(0x41, 0x42),
    k(0x42, 0x43),
    k(0x43, 0x44),
    k(0x44, 0x57), // F11
    k(0x45, 0x58), // F12
    k(0x49, 0x52), // Insert      (E0)
    k(0x4A, 0x47), // Home        (E0)
    k(0x4B, 0x49), // PageUp      (E0)
    k(0x4C, 0x53), // Delete      (E0)
    k(0x4D, 0x4F), // End         (E0)
    k(0x4E, 0x51), // PageDown    (E0)
    k(0x4F, 0x4D), // Right       (E0)
    k(0x50, 0x4B), // Left        (E0)
    k(0x51, 0x50), // Down        (E0)
    k(0x52, 0x48), // Up          (E0)
    k(0x53, 0x45), // NumLock
    k(0x54, 0x35), // KP /
    k(0x55, 0x37), // KP *
    k(0x56, 0x4A), // KP -
    k(0x57, 0x4E), // KP +
    k(0x58, 0x1C), // KP Enter
    k(0x59, 0x4F), // KP 1
    k(0x5A, 0x50),
    k(0x5B, 0x51),
    k(0x5C, 0x4B),
    k(0x5D, 0x4C),
    k(0x5E, 0x4D),
    k(0x5F, 0x47),
    k(0x60, 0x48),
    k(0x61, 0x49),
    k(0x62, 0x52),
    k(0x63, 0x53), // KP .
    k(0x64, 0x56), // 102nd key (<> on ISO layouts)
    k(0x68, 0x3F), // F13 (uncommon)
    k(0x69, 0x40),
    k(0x6A, 0x41),
    k(0x6B, 0x42),
    k(0x6C, 0x43),
    k(0x6D, 0x44),
    k(0x6E, 0x57),
    k(0x6F, 0x58),
];

/// Usages that must be sent with the `0xE0` prefix in a PS/2 stream.
///
/// A real 8042 emits the prefix for the "extended" half of the keyboard —
/// arrows, navigation cluster, keypad Enter/slash, and the right-hand
/// modifiers. Getting this wrong is visible: `^C` from the right Ctrl, or an
/// arrow key, would arrive as an unrelated character.
fn is_extended(usage: u8) -> bool {
    matches!(usage, 0x49..=0x52 | 0x54 | 0x55 | 0x57 | 0x58 | 0x5E..=0x63 | 0x64)
}

fn lookup(usage: u8) -> Option<u8> {
    MAP.iter().find(|m| m.usage == usage).map(|m| m.code)
}

/// Keyboard state: the keys and modifiers held at the previous report.
struct KeyboardState {
    /// Usage ids currently reported as held (0 = none).
    keys: [u8; 6],
    modifiers: u8,
    /// Reports translated, for diagnostics.
    reports: u64,
    /// Keys that could not be mapped (so an operator can see why a key is dead).
    unmapped: u64,
}

static MOUSE_REPORTS: AtomicU64 = AtomicU64::new(0);

static KEYBOARD: Spinlock<KeyboardState> = Spinlock::new(KeyboardState {
    keys: [0; 6],
    modifiers: 0,
    reports: 0,
    unmapped: 0,
});

/// Modifier bits in byte 0 of a boot keyboard report, with the scancode each
/// one maps to. `ext` marks the right-hand variants (0xE0 prefix).
const MODIFIERS: [(u8, u8, bool); 8] = [
    (1 << 0, 0x1D, false), // Left Ctrl
    (1 << 1, 0x2A, false), // Left Shift
    (1 << 2, 0x38, false), // Left Alt
    (1 << 3, 0x5B, true),  // Left GUI
    (1 << 4, 0x1D, true),  // Right Ctrl
    (1 << 5, 0x36, false), // Right Shift
    (1 << 6, 0x38, true),  // Right Alt
    (1 << 7, 0x5C, true),  // Right GUI
];

fn emit(usage: u8, make: bool) {
    let Some(code) = lookup(usage) else {
        return;
    };
    let ext = is_extended(usage);
    if ext {
        ps2_kbd::inject_scancode(0xE0);
    }
    ps2_kbd::inject_scancode(if make { code } else { code | 0x80 });
}

/// Translate one boot keyboard report (8 bytes) into PS/2 scancodes.
fn feed_keyboard(report: &[u8]) {
    if report.len() < 8 {
        return;
    }
    let mods = report[0];
    let mut held = [0u8; 6];
    for (i, slot) in held.iter_mut().enumerate() {
        let usage = report[2 + i];
        // 0 = no key, 1 = ErrorRollOver (the keyboard is asked for more keys
        // than it can report); neither is a key press.
        *slot = if usage <= 1 { 0 } else { usage };
    }

    let mut state = KEYBOARD.lock();
    state.reports += 1;

    // Modifiers first, so a chord like Ctrl+C is delivered in order.
    for (bit, code, ext) in MODIFIERS {
        let was = state.modifiers & bit != 0;
        let now = mods & bit != 0;
        if was == now {
            continue;
        }
        if ext {
            ps2_kbd::inject_scancode(0xE0);
        }
        ps2_kbd::inject_scancode(if now { code } else { code | 0x80 });
    }
    state.modifiers = mods;

    // Break codes for keys that were held and are not any more.
    for i in 0..6 {
        let old = state.keys[i];
        if old == 0 || held.contains(&old) {
            continue;
        }
        if lookup(old).is_none() {
            continue;
        }
        emit(old, false);
    }
    // Make codes for keys that are newly held.
    for i in 0..6 {
        let new = held[i];
        if new == 0 || state.keys.contains(&new) {
            continue;
        }
        if lookup(new).is_none() {
            state.unmapped += 1;
            continue;
        }
        emit(new, true);
    }
    state.keys = held;
}

/// Translate one boot mouse report into the PS/2 packet the mouse driver
/// assembles, so the cursor moves through exactly the same path.
fn feed_mouse(report: &[u8]) {
    if report.len() < 3 {
        return;
    }
    MOUSE_REPORTS.fetch_add(1, Ordering::Relaxed);
    let buttons = report[0] & 0x07;
    let dx = report[1] as i8;
    // HID boot-mouse reports are Y-down (positive dy = towards the bottom of
    // the screen), while the PS/2 convention the decoder below implements is
    // Y-up (the driver computes `y - dy`). Flip the sign so a USB mouse moves
    // the cursor the same way a PS/2 one does.
    let dy = (report[2] as i8).wrapping_neg();
    // PS/2 packet: flags (bit 3 always set, bits 0-2 buttons, bits 4/5 the sign
    // of dx/dy), then the two deltas as 9-bit two's complement bytes.
    let flags = 0x08 | buttons | if dx < 0 { 0x10 } else { 0 } | if dy < 0 { 0x20 } else { 0 };
    ps2_mouse::inject_packet([flags, dx as u8, dy as u8]);
}

/// Feed one HID report. `protocol` is the interface's boot protocol byte:
/// 1 = keyboard, 2 = mouse, anything else is ignored (and counted by the caller
/// as a report that produced no input).
pub fn feed(report: &[u8], protocol: u8) {
    match protocol {
        1 => feed_keyboard(report),
        2 => feed_mouse(report),
        _ => {}
    }
}

/// `(keyboard reports, mouse reports, unmapped keys)` for diagnostics.
pub fn stats() -> (u64, u64, u64) {
    let k = KEYBOARD.lock();
    (k.reports, MOUSE_REPORTS.load(Ordering::Relaxed), k.unmapped)
}
