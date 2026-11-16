//! Pagh Desktop Protocol PDP/1 — wire codec (client side).
//!
//! This implements the same transport the compositor documents in
//! `rust-apps/pagh-desktop/PROTOCOL.md` (sibling `feat/desktop-server`
//! worktree), so a terminal surface produced by this crate can talk to that
//! server without an adapter:
//!
//! ```text
//!   offset  size  field
//!   0       4     magic  = b"PDS1"
//!   4       2     type   (u16 LE)
//!   6       2     flags  (u16 LE, must be 0)
//!   8       4     length (u32 LE, payload bytes)
//!   12      N     payload
//! ```
//!
//! All integers are little-endian.  Strings are `u16 length` + UTF-8 bytes.
//! `commit` uploads a full frame in-band; `damage` is only a hint in v1.
//! Unknown message types and trailing payload bytes are rejected, matching the
//! server's decoder.

use std::fmt;
use std::io::{self, Read, Write};

pub const MAGIC: [u8; 4] = *b"PDS1";
pub const VERSION: u32 = 1;
pub const HEADER_LEN: usize = 12;
/// Maximum payload: a 4096x4096 XRGB frame plus the commit header.
pub const MAX_PAYLOAD: usize = 4096 * 4096 * 4 + 4096;
/// Maximum surface side accepted by the compositor.
pub const MAX_SURFACE_DIM: u32 = 4096;
/// Maximum extra stride padding accepted by the compositor.
pub const MAX_STRIDE_PADDING: u32 = 4096;
/// Maximum damage rects in one shm `COMMIT`/`DAMAGE` message.
pub const MAX_DAMAGE_RECTS: usize = 1024;

pub mod types {
    pub const HELLO: u16 = 1;
    pub const WELCOME: u16 = 2;
    pub const CREATE: u16 = 3;
    pub const CONFIGURE: u16 = 4;
    pub const COMMIT: u16 = 5;
    pub const INPUT: u16 = 6;
    pub const FOCUS: u16 = 7;
    pub const CLOSE: u16 = 8;
    pub const DAMAGE: u16 = 9;
    pub const ERROR: u16 = 10;
    pub const PING: u16 = 11;
    pub const PONG: u16 = 12;
    /// c→s: shared surface offer (memfd via SCM_RIGHTS).
    pub const SHM_OFFER: u16 = 14;
    /// s→c: server accepted the offer and assigned a generation.
    pub const SHM_READY: u16 = 15;
    /// either direction: generation released.
    pub const SHM_RELEASE: u16 = 16;
}

/// PDP/1 header flag bits.
pub mod header_flags {
    /// Bit 0: payload is the shm form of `COMMIT`/`DAMAGE` (no pixel bytes).
    pub const SHM: u16 = 1 << 0;
}

pub mod welcome_flags {
    pub const FORMAT_XRGB8888: u32 = 1 << 0;
    pub const FORMAT_RGB565: u32 = 1 << 1;
    pub const SHM_SURFACE: u32 = 1 << 2;
}

pub mod configure_flags {
    pub const FOCUSED: u32 = 1 << 0;
    pub const MAXIMIZED: u32 = 1 << 1;
    pub const MINIMIZED: u32 = 1 << 2;
    pub const RESIZABLE: u32 = 1 << 3;
}

pub mod close_reason {
    pub const CLIENT: u32 = 0;
    pub const SERVER: u32 = 1;
    pub const DISCONNECT: u32 = 2;
    pub const PROTOCOL_ERROR: u32 = 3;
    pub const DESTROYED: u32 = 4;
}

pub mod error_codes {
    pub const BAD_MESSAGE: u32 = 1;
    pub const NOT_HELLO: u32 = 2;
    pub const DUPLICATE_WINDOW: u32 = 3;
    pub const UNKNOWN_WINDOW: u32 = 4;
    pub const NOT_OWNER: u32 = 5;
    pub const INVALID_SURFACE: u32 = 6;
    pub const LIMIT: u32 = 7;
    pub const UNSUPPORTED: u32 = 8;
    pub const INTERNAL: u32 = 9;
    /// shm commit with a generation the server already evicted.
    pub const STALE_GENERATION: u32 = 10;
}

pub mod buttons {
    /// Button identifier in `input.button`.
    pub const LEFT: u32 = 1;
    pub const MIDDLE: u32 = 2;
    pub const RIGHT: u32 = 3;
    /// Mask of held buttons in `input.buttons`.
    pub const MASK_LEFT: u32 = 1 << 0;
    pub const MASK_MIDDLE: u32 = 1 << 1;
    pub const MASK_RIGHT: u32 = 1 << 2;
}

pub mod mods {
    pub const SHIFT: u32 = 1 << 0;
    pub const CTRL: u32 = 1 << 1;
    pub const ALT: u32 = 1 << 2;
    pub const SUPER: u32 = 1 << 3;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    BadMagic,
    ReservedFlags(u16),
    TooLarge { length: usize },
    UnknownType(u16),
    BadPayload(String),
    Truncated(&'static str),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::BadMagic => write!(f, "not a PDS1 frame"),
            ProtocolError::ReservedFlags(flags) => {
                write!(f, "reserved header flags are non-zero ({flags:#x})")
            }
            ProtocolError::TooLarge { length } => {
                write!(f, "PDP/1 payload too large ({length} bytes)")
            }
            ProtocolError::UnknownType(kind) => write!(f, "unknown PDP/1 message type {kind}"),
            ProtocolError::BadPayload(what) => write!(f, "bad PDP/1 payload: {what}"),
            ProtocolError::Truncated(what) => write!(f, "truncated PDP/1 {what}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<ProtocolError> for io::Error {
    fn from(error: ProtocolError) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }
}

/// Pixel format as written on the wire (1 = XRGB8888, 2 = RGB565).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameFormat {
    Xrgb8888,
    Rgb565,
}

impl FrameFormat {
    pub const XRGB8888_WIRE: u32 = 1;
    pub const RGB565_WIRE: u32 = 2;

    pub fn from_wire(value: u32) -> Option<FrameFormat> {
        match value {
            Self::XRGB8888_WIRE => Some(FrameFormat::Xrgb8888),
            Self::RGB565_WIRE => Some(FrameFormat::Rgb565),
            _ => None,
        }
    }

    pub fn to_wire(self) -> u32 {
        match self {
            FrameFormat::Xrgb8888 => Self::XRGB8888_WIRE,
            FrameFormat::Rgb565 => Self::RGB565_WIRE,
        }
    }

    pub fn bytes_per_pixel(self) -> usize {
        match self {
            FrameFormat::Xrgb8888 => 4,
            FrameFormat::Rgb565 => 2,
        }
    }

    /// Pick the preferred format from the `welcome.flags` mask.  XRGB8888 is
    /// preferred because it needs no quantization.
    pub fn from_welcome_flags(flags: u32) -> FrameFormat {
        if flags & welcome_flags::FORMAT_XRGB8888 != 0 {
            FrameFormat::Xrgb8888
        } else if flags & welcome_flags::FORMAT_RGB565 != 0 {
            FrameFormat::Rgb565
        } else {
            FrameFormat::Xrgb8888
        }
    }
}

/// Input event delivered by the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    Motion {
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
        buttons: u32,
    },
    Button {
        x: i32,
        y: i32,
        button: u32,
        pressed: bool,
        buttons: u32,
    },
    Key {
        code: u32,
        pressed: bool,
        modifiers: u32,
    },
    Axis {
        x: i32,
        y: i32,
        horizontal: i32,
        vertical: i32,
        buttons: u32,
    },
}

impl InputEvent {
    pub fn kind(self) -> u8 {
        match self {
            InputEvent::Motion { .. } => 1,
            InputEvent::Button { .. } => 2,
            InputEvent::Key { .. } => 3,
            InputEvent::Axis { .. } => 4,
        }
    }
}

/// PDP/1 window configure event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Configure {
    pub window_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub flags: u32,
}

impl Configure {
    pub fn focused(&self) -> bool {
        self.flags & configure_flags::FOCUSED != 0
    }
}

/// Split a PDP/1 header without consuming the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: u16,
    pub flags: u16,
    pub length: usize,
}

impl Header {
    pub fn parse(bytes: &[u8]) -> Result<Header, ProtocolError> {
        if bytes.len() < HEADER_LEN {
            return Err(ProtocolError::Truncated("header"));
        }
        if bytes[..4] != MAGIC {
            return Err(ProtocolError::BadMagic);
        }
        let kind = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
        let flags = u16::from_le_bytes(bytes[6..8].try_into().unwrap());
        if flags & !header_flags::SHM != 0 {
            return Err(ProtocolError::ReservedFlags(flags));
        }
        if flags != 0 && kind != self::types::COMMIT && kind != self::types::DAMAGE {
            return Err(ProtocolError::ReservedFlags(flags));
        }
        let length = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        if length > MAX_PAYLOAD {
            return Err(ProtocolError::TooLarge { length });
        }
        Ok(Header {
            kind,
            flags,
            length,
        })
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..4].copy_from_slice(&MAGIC);
        out[4..6].copy_from_slice(&self.kind.to_le_bytes());
        out[6..8].copy_from_slice(&self.flags.to_le_bytes());
        out[8..12].copy_from_slice(&(self.length as u32).to_le_bytes());
        out
    }
}

/// One PDP/1 message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello {
        version: u32,
        pid: u32,
        app: String,
    },
    Welcome {
        version: u32,
        client_id: u64,
        width: u32,
        height: u32,
        flags: u32,
    },
    Create {
        window_id: u32,
        width: u32,
        height: u32,
        format: u32,
        title: String,
    },
    Configure {
        window_id: u32,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        flags: u32,
    },
    Commit {
        window_id: u32,
        width: u32,
        height: u32,
        format: u32,
        stride: u32,
        pixels: Vec<u8>,
    },
    Input {
        window_id: u32,
        event: InputEvent,
    },
    Focus {
        window_id: u32,
        focused: bool,
    },
    Close {
        window_id: u32,
        reason: u32,
    },
    Damage {
        window_id: u32,
        rects: Vec<(i32, i32, u16, u16)>,
    },
    Error {
        code: u32,
        message: String,
    },
    Ping {
        nonce: u32,
    },
    Pong {
        nonce: u32,
    },
    /// c→s: shared surface geometry; the memfd rides in `SCM_RIGHTS`.
    ShmOffer {
        window_id: u32,
        width: u32,
        height: u32,
        format: u32,
        stride: u32,
        flags: u32,
    },
    /// s→c: server accepted the surface and assigned `generation`.
    ShmReady {
        window_id: u32,
        generation: u32,
    },
    /// either direction: generation can be released.
    ShmRelease {
        window_id: u32,
        generation: u32,
    },
    /// c→s: shm commit — damage rects only, pixels live in the memfd.
    CommitShm {
        window_id: u32,
        generation: u32,
        rects: Vec<(i32, i32, u16, u16)>,
    },
}

impl Message {
    pub fn kind(&self) -> u16 {
        match self {
            Message::Hello { .. } => types::HELLO,
            Message::Welcome { .. } => types::WELCOME,
            Message::Create { .. } => types::CREATE,
            Message::Configure { .. } => types::CONFIGURE,
            Message::Commit { .. } => types::COMMIT,
            Message::Input { .. } => types::INPUT,
            Message::Focus { .. } => types::FOCUS,
            Message::Close { .. } => types::CLOSE,
            Message::Damage { .. } => types::DAMAGE,
            Message::Error { .. } => types::ERROR,
            Message::Ping { .. } => types::PING,
            Message::Pong { .. } => types::PONG,
            Message::ShmOffer { .. } => types::SHM_OFFER,
            Message::ShmReady { .. } => types::SHM_READY,
            Message::ShmRelease { .. } => types::SHM_RELEASE,
            Message::CommitShm { .. } => types::COMMIT,
        }
    }

    /// Header flag bits this message requires.
    pub fn header_flags(&self) -> u16 {
        match self {
            Message::CommitShm { .. } => header_flags::SHM,
            _ => 0,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Message::Hello { .. } => "hello",
            Message::Welcome { .. } => "welcome",
            Message::Create { .. } => "create",
            Message::Configure { .. } => "configure",
            Message::Commit { .. } => "commit",
            Message::Input { .. } => "input",
            Message::Focus { .. } => "focus",
            Message::Close { .. } => "close",
            Message::Damage { .. } => "damage",
            Message::Error { .. } => "error",
            Message::Ping { .. } => "ping",
            Message::Pong { .. } => "pong",
            Message::ShmOffer { .. } => "shm_offer",
            Message::ShmReady { .. } => "shm_ready",
            Message::ShmRelease { .. } => "shm_release",
            Message::CommitShm { .. } => "commit_shm",
        }
    }

    /// Encode header + payload.
    pub fn encode(&self) -> Vec<u8> {
        let payload = self.encode_payload();
        let header = Header {
            kind: self.kind(),
            flags: self.header_flags(),
            length: payload.len(),
        };
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(&payload);
        out
    }

    fn encode_payload(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Message::Hello { version, pid, app } => {
                w.u32(*version);
                w.u32(*pid);
                w.string(app);
            }
            Message::Welcome {
                version,
                client_id,
                width,
                height,
                flags,
            } => {
                w.u32(*version);
                w.u64(*client_id);
                w.u32(*width);
                w.u32(*height);
                w.u32(*flags);
            }
            Message::Create {
                window_id,
                width,
                height,
                format,
                title,
            } => {
                w.u32(*window_id);
                w.u32(*width);
                w.u32(*height);
                w.u32(*format);
                w.string(title);
            }
            Message::Configure {
                window_id,
                x,
                y,
                width,
                height,
                flags,
            } => {
                w.u32(*window_id);
                w.i32(*x);
                w.i32(*y);
                w.u32(*width);
                w.u32(*height);
                w.u32(*flags);
            }
            Message::Commit {
                window_id,
                width,
                height,
                format,
                stride,
                pixels,
            } => {
                w.u32(*window_id);
                w.u32(*width);
                w.u32(*height);
                w.u32(*format);
                w.u32(*stride);
                w.u32(pixels.len() as u32);
                w.bytes(pixels);
            }
            Message::Input { window_id, event } => {
                w.u32(*window_id);
                w.u8(event.kind());
                match *event {
                    InputEvent::Motion {
                        x,
                        y,
                        dx,
                        dy,
                        buttons,
                    } => {
                        w.u8(0);
                        w.u16(0);
                        w.u32(0);
                        w.i32(x);
                        w.i32(y);
                        w.i32(dx);
                        w.i32(dy);
                        w.u32(buttons);
                    }
                    InputEvent::Button {
                        x,
                        y,
                        button,
                        pressed,
                        buttons,
                    } => {
                        w.u8(u8::from(pressed));
                        w.u16(0);
                        w.u32(button);
                        w.i32(x);
                        w.i32(y);
                        w.i32(0);
                        w.i32(0);
                        w.u32(buttons);
                    }
                    InputEvent::Key {
                        code,
                        pressed,
                        modifiers,
                    } => {
                        w.u8(u8::from(pressed));
                        w.u16(modifiers as u16);
                        w.u32(code);
                        w.i32(0);
                        w.i32(0);
                        w.i32(0);
                        w.i32(0);
                        w.u32(0);
                    }
                    InputEvent::Axis {
                        x,
                        y,
                        horizontal,
                        vertical,
                        buttons,
                    } => {
                        w.u8(0);
                        w.u16(0);
                        w.u32(0);
                        w.i32(x);
                        w.i32(y);
                        w.i32(horizontal);
                        w.i32(vertical);
                        w.u32(buttons);
                    }
                }
            }
            Message::Focus { window_id, focused } => {
                w.u32(*window_id);
                w.u8(u8::from(*focused));
                w.bytes(&[0, 0, 0]);
            }
            Message::Close { window_id, reason } => {
                w.u32(*window_id);
                w.u32(*reason);
            }
            Message::Damage { window_id, rects } => {
                w.u32(*window_id);
                w.u16(rects.len().min(u16::MAX as usize) as u16);
                for (x, y, width, height) in rects.iter().take(u16::MAX as usize) {
                    w.i32(*x);
                    w.i32(*y);
                    w.u16(*width);
                    w.u16(*height);
                }
            }
            Message::Error { code, message } => {
                w.u32(*code);
                w.string(message);
            }
            Message::Ping { nonce } | Message::Pong { nonce } => w.u32(*nonce),
            Message::ShmOffer {
                window_id,
                width,
                height,
                format,
                stride,
                flags,
            } => {
                w.u32(*window_id);
                w.u32(*width);
                w.u32(*height);
                w.u32(*format);
                w.u32(*stride);
                w.u32(*flags);
            }
            Message::ShmReady {
                window_id,
                generation,
            }
            | Message::ShmRelease {
                window_id,
                generation,
            } => {
                w.u32(*window_id);
                w.u32(*generation);
            }
            Message::CommitShm {
                window_id,
                generation,
                rects,
            } => {
                w.u32(*window_id);
                w.u32(*generation);
                w.u32(rects.len().min(MAX_DAMAGE_RECTS) as u32);
                for (x, y, width, height) in rects.iter().take(MAX_DAMAGE_RECTS) {
                    w.i32(*x);
                    w.i32(*y);
                    w.u16(*width);
                    w.u16(*height);
                }
            }
        }
        w.buf
    }

    /// Decode a payload for `kind`; the payload must be consumed exactly.
    /// Header flags are assumed to be zero (v1); shm forms need
    /// [`Message::decode_with_flags`].
    pub fn decode(kind: u16, payload: &[u8]) -> Result<Message, ProtocolError> {
        Message::decode_with_flags(kind, 0, payload)
    }

    /// Decode a payload taking the header flags (`header_flags::SHM`) into account.
    pub fn decode_with_flags(
        kind: u16,
        flags: u16,
        payload: &[u8],
    ) -> Result<Message, ProtocolError> {
        if flags & !header_flags::SHM != 0 {
            return Err(ProtocolError::ReservedFlags(flags));
        }
        let shm = flags & header_flags::SHM != 0;
        if shm && kind != types::COMMIT && kind != types::DAMAGE {
            return Err(ProtocolError::ReservedFlags(flags));
        }
        let mut r = Reader::new(payload);
        let message = match kind {
            types::HELLO => Message::Hello {
                version: r.u32()?,
                pid: r.u32()?,
                app: r.string()?,
            },
            types::WELCOME => Message::Welcome {
                version: r.u32()?,
                client_id: r.u64()?,
                width: r.u32()?,
                height: r.u32()?,
                flags: r.u32()?,
            },
            types::CREATE => Message::Create {
                window_id: r.u32()?,
                width: r.u32()?,
                height: r.u32()?,
                format: r.u32()?,
                title: r.string()?,
            },
            types::CONFIGURE => Message::Configure {
                window_id: r.u32()?,
                x: r.i32()?,
                y: r.i32()?,
                width: r.u32()?,
                height: r.u32()?,
                flags: r.u32()?,
            },
            types::COMMIT if shm => {
                let window_id = r.u32()?;
                let generation = r.u32()?;
                let count = r.u32()? as usize;
                if count > MAX_DAMAGE_RECTS {
                    return Err(ProtocolError::BadPayload(format!(
                        "too many shm damage rects: {count}"
                    )));
                }
                let mut rects = Vec::with_capacity(count);
                for _ in 0..count {
                    rects.push((r.i32()?, r.i32()?, r.u16()?, r.u16()?));
                }
                Message::CommitShm {
                    window_id,
                    generation,
                    rects,
                }
            }
            types::COMMIT => {
                let window_id = r.u32()?;
                let width = r.u32()?;
                let height = r.u32()?;
                let format = r.u32()?;
                let stride = r.u32()?;
                let data_len = r.u32()? as usize;
                let pixels = r.bytes(data_len)?.to_vec();
                validate_commit(width, height, format, stride, pixels.len())?;
                Message::Commit {
                    window_id,
                    width,
                    height,
                    format,
                    stride,
                    pixels,
                }
            }
            types::INPUT => {
                let window_id = r.u32()?;
                let kind_byte = r.u8()?;
                let pressed = r.u8()? != 0;
                let modifiers = u32::from(r.u16()?);
                let code = r.u32()?;
                let x = r.i32()?;
                let y = r.i32()?;
                let dx = r.i32()?;
                let dy = r.i32()?;
                let buttons = r.u32()?;
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
                            "unknown input kind {other}"
                        )))
                    }
                };
                Message::Input { window_id, event }
            }
            types::FOCUS => {
                let window_id = r.u32()?;
                let focused = r.u8()? != 0;
                r.bytes(3)?;
                Message::Focus { window_id, focused }
            }
            types::CLOSE => Message::Close {
                window_id: r.u32()?,
                reason: r.u32()?,
            },
            types::DAMAGE if shm => {
                let window_id = r.u32()?;
                let generation = r.u32()?;
                let count = r.u32()? as usize;
                if count > MAX_DAMAGE_RECTS {
                    return Err(ProtocolError::BadPayload(format!(
                        "too many shm damage rects: {count}"
                    )));
                }
                let mut rects = Vec::with_capacity(count);
                for _ in 0..count {
                    rects.push((r.i32()?, r.i32()?, r.u16()?, r.u16()?));
                }
                let _ = generation;
                Message::Damage { window_id, rects }
            }
            types::DAMAGE => {
                let window_id = r.u32()?;
                let count = r.u16()? as usize;
                let mut rects = Vec::with_capacity(count);
                for _ in 0..count {
                    rects.push((r.i32()?, r.i32()?, r.u16()?, r.u16()?));
                }
                Message::Damage { window_id, rects }
            }
            types::ERROR => Message::Error {
                code: r.u32()?,
                message: r.string()?,
            },
            types::PING => Message::Ping { nonce: r.u32()? },
            types::PONG => Message::Pong { nonce: r.u32()? },
            types::SHM_OFFER => Message::ShmOffer {
                window_id: r.u32()?,
                width: r.u32()?,
                height: r.u32()?,
                format: r.u32()?,
                stride: r.u32()?,
                flags: r.u32()?,
            },
            types::SHM_READY => Message::ShmReady {
                window_id: r.u32()?,
                generation: r.u32()?,
            },
            types::SHM_RELEASE => Message::ShmRelease {
                window_id: r.u32()?,
                generation: r.u32()?,
            },
            other => return Err(ProtocolError::UnknownType(other)),
        };
        r.finish()?;
        Ok(message)
    }
}

fn validate_commit(
    width: u32,
    height: u32,
    format: u32,
    stride: u32,
    pixel_len: usize,
) -> Result<(), ProtocolError> {
    let format = FrameFormat::from_wire(format)
        .ok_or_else(|| ProtocolError::BadPayload("unknown pixel format".into()))?;
    if width == 0 || height == 0 || width > MAX_SURFACE_DIM || height > MAX_SURFACE_DIM {
        return Err(ProtocolError::BadPayload(
            "surface size out of range".into(),
        ));
    }
    let bpp = format.bytes_per_pixel() as u32;
    let minimum = width.saturating_mul(bpp);
    if stride < minimum {
        return Err(ProtocolError::BadPayload(
            "stride smaller than width".into(),
        ));
    }
    if stride > minimum.saturating_add(MAX_STRIDE_PADDING) {
        return Err(ProtocolError::BadPayload("stride padding too large".into()));
    }
    let expected = (stride as usize)
        .checked_mul(height as usize)
        .ok_or(ProtocolError::BadPayload("frame size overflow".into()))?;
    if pixel_len != expected {
        return Err(ProtocolError::BadPayload(format!(
            "pixel length {pixel_len} != stride*height {expected}"
        )));
    }
    Ok(())
}

/// Write one message to a stream.
pub fn write_message<W: Write>(writer: &mut W, message: &Message) -> io::Result<()> {
    writer.write_all(&message.encode())
}

/// Read one message from a blocking stream (used by tests and simple clients).
pub fn read_message<R: Read>(reader: &mut R) -> io::Result<Message> {
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header)?;
    let header = Header::parse(&header)?;
    let mut payload = vec![0u8; header.length];
    reader.read_exact(&mut payload)?;
    Ok(Message::decode_with_flags(
        header.kind,
        header.flags,
        &payload,
    )?)
}

/// Incremental frame decoder.  Feed it arbitrary socket chunks; it returns
/// complete messages and keeps partial frames buffered.
#[derive(Debug, Default)]
pub struct MessageReader {
    buffer: Vec<u8>,
    limit: Option<usize>,
}

impl MessageReader {
    pub fn new() -> MessageReader {
        MessageReader {
            buffer: Vec::new(),
            limit: None,
        }
    }

    pub fn with_limit(limit: usize) -> MessageReader {
        MessageReader {
            buffer: Vec::new(),
            limit: Some(limit),
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    pub fn next(&mut self) -> Result<Option<Message>, ProtocolError> {
        if self.buffer.len() < HEADER_LEN {
            return Ok(None);
        }
        let header = Header::parse(&self.buffer)?;
        let limit = self.limit.unwrap_or(MAX_PAYLOAD);
        if header.length > limit {
            return Err(ProtocolError::TooLarge {
                length: header.length,
            });
        }
        let total = HEADER_LEN + header.length;
        if self.buffer.len() < total {
            return Ok(None);
        }
        let payload = self.buffer[HEADER_LEN..total].to_vec();
        self.buffer.drain(..total);
        Message::decode(header.kind, &payload).map(Some)
    }
}

// ── pixel packing ──────────────────────────────────────────────────────────

/// Pack straight RGBA into XRGB8888 (`0x00RRGGBB` little-endian: B,G,R,X).
pub fn pack_xrgb8888(rgba: &[u8], width: usize, height: usize) -> Result<Vec<u8>, ProtocolError> {
    if rgba.len() != width * height * 4 {
        return Err(ProtocolError::BadPayload("rgba length mismatch".into()));
    }
    let mut out = Vec::with_capacity(width * height * 4);
    for px in rgba.chunks_exact(4) {
        out.extend_from_slice(&[px[2], px[1], px[0], 0]);
    }
    Ok(out)
}

pub fn unpack_xrgb8888(data: &[u8], width: usize, height: usize) -> Option<Vec<u8>> {
    if data.len() != width * height * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    for px in data.chunks_exact(4) {
        out.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    Some(out)
}

/// Pack straight RGBA into RGB565 little-endian.
pub fn pack_rgb565(rgba: &[u8], width: usize, height: usize) -> Result<Vec<u8>, ProtocolError> {
    if rgba.len() != width * height * 4 {
        return Err(ProtocolError::BadPayload("rgba length mismatch".into()));
    }
    let mut out = Vec::with_capacity(width * height * 2);
    for px in rgba.chunks_exact(4) {
        let r = u16::from(px[0] >> 3);
        let g = u16::from(px[1] >> 2);
        let b = u16::from(px[2] >> 3);
        out.extend_from_slice(&((r << 11) | (g << 5) | b).to_le_bytes());
    }
    Ok(out)
}

pub fn unpack_rgb565(data: &[u8], width: usize, height: usize) -> Option<Vec<u8>> {
    if data.len() != width * height * 2 {
        return None;
    }
    let mut out = Vec::with_capacity(width * height * 4);
    for px in data.chunks_exact(2) {
        let v = u16::from_le_bytes([px[0], px[1]]);
        out.extend_from_slice(&[
            (((v >> 11) & 0x1F) as u8) << 3,
            (((v >> 5) & 0x3F) as u8) << 2,
            ((v & 0x1F) as u8) << 3,
            255,
        ]);
    }
    Some(out)
}

// ── internal codecs ────────────────────────────────────────────────────────

struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn new() -> Writer {
        Writer { buf: Vec::new() }
    }
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }
    fn string(&mut self, value: &str) {
        let bytes = value.as_bytes();
        let len = bytes.len().min(u16::MAX as usize);
        self.u16(len as u16);
        self.bytes(&bytes[..len]);
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }
    fn take(&mut self, len: usize) -> Result<&'a [u8], ProtocolError> {
        if self.pos + len > self.data.len() {
            return Err(ProtocolError::BadPayload(format!(
                "need {len} bytes, have {}",
                self.data.len().saturating_sub(self.pos)
            )));
        }
        let out = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ProtocolError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, ProtocolError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, ProtocolError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, ProtocolError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self, len: usize) -> Result<&'a [u8], ProtocolError> {
        self.take(len)
    }
    fn string(&mut self) -> Result<String, ProtocolError> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| ProtocolError::BadPayload("string is not UTF-8".into()))
    }
    fn finish(&self) -> Result<(), ProtocolError> {
        if self.pos != self.data.len() {
            return Err(ProtocolError::BadPayload(format!(
                "{} trailing payload bytes",
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
        let mut reader = MessageReader::new();
        reader.push(&encoded);
        assert_eq!(reader.next().unwrap(), Some(message));
        assert!(reader.next().unwrap().is_none());
        assert_eq!(reader.buffered(), 0);
    }

    #[test]
    fn header_is_stable() {
        let encoded = Message::Ping { nonce: 0x0102_0304 }.encode();
        assert_eq!(&encoded[0..4], b"PDS1");
        assert_eq!(&encoded[4..6], &types::PING.to_le_bytes());
        assert_eq!(&encoded[6..8], &[0, 0]);
        assert_eq!(&encoded[8..12], &4u32.to_le_bytes());
        assert_eq!(&encoded[12..16], &[4, 3, 2, 1]);
    }

    #[test]
    fn all_messages_roundtrip() {
        roundtrip(Message::Hello {
            version: VERSION,
            pid: 42,
            app: "pagh-terminal".into(),
        });
        roundtrip(Message::Welcome {
            version: VERSION,
            client_id: 7,
            width: 1280,
            height: 720,
            flags: 3,
        });
        roundtrip(Message::Create {
            window_id: 101,
            width: 640,
            height: 400,
            format: FrameFormat::Xrgb8888.to_wire(),
            title: "Terminal — pagh".into(),
        });
        roundtrip(Message::Configure {
            window_id: 101,
            x: -20,
            y: 30,
            width: 640,
            height: 400,
            flags: configure_flags::FOCUSED | configure_flags::RESIZABLE,
        });
        roundtrip(Message::Commit {
            window_id: 101,
            width: 2,
            height: 1,
            format: FrameFormat::Xrgb8888.to_wire(),
            stride: 8,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8],
        });
        roundtrip(Message::Input {
            window_id: 101,
            event: InputEvent::Motion {
                x: 1,
                y: 2,
                dx: -1,
                dy: 3,
                buttons: buttons::MASK_LEFT,
            },
        });
        roundtrip(Message::Input {
            window_id: 101,
            event: InputEvent::Button {
                x: 1,
                y: 2,
                button: buttons::LEFT,
                pressed: true,
                buttons: buttons::MASK_LEFT,
            },
        });
        roundtrip(Message::Input {
            window_id: 101,
            event: InputEvent::Key {
                code: 30,
                pressed: true,
                modifiers: mods::CTRL,
            },
        });
        roundtrip(Message::Input {
            window_id: 101,
            event: InputEvent::Axis {
                x: 1,
                y: 2,
                horizontal: 0,
                vertical: -1,
                buttons: 0,
            },
        });
        roundtrip(Message::Focus {
            window_id: 101,
            focused: false,
        });
        roundtrip(Message::Close {
            window_id: 101,
            reason: close_reason::SERVER,
        });
        roundtrip(Message::Damage {
            window_id: 101,
            rects: vec![(0, 0, 10, 20), (5, 5, 1, 1)],
        });
        roundtrip(Message::Error {
            code: error_codes::INTERNAL,
            message: "ошибка".into(),
        });
        roundtrip(Message::Ping { nonce: 99 });
        roundtrip(Message::Pong { nonce: 99 });
    }

    #[test]
    fn fragmented_and_coalesced_frames() {
        let mut wire = Message::Ping { nonce: 1 }.encode();
        wire.extend_from_slice(&Message::Pong { nonce: 2 }.encode());
        let mut reader = MessageReader::new();
        for byte in &wire {
            reader.push(&[*byte]);
        }
        assert_eq!(reader.next().unwrap(), Some(Message::Ping { nonce: 1 }));
        assert_eq!(reader.next().unwrap(), Some(Message::Pong { nonce: 2 }));
        assert!(reader.next().unwrap().is_none());
    }

    #[test]
    fn rejects_bad_magic_flags_type_and_size() {
        let mut bytes = Message::Ping { nonce: 1 }.encode();
        bytes[0] = b'X';
        let mut reader = MessageReader::new();
        reader.push(&bytes);
        assert!(matches!(reader.next(), Err(ProtocolError::BadMagic)));

        let mut bytes = Message::Ping { nonce: 1 }.encode();
        bytes[6] = 1;
        let mut reader = MessageReader::new();
        reader.push(&bytes);
        assert!(matches!(
            reader.next(),
            Err(ProtocolError::ReservedFlags(1))
        ));

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&999u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let mut reader = MessageReader::new();
        reader.push(&bytes);
        assert!(matches!(
            reader.next(),
            Err(ProtocolError::UnknownType(999))
        ));

        let mut header = Vec::new();
        header.extend_from_slice(&MAGIC);
        header.extend_from_slice(&types::PING.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());
        let mut reader = MessageReader::new();
        reader.push(&header);
        assert!(matches!(reader.next(), Err(ProtocolError::TooLarge { .. })));
    }

    #[test]
    fn rejects_trailing_payload_and_bad_commit() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&types::PING.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&5u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(0xAA);
        let mut reader = MessageReader::new();
        reader.push(&bytes);
        assert!(matches!(reader.next(), Err(ProtocolError::BadPayload(_))));

        let bad = Message::Commit {
            window_id: 1,
            width: 4,
            height: 2,
            format: FrameFormat::Xrgb8888.to_wire(),
            stride: 8, // smaller than width * 4
            pixels: vec![0; 16],
        };
        let mut reader = MessageReader::new();
        reader.push(&bad.encode());
        assert!(matches!(reader.next(), Err(ProtocolError::BadPayload(_))));
    }

    #[test]
    fn pixel_roundtrips() {
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255];
        let xrgb = pack_xrgb8888(&rgba, 2, 1).unwrap();
        assert_eq!(xrgb, vec![0, 0, 255, 0, 0, 255, 0, 0]);
        assert_eq!(unpack_xrgb8888(&xrgb, 2, 1).unwrap(), rgba);
        let rgb565 = pack_rgb565(&rgba, 2, 1).unwrap();
        let back = unpack_rgb565(&rgb565, 2, 1).unwrap();
        assert_eq!(&back[..3], &[0xF8, 0, 0]);
        assert_eq!(&back[4..7], &[0, 0xFC, 0]);
    }

    #[test]
    fn format_from_welcome_flags() {
        assert_eq!(
            FrameFormat::from_welcome_flags(welcome_flags::FORMAT_RGB565),
            FrameFormat::Rgb565
        );
        assert_eq!(
            FrameFormat::from_welcome_flags(
                welcome_flags::FORMAT_RGB565 | welcome_flags::FORMAT_XRGB8888
            ),
            FrameFormat::Xrgb8888
        );
        assert_eq!(FrameFormat::from_welcome_flags(0), FrameFormat::Xrgb8888);
    }
}
