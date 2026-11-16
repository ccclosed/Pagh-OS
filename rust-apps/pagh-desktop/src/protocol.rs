//! Проводной протокол Pagh Desktop Protocol (PDP/1) поверх AF_UNIX SOCK_STREAM.
//!
//! Каждое сообщение — 12-байтовый заголовок и полезная нагрузка:
//!
//! ```text
//! off  size  поле
//!   0     4  magic  = b"PDS1"
//!   4     2  type   (u16, little-endian)
//!   6     2  flags  (u16, зарезервировано, должно быть 0)
//!   8     4  length (u32, длина payload в байтах)
//!  12   ...  payload
//! ```
//!
//! Все целые — little-endian. Строки — `u16` длина в байтах + UTF-8. Полное описание
//! сообщений и последовательности рукопожатия — в `PROTOCOL.md`.

use std::io::{self, Read, Write};

/// Сигнатура кадра.
pub const MAGIC: [u8; 4] = *b"PDS1";
/// Версия протокола.
pub const VERSION: u32 = 1;
/// Заголовок сообщения.
pub const HEADER_LEN: usize = 12;
/// Максимальный payload: commit кадра 4096x4096x4 плюс служебные поля.
pub const MAX_PAYLOAD: usize = 4096 * 4096 * 4 + 4096;

pub const TYPE_HELLO: u16 = 1;
pub const TYPE_WELCOME: u16 = 2;
pub const TYPE_CREATE: u16 = 3;
pub const TYPE_CONFIGURE: u16 = 4;
pub const TYPE_COMMIT: u16 = 5;
pub const TYPE_INPUT: u16 = 6;
pub const TYPE_FOCUS: u16 = 7;
pub const TYPE_CLOSE: u16 = 8;
pub const TYPE_DAMAGE: u16 = 9;
pub const TYPE_ERROR: u16 = 10;
pub const TYPE_PING: u16 = 11;
pub const TYPE_PONG: u16 = 12;
/// Расширение PDP/1: просьба клиента запустить приложение (launch).
pub const TYPE_LAUNCH: u16 = 13;
/// c→s: клиент предлагает memfd как поверхность окна (ровно один SCM_RIGHTS fd).
pub const TYPE_SHM_OFFER: u16 = 14;
/// s→c: сервер принял memfd и назначил generation.
pub const TYPE_SHM_READY: u16 = 15;
/// обе стороны: освободить generation (s→c после reoffer, c→s при закрытии).
pub const TYPE_SHM_RELEASE: u16 = 16;

/// Бит 0 флагов заголовка: `commit` в SHM-форме (без пикселей).
pub const HEADER_FLAG_SHM: u16 = 1 << 0;

/// Флаги `configure`.
pub const CONFIGURE_FOCUSED: u32 = 1 << 0;
pub const CONFIGURE_MAXIMIZED: u32 = 1 << 1;
pub const CONFIGURE_MINIMIZED: u32 = 1 << 2;
pub const CONFIGURE_RESIZABLE: u32 = 1 << 3;

/// Флаги `welcome`.
pub const WELCOME_FORMAT_XRGB8888: u32 = 1 << 0;
pub const WELCOME_FORMAT_RGB565: u32 = 1 << 1;
/// Поддерживаются ShmSurface/SCM_RIGHTS (memfd + damage-only commit).
pub const WELCOME_SHM_SURFACE: u32 = 1 << 2;

/// Причины `close`.
pub const CLOSE_CLIENT: u32 = 0;
pub const CLOSE_SERVER: u32 = 1;
pub const CLOSE_DISCONNECT: u32 = 2;
pub const CLOSE_PROTOCOL_ERROR: u32 = 3;
pub const CLOSE_DESTROYED: u32 = 4;

/// Коды ошибок `error`.
pub const ERR_BAD_MESSAGE: u32 = 1;
pub const ERR_NOT_HELLO: u32 = 2;
pub const ERR_DUPLICATE_WINDOW: u32 = 3;
pub const ERR_UNKNOWN_WINDOW: u32 = 4;
pub const ERR_NOT_OWNER: u32 = 5;
pub const ERR_INVALID_SURFACE: u32 = 6;
pub const ERR_LIMIT: u32 = 7;
pub const ERR_UNSUPPORTED: u32 = 8;
pub const ERR_INTERNAL: u32 = 9;

/// Кнопки мыши в `input`.
pub const BUTTON_LEFT: u32 = 1;
pub const BUTTON_MIDDLE: u32 = 2;
pub const BUTTON_RIGHT: u32 = 3;
/// Битовая маска нажатых кнопок для поля `buttons`.
pub const BUTTONS_LEFT: u32 = 1 << 0;
pub const BUTTONS_MIDDLE: u32 = 1 << 1;
pub const BUTTONS_RIGHT: u32 = 1 << 2;

/// Модификаторы клавиатуры в `input`.
pub const MOD_SHIFT: u32 = 1 << 0;
pub const MOD_CTRL: u32 = 1 << 1;
pub const MOD_ALT: u32 = 1 << 2;
pub const MOD_SUPER: u32 = 1 << 3;

/// Подмножество evdev-кодов клавиш, используемое сервером и клиентами.
pub mod key {
    pub const ESC: u32 = 1;
    pub const BACKSPACE: u32 = 14;
    pub const TAB: u32 = 15;
    pub const ENTER: u32 = 28;
    pub const LEFT_CTRL: u32 = 29;
    pub const LEFT_SHIFT: u32 = 42;
    pub const LEFT_ALT: u32 = 56;
    pub const F4: u32 = 62;
    pub const F5: u32 = 63;
    pub const F10: u32 = 68;
    pub const HOME: u32 = 102;
    pub const PAGEUP: u32 = 104;
    pub const END: u32 = 107;
    pub const PAGEDOWN: u32 = 109;
    pub const SPACE: u32 = 57;
    pub const UP: u32 = 103;
    pub const LEFT: u32 = 105;
    pub const RIGHT: u32 = 106;
    pub const DOWN: u32 = 108;
    pub const DELETE: u32 = 111;
    pub const SUPER: u32 = 125;
    pub const A: u32 = 30;
    pub const T: u32 = 20;
    pub const X: u32 = 45;
}

/// Событие ввода, доставляемое клиенту.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// Движение указателя; `x`/`y` — координаты внутри клиентской поверхности.
    Motion {
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
        buttons: u32,
    },
    /// Нажатие/отпускание кнопки мыши.
    Button {
        x: i32,
        y: i32,
        button: u32,
        pressed: bool,
        buttons: u32,
    },
    /// Клавиша; `code` — evdev-подобный код, `modifiers` — биты `MOD_*`.
    Key {
        code: u32,
        pressed: bool,
        modifiers: u32,
    },
    /// Прокрутка колесом.
    Axis {
        x: i32,
        y: i32,
        horizontal: i32,
        vertical: i32,
        buttons: u32,
    },
}

impl InputEvent {
    fn kind(self) -> u8 {
        match self {
            Self::Motion { .. } => 1,
            Self::Button { .. } => 2,
            Self::Key { .. } => 3,
            Self::Axis { .. } => 4,
        }
    }
}

/// Сообщение протокола.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// c→s: первое сообщение соединения.
    Hello { version: u32, pid: u32, app: String },
    /// s→c: ответ на hello.
    Welcome {
        version: u32,
        client_id: u64,
        width: u32,
        height: u32,
        flags: u32,
    },
    /// c→s: создать окно.
    Create {
        window_id: u32,
        width: u32,
        height: u32,
        format: u32,
        title: String,
    },
    /// s→c: сервер разместил/изменил окно.
    Configure {
        window_id: u32,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        flags: u32,
    },
    /// c→s: полный кадр окна.
    Commit {
        window_id: u32,
        width: u32,
        height: u32,
        format: u32,
        stride: u32,
        pixels: Vec<u8>,
    },
    /// s→c: событие ввода для окна.
    Input { window_id: u32, event: InputEvent },
    /// s→c: окно получило/потеряло фокус.
    Focus { window_id: u32, focused: bool },
    /// обе стороны: закрыть окно.
    Close { window_id: u32, reason: u32 },
    /// c→s: подсказка damage для будущего частичного аплоада.
    Damage {
        window_id: u32,
        rects: Vec<(i32, i32, u16, u16)>,
    },
    /// s→c: ошибка обработки запроса.
    Error { code: u32, message: String },
    /// Проверка живости соединения.
    Ping { nonce: u32 },
    /// Ответ на ping.
    Pong { nonce: u32 },
    /// c→s: открыть известное композитору приложение `app` в каталоге `cwd`.
    ///
    /// Идентификаторы задаёт сервер через `ServerOptions.launch_targets`
    /// (`"terminal"`, `"files"`, …); `cwd` — подсказка приложениям с рабочим
    /// каталогом (может быть пустой). Успех не подтверждается отдельным
    /// сообщением, ошибка приходит как [`Message::Error`].
    Launch { app: String, cwd: String },
    /// c→s: memfd поверхности окна; fd приходит отдельным ancillary SCM_RIGHTS.
    ShmOffer {
        window_id: u32,
        width: u32,
        height: u32,
        format: u32,
        stride: u32,
        flags: u32,
    },
    /// s→c: memfd принят, generation можно использовать в commit/damage.
    ShmReady { window_id: u32, generation: u32 },
    /// обе стороны: generation больше не нужен.
    ShmRelease { window_id: u32, generation: u32 },
    /// c→s: commit без пикселей (header flags bit 0).
    ShmCommit {
        window_id: u32,
        generation: u32,
        rects: Vec<(i32, i32, u32, u32)>,
    },
    /// c→s: damage-форма ShmSurface (type 9 с generation).
    ShmDamage {
        window_id: u32,
        generation: u32,
        rects: Vec<(i32, i32, u32, u32)>,
    },
}

impl Message {
    pub fn kind(&self) -> u16 {
        match self {
            Self::Hello { .. } => TYPE_HELLO,
            Self::Welcome { .. } => TYPE_WELCOME,
            Self::Create { .. } => TYPE_CREATE,
            Self::Configure { .. } => TYPE_CONFIGURE,
            Self::Commit { .. } => TYPE_COMMIT,
            Self::Input { .. } => TYPE_INPUT,
            Self::Focus { .. } => TYPE_FOCUS,
            Self::Close { .. } => TYPE_CLOSE,
            Self::Damage { .. } => TYPE_DAMAGE,
            Self::Error { .. } => TYPE_ERROR,
            Self::Ping { .. } => TYPE_PING,
            Self::Pong { .. } => TYPE_PONG,
            Self::Launch { .. } => TYPE_LAUNCH,
            Self::ShmOffer { .. } => TYPE_SHM_OFFER,
            Self::ShmReady { .. } => TYPE_SHM_READY,
            Self::ShmRelease { .. } => TYPE_SHM_RELEASE,
            Self::ShmCommit { .. } => TYPE_COMMIT,
            Self::ShmDamage { .. } => TYPE_DAMAGE,
        }
    }

    /// Флаги заголовка сообщения.
    pub fn header_flags(&self) -> u16 {
        match self {
            Self::ShmCommit { .. } => HEADER_FLAG_SHM,
            _ => 0,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::Welcome { .. } => "welcome",
            Self::Create { .. } => "create",
            Self::Configure { .. } => "configure",
            Self::Commit { .. } => "commit",
            Self::Input { .. } => "input",
            Self::Focus { .. } => "focus",
            Self::Close { .. } => "close",
            Self::Damage { .. } => "damage",
            Self::Error { .. } => "error",
            Self::Ping { .. } => "ping",
            Self::Pong { .. } => "pong",
            Self::Launch { .. } => "launch",
            Self::ShmOffer { .. } => "shm_offer",
            Self::ShmReady { .. } => "shm_ready",
            Self::ShmRelease { .. } => "shm_release",
            Self::ShmCommit { .. } => "shm_commit",
            Self::ShmDamage { .. } => "shm_damage",
        }
    }

    /// Полный кадр: заголовок + payload.
    pub fn encode(&self) -> Vec<u8> {
        let payload = self.encode_payload();
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&self.kind().to_le_bytes());
        out.extend_from_slice(&self.header_flags().to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    fn encode_payload(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        match self {
            Self::Hello { version, pid, app } => {
                e.u32(*version);
                e.u32(*pid);
                e.string(app);
            }
            Self::Welcome {
                version,
                client_id,
                width,
                height,
                flags,
            } => {
                e.u32(*version);
                e.u64(*client_id);
                e.u32(*width);
                e.u32(*height);
                e.u32(*flags);
            }
            Self::Create {
                window_id,
                width,
                height,
                format,
                title,
            } => {
                e.u32(*window_id);
                e.u32(*width);
                e.u32(*height);
                e.u32(*format);
                e.string(title);
            }
            Self::Configure {
                window_id,
                x,
                y,
                width,
                height,
                flags,
            } => {
                e.u32(*window_id);
                e.i32(*x);
                e.i32(*y);
                e.u32(*width);
                e.u32(*height);
                e.u32(*flags);
            }
            Self::Commit {
                window_id,
                width,
                height,
                format,
                stride,
                pixels,
            } => {
                e.u32(*window_id);
                e.u32(*width);
                e.u32(*height);
                e.u32(*format);
                e.u32(*stride);
                e.u32(pixels.len() as u32);
                e.bytes(pixels);
            }
            Self::Input { window_id, event } => {
                e.u32(*window_id);
                e.u8(event.kind());
                match *event {
                    InputEvent::Motion {
                        x,
                        y,
                        dx,
                        dy,
                        buttons,
                    } => {
                        e.u8(0);
                        e.u16(0);
                        e.u32(0);
                        e.i32(x);
                        e.i32(y);
                        e.i32(dx);
                        e.i32(dy);
                        e.u32(buttons);
                    }
                    InputEvent::Button {
                        x,
                        y,
                        button,
                        pressed,
                        buttons,
                    } => {
                        e.u8(u8::from(pressed));
                        e.u16(0);
                        e.u32(button);
                        e.i32(x);
                        e.i32(y);
                        e.i32(0);
                        e.i32(0);
                        e.u32(buttons);
                    }
                    InputEvent::Key {
                        code,
                        pressed,
                        modifiers,
                    } => {
                        e.u8(u8::from(pressed));
                        e.u16(modifiers as u16);
                        e.u32(code);
                        e.i32(0);
                        e.i32(0);
                        e.i32(0);
                        e.i32(0);
                        e.u32(0);
                    }
                    InputEvent::Axis {
                        x,
                        y,
                        horizontal,
                        vertical,
                        buttons,
                    } => {
                        e.u8(0);
                        e.u16(0);
                        e.u32(0);
                        e.i32(x);
                        e.i32(y);
                        e.i32(horizontal);
                        e.i32(vertical);
                        e.u32(buttons);
                    }
                }
            }
            Self::Focus { window_id, focused } => {
                e.u32(*window_id);
                e.u8(u8::from(*focused));
                e.bytes(&[0, 0, 0]);
            }
            Self::Close { window_id, reason } => {
                e.u32(*window_id);
                e.u32(*reason);
            }
            Self::Damage { window_id, rects } => {
                e.u32(*window_id);
                e.u16(rects.len().min(u16::MAX as usize) as u16);
                for (x, y, w, h) in rects.iter().take(u16::MAX as usize) {
                    e.i32(*x);
                    e.i32(*y);
                    e.u16(*w);
                    e.u16(*h);
                }
            }
            Self::Error { code, message } => {
                e.u32(*code);
                e.string(message);
            }
            Self::Ping { nonce } | Self::Pong { nonce } => e.u32(*nonce),
            Self::Launch { app, cwd } => {
                e.string(app);
                e.string(cwd);
            }
            Self::ShmOffer {
                window_id,
                width,
                height,
                format,
                stride,
                flags,
            } => {
                e.u32(*window_id);
                e.u32(*width);
                e.u32(*height);
                e.u32(*format);
                e.u32(*stride);
                e.u32(*flags);
            }
            Self::ShmReady {
                window_id,
                generation,
            }
            | Self::ShmRelease {
                window_id,
                generation,
            } => {
                e.u32(*window_id);
                e.u32(*generation);
            }
            Self::ShmCommit {
                window_id,
                generation,
                rects,
            }
            | Self::ShmDamage {
                window_id,
                generation,
                rects,
            } => {
                e.u32(*window_id);
                e.u32(*generation);
                e.u32(rects.len() as u32);
                for (x, y, w, h) in rects {
                    e.i32(*x);
                    e.i32(*y);
                    e.u32(*w);
                    e.u32(*h);
                }
            }
        }
        e.into_inner()
    }

    /// Разобрать payload по известному типу и флагам заголовка.
    fn decode(kind: u16, flags: u16, payload: &[u8]) -> Result<Self, ProtocolError> {
        let mut d = PayloadDecoder::payload(payload);
        let message = match kind {
            TYPE_HELLO => Self::Hello {
                version: d.u32()?,
                pid: d.u32()?,
                app: d.string()?,
            },
            TYPE_WELCOME => Self::Welcome {
                version: d.u32()?,
                client_id: d.u64()?,
                width: d.u32()?,
                height: d.u32()?,
                flags: d.u32()?,
            },
            TYPE_CREATE => Self::Create {
                window_id: d.u32()?,
                width: d.u32()?,
                height: d.u32()?,
                format: d.u32()?,
                title: d.string()?,
            },
            TYPE_CONFIGURE => Self::Configure {
                window_id: d.u32()?,
                x: d.i32()?,
                y: d.i32()?,
                width: d.u32()?,
                height: d.u32()?,
                flags: d.u32()?,
            },
            TYPE_COMMIT => {
                if flags & HEADER_FLAG_SHM != 0 {
                    let window_id = d.u32()?;
                    let generation = d.u32()?;
                    let count = d.u32()? as usize;
                    let rects = decode_rects_u32(&mut d, count)?;
                    d.finish()?;
                    Self::ShmCommit {
                        window_id,
                        generation,
                        rects,
                    }
                } else {
                    let window_id = d.u32()?;
                    let width = d.u32()?;
                    let height = d.u32()?;
                    let format = d.u32()?;
                    let stride = d.u32()?;
                    let data_len = d.u32()? as usize;
                    let pixels = d.bytes(data_len)?.to_vec();
                    d.finish()?;
                    Self::Commit {
                        window_id,
                        width,
                        height,
                        format,
                        stride,
                        pixels,
                    }
                }
            }
            TYPE_INPUT => {
                let window_id = d.u32()?;
                let kind_byte = d.u8()?;
                let pressed = d.u8()? != 0;
                let modifiers = d.u16()? as u32;
                let code = d.u32()?;
                let x = d.i32()?;
                let y = d.i32()?;
                let dx = d.i32()?;
                let dy = d.i32()?;
                let buttons = d.u32()?;
                let event = match kind_byte {
                    1 => InputEvent::Motion {
                        x,
                        y,
                        dx,
                        dy,
                        buttons,
                    },
                    2 => InputEvent::Button {
                        x,
                        y,
                        button: code,
                        pressed,
                        buttons,
                    },
                    3 => InputEvent::Key {
                        code,
                        pressed,
                        modifiers,
                    },
                    4 => InputEvent::Axis {
                        x,
                        y,
                        horizontal: dx,
                        vertical: dy,
                        buttons,
                    },
                    other => {
                        return Err(ProtocolError::BadPayload(format!(
                            "неизвестный тип input {other}"
                        )))
                    }
                };
                Self::Input { window_id, event }
            }
            TYPE_FOCUS => {
                let window_id = d.u32()?;
                let focused = d.u8()? != 0;
                d.bytes(3)?;
                Self::Focus { window_id, focused }
            }
            TYPE_CLOSE => Self::Close {
                window_id: d.u32()?,
                reason: d.u32()?,
            },
            TYPE_DAMAGE => {
                // Две формы: legacy (u16 count; длина 6 mod 12) и ShmSurface
                // (generation + u32 count; длина кратна 12).
                let shm_form = payload.len() >= 12 && payload.len() % 12 == 0;
                let window_id = d.u32()?;
                if shm_form {
                    let generation = d.u32()?;
                    let count = d.u32()? as usize;
                    let rects = decode_rects_u32(&mut d, count)?;
                    Self::ShmDamage {
                        window_id,
                        generation,
                        rects,
                    }
                } else {
                    let count = d.u16()? as usize;
                    let mut rects = Vec::with_capacity(count);
                    for _ in 0..count {
                        let x = d.i32()?;
                        let y = d.i32()?;
                        let w = d.u16()?;
                        let h = d.u16()?;
                        rects.push((x, y, w, h));
                    }
                    Self::Damage { window_id, rects }
                }
            }
            TYPE_ERROR => Self::Error {
                code: d.u32()?,
                message: d.string()?,
            },
            TYPE_PING => Self::Ping { nonce: d.u32()? },
            TYPE_PONG => Self::Pong { nonce: d.u32()? },
            TYPE_LAUNCH => Self::Launch {
                app: d.string()?,
                cwd: d.string()?,
            },
            TYPE_SHM_OFFER => Self::ShmOffer {
                window_id: d.u32()?,
                width: d.u32()?,
                height: d.u32()?,
                format: d.u32()?,
                stride: d.u32()?,
                flags: d.u32()?,
            },
            TYPE_SHM_READY => Self::ShmReady {
                window_id: d.u32()?,
                generation: d.u32()?,
            },
            TYPE_SHM_RELEASE => Self::ShmRelease {
                window_id: d.u32()?,
                generation: d.u32()?,
            },
            other => return Err(ProtocolError::UnknownType(other)),
        };
        d.finish()?;
        Ok(message)
    }
}

fn decode_rects_u32(
    d: &mut PayloadDecoder<'_>,
    count: usize,
) -> Result<Vec<(i32, i32, u32, u32)>, ProtocolError> {
    // Каждый прямоугольник — 12 байт; count ограничен длиной payload, но
    // защищаемся от гигантского Vec::with_capacity.
    let count = count.min(d.remaining() / 12);
    let mut rects = Vec::with_capacity(count);
    for _ in 0..count {
        rects.push((d.i32()?, d.i32()?, d.u32()?, d.u32()?));
    }
    Ok(rects)
}

/// Ошибка разбора/валидации протокола.
#[derive(Debug)]
pub enum ProtocolError {
    BadMagic,
    TooLarge { length: usize },
    UnknownType(u16),
    BadPayload(String),
    Io(io::Error),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic => write!(f, "неверная сигнатура кадра"),
            Self::TooLarge { length } => write!(f, "слишком большое сообщение: {length} байт"),
            Self::UnknownType(kind) => write!(f, "неизвестный тип сообщения {kind}"),
            Self::BadPayload(reason) => write!(f, "некорректный payload: {reason}"),
            Self::Io(err) => write!(f, "ошибка ввода-вывода: {err}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<io::Error> for ProtocolError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Флаги заголовка: bit 0 разрешён только для `commit` (SHM-форма).
fn validate_header_flags(kind: u16, flags: u16) -> Result<(), ProtocolError> {
    if flags & !HEADER_FLAG_SHM != 0 {
        return Err(ProtocolError::BadPayload(format!(
            "неизвестные флаги заголовка: {flags:#x}"
        )));
    }
    if flags != 0 && kind != TYPE_COMMIT {
        return Err(ProtocolError::BadPayload(format!(
            "флаги {flags:#x} допустимы только для commit (type {kind})"
        )));
    }
    Ok(())
}

/// Инкрементальный разбор потока кадров.
#[derive(Debug, Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// Добавить байты из сокета.
    pub fn push(&mut self, data: &[u8]) -> Result<(), ProtocolError> {
        self.buffer.extend_from_slice(data);
        if self.buffer.len() >= HEADER_LEN {
            let length = u32::from_le_bytes([
                self.buffer[8],
                self.buffer[9],
                self.buffer[10],
                self.buffer[11],
            ]) as usize;
            if length > MAX_PAYLOAD {
                return Err(ProtocolError::TooLarge { length });
            }
            if self.buffer.len() > HEADER_LEN + length {
                // Лишние байты — начало следующего кадра; их проверит следующий next().
            }
        }
        Ok(())
    }

    /// Извлечь следующий полный кадр, если он уже накоплен.
    pub fn next_message(&mut self) -> Result<Option<Message>, ProtocolError> {
        if self.buffer.len() < HEADER_LEN {
            return Ok(None);
        }
        if self.buffer[0..4] != MAGIC {
            return Err(ProtocolError::BadMagic);
        }
        let kind = u16::from_le_bytes([self.buffer[4], self.buffer[5]]);
        let flags = u16::from_le_bytes([self.buffer[6], self.buffer[7]]);
        validate_header_flags(kind, flags)?;
        let length = u32::from_le_bytes([
            self.buffer[8],
            self.buffer[9],
            self.buffer[10],
            self.buffer[11],
        ]) as usize;
        if length > MAX_PAYLOAD {
            return Err(ProtocolError::TooLarge { length });
        }
        let total = HEADER_LEN + length;
        if self.buffer.len() < total {
            return Ok(None);
        }
        let payload: Vec<u8> = self.buffer[HEADER_LEN..total].to_vec();
        self.buffer.drain(..total);
        Message::decode(kind, flags, &payload).map(Some)
    }
}

/// Записать сообщение в поток.
pub fn write_message<W: Write>(writer: &mut W, message: &Message) -> io::Result<()> {
    writer.write_all(&message.encode())
}

/// Прочитать одно сообщение из блокирующего потока.
pub fn read_message<R: Read>(reader: &mut R) -> io::Result<Message> {
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header)?;
    if header[0..4] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "неверная сигнатура кадра",
        ));
    }
    let kind = u16::from_le_bytes([header[4], header[5]]);
    let flags = u16::from_le_bytes([header[6], header[7]]);
    validate_header_flags(kind, flags)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let length = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    if length > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "слишком большое сообщение",
        ));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload)?;
    Message::decode(kind, flags, &payload)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))
}

struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn i32(&mut self, value: i32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn bytes(&mut self, value: &[u8]) {
        self.buf.extend_from_slice(value);
    }

    fn string(&mut self, value: &str) {
        let bytes = value.as_bytes();
        let len = bytes.len().min(u16::MAX as usize);
        self.u16(len as u16);
        self.bytes(&bytes[..len]);
    }

    fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

struct PayloadDecoder<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PayloadDecoder<'a> {
    fn payload(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], ProtocolError> {
        if self.pos + len > self.data.len() {
            return Err(ProtocolError::BadPayload(format!(
                "недостаточно данных: нужно {len}, осталось {}",
                self.data.len().saturating_sub(self.pos)
            )));
        }
        let slice = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ProtocolError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn i32(&mut self) -> Result<i32, ProtocolError> {
        Ok(self.u32()? as i32)
    }

    fn u64(&mut self) -> Result<u64, ProtocolError> {
        let bytes = self.take(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(arr))
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8], ProtocolError> {
        self.take(len)
    }

    fn string(&mut self) -> Result<String, ProtocolError> {
        let len = self.u16()? as usize;
        let bytes = self.bytes(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| ProtocolError::BadPayload("строка не является UTF-8".into()))
    }

    fn finish(&self) -> Result<(), ProtocolError> {
        if self.pos != self.data.len() {
            return Err(ProtocolError::BadPayload(format!(
                "лишние байты payload: {}",
                self.data.len() - self.pos
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(message: Message) {
        let encoded = message.encode();
        let mut decoder = Decoder::new();
        decoder.push(&encoded).unwrap();
        assert_eq!(decoder.next_message().unwrap(), Some(message));
        assert!(decoder.next_message().unwrap().is_none());
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn all_messages_roundtrip() {
        roundtrip(Message::Hello {
            version: VERSION,
            pid: 42,
            app: "pagh-test".into(),
        });
        roundtrip(Message::Welcome {
            version: VERSION,
            client_id: 7,
            width: 1280,
            height: 720,
            flags: 3,
        });
        roundtrip(Message::Create {
            window_id: 1,
            width: 640,
            height: 400,
            format: 1,
            title: "Terminal — pagh".into(),
        });
        roundtrip(Message::Configure {
            window_id: 1,
            x: -20,
            y: 30,
            width: 640,
            height: 400,
            flags: CONFIGURE_FOCUSED | CONFIGURE_RESIZABLE,
        });
        roundtrip(Message::Commit {
            window_id: 1,
            width: 2,
            height: 1,
            format: 1,
            stride: 8,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8],
        });
        roundtrip(Message::Input {
            window_id: 1,
            event: InputEvent::Motion {
                x: 10,
                y: 20,
                dx: -1,
                dy: 2,
                buttons: BUTTONS_LEFT,
            },
        });
        roundtrip(Message::Input {
            window_id: 1,
            event: InputEvent::Button {
                x: 10,
                y: 20,
                button: BUTTON_LEFT,
                pressed: true,
                buttons: BUTTONS_LEFT,
            },
        });
        roundtrip(Message::Input {
            window_id: 1,
            event: InputEvent::Key {
                code: key::A,
                pressed: true,
                modifiers: MOD_CTRL,
            },
        });
        roundtrip(Message::Input {
            window_id: 1,
            event: InputEvent::Axis {
                x: 1,
                y: 2,
                horizontal: 0,
                vertical: -1,
                buttons: 0,
            },
        });
        roundtrip(Message::Focus {
            window_id: 1,
            focused: false,
        });
        roundtrip(Message::Close {
            window_id: 1,
            reason: CLOSE_SERVER,
        });
        roundtrip(Message::Damage {
            window_id: 1,
            rects: vec![(0, 0, 10, 20), (5, 5, 1, 1)],
        });
        roundtrip(Message::Error {
            code: ERR_NONE_PLACEHOLDER,
            message: "ошибка".into(),
        });
        roundtrip(Message::Ping { nonce: 99 });
        roundtrip(Message::Pong { nonce: 99 });
    }

    // Отдельная константа, чтобы тест не зависел от конкретного кода ошибки сервера.
    const ERR_NONE_PLACEHOLDER: u32 = 42;

    #[test]
    fn decoder_handles_partial_frames() {
        let message = Message::Ping { nonce: 1 };
        let encoded = message.encode();
        let mut decoder = Decoder::new();
        for chunk in encoded.chunks(3) {
            decoder.push(chunk).unwrap();
        }
        assert_eq!(decoder.next_message().unwrap(), Some(message));
    }

    #[test]
    fn decoder_handles_multiple_frames_in_one_read() {
        let mut bytes = Message::Ping { nonce: 1 }.encode();
        bytes.extend_from_slice(&Message::Pong { nonce: 2 }.encode());
        let mut decoder = Decoder::new();
        decoder.push(&bytes).unwrap();
        assert_eq!(
            decoder.next_message().unwrap(),
            Some(Message::Ping { nonce: 1 })
        );
        assert_eq!(
            decoder.next_message().unwrap(),
            Some(Message::Pong { nonce: 2 })
        );
        assert!(decoder.next_message().unwrap().is_none());
    }

    #[test]
    fn rejects_bad_magic_and_oversize() {
        let mut decoder = Decoder::new();
        decoder
            .push(b"XXXX\x01\x00\x00\x00\x00\x00\x00\x00")
            .unwrap();
        assert!(matches!(
            decoder.next_message(),
            Err(ProtocolError::BadMagic)
        ));

        let mut decoder = Decoder::new();
        let mut header = Vec::new();
        header.extend_from_slice(&MAGIC);
        header.extend_from_slice(&TYPE_PING.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());
        decoder.push(&header).unwrap_err();
        assert!(matches!(
            decoder.push(&header),
            Err(ProtocolError::TooLarge { .. })
        ));
    }

    #[test]
    fn rejects_unknown_type_and_trailing_bytes() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&999u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let mut decoder = Decoder::new();
        decoder.push(&bytes).unwrap();
        assert!(matches!(
            decoder.next_message(),
            Err(ProtocolError::UnknownType(999))
        ));

        // Заголовок обещает 5 байт payload, а Ping принимает ровно 4.
        let mut ping_header = Vec::new();
        ping_header.extend_from_slice(&MAGIC);
        ping_header.extend_from_slice(&TYPE_PING.to_le_bytes());
        ping_header.extend_from_slice(&0u16.to_le_bytes());
        ping_header.extend_from_slice(&5u32.to_le_bytes());
        ping_header.extend_from_slice(&1u32.to_le_bytes());
        ping_header.push(0xAA);
        let mut decoder = Decoder::new();
        decoder.push(&ping_header).unwrap();
        assert!(matches!(
            decoder.next_message(),
            Err(ProtocolError::BadPayload(_))
        ));
    }

    #[test]
    fn header_is_stable() {
        let encoded = Message::Ping { nonce: 0x0102_0304 }.encode();
        assert_eq!(&encoded[0..4], b"PDS1");
        assert_eq!(&encoded[4..6], &TYPE_PING.to_le_bytes());
        assert_eq!(&encoded[6..8], &[0, 0]);
        assert_eq!(&encoded[8..12], &4u32.to_le_bytes());
        assert_eq!(&encoded[12..16], &[4, 3, 2, 1]);
    }
}
