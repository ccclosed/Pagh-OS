//! Чистый кодек evdev-совместимого ввода: `struct input_event`, PS/2 set-1
//! скан-коды → Linux keycodes и раскладка относительного указателя.
//!
//! Модуль `core`-only, поэтому host-tests включают его через `#[path]` и
//! проверяют ту же таблицу/кодирование, что компилирует ядро. Очередь событий и
//! привязка к железу живут в [`super::evdev`]: здесь нет ни спинлоков, ни
//! `crate::`, только байты ABI.
//!
//! Формат события — Linux x86_64 `struct input_event` (24 байта):
//! `struct timeval { i64 tv_sec; i64 tv_usec; }`, затем `u16 type`, `u16 code`,
//! `i32 value`. Проверено `gcc -offsetof(linux/input.h)`: 0/8/16/18/20.

/// `EV_SYN` — разделитель пакета событий.
pub const EV_SYN: u16 = 0x00;
/// `EV_KEY` — нажатие/отпускание клавиши или кнопки.
pub const EV_KEY: u16 = 0x01;
/// `EV_REL` — относительное движение указателя.
pub const EV_REL: u16 = 0x02;
/// `SYN_REPORT` — конец пакета (после него readers забирают события).
pub const SYN_REPORT: u16 = 0;
/// `REL_X` — горизонтальная дельта (вправо положительна).
pub const REL_X: u16 = 0x00;
/// `REL_Y` — вертикальная дельта (вниз положительна, как у экрана).
pub const REL_Y: u16 = 0x01;
/// `REL_WHEEL` — вертикальное колесо (вверх положительно, как в Linux).
pub const REL_WHEEL: u16 = 0x08;
/// `BTN_LEFT` / `BTN_RIGHT` / `BTN_MIDDLE` — кнопки мыши.
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

/// `sizeof(struct input_event)` на x86_64.
pub const INPUT_EVENT_SIZE: usize = 24;

/// Один декодированный пакет из PS/2-потока — ровно то, что уходит в очередь.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Event {
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

impl Event {
    pub const fn new(type_: u16, code: u16, value: i32) -> Self {
        Event { type_, code, value }
    }
}

/// Закодировать одно событие в 24-байтный `struct input_event`.
pub fn encode_input_event(sec: u64, usec: u64, ev: Event) -> [u8; INPUT_EVENT_SIZE] {
    let mut b = [0u8; INPUT_EVENT_SIZE];
    b[0..8].copy_from_slice(&(sec as i64).to_le_bytes());
    b[8..16].copy_from_slice(&(usec as i64).to_le_bytes());
    b[16..18].copy_from_slice(&ev.type_.to_le_bytes());
    b[18..20].copy_from_slice(&ev.code.to_le_bytes());
    b[20..24].copy_from_slice(&ev.value.to_le_bytes());
    b
}

/// Linux keycode для PS/2 set-1 скан-кода (без бита release).
///
/// Обычный блок set-1 совпадает с keycodes Linux (`0x1E` — `KEY_A`, `0x02` —
/// `KEY_1`, `0x3B` — `KEY_F1`), поэтому общая таблица не нужна. Отдельно
/// переводятся только extended-клавиши (`E0`), где номера расходятся: правый
/// Ctrl — 97, а не 29; стрелки — 103..109, а не 72..80; `E0 1C` — `KEY_KPENTER`
/// (96), а не обычный Enter.
///
/// Возвращает `None` для префиксов/служебных байтов, которые не являются
/// keycode (fake shift от PrintScreen, Pause-последовательность, ACK `0xFA`):
/// они не должны превращаться в выдуманное нажатие.
pub fn set1_keycode(extended: bool, code: u8) -> Option<u16> {
    if code == 0 {
        return None; // buffer overrun
    }
    if !extended {
        // Set-1 не любое число: 0xFA — ACK, 0x00 — overrun, а «коды» вроде
        // 0x7A/0x59 не являются клавишами и не должны становиться keycode.
        // Диапазоны ниже — реальные клавиши set-1 (Esc..F10, NumLock,
        // ScrollLock, цифровой блок, 102nd и F11/F12).
        let valid = (0x01..=0x44).contains(&code)
            || code == 0x45
            || code == 0x46
            || (0x47..=0x53).contains(&code)
            || (0x56..=0x58).contains(&code);
        return if valid { Some(code as u16) } else { None };
    }
    Some(match code {
        0x1C => 96,  // KEY_KPENTER
        0x1D => 97,  // KEY_RIGHTCTRL
        0x35 => 98,  // KEY_KPSLASH
        0x38 => 100, // KEY_RIGHTALT
        0x47 => 102, // KEY_HOME
        0x48 => 103, // KEY_UP
        0x49 => 104, // KEY_PAGEUP
        0x4B => 105, // KEY_LEFT
        0x4D => 106, // KEY_RIGHT
        0x4F => 107, // KEY_END
        0x50 => 108, // KEY_DOWN
        0x51 => 109, // KEY_PAGEDOWN
        0x52 => 110, // KEY_INSERT
        0x53 => 111, // KEY_DELETE
        0x5B => 125, // KEY_LEFTMETA
        0x5C => 126, // KEY_RIGHTMETA
        0x5D => 127, // KEY_COMPOSE
        // E0 2A/E0 36 — не клавиши, а fake shift вокруг PrintScreen.
        0x2A | 0x36 => return None,
        _ => return None,
    })
}

/// Государственный автомат потока set-1: собирает `E0`/`E1`-префиксы и
/// превращает make/break-коды в [`Event`] `EV_KEY`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScancodeDecoder {
    extended: bool,
    /// Сколько байт ещё проглотить (Pause: `E1 1D 45 E1 9D C5`).
    skip: u8,
}

impl ScancodeDecoder {
    pub const fn new() -> Self {
        ScancodeDecoder {
            extended: false,
            skip: 0,
        }
    }

    /// Сбросить недособранное состояние (между переоткрытиями устройства или в
    /// selftest, чтобы прошлый тест не оставил висящий `E0`).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Скормить один байт PS/2 set-1. Возвращает событие `EV_KEY` для настоящей
    /// клавиши; префиксы и служебные байты возвращают `None`.
    pub fn feed(&mut self, byte: u8) -> Option<Event> {
        if self.skip != 0 {
            self.skip -= 1;
            return None;
        }
        match byte {
            0xE0 => {
                self.extended = true;
                return None;
            }
            0xE1 => {
                // Pause/Break: E1 1D 45 (make) и E1 9D C5 (break) — три байта
                // на последовательность, ни один из них не keycode.
                self.extended = false;
                self.skip = 2;
                return None;
            }
            _ => {}
        }
        let extended = core::mem::replace(&mut self.extended, false);
        let released = byte & 0x80 != 0;
        let code = byte & 0x7F;
        let keycode = set1_keycode(extended, code)?;
        Some(Event::new(EV_KEY, keycode, if released { 0 } else { 1 }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> alloc::vec::Vec<Event> {
        let mut d = ScancodeDecoder::new();
        bytes.iter().filter_map(|b| d.feed(*b)).collect()
    }

    #[test]
    fn input_event_layout_is_24_bytes() {
        let b = encode_input_event(
            0x0102_0304_0506_0708,
            0x1112_1314_1516_1718,
            Event::new(EV_KEY, 30, 1),
        );
        assert_eq!(b.len(), 24);
        assert_eq!(
            i64::from_le_bytes(b[0..8].try_into().unwrap()),
            0x0102_0304_0506_0708
        );
        assert_eq!(
            i64::from_le_bytes(b[8..16].try_into().unwrap()),
            0x1112_1314_1516_1718
        );
        assert_eq!(u16::from_le_bytes(b[16..18].try_into().unwrap()), EV_KEY);
        assert_eq!(u16::from_le_bytes(b[18..20].try_into().unwrap()), 30);
        assert_eq!(i32::from_le_bytes(b[20..24].try_into().unwrap()), 1);
    }

    #[test]
    fn letters_and_digits_match_linux_keycodes() {
        // 'a' make/break → KEY_A 30.
        assert_eq!(decode(&[0x1E]), alloc::vec![Event::new(EV_KEY, 30, 1)]);
        assert_eq!(decode(&[0x9E]), alloc::vec![Event::new(EV_KEY, 30, 0)]);
        // '1' → KEY_1 2, Enter → 28, Esc → 1, Space → 57.
        assert_eq!(decode(&[0x02]), alloc::vec![Event::new(EV_KEY, 2, 1)]);
        assert_eq!(decode(&[0x1C]), alloc::vec![Event::new(EV_KEY, 28, 1)]);
        assert_eq!(decode(&[0x01]), alloc::vec![Event::new(EV_KEY, 1, 1)]);
        assert_eq!(decode(&[0x39]), alloc::vec![Event::new(EV_KEY, 57, 1)]);
        // Left Ctrl / Shift / Alt.
        assert_eq!(decode(&[0x1D]), alloc::vec![Event::new(EV_KEY, 29, 1)]);
        assert_eq!(decode(&[0x2A]), alloc::vec![Event::new(EV_KEY, 42, 1)]);
        assert_eq!(decode(&[0x38]), alloc::vec![Event::new(EV_KEY, 56, 1)]);
    }

    #[test]
    fn extended_keys_use_linux_extended_codes() {
        // E0 4B — Left; E0 CB — release.
        assert_eq!(
            decode(&[0xE0, 0x4B]),
            alloc::vec![Event::new(EV_KEY, 105, 1)]
        );
        assert_eq!(
            decode(&[0xE0, 0xCB]),
            alloc::vec![Event::new(EV_KEY, 105, 0)]
        );
        // E0 1D — Right Ctrl (97), E0 1C — Keypad Enter (96), E0 35 — KP / (98).
        assert_eq!(
            decode(&[0xE0, 0x1D]),
            alloc::vec![Event::new(EV_KEY, 97, 1)]
        );
        assert_eq!(
            decode(&[0xE0, 0x1C]),
            alloc::vec![Event::new(EV_KEY, 96, 1)]
        );
        assert_eq!(
            decode(&[0xE0, 0x35]),
            alloc::vec![Event::new(EV_KEY, 98, 1)]
        );
        // E0 5B — Left Meta.
        assert_eq!(
            decode(&[0xE0, 0x5B]),
            alloc::vec![Event::new(EV_KEY, 125, 1)]
        );
    }

    #[test]
    fn fake_shifts_and_service_bytes_are_not_keys() {
        // PrintScreen — E0 2A E0 37: fake shifts and the extended 0x37 are not
        // keys themselves, so nothing may be emitted as a spurious keystroke.
        assert!(decode(&[0xE0, 0x2A, 0xE0, 0x37]).is_empty());
        // ACK / overrun / Pause sequence produce nothing.
        assert!(decode(&[0xFA, 0x00, 0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5]).is_empty());
    }

    #[test]
    fn extended_prefix_does_not_stick_to_the_next_key() {
        // E0 1E is not a key; the following normal 0x1E must still be KEY_A.
        assert_eq!(
            decode(&[0xE0, 0x1E, 0x1E]),
            alloc::vec![Event::new(EV_KEY, 30, 1)]
        );
    }
}
