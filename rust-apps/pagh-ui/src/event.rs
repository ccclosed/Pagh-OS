//! События ввода и клавиатуры, общие для приложений и протокола.
//!
//! Модель событий повторяет evdev-подобный поток `/dev/input/event0`: позиция
//! указателя абсолютная, кнопки и клавиши приходят парами press/release. Клиент
//! протокола получает [`Event`] от композитора и передаёт его виджетам как есть.

use crate::geom::Point;
use crate::geom::Size;

/// Кнопка мыши.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

impl MouseButton {
    /// Код кнопки для протокола (как BTN_LEFT/BTN_MIDDLE/BTN_RIGHT в evdev).
    pub const fn protocol_code(self) -> u8 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        }
    }

    pub fn from_protocol(code: u8) -> Option<MouseButton> {
        match code {
            0 => Some(MouseButton::Left),
            1 => Some(MouseButton::Middle),
            2 => Some(MouseButton::Right),
            _ => None,
        }
    }
}

/// Модификаторы клавиатуры.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl Modifiers {
    pub const NONE: Modifiers = Modifiers {
        shift: false,
        ctrl: false,
        alt: false,
    };

    pub const fn shift() -> Modifiers {
        Modifiers {
            shift: true,
            ctrl: false,
            alt: false,
        }
    }

    pub const fn ctrl() -> Modifiers {
        Modifiers {
            shift: false,
            ctrl: true,
            alt: false,
        }
    }

    /// Битовая маска для протокола.
    pub const fn bits(self) -> u8 {
        (self.shift as u8) | ((self.ctrl as u8) << 1) | ((self.alt as u8) << 2)
    }

    pub const fn from_bits(bits: u8) -> Modifiers {
        Modifiers {
            shift: bits & 1 != 0,
            ctrl: bits & 2 != 0,
            alt: bits & 4 != 0,
        }
    }
}

/// Клавиши, которые разбирает тулкит.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Enter,
    Escape,
    Tab,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F4,
    F5,
    /// Печатный символ (приходит из раскладки композитора).
    Char(char),
}

impl Key {
    /// Код клавиши для протокола (Linux KEY_*-подобные значения).
    pub const fn protocol_code(self) -> u32 {
        match self {
            Key::Enter => 28,
            Key::Escape => 1,
            Key::Tab => 15,
            Key::Backspace => 14,
            Key::Delete => 111,
            Key::Up => 103,
            Key::Down => 108,
            Key::Left => 105,
            Key::Right => 106,
            Key::Home => 102,
            Key::End => 107,
            Key::PageUp => 104,
            Key::PageDown => 109,
            Key::F4 => 62,
            Key::F5 => 63,
            // Печатные символы не ходят по проводу: PDP/1 передаёт только
            // evdev-код, текст восстанавливает клиент.
            Key::Char(_) => 0,
        }
    }

    pub fn from_protocol(code: u32) -> Option<Key> {
        match code {
            28 => Some(Key::Enter),
            1 => Some(Key::Escape),
            15 => Some(Key::Tab),
            14 => Some(Key::Backspace),
            111 => Some(Key::Delete),
            103 => Some(Key::Up),
            108 => Some(Key::Down),
            105 => Some(Key::Left),
            106 => Some(Key::Right),
            102 => Some(Key::Home),
            107 => Some(Key::End),
            104 => Some(Key::PageUp),
            109 => Some(Key::PageDown),
            62 => Some(Key::F4),
            63 => Some(Key::F5),
            _ => None,
        }
    }
}

/// Событие ввода.
#[derive(Clone, PartialEq, Debug)]
pub enum Event {
    /// Указатель переместился в абсолютную позицию.
    MouseMove { pos: Point },
    /// Кнопка мыши нажата.
    MouseDown { pos: Point, button: MouseButton },
    /// Кнопка мыши отпущена.
    MouseUp { pos: Point, button: MouseButton },
    /// Двойной клик (эмуляция двойного нажатия левой кнопкой).
    DoubleClick { pos: Point },
    /// Колесо: `delta` > 0 — вверх.
    Wheel { pos: Point, delta: i32 },
    /// Клавиша нажата/отпущена.
    Key {
        key: Key,
        pressed: bool,
        modifiers: Modifiers,
    },
    /// Ввод текста (уже с учётом раскладки).
    Text { text: String },
    /// Изменился размер поверхности окна.
    Resize { size: Size },
    /// Окно получило/потеряло фокус.
    Focus { gained: bool },
    /// Композитор/пользователь просит закрыть окно.
    CloseRequest,
}

impl Event {
    /// Позиция указателя, если событие «мышиное».
    pub fn pointer(&self) -> Option<Point> {
        match self {
            Event::MouseMove { pos }
            | Event::MouseDown { pos, .. }
            | Event::MouseUp { pos, .. }
            | Event::DoubleClick { pos }
            | Event::Wheel { pos, .. } => Some(*pos),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_button_codes_roundtrip() {
        for button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
            assert_eq!(
                MouseButton::from_protocol(button.protocol_code()),
                Some(button)
            );
        }
        assert_eq!(MouseButton::from_protocol(9), None);
    }

    #[test]
    fn key_codes_roundtrip() {
        for key in [
            Key::Enter,
            Key::Escape,
            Key::Tab,
            Key::Backspace,
            Key::Delete,
            Key::Up,
            Key::Down,
            Key::Left,
            Key::Right,
            Key::Home,
            Key::End,
            Key::PageUp,
            Key::PageDown,
            Key::F4,
            Key::F5,
        ] {
            assert_eq!(Key::from_protocol(key.protocol_code()), Some(key));
        }
        // Печатные символы по проводу не передаются.
        assert_eq!(Key::from_protocol(Key::Char('ё').protocol_code()), None);
    }

    #[test]
    fn modifiers_bits() {
        let m = Modifiers {
            shift: true,
            ctrl: true,
            alt: false,
        };
        assert_eq!(m.bits(), 3);
        assert_eq!(Modifiers::from_bits(3), m);
        assert!(Modifiers::from_bits(7).alt);
    }

    #[test]
    fn pointer_accessor() {
        assert_eq!(
            Event::MouseDown {
                pos: Point::new(3, 4),
                button: MouseButton::Right
            }
            .pointer(),
            Some(Point::new(3, 4))
        );
        assert_eq!(Event::CloseRequest.pointer(), None);
    }
}
