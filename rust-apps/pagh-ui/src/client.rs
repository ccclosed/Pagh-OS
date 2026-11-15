//! Клиент PDP/1: подключение к `pagh-desktop`, окна, кадры, ввод и закрытие.
//!
//! Композитор владеет размещением, заголовком окна (`v`/`^`/`x`), фокусом и
//! панелью; клиент рисует **только клиентскую область** и получает координаты
//! ввода относительно неё. Кадры в v1 идут полной поверхностью внутри сокета
//! (`XRGB8888` или `RGB565`), `damage` — только подсказка.
//!
//! Поверх [`protocol`] клиент даёт удобный [`DesktopClient`]:
//!
//! ```no_run
//! use pagh_ui::client::DesktopClient;
//! use pagh_ui::geom::Size;
//!
//! let mut client = DesktopClient::connect("/tmp/pagh-desktop.sock", "pagh-files")?;
//! let window = client.open_window(Size::new(1000, 600), "Домашняя папка — Files")?;
//! // ... нарисовать кадр ...
//! # Ok::<(), std::io::Error>(())
//! ```

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::canvas::Canvas;
use crate::event::{Event, Key, Modifiers, MouseButton};
use crate::geom::{Point, Size};
use crate::protocol::{
    self, buttons, close_reason, mods, pack_rgb565, pack_xrgb8888, FrameFormat, InputEvent,
    Message, MessageReader, HEADER_LEN, MAX_PAYLOAD, MAX_SURFACE_DIM, VERSION,
};

/// Сокет композитора по умолчанию (совпадает с `pagh-desktop`).
pub const DEFAULT_SOCKET: &str = "/tmp/pagh-desktop.sock";

/// Информация из `welcome`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub version: u32,
    pub client_id: u64,
    pub screen: Size,
    pub flags: u32,
}

/// Входящее событие композитора, переведённое в модель приложения.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Композитор разместил/изменил окно (размер клиентской области, позиция,
    /// флаги `CONFIGURE_*`).
    Configure {
        window_id: u32,
        position: Point,
        size: Size,
        flags: u32,
    },
    /// Событие ввода для окна (координаты — внутри клиентской области).
    Event { window_id: u32, event: Event },
    /// Окно получило/потеряло фокус.
    Focus { window_id: u32, focused: bool },
    /// Композитор закрыл окно; `reason` — `close_reason::*`.
    Close { window_id: u32, reason: u32 },
    /// Ошибка протокола от композитора.
    Error { code: u32, message: String },
    /// `ping` (клиент уже ответил `pong`).
    Ping { nonce: u32 },
}

/// Подключение к композитору Pagh desktop.
pub struct DesktopClient {
    stream: UnixStream,
    reader: MessageReader,
    welcome: Option<Welcome>,
    format: FrameFormat,
    screen: Size,
    window_id: u32,
    width: u32,
    height: u32,
    flags: u32,
    peer_closed: bool,
    next_window: u32,
}

impl DesktopClient {
    /// Подключиться к сокету (без рукопожатия).
    pub fn connect_socket(path: impl AsRef<Path>) -> io::Result<DesktopClient> {
        let stream = UnixStream::connect(path.as_ref())?;
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(DesktopClient {
            stream,
            reader: MessageReader::new(),
            welcome: None,
            format: FrameFormat::Xrgb8888,
            screen: Size::ZERO,
            window_id: 0,
            width: 0,
            height: 0,
            flags: 0,
            peer_closed: false,
            next_window: 1,
        })
    }

    /// Подключиться и пройти `hello`/`welcome`.
    pub fn connect(path: impl AsRef<Path>, app: &str) -> io::Result<DesktopClient> {
        let mut client = DesktopClient::connect_socket(path)?;
        client.hello(app, Duration::from_secs(5))?;
        Ok(client)
    }

    /// Подключиться с повторами (сервер может ещё создавать сокет).
    pub fn connect_retry(
        path: impl AsRef<Path>,
        app: &str,
        attempts: u32,
    ) -> io::Result<DesktopClient> {
        let mut last = None;
        for attempt in 0..attempts.max(1) {
            match DesktopClient::connect(path.as_ref(), app) {
                Ok(client) => return Ok(client),
                Err(error) => {
                    last = Some(error);
                    std::thread::sleep(Duration::from_millis(20 + attempt as u64 * 15));
                }
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("нет попыток подключения")))
    }

    pub fn welcome(&self) -> Option<&Welcome> {
        self.welcome.as_ref()
    }

    pub fn client_id(&self) -> u64 {
        self.welcome
            .as_ref()
            .map(|welcome| welcome.client_id)
            .unwrap_or(0)
    }

    pub fn screen(&self) -> Size {
        self.screen
    }

    pub fn format(&self) -> FrameFormat {
        self.format
    }

    /// Текущее окно клиента (0, пока окно не создано).
    pub fn window_id(&self) -> u32 {
        self.window_id
    }

    /// Текущий размер клиентской области по последнему `configure`/`commit`.
    pub fn size(&self) -> Size {
        Size::new(self.width as i32, self.height as i32)
    }

    /// Флаги последнего `configure` (`CONFIGURE_*`).
    pub fn configure_flags(&self) -> u32 {
        self.flags
    }

    /// Соединение закрыто композитором.
    pub fn peer_closed(&self) -> bool {
        self.peer_closed
    }

    /// Отправить `hello` и дождаться `welcome`.
    pub fn hello(&mut self, app: &str, timeout: Duration) -> io::Result<Welcome> {
        self.send(&Message::Hello {
            version: VERSION,
            pid: std::process::id(),
            app: app.to_string(),
        })?;
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "композитор не прислал welcome",
                ));
            }
            let Some(message) = self.next_message(remaining)? else {
                continue;
            };
            match message {
                Message::Welcome {
                    version,
                    client_id,
                    width,
                    height,
                    flags,
                } => {
                    if version != VERSION {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("неподдерживаемая версия PDP/1: {version}"),
                        ));
                    }
                    self.format = FrameFormat::from_welcome_flags(flags);
                    self.screen = Size::new(width as i32, height as i32);
                    let welcome = Welcome {
                        version,
                        client_id,
                        screen: self.screen,
                        flags,
                    };
                    self.welcome = Some(welcome.clone());
                    return Ok(welcome);
                }
                Message::Error { code, message } => {
                    return Err(io::Error::other(format!(
                        "композитор вернул ошибку {code}: {message}"
                    )));
                }
                _ => {}
            }
        }
    }

    /// Создать окно с явным идентификатором (не 0, уникален у клиента).
    pub fn create_window(&mut self, window_id: u32, size: Size, title: &str) -> io::Result<()> {
        if window_id == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "window_id должен быть ненулевым",
            ));
        }
        if size.w <= 0
            || size.h <= 0
            || size.w as u32 > MAX_SURFACE_DIM
            || size.h as u32 > MAX_SURFACE_DIM
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "недопустимый размер окна",
            ));
        }
        self.window_id = window_id;
        self.width = size.w as u32;
        self.height = size.h as u32;
        self.send(&Message::Create {
            window_id,
            width: size.w as u32,
            height: size.h as u32,
            format: self.format.to_wire(),
            title: title.to_string(),
        })
    }

    /// Создать окно, выбрав следующий свободный идентификатор.
    pub fn open_window(&mut self, size: Size, title: &str) -> io::Result<u32> {
        let window_id = self.next_window;
        self.next_window = self.next_window.wrapping_add(1).max(1);
        self.create_window(window_id, size, title)?;
        Ok(window_id)
    }

    /// Отправить полный кадр поверхности окна.
    pub fn commit(&mut self, window_id: u32, canvas: &Canvas) -> io::Result<()> {
        self.commit_rgba(
            window_id,
            canvas.width() as u32,
            canvas.height() as u32,
            canvas.pixels(),
        )
    }

    /// Отправить кадр из straight-RGBA пикселей.
    pub fn commit_rgba(
        &mut self,
        window_id: u32,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> io::Result<()> {
        if width == 0 || height == 0 || width > MAX_SURFACE_DIM || height > MAX_SURFACE_DIM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "недопустимый размер кадра",
            ));
        }
        let (pixels, stride) = match self.format {
            FrameFormat::Xrgb8888 => (
                pack_xrgb8888(rgba, width as usize, height as usize).map_err(io::Error::from)?,
                width * 4,
            ),
            FrameFormat::Rgb565 => (
                pack_rgb565(rgba, width as usize, height as usize).map_err(io::Error::from)?,
                width * 2,
            ),
        };
        if window_id == self.window_id {
            self.width = width;
            self.height = height;
        }
        self.send(&Message::Commit {
            window_id,
            width,
            height,
            format: self.format.to_wire(),
            stride,
            pixels,
        })
    }

    /// Подсказка damage (в v1 не уменьшает кадр, но композитор её использует).
    pub fn damage_hint(
        &mut self,
        window_id: u32,
        rects: &[(i32, i32, u16, u16)],
    ) -> io::Result<()> {
        self.send(&Message::Damage {
            window_id,
            rects: rects.to_vec(),
        })
    }

    /// Попросить композитор открыть известное приложение (LAUNCH, type 13).
    ///
    /// `app` — идентификатор приложения (`"terminal"`, `"files"`, …); `cwd` —
    /// подсказка рабочего каталога (может быть пустой). Ошибка запуска придёт
    /// как [`Incoming::Error`].
    pub fn launch(&mut self, app: &str, cwd: &str) -> io::Result<()> {
        self.send(&Message::Launch {
            app: app.to_string(),
            cwd: cwd.to_string(),
        })
    }

    /// Попросить композитор закрыть окно (причина `CLIENT`).
    pub fn close_window(&mut self, window_id: u32) -> io::Result<()> {
        self.send(&Message::Close {
            window_id,
            reason: close_reason::CLIENT,
        })
    }

    /// Проверка живости соединения.
    pub fn ping(&mut self, nonce: u32) -> io::Result<()> {
        self.send(&Message::Ping { nonce })
    }

    fn send(&mut self, message: &Message) -> io::Result<()> {
        let encoded = message.encode();
        if encoded.len() > HEADER_LEN + MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "слишком большое сообщение PDP/1",
            ));
        }
        self.stream.write_all(&encoded)?;
        self.stream.flush()
    }

    /// Прочитать все события, ожидая первое не дольше `timeout`.
    pub fn poll(&mut self, timeout: Duration) -> io::Result<Vec<Incoming>> {
        let deadline = Instant::now() + timeout;
        let mut incoming = Vec::new();
        loop {
            if let Some(message) = self.reader.next().map_err(io::Error::from)? {
                self.handle_message(message, &mut incoming)?;
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            self.stream
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))?;
            let mut buffer = [0u8; 64 * 1024];
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    self.peer_closed = true;
                    incoming.push(Incoming::Close {
                        window_id: self.window_id,
                        reason: close_reason::DISCONNECT,
                    });
                    return Ok(incoming);
                }
                Ok(count) => self.reader.push(&buffer[..count]),
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut =>
                {
                    if incoming.is_empty() && !remaining.is_zero() {
                        continue;
                    }
                    return Ok(incoming);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn handle_message(&mut self, message: Message, incoming: &mut Vec<Incoming>) -> io::Result<()> {
        match message {
            Message::Welcome {
                client_id,
                width,
                height,
                flags,
                version,
            } => {
                self.format = FrameFormat::from_welcome_flags(flags);
                self.screen = Size::new(width as i32, height as i32);
                self.welcome = Some(Welcome {
                    version,
                    client_id,
                    screen: self.screen,
                    flags,
                });
            }
            Message::Configure {
                window_id,
                x,
                y,
                width,
                height,
                flags,
            } => {
                if window_id == self.window_id {
                    self.width = width;
                    self.height = height;
                    self.flags = flags;
                }
                incoming.push(Incoming::Configure {
                    window_id,
                    position: Point::new(x, y),
                    size: Size::new(width as i32, height as i32),
                    flags,
                });
            }
            Message::Input { window_id, event } => {
                if window_id != self.window_id && self.window_id != 0 {
                    return Ok(());
                }
                push_input_events(event, window_id, incoming);
            }
            Message::Focus { window_id, focused } => {
                incoming.push(Incoming::Focus { window_id, focused });
            }
            Message::Close { window_id, reason } => {
                incoming.push(Incoming::Close { window_id, reason });
            }
            Message::Error { code, message } => {
                incoming.push(Incoming::Error { code, message });
            }
            Message::Ping { nonce } => {
                self.send(&Message::Pong { nonce })?;
                incoming.push(Incoming::Ping { nonce });
            }
            Message::Pong { .. } | Message::Damage { .. } => {}
            Message::Hello { .. }
            | Message::Create { .. }
            | Message::Commit { .. }
            | Message::Launch { .. } => {}
        }
        Ok(())
    }

    /// Прочитать одно сообщение с дедлайном (для рукопожатия).
    fn next_message(&mut self, timeout: Duration) -> io::Result<Option<Message>> {
        if let Some(message) = self.reader.next().map_err(io::Error::from)? {
            return Ok(Some(message));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            self.stream
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))?;
            let mut buffer = [0u8; 64 * 1024];
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    self.peer_closed = true;
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "композитор закрыл соединение",
                    ));
                }
                Ok(count) => {
                    self.reader.push(&buffer[..count]);
                    if let Some(message) = self.reader.next().map_err(io::Error::from)? {
                        return Ok(Some(message));
                    }
                }
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut =>
                {
                    return Ok(None)
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

/// Перевести событие ввода PDP/1 в события приложения.
///
/// Клавиши приходят evdev-кодами без текста; печатные символы восстанавливаются
/// по US-раскладке (Shift учитывается, Caps Lock и другие раскладки — нет).
/// На нажатие печатной клавиши без Ctrl/Alt приходит пара `Key::Char` +
/// [`Event::Text`], чтобы поля ввода и горячие клавиши работали одинаково.
fn push_input_events(event: InputEvent, window_id: u32, incoming: &mut Vec<Incoming>) {
    match event {
        InputEvent::Motion { x, y, .. } => incoming.push(Incoming::Event {
            window_id,
            event: Event::MouseMove {
                pos: Point::new(x, y),
            },
        }),
        InputEvent::Button {
            x,
            y,
            button,
            pressed,
            buttons: _,
        } => {
            let Some(button) = mouse_button(button) else {
                return;
            };
            let pos = Point::new(x, y);
            incoming.push(Incoming::Event {
                window_id,
                event: if pressed {
                    Event::MouseDown { pos, button }
                } else {
                    Event::MouseUp { pos, button }
                },
            });
        }
        InputEvent::Axis { x, y, vertical, .. } => incoming.push(Incoming::Event {
            window_id,
            event: Event::Wheel {
                pos: Point::new(x, y),
                delta: vertical.clamp(-8, 8),
            },
        }),
        InputEvent::Key {
            code,
            pressed,
            modifiers,
        } => {
            let modifiers = Modifiers {
                shift: modifiers & mods::SHIFT != 0,
                ctrl: modifiers & mods::CTRL != 0,
                alt: modifiers & mods::ALT != 0,
            };
            let Some((key, text)) = map_key(code, modifiers) else {
                return;
            };
            incoming.push(Incoming::Event {
                window_id,
                event: Event::Key {
                    key,
                    pressed,
                    modifiers,
                },
            });
            if pressed && !modifiers.ctrl && !modifiers.alt {
                if let Some(text) = text {
                    incoming.push(Incoming::Event {
                        window_id,
                        event: Event::Text {
                            text: text.to_string(),
                        },
                    });
                }
            }
        }
    }
}

fn mouse_button(button: u32) -> Option<MouseButton> {
    match button {
        buttons::LEFT => Some(MouseButton::Left),
        buttons::MIDDLE => Some(MouseButton::Middle),
        buttons::RIGHT => Some(MouseButton::Right),
        _ => None,
    }
}

/// Evdev-код непечатной клавиши → [`Key`].
fn special_key(code: u32) -> Option<Key> {
    Some(match code {
        protocol::key::ESC => Key::Escape,
        protocol::key::TAB => Key::Tab,
        protocol::key::BACKSPACE => Key::Backspace,
        protocol::key::ENTER => Key::Enter,
        protocol::key::DELETE => Key::Delete,
        protocol::key::UP => Key::Up,
        protocol::key::DOWN => Key::Down,
        protocol::key::LEFT => Key::Left,
        protocol::key::RIGHT => Key::Right,
        protocol::key::HOME => Key::Home,
        protocol::key::END => Key::End,
        protocol::key::PAGEUP => Key::PageUp,
        protocol::key::PAGEDOWN => Key::PageDown,
        protocol::key::F4 => Key::F4,
        protocol::key::F5 => Key::F5,
        _ => return None,
    })
}

/// Печатная клавиша по US-раскладке с учётом Shift.
fn printable_key(code: u32, shift: bool) -> Option<char> {
    let unshifted = match code {
        protocol::key::SPACE => ' ',
        protocol::key::KEY_1 => '1',
        protocol::key::KEY_2 => '2',
        protocol::key::KEY_3 => '3',
        protocol::key::KEY_4 => '4',
        protocol::key::KEY_5 => '5',
        protocol::key::KEY_6 => '6',
        protocol::key::KEY_7 => '7',
        protocol::key::KEY_8 => '8',
        protocol::key::KEY_9 => '9',
        protocol::key::KEY_0 => '0',
        protocol::key::KEY_A => 'a',
        protocol::key::KEY_B => 'b',
        protocol::key::KEY_C => 'c',
        protocol::key::KEY_D => 'd',
        protocol::key::KEY_E => 'e',
        protocol::key::KEY_F => 'f',
        protocol::key::KEY_G => 'g',
        protocol::key::KEY_H => 'h',
        protocol::key::KEY_I => 'i',
        protocol::key::KEY_J => 'j',
        protocol::key::KEY_K => 'k',
        protocol::key::KEY_L => 'l',
        protocol::key::KEY_M => 'm',
        protocol::key::KEY_N => 'n',
        protocol::key::KEY_O => 'o',
        protocol::key::KEY_P => 'p',
        protocol::key::KEY_Q => 'q',
        protocol::key::KEY_R => 'r',
        protocol::key::KEY_S => 's',
        protocol::key::KEY_T => 't',
        protocol::key::KEY_U => 'u',
        protocol::key::KEY_V => 'v',
        protocol::key::KEY_W => 'w',
        protocol::key::KEY_X => 'x',
        protocol::key::KEY_Y => 'y',
        protocol::key::KEY_Z => 'z',
        protocol::key::MINUS => '-',
        protocol::key::EQUAL => '=',
        protocol::key::COMMA => ',',
        protocol::key::DOT => '.',
        protocol::key::SLASH => '/',
        protocol::key::SEMICOLON => ';',
        protocol::key::APOSTROPHE => '\'',
        protocol::key::GRAVE => '`',
        protocol::key::KEY_LEFTBRACE => '[',
        protocol::key::KEY_RIGHTBRACE => ']',
        protocol::key::BACKSLASH => '\\',
        _ => return None,
    };
    if !shift {
        return Some(unshifted);
    }
    let shifted = match unshifted {
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
        '-' => '_',
        '=' => '+',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        ';' => ':',
        '\'' => '"',
        '`' => '~',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        other => return Some(other.to_ascii_uppercase()),
    };
    Some(shifted)
}

/// Evdev-код → (`Key`, возможно печатный символ).
pub fn map_key(code: u32, modifiers: Modifiers) -> Option<(Key, Option<char>)> {
    if let Some(key) = special_key(code) {
        return Some((key, None));
    }
    let ch = printable_key(code, modifiers.shift)?;
    Some((Key::Char(ch), Some(ch)))
}

/// Имя кода ошибки PDP/1.
pub fn error_name(code: u32) -> &'static str {
    match code {
        protocol::error_codes::BAD_MESSAGE => "некорректное сообщение",
        protocol::error_codes::NOT_HELLO => "запрос до hello",
        protocol::error_codes::DUPLICATE_WINDOW => "окно уже существует",
        protocol::error_codes::UNKNOWN_WINDOW => "неизвестное окно",
        protocol::error_codes::NOT_OWNER => "чужое окно",
        protocol::error_codes::INVALID_SURFACE => "недопустимая поверхность",
        protocol::error_codes::LIMIT => "превышен лимит окон",
        protocol::error_codes::UNSUPPORTED => "не поддерживается",
        protocol::error_codes::INTERNAL => "внутренняя ошибка",
        _ => "неизвестная ошибка",
    }
}

/// Имя причины закрытия окна.
pub fn close_name(reason: u32) -> &'static str {
    match reason {
        close_reason::CLIENT => "клиент",
        close_reason::SERVER => "композитор",
        close_reason::DISCONNECT => "соединение разорвано",
        close_reason::PROTOCOL_ERROR => "ошибка протокола",
        close_reason::DESTROYED => "окно уничтожено",
        _ => "неизвестная причина",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn socket_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pagh-ui-pdp-{tag}-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// Тестовый сервер: пишет hello, отвечает welcome, дальше — по сценарию.
    fn serve(listener: UnixListener, script: impl FnOnce(&mut UnixStream) + Send + 'static) {
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello = protocol::read_message(&mut stream).unwrap();
            assert!(matches!(hello, Message::Hello { .. }));
            stream
                .write_all(
                    &Message::Welcome {
                        version: VERSION,
                        client_id: 7,
                        width: 1280,
                        height: 800,
                        flags: protocol::welcome_flags::FORMAT_XRGB8888
                            | protocol::welcome_flags::FORMAT_RGB565,
                    }
                    .encode(),
                )
                .unwrap();
            script(&mut stream);
        });
    }

    #[test]
    fn handshake_create_commit_and_events() {
        let path = socket_path("basic");
        let listener = UnixListener::bind(&path).unwrap();
        serve(listener, |stream| {
            let create = protocol::read_message(stream).unwrap();
            let Message::Create {
                window_id,
                width,
                height,
                format,
                title,
            } = create
            else {
                panic!("ожидался create, получено {create:?}");
            };
            assert_eq!((window_id, width, height, format), (1, 200, 100, 1));
            assert_eq!(title, "Тестовое окно");
            stream
                .write_all(
                    &Message::Focus {
                        window_id,
                        focused: true,
                    }
                    .encode(),
                )
                .unwrap();
            stream
                .write_all(
                    &Message::Configure {
                        window_id,
                        x: 40,
                        y: 60,
                        width: 220,
                        height: 110,
                        flags: protocol::configure_flags::FOCUSED,
                    }
                    .encode(),
                )
                .unwrap();
            let commit = protocol::read_message(stream).unwrap();
            let Message::Commit {
                window_id: id,
                width: 220,
                height: 110,
                format: 1,
                stride: 880,
                pixels,
            } = commit
            else {
                panic!("ожидался commit, получено {commit:?}");
            };
            assert_eq!((id, pixels.len()), (window_id, 880 * 110));
            // Ввод: движение, кнопка, клавиша, колесо.
            for event in [
                InputEvent::Motion {
                    x: 10,
                    y: 20,
                    dx: 1,
                    dy: 2,
                    buttons: 0,
                },
                InputEvent::Button {
                    x: 10,
                    y: 20,
                    button: buttons::RIGHT,
                    pressed: true,
                    buttons: buttons::MASK_RIGHT,
                },
                InputEvent::Key {
                    code: protocol::key::ENTER,
                    pressed: true,
                    modifiers: 0,
                },
                InputEvent::Axis {
                    x: 10,
                    y: 20,
                    horizontal: 0,
                    vertical: -1,
                    buttons: 0,
                },
            ] {
                stream
                    .write_all(&Message::Input { window_id, event }.encode())
                    .unwrap();
            }
            stream
                .write_all(
                    &Message::Close {
                        window_id,
                        reason: close_reason::SERVER,
                    }
                    .encode(),
                )
                .unwrap();
        });
        thread::sleep(Duration::from_millis(20));

        let mut client = DesktopClient::connect(&path, "pagh-ui-test").unwrap();
        assert_eq!(client.welcome().unwrap().client_id, 7);
        assert_eq!(client.screen(), Size::new(1280, 800));
        assert_eq!(client.format(), FrameFormat::Xrgb8888);

        let window = client
            .open_window(Size::new(200, 100), "Тестовое окно")
            .unwrap();
        assert_eq!(window, 1);

        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && events.len() < 4 {
            events.extend(client.poll(Duration::from_millis(50)).unwrap());
            if events
                .iter()
                .any(|event| matches!(event, Incoming::Configure { .. }))
            {
                break;
            }
        }
        assert!(events
            .iter()
            .any(|event| matches!(event, Incoming::Focus { focused: true, .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            Incoming::Configure { size, position, .. }
                if *size == Size::new(220, 110) && *position == Point::new(40, 60)
        )));

        let canvas = Canvas::filled(220, 110, crate::color::Color::hex(0x081216));
        client.commit(window, &canvas).unwrap();
        assert_eq!(client.size(), Size::new(220, 110));

        let mut received = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            received.extend(client.poll(Duration::from_millis(50)).unwrap());
            if received
                .iter()
                .any(|event| matches!(event, Incoming::Close { .. }))
            {
                break;
            }
        }
        let events: Vec<&Incoming> = received
            .iter()
            .filter(|event| matches!(event, Incoming::Event { .. }))
            .collect();
        assert_eq!(events.len(), 4, "motion + button + key + wheel: {events:?}");
        assert!(received.iter().any(|event| matches!(
            event,
            Incoming::Event {
                event: Event::MouseDown {
                    button: MouseButton::Right,
                    ..
                },
                ..
            }
        )));
        assert!(received.iter().any(|event| matches!(
            event,
            Incoming::Event {
                event: Event::Key {
                    key: Key::Enter,
                    pressed: true,
                    ..
                },
                ..
            }
        )));
        assert!(received.iter().any(|event| matches!(
            event,
            Incoming::Event {
                event: Event::Wheel { delta: -1, .. },
                ..
            }
        )));
        assert!(received.iter().any(|event| matches!(
            event,
            Incoming::Close { reason, .. } if *reason == close_reason::SERVER
        )));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn launch_and_close_are_encoded() {
        let path = socket_path("launch");
        let listener = UnixListener::bind(&path).unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = received.clone();
        serve(listener, move |stream| {
            for _ in 0..2 {
                let message = protocol::read_message(stream).unwrap();
                sink.lock().unwrap().push(message);
            }
        });
        thread::sleep(Duration::from_millis(20));
        let mut client = DesktopClient::connect(&path, "pagh-ui-test").unwrap();
        client.launch("terminal", "/home/user/Документы").unwrap();
        client.close_window(1).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && received.lock().unwrap().len() < 2 {
            thread::sleep(Duration::from_millis(10));
        }
        let messages = received.lock().unwrap().clone();
        assert!(matches!(
            messages.first(),
            Some(Message::Launch { app, cwd })
                if app == "terminal" && cwd == "/home/user/Документы"
        ));
        assert!(matches!(
            messages.get(1),
            Some(Message::Close { window_id: 1, reason }) if *reason == close_reason::CLIENT
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn key_mapping_covers_letters_and_special_keys() {
        let plain = Modifiers::NONE;
        assert_eq!(
            map_key(protocol::key::KEY_A, plain),
            Some((Key::Char('a'), Some('a')))
        );
        assert_eq!(
            map_key(protocol::key::KEY_A, Modifiers::shift()),
            Some((Key::Char('A'), Some('A')))
        );
        assert_eq!(
            map_key(protocol::key::KEY_1, Modifiers::shift()),
            Some((Key::Char('!'), Some('!')))
        );
        assert_eq!(
            map_key(protocol::key::SPACE, plain),
            Some((Key::Char(' '), Some(' ')))
        );
        assert_eq!(
            map_key(protocol::key::ENTER, plain),
            Some((Key::Enter, None))
        );
        assert_eq!(map_key(protocol::key::F5, plain), Some((Key::F5, None)));
        assert_eq!(map_key(999, plain), None);
    }

    #[test]
    fn error_and_close_names_are_russian() {
        assert_eq!(
            error_name(protocol::error_codes::UNSUPPORTED),
            "не поддерживается"
        );
        assert_eq!(close_name(close_reason::SERVER), "композитор");
    }
}
