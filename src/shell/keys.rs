//! Scancode decoding: `KeyEvent` and the `0xE0`-prefixed extended-scancode
//! state machine.
//!
//! This module owns PS/2 Set 1 scancode decoding for the shell. The base
//! make-code → ASCII table (`scancode_to_ascii`) lives here, and the stateful
//! [`Decoder`] layers shift tracking and the `0xE0` extended-prefix navigation
//! keys on top of it. The decoder is consumed by the interactive read loop in
//! a later task; until then it is allowed to be dead code.

/// A decoded keyboard event.
///
/// Closed enum of the keys the line editor understands. Printable input is
/// carried by [`KeyEvent::Char`]; everything else is an editing/navigation
/// action. Extended (`0xE0`-prefixed) scancodes only ever decode to the
/// navigation variants — never to `Char` (R1.7).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Char(char),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Up,
    Down,
    Tab,
    Enter,
}

/// Stateful PS/2 Set 1 scancode decoder.
///
/// Holds the minimal state needed to interpret the raw scancode byte stream:
/// `extended` is set for the one byte following a standalone `0xE0` prefix, and
/// `shift` tracks the current Shift modifier across make/break codes. The
/// decoder never panics on any input byte (Properties 25 & 27).
#[allow(dead_code)]
pub struct Decoder {
    extended: bool,
    shift: bool,
}

#[allow(dead_code)]
impl Decoder {
    /// Create a fresh decoder with no pending prefix and Shift released.
    pub fn new() -> Self {
        Decoder {
            extended: false,
            shift: false,
        }
    }

    /// Feed one raw scancode byte.
    ///
    /// Returns `Some(event)` for a complete key, or `None` while mid-prefix or
    /// for a consumed modifier/break code. This is total over all `u8` inputs
    /// and never panics.
    pub fn feed(&mut self, scancode: u8) -> Option<KeyEvent> {
        // Shift make/break: track the modifier, emit no event. Handled before
        // the extended path because Shift codes are never E0-prefixed here.
        match scancode {
            0x2A | 0x36 => {
                self.shift = true;
                return None;
            }
            0xAA | 0xB6 => {
                self.shift = false;
                return None;
            }
            _ => {}
        }

        // A standalone 0xE0 prefix: arm the extended flag, yield nothing yet.
        if scancode == 0xE0 {
            self.extended = true;
            return None;
        }

        if self.extended {
            // Always clear the prefix flag after the byte following 0xE0 so a
            // stray prefix can never wedge the decoder (R11.2).
            self.extended = false;

            // Extended break code (key release): consume and ignore.
            if scancode >= 0x80 {
                return None;
            }

            // Extended make-codes decode to navigation keys only. Unknown
            // extended codes produce no event. GUARANTEE: never a Char (R1.7).
            return match scancode {
                0x4B => Some(KeyEvent::Left),
                0x4D => Some(KeyEvent::Right),
                0x47 => Some(KeyEvent::Home),
                0x4F => Some(KeyEvent::End),
                0x53 => Some(KeyEvent::Delete),
                0x48 => Some(KeyEvent::Up),
                0x50 => Some(KeyEvent::Down),
                _ => None,
            };
        }

        // Base (non-extended) make-codes.

        // Base break codes (key release): ignore.
        if scancode >= 0x80 {
            return None;
        }

        match scancode {
            0x1C => Some(KeyEvent::Enter),
            0x0E => Some(KeyEvent::Backspace),
            0x0F => Some(KeyEvent::Tab),
            _ => {
                // Map the remaining base make-codes via the Set 1 ASCII table.
                // Enter ('\n') and Tab ('\t') are handled above as their own
                // events, so guard against the table also producing them.
                match scancode_to_ascii(scancode, self.shift) {
                    Some(ch) if ch != '\n' && ch != '\t' => Some(KeyEvent::Char(ch)),
                    _ => None,
                }
            }
        }
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Simple scancode to ASCII mapping (US keyboard layout, PS/2 Set 1 base
/// make-codes). Owned here since `keys.rs` owns scancode decoding; the shell
/// read loop calls it via `keys::scancode_to_ascii`.
pub(super) fn scancode_to_ascii(scancode: u8, shift: bool) -> Option<char> {
    match scancode {
        0x02 => Some(if shift { '!' } else { '1' }),
        0x03 => Some(if shift { '@' } else { '2' }),
        0x04 => Some(if shift { '#' } else { '3' }),
        0x05 => Some(if shift { '$' } else { '4' }),
        0x06 => Some(if shift { '%' } else { '5' }),
        0x07 => Some(if shift { '^' } else { '6' }),
        0x08 => Some(if shift { '&' } else { '7' }),
        0x09 => Some(if shift { '*' } else { '8' }),
        0x0A => Some(if shift { '(' } else { '9' }),
        0x0B => Some(if shift { ')' } else { '0' }),
        0x0C => Some(if shift { '_' } else { '-' }),
        0x0D => Some(if shift { '+' } else { '=' }),
        0x10 => Some(if shift { 'Q' } else { 'q' }),
        0x11 => Some(if shift { 'W' } else { 'w' }),
        0x12 => Some(if shift { 'E' } else { 'e' }),
        0x13 => Some(if shift { 'R' } else { 'r' }),
        0x14 => Some(if shift { 'T' } else { 't' }),
        0x15 => Some(if shift { 'Y' } else { 'y' }),
        0x16 => Some(if shift { 'U' } else { 'u' }),
        0x17 => Some(if shift { 'I' } else { 'i' }),
        0x18 => Some(if shift { 'O' } else { 'o' }),
        0x19 => Some(if shift { 'P' } else { 'p' }),
        0x1A => Some(if shift { '{' } else { '[' }),
        0x1B => Some(if shift { '}' } else { ']' }),
        0x1C => Some('\n'), // Enter
        0x1E => Some(if shift { 'A' } else { 'a' }),
        0x1F => Some(if shift { 'S' } else { 's' }),
        0x20 => Some(if shift { 'D' } else { 'd' }),
        0x21 => Some(if shift { 'F' } else { 'f' }),
        0x22 => Some(if shift { 'G' } else { 'g' }),
        0x23 => Some(if shift { 'H' } else { 'h' }),
        0x24 => Some(if shift { 'J' } else { 'j' }),
        0x25 => Some(if shift { 'K' } else { 'k' }),
        0x26 => Some(if shift { 'L' } else { 'l' }),
        0x27 => Some(if shift { ':' } else { ';' }),
        0x28 => Some(if shift { '"' } else { '\'' }),
        0x29 => Some(if shift { '~' } else { '`' }),
        0x2B => Some(if shift { '|' } else { '\\' }),
        0x2C => Some(if shift { 'Z' } else { 'z' }),
        0x2D => Some(if shift { 'X' } else { 'x' }),
        0x2E => Some(if shift { 'C' } else { 'c' }),
        0x2F => Some(if shift { 'V' } else { 'v' }),
        0x30 => Some(if shift { 'B' } else { 'b' }),
        0x31 => Some(if shift { 'N' } else { 'n' }),
        0x32 => Some(if shift { 'M' } else { 'm' }),
        0x33 => Some(if shift { '<' } else { ',' }),
        0x34 => Some(if shift { '>' } else { '.' }),
        0x35 => Some(if shift { '?' } else { '/' }),
        0x39 => Some(' '), // Space
        _ => None,
    }
}
