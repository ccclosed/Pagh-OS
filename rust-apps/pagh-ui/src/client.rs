//! Транспорт клиента: `AF_UNIX` `SOCK_STREAM` и обмен кадрами PGUI v1.
//!
//! Клиент инкапсулирует сокет, кодирование сообщений и накопление входных
//! кадров. Приложение получает [`Incoming`] (события ввода, `configure`,
//! `closed`, `error`), рисует кадр в [`Canvas`] и отдаёт его в [`commit`].
//!
//! Поверхность отдаётся целиком (без damage-прямоугольников): при 1280×800 и
//! XRGB8888 кадр — 4 МБ, что для локального сокета приемлемо; damage появится,
//! когда композитор заявит его в `welcome`.
//!
//! [`commit`]: DesktopClient::commit

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::canvas::Canvas;
use crate::event::Event;
use crate::geom::Size;
use crate::protocol::{
    self, ClientMessage, FrameReader, InputEvent, PixelFormat, ProtocolError, ServerMessage,
    MAX_FRAME,
};

/// Входящее сообщение композитора, уже переведённое в модель тулкита.
#[derive(Clone, PartialEq, Debug)]
pub enum Incoming {
    /// Событие ввода для окна.
    Event { window_id: u32, event: Event },
    /// Композитор поменял размер поверхности.
    Configure { window_id: u32, size: Size },
    /// Окно закрыто композитором.
    Closed { window_id: u32 },
    /// Ошибка протокола от композитора.
    Error { code: u32, message: String },
}

/// Подключение к композитору Pagh desktop B.
pub struct DesktopClient {
    stream: UnixStream,
    reader: FrameReader,
    screen: Size,
    format: PixelFormat,
    max_frame: u32,
    sequence: u32,
    windows: Vec<u32>,
}

impl DesktopClient {
    /// Подключиться к сокету композитора и пройти `hello`/`welcome`.
    pub fn connect(socket: impl AsRef<Path>, app: &str) -> io::Result<DesktopClient> {
        let stream = UnixStream::connect(socket)?;
        let mut client = DesktopClient {
            stream,
            reader: FrameReader::new(),
            screen: Size::ZERO,
            format: PixelFormat::Xrgb8888,
            max_frame: MAX_FRAME,
            sequence: 0,
            windows: Vec::new(),
        };
        client.send(&ClientMessage::Hello {
            pid: std::process::id(),
            app: app.to_string(),
            capabilities: 0,
        })?;
        client.handshake(Duration::from_secs(5))?;
        Ok(client)
    }

    /// Подключение по уже готовому сокету (например, унаследованному от лаунчера).
    pub fn from_stream(stream: UnixStream, app: &str) -> io::Result<DesktopClient> {
        let mut client = DesktopClient {
            stream,
            reader: FrameReader::new(),
            screen: Size::ZERO,
            format: PixelFormat::Xrgb8888,
            max_frame: MAX_FRAME,
            sequence: 0,
            windows: Vec::new(),
        };
        client.send(&ClientMessage::Hello {
            pid: std::process::id(),
            app: app.to_string(),
            capabilities: 0,
        })?;
        client.handshake(Duration::from_secs(5))?;
        Ok(client)
    }

    fn handshake(&mut self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        self.stream
            .set_read_timeout(Some(Duration::from_millis(100)))?;
        loop {
            match self.read_available() {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "композитор закрыл соединение до welcome",
                    ))
                }
                Err(error) => return Err(error),
            }
            while let Some(payload) = self.reader.next_payload().map_err(protocol_error)? {
                match protocol::decode_server(&payload).map_err(protocol_error)? {
                    ServerMessage::Welcome {
                        screen_width,
                        screen_height,
                        format,
                        max_frame,
                        ..
                    } => {
                        self.screen = Size::new(screen_width as i32, screen_height as i32);
                        self.format = format;
                        self.max_frame = max_frame.min(MAX_FRAME);
                        self.stream.set_read_timeout(None)?;
                        return Ok(());
                    }
                    ServerMessage::Error { code, message } => {
                        return Err(io::Error::other(format!(
                            "композитор вернул ошибку {code}: {message}"
                        )))
                    }
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "композитор не прислал welcome",
                ));
            }
        }
    }

    /// Экран композитора (из `welcome`).
    pub fn screen(&self) -> Size {
        self.screen
    }

    /// Формат пикселей, который ждёт композитор.
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// Максимальный принимаемый кадр.
    pub fn max_frame(&self) -> u32 {
        self.max_frame
    }

    /// Идентификаторы открытых этим клиентом окон.
    pub fn windows(&self) -> &[u32] {
        &self.windows
    }

    /// Создать окно и вернуть его идентификатор.
    pub fn create_window(&mut self, size: Size, title: &str) -> io::Result<u32> {
        let window_id = self.windows.iter().copied().max().unwrap_or(0) + 1;
        self.send(&ClientMessage::CreateWindow {
            window_id,
            width: size.w.clamp(1, u16::MAX as i32) as u16,
            height: size.h.clamp(1, u16::MAX as i32) as u16,
            format: self.format,
            title: title.to_string(),
        })?;
        self.windows.push(window_id);
        Ok(window_id)
    }

    /// Отправить кадр поверхности окна.
    pub fn commit(&mut self, window_id: u32, canvas: &Canvas) -> io::Result<()> {
        let pixels = protocol::pack_pixels(canvas.pixels(), self.format);
        let stride = canvas.width() as u32 * self.format.bytes_per_pixel();
        let frame_size = pixels.len() + 32;
        if frame_size > self.max_frame as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("кадр {frame_size} байт больше max_frame {}", self.max_frame),
            ));
        }
        self.sequence = self.sequence.wrapping_add(1);
        self.send(&ClientMessage::CommitFrame {
            window_id,
            sequence: self.sequence,
            width: canvas.width().clamp(0, u16::MAX as i32) as u16,
            height: canvas.height().clamp(0, u16::MAX as i32) as u16,
            stride: stride.min(u16::MAX as u32) as u16,
            format: self.format,
            pixels,
        })
    }

    /// Попросить композитор закрыть окно.
    pub fn close_window(&mut self, window_id: u32) -> io::Result<()> {
        self.send(&ClientMessage::Close { window_id })?;
        self.windows.retain(|id| *id != window_id);
        Ok(())
    }

    /// Прочитать все доступные сообщения композитора (неблокирующе).
    pub fn poll(&mut self) -> io::Result<Vec<Incoming>> {
        match self.read_available() {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(Vec::new()),
            Err(error) => return Err(error),
        }
        let mut incoming = Vec::new();
        while let Some(payload) = self.reader.next_payload().map_err(protocol_error)? {
            let message = protocol::decode_server(&payload).map_err(protocol_error)?;
            match message {
                ServerMessage::Welcome { .. } => {}
                ServerMessage::Configure {
                    window_id,
                    width,
                    height,
                } => incoming.push(Incoming::Configure {
                    window_id,
                    size: Size::new(width as i32, height as i32),
                }),
                ServerMessage::Input { window_id, event } => {
                    if let Some(event) = event_to_ui(event) {
                        incoming.push(Incoming::Event { window_id, event });
                    }
                }
                ServerMessage::Closed { window_id } => {
                    self.windows.retain(|id| *id != window_id);
                    incoming.push(Incoming::Closed { window_id });
                }
                ServerMessage::Error { code, message } => {
                    incoming.push(Incoming::Error { code, message });
                }
            }
        }
        Ok(incoming)
    }

    /// Переключить неблокирующий режим сокета.
    pub fn set_nonblocking(&mut self, value: bool) -> io::Result<()> {
        self.stream.set_nonblocking(value)
    }

    /// Доступ к сокету (например, для `poll(2)` в цикле событий).
    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    fn send(&mut self, message: &ClientMessage) -> io::Result<()> {
        let frame = protocol::encode_client(message);
        self.stream.write_all(&frame)?;
        self.stream.flush()
    }

    fn read_available(&mut self) -> io::Result<()> {
        let mut buffer = [0u8; 16 * 1024];
        match self.stream.read(&mut buffer) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "композитор закрыл соединение",
            )),
            Ok(count) => {
                self.reader.feed(&buffer[..count]);
                Ok(())
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut =>
            {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
            Err(error) => Err(error),
        }
    }
}

fn protocol_error(error: ProtocolError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn event_to_ui(event: InputEvent) -> Option<Event> {
    event.to_event()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color;
    use crate::event::{Key, Modifiers};
    use crate::geom::{Point, Rect};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::thread;

    fn socket_path(tag: &str) -> PathBuf {
        let unique = format!(
            "pagh-ui-{}-{}-{}.sock",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique)
    }

    #[test]
    fn client_handshake_and_frames() {
        let path = socket_path("handshake");
        let listener = UnixListener::bind(&path).unwrap();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = FrameReader::new();
            let mut buffer = [0u8; 4096];
            let mut hello = None;
            let mut created = None;
            let mut committed = None;
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                use std::io::Read as _;
                let count = match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(_) => break,
                };
                reader.feed(&buffer[..count]);
                while let Some(payload) = reader.next_payload().unwrap() {
                    match protocol::decode_client(&payload).unwrap() {
                        ClientMessage::Hello { app, .. } => {
                            hello = Some(app);
                            stream
                                .write_all(&protocol::encode_server(&ServerMessage::Welcome {
                                    protocol: protocol::VERSION,
                                    server_pid: 99,
                                    screen_width: 1280,
                                    screen_height: 800,
                                    format: PixelFormat::Xrgb8888,
                                    max_frame: MAX_FRAME,
                                }))
                                .unwrap();
                        }
                        ClientMessage::CreateWindow {
                            window_id,
                            width,
                            height,
                            title,
                            ..
                        } => {
                            created = Some((window_id, width, height, title));
                            stream
                                .write_all(&protocol::encode_server(&ServerMessage::Configure {
                                    window_id,
                                    width,
                                    height,
                                }))
                                .unwrap();
                        }
                        ClientMessage::CommitFrame {
                            window_id, pixels, ..
                        } => {
                            committed = Some((window_id, pixels.len()));
                            stream
                                .write_all(&protocol::encode_server(&ServerMessage::Input {
                                    window_id,
                                    event: InputEvent::PointerButton {
                                        x: 12,
                                        y: 34,
                                        button: crate::event::MouseButton::Left,
                                        pressed: true,
                                    },
                                }))
                                .unwrap();
                        }
                        ClientMessage::Close { .. } => {
                            return (hello, created, committed);
                        }
                    }
                }
            }
            (hello, created, committed)
        });

        // Ждём, пока сервер начнёт слушать.
        thread::sleep(Duration::from_millis(20));
        let mut client = DesktopClient::connect(&path, "pagh-files-test").unwrap();
        assert_eq!(client.screen(), Size::new(1280, 800));
        assert_eq!(client.format(), PixelFormat::Xrgb8888);

        let window = client.create_window(Size::new(64, 48), "Файлы").unwrap();
        assert_eq!(client.windows(), &[window]);

        let mut canvas = Canvas::filled(64, 48, Color::hex(0x081216));
        canvas.fill(Rect::new(4, 4, 8, 8), Color::WHITE);
        client.commit(window, &canvas).unwrap();

        // Забираем configure и input с ретраями (сокет блокирующий).
        let mut received = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            received.extend(client.poll().unwrap());
            if received.len() >= 2 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(received
            .iter()
            .any(|message| matches!(message, Incoming::Configure { .. })));
        assert!(received.iter().any(|message| matches!(
            message,
            Incoming::Event {
                event: Event::MouseDown { pos, .. },
                ..
            } if *pos == Point::new(12, 34)
        )));

        client.close_window(window).unwrap();
        let (hello, created, committed) = server.join().unwrap();
        assert_eq!(hello.as_deref(), Some("pagh-files-test"));
        assert_eq!(created, Some((window, 64, 48, "Файлы".to_string())));
        assert_eq!(committed, Some((window, 64 * 48 * 4)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn client_reports_server_error_during_handshake() {
        let path = socket_path("error");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = FrameReader::new();
            let mut buffer = [0u8; 1024];
            use std::io::Read as _;
            let count = stream.read(&mut buffer).unwrap();
            reader.feed(&buffer[..count]);
            let _ = reader.next_payload().unwrap();
            stream
                .write_all(&protocol::encode_server(&ServerMessage::Error {
                    code: 7,
                    message: "нет места".to_string(),
                }))
                .unwrap();
        });
        thread::sleep(Duration::from_millis(20));
        let error = DesktopClient::connect(&path, "x").err().expect("ошибка");
        assert!(error.to_string().contains("нет места"), "error = {error}");
        server.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn key_events_survive_transport() {
        let event = InputEvent::Key {
            code: Key::Escape.protocol_code(),
            pressed: true,
            modifiers: Modifiers::ctrl(),
        };
        assert_eq!(
            event.to_event(),
            Some(Event::Key {
                key: Key::Escape,
                pressed: true,
                modifiers: Modifiers::ctrl()
            })
        );
    }
}
