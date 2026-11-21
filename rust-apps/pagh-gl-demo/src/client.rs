//! Минимальный клиент PDP/1 для GL-демо: `hello/welcome` → `create` →
//! `SHM_OFFER`/`SHM_READY` → `COMMIT`+`DAMAGE` → `SHM_RELEASE`/`CLOSE`.
//!
//! Почему не `pagh_ui::client::DesktopClient`: его `ShmFrame` держит
//! отображение внутри `Canvas`, а TinyGL нужен **сырой** указатель на ту же
//! память, живущий дольше одного кадра. Клиент здесь использует публичные
//! `pagh_ui::protocol`/`pagh_ui::shm` (тот же провод и тот же memfd/SCM_RIGHTS
//! путь), но владеет `SharedFrame` отдельно от `Context` — это позволяет
//! открыть Z-буфер один раз на размер и слать коммиты без копии.
//!
//! Fallback на full-frame upload сознательно **не** реализуется: спека §4.5
//! требует честно упасть, если композитор не объявил `ShmSurface`. Копия
//! `w*h*4` по сокету каждый кадр — ровно то, чего GL-путь избегает.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use pagh_ui::protocol::{self, FrameFormat, Message, MessageReader, VERSION};
use pagh_ui::shm;

use crate::surface::SharedFrame;

pub const DEFAULT_SOCKET: &str = "/tmp/pagh-desktop.sock";
const WINDOW_ID: u32 = 1;
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const SHM_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Событие композитора, которое влияет на рендер.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Configure {
        width: u32,
        height: u32,
    },
    Close {
        reason: u32,
    },
    Error {
        code: u32,
        message: String,
    },
    /// Сервер освободил текущее поколение поверхности — нужен новый offer.
    ShmReleased {
        generation: u32,
    },
}

/// Подключение к `pagh-desktop` и текущая shm-поверхность.
pub struct PdpClient {
    stream: UnixStream,
    reader: MessageReader,
    window_id: u32,
    generation: u32,
    width: u32,
    height: u32,
    pending: VecDeque<Event>,
}

impl PdpClient {
    /// Подключиться, пройти `hello`, создать окно и предложить `ShmSurface`.
    ///
    /// Ошибка — если композитор не объявил bit 2 в `welcome.flags` или
    /// окружение не умеет memfd/mmap/`SCM_RIGHTS`: GL-демо не имеет
    /// full-frame fallback (спека §4.5).
    pub fn connect(
        path: &str,
        app: &str,
        width: u32,
        height: u32,
        title: &str,
    ) -> io::Result<(PdpClient, SharedFrame)> {
        let stream = UnixStream::connect(path)?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        let mut client = PdpClient {
            stream,
            reader: MessageReader::new(),
            window_id: WINDOW_ID,
            generation: 0,
            width,
            height,
            pending: VecDeque::new(),
        };
        client.hello(app)?;
        client.create(width, height, title)?;
        let frame = client.offer(width, height)?;
        Ok((client, frame))
    }

    pub fn window_id(&self) -> u32 {
        self.window_id
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Создать новый memfd, отправить `SHM_OFFER` с fd и дождаться
    /// `SHM_READY`; старое поколение освобождается (`SHM_RELEASE`).
    pub fn resize(&mut self, width: u32, height: u32) -> io::Result<SharedFrame> {
        let old_generation = self.generation;
        let frame = self.offer(width, height)?;
        if old_generation != 0 {
            self.send(&Message::ShmRelease {
                window_id: self.window_id,
                generation: old_generation,
            })?;
        }
        Ok(frame)
    }

    /// Отправить `COMMIT` (header flags SHM) с полным damage-прямоугольником.
    pub fn commit(&mut self) -> io::Result<()> {
        if self.generation == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no live SHM generation (server released the surface)",
            ));
        }
        let message = Message::CommitShm {
            window_id: self.window_id,
            generation: self.generation,
            rects: vec![(0, 0, self.width, self.height)],
        };
        self.send(&message)
    }

    /// Попросить композитор закрыть окно (reason 0 — клиент).
    pub fn close(&mut self) {
        let _ = self.send(&Message::Close {
            window_id: self.window_id,
            reason: 0,
        });
    }

    /// Прочитать все доступные сообщения, ожидая первое не дольше `timeout`.
    pub fn poll(&mut self, timeout: Duration) -> io::Result<Vec<Event>> {
        let mut events: Vec<Event> = self.pending.drain(..).collect();
        let deadline = Instant::now() + timeout;
        let mut first = true;
        loop {
            if let Some(message) = self
                .reader
                .next()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?
            {
                self.handle_message(message, &mut events)?;
                first = false;
                continue;
            }
            let remaining = if first {
                timeout.max(Duration::from_millis(1))
            } else {
                deadline.saturating_duration_since(Instant::now())
            };
            if remaining.is_zero() {
                break;
            }
            first = false;
            self.stream
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))?;
            let mut buffer = [0u8; 64 * 1024];
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    events.push(Event::Close {
                        reason: protocol::close_reason::DISCONNECT,
                    });
                    return Ok(events);
                }
                Ok(count) => self.reader.push(&buffer[..count]),
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(events)
    }

    fn hello(&mut self, app: &str) -> io::Result<()> {
        self.send(&Message::Hello {
            version: VERSION,
            pid: std::process::id(),
            app: app.to_string(),
        })?;
        let deadline = Instant::now() + HELLO_TIMEOUT;
        loop {
            let Some(message) = self.read_until(deadline)? else {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "композитор не прислал welcome",
                ));
            };
            match message {
                Message::Welcome {
                    version,
                    width: _,
                    height: _,
                    flags,
                    ..
                } => {
                    if version != VERSION {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("неподдерживаемая версия PDP/1: {version}"),
                        ));
                    }
                    if flags & protocol::welcome_flags::SHM_SURFACE == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "композитор не объявил ShmSurface (welcome.flags bit 2); \
                             GL-демо не поддерживает full-frame upload",
                        ));
                    }
                    if !shm::shm_supported() {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "memfd/mmap/SCM_RIGHTS недоступны в этом окружении; \
                             GL-демо требует ShmSurface",
                        ));
                    }
                    return Ok(());
                }
                Message::Error { code, message } => {
                    return Err(io::Error::other(format!(
                        "композитор вернул ошибку {code}: {message}"
                    )));
                }
                other => self.handle_message(other, &mut Vec::new())?,
            }
        }
    }

    fn create(&mut self, width: u32, height: u32, title: &str) -> io::Result<()> {
        self.send(&Message::Create {
            window_id: self.window_id,
            width,
            height,
            format: FrameFormat::Xrgb8888.to_wire(),
            title: title.to_string(),
        })
    }

    fn offer(&mut self, width: u32, height: u32) -> io::Result<SharedFrame> {
        let name = format!("pagh-gl-{}-{}", std::process::id(), width);
        let frame = SharedFrame::create(&name, width, height)?;
        let offer = Message::ShmOffer {
            window_id: self.window_id,
            width,
            height,
            format: FrameFormat::Xrgb8888.to_wire(),
            stride: frame.stride(),
            flags: 0,
        };
        shm::send_fd_payload(&self.stream, &offer.encode(), frame.fd())?;
        let generation = self.wait_ready()?;
        self.generation = generation;
        self.width = width;
        self.height = height;
        Ok(frame)
    }

    fn wait_ready(&mut self) -> io::Result<u32> {
        let deadline = Instant::now() + SHM_READY_TIMEOUT;
        loop {
            let Some(message) = self.read_until(deadline)? else {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "композитор не прислал SHM_READY",
                ));
            };
            match message {
                Message::ShmReady {
                    window_id,
                    generation,
                } if window_id == self.window_id => return Ok(generation),
                Message::Error { code, message } => {
                    return Err(io::Error::other(format!(
                        "SHM_OFFER отклонён ({code}): {message}"
                    )));
                }
                Message::Close { reason, .. } => {
                    return Err(io::Error::other(format!(
                        "композитор закрыл соединение во время SHM_OFFER (reason {reason})"
                    )));
                }
                other => {
                    let mut events = Vec::new();
                    self.handle_message(other, &mut events)?;
                    self.pending.extend(events);
                }
            }
        }
    }

    fn read_until(&mut self, deadline: Instant) -> io::Result<Option<Message>> {
        loop {
            if let Some(message) = self
                .reader
                .next()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?
            {
                return Ok(Some(message));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            self.stream
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))?;
            let mut buffer = [0u8; 64 * 1024];
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    return Err(io::Error::other("композитор закрыл соединение (PDP/1 EOF)"));
                }
                Ok(count) => self.reader.push(&buffer[..count]),
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn handle_message(&mut self, message: Message, events: &mut Vec<Event>) -> io::Result<()> {
        match message {
            Message::Configure {
                window_id,
                width,
                height,
                ..
            } => {
                if window_id == self.window_id {
                    self.width = width;
                    self.height = height;
                    events.push(Event::Configure { width, height });
                }
            }
            Message::Close { reason, .. } => events.push(Event::Close { reason }),
            Message::Error { code, message } => events.push(Event::Error { code, message }),
            Message::Ping { nonce } => self.send(&Message::Pong { nonce })?,
            Message::ShmRelease {
                window_id,
                generation,
            } => {
                if window_id == self.window_id && generation == self.generation {
                    self.generation = 0;
                    events.push(Event::ShmReleased { generation });
                }
            }
            Message::ShmReady { .. }
            | Message::Welcome { .. }
            | Message::Pong { .. }
            | Message::Damage { .. }
            | Message::DamageShm { .. }
            | Message::Input { .. }
            | Message::Focus { .. }
            | Message::Commit { .. }
            | Message::CommitShm { .. }
            | Message::ShmOffer { .. }
            | Message::Hello { .. }
            | Message::Create { .. }
            | Message::Launch { .. } => {}
        }
        Ok(())
    }

    fn send(&mut self, message: &Message) -> io::Result<()> {
        let encoded = message.encode();
        self.stream.write_all(&encoded)?;
        self.stream.flush()
    }
}
