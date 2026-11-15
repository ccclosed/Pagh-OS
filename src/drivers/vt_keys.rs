//! Чистый переводчик VT-байтов (`/dev/vtinput`) в PS/2 set-1 скан-коды.
//!
//! Зачем не «просто отдать байты в stdin»: kernel-shell читает ввод только из
//! PS/2-кольца, и его line editor/хоткеи — это декодер скан-кодов. Терминал
//! (pagh-terminal) присылает нажатия как VT-байты (`keys.rs`): printable ASCII,
//! `\r`, `\x7f`, CSI-последовательности стрелок. Перевод в set-1 даёт shell
//! ровно тот же ввод, что и физическая клавиатура, не заводя второй путь
//! ввода и не ломая `^C`/историю/редактор.
//!
//! Модуль `core`-only и без состояния ядра: host-тесты гоняют тот же парсер.
//! UTF-8 > ASCII в v1 отбрасывается: kernel-VT хранит `u8`, а shell-line-editor
//! принимает ASCII; это задокументированное сужение ABI v1 (см.
//! `docs/desktop-b.md`).
//!
//! ## Последовательности
//!
//! * `\x1b[A/B/C/D` и `\x1bOA..OD` — стрелки; `H/F`, `1~..8~` — Home/End/
//!   Insert/Delete/PageUp/PageDown;
//! * одиночный `\x1b` в конце write — Esc; разорванная CSI-последовательность
//!   между write'ами сохраняется до финального байта;
//! * `Ctrl+A..Z` (0x01..0x1A) — Ctrl make + буква, как физическая клавиатура;
//! * printable ASCII — make+break, с Shift make/break для заглавных и символов
//!   верхнего регистра (US-раскладка, как у PS/2 set-1).

/// Максимум байт ESC-последовательности, которые держим между write'ами.
const PEND_MAX: usize = 8;

/// Государственный автомат: неполная ESC-последовательность переживает
/// границу `write(2)`.
#[derive(Clone, Copy, Debug)]
pub struct KeyParser {
    pending: [u8; PEND_MAX],
    len: u8,
}

impl Default for KeyParser {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyParser {
    pub const fn new() -> Self {
        KeyParser {
            pending: [0; PEND_MAX],
            len: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Скормить порцию VT-байтов, вызывая `sink` для каждого скан-кода.
    pub fn feed(&mut self, bytes: &[u8], sink: &mut dyn FnMut(u8)) {
        for &b in bytes {
            self.push(b, sink);
        }
        // Одиночный Esc в конце write — это нажатие Esc, а не начало CSI:
        // разорванную `ESC [` ждём (следующий write принесёт финальный байт).
        if self.len == 1 && self.pending[0] == 0x1B {
            self.emit_key(0x01, false, sink);
            self.len = 0;
        }
    }

    fn push(&mut self, b: u8, sink: &mut dyn FnMut(u8)) {
        if self.len == 0 {
            if b == 0x1B {
                self.pending[0] = b;
                self.len = 1;
            } else {
                Self::process_byte(b, sink);
            }
            return;
        }
        // Уже собираем ESC-последовательность.
        if self.len == 1 {
            if b == b'[' || b == b'O' {
                self.pending[1] = b;
                self.len = 2;
                return;
            }
            // Alt+<byte>/ESC<byte>: Esc как отдельная клавиша, затем символ.
            // Shell не использует Alt-модификатор, поэтому не теряем символ.
            self.emit_key(0x01, false, sink);
            self.len = 0;
            Self::process_byte(b, sink);
            return;
        }
        let idx = self.len as usize;
        if idx < PEND_MAX {
            self.pending[idx] = b;
            self.len += 1;
        }
        if (0x40..=0x7E).contains(&b) {
            self.emit_sequence(self.len as usize, sink);
            self.len = 0;
        } else if self.len as usize >= PEND_MAX {
            // Не финализировалась — отбросить, чтобы не заклинить ввод.
            self.emit_key(0x01, false, sink);
            self.len = 0;
        }
    }

    fn emit_sequence(&self, len: usize, sink: &mut dyn FnMut(u8)) {
        let seq = &self.pending[..len];
        if len < 3 {
            return;
        }
        let intro = seq[1];
        let last = seq[len - 1];
        // `ESC [ A` / `ESC O A`: одиночный финальный байт.
        if len == 3 {
            let key = match (intro, last) {
                (b'[', b'A') | (b'O', b'A') => Some((0x48, true)), // Up
                (b'[', b'B') | (b'O', b'B') => Some((0x50, true)), // Down
                (b'[', b'C') | (b'O', b'C') => Some((0x4D, true)), // Right
                (b'[', b'D') | (b'O', b'D') => Some((0x4B, true)), // Left
                (b'[', b'H') | (b'O', b'H') => Some((0x47, true)), // Home
                (b'[', b'F') | (b'O', b'F') => Some((0x4F, true)), // End
                _ => None,
            };
            if let Some((code, ext)) = key {
                self.emit_key(code, ext, sink);
            }
            return;
        }
        // `ESC [ <n> ~` — цифровые клавиши навигации.
        if intro == b'[' && last == b'~' {
            let digit = seq[2];
            let key = match digit {
                b'1' | b'7' => Some((0x47, true)), // Home
                b'2' => Some((0x52, true)),        // Insert
                b'3' => Some((0x53, true)),        // Delete
                b'4' | b'8' => Some((0x4F, true)), // End
                b'5' => Some((0x49, true)),        // PageUp
                b'6' => Some((0x51, true)),        // PageDown
                _ => None,
            };
            if let Some((code, ext)) = key {
                self.emit_key(code, ext, sink);
            }
        }
    }

    fn emit_key(&self, code: u8, extended: bool, sink: &mut dyn FnMut(u8)) {
        if extended {
            sink(0xE0);
        }
        sink(code);
        if extended {
            sink(0xE0);
        }
        sink(code | 0x80);
    }

    fn emit_shifted(&self, code: u8, sink: &mut dyn FnMut(u8)) {
        sink(0x2A); // Left Shift make
        self.emit_key(code, false, sink);
        sink(0xAA); // Left Shift break
    }

    fn emit_ctrl(&self, letter: u8, sink: &mut dyn FnMut(u8)) {
        sink(0x1D); // Left Ctrl make
        let code = ascii_scancode(letter).map(|(c, _)| c).unwrap_or(0x1E);
        self.emit_key(code, false, sink);
        sink(0x9D); // Left Ctrl break
    }

    fn process_byte(b: u8, sink: &mut dyn FnMut(u8)) {
        match b {
            0x1B => {
                // Одиночный Esc вне последовательности.
                sink(0x01);
                sink(0x81);
            }
            b'\r' | b'\n' => {
                sink(0x1C);
                sink(0x9C);
            }
            0x7F | 0x08 => {
                sink(0x0E);
                sink(0x8E);
            }
            b'\t' => {
                sink(0x0F);
                sink(0x8F);
            }
            0x01..=0x1A => {
                // Ctrl+A..Z. Uppercase letter for the scancode table.
                let letter = b + b'a' - 1;
                let p = KeyParser::new();
                p.emit_ctrl(letter, sink);
            }
            b' '..=b'~' => {
                let p = KeyParser::new();
                if let Some((code, shift)) = ascii_scancode(b) {
                    if shift {
                        p.emit_shifted(code, sink);
                    } else {
                        p.emit_key(code, false, sink);
                    }
                }
            }
            // UTF-8/управляющие > 0x7F в v1 не имеют PS/2-представления.
            _ => {}
        }
    }
}

/// set-1 коды букв `a..z` в алфавитном порядке: QWERTY-раскладка физических
/// клавиш не совпадает с порядком букв (a=0x1E, но q=0x10, z=0x2C).
const LETTER_SCANCODES: [u8; 26] = [
    0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18, 0x19,
    0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
];

/// US set-1: ASCII-байт → `(скан-код, требует Shift)`. `None` — символа нет на
/// клавиатуре (например, `\x00`).
pub fn ascii_scancode(ch: u8) -> Option<(u8, bool)> {
    let unshifted = |code: u8| Some((code, false));
    let shifted = |code: u8| Some((code, true));
    match ch {
        b'a'..=b'z' => unshifted(LETTER_SCANCODES[(ch - b'a') as usize]),
        b'A'..=b'Z' => shifted(LETTER_SCANCODES[(ch - b'A') as usize]),
        b'1'..=b'9' => unshifted(ch - b'1' + 0x02),
        b'0' => unshifted(0x0B),
        b'-' => unshifted(0x0C),
        b'=' => unshifted(0x0D),
        b'[' => unshifted(0x1A),
        b']' => unshifted(0x1B),
        b'\\' => unshifted(0x2B),
        b';' => unshifted(0x27),
        b'\'' => unshifted(0x28),
        b'`' => unshifted(0x29),
        b',' => unshifted(0x33),
        b'.' => unshifted(0x34),
        b'/' => unshifted(0x35),
        b' ' => unshifted(0x39),
        b'!' => shifted(0x02),
        b'@' => shifted(0x03),
        b'#' => shifted(0x04),
        b'$' => shifted(0x05),
        b'%' => shifted(0x06),
        b'^' => shifted(0x07),
        b'&' => shifted(0x08),
        b'*' => shifted(0x09),
        b'(' => shifted(0x0A),
        b')' => shifted(0x0B),
        b'_' => shifted(0x0C),
        b'+' => shifted(0x0D),
        b'{' => shifted(0x1A),
        b'}' => shifted(0x1B),
        b'|' => shifted(0x2B),
        b':' => shifted(0x27),
        b'"' => shifted(0x28),
        b'~' => shifted(0x29),
        b'<' => shifted(0x33),
        b'>' => shifted(0x34),
        b'?' => shifted(0x35),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(bytes: &[u8]) -> alloc::vec::Vec<u8> {
        let mut p = KeyParser::new();
        let mut out = alloc::vec::Vec::new();
        p.feed(bytes, &mut |b| out.push(b));
        out
    }

    #[test]
    fn plain_text_is_make_break_pairs() {
        // `echo` → 'e' 'c' 'h' 'o' make/break, no shift.
        let out = feed_all(b"echo");
        assert_eq!(
            out,
            alloc::vec![0x12, 0x92, 0x2E, 0xAE, 0x23, 0xA3, 0x18, 0x98]
        );
        // Enter at the end.
        let out = feed_all(b"\r");
        assert_eq!(out, alloc::vec![0x1C, 0x9C]);
    }

    #[test]
    fn uppercase_and_symbols_use_shift() {
        let out = feed_all(b"A");
        assert_eq!(out, alloc::vec![0x2A, 0x1E, 0x9E, 0xAA]);
        let out = feed_all(b"!");
        assert_eq!(out, alloc::vec![0x2A, 0x02, 0x82, 0xAA]);
        let out = feed_all(b" ");
        assert_eq!(out, alloc::vec![0x39, 0xB9]);
    }

    #[test]
    fn control_bytes_are_ctrl_chords() {
        // Ctrl+C = 0x03.
        let out = feed_all(&[0x03]);
        assert_eq!(out, alloc::vec![0x1D, 0x2E, 0xAE, 0x9D]);
        // Ctrl+D (EOF) = 0x04.
        let out = feed_all(&[0x04]);
        assert_eq!(out, alloc::vec![0x1D, 0x20, 0xA0, 0x9D]);
    }

    #[test]
    fn navigation_sequences_map_to_extended_keys() {
        assert_eq!(feed_all(b"\x1b[A"), alloc::vec![0xE0, 0x48, 0xE0, 0xC8]);
        assert_eq!(feed_all(b"\x1b[B"), alloc::vec![0xE0, 0x50, 0xE0, 0xD0]);
        assert_eq!(feed_all(b"\x1b[D"), alloc::vec![0xE0, 0x4B, 0xE0, 0xCB]);
        assert_eq!(feed_all(b"\x1b[3~"), alloc::vec![0xE0, 0x53, 0xE0, 0xD3]);
        assert_eq!(feed_all(b"\x1b[H"), alloc::vec![0xE0, 0x47, 0xE0, 0xC7]);
        assert_eq!(feed_all(b"\x1b[6~"), alloc::vec![0xE0, 0x51, 0xE0, 0xD1]);
    }

    #[test]
    fn split_csi_survives_write_boundary() {
        let mut p = KeyParser::new();
        let mut out = alloc::vec::Vec::new();
        p.feed(b"\x1b[", &mut |b| out.push(b));
        assert!(out.is_empty(), "incomplete CSI must not emit anything");
        p.feed(b"A", &mut |b| out.push(b));
        assert_eq!(out, alloc::vec![0xE0, 0x48, 0xE0, 0xC8]);
    }

    #[test]
    fn lone_escape_is_esc_at_end_of_write() {
        assert_eq!(feed_all(b"\x1b"), alloc::vec![0x01, 0x81]);
    }

    #[test]
    fn utf8_and_unknown_bytes_are_dropped() {
        // Cyrillic "Ж" (D0 96) не имеет PS/2 set-1 представления в v1.
        assert!(feed_all(&[0xD0, 0x96]).is_empty());
        // Неизвестная CSI-последовательность не должна печатать свои байты.
        assert!(feed_all(b"\x1b[99z").is_empty());
    }

    #[test]
    fn backspace_tab_and_del_map_to_keys() {
        assert_eq!(feed_all(&[0x7F]), alloc::vec![0x0E, 0x8E]);
        assert_eq!(feed_all(&[0x08]), alloc::vec![0x0E, 0x8E]);
        assert_eq!(feed_all(b"\t"), alloc::vec![0x0F, 0x8F]);
    }
}
