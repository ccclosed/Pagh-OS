//! Клиент PDP/1: подключение к `pagh-desktop`, окна, кадры, ввод и закрытие.
//!
//! Композитор владеет размещением, заголовком окна (`v`/`^`/`x`), фокусом и
//! панелью; клиент рисует **только клиентскую область** и получает координаты
//! ввода относительно неё. Есть два пути доставки пикселей:
//!
//! * **ShmSurface** (приоритет): клиент создаёт memfd, отображает его
//!   `MAP_SHARED`, отправляет fd через `SCM_RIGHTS` (`SHM_OFFER`), ждёт
//!   `SHM_READY`, рендерит прямо в mapping и шлёт только damage-прямоугольники
//!   (`COMMIT` с header flags bit 0) — пиксели по сокету не идут;
//! * **frame upload** (fallback): полный кадр `XRGB8888`/`RGB565` внутри
//!   сокета, как в v1. Используется, если `welcome.flags` не содержит bit 2,
//!   `SCM_RIGHTS`/`memfd` недоступны, сервер не прислал `SHM_READY` за
//!   [`SHM_READY_TIMEOUT`] или ответил ошибкой на `SHM_OFFER`.
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

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::canvas::Canvas;
use crate::damage::Damage;
use crate::event::{Event, Key, Modifiers, MouseButton};
use crate::geom::{Point, Size};
use crate::protocol::{
    self, buttons, close_reason, header_flags, mods, pack_rgb565, pack_xrgb8888, FrameFormat,
    InputEvent, Message, MessageReader, HEADER_LEN, MAGIC, MAX_PAYLOAD, MAX_SURFACE_DIM, VERSION,
};
use crate::shm::{self, ShmSurface};

/// Сокет композитора по умолчанию (совпадает с `pagh-desktop`).
pub const DEFAULT_SOCKET: &str = "/tmp/pagh-desktop.sock";

/// Сколько ждать `SHM_READY` после `SHM_OFFER`, прежде чем откатиться на
/// полный кадр (legacy-сервер не отвечает на type 14).
pub const SHM_READY_TIMEOUT: Duration = Duration::from_secs(2);
/// Переменная окружения для трассировки PDP/1 (одна строка на сообщение,
/// без пикселей — длина payload вместо байтов).
pub const TRACE_ENV: &str = "PAGH_UI_PDP_TRACE";

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
    /// Сервер принял `SHM_OFFER` и назначил поколение.
    ShmReady { window_id: u32, generation: u32 },
    /// Сервер освободил поколение разделяемой поверхности.
    ShmRelease { window_id: u32, generation: u32 },
}

/// Клиентская разделяемая поверхность: mapping + поколение для `COMMIT`.
///
/// Пока `frame` жив, приложение рисует прямо в [`ShmFrame::canvas`] (это mmap
/// memfd) и отправляет только damage через [`DesktopClient::commit_shm`].
pub struct ShmFrame {
    pub canvas: Canvas,
    pub window_id: u32,
    pub generation: u32,
    pub format: FrameFormat,
}

impl ShmFrame {
    pub fn size(&self) -> Size {
        self.canvas.size()
    }

    pub fn stride(&self) -> u32 {
        self.canvas.stride() as u32
    }

    pub fn rect(&self) -> crate::geom::Rect {
        self.canvas.rect()
    }
}

/// Куда писать компактную трассировку PDP/1 (для e2e/отладки).
struct Trace {
    writer: BufWriter<File>,
}

impl DesktopClient {
    fn trace(&mut self, direction: char, kind: &str, flags: u16, length: usize, detail: &str) {
        let Some(trace) = self.trace.as_mut() else {
            return;
        };
        if detail.is_empty() {
            let _ = writeln!(
                trace.writer,
                "{direction} {kind} flags={flags:#x} len={length}"
            );
        } else {
            let _ = writeln!(
                trace.writer,
                "{direction} {kind} flags={flags:#x} len={length} {detail}"
            );
        }
        let _ = trace.writer.flush();
    }
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
    /// Сервер объявил ShmSurface и окружение его поддерживает.
    shm_capable: bool,
    /// Текущее поколение shm-поверхности (0 = ещё не готово).
    generation: u32,
    /// События, полученные во время синхронного ожидания `SHM_READY`.
    pending: VecDeque<Incoming>,
    /// Переиспользуемый буфер заголовка+payload (без аллокаций в кадре).
    tx_buffer: Vec<u8>,
    /// Необязательная трассировка отправленных/принятых сообщений.
    trace: Option<Trace>,
}

impl DesktopClient {
    /// Подключиться к сокету (без рукопожатия).
    pub fn connect_socket(path: impl AsRef<Path>) -> io::Result<DesktopClient> {
        let stream = UnixStream::connect(path.as_ref())?;
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let trace = std::env::var_os(TRACE_ENV)
            .filter(|value| !value.is_empty())
            .and_then(|path| File::create(std::path::Path::new(&path)).ok())
            .map(|file| Trace {
                writer: BufWriter::new(file),
            });
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
            shm_capable: false,
            generation: 0,
            pending: VecDeque::new(),
            tx_buffer: Vec::with_capacity(64 * 1024),
            trace,
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
                    self.shm_capable =
                        flags & protocol::welcome_flags::SHM_SURFACE != 0 && shm::shm_supported();
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
    ///
    /// `canvas` обязан быть обычным RGBA-холстом; для `ShmSurface` используйте
    /// [`DesktopClient::commit_shm`] — у неё другой порядок пикселей и не нужен
    /// аплоад.
    pub fn commit(&mut self, window_id: u32, canvas: &Canvas) -> io::Result<()> {
        if canvas.is_shared() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared surface must be committed with commit_shm",
            ));
        }
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

    // ───────────────────────── ShmSurface (PDP/1 B5) ────────────────────────

    /// Умеет ли соединение разделяемые поверхности: `welcome.flags` bit 2 **и**
    /// окружение (memfd/mmap/SCM_RIGHTS); тестовый выключатель —
    /// [`shm::DISABLE_ENV`].
    pub fn shm_supported(&self) -> bool {
        self.shm_capable
    }

    /// Текущее поколение разделяемой поверхности (0 = ещё не готово).
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Создать memfd-поверхность, отправить `SHM_OFFER` и дождаться `SHM_READY`.
    ///
    /// Возвращает `Ok(None)`, если сервер/окружение не поддерживают ShmSurface
    /// или сервер не ответил за [`SHM_READY_TIMEOUT`]: вызывающий продолжает
    /// работать через full-frame [`DesktopClient::commit`].
    pub fn enable_shm(&mut self, window_id: u32, size: Size) -> io::Result<Option<ShmFrame>> {
        if !self.shm_capable || window_id == 0 || size.w <= 0 || size.h <= 0 {
            return Ok(None);
        }
        let Some((mut frame, generation)) = self.offer_shm(window_id, size)? else {
            return Ok(None);
        };
        frame.generation = generation;
        self.generation = generation;
        Ok(Some(frame))
    }

    /// Пересоздать разделяемую поверхность под новый размер (`configure`).
    ///
    /// Новый memfd + `SHM_OFFER` + ожидание `SHM_READY`; старый mapping
    /// освобождается после подтверждения (и клиент шлёт `SHM_RELEASE(old)`).
    /// `Ok(false)` — сервер не подтвердил, `frame` остаётся прежним; вызывающий
    /// продолжает full-frame commit.
    pub fn resize_shm(&mut self, frame: &mut ShmFrame, size: Size) -> io::Result<bool> {
        if !self.shm_capable || size.w <= 0 || size.h <= 0 {
            return Ok(false);
        }
        let window_id = frame.window_id;
        let old_generation = frame.generation;
        let Some((mut new_frame, generation)) = self.offer_shm(window_id, size)? else {
            return Ok(false);
        };
        new_frame.generation = generation;
        let _ = self.send(&Message::ShmRelease {
            window_id,
            generation: old_generation,
        });
        self.generation = generation;
        self.width = size.w as u32;
        self.height = size.h as u32;
        *frame = new_frame;
        Ok(true)
    }

    /// Отправить только damage-прямоугольники (shm-форма `COMMIT`, без пикселей).
    ///
    /// Пустой damage ничего не отправляет (idle → ноль сообщений). Ошибка
    /// `InvalidData` означает, что поколение устарело: нужно откатиться на
    /// full-frame commit или пересоздать поверхность.
    pub fn commit_shm(&mut self, frame: &ShmFrame, damage: &Damage) -> io::Result<()> {
        if !self.shm_capable {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "ShmSurface is not enabled",
            ));
        }
        if frame.generation != self.generation {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stale shm generation",
            ));
        }
        if damage.is_empty() {
            return Ok(());
        }
        let rects = damage.rects();
        let payload_len = 12 + rects.len() * 12;
        let total = HEADER_LEN + payload_len;
        self.tx_buffer.clear();
        self.tx_buffer.reserve(total);
        self.tx_buffer.extend_from_slice(&MAGIC);
        self.tx_buffer
            .extend_from_slice(&protocol::types::COMMIT.to_le_bytes());
        self.tx_buffer
            .extend_from_slice(&header_flags::SHM.to_le_bytes());
        let payload_len = payload_len as u32;
        self.tx_buffer.extend_from_slice(&payload_len.to_le_bytes());
        self.tx_buffer
            .extend_from_slice(&frame.window_id.to_le_bytes());
        self.tx_buffer
            .extend_from_slice(&frame.generation.to_le_bytes());
        self.tx_buffer
            .extend_from_slice(&(rects.len() as u32).to_le_bytes());
        for rect in rects {
            self.tx_buffer.extend_from_slice(&rect.x.to_le_bytes());
            self.tx_buffer.extend_from_slice(&rect.y.to_le_bytes());
            self.tx_buffer
                .extend_from_slice(&(rect.w.clamp(0, u16::MAX as i32) as u16).to_le_bytes());
            self.tx_buffer
                .extend_from_slice(&(rect.h.clamp(0, u16::MAX as i32) as u16).to_le_bytes());
        }
        self.stream.write_all(&self.tx_buffer)?;
        self.stream.flush()?;
        if self.trace.is_some() {
            let detail = format!(
                "window={} gen={} damage={}",
                frame.window_id,
                frame.generation,
                rects.len()
            );
            self.trace(
                '>',
                "commit_shm",
                header_flags::SHM,
                payload_len as usize,
                &detail,
            );
        }
        Ok(())
    }

    /// Сообщить серверу, что поколение больше не нужно, и отключить shm-путь.
    pub fn release_shm(&mut self, frame: &ShmFrame) -> io::Result<()> {
        self.shm_capable = false;
        self.generation = 0;
        self.send(&Message::ShmRelease {
            window_id: frame.window_id,
            generation: frame.generation,
        })
    }

    /// Создать memfd+canvas и провести offer/ready; на неудаче — `Ok(None)`
    /// (fallback), без разрыва соединения.
    fn offer_shm(&mut self, window_id: u32, size: Size) -> io::Result<Option<(ShmFrame, u32)>> {
        let name = format!("pagh-ui-{}-{}", std::process::id(), window_id);
        let surface = match ShmSurface::create(&name, size.w as u32, size.h as u32, self.format) {
            Ok(surface) => surface,
            Err(_) => {
                self.shm_capable = false;
                return Ok(None);
            }
        };
        let canvas = match Canvas::map_surface(&surface) {
            Ok(canvas) => canvas,
            Err(_) => {
                self.shm_capable = false;
                return Ok(None);
            }
        };
        let offer = Message::ShmOffer {
            window_id,
            width: size.w as u32,
            height: size.h as u32,
            format: self.format.to_wire(),
            stride: surface.stride(),
            flags: 0,
        };
        let encoded = offer.encode();
        if shm::send_fd_payload(&self.stream, &encoded, surface.fd()).is_err() {
            self.shm_capable = false;
            return Ok(None);
        }
        if self.trace.is_some() {
            let detail = format!(
                "window={} {}x{} stride={} format={}",
                window_id,
                size.w,
                size.h,
                surface.stride(),
                self.format.to_wire()
            );
            self.trace('>', "shm_offer", 0, encoded.len() - HEADER_LEN, &detail);
        }
        let Some(generation) = self.wait_for_shm_ready(window_id)? else {
            return Ok(None);
        };
        Ok(Some((
            ShmFrame {
                canvas,
                window_id,
                generation,
                format: self.format,
            },
            generation,
        )))
    }

    /// Ждать `SHM_READY`/ошибку с дедлайном, не теряя прочие события.
    fn wait_for_shm_ready(&mut self, window_id: u32) -> io::Result<Option<u32>> {
        let deadline = Instant::now() + SHM_READY_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let Some(message) = self.next_message(remaining)? else {
                return Ok(None);
            };
            match message {
                Message::ShmReady {
                    window_id: id,
                    generation,
                } if id == window_id => {
                    self.trace('<', "shm_ready", 0, 8, "");
                    return Ok(Some(generation));
                }
                Message::Error { code, message } => {
                    self.trace('<', "error", 0, message.len(), &format!("code={code}"));
                    if code == protocol::error_codes::STALE_GENERATION {
                        self.generation = 0;
                    }
                    return Ok(None);
                }
                other => {
                    let mut incoming = Vec::new();
                    self.handle_message(other, &mut incoming)?;
                    self.pending.extend(incoming);
                }
            }
        }
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
        self.stream.flush()?;
        if self.trace.is_some() {
            let detail = match message {
                Message::Commit {
                    window_id,
                    width,
                    height,
                    stride,
                    pixels,
                    ..
                } => format!(
                    "window={window_id} {}x{} stride={stride} pixels={}",
                    width,
                    height,
                    pixels.len()
                ),
                Message::CommitShm {
                    window_id,
                    generation,
                    rects,
                } => format!("window={window_id} gen={generation} damage={}", rects.len()),
                Message::Damage { window_id, rects } => {
                    format!("window={window_id} hint={}", rects.len())
                }
                Message::ShmOffer {
                    window_id,
                    width,
                    height,
                    stride,
                    ..
                } => format!("window={window_id} {width}x{height} stride={stride} fd"),
                Message::ShmRelease {
                    window_id,
                    generation,
                } => format!("window={window_id} gen={generation}"),
                _ => String::new(),
            };
            self.trace(
                '>',
                message.name(),
                message.header_flags(),
                encoded.len() - HEADER_LEN,
                &detail,
            );
        }
        Ok(())
    }

    /// Прочитать все события, ожидая первое не дольше `timeout`.
    pub fn poll(&mut self, timeout: Duration) -> io::Result<Vec<Incoming>> {
        let deadline = Instant::now() + timeout;
        let mut incoming = Vec::new();
        // События, пришедшие во время синхронного ожидания SHM_READY,
        // отдаём до чтения сокета и не блокируемся повторно.
        while let Some(event) = self.pending.pop_front() {
            incoming.push(event);
        }
        if !incoming.is_empty() {
            return Ok(incoming);
        }
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
                self.shm_capable =
                    flags & protocol::welcome_flags::SHM_SURFACE != 0 && shm::shm_supported();
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
            Message::ShmReady {
                window_id,
                generation,
            } => {
                if window_id == self.window_id {
                    self.generation = generation;
                }
                incoming.push(Incoming::ShmReady {
                    window_id,
                    generation,
                });
            }
            Message::ShmRelease {
                window_id,
                generation,
            } => {
                if window_id == self.window_id && generation == self.generation {
                    // Сервер освободил текущую поверхность: до нового offer
                    // рендерим/шлём полный кадр.
                    self.generation = 0;
                }
                incoming.push(Incoming::ShmRelease {
                    window_id,
                    generation,
                });
            }
            Message::Pong { .. } | Message::Damage { .. } | Message::DamageShm { .. } => {}
            Message::Hello { .. }
            | Message::Create { .. }
            | Message::Commit { .. }
            | Message::CommitShm { .. }
            | Message::ShmOffer { .. }
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
