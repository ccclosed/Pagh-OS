//! Декодирование сканкодов: `KeyEvent` и state machine расширенных сканкодов
//! с префиксом `0xE0`.
//!
//! Модуль владеет декодированием PS/2 Set 1 для shell. Базовая таблица
//! make-code → ASCII (`scancode_to_ascii`) живёт здесь, а stateful [`Decoder`]
//! добавляет поверх неё отслеживание Shift и навигационные клавиши с префиксом
//! `0xE0`. Decoder потребляет интерактивный цикл чтения; до подключения он
//! считается мёртвым кодом.

/// Декодированное событие клавиатуры.
///
/// Закрытый enum клавиш, которые понимает построчный редактор. Печатный ввод
/// несёт [`KeyEvent::Char`]; остальное — действия редактирования/навигации.
/// Расширенные (с префиксом `0xE0`) сканкоды декодируются только в
/// навигационные варианты — никогда в `Char` (R1.7).
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
    PageUp,
    PageDown,
    Tab,
    Escape,
    Ctrl(char),
    Enter,
}

/// Stateful-декодер сканкодов PS/2 Set 1.
///
/// Держит минимальное состояние для интерпретации сырого потока байт:
/// `extended` ставится на один байт после одиночного префикса `0xE0`, а `shift`
/// отслеживает текущий модификатор Shift через make/break-коды. Decoder никогда
/// не паникует ни на каком входном байте (свойства 25 и 27).
#[allow(dead_code)]
/// Ставится любым декодером, увидевшим Ctrl+C (и построчным редактором shell,
/// и kernel-side raw-mode декодером stdin). Foreground-ожидание `lxrun`
/// опрашивает и забирает эту защёлку, чтобы завершить ребёнка — классическая
/// семантика ^C для ядра без доставки сигналов.
pub static CTRL_C_LATCH: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Защёлка Ctrl+Z для foreground-ожидания `lxrun` (тот же путь, что Ctrl+C).
pub static CTRL_Z_LATCH: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Постоянное состояние CapsLock. Декодеры создаются на каждый вызов чтения на
/// пути stdin, так что переключатель должен жить дольше любого экземпляра.
static CAPS_LOCK_STATE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub struct Decoder {
    extended: bool,
    shift: bool,
    ctrl: bool,
    caps: bool,
}

#[allow(dead_code)]
impl Decoder {
    /// Создать свежий decoder без ожидающего префикса и с отпущенным Shift.
    pub fn new() -> Self {
        Decoder {
            extended: false,
            shift: false,
            ctrl: false,
            caps: CAPS_LOCK_STATE.load(core::sync::atomic::Ordering::Relaxed),
        }
    }

    /// Подать один сырой байт сканкода.
    ///
    /// Возвращает `Some(event)` для полной клавиши или `None` посреди префикса
    /// либо для поглощённого модификатора/break-кода. Функция тотальна по всем
    /// `u8` и никогда не паникует.
    pub fn feed(&mut self, scancode: u8) -> Option<KeyEvent> {
        // Ctrl make/break отслеживается, чтобы модальные приложения могли
        // получать сокращения, не разбирая сырые сканкоды. Правый Ctrl идёт с
        // префиксом E0; поглотить его здесь, чтобы он не утёк в следующую клавишу.
        if self.extended && matches!(scancode, 0x1D | 0x9D) {
            self.extended = false;
            self.ctrl = scancode == 0x1D;
            return None;
        }
        match scancode {
            0x1D => {
                self.ctrl = true;
                return None;
            }
            0x9D => {
                self.ctrl = false;
                return None;
            }
            _ => {}
        }

        // CapsLock make: переключить постоянное состояние, событие не выдавать.
        // Break (0xBA) игнорируется, как LED-only ответ реальной клавиатуры.
        if scancode == 0x3A {
            self.caps = !self.caps;
            CAPS_LOCK_STATE.store(self.caps, core::sync::atomic::Ordering::Relaxed);
            return None;
        }

        // Shift make/break: отслеживать модификатор, событие не выдавать. Обработан
        // до расширенного пути, потому что коды Shift здесь никогда не с E0.
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

        // Одиночный префикс 0xE0: взвести флаг extended, пока ничего не выдавать.
        if scancode == 0xE0 {
            self.extended = true;
            return None;
        }

        if self.extended {
            // Всегда сбрасывать флаг префикса после байта за 0xE0, чтобы
            // случайный префикс не мог заклинить decoder (R11.2).
            self.extended = false;

            // Расширенный break-код (отпускание клавиши): поглотить и игнорировать.
            if scancode >= 0x80 {
                return None;
            }

            // Расширенные make-коды декодируются только в навигационные клавиши.
            // Неизвестные расширенные коды события не дают. ГАРАНТИЯ: никогда Char (R1.7).
            return match scancode {
                0x4B => Some(KeyEvent::Left),
                0x4D => Some(KeyEvent::Right),
                0x47 => Some(KeyEvent::Home),
                0x4F => Some(KeyEvent::End),
                0x53 => Some(KeyEvent::Delete),
                0x48 => Some(KeyEvent::Up),
                0x50 => Some(KeyEvent::Down),
                0x49 => Some(KeyEvent::PageUp),
                0x51 => Some(KeyEvent::PageDown),
                _ => None,
            };
        }

        // Базовые (не расширенные) make-коды.

        // Базовые break-коды (отпускание клавиши): игнорировать.
        if scancode >= 0x80 {
            return None;
        }

        match scancode {
            0x01 => Some(KeyEvent::Escape),
            0x1C => Some(KeyEvent::Enter),
            0x0E => Some(KeyEvent::Backspace),
            0x0F => Some(KeyEvent::Tab),
            _ => {
                // Отобразить оставшиеся базовые make-коды через таблицу ASCII Set 1.
                // Enter ('\n') и Tab ('\t') обработаны выше как отдельные события,
                // поэтому защититься от того, что таблица тоже их выдаст.
                match scancode_to_ascii(scancode, self.shift) {
                    Some(ch) if ch != '\n' && ch != '\t' && self.ctrl => {
                        if ch.to_ascii_lowercase() == 'c' {
                            CTRL_C_LATCH.store(true, core::sync::atomic::Ordering::Relaxed);
                        }
                        if ch.to_ascii_lowercase() == 'z' {
                            CTRL_Z_LATCH.store(true, core::sync::atomic::Ordering::Relaxed);
                        }
                        Some(KeyEvent::Ctrl(ch.to_ascii_lowercase()))
                    }
                    Some(mut ch) if ch != '\n' && ch != '\t' => {
                        // `scancode_to_ascii` уже применил Shift, поэтому
                        // единший оставшийся флип регистра — CapsLock: он
                        // переворачивает букву *относительно того, что выдал
                        // Shift*, — стандартное правило caps-XOR-shift. Сравнение
                        // двух модификаторов здесь (`caps != shift`) дважды
                        // учитывало Shift и делало Shift+букву строчной при
                        // выключенном CapsLock — противоположность клавиатуре.
                        if ch.is_ascii_alphabetic() && self.caps {
                            ch = if ch.is_ascii_lowercase() {
                                ch.to_ascii_uppercase()
                            } else {
                                ch.to_ascii_lowercase()
                            };
                        }
                        Some(KeyEvent::Char(ch))
                    }
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

/// Простое отображение сканкод → ASCII (раскладка US, базовые make-коды PS/2
/// Set 1). Живёт здесь, так как `keys.rs` владеет декодированием сканкодов;
/// цикл чтения shell вызывает его через `keys::scancode_to_ascii`.
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
