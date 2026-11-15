//! Ввод `/dev/input/event0`: 24-байтные `input_event`, EV_KEY/EV_REL/EV_SYN.
//!
//! Устройство открывается неблокирующе; сервер опрашивает его в своём цикле и
//! превращает события в [`InputAction`] — те же действия, что приходят через
//! `ServerCommand` из тестов. Никакого второго потока/`poll(2)` не нужно:
//! цикл сервера и так просыпается каждые несколько миллисекунд.
//!
//! Абсолютной координации в ABI нет: накапливаем `REL_X`/`REL_Y` до `SYN_REPORT`
//! и двигаем курсор относительно; `REL_WHEEL` превращается в `Axis`. Кнопки —
//! `BTN_LEFT`/`BTN_MIDDLE`/`BTN_RIGHT`. Модификаторы (`KEY_LEFTSHIFT` и т.п.)
//! отслеживаются здесь, потому что `INPUT`-сообщение требует их маску.

use crate::compositor::MouseButton;
use crate::protocol::{MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_SUPER};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// `sizeof(struct input_event)` на x86_64.
pub const EVENT_SIZE: usize = 24;
/// `O_NONBLOCK` (Linux/musl x86_64).
const O_NONBLOCK: i32 = 0o4000;

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const SYN_REPORT: u16 = 0;

pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_WHEEL: u16 = 0x08;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_RIGHTALT: u16 = 100;
const KEY_LEFTSUPER: u16 = 125;
const KEY_RIGHTSUPER: u16 = 126;

/// Одно `struct input_event`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawEvent {
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

impl RawEvent {
    /// Разобрать 24 байта Linux `input_event` (little-endian x86_64).
    pub fn decode(bytes: &[u8; EVENT_SIZE]) -> Self {
        Self {
            type_: u16::from_le_bytes([bytes[16], bytes[17]]),
            code: u16::from_le_bytes([bytes[18], bytes[19]]),
            value: i32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
        }
    }
}

/// Действие, которое нужно применить к композитору.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputAction {
    Motion {
        dx: i32,
        dy: i32,
    },
    Wheel {
        delta: i32,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    Key {
        code: u32,
        pressed: bool,
        modifiers: u32,
    },
}

/// Открытый evdev-источник.
pub struct EvdevInput {
    file: File,
    path: PathBuf,
    pending_dx: i32,
    pending_dy: i32,
    pending_wheel: i32,
    modifiers: u32,
    pub events_read: u64,
    /// События, накопившиеся до старта композитора (набранная shell-команда):
    /// выбрасываются, чтобы десктоп не «переигрывал» старый ввод.
    pub stale_discarded: u64,
}

impl std::fmt::Debug for EvdevInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvdevInput")
            .field("path", &self.path)
            .field("modifiers", &self.modifiers)
            .field("events_read", &self.events_read)
            .field("stale_discarded", &self.stale_discarded)
            .finish()
    }
}

impl EvdevInput {
    /// Открыть `/dev/input/event0` в неблокирующем режиме.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open(&path)?;
        let mut input = Self {
            file,
            path,
            pending_dx: 0,
            pending_dy: 0,
            pending_wheel: 0,
            modifiers: 0,
            events_read: 0,
            stale_discarded: 0,
        };
        input.drain_pending();
        Ok(input)
    }

    /// Выбросить всё, что успело накопиться в кольце evdev до открытия
    /// композитора (обычно — строка запуска, набранная в shell).
    fn drain_pending(&mut self) {
        loop {
            let mut bytes = [0u8; EVENT_SIZE];
            match self.file.read(&mut bytes) {
                Ok(EVENT_SIZE) => self.stale_discarded += 1,
                Ok(_) => break,
                Err(err)
                    if err.kind() == io::ErrorKind::WouldBlock
                        || err.kind() == io::ErrorKind::TimedOut =>
                {
                    break
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn modifiers(&self) -> u32 {
        self.modifiers
    }

    /// Прочитать все доступные события и вернуть действия в порядке поступления.
    ///
    /// Возвращает `Ok(vec![])`, когда очередь пуста (`EAGAIN`) — это нормальный
    /// idle-путь цикла сервера.
    pub fn poll(&mut self) -> io::Result<Vec<InputAction>> {
        let mut actions = Vec::new();
        loop {
            let mut bytes = [0u8; EVENT_SIZE];
            match self.file.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) if n == EVENT_SIZE => {
                    self.events_read += 1;
                    let event = RawEvent::decode(&bytes);
                    self.handle_event(event, &mut actions);
                }
                Ok(n) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("evdev: неполное событие {n} байт"),
                    ))
                }
                Err(err)
                    if err.kind() == io::ErrorKind::WouldBlock
                        || err.kind() == io::ErrorKind::TimedOut =>
                {
                    break
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
        Ok(actions)
    }

    /// Чистая обработка одного события (host-тестируемая; без файла).
    pub fn handle_event(&mut self, event: RawEvent, out: &mut Vec<InputAction>) {
        match event.type_ {
            EV_REL => match event.code {
                REL_X => self.pending_dx = self.pending_dx.saturating_add(event.value),
                REL_Y => self.pending_dy = self.pending_dy.saturating_add(event.value),
                REL_WHEEL => self.pending_wheel = self.pending_wheel.saturating_add(event.value),
                _ => {}
            },
            EV_KEY => {
                // Перед кнопкой/клавишей отдаём накопленное движение, чтобы
                // клик пришёлся на актуальную позицию курсора.
                self.flush_motion(out);
                if let Some(button) = mouse_button(event.code) {
                    out.push(InputAction::Button {
                        button,
                        pressed: event.value != 0,
                    });
                    return;
                }
                if let Some(bit) = modifier_bit(event.code) {
                    if event.value != 0 {
                        self.modifiers |= bit;
                    } else {
                        self.modifiers &= !bit;
                    }
                }
                out.push(InputAction::Key {
                    code: event.code as u32,
                    pressed: event.value != 0,
                    modifiers: self.modifiers,
                });
            }
            EV_SYN if event.code == SYN_REPORT => self.flush_motion(out),
            _ => {}
        }
    }

    fn flush_motion(&mut self, out: &mut Vec<InputAction>) {
        if self.pending_dx != 0 || self.pending_dy != 0 {
            out.push(InputAction::Motion {
                dx: self.pending_dx,
                dy: self.pending_dy,
            });
            self.pending_dx = 0;
            self.pending_dy = 0;
        }
        if self.pending_wheel != 0 {
            out.push(InputAction::Wheel {
                delta: self.pending_wheel,
            });
            self.pending_wheel = 0;
        }
    }
}

pub fn mouse_button(code: u16) -> Option<MouseButton> {
    match code {
        BTN_LEFT => Some(MouseButton::Left),
        BTN_MIDDLE => Some(MouseButton::Middle),
        BTN_RIGHT => Some(MouseButton::Right),
        _ => None,
    }
}

pub fn modifier_bit(code: u16) -> Option<u32> {
    match code {
        KEY_LEFTSHIFT | KEY_RIGHTSHIFT => Some(MOD_SHIFT),
        KEY_LEFTCTRL | KEY_RIGHTCTRL => Some(MOD_CTRL),
        KEY_LEFTALT | KEY_RIGHTALT => Some(MOD_ALT),
        KEY_LEFTSUPER | KEY_RIGHTSUPER => Some(MOD_SUPER),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(path: &Path) -> EvdevInput {
        // Для чистых тестов файл не нужен; создаём временный, чтобы конструктор
        // был тем же, что в бою.
        std::fs::write(path, b"").unwrap();
        EvdevInput::open(path).unwrap()
    }

    fn event(type_: u16, code: u16, value: i32) -> RawEvent {
        RawEvent { type_, code, value }
    }

    #[test]
    fn decodes_linux_input_event() {
        let mut bytes = [0u8; 24];
        bytes[0..8].copy_from_slice(&123i64.to_le_bytes());
        bytes[8..16].copy_from_slice(&456i64.to_le_bytes());
        bytes[16..18].copy_from_slice(&EV_REL.to_le_bytes());
        bytes[18..20].copy_from_slice(&REL_X.to_le_bytes());
        bytes[20..24].copy_from_slice(&(-7i32).to_le_bytes());
        assert_eq!(
            RawEvent::decode(&bytes),
            RawEvent {
                type_: EV_REL,
                code: REL_X,
                value: -7
            }
        );
    }

    #[test]
    fn accumulates_motion_until_syn() {
        let dir = std::env::temp_dir().join(format!("pagh-evdev-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut input = input(&dir.join("e0"));
        let mut out = Vec::new();
        input.handle_event(event(EV_REL, REL_X, 4), &mut out);
        input.handle_event(event(EV_REL, REL_Y, -3), &mut out);
        input.handle_event(event(EV_REL, REL_X, 1), &mut out);
        assert!(out.is_empty(), "до SYN_REPORT действий быть не должно");
        input.handle_event(event(EV_SYN, SYN_REPORT, 0), &mut out);
        assert_eq!(out, vec![InputAction::Motion { dx: 5, dy: -3 }]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn buttons_and_wheel() {
        let dir = std::env::temp_dir().join(format!("pagh-evdev-btn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut input = input(&dir.join("e0"));
        let mut out = Vec::new();
        input.handle_event(event(EV_KEY, BTN_LEFT, 1), &mut out);
        input.handle_event(event(EV_KEY, BTN_LEFT, 0), &mut out);
        input.handle_event(event(EV_REL, REL_WHEEL, 2), &mut out);
        input.handle_event(event(EV_SYN, SYN_REPORT, 0), &mut out);
        assert_eq!(
            out,
            vec![
                InputAction::Button {
                    button: MouseButton::Left,
                    pressed: true
                },
                InputAction::Button {
                    button: MouseButton::Left,
                    pressed: false
                },
                InputAction::Wheel { delta: 2 },
            ]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn modifiers_track_press_and_release() {
        let dir = std::env::temp_dir().join(format!("pagh-evdev-mod-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut input = input(&dir.join("e0"));
        let mut out = Vec::new();
        input.handle_event(event(EV_KEY, KEY_LEFTSHIFT, 1), &mut out);
        input.handle_event(event(EV_KEY, 30, 1), &mut out); // KEY_A
        input.handle_event(event(EV_KEY, KEY_LEFTSHIFT, 0), &mut out);
        input.handle_event(event(EV_KEY, 30, 0), &mut out);
        assert_eq!(
            out[1],
            InputAction::Key {
                code: 30,
                pressed: true,
                modifiers: MOD_SHIFT
            }
        );
        assert_eq!(input.modifiers(), 0);
        let key_releases: Vec<_> = out
            .iter()
            .filter(|action| matches!(action, InputAction::Key { .. }))
            .collect();
        assert_eq!(key_releases.len(), 4);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn motion_before_click_is_flushed_first() {
        let dir = std::env::temp_dir().join(format!("pagh-evdev-order-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut input = input(&dir.join("e0"));
        let mut out = Vec::new();
        input.handle_event(event(EV_REL, REL_X, 9), &mut out);
        input.handle_event(event(EV_KEY, BTN_RIGHT, 1), &mut out);
        assert_eq!(
            out,
            vec![
                InputAction::Motion { dx: 9, dy: 0 },
                InputAction::Button {
                    button: MouseButton::Right,
                    pressed: true
                }
            ]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn open_missing_device_fails() {
        let err = EvdevInput::open("/dev/definitely-no-event0").unwrap_err();
        assert!(matches!(
            err.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
        ));
    }
}
