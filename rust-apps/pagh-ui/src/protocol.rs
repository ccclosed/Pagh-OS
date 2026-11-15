//! Клиентская сторона протокола Pagh desktop B (PGUI v1).
//!
//! Транспорт — `AF_UNIX` `SOCK_STREAM`: сначала 4 байта длины кадра (little
//! endian), затем полезная нагрузка `[u8 kind][body]`. Кадр целиком помещается
//! в один `write`, поэтому сервер может читать `len` и ровно `len` байт.
//!
//! Клиент шлёт `hello`, `create_window`, `commit_frame` и `close`, получает
//! `welcome`, `configure`, `input`, `closed` и `error`. Полное описание ABI —
//! в `PROTOCOL.md` рядом с крейтом.

use crate::color::Color;
use crate::event::{Event, Key, Modifiers, MouseButton};
use crate::geom::{Point, Size};

/// Сигнатура протокола: ASCII «PGUI».
pub const MAGIC: u32 = 0x5047_5549;
/// Версия протокола.
pub const VERSION: u16 = 1;
/// Верхняя граница кадра, которую клиент соглашается принять.
pub const MAX_FRAME: u32 = 16 * 1024 * 1024;

// ───────────────────────────── Форматы пикселей ─────────────────────────────

/// Формат пикселей поверхности окна.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PixelFormat {
    /// 32 бита, старший байт игнорируется: `0x00RRGGBB`.
    Xrgb8888,
    /// 32 бита с альфой: `0xAARRGGBB`.
    Argb8888,
    /// 16 бит: `RRRRRGGGGGGBBBBB`.
    Rgb565,
}

impl PixelFormat {
    pub const fn code(self) -> u8 {
        match self {
            PixelFormat::Xrgb8888 => 1,
            PixelFormat::Argb8888 => 2,
            PixelFormat::Rgb565 => 3,
        }
    }

    pub fn from_code(code: u8) -> Option<PixelFormat> {
        match code {
            1 => Some(PixelFormat::Xrgb8888),
            2 => Some(PixelFormat::Argb8888),
            3 => Some(PixelFormat::Rgb565),
            _ => None,
        }
    }

    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            PixelFormat::Xrgb8888 | PixelFormat::Argb8888 => 4,
            PixelFormat::Rgb565 => 2,
        }
    }
}

/// Упаковать RGBA-поверхность в кадр выбранного формата (построчно).
pub fn pack_pixels(pixels: &[u8], format: PixelFormat) -> Vec<u8> {
    let count = pixels.len() / 4;
    let mut out = Vec::with_capacity(count * format.bytes_per_pixel() as usize);
    for index in 0..count {
        let base = index * 4;
        let color = Color::rgba(
            pixels[base],
            pixels[base + 1],
            pixels[base + 2],
            pixels[base + 3],
        );
        match format {
            PixelFormat::Xrgb8888 => out.extend_from_slice(&color.to_rgb32().to_le_bytes()),
            PixelFormat::Argb8888 => out.extend_from_slice(&color.to_argb32().to_le_bytes()),
            PixelFormat::Rgb565 => {
                let value = ((color.r as u16 >> 3) << 11)
                    | ((color.g as u16 >> 2) << 5)
                    | (color.b as u16 >> 3);
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    out
}

// ───────────────────────────── Сообщения ─────────────────────────────

/// Сообщение клиента композитору.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ClientMessage {
    /// Приветствие: pid, имя приложения, битовая маска возможностей.
    Hello {
        pid: u32,
        app: String,
        capabilities: u32,
    },
    /// Создать поверхность окна.
    CreateWindow {
        window_id: u32,
        width: u16,
        height: u16,
        format: PixelFormat,
        title: String,
    },
    /// Отправить кадр окна (пиксели в выбранном формате).
    CommitFrame {
        window_id: u32,
        sequence: u32,
        width: u16,
        height: u16,
        stride: u16,
        format: PixelFormat,
        pixels: Vec<u8>,
    },
    /// Закрыть окно.
    Close { window_id: u32 },
}

/// Сообщение композитора клиенту.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ServerMessage {
    /// Ответ на приветствие: параметры экрана и протокола.
    Welcome {
        protocol: u16,
        server_pid: u32,
        screen_width: u16,
        screen_height: u16,
        format: PixelFormat,
        max_frame: u32,
    },
    /// Композитор изменил размер/положение поверхности.
    Configure {
        window_id: u32,
        width: u16,
        height: u16,
    },
    /// Событие ввода для окна.
    Input { window_id: u32, event: InputEvent },
    /// Окно закрыто композитором.
    Closed { window_id: u32 },
    /// Ошибка протокола.
    Error { code: u32, message: String },
}

/// Событие ввода в протоколе.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputEvent {
    PointerMove {
        x: i16,
        y: i16,
    },
    PointerButton {
        x: i16,
        y: i16,
        button: MouseButton,
        pressed: bool,
    },
    Wheel {
        x: i16,
        y: i16,
        delta_x: i16,
        delta_y: i16,
    },
    Key {
        code: u32,
        pressed: bool,
        modifiers: Modifiers,
    },
    Focus {
        gained: bool,
    },
}

impl InputEvent {
    /// Преобразовать сетевое событие в событие тулкита.
    pub fn to_event(self) -> Option<Event> {
        Some(match self {
            InputEvent::PointerMove { x, y } => Event::MouseMove {
                pos: Point::new(x as i32, y as i32),
            },
            InputEvent::PointerButton {
                x,
                y,
                button,
                pressed: true,
            } => Event::MouseDown {
                pos: Point::new(x as i32, y as i32),
                button,
            },
            InputEvent::PointerButton {
                x,
                y,
                button,
                pressed: false,
            } => Event::MouseUp {
                pos: Point::new(x as i32, y as i32),
                button,
            },
            InputEvent::Wheel { x, y, delta_y, .. } => Event::Wheel {
                pos: Point::new(x as i32, y as i32),
                delta: delta_y as i32,
            },
            InputEvent::Key {
                code,
                pressed,
                modifiers,
            } => {
                let key = Key::from_protocol(code)?;
                Event::Key {
                    key,
                    pressed,
                    modifiers,
                }
            }
            InputEvent::Focus { gained } => Event::Focus { gained },
        })
    }
}

// ───────────────────────────── Коды типов кадров ─────────────────────────────

mod kind {
    pub const HELLO: u8 = 1;
    pub const WELCOME: u8 = 2;
    pub const CREATE_WINDOW: u8 = 3;
    pub const CONFIGURE: u8 = 4;
    pub const COMMIT_FRAME: u8 = 5;
    pub const INPUT: u8 = 6;
    pub const CLOSE: u8 = 7;
    pub const CLOSED: u8 = 8;
    pub const ERROR: u8 = 9;
}

/// Ошибка разбора кадра.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProtocolError {
    pub message: String,
}

impl ProtocolError {
    fn new(message: impl Into<String>) -> ProtocolError {
        ProtocolError {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "protocol error: {}", self.message)
    }
}

impl std::error::Error for ProtocolError {}

// ───────────────────────────── Кодирование ─────────────────────────────

struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn new(kind: u8) -> Writer {
        Writer { bytes: vec![kind] }
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn bool(&mut self, value: bool) {
        self.u8(value as u8);
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn i16(&mut self, value: i16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn string(&mut self, value: &str) {
        let bytes = value.as_bytes();
        let len = bytes.len().min(u16::MAX as usize);
        self.u16(len as u16);
        self.bytes.extend_from_slice(&bytes[..len]);
    }

    fn raw(&mut self, value: &[u8]) {
        self.u32(value.len() as u32);
        self.bytes.extend_from_slice(value);
    }

    fn finish(self) -> Vec<u8> {
        frame(self.bytes)
    }
}

fn frame(payload: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Закодировать сообщение клиента в кадр (с префиксом длины).
pub fn encode_client(message: &ClientMessage) -> Vec<u8> {
    match message {
        ClientMessage::Hello {
            pid,
            app,
            capabilities,
        } => {
            let mut w = Writer::new(kind::HELLO);
            w.u32(MAGIC);
            w.u16(VERSION);
            w.u32(*pid);
            w.u32(*capabilities);
            w.string(app);
            w.finish()
        }
        ClientMessage::CreateWindow {
            window_id,
            width,
            height,
            format,
            title,
        } => {
            let mut w = Writer::new(kind::CREATE_WINDOW);
            w.u32(*window_id);
            w.u16(*width);
            w.u16(*height);
            w.u8(format.code());
            w.string(title);
            w.finish()
        }
        ClientMessage::CommitFrame {
            window_id,
            sequence,
            width,
            height,
            stride,
            format,
            pixels,
        } => {
            let mut w = Writer::new(kind::COMMIT_FRAME);
            w.u32(*window_id);
            w.u32(*sequence);
            w.u16(*width);
            w.u16(*height);
            w.u16(*stride);
            w.u8(format.code());
            w.raw(pixels);
            w.finish()
        }
        ClientMessage::Close { window_id } => {
            let mut w = Writer::new(kind::CLOSE);
            w.u32(*window_id);
            w.finish()
        }
    }
}

/// Закодировать сообщение сервера в кадр (для тестов и будущего композитора).
pub fn encode_server(message: &ServerMessage) -> Vec<u8> {
    match message {
        ServerMessage::Welcome {
            protocol,
            server_pid,
            screen_width,
            screen_height,
            format,
            max_frame,
        } => {
            let mut w = Writer::new(kind::WELCOME);
            w.u32(MAGIC);
            w.u16(*protocol);
            w.u32(*server_pid);
            w.u16(*screen_width);
            w.u16(*screen_height);
            w.u8(format.code());
            w.u32(*max_frame);
            w.finish()
        }
        ServerMessage::Configure {
            window_id,
            width,
            height,
        } => {
            let mut w = Writer::new(kind::CONFIGURE);
            w.u32(*window_id);
            w.u16(*width);
            w.u16(*height);
            w.finish()
        }
        ServerMessage::Input { window_id, event } => {
            let mut w = Writer::new(kind::INPUT);
            w.u32(*window_id);
            encode_input(&mut w, *event);
            w.finish()
        }
        ServerMessage::Closed { window_id } => {
            let mut w = Writer::new(kind::CLOSED);
            w.u32(*window_id);
            w.finish()
        }
        ServerMessage::Error { code, message } => {
            let mut w = Writer::new(kind::ERROR);
            w.u32(*code);
            w.string(message);
            w.finish()
        }
    }
}

fn encode_input(w: &mut Writer, event: InputEvent) {
    match event {
        InputEvent::PointerMove { x, y } => {
            w.u8(0);
            w.i16(x);
            w.i16(y);
        }
        InputEvent::PointerButton {
            x,
            y,
            button,
            pressed,
        } => {
            w.u8(1);
            w.i16(x);
            w.i16(y);
            w.u8(button.protocol_code());
            w.bool(pressed);
        }
        InputEvent::Wheel {
            x,
            y,
            delta_x,
            delta_y,
        } => {
            w.u8(2);
            w.i16(x);
            w.i16(y);
            w.i16(delta_x);
            w.i16(delta_y);
        }
        InputEvent::Key {
            code,
            pressed,
            modifiers,
        } => {
            w.u8(3);
            w.u32(code);
            w.bool(pressed);
            w.u8(modifiers.bits());
        }
        InputEvent::Focus { gained } => {
            w.u8(4);
            w.bool(gained);
        }
    }
}

// ───────────────────────────── Декодирование ─────────────────────────────

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], ProtocolError> {
        if self.remaining() < count {
            return Err(ProtocolError::new(format!(
                "кадр короче ожидаемого: нужно {count} байт, есть {}",
                self.remaining()
            )));
        }
        let slice = &self.bytes[self.pos..self.pos + count];
        self.pos += count;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }

    fn bool(&mut self) -> Result<bool, ProtocolError> {
        Ok(self.u8()? != 0)
    }

    fn u16(&mut self) -> Result<u16, ProtocolError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn i16(&mut self) -> Result<i16, ProtocolError> {
        Ok(self.u16()? as i16)
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn string(&mut self) -> Result<String, ProtocolError> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| ProtocolError::new("строка не UTF-8"))
    }

    fn raw(&mut self) -> Result<Vec<u8>, ProtocolError> {
        let len = self.u32()? as usize;
        if len > MAX_FRAME as usize {
            return Err(ProtocolError::new(format!(
                "вложенный буфер слишком велик: {len} байт"
            )));
        }
        Ok(self.take(len)?.to_vec())
    }

    fn format(&mut self) -> Result<PixelFormat, ProtocolError> {
        PixelFormat::from_code(self.u8()?).ok_or_else(|| ProtocolError::new("неизвестный формат"))
    }

    fn magic(&mut self) -> Result<(), ProtocolError> {
        let magic = self.u32()?;
        if magic != MAGIC {
            return Err(ProtocolError::new(format!(
                "неверная сигнатура: {magic:#010x}"
            )));
        }
        Ok(())
    }

    fn protocol(&mut self) -> Result<u16, ProtocolError> {
        let version = self.u16()?;
        if version != VERSION {
            return Err(ProtocolError::new(format!(
                "версия протокола {version}, поддерживается {VERSION}"
            )));
        }
        Ok(version)
    }

    fn finish(&self) -> Result<(), ProtocolError> {
        if self.remaining() != 0 {
            return Err(ProtocolError::new(format!(
                "лишние {} байт в кадре",
                self.remaining()
            )));
        }
        Ok(())
    }
}

/// Разобрать полезную нагрузку кадра клиента (первый байт — тип).
pub fn decode_client(payload: &[u8]) -> Result<ClientMessage, ProtocolError> {
    let mut reader = Reader::new(payload);
    let kind = reader.u8()?;
    let message = match kind {
        kind::HELLO => {
            reader.magic()?;
            reader.protocol()?;
            let pid = reader.u32()?;
            let capabilities = reader.u32()?;
            let app = reader.string()?;
            ClientMessage::Hello {
                pid,
                app,
                capabilities,
            }
        }
        kind::CREATE_WINDOW => {
            let window_id = reader.u32()?;
            let width = reader.u16()?;
            let height = reader.u16()?;
            let format = reader.format()?;
            let title = reader.string()?;
            ClientMessage::CreateWindow {
                window_id,
                width,
                height,
                format,
                title,
            }
        }
        kind::COMMIT_FRAME => {
            let window_id = reader.u32()?;
            let sequence = reader.u32()?;
            let width = reader.u16()?;
            let height = reader.u16()?;
            let stride = reader.u16()?;
            let format = reader.format()?;
            let pixels = reader.raw()?;
            ClientMessage::CommitFrame {
                window_id,
                sequence,
                width,
                height,
                stride,
                format,
                pixels,
            }
        }
        kind::CLOSE => ClientMessage::Close {
            window_id: reader.u32()?,
        },
        other => {
            return Err(ProtocolError::new(format!(
                "неизвестный тип сообщения клиента: {other}"
            )))
        }
    };
    reader.finish()?;
    Ok(message)
}

/// Разобрать полезную нагрузку кадра сервера (первый байт — тип).
pub fn decode_server(payload: &[u8]) -> Result<ServerMessage, ProtocolError> {
    let mut reader = Reader::new(payload);
    let kind = reader.u8()?;
    let message = match kind {
        kind::WELCOME => {
            reader.magic()?;
            let protocol = reader.protocol()?;
            let server_pid = reader.u32()?;
            let screen_width = reader.u16()?;
            let screen_height = reader.u16()?;
            let format = reader.format()?;
            let max_frame = reader.u32()?;
            ServerMessage::Welcome {
                protocol,
                server_pid,
                screen_width,
                screen_height,
                format,
                max_frame,
            }
        }
        kind::CONFIGURE => ServerMessage::Configure {
            window_id: reader.u32()?,
            width: reader.u16()?,
            height: reader.u16()?,
        },
        kind::INPUT => {
            let window_id = reader.u32()?;
            let event = decode_input(&mut reader)?;
            ServerMessage::Input { window_id, event }
        }
        kind::CLOSED => ServerMessage::Closed {
            window_id: reader.u32()?,
        },
        kind::ERROR => ServerMessage::Error {
            code: reader.u32()?,
            message: reader.string()?,
        },
        other => {
            return Err(ProtocolError::new(format!(
                "неизвестный тип сообщения сервера: {other}"
            )))
        }
    };
    reader.finish()?;
    Ok(message)
}

fn decode_input(reader: &mut Reader<'_>) -> Result<InputEvent, ProtocolError> {
    let kind = reader.u8()?;
    Ok(match kind {
        0 => InputEvent::PointerMove {
            x: reader.i16()?,
            y: reader.i16()?,
        },
        1 => InputEvent::PointerButton {
            x: reader.i16()?,
            y: reader.i16()?,
            button: MouseButton::from_protocol(reader.u8()?)
                .ok_or_else(|| ProtocolError::new("неизвестная кнопка мыши"))?,
            pressed: reader.bool()?,
        },
        2 => InputEvent::Wheel {
            x: reader.i16()?,
            y: reader.i16()?,
            delta_x: reader.i16()?,
            delta_y: reader.i16()?,
        },
        3 => InputEvent::Key {
            code: reader.u32()?,
            pressed: reader.bool()?,
            modifiers: Modifiers::from_bits(reader.u8()?),
        },
        4 => InputEvent::Focus {
            gained: reader.bool()?,
        },
        other => {
            return Err(ProtocolError::new(format!(
                "неизвестный тип события ввода: {other}"
            )))
        }
    })
}

/// Накопитель потока: собирает кадры из произвольных чтений сокета.
#[derive(Clone, Debug, Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> FrameReader {
        FrameReader::default()
    }

    /// Добавить прочитанные из сокета байты.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Сколько байт ждёт разбора.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// Забрать следующий полный кадр (полезная нагрузка с типом), если он есть.
    pub fn next_payload(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([
            self.buffer[0],
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
        ]) as usize;
        if len == 0 || len > MAX_FRAME as usize {
            return Err(ProtocolError::new(format!(
                "недопустимая длина кадра: {len}"
            )));
        }
        if self.buffer.len() < 4 + len {
            return Ok(None);
        }
        let payload = self.buffer[4..4 + len].to_vec();
        self.buffer.drain(..4 + len);
        Ok(Some(payload))
    }
}

/// Длина кадра по первым байтам буфера (для диагностики и тестов).
pub fn payload_len(header: &[u8]) -> Option<usize> {
    if header.len() < 4 {
        return None;
    }
    Some(u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize)
}

/// Размер кадра `commit_frame` для поверхности данного размера и формата.
pub fn commit_frame_size(size: Size, format: PixelFormat) -> usize {
    let stride = size.w.max(0) as u32 * format.bytes_per_pixel();
    // 4 (длина кадра) + 1 (тип) + 4+4+2+2+2+1 (поля) + 4 (длина буфера) + пиксели.
    4 + 1 + 4 + 4 + 2 + 2 + 2 + 1 + 4 + (stride * size.h.max(0) as u32) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_client(message: ClientMessage) {
        let frame = encode_client(&message);
        let len = payload_len(&frame).unwrap();
        assert_eq!(len, frame.len() - 4);
        let decoded = decode_client(&frame[4..]).expect("decode");
        assert_eq!(decoded, message);
    }

    fn roundtrip_server(message: ServerMessage) {
        let frame = encode_server(&message);
        let decoded = decode_server(&frame[4..]).expect("decode");
        assert_eq!(decoded, message);
    }

    #[test]
    fn client_messages_roundtrip() {
        roundtrip_client(ClientMessage::Hello {
            pid: 42,
            app: "pagh-files".to_string(),
            capabilities: 0b101,
        });
        roundtrip_client(ClientMessage::CreateWindow {
            window_id: 7,
            width: 1060,
            height: 640,
            format: PixelFormat::Xrgb8888,
            title: "Домашняя папка — Files".to_string(),
        });
        roundtrip_client(ClientMessage::CommitFrame {
            window_id: 7,
            sequence: 3,
            width: 4,
            height: 2,
            stride: 16,
            format: PixelFormat::Argb8888,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        });
        roundtrip_client(ClientMessage::Close { window_id: 7 });
    }

    #[test]
    fn server_messages_roundtrip() {
        roundtrip_server(ServerMessage::Welcome {
            protocol: VERSION,
            server_pid: 1,
            screen_width: 1280,
            screen_height: 800,
            format: PixelFormat::Xrgb8888,
            max_frame: MAX_FRAME,
        });
        roundtrip_server(ServerMessage::Configure {
            window_id: 9,
            width: 800,
            height: 600,
        });
        for event in [
            InputEvent::PointerMove { x: 10, y: 20 },
            InputEvent::PointerButton {
                x: 1,
                y: 2,
                button: MouseButton::Right,
                pressed: true,
            },
            InputEvent::Wheel {
                x: 3,
                y: 4,
                delta_x: 0,
                delta_y: -120,
            },
            InputEvent::Key {
                code: Key::Escape.protocol_code(),
                pressed: true,
                modifiers: Modifiers::NONE,
            },
            InputEvent::Focus { gained: true },
        ] {
            roundtrip_server(ServerMessage::Input {
                window_id: 9,
                event,
            });
        }
        roundtrip_server(ServerMessage::Closed { window_id: 9 });
        roundtrip_server(ServerMessage::Error {
            code: 3,
            message: "окно не найдено".to_string(),
        });
    }

    #[test]
    fn hello_has_magic_and_version() {
        let frame = encode_client(&ClientMessage::Hello {
            pid: 1,
            app: "x".to_string(),
            capabilities: 0,
        });
        let payload = &frame[4..];
        assert_eq!(payload[0], kind::HELLO);
        assert_eq!(u32::from_le_bytes(payload[1..5].try_into().unwrap()), MAGIC);
        assert_eq!(
            u16::from_le_bytes(payload[5..7].try_into().unwrap()),
            VERSION
        );

        let mut broken = payload.to_vec();
        broken[1] = 0xde;
        assert!(decode_client(&broken).is_err());
    }

    #[test]
    fn truncated_frame_errors() {
        let frame = encode_client(&ClientMessage::Close { window_id: 1 });
        assert!(decode_client(&frame[4..frame.len() - 1]).is_err());
        assert!(decode_client(&[]).is_err());
        assert!(decode_client(&[0x7f, 0, 0, 0]).is_err());
    }

    #[test]
    fn frame_reader_handles_partial_reads() {
        let mut reader = FrameReader::new();
        let one = encode_client(&ClientMessage::CreateWindow {
            window_id: 1,
            width: 10,
            height: 10,
            format: PixelFormat::Rgb565,
            title: "Тест".to_string(),
        });
        let two = encode_server(&ServerMessage::Closed { window_id: 1 });
        let all: Vec<u8> = one.iter().chain(two.iter()).copied().collect();

        reader.feed(&all[..2]);
        assert!(reader.next_payload().unwrap().is_none());
        reader.feed(&all[2..all.len() - 3]);
        let first = reader.next_payload().unwrap().expect("первый кадр");
        assert!(matches!(
            decode_client(&first).unwrap(),
            ClientMessage::CreateWindow { .. }
        ));
        assert!(reader.next_payload().unwrap().is_none());
        reader.feed(&all[all.len() - 3..]);
        let second = reader.next_payload().unwrap().expect("второй кадр");
        assert!(matches!(
            decode_server(&second).unwrap(),
            ServerMessage::Closed { .. }
        ));
        assert_eq!(reader.buffered(), 0);
    }

    #[test]
    fn frame_reader_rejects_bad_length() {
        let mut reader = FrameReader::new();
        reader.feed(&[0, 0, 0, 0]);
        assert!(reader.next_payload().is_err());
        let mut reader = FrameReader::new();
        reader.feed(&u32::MAX.to_le_bytes());
        assert!(reader.next_payload().is_err());
    }

    #[test]
    fn pack_pixels_formats() {
        let pixels = vec![255, 0, 0, 255, 0, 255, 0, 255];
        assert_eq!(
            pack_pixels(&pixels, PixelFormat::Xrgb8888),
            vec![0x00, 0x00, 0xff, 0x00, 0x00, 0xff, 0x00, 0x00]
        );
        let argb = pack_pixels(&pixels, PixelFormat::Argb8888);
        assert_eq!(&argb[..4], &0xffff_0000u32.to_le_bytes());
        let rgb565 = pack_pixels(&pixels, PixelFormat::Rgb565);
        assert_eq!(&rgb565[..2], &0xf800u16.to_le_bytes());
        assert_eq!(&rgb565[2..4], &0x07e0u16.to_le_bytes());
    }

    #[test]
    fn commit_frame_size_matches_encoding() {
        let size = Size::new(4, 3);
        let frame = encode_client(&ClientMessage::CommitFrame {
            window_id: 1,
            sequence: 0,
            width: 4,
            height: 3,
            stride: 16,
            format: PixelFormat::Xrgb8888,
            pixels: vec![0u8; 4 * 3 * 4],
        });
        assert_eq!(frame.len(), commit_frame_size(size, PixelFormat::Xrgb8888));
    }

    #[test]
    fn input_events_map_to_ui_events() {
        assert_eq!(
            InputEvent::PointerButton {
                x: 5,
                y: 6,
                button: MouseButton::Right,
                pressed: true,
            }
            .to_event(),
            Some(Event::MouseDown {
                pos: Point::new(5, 6),
                button: MouseButton::Right
            })
        );
        assert_eq!(
            InputEvent::Key {
                code: Key::Enter.protocol_code(),
                pressed: true,
                modifiers: Modifiers::NONE,
            }
            .to_event(),
            Some(Event::Key {
                key: Key::Enter,
                pressed: true,
                modifiers: Modifiers::NONE
            })
        );
        assert_eq!(
            InputEvent::Focus { gained: true }.to_event(),
            Some(Event::Focus { gained: true })
        );
    }
}
