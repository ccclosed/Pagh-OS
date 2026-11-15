//! Linux evdev key codes and VT input encoding.
//!
//! PDP/1 delivers semantic input: evdev key code + pressed + modifier bits.
//! There is no UTF-8 text field, so printable characters are derived from a
//! US-QWERTY layout (Shift is applied; Caps Lock is not reported by the
//! server).  Applications that need a different layout should provide text via
//! a future protocol extension or an input method.
//!
//! This module turns events into the byte sequences a VT100-compatible
//! terminal sends to its PTY: arrows (normal or application cursor mode),
//! function keys, modified cursor keys, control characters, SGR mouse reports,
//! focus events and bracketed paste.

use crate::protocol::{buttons, mods, InputEvent, MAX_PAYLOAD};
use crate::screen::{MouseMode, Screen};

/// Linux input-event-codes used by the terminal.
pub mod evdev {
    pub const KEY_ESC: u32 = 1;
    pub const KEY_1: u32 = 2;
    pub const KEY_0: u32 = 11;
    pub const KEY_MINUS: u32 = 12;
    pub const KEY_EQUAL: u32 = 13;
    pub const KEY_BACKSPACE: u32 = 14;
    pub const KEY_TAB: u32 = 15;
    pub const KEY_Q: u32 = 16;
    pub const KEY_O: u32 = 24;
    pub const KEY_I: u32 = 23;
    pub const KEY_U: u32 = 22;
    pub const KEY_Y: u32 = 21;
    pub const KEY_T: u32 = 20;
    pub const KEY_R: u32 = 19;
    pub const KEY_E: u32 = 18;
    pub const KEY_W: u32 = 17;
    pub const KEY_P: u32 = 25;
    pub const KEY_LEFTBRACE: u32 = 26;
    pub const KEY_RIGHTBRACE: u32 = 27;
    pub const KEY_ENTER: u32 = 28;
    pub const KEY_LEFTCTRL: u32 = 29;
    pub const KEY_A: u32 = 30;
    pub const KEY_N: u32 = 49;
    pub const KEY_B: u32 = 48;
    pub const KEY_K: u32 = 37;
    pub const KEY_J: u32 = 36;
    pub const KEY_H: u32 = 35;
    pub const KEY_G: u32 = 34;
    pub const KEY_F: u32 = 33;
    pub const KEY_D: u32 = 32;
    pub const KEY_S: u32 = 31;
    pub const KEY_L: u32 = 38;
    pub const KEY_SEMICOLON: u32 = 39;
    pub const KEY_APOSTROPHE: u32 = 40;
    pub const KEY_GRAVE: u32 = 41;
    pub const KEY_LEFTSHIFT: u32 = 42;
    pub const KEY_BACKSLASH: u32 = 43;
    pub const KEY_Z: u32 = 44;
    pub const KEY_X: u32 = 45;
    pub const KEY_C: u32 = 46;
    pub const KEY_V: u32 = 47;
    pub const KEY_M: u32 = 50;
    pub const KEY_COMMA: u32 = 51;
    pub const KEY_DOT: u32 = 52;
    pub const KEY_SLASH: u32 = 53;
    pub const KEY_RIGHTSHIFT: u32 = 54;
    pub const KEY_LEFTALT: u32 = 56;
    pub const KEY_SPACE: u32 = 57;
    pub const KEY_CAPSLOCK: u32 = 58;
    pub const KEY_F1: u32 = 59;
    pub const KEY_F2: u32 = 60;
    pub const KEY_F3: u32 = 61;
    pub const KEY_F4: u32 = 62;
    pub const KEY_F5: u32 = 63;
    pub const KEY_F6: u32 = 64;
    pub const KEY_F7: u32 = 65;
    pub const KEY_F8: u32 = 66;
    pub const KEY_F9: u32 = 67;
    pub const KEY_F10: u32 = 68;
    pub const KEY_F11: u32 = 87;
    pub const KEY_F12: u32 = 88;
    pub const KEY_RIGHTCTRL: u32 = 97;
    pub const KEY_RIGHTALT: u32 = 100;
    pub const KEY_HOME: u32 = 102;
    pub const KEY_UP: u32 = 103;
    pub const KEY_PAGEUP: u32 = 104;
    pub const KEY_LEFT: u32 = 105;
    pub const KEY_RIGHT: u32 = 106;
    pub const KEY_END: u32 = 107;
    pub const KEY_DOWN: u32 = 108;
    pub const KEY_PAGEDOWN: u32 = 109;
    pub const KEY_INSERT: u32 = 110;
    pub const KEY_DELETE: u32 = 111;
    pub const KEY_LEFTMETA: u32 = 125;
    pub const KEY_RIGHTMETA: u32 = 126;
}

/// Terminal modes that affect input encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyModes {
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    pub mouse_mode: MouseMode,
    pub mouse_sgr: bool,
    pub focus_events: bool,
    /// Send DEL (0x7f) for Backspace; VT100 typically does.
    pub backspace_del: bool,
}

impl Default for KeyModes {
    fn default() -> KeyModes {
        KeyModes {
            app_cursor: false,
            bracketed_paste: false,
            mouse_mode: MouseMode::Off,
            mouse_sgr: false,
            focus_events: false,
            backspace_del: true,
        }
    }
}

impl KeyModes {
    pub fn from_screen(screen: &Screen) -> KeyModes {
        KeyModes {
            app_cursor: screen.app_cursor,
            bracketed_paste: screen.bracketed_paste,
            mouse_mode: screen.mouse_mode,
            mouse_sgr: screen.mouse_sgr,
            focus_events: screen.focus_events,
            backspace_del: true,
        }
    }
}

/// Encode any PDP/1 input event.  Keys, mouse, wheel; unknown/irrelevant
/// events return an empty vector.
pub fn encode_input(event: &InputEvent, modes: &KeyModes) -> Vec<u8> {
    match *event {
        InputEvent::Key {
            code,
            pressed,
            modifiers,
        } => encode_key(code, pressed, modifiers, modes),
        InputEvent::Motion {
            x,
            y,
            dx,
            dy,
            buttons,
        } => encode_mouse_motion(x, y, dx, dy, buttons, modes),
        InputEvent::Button {
            x,
            y,
            button,
            pressed,
            buttons,
        } => encode_mouse_button(x, y, button, pressed, buttons, modes),
        InputEvent::Axis {
            x,
            y,
            horizontal,
            vertical,
            buttons,
        } => encode_mouse_axis(x, y, horizontal, vertical, buttons, modes),
    }
}

/// xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
fn modifier_param(modifiers: u32) -> u8 {
    let mut m = 1;
    if modifiers & mods::SHIFT != 0 {
        m += 1;
    }
    if modifiers & mods::ALT != 0 {
        m += 2;
    }
    if modifiers & mods::CTRL != 0 {
        m += 4;
    }
    m
}

fn has_modifier(modifiers: u32) -> bool {
    modifiers & (mods::SHIFT | mods::ALT | mods::CTRL) != 0
}

/// Encode one key press.  Releases and pure modifier keys produce nothing.
pub fn encode_key(code: u32, pressed: bool, modifiers: u32, modes: &KeyModes) -> Vec<u8> {
    if !pressed {
        return Vec::new();
    }
    let shift = modifiers & mods::SHIFT != 0;
    let ctrl = modifiers & mods::CTRL != 0;
    let alt = modifiers & mods::ALT != 0;

    if let Some(sequence) = cursor_key(code, modifiers, modes) {
        return sequence.into_bytes();
    }

    match code {
        evdev::KEY_ENTER => {
            let mut out = Vec::new();
            if alt {
                out.push(0x1B);
            }
            out.push(b'\r');
            return out;
        }
        evdev::KEY_BACKSPACE => {
            let mut out = Vec::new();
            if alt {
                out.push(0x1B);
            }
            out.push(if modes.backspace_del { 0x7F } else { 0x08 });
            return out;
        }
        evdev::KEY_TAB => {
            if shift {
                return b"\x1b[Z".to_vec();
            }
            let mut out = Vec::new();
            if alt {
                out.push(0x1B);
            }
            out.push(b'\t');
            return out;
        }
        evdev::KEY_ESC => return vec![0x1B],
        evdev::KEY_LEFTCTRL
        | evdev::KEY_RIGHTCTRL
        | evdev::KEY_LEFTSHIFT
        | evdev::KEY_RIGHTSHIFT
        | evdev::KEY_LEFTALT
        | evdev::KEY_RIGHTALT
        | evdev::KEY_LEFTMETA
        | evdev::KEY_RIGHTMETA
        | evdev::KEY_CAPSLOCK => return Vec::new(),
        _ => {}
    }

    let Some(ch) = text_for_key(code, modifiers) else {
        return Vec::new();
    };
    if ctrl {
        if let Some(byte) = control_byte(ch) {
            let mut out = Vec::new();
            if alt {
                out.push(0x1B);
            }
            out.push(byte);
            return out;
        }
    }
    let mut text = String::new();
    if alt {
        text.push('\u{1b}');
    }
    text.push(ch);
    text.into_bytes()
}

/// Printable character produced by an evdev code + modifier bits on a US
/// keyboard.  Returns `None` for non-text keys.
pub fn text_for_key(code: u32, modifiers: u32) -> Option<char> {
    let shift = modifiers & mods::SHIFT != 0;
    // US-QWERTY rows.  Evdev codes are ordered by physical key position, not by
    // alphabet, so the rows are explicit tables (30..38 is a,s,d,f,g,h,j,k,l).
    const TOP: &[(u32, char)] = &[
        (16, 'q'),
        (17, 'w'),
        (18, 'e'),
        (19, 'r'),
        (20, 't'),
        (21, 'y'),
        (22, 'u'),
        (23, 'i'),
        (24, 'o'),
        (25, 'p'),
    ];
    const HOME: &[(u32, char)] = &[
        (30, 'a'),
        (31, 's'),
        (32, 'd'),
        (33, 'f'),
        (34, 'g'),
        (35, 'h'),
        (36, 'j'),
        (37, 'k'),
        (38, 'l'),
    ];
    const BOTTOM: &[(u32, char)] = &[
        (44, 'z'),
        (45, 'x'),
        (46, 'c'),
        (47, 'v'),
        (48, 'b'),
        (49, 'n'),
        (50, 'm'),
    ];
    for row in [TOP, HOME, BOTTOM] {
        if let Some((_, ch)) = row.iter().find(|(key, _)| *key == code) {
            return Some(if shift { ch.to_ascii_uppercase() } else { *ch });
        }
    }
    if (evdev::KEY_1..=evdev::KEY_0).contains(&code) {
        let digit = if code == evdev::KEY_0 {
            '0'
        } else {
            (b'1' + (code - evdev::KEY_1) as u8) as char
        };
        let shifted = match digit {
            '1' => '!',
            '2' => '@',
            '3' => '#',
            '4' => '$',
            '5' => '%',
            '6' => '^',
            '7' => '&',
            '8' => '*',
            '9' => '(',
            '0' => ')',
            _ => digit,
        };
        return Some(if shift { shifted } else { digit });
    }
    let (plain, shifted) = match code {
        evdev::KEY_MINUS => ('-', '_'),
        evdev::KEY_EQUAL => ('=', '+'),
        evdev::KEY_LEFTBRACE => ('[', '{'),
        evdev::KEY_RIGHTBRACE => (']', '}'),
        evdev::KEY_BACKSLASH => ('\\', '|'),
        evdev::KEY_SEMICOLON => (';', ':'),
        evdev::KEY_APOSTROPHE => ('\'', '"'),
        evdev::KEY_GRAVE => ('`', '~'),
        evdev::KEY_COMMA => (',', '<'),
        evdev::KEY_DOT => ('.', '>'),
        evdev::KEY_SLASH => ('/', '?'),
        evdev::KEY_SPACE => (' ', ' '),
        _ => return None,
    };
    Some(if shift { shifted } else { plain })
}

fn cursor_key(code: u32, modifiers: u32, modes: &KeyModes) -> Option<String> {
    let modified = has_modifier(modifiers);
    let fkey14 = (evdev::KEY_F1..=evdev::KEY_F4).contains(&code);
    let (final_byte, tilde, number) = match code {
        evdev::KEY_UP => ('A', false, 1),
        evdev::KEY_DOWN => ('B', false, 1),
        evdev::KEY_RIGHT => ('C', false, 1),
        evdev::KEY_LEFT => ('D', false, 1),
        evdev::KEY_HOME => ('H', false, 1),
        evdev::KEY_END => ('F', false, 1),
        evdev::KEY_INSERT => ('~', true, 2),
        evdev::KEY_DELETE => ('~', true, 3),
        evdev::KEY_PAGEUP => ('~', true, 5),
        evdev::KEY_PAGEDOWN => ('~', true, 6),
        evdev::KEY_F1 => ('P', false, 1),
        evdev::KEY_F2 => ('Q', false, 1),
        evdev::KEY_F3 => ('R', false, 1),
        evdev::KEY_F4 => ('S', false, 1),
        evdev::KEY_F5 => ('~', true, 15),
        evdev::KEY_F6 => ('~', true, 17),
        evdev::KEY_F7 => ('~', true, 18),
        evdev::KEY_F8 => ('~', true, 19),
        evdev::KEY_F9 => ('~', true, 20),
        evdev::KEY_F10 => ('~', true, 21),
        evdev::KEY_F11 => ('~', true, 23),
        evdev::KEY_F12 => ('~', true, 24),
        _ => return None,
    };
    let m = modifier_param(modifiers);
    if tilde {
        Some(if modified {
            format!("\x1b[{number};{m}~")
        } else {
            format!("\x1b[{number}~")
        })
    } else if fkey14 && !modified {
        Some(format!("\x1bO{final_byte}"))
    } else if modes.app_cursor && !modified {
        Some(format!("\x1bO{final_byte}"))
    } else if modified {
        Some(format!("\x1b[1;{m}{final_byte}"))
    } else {
        Some(format!("\x1b[{final_byte}"))
    }
}

fn control_byte(ch: char) -> Option<u8> {
    let ch = ch.to_ascii_lowercase();
    match ch {
        'a'..='z' => Some(ch as u8 - b'a' + 1),
        '@' | ' ' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        '2' => Some(0x00),
        '3' => Some(0x1b),
        '4' => Some(0x1c),
        '5' => Some(0x1d),
        '6' => Some(0x1e),
        '7' => Some(0x1f),
        '8' => Some(0x7f),
        _ => None,
    }
}

fn held_button(buttons_mask: u32) -> u16 {
    if buttons_mask & buttons::MASK_LEFT != 0 {
        0
    } else if buttons_mask & buttons::MASK_MIDDLE != 0 {
        1
    } else if buttons_mask & buttons::MASK_RIGHT != 0 {
        2
    } else {
        3
    }
}

fn encode_pointer(code: u16, x: i32, y: i32, sgr: bool, release: bool) -> Vec<u8> {
    if sgr {
        format!(
            "\x1b[<{};{};{}{}",
            code,
            x.max(0) + 1,
            y.max(0) + 1,
            if release { 'm' } else { 'M' }
        )
        .into_bytes()
    } else {
        let b = (32 + i32::from(code)).clamp(32, 255) as u8;
        let x = (32 + x.max(0) + 1).clamp(32, 255) as u8;
        let y = (32 + y.max(0) + 1).clamp(32, 255) as u8;
        let mut out = vec![0x1b, b'[', b'M'];
        out.extend_from_slice(&[b, x, y]);
        out
    }
}

fn button_code(button: u32) -> u16 {
    match button {
        buttons::LEFT => 0,
        buttons::MIDDLE => 1,
        buttons::RIGHT => 2,
        _ => 0,
    }
}

/// Encode a mouse button event honouring DECSET 1000/1002/1003 and 1006.
pub fn encode_mouse_button(
    x: i32,
    y: i32,
    button: u32,
    pressed: bool,
    _buttons: u32,
    modes: &KeyModes,
) -> Vec<u8> {
    if modes.mouse_mode == MouseMode::Off {
        return Vec::new();
    }
    let mut code = button_code(button);
    if !pressed && !modes.mouse_sgr {
        // X10 release is button 3; SGR keeps the button number and uses `m`.
        code = 3;
    }
    encode_pointer(code, x, y, modes.mouse_sgr, !pressed)
}

/// Encode pointer motion.  DECSET 1003 reports all motion; 1002 only while a
/// button is held.
pub fn encode_mouse_motion(
    x: i32,
    y: i32,
    _dx: i32,
    _dy: i32,
    buttons_mask: u32,
    modes: &KeyModes,
) -> Vec<u8> {
    if modes.mouse_mode == MouseMode::Off {
        return Vec::new();
    }
    if modes.mouse_mode == MouseMode::Drag && buttons_mask == 0 {
        return Vec::new();
    }
    let code = 32 + held_button(buttons_mask);
    encode_pointer(code, x, y, modes.mouse_sgr, false)
}

/// Encode a scroll wheel event.  Positive vertical scrolls down (xterm 65).
pub fn encode_mouse_axis(
    x: i32,
    y: i32,
    horizontal: i32,
    vertical: i32,
    _buttons: u32,
    modes: &KeyModes,
) -> Vec<u8> {
    if modes.mouse_mode == MouseMode::Off {
        return Vec::new();
    }
    if vertical != 0 {
        let code = if vertical < 0 { 64 } else { 65 };
        return encode_pointer(code, x, y, modes.mouse_sgr, false);
    }
    if horizontal != 0 {
        let code = if horizontal < 0 { 66 } else { 67 };
        return encode_pointer(code, x, y, modes.mouse_sgr, false);
    }
    Vec::new()
}

/// Focus in/out reports enabled by DECSET 1004.
pub fn encode_focus(focused: bool, modes: &KeyModes) -> Vec<u8> {
    if !modes.focus_events {
        return Vec::new();
    }
    if focused {
        b"\x1b[I".to_vec()
    } else {
        b"\x1b[O".to_vec()
    }
}

/// Wrap pasted text in bracketed-paste markers when DECSET 2004 is active.
pub fn bracketed_paste(text: &str, modes: &KeyModes) -> Vec<u8> {
    if modes.bracketed_paste {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out.truncate(out.len().min(MAX_PAYLOAD));
        out
    } else {
        text.as_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: u32, modifiers: u32) -> Vec<u8> {
        encode_key(code, true, modifiers, &KeyModes::default())
    }

    #[test]
    fn printable_and_shifted() {
        assert_eq!(key(evdev::KEY_A, 0), b"a");
        assert_eq!(key(evdev::KEY_S, 0), b"s");
        assert_eq!(key(evdev::KEY_D, 0), b"d");
        assert_eq!(key(evdev::KEY_C, 0), b"c");
        assert_eq!(key(evdev::KEY_Q, 0), b"q");
        assert_eq!(key(evdev::KEY_W, 0), b"w");
        assert_eq!(key(evdev::KEY_Z, 0), b"z");
        assert_eq!(key(evdev::KEY_M, 0), b"m");
        assert_eq!(key(evdev::KEY_A, mods::SHIFT), b"A");
        assert_eq!(key(evdev::KEY_1, mods::SHIFT), b"!");
        assert_eq!(key(evdev::KEY_LEFTBRACE, mods::SHIFT), b"{");
        assert_eq!(key(evdev::KEY_SPACE, 0), b" ");
    }

    #[test]
    fn control_and_alt() {
        assert_eq!(key(evdev::KEY_A, mods::CTRL), vec![0x01]);
        assert_eq!(key(evdev::KEY_C, mods::CTRL), vec![0x03]);
        assert_eq!(key(evdev::KEY_SPACE, mods::CTRL), vec![0x00]);
        assert_eq!(key(evdev::KEY_A, mods::ALT), b"\x1ba");
        assert_eq!(key(evdev::KEY_A, mods::CTRL | mods::ALT), b"\x1b\x01");
        assert_eq!(
            encode_key(evdev::KEY_ENTER, true, 0, &KeyModes::default()),
            b"\r"
        );
        assert_eq!(
            encode_key(evdev::KEY_BACKSPACE, true, 0, &KeyModes::default()),
            vec![0x7f]
        );
    }

    #[test]
    fn cursor_keys_normal_and_application() {
        let mut modes = KeyModes::default();
        assert_eq!(key(evdev::KEY_UP, 0), b"\x1b[A");
        assert_eq!(key(evdev::KEY_LEFT, mods::CTRL), b"\x1b[1;5D");
        modes.app_cursor = true;
        assert_eq!(encode_key(evdev::KEY_UP, true, 0, &modes), b"\x1bOA");
        assert_eq!(
            encode_key(evdev::KEY_UP, true, mods::SHIFT, &modes),
            b"\x1b[1;2A"
        );
        assert_eq!(encode_key(evdev::KEY_HOME, true, 0, &modes), b"\x1bOH");
        assert_eq!(encode_key(evdev::KEY_F1, true, 0, &modes), b"\x1bOP");
        assert_eq!(encode_key(evdev::KEY_F5, true, 0, &modes), b"\x1b[15~");
        assert_eq!(
            encode_key(evdev::KEY_DELETE, true, mods::ALT, &modes),
            b"\x1b[3;3~"
        );
        assert_eq!(
            encode_key(evdev::KEY_TAB, true, mods::SHIFT, &modes),
            b"\x1b[Z"
        );
    }

    #[test]
    fn releases_and_modifiers_emit_nothing() {
        let modes = KeyModes::default();
        assert!(encode_key(evdev::KEY_A, false, 0, &modes).is_empty());
        assert!(encode_key(evdev::KEY_LEFTCTRL, true, 0, &modes).is_empty());
        assert!(encode_key(evdev::KEY_LEFTSHIFT, true, mods::SHIFT, &modes).is_empty());
    }

    #[test]
    fn sgr_mouse_wheel_and_legacy_release() {
        let mut modes = KeyModes {
            mouse_mode: MouseMode::Click,
            mouse_sgr: true,
            ..KeyModes::default()
        };
        assert_eq!(
            encode_mouse_button(10, 20, buttons::LEFT, true, 1, &modes),
            b"\x1b[<0;11;21M"
        );
        assert_eq!(
            encode_mouse_button(10, 20, buttons::LEFT, false, 0, &modes),
            b"\x1b[<0;11;21m"
        );
        assert_eq!(encode_mouse_axis(0, 0, 0, 1, 0, &modes), b"\x1b[<65;1;1M");
        assert_eq!(encode_mouse_axis(0, 0, 0, -1, 0, &modes), b"\x1b[<64;1;1M");
        modes.mouse_sgr = false;
        assert_eq!(
            encode_mouse_button(10, 20, buttons::LEFT, true, 1, &modes),
            vec![0x1b, b'[', b'M', 32, 43, 53]
        );
        assert_eq!(
            encode_mouse_button(10, 20, buttons::LEFT, false, 0, &modes),
            vec![0x1b, b'[', b'M', 35, 43, 53]
        );
    }

    #[test]
    fn drag_and_motion_modes() {
        let modes = KeyModes {
            mouse_mode: MouseMode::Drag,
            mouse_sgr: true,
            ..KeyModes::default()
        };
        assert!(encode_mouse_motion(1, 2, 0, 0, 0, &modes).is_empty());
        assert_eq!(
            encode_mouse_motion(1, 2, 0, 0, buttons::MASK_LEFT, &modes),
            b"\x1b[<32;2;3M"
        );
    }

    #[test]
    fn focus_and_paste() {
        let modes = KeyModes {
            focus_events: true,
            bracketed_paste: true,
            ..KeyModes::default()
        };
        assert_eq!(encode_focus(true, &modes), b"\x1b[I");
        assert_eq!(bracketed_paste("hi", &modes), b"\x1b[200~hi\x1b[201~");
    }

    #[test]
    fn encode_input_dispatches_every_kind() {
        let modes = KeyModes {
            mouse_mode: MouseMode::Click,
            mouse_sgr: true,
            ..KeyModes::default()
        };
        assert_eq!(
            encode_input(
                &InputEvent::Key {
                    code: evdev::KEY_A,
                    pressed: true,
                    modifiers: 0,
                },
                &modes
            ),
            b"a"
        );
        assert_eq!(
            encode_input(
                &InputEvent::Button {
                    x: 0,
                    y: 0,
                    button: buttons::LEFT,
                    pressed: true,
                    buttons: 1,
                },
                &modes
            ),
            b"\x1b[<0;1;1M"
        );
        assert_eq!(
            encode_input(
                &InputEvent::Axis {
                    x: 0,
                    y: 0,
                    horizontal: 0,
                    vertical: 1,
                    buttons: 0,
                },
                &modes
            ),
            b"\x1b[<65;1;1M"
        );
    }
}
