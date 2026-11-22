//! AF_UNIX desktop-сервер: приём клиентов, маршрутизация протокола, цикл композитора.
//!
//! Один серверный поток владеет [`Compositor`]; на каждое соединение создаётся
//! читающий поток, который складывает разобранные сообщения в общий канал. Сервер
//! обрабатывает канал, рассылает `configure`/`input`/`focus`/`close` и после каждого
//! пакета событий перерисовывает только повреждённые области.

use crate::backend::{Backend, BackendKind, HeadlessBackend, OutputBackend};
use crate::client::Welcome;
use crate::compositor::{AppKind, Compositor, HostEvent, MouseButton};
use crate::demo;
use crate::evdev::{EvdevInput, InputAction};
use crate::fbdev::FbdevBackend;
use crate::geometry::Point;
use crate::netinfo::{self, NetInfo};
use crate::protocol::{
    self, Decoder, Message, ERR_BAD_MESSAGE, ERR_INTERNAL, ERR_INVALID_SURFACE, ERR_NOT_HELLO,
    ERR_NOT_OWNER, ERR_UNSUPPORTED, VERSION, WELCOME_FORMAT_RGB565, WELCOME_FORMAT_XRGB8888,
    WELCOME_SHM_SURFACE,
};
use crate::surface::{PixelFormat, Surface};
use std::io::{self, ErrorKind};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Выбор backend'а при старте.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputChoice {
    /// Попробовать `/dev/fb0`; если его нет — headless.
    Auto,
    Headless,
    Fbdev,
}

/// Выбор источника ввода.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputChoice {
    /// Evdev, только если выбран fbdev backend; иначе без ввода.
    Auto,
    None,
    Evdev,
}

/// Источник сетевых данных для панели/флайаута.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetChoice {
    /// Опрашивать `/proc/net/dev` + `SIOCGIF*` раз в секунду.
    Auto,
    /// Не трогать сеть (headless-тесты/прогоны без сетевого стека).
    None,
}

/// Команда запуска приложения по запросу `LAUNCH` (расширение PDP/1, type 13).
///
/// `command` — argv без имени сокета; токен `{socket}` заменяется путём сокета
/// сервера. Пример для терминала, который подключается флагом `--socket`:
///
/// ```text
/// LaunchTarget { app: "terminal".into(), command: vec![
///     "pagh-terminal".into(), "--socket".into(), "{socket}".into(),
/// ] }
/// ```
///
/// Если токена `{socket}` нет, сервер добавит `--connect <socket>` в конец.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchTarget {
    /// Идентификатор, который клиент присылает в `launch.app`.
    pub app: String,
    /// argv запуска (см. описание выше).
    pub command: Vec<String>,
}

impl LaunchTarget {
    pub fn new(app: impl Into<String>, command: Vec<String>) -> LaunchTarget {
        LaunchTarget {
            app: app.into(),
            command,
        }
    }

    /// Готовая argv с подставленным сокетом.
    fn command_for(&self, socket: &std::path::Path) -> Vec<std::ffi::OsString> {
        let socket = socket.as_os_str().to_os_string();
        let has_placeholder = self.command.iter().any(|token| token.contains("{socket}"));
        let mut argv: Vec<std::ffi::OsString> = self
            .command
            .iter()
            .map(|token| {
                if token.contains("{socket}") {
                    token.replace("{socket}", &socket.to_string_lossy()).into()
                } else {
                    token.into()
                }
            })
            .collect();
        if !has_placeholder {
            argv.push("--connect".into());
            argv.push(socket);
        }
        argv
    }
}

/// Настройки сервера.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    pub socket: PathBuf,
    /// Размер headless-экрана (для fbdev берётся из `FBIOGET_VSCREENINFO`).
    pub width: u32,
    pub height: u32,
    /// Если задано — каждый presented-кадр пишется в `frame_NNNNN.png` этого каталога.
    pub screenshot_dir: Option<PathBuf>,
    /// Остановиться после N presented-кадров (headless smoke-тест).
    pub max_frames: Option<u64>,
    /// Остановиться через N миллисекунд после старта (QEMU e2e / демо).
    pub exit_after_ms: Option<u64>,
    /// `--fbdev-selfcheck` через N мс после старта (только fbdev).
    pub fbdev_selfcheck_ms: Option<u64>,
    /// Backend: auto/headless/fbdev.
    pub backend: OutputChoice,
    pub fbdev_path: PathBuf,
    /// Ввод: auto/none/evdev.
    pub input: InputChoice,
    pub evdev_path: PathBuf,
    /// Печатать каждое декодированное evdev-событие (QEMU e2e evidence).
    pub input_trace: bool,
    /// Всегда презентовать полный кадр (QEMU screendump обновляет поверхность
    /// надёжнее; в headless/damage-тестах остаётся частичная отрисовка).
    pub full_render: bool,
    /// Создать одно демонстрационное окно без AF_UNIX-клиента.
    ///
    /// ТОЛЬКО ТЕСТЫ (`--demo-window`, `--e2e`): окно рисует
    /// `demo::terminal_surface`, то есть фейковый терминал с demo-текстом.
    /// Обычный образ этот флаг не передаёт — там окна создают реальные
    /// клиенты (`pagh-files`/`pagh-terminal`) через PDP/1.
    pub demo_window: bool,
    /// Источник сетевых данных (этап U): реальные `/proc/net/dev` + `SIOCGIF*`.
    pub net: NetChoice,
    /// Не печатать диагностику подключений.
    pub quiet: bool,
    /// Запускать `pagh-desktop-demo` по запросам панели/меню.
    ///
    /// ТОЛЬКО ТЕСТЫ (`tools/e2e.py desktop --legacy-demo`): в обычном образе
    /// приложения обязаны приходить из `launch_targets` (`desktop-autorun`).
    pub spawn_demo: bool,
    /// Явный путь к демо-клиенту (иначе ищется рядом с сервером).
    pub demo_binary: Option<PathBuf>,
    /// Приложения, которые сервер умеет запускать по `launch` (type 13).
    ///
    /// Это единственный путь обычного образа (см. `desktop-autorun`). Если
    /// цели нет и `spawn_demo` выключен, сервер сообщает «приложение не
    /// настроено» (лог/`ERR_UNSUPPORTED`); включённый `spawn_demo` (тесты)
    /// запускает демо-клиент для известного `AppKind::id()`.
    pub launch_targets: Vec<LaunchTarget>,
    /// Печатать строку `frame-stats` каждые N presented-кадров (0 — выключено).
    ///
    /// Включается в `--e2e` (120) и флагом `--frame-log-every N`; строка несёт
    /// frame time avg/p50/p95 и breakdown render/blend/copy/present.
    pub frame_log_every: u64,
    /// Автозапуск приложения по `launch_targets` сразу после старта сервера.
    ///
    /// Нужен MVP-образу: панель остаётся интерактивной, но первое окно
    /// (например, `files`) открывается без ручного клика. Один раз и только
    /// пока ни одно окно не запущено (тот же флаг, что у `spawn_demo`).
    /// `None`/`""` — выключено.
    pub autostart: Option<String>,
    /// Живые стенные часы панели (`--clock now`).
    ///
    /// Иначе панель показывает время, зафиксированное при старте: `HH:MM`
    /// содержит только минуты, поэтому «часы идут» — это перерисовка
    /// маленького `clock_rect` на смене минуты, а не кадр в секунду.
    pub live_clock: bool,
    /// Файл посекундной статистики кадров (`--frame-stats PATH`).
    ///
    /// Строка на секунду, включая секунды с `frames=0`: пропуск кадра из-за
    /// того, что композитор не планировался, виден как `gap_max_ms`, а не
    /// маскируется средней длительностью кадра.
    pub frame_stats: Option<PathBuf>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            socket: PathBuf::from("/tmp/pagh-desktop.sock"),
            width: 1280,
            height: 720,
            screenshot_dir: None,
            max_frames: None,
            exit_after_ms: None,
            fbdev_selfcheck_ms: None,
            backend: OutputChoice::Headless,
            fbdev_path: PathBuf::from("/dev/fb0"),
            input: InputChoice::None,
            evdev_path: PathBuf::from("/dev/input/event0"),
            input_trace: false,
            full_render: false,
            demo_window: false,
            quiet: false,
            spawn_demo: false,
            demo_binary: None,
            launch_targets: Vec::new(),
            frame_log_every: 0,
            autostart: None,
            live_clock: false,
            frame_stats: None,
            net: NetChoice::Auto,
        }
    }
}

/// Команды управления запущенным сервером (используются тестами и CLI-хуками).
#[derive(Clone, Debug)]
pub enum ServerCommand {
    PointerMove {
        x: i32,
        y: i32,
    },
    PointerRelative {
        dx: i32,
        dy: i32,
    },
    PointerButton {
        button: u32,
        pressed: bool,
    },
    Key {
        code: u32,
        pressed: bool,
        modifiers: u32,
    },
    Axis {
        horizontal: i32,
        vertical: i32,
    },
    Screenshot(PathBuf),
    /// Закрыть окно от имени сервера (тестовый/служебный хук; клиент получает
    /// `close(reason=SERVER)`).
    CloseWindow {
        window_id: u32,
    },
    /// Прочитать пиксель текущего back buffer (диагностика/e2e без PNG-декодера).
    SamplePixel {
        x: i32,
        y: i32,
        tx: Sender<Option<u32>>,
    },
    Shutdown,
}

/// Что случилось с memfd из `shm_offer` в читающем потоке.
///
/// Отображение делается ЗДЕСЬ, а не в главном потоке: `clone_current_compat`
/// даёт каждому треду копию fd-таблицы (`docs/procfs.md` §13.7 #22 —
/// `CLONE_FILES` не разделяет таблицу), поэтому принятый `SCM_RIGHTS` fd
/// существует только в таблице читающего треда. Передать главному потоку
/// голый номер значило бы получить `EBADF` (а `Drop` чужого `OwnedFd` закрыл бы
/// в его таблице посторонний дескриптор). `ShmMapping` — `Send`, поэтому через
/// канал едет само отображение.
enum ShmArrival {
    /// fd пришёл, поверхность отображена.
    Mapped(crate::shm::ShmMapping),
    /// fd пришёл, но отображение не удалось (не memfd, мал, EBADF).
    Failed(io::Error),
}

enum Incoming {
    Client {
        conn: u64,
        message: Message,
        /// Отображённая поверхность `shm_offer` (см. [`ShmArrival`]).
        shm: Option<ShmArrival>,
    },
    Disconnected(u64),
    ProtocolError {
        conn: u64,
        detail: String,
    },
    Command(ServerCommand),
    /// Сетевой сэмпл из фонового треда (см. `spawn_net_poller`).
    Net(NetInfo),
    /// `/proc/net/dev` не дал интерфейса — фоновый тред сообщает один раз.
    NetUnavailable,
}

struct ClientConn {
    id: u64,
    client_id: Option<u64>,
    name: String,
    writer: Arc<Mutex<UnixStream>>,
}

/// Счётчики кадра: present-ы, байты scanout, время compose/present и ShmSurface.
#[derive(Debug, Default)]
pub struct RenderCounters {
    presents: AtomicU64,
    bytes_presented: AtomicU64,
    damage_rects: AtomicU64,
    compose_ns: AtomicU64,
    present_ns: AtomicU64,
    shm_offers: AtomicU64,
    shm_commits: AtomicU64,
    shm_bytes: AtomicU64,
}

/// Снимок [`RenderCounters`] для логов/тестов.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderStats {
    pub presents: u64,
    pub bytes_presented: u64,
    pub damage_rects: u64,
    pub compose_ns: u64,
    pub present_ns: u64,
    pub shm_offers: u64,
    pub shm_commits: u64,
    pub shm_bytes: u64,
}

impl RenderCounters {
    pub fn snapshot(&self) -> RenderStats {
        RenderStats {
            presents: self.presents.load(Ordering::Relaxed),
            bytes_presented: self.bytes_presented.load(Ordering::Relaxed),
            damage_rects: self.damage_rects.load(Ordering::Relaxed),
            compose_ns: self.compose_ns.load(Ordering::Relaxed),
            present_ns: self.present_ns.load(Ordering::Relaxed),
            shm_offers: self.shm_offers.load(Ordering::Relaxed),
            shm_commits: self.shm_commits.load(Ordering::Relaxed),
            shm_bytes: self.shm_bytes.load(Ordering::Relaxed),
        }
    }

    pub fn reset(&self) {
        for counter in [
            &self.presents,
            &self.bytes_presented,
            &self.damage_rects,
            &self.compose_ns,
            &self.present_ns,
            &self.shm_offers,
            &self.shm_commits,
            &self.shm_bytes,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
    }
}

impl RenderStats {
    pub fn compose_ms(&self) -> f64 {
        self.compose_ns as f64 / 1_000_000.0
    }

    pub fn present_ms(&self) -> f64 {
        self.present_ns as f64 / 1_000_000.0
    }

    /// Строка для логов/selftest.
    pub fn log_line(&self) -> String {
        format!(
            "presents={} bytes_presented={} damage_rects={} compose_ms={:.3} present_ms={:.3} shm_offers={} shm_commits={} shm_bytes={}",
            self.presents,
            self.bytes_presented,
            self.damage_rects,
            self.compose_ms(),
            self.present_ms(),
            self.shm_offers,
            self.shm_commits,
            self.shm_bytes
        )
    }
}

/// Управляющий handle к серверу.
pub struct ServerControl {
    tx: Sender<Incoming>,
    running: Arc<AtomicBool>,
    pub socket: PathBuf,
}

impl ServerControl {
    pub fn send(&self, command: ServerCommand) {
        let _ = self.tx.send(Incoming::Command(command));
    }

    pub fn mouse_move(&self, x: i32, y: i32) {
        self.send(ServerCommand::PointerMove { x, y });
    }

    pub fn mouse_relative(&self, dx: i32, dy: i32) {
        self.send(ServerCommand::PointerRelative { dx, dy });
    }

    pub fn mouse_button(&self, button: u32, pressed: bool) {
        self.send(ServerCommand::PointerButton { button, pressed });
    }

    pub fn key(&self, code: u32, pressed: bool, modifiers: u32) {
        self.send(ServerCommand::Key {
            code,
            pressed,
            modifiers,
        });
    }

    pub fn axis(&self, horizontal: i32, vertical: i32) {
        self.send(ServerCommand::Axis {
            horizontal,
            vertical,
        });
    }

    pub fn screenshot(&self, path: impl Into<PathBuf>) {
        self.send(ServerCommand::Screenshot(path.into()));
    }

    pub fn close_window(&self, window_id: u32) {
        self.send(ServerCommand::CloseWindow { window_id });
    }

    /// Пиксель текущего back buffer (диагностика/e2e).
    pub fn sample_pixel(&self, x: i32, y: i32) -> Option<u32> {
        let (tx, rx) = channel();
        self.send(ServerCommand::SamplePixel { x, y, tx });
        rx.recv_timeout(Duration::from_secs(2)).ok().flatten()
    }

    pub fn shutdown(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.send(ServerCommand::Shutdown);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

/// Handle запущенного в отдельном потоке сервера.
pub struct ServerHandle {
    pub control: ServerControl,
    /// Счётчики текущего сервера (presents/bytes/compose/SHM).
    pub counters: Arc<RenderCounters>,
    join: JoinHandle<io::Result<()>>,
}

impl ServerHandle {
    pub fn join(self) -> io::Result<()> {
        self.join
            .join()
            .map_err(|_| io::Error::other("поток сервера запаниковал"))?
    }
}

/// Сам сервер. `run` захватывает `self` и работает до shutdown/лимита кадров.
pub struct Server {
    options: ServerOptions,
    listener: UnixListener,
    compositor: Compositor,
    backend: OutputBackend,
    input: Option<EvdevInput>,
    clients: Vec<ClientConn>,
    incoming_tx: Sender<Incoming>,
    incoming_rx: Receiver<Incoming>,
    running: Arc<AtomicBool>,
    next_conn: u64,
    next_client: u64,
    children: Vec<Child>,
    demo_started: bool,
    demo_window_created: bool,
    exit_due: Option<Instant>,
    /// Сетевой опрос идёт в фоновом треде, а не в кадре: чужой `Instant` здесь
    /// больше не нужен — только факт, что первая строка `net …` уже напечатана.
    /// (Раньше `net_due`/`net_prev` жили тут, и `poll_network` блокировал
    /// главный цикл на время `/proc/net/dev` + четырёх `SIOCGIF*`.)
    /// Стартовая строка `pagh-desktop: net ...` печатается один раз (e2e).
    net_logged: bool,
    selfcheck_due: Option<Instant>,
    selfcheck_done: bool,
    selfcheck_failed: bool,
    counters: Arc<RenderCounters>,
    shm: std::collections::HashMap<u32, crate::shm::ShmBinding>,
    next_shm_generation: u32,
    /// Следующий опрос живых часов панели (`--clock now`).
    clock_due: Instant,
    /// Базовая точка живых часов: (Instant старта, секунды UNIX на старте).
    ///
    /// Кадр не читает RTC: в Pagh `CLOCK_REALTIME` — это port-I/O к CMOS, и
    /// вызов из цикла стоил десятки миллисекунд паузы (измерено под TCG:
    /// inter-frame gap подскакивал до ~80 мс дважды в секунду). Один снимок на
    /// старте плюс монотонный `Instant` дают ту же минуту без syscall'а.
    clock_base: (Instant, u64),
    /// Посекундный лог кадров (`--frame-stats`), если запрошен.
    stats: Option<FrameStatsLog>,
    /// Момент последнего presented-кадра: разрыв между кадрами — метрика
    /// голодания композитора.
    last_present: Option<Instant>,
    /// Выборка frame time (мс) для периодической строки `frame-stats`.
    frame_samples: crate::timing::Samples,
    /// Выборки стадий текущего окна логирования.
    render_samples: crate::timing::Samples,
    blend_samples: crate::timing::Samples,
    copy_samples: crate::timing::Samples,
    present_samples: crate::timing::Samples,
    /// Сколько presents накопилось с последней строки статистики.
    frames_since_log: u64,
    /// Сумма damage-rects и пикселей за окно логирования.
    damage_since_log: u64,
    opaque_since_log: u64,
    blended_since_log: u64,
}

/// Посекундный лог кадров `--frame-stats PATH`.
///
/// Открывается один раз (truncate), пишется построчно и сбрасывается на диск
/// сразу: файл — свидетельство о прогоне, а не буфер, который потеряется при
/// убийстве композитора. Секунды, в которые композитор не исполнял главный
/// цикл, выводятся пустыми (`frames=0`) с расписанием `t=`, поэтому разрыв в
/// файле читается как «здесь не рисовали», а не «здесь лог потерялся».
struct FrameStatsLog {
    file: std::fs::File,
    started: Instant,
    /// Дедлайн следующей строки (ровно `started + N секунд`).
    due: Instant,
    bucket: crate::framestats::FrameBucket,
    /// Итоги за весь прогон для финальной строки `summary`.
    total_frames: u64,
    total_ms: f64,
    total_max_ms: f64,
}

impl FrameStatsLog {
    fn open(path: Option<PathBuf>, now: Instant) -> Option<Self> {
        let path = path?;
        let file = match std::fs::File::create(&path) {
            Ok(file) => file,
            Err(err) => {
                eprintln!("pagh-desktop: frame-stats {}: {err}", path.display());
                return None;
            }
        };
        Some(FrameStatsLog {
            file,
            started: now,
            due: now + Duration::from_secs(1),
            bucket: crate::framestats::FrameBucket::new(),
            total_frames: 0,
            total_ms: 0.0,
            total_max_ms: 0.0,
        })
    }

    fn write_line(&mut self, line: &str) {
        use std::io::Write as _;
        let _ = writeln!(self.file, "{line}");
        let _ = self.file.flush();
    }

    /// Записать строку текущего интервала и (если цикл проспал больше секунды)
    /// пустые строки за пропущенные: `t=` остаётся расписанием, а не временем
    /// пробуждения.
    fn tick(&mut self, now: Instant) {
        if now < self.due {
            return;
        }
        // Наверстать пропущенные секунды (ограниченно: час простоя не должен
        // порождать мегабайты пустых строк).
        let mut emitted = 0u32;
        while now >= self.due && emitted < 3600 {
            let t = self.due.duration_since(self.started).as_secs_f64();
            let line = self.bucket.line(t);
            self.write_line(&line);
            self.total_frames += self.bucket.frames();
            self.total_ms += self.bucket.frames() as f64 * self.bucket.avg_ms();
            if self.bucket.max_ms() > self.total_max_ms {
                self.total_max_ms = self.bucket.max_ms();
            }
            self.bucket.clear();
            self.due += Duration::from_secs(1);
            emitted += 1;
        }
    }

    fn finish(&mut self, now: Instant) {
        // Последний неполный интервал не теряется.
        let t = self.due.duration_since(self.started).as_secs_f64();
        let line = self.bucket.line(t);
        self.write_line(&line);
        self.total_frames += self.bucket.frames();
        self.total_ms += self.bucket.frames() as f64 * self.bucket.avg_ms();
        if self.bucket.max_ms() > self.total_max_ms {
            self.total_max_ms = self.bucket.max_ms();
        }
        let avg = if self.total_frames == 0 {
            0.0
        } else {
            self.total_ms / self.total_frames as f64
        };
        let elapsed = now.duration_since(self.started).as_secs_f64();
        self.write_line(&format!(
            "summary t={elapsed:.1} frames={} fps={:.1} avg_ms={avg:.3} max_ms={:.3}",
            self.total_frames,
            if elapsed > 0.0 {
                self.total_frames as f64 / elapsed
            } else {
                0.0
            },
            self.total_max_ms
        ));
    }
}

fn open_backend(options: &ServerOptions) -> io::Result<OutputBackend> {
    match options.backend {
        OutputChoice::Headless => Ok(OutputBackend::Headless(HeadlessBackend::new(
            options.width,
            options.height,
        ))),
        OutputChoice::Fbdev => {
            let backend = FbdevBackend::open(&options.fbdev_path).map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!("fbdev {}: {err}", options.fbdev_path.display()),
                )
            })?;
            if !options.quiet {
                println!("pagh-desktop: backend=fbdev {}", backend.describe());
                println!(
                    "pagh-desktop: fbdev takeover acquired (kernel console flush suspended; ABI one-way)"
                );
            }
            Ok(OutputBackend::Fbdev(backend))
        }
        OutputChoice::Auto => match FbdevBackend::open(&options.fbdev_path) {
            Ok(backend) => {
                if !options.quiet {
                    println!("pagh-desktop: backend=fbdev {}", backend.describe());
                    println!(
                        "pagh-desktop: fbdev takeover acquired (kernel console flush suspended; ABI one-way)"
                    );
                }
                Ok(OutputBackend::Fbdev(backend))
            }
            Err(err) => {
                if !options.quiet {
                    eprintln!(
                        "pagh-desktop: fbdev {} недоступен ({err}); backend=headless",
                        options.fbdev_path.display()
                    );
                }
                Ok(OutputBackend::Headless(HeadlessBackend::new(
                    options.width,
                    options.height,
                )))
            }
        },
    }
}

fn open_input(options: &ServerOptions, backend: BackendKind) -> io::Result<Option<EvdevInput>> {
    let wanted = match options.input {
        InputChoice::None => false,
        InputChoice::Evdev => true,
        InputChoice::Auto => backend == BackendKind::Fbdev,
    };
    if !wanted {
        return Ok(None);
    }
    match EvdevInput::open(&options.evdev_path) {
        Ok(input) => {
            if !options.quiet {
                println!("pagh-desktop: input=evdev {}", options.evdev_path.display());
            }
            Ok(Some(input))
        }
        Err(err) if options.input == InputChoice::Evdev => Err(io::Error::new(
            err.kind(),
            format!("evdev {}: {err}", options.evdev_path.display()),
        )),
        Err(err) => {
            if !options.quiet {
                eprintln!(
                    "pagh-desktop: evdev {} недоступен ({err}); input выключен",
                    options.evdev_path.display()
                );
            }
            Ok(None)
        }
    }
}

/// Диагностика «приложение не настроено» для обычного (не тестового) пути.
///
/// Вынесена в функцию, чтобы панель/меню и PDP `launch` (type 13) говорили
/// одно и то же: подсказывают `--launch-target`, а не предлагают включить
/// demo-клиент. Тестовый фолбэк `--spawn-demo` этот текст не использует.
fn unconfigured_launch_message(app: &str) -> String {
    format!(
        "pagh-desktop: launch {app}: приложение не настроено — задайте \
         --launch-target {app}=<программа>|<аргументы> (демо-клиент доступен \
         только с --spawn-demo)"
    )
}

impl Server {
    pub fn bind(options: ServerOptions) -> io::Result<Self> {
        if options.width == 0
            || options.height == 0
            || options.width > 8192
            || options.height > 8192
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "недопустимый размер экрана",
            ));
        }
        if let Some(parent) = options.socket.parent() {
            // На Pagh `/tmp` уже существует (ramfs), а `Path::exists()`/`stat(2)`
            // в compat-слое не поддерживаются (ENOSYS), поэтому проверять
            // существование нельзя: mkdir существующего каталога штатно отвечает
            // EEXIST — это и есть «каталог уже на месте».
            if !parent.as_os_str().is_empty() {
                match std::fs::create_dir_all(parent) {
                    Ok(()) => {}
                    Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
                    Err(err) => return Err(err),
                }
            }
        }
        if options.socket.exists() {
            let metadata = std::fs::symlink_metadata(&options.socket)?;
            if metadata.file_type().is_socket() {
                std::fs::remove_file(&options.socket)?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} существует и не является сокетом",
                        options.socket.display()
                    ),
                ));
            }
        }
        let listener = UnixListener::bind(&options.socket)?;
        listener.set_nonblocking(true)?;
        let backend = match open_backend(&options) {
            Ok(backend) => backend,
            Err(err) => {
                let _ = std::fs::remove_file(&options.socket);
                return Err(err);
            }
        };
        let (width, height) = backend.size();
        let input = match open_input(&options, backend.kind()) {
            Ok(input) => input,
            Err(err) => {
                let _ = std::fs::remove_file(&options.socket);
                return Err(err);
            }
        };
        if !options.quiet {
            println!(
                "pagh-desktop: backend={} {}x{}",
                backend.kind().name(),
                width,
                height
            );
        }
        let (tx, rx) = channel();
        let compositor = Compositor::new(width, height);
        let now = Instant::now();
        let options_frame_stats = options.frame_stats.clone();
        let exit_due = options
            .exit_after_ms
            .map(|ms| now + Duration::from_millis(ms));
        let selfcheck_due = if backend.kind() == BackendKind::Fbdev {
            options
                .fbdev_selfcheck_ms
                .map(|ms| now + Duration::from_millis(ms))
        } else {
            None
        };
        Ok(Self {
            options,
            listener,
            compositor,
            backend,
            input,
            clients: Vec::new(),
            incoming_tx: tx,
            incoming_rx: rx,
            running: Arc::new(AtomicBool::new(true)),
            next_conn: 1,
            next_client: 1,
            children: Vec::new(),
            demo_started: false,
            demo_window_created: false,
            exit_due,
            net_logged: false,
            selfcheck_due,
            selfcheck_done: false,
            selfcheck_failed: false,
            counters: Arc::new(RenderCounters::default()),
            shm: std::collections::HashMap::new(),
            next_shm_generation: 1,
            clock_due: now,
            clock_base: (now, crate::demo::unix_now()),
            stats: FrameStatsLog::open(options_frame_stats, now),
            last_present: None,
            frame_samples: crate::timing::Samples::with_capacity(256),
            render_samples: crate::timing::Samples::with_capacity(256),
            blend_samples: crate::timing::Samples::with_capacity(256),
            copy_samples: crate::timing::Samples::with_capacity(256),
            present_samples: crate::timing::Samples::with_capacity(256),
            frames_since_log: 0,
            damage_since_log: 0,
            opaque_since_log: 0,
            blended_since_log: 0,
        })
    }

    /// Поднять сервер в фоновом потоке и вернуть handle.
    pub fn spawn(options: ServerOptions) -> io::Result<ServerHandle> {
        let server = Self::bind(options)?;
        let control = server.control();
        let counters = server.counters.clone();
        let join = thread::spawn(move || server.run());
        Ok(ServerHandle {
            control,
            counters,
            join,
        })
    }

    pub fn counters(&self) -> Arc<RenderCounters> {
        self.counters.clone()
    }

    pub fn control(&self) -> ServerControl {
        ServerControl {
            tx: self.incoming_tx.clone(),
            running: self.running.clone(),
            socket: self.options.socket.clone(),
        }
    }

    pub fn compositor(&self) -> &Compositor {
        &self.compositor
    }

    pub fn compositor_mut(&mut self) -> &mut Compositor {
        &mut self.compositor
    }

    pub fn take_screenshot(&mut self, path: impl AsRef<std::path::Path>) -> io::Result<()> {
        self.compositor.render_full();
        crate::png::write_png(path, self.compositor.frame())
    }

    /// Основной цикл. Завершается по shutdown, лимиту кадров, `exit-after-ms`
    /// или ошибке backend'а.
    pub fn run(mut self) -> io::Result<()> {
        let screen = self.compositor.screen();
        if !self.options.quiet {
            println!(
                "pagh-desktop: слушаю {} ({}x{}, backend={})",
                self.options.socket.display(),
                screen.w,
                screen.h,
                self.backend.kind().name()
            );
        }
        self.ensure_demo_window();
        // 60 Hz: один present на интервал, несколько commit'ов в одной итерации
        // цикла сливаются в один damage-набор.
        const FRAME_INTERVAL: Duration = Duration::from_millis(16);
        self.spawn_net_poller();
        let mut next_frame = Instant::now();
        loop {
            if !self.running.load(Ordering::SeqCst) {
                break;
            }
            // Часы панели и посекундный лог кадров идут по стене главного
            // цикла, а не по presented-кадрам: секунда без кадра обязана
            // оставить след, иначе «композитор не планировался» выглядит как
            // «лог не писался».
            self.poll_clock();
            if let Some(stats) = self.stats.as_mut() {
                stats.tick(Instant::now());
            }
            self.poll_input();
            // Снимок ввода для строки `--frame-stats`: доказательство того, что
            // композитор не только рисует, но и получает события.
            let ev_events = self.input.as_ref().map(|i| i.events_read).unwrap_or(0);
            let pointer = self.compositor.pointer();
            if let Some(stats) = self.stats.as_mut() {
                stats.bucket.set_input(ev_events, (pointer.x, pointer.y));
            }
            self.accept_pending();
            let mut shutdown = false;
            // Неблокирующий дренаж: `recv_timeout` в compat-слое Pagh опирается
            // на futex-таймаут, который может просыпаться с большой задержкой,
            // поэтому канал дренируем без блокировки, а паузу делаем `sleep`.
            while let Ok(incoming) = self.incoming_rx.try_recv() {
                if !self.handle_incoming(incoming) {
                    shutdown = true;
                    break;
                }
            }
            if shutdown {
                break;
            }
            let now = Instant::now();
            if now >= next_frame {
                let before = self.backend.frames();
                self.render_if_needed()?;
                next_frame = if self.backend.frames() != before {
                    Instant::now() + FRAME_INTERVAL
                } else {
                    // Damage нет: следующий commit может быть показан сразу.
                    now
                };
            }
            self.reap_children();
            if !self.demo_started {
                if let Some(app) = self.options.autostart.clone() {
                    self.demo_started = true;
                    self.launch_app(&app, "", 0);
                } else if self.options.spawn_demo {
                    self.demo_started = true;
                    self.launch(AppKind::Terminal);
                }
            }
            self.maybe_selfcheck();
            if let Some(max) = self.options.max_frames {
                if self.backend.frames() >= max {
                    break;
                }
            }
            if let Some(due) = self.exit_due {
                if Instant::now() >= due {
                    // Тот же TOSTOP-контракт, что и у per-client диагностики:
                    // фоновый `--quiet` композитор не пишет в управляющий
                    // терминал, иначе SIGTTOU паркует его вместо выхода.
                    if !self.options.quiet {
                        println!("pagh-desktop: exit-after-ms reached");
                    }
                    break;
                }
            }
            // Пауза до ближайшего дедлайна: нет damage — не крутим кадры.
            thread::sleep(self.next_wakeup(next_frame));
        }
        let failed = self.selfcheck_failed;
        self.cleanup();
        if failed {
            Err(io::Error::other("fbdev selfcheck failed"))
        } else {
            Ok(())
        }
    }

    /// Обновить часы панели, если сменилась минута (`--clock now`).
    ///
    /// `HH:MM` меняется раз в минуту, поэтому опрос идёт раз в 500 мс, а
    /// damage — только `panel::clock_rect`: обновление часов не перерисовывает
    /// рабочий стол и не зависит от того, пришёл ли кадр от клиента.
    fn poll_clock(&mut self) {
        if !self.options.live_clock {
            return;
        }
        let now = Instant::now();
        if now < self.clock_due {
            return;
        }
        self.clock_due = now + Duration::from_millis(500);
        let (base_at, base_unix) = self.clock_base;
        let (time, date) =
            crate::demo::format_unix(base_unix + now.saturating_duration_since(base_at).as_secs());
        let (shown_time, shown_date) = self.compositor.clock_text();
        if shown_time != time || shown_date != date {
            self.compositor.set_clock(time, date);
        }
    }

    /// Запустить фоновый опрос сети (раз в секунду).
    ///
    /// Раньше `poll_network` вызывался прямо в главном цикле и на время
    /// `/proc/net/dev` + четырёх `SIOCGIF*` останавливал кадры. Измерение
    /// (`--frame-stats`) показало inter-frame gap 60 мс ровно раз в секунду —
    /// и всё это время курсор не мог сдвинуться. Опрос — обычные syscall'ы,
    /// которым нечего делать на пути кадра, поэтому они уходят в тред, а кадр
    /// только применяет готовый сэмпл из того же канала, что и клиенты.
    ///
    /// Скорости — дельта счётчиков по времени, поэтому первый сэмпл всегда
    /// 0 Б/с: истории ещё нет, и выдумывать её нельзя.
    fn spawn_net_poller(&self) {
        if self.options.net == NetChoice::None {
            return;
        }
        let tx = self.incoming_tx.clone();
        let running = self.running.clone();
        thread::spawn(move || {
            let mut prev: Option<NetInfo> = None;
            while running.load(Ordering::SeqCst) {
                match netinfo::read() {
                    Some(mut info) => {
                        if let Some(prev) = &prev {
                            let (rx, tx_rate) = netinfo::rates(prev, &info);
                            info.rx_rate = rx;
                            info.tx_rate = tx_rate;
                        }
                        prev = Some(info.clone());
                        if tx.send(Incoming::Net(info)).is_err() {
                            break;
                        }
                    }
                    None => {
                        if tx.send(Incoming::NetUnavailable).is_err() {
                            break;
                        }
                    }
                }
                // Одна секунда кусками по 100 мс: выключение сервера не должно
                // ждать целый период опроса.
                for _ in 0..10 {
                    if !running.load(Ordering::SeqCst) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });
    }

    /// Одна строка о сети в serial — при первом сэмпле (или первом отказе).
    fn log_net_once(&mut self, info: Option<&NetInfo>) {
        if self.net_logged || self.options.quiet {
            self.net_logged = true;
            return;
        }
        self.net_logged = true;
        match info {
            Some(info) => {
                let addr = match (info.ip, info.prefix) {
                    (Some(ip), Some(prefix)) => format!("{ip}/{prefix}"),
                    (Some(ip), None) => ip.to_string(),
                    _ => "none".to_string(),
                };
                let state = match (info.up, info.running) {
                    (true, true) => "up running",
                    (true, false) => "up no-link",
                    _ => "down",
                };
                println!(
                    "pagh-desktop: net iface={} ip={} {}",
                    info.iface, addr, state
                );
            }
            None => println!("pagh-desktop: net unavailable (/proc/net/dev: no eth0)"),
        }
    }

    /// Создать одно демонстрационное окно без AF_UNIX-клиента (QEMU e2e).
    fn ensure_demo_window(&mut self) {
        if !self.options.demo_window || self.demo_window_created {
            return;
        }
        self.demo_window_created = true;
        let (width, height) = (640u32, 400u32);
        let events = match self.compositor.add_window(
            0,
            1,
            "Terminal - pagh",
            width,
            height,
            PixelFormat::Xrgb8888,
        ) {
            Ok(events) => events,
            Err(err) => {
                eprintln!("pagh-desktop: demo window: {err}");
                return;
            }
        };
        self.dispatch(events);
        let state = demo::DemoState {
            focused: true,
            mouse: Some(Point::new(280, 160)),
            last_input: "move 280,160".into(),
            ..demo::DemoState::default()
        };
        let surface = demo::terminal_surface(width, height, &state);
        if let Err(err) = self.compositor.commit_surface(0, 1, surface) {
            eprintln!("pagh-desktop: demo frame: {err}");
            return;
        }
        // Не вызываем render_full() здесь: он бы погасил damage до первого
        // present, и fbdev остался бы с неотрисованным кадром.
        println!("pagh-desktop: embedded demo window id=1 {width}x{height}");
    }

    /// Прочитать все накопленные evdev-события (неблокирующе).
    fn poll_input(&mut self) {
        let Some(input) = self.input.as_mut() else {
            return;
        };
        match input.poll() {
            Ok(actions) => {
                if !actions.is_empty() {
                    self.apply_input_actions(actions);
                }
            }
            Err(err) => {
                eprintln!("pagh-desktop: evdev: {err}; input отключён");
                self.input = None;
            }
        }
    }

    fn apply_input_actions(&mut self, actions: Vec<InputAction>) {
        for action in actions {
            if self.options.input_trace {
                match action {
                    InputAction::Motion { dx, dy } => println!("evdev: rel dx={dx} dy={dy}"),
                    InputAction::Wheel { delta } => println!("evdev: wheel {delta}"),
                    InputAction::Button { button, pressed } => println!(
                        "evdev: button {} {}",
                        button_name(button),
                        if pressed { "down" } else { "up" }
                    ),
                    InputAction::Key {
                        code,
                        pressed,
                        modifiers,
                    } => println!(
                        "evdev: key code={code} {} mods={modifiers:#x}",
                        if pressed { "down" } else { "up" }
                    ),
                }
            }
            let events = match action {
                InputAction::Motion { dx, dy } => {
                    let before = self.compositor.selection_rect();
                    let events = self.compositor.pointer_motion_relative(dx, dy);
                    if self.options.input_trace {
                        if let (Some(before), Some(after)) =
                            (before, self.compositor.selection_rect())
                        {
                            if before != after {
                                println!(
                                    "pagh-desktop: selection rect=({}, {}, {}x{})",
                                    after.x, after.y, after.w, after.h
                                );
                            }
                        }
                    }
                    events
                }
                InputAction::Wheel { delta } => self.compositor.axis(0, delta),
                InputAction::Button { button, pressed } => {
                    let before = self.compositor.selection_rect();
                    let events = self.compositor.pointer_button(button, pressed);
                    // e2e evidence for the rubber-band selection: the trace names
                    // the drag start and the committed rect; the QEMU screenshot
                    // is taken between them.
                    if self.options.input_trace && button == MouseButton::Left {
                        let after = self.compositor.selection_rect();
                        if pressed && self.compositor.selection_drag_active() {
                            println!(
                                "pagh-desktop: selection drag start pointer=({}, {})",
                                self.compositor.pointer().x,
                                self.compositor.pointer().y
                            );
                        } else if !pressed {
                            if let Some(rect) = after {
                                println!(
                                    "pagh-desktop: selection committed rect=({}, {}, {}x{})",
                                    rect.x, rect.y, rect.w, rect.h
                                );
                            } else if before.is_some() {
                                println!("pagh-desktop: selection cleared");
                            }
                        }
                    }
                    events
                }
                InputAction::Key {
                    code,
                    pressed,
                    modifiers,
                } => {
                    let events = self.compositor.key_event(code, pressed, modifiers);
                    if code == crate::protocol::key::F10 && pressed && self.options.input_trace {
                        println!(
                            "pagh-desktop: shortcut F10 menu_open={} pointer=({}, {})",
                            self.compositor.context_menu().is_some(),
                            self.compositor.pointer().x,
                            self.compositor.pointer().y
                        );
                    }
                    events
                }
            };
            self.dispatch(events);
        }
    }

    /// Проверка, что fbdev-кадр реально попал в scanout (панель/обои/окно).
    fn maybe_selfcheck(&mut self) {
        if self.selfcheck_done {
            return;
        }
        let Some(due) = self.selfcheck_due else {
            return;
        };
        if Instant::now() < due {
            return;
        }
        self.selfcheck_done = true;
        if self.backend.as_fbdev().is_none() {
            return;
        }
        // Гарантировать, что накопленный damage уже в scanout (selfcheck может
        // сработать раньше, чем серверный цикл сделал следующий present).
        if let Err(err) = self.render_if_needed() {
            eprintln!("pagh-desktop: selfcheck present: {err}");
        }
        let Some(fbdev) = self.backend.as_fbdev() else {
            return;
        };
        let screen = self.compositor.screen();
        let panel = fbdev.sample(screen.w / 2, screen.h - 4);
        let wallpaper = fbdev.sample(4, 4);
        let window_count = self
            .compositor
            .windows()
            .iter()
            .filter(|window| !window.minimized)
            .count();
        let window_pixel = self
            .compositor
            .windows()
            .iter()
            .find(|window| !window.minimized)
            .and_then(|window| {
                let content = window.content_rect();
                fbdev.sample(content.center().x, content.center().y)
            });
        let pass = wallpaper.is_some()
            && panel.is_some()
            && panel != wallpaper
            && panel
                .map(|color| (color & 0x00FF_FFFF) != 0)
                .unwrap_or(false)
            && (window_count == 0 || window_pixel.is_some_and(|pixel| Some(pixel) != wallpaper));
        let fmt = |value: Option<u32>| match value {
            Some(color) => format!("#{:06x}", color & 0x00FF_FFFF),
            None => "none".to_string(),
        };
        let canvas_panel = self.compositor.frame().get(screen.w / 2, screen.h - 4) & 0x00FF_FFFF;
        let canvas_wallpaper = self.compositor.frame().get(4, 4) & 0x00FF_FFFF;
        println!(
            "pagh-desktop: fbdev selfcheck {} panel={} wallpaper={} windows={} window_px={} frames={} canvas_panel=#{:06x} canvas_wall=#{:06x}",
            if pass { "PASS" } else { "FAIL" },
            fmt(panel),
            fmt(wallpaper),
            window_count,
            fmt(window_pixel),
            self.backend.frames(),
            canvas_panel,
            canvas_wallpaper
        );
        self.selfcheck_failed = !pass;
    }

    fn accept_pending(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => self.attach(stream),
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) => {
                    if !self.options.quiet {
                        eprintln!("pagh-desktop: accept: {err}");
                    }
                    break;
                }
            }
        }
    }

    fn attach(&mut self, stream: UnixStream) {
        let conn = self.next_conn;
        self.next_conn += 1;
        let writer = match stream.try_clone() {
            Ok(writer) => writer,
            Err(err) => {
                eprintln!("pagh-desktop: не удалось клонировать сокет: {err}");
                return;
            }
        };
        let _ = writer.set_write_timeout(Some(Duration::from_secs(2)));
        let tx = self.incoming_tx.clone();
        let running = self.running.clone();
        // В Pagh у AF_UNIX нет очередей ожидания: блокирующий recvmsg
        // уступает CPU и перечитывает очередь, то есть горячий тред-читатель
        // занимает ядро целиком и на каждом витке берёт глобальный compat-лок.
        // С неблокирующим сокетом тред спит между попытками (см. read_loop).
        let reader = stream;
        let _ = reader.set_read_timeout(Some(Duration::from_millis(200)));
        let _ = reader.set_nonblocking(true);
        let quiet = self.options.quiet;
        thread::spawn(move || read_loop(conn, reader, tx, running, quiet));
        self.clients.push(ClientConn {
            id: conn,
            client_id: None,
            name: String::new(),
            writer: Arc::new(Mutex::new(writer)),
        });
        if !self.options.quiet {
            eprintln!("pagh-desktop: клиент #{conn} подключён");
        }
    }

    fn handle_incoming(&mut self, incoming: Incoming) -> bool {
        match incoming {
            Incoming::Client { conn, message, shm } => {
                self.handle_client_message(conn, message, shm);
                true
            }
            Incoming::Disconnected(conn) => {
                self.drop_client(conn);
                true
            }
            Incoming::ProtocolError { conn, detail } => {
                self.reply_error(conn, ERR_BAD_MESSAGE, detail);
                if let Some(index) = self.clients.iter().position(|c| c.id == conn) {
                    shutdown_stream(&self.clients[index].writer);
                }
                self.drop_client(conn);
                true
            }
            Incoming::Command(command) => self.handle_command(command),
            Incoming::Net(info) => {
                self.log_net_once(Some(&info));
                self.compositor.set_network_info(Some(info));
                true
            }
            Incoming::NetUnavailable => {
                self.log_net_once(None);
                // Нет данных — честное состояние панели, а не «отключено».
                self.compositor.set_network_info(None);
                true
            }
        }
    }

    fn handle_command(&mut self, command: ServerCommand) -> bool {
        match command {
            ServerCommand::PointerMove { x, y } => {
                let events = self.compositor.pointer_motion(Point::new(x, y));
                self.dispatch(events);
            }
            ServerCommand::PointerRelative { dx, dy } => {
                let events = self.compositor.pointer_motion_relative(dx, dy);
                self.dispatch(events);
            }
            ServerCommand::PointerButton { button, pressed } => {
                let events = self
                    .compositor
                    .pointer_button(MouseButton::from_wire(button), pressed);
                self.dispatch(events);
            }
            ServerCommand::Key {
                code,
                pressed,
                modifiers,
            } => {
                let events = self.compositor.key_event(code, pressed, modifiers);
                self.dispatch(events);
            }
            ServerCommand::Axis {
                horizontal,
                vertical,
            } => {
                let events = self.compositor.axis(horizontal, vertical);
                self.dispatch(events);
            }
            ServerCommand::Screenshot(path) => {
                if let Err(err) = self.take_screenshot(&path) {
                    eprintln!("pagh-desktop: screenshot {}: {err}", path.display());
                }
            }
            ServerCommand::SamplePixel { x, y, tx } => {
                let pixel = self.compositor.frame().get(x, y) & 0x00FF_FFFF;
                let _ = tx.send(Some(pixel));
            }
            ServerCommand::CloseWindow { window_id } => {
                // notify=true: клиент получает close(reason=SERVER) и сам
                // завершается — это имитация нажатия `x` в заголовке.
                let events = self.compositor.remove_window(window_id, true);
                self.shm.remove(&window_id);
                self.dispatch(events);
            }
            ServerCommand::Shutdown => return false,
        }
        true
    }

    fn handle_client_message(&mut self, conn: u64, message: Message, shm: Option<ShmArrival>) {
        let Some(index) = self.clients.iter().position(|c| c.id == conn) else {
            return;
        };
        let client_id = self.clients[index].client_id;
        match message {
            Message::Hello { version, pid, app } => {
                if client_id.is_some() {
                    self.reply_error(conn, ERR_BAD_MESSAGE, "повторный hello");
                    return;
                }
                if version != VERSION {
                    self.reply_error(
                        conn,
                        ERR_UNSUPPORTED,
                        format!("поддерживается только протокол версии {VERSION}"),
                    );
                    return;
                }
                let id = self.next_client;
                self.next_client += 1;
                self.clients[index].client_id = Some(id);
                self.clients[index].name = app.chars().take(64).collect();
                if !self.options.quiet {
                    eprintln!(
                        "pagh-desktop: hello client_id={id} pid={pid} app={:?}",
                        self.clients[index].name
                    );
                }
                self.send_message(
                    index,
                    &Message::Welcome {
                        version: VERSION,
                        client_id: id,
                        width: self.compositor.screen().w as u32,
                        height: self.compositor.screen().h as u32,
                        flags: WELCOME_FORMAT_XRGB8888
                            | WELCOME_FORMAT_RGB565
                            | WELCOME_SHM_SURFACE,
                    },
                );
            }
            Message::Create {
                window_id,
                width,
                height,
                format,
                title,
            } => {
                let Some(client_id) = client_id else {
                    self.reply_error(conn, ERR_NOT_HELLO, "сначала hello");
                    return;
                };
                let Some(format) = PixelFormat::from_wire(format) else {
                    self.reply_error(conn, ERR_INVALID_SURFACE, "неизвестный формат пикселей");
                    return;
                };
                match self
                    .compositor
                    .add_window(client_id, window_id, title, width, height, format)
                {
                    Ok(events) => self.dispatch(events),
                    Err(err) => self.reply_error(conn, err.code(), err.to_string()),
                }
            }
            Message::Commit {
                window_id,
                width,
                height,
                format,
                stride,
                pixels,
            } => {
                let Some(client_id) = client_id else {
                    self.reply_error(conn, ERR_NOT_HELLO, "сначала hello");
                    return;
                };
                // Клиент вернулся на legacy frame upload: SHM generation больше
                // не нужна, освобождаем её и принимаем полный кадр.
                if let Some(old) = self.shm.remove(&window_id) {
                    if old.client_id == client_id {
                        self.send_to_client(
                            client_id,
                            Message::ShmRelease {
                                window_id,
                                generation: old.generation,
                            },
                        );
                    }
                }
                match Surface::from_wire(width, height, stride, format, &pixels) {
                    Ok(surface) => match self
                        .compositor
                        .commit_surface(client_id, window_id, surface)
                    {
                        Ok(events) => self.dispatch(events),
                        Err(err) => self.reply_error(conn, err.code(), err.to_string()),
                    },
                    Err(err) => self.reply_error(conn, ERR_INVALID_SURFACE, err.to_string()),
                }
            }
            Message::ShmOffer {
                window_id,
                width,
                height,
                format,
                stride,
                flags,
            } => {
                let Some(client_id) = client_id else {
                    self.reply_error(conn, ERR_NOT_HELLO, "сначала hello");
                    return;
                };
                self.handle_shm_offer(
                    conn, client_id, window_id, width, height, format, stride, flags, shm,
                );
            }
            Message::ShmCommit {
                window_id,
                generation,
                rects,
            }
            | Message::ShmDamage {
                window_id,
                generation,
                rects,
            } => {
                let Some(client_id) = client_id else {
                    self.reply_error(conn, ERR_NOT_HELLO, "сначала hello");
                    return;
                };
                self.handle_shm_update(conn, client_id, window_id, generation, &rects);
            }
            Message::ShmRelease {
                window_id,
                generation,
            } => {
                let Some(client_id) = client_id else {
                    self.reply_error(conn, ERR_NOT_HELLO, "сначала hello");
                    return;
                };
                let owned = self
                    .shm
                    .get(&window_id)
                    .map(|binding| {
                        binding.client_id == client_id && binding.generation == generation
                    })
                    .unwrap_or(false);
                if owned {
                    self.shm.remove(&window_id);
                }
            }
            Message::Damage { window_id, rects } => {
                let Some(client_id) = client_id else {
                    self.reply_error(conn, ERR_NOT_HELLO, "сначала hello");
                    return;
                };
                if let Err(err) = self.compositor.note_damage(client_id, window_id, &rects) {
                    self.reply_error(conn, err.code(), err.to_string());
                }
            }
            Message::Close { window_id, .. } => {
                let Some(client_id) = client_id else {
                    self.reply_error(conn, ERR_NOT_HELLO, "сначала hello");
                    return;
                };
                let owned = self
                    .compositor
                    .window(window_id)
                    .map(|window| window.client_id == client_id)
                    .unwrap_or(false);
                if !owned {
                    self.reply_error(conn, ERR_NOT_OWNER, "нельзя закрыть чужое окно");
                    return;
                }
                let events = self.compositor.remove_window(window_id, false);
                self.shm.remove(&window_id);
                self.dispatch(events);
            }
            Message::Ping { nonce } => {
                self.send_message(index, &Message::Pong { nonce });
            }
            Message::Launch { app, cwd } => {
                if client_id.is_none() {
                    self.reply_error(conn, ERR_NOT_HELLO, "сначала hello");
                    return;
                }
                self.launch_app(&app, &cwd, conn);
            }
            other => {
                self.reply_error(
                    conn,
                    ERR_UNSUPPORTED,
                    format!("клиент не должен присылать {}", other.name()),
                );
            }
        }
    }

    fn handle_shm_offer(
        &mut self,
        conn: u64,
        client_id: u64,
        window_id: u32,
        width: u32,
        height: u32,
        format: u32,
        stride: u32,
        flags: u32,
        shm: Option<ShmArrival>,
    ) {
        if flags != 0 {
            self.reply_error(conn, ERR_INVALID_SURFACE, "shm_offer: flags должны быть 0");
            return;
        }
        let Some(format) = PixelFormat::from_wire(format) else {
            self.reply_error(conn, ERR_INVALID_SURFACE, "shm_offer: неизвестный формат");
            return;
        };
        let owned = self
            .compositor
            .window(window_id)
            .map(|window| window.client_id == client_id)
            .unwrap_or(false);
        if !owned {
            let exists = self.compositor.window(window_id).is_some();
            self.reply_error(
                conn,
                if exists {
                    ERR_NOT_OWNER
                } else {
                    ERR_INVALID_SURFACE
                },
                "shm_offer: окно не найдено или чужое",
            );
            return;
        }
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            self.reply_error(conn, ERR_INVALID_SURFACE, "shm_offer: недопустимый размер");
            return;
        }
        let minimum = width.saturating_mul(format.bytes_per_pixel());
        if stride < minimum || stride > minimum.saturating_add(4096) {
            self.reply_error(
                conn,
                ERR_INVALID_SURFACE,
                format!("shm_offer: stride {stride} вне [{minimum}, {minimum}+4096]"),
            );
            return;
        }
        let Some(needed) = (stride as usize).checked_mul(height as usize) else {
            self.reply_error(conn, ERR_INVALID_SURFACE, "shm_offer: overflow");
            return;
        };
        let Some(shm) = shm else {
            self.reply_error(conn, ERR_BAD_MESSAGE, "shm_offer без SCM_RIGHTS fd");
            return;
        };
        let mapping = match shm {
            // Отображение сделал читающий поток: только его fd-таблица видит
            // принятый `SCM_RIGHTS` дескриптор (см. [`ShmArrival`]).
            ShmArrival::Mapped(mapping) => mapping,
            ShmArrival::Failed(err) => {
                self.reply_error(conn, ERR_INVALID_SURFACE, format!("shm_offer: {err}"));
                return;
            }
        };
        if mapping.len() < needed {
            // Страховка от расхождения проверок между потоками: сервер не
            // должен отдавать композитору view короче `stride*height`.
            self.reply_error(
                conn,
                ERR_INVALID_SURFACE,
                format!(
                    "shm_offer: отображено {} байт, нужно {needed}",
                    mapping.len()
                ),
            );
            return;
        }
        if let Err(err) = self
            .compositor
            .install_shm_surface(client_id, window_id, width, height, format)
        {
            self.reply_error(conn, err.code(), err.to_string());
            return;
        }
        let generation = self.next_shm_generation.max(1);
        self.next_shm_generation = generation.wrapping_add(1).max(1);
        let old = self.shm.insert(
            window_id,
            crate::shm::ShmBinding {
                client_id,
                window_id,
                generation,
                width,
                height,
                format,
                stride,
                mapping,
            },
        );
        self.counters.shm_offers.fetch_add(1, Ordering::Relaxed);
        if !self.options.quiet {
            // Единственная строка, по которой видно, каким путём клиент рисует:
            // shm-поверхность или полный кадр. Без неё «окно есть, контент есть»
            // не отличает рабочий SHM от молчаливого fallback'а.
            eprintln!(
                "pagh-desktop: shm surface window={window_id} {width}x{height} stride={stride} generation={generation}"
            );
        }
        self.send_to_client(
            client_id,
            Message::ShmReady {
                window_id,
                generation,
            },
        );
        if let Some(old) = old {
            // Старый generation освобождается сразу после reoffer; клиент
            // обязан прислать новый memfd/fd, старые commit'ы будут отброшены.
            self.send_to_client(
                client_id,
                Message::ShmRelease {
                    window_id,
                    generation: old.generation,
                },
            );
        }
    }

    fn handle_shm_update(
        &mut self,
        conn: u64,
        client_id: u64,
        window_id: u32,
        generation: u32,
        rects: &[(i32, i32, u32, u32)],
    ) {
        let Some(binding) = self.shm.get(&window_id) else {
            self.reply_error(conn, ERR_INVALID_SURFACE, "shm commit без shm_offer");
            return;
        };
        if binding.client_id != client_id {
            self.reply_error(conn, ERR_NOT_OWNER, "shm commit чужого окна");
            return;
        }
        if binding.generation != generation {
            // Устаревший кадр: молча отбрасываем (клиент уже получил RELEASE).
            return;
        }
        let Some(view) = binding.view() else {
            self.reply_error(
                conn,
                ERR_INVALID_SURFACE,
                "shm mapping короче stride*height",
            );
            return;
        };
        let bpp = binding.format.bytes_per_pixel() as u64;
        let view_rect =
            crate::geometry::Rect::new(0, 0, binding.width as i32, binding.height as i32);
        let mut area = 0u64;
        for &(x, y, w, h) in rects {
            let w = w.min(i32::MAX as u32) as i32;
            let h = h.min(i32::MAX as u32) as i32;
            if let Some(rect) = crate::geometry::Rect::new(x, y, w, h).intersection(view_rect) {
                area += (rect.area().max(0)) as u64;
            }
        }
        match self
            .compositor
            .shm_update(client_id, window_id, view, rects)
        {
            Ok(()) => {
                self.counters.shm_commits.fetch_add(1, Ordering::Relaxed);
                self.counters
                    .shm_bytes
                    .fetch_add(area * bpp, Ordering::Relaxed);
            }
            Err(err) => self.reply_error(conn, err.code(), err.to_string()),
        }
    }

    fn drop_shm_for_client(&mut self, client_id: u64) {
        self.shm.retain(|_, binding| binding.client_id != client_id);
    }

    fn reply_error(&mut self, conn: u64, code: u32, message: impl Into<String>) {
        if let Some(index) = self.clients.iter().position(|c| c.id == conn) {
            self.send_message(
                index,
                &Message::Error {
                    code,
                    message: message.into(),
                },
            );
        }
    }

    fn send_message(&mut self, index: usize, message: &Message) {
        let Some(conn) = self.clients.get(index) else {
            return;
        };
        let Ok(mut writer) = conn.writer.lock() else {
            return;
        };
        if let Err(err) = protocol::write_message(&mut *writer, message) {
            if !self.options.quiet {
                eprintln!("pagh-desktop: отправка клиенту #{}: {err}", conn.id);
            }
        }
    }

    fn send_to_client(&mut self, client_id: u64, message: Message) {
        if let Some(index) = self
            .clients
            .iter()
            .position(|conn| conn.client_id == Some(client_id))
        {
            self.send_message(index, &message);
        }
    }

    fn dispatch(&mut self, events: Vec<HostEvent>) {
        for event in events {
            match event {
                HostEvent::Send { client_id, message } => {
                    self.send_to_client(client_id, message);
                }
                HostEvent::Launch(app) => self.launch(app),
            }
        }
    }

    /// Запустить приложение по событию композитора (иконка панели, пункт меню,
    /// хоткей Ctrl+Alt+T).
    ///
    /// Сначала ищется цель `launch_targets` по `AppKind::id()` — панель/меню
    /// обязаны открывать тот же бинарник, что и клиентский PDP `launch`
    /// (type 13). Обычный образ задаёт цели в `/mnt/etc/autorun`
    /// (`desktop-autorun`), поэтому демо-клиент — не «тихий фолбэк»: без
    /// `--spawn-demo` нечем запускать, и это честно пишется в лог, а не
    /// выглядит как молчащее действие. `--spawn-demo` остаётся ровно для
    /// тестов (`tools/e2e.py desktop --legacy-demo`) и работает как раньше.
    fn launch(&mut self, app: AppKind) {
        if let Some(target) = self
            .options
            .launch_targets
            .iter()
            .find(|target| target.app == app.id())
            .cloned()
        {
            let argv = target.command_for(&self.options.socket);
            if let Err(error) = self.spawn_target(app.id(), &argv) {
                if !self.options.quiet {
                    eprintln!("pagh-desktop: launch {}: {error}", app.id());
                }
            }
            return;
        }
        if !self.options.spawn_demo {
            if !self.options.quiet {
                eprintln!("{}", unconfigured_launch_message(app.id()));
            }
            return;
        }
        if let Err(error) = self.spawn_demo(app) {
            if !self.options.quiet {
                eprintln!("pagh-desktop: launch {}: {error}", app.id());
            }
        }
    }

    /// Запустить готовую argv приложения из `launch_targets`.
    ///
    /// Общий путь `launch_app` (клиент PDP, type 13) и `launch` (панель/меню/
    /// хоткей): argv уже содержит подставленный сокет (`LaunchTarget::command_for`),
    /// дочерний процесс попадает в `children` и reap'ится в главном цикле.
    fn spawn_target(&mut self, app: &str, argv: &[std::ffi::OsString]) -> Result<(), String> {
        let Some(program) = argv.first() else {
            return Err(format!("пустая команда для {app}"));
        };
        match Command::new(program).args(&argv[1..]).spawn() {
            Ok(child) => {
                if !self.options.quiet {
                    eprintln!(
                        "pagh-desktop: launch {app} ok -> {} (pid {})",
                        program.to_string_lossy(),
                        child.id()
                    );
                }
                self.children.push(child);
                Ok(())
            }
            Err(err) => Err(format!("запуск {app}: {err}")),
        }
    }

    /// Запустить демо-клиент для известного приложения.
    fn spawn_demo(&mut self, app: AppKind) -> Result<(), String> {
        if !self.options.spawn_demo {
            return Err("spawn_demo выключен".to_string());
        }
        let binary = self.options.demo_binary.clone().or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.join("pagh-desktop-demo")))
        });
        let Some(binary) = binary else {
            return Err("не найден демо-клиент для запуска".to_string());
        };
        match Command::new(&binary)
            .arg("--connect")
            .arg(&self.options.socket)
            .arg("--app")
            .arg(app.id())
            .spawn()
        {
            Ok(child) => {
                if !self.options.quiet {
                    // Стабильный English-маркер для e2e (панель/меню Files/
                    // Terminal идут тем же путём, что и --spawn-demo).
                    eprintln!(
                        "pagh-desktop: launch {} ok -> {} (pid {})",
                        app.id(),
                        binary.display(),
                        child.id()
                    );
                }
                self.children.push(child);
                Ok(())
            }
            Err(err) => Err(format!("запуск {}: {err}", binary.display())),
        }
    }

    /// Обработать `launch` (type 13): запустить приложение по идентификатору.
    ///
    /// Сначала ищется явная цель `ServerOptions.launch_targets`. Цели нет: без
    /// `--spawn-demo` клиенту возвращается `ERR_UNSUPPORTED` с подсказкой о
    /// `--launch-target` (обычный образ не должен «молча» ничего не делать);
    /// с `--spawn-demo` известный `AppKind` запускается демо-клиентом, как в
    /// тестах.
    fn launch_app(&mut self, app: &str, cwd: &str, conn: u64) {
        if app.is_empty() || app.len() > 64 {
            self.reply_error(conn, ERR_UNSUPPORTED, "пустой или слишком длинный app id");
            return;
        }
        if let Some(target) = self
            .options
            .launch_targets
            .iter()
            .find(|target| target.app == app)
            .cloned()
        {
            let argv = target.command_for(&self.options.socket);
            match self.spawn_target(app, &argv) {
                Ok(()) => {
                    if !self.options.quiet && !cwd.is_empty() {
                        eprintln!("pagh-desktop: launch {app}: cwd {cwd} (подсказка)");
                    }
                }
                Err(error) => {
                    self.reply_error(conn, ERR_INTERNAL, error);
                }
            }
            return;
        }
        match AppKind::from_id(app) {
            // Без --spawn-demo демо-клиента нет: это обычный образ, где
            // приложение обязано быть в --launch-target. Клиент получает
            // внятную причину, а не «spawn_demo выключен».
            Some(_) if !self.options.spawn_demo => {
                self.reply_error(conn, ERR_UNSUPPORTED, unconfigured_launch_message(app));
            }
            Some(kind) => match self.spawn_demo(kind) {
                Ok(()) => {}
                Err(error) => {
                    self.reply_error(conn, ERR_UNSUPPORTED, format!("{app}: {error}"));
                }
            },
            None => {
                self.reply_error(
                    conn,
                    ERR_UNSUPPORTED,
                    format!("приложение {app:?} не настроено на сервере"),
                );
            }
        }
    }

    fn reap_children(&mut self) {
        self.children.retain_mut(|child| match child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(_) => false,
        });
    }

    fn drop_client(&mut self, conn: u64) {
        let Some(index) = self.clients.iter().position(|c| c.id == conn) else {
            return;
        };
        let client = self.clients.remove(index);
        if let Some(client_id) = client.client_id {
            let events = self.compositor.remove_client(client_id);
            self.drop_shm_for_client(client_id);
            self.dispatch(events);
        }
        if !self.options.quiet {
            eprintln!("pagh-desktop: клиент #{conn} отключён");
        }
        self.compositor.render_full();
    }

    /// Накопить frame time текущего кадра и, раз в `frame_log_every`
    /// presents, напечатать строку `frame-stats` (serial в QEMU).
    ///
    /// `frame_start` — `rdtsc` перед `render`; стадии читаются из
    /// [`crate::timing::take_frame`] сразу после `present`, поэтому строка
    /// сравнивает средние за окно, а не мгновенный кадр.
    fn note_frame_timing(&mut self, frame_start: u64, damage_rects: u64) {
        let stages = crate::timing::take_frame();
        let total = crate::timing::now_cycles().saturating_sub(frame_start);
        let frame_ms = crate::timing::cycles_to_ms(total);
        // Inter-frame gap: how long the compositor went *unscheduled* between
        // two presents. Measured from wall time, so a 2 s freeze is visible even
        // when both neighbouring frames were cheap to draw.
        let now = Instant::now();
        if let (Some(stats), Some(prev)) = (self.stats.as_mut(), self.last_present) {
            stats
                .bucket
                .note_gap(now.duration_since(prev).as_secs_f64() * 1000.0);
        }
        self.last_present = Some(now);
        if let Some(stats) = self.stats.as_mut() {
            stats.bucket.record(frame_ms, damage_rects);
        }
        // `--frame-log-every 0` (the default) must not accumulate samples: the
        // old code pushed every frame and only cleared them when a log line was
        // due, so a long-running compositor grew this Vec at frame rate.
        if self.options.frame_log_every == 0 {
            self.frames_since_log += 1;
            self.damage_since_log += damage_rects;
            return;
        }
        self.frame_samples.push(frame_ms);
        self.render_samples.push(stages.render_ms());
        self.blend_samples.push(stages.blend_ms());
        self.copy_samples.push(stages.copy_ms());
        self.present_samples.push(stages.present_ms());
        self.frames_since_log += 1;
        self.damage_since_log += damage_rects;
        self.opaque_since_log += stages.opaque_px;
        self.blended_since_log += stages.blended_px;
        let every = self.options.frame_log_every;
        if every == 0 || self.frames_since_log < every {
            return;
        }
        let frames = self.frames_since_log as f64;
        let painted = self.opaque_since_log + self.blended_since_log;
        let opaque_pct = if painted == 0 {
            0.0
        } else {
            self.opaque_since_log as f64 * 100.0 / painted as f64
        };
        if !self.options.quiet {
            println!(
                "pagh-desktop: frame-stats frames={} frame_ms avg={:.3} p50={:.3} p95={:.3} max={:.3} render_ms={:.3} blend_ms={:.3} copy_ms={:.3} present_ms={:.3} damage_rects={:.2}/frame opaque_pct={:.1}",
                self.frames_since_log,
                self.frame_samples.avg(),
                self.frame_samples.p50(),
                self.frame_samples.p95(),
                self.frame_samples.max(),
                self.render_samples.avg(),
                self.blend_samples.avg(),
                self.copy_samples.avg(),
                self.present_samples.avg(),
                self.damage_since_log as f64 / frames,
                opaque_pct
            );
        }
        self.frame_samples.clear();
        self.render_samples.clear();
        self.blend_samples.clear();
        self.copy_samples.clear();
        self.present_samples.clear();
        self.frames_since_log = 0;
        self.damage_since_log = 0;
        self.opaque_since_log = 0;
        self.blended_since_log = 0;
    }

    fn render_if_needed(&mut self) -> io::Result<()> {
        // Границы кадра: счётчики стадий обнуляются до `render`, читаются в
        // `note_frame_timing` после `present`.
        crate::timing::reset_frame();
        let frame_start = crate::timing::now_cycles();
        let compose_start = Instant::now();
        let damage = if self.options.full_render {
            self.compositor.render_full()
        } else {
            self.compositor.render()
        };
        self.counters
            .compose_ns
            .fetch_add(compose_start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if damage.is_empty() {
            // Idle: не трогаем scanout и не считаем present.
            return Ok(());
        }
        let bytes_per_pixel = self
            .backend
            .as_fbdev()
            .map(|fb| (fb.layout().bpp / 8) as u64)
            .unwrap_or(4);
        let area: i64 = damage.iter().map(|rect| rect.area().max(0)).sum();
        let present_start = Instant::now();
        {
            let _stage = crate::timing::Scope::new(crate::timing::Stage::Present);
            self.backend.present(self.compositor.frame(), &damage)?;
        }
        self.counters
            .present_ns
            .fetch_add(present_start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        self.counters.presents.fetch_add(1, Ordering::Relaxed);
        self.counters
            .bytes_presented
            .fetch_add(area as u64 * bytes_per_pixel, Ordering::Relaxed);
        self.counters
            .damage_rects
            .fetch_add(damage.len() as u64, Ordering::Relaxed);
        self.note_frame_timing(frame_start, damage.len() as u64);
        if let Some(dir) = &self.options.screenshot_dir {
            let path = dir.join(format!("frame_{:05}.png", self.backend.frames()));
            if let Err(err) = crate::png::write_png(&path, self.compositor.frame()) {
                eprintln!("pagh-desktop: screenshot {}: {err}", path.display());
            }
        }
        Ok(())
    }

    /// Сколько ждать до следующего пробуждения: событие в сокете, ввод,
    /// 60 Hz-дедлайн кадра или служебный таймер.
    fn next_wakeup(&self, next_frame: Instant) -> Duration {
        let now = Instant::now();
        let mut timeout = if self.compositor.has_damage() {
            next_frame.saturating_duration_since(now)
        } else if self.clients.is_empty() {
            // Idle-рабочий стол без клиентов: редкий poll, 0 presents.
            Duration::from_millis(50)
        } else {
            // Живой клиент: просыпаемся на каждом кадре, чтобы commit не ждал.
            Duration::from_millis(16)
        };
        if self.input.is_some() {
            // evdev — неблокирующий fd; опрашиваем его чаще, чем кадр.
            timeout = timeout.min(Duration::from_millis(4));
        }
        if let Some(due) = self.exit_due {
            timeout = timeout.min(due.saturating_duration_since(now));
        }
        if let Some(due) = self.selfcheck_due {
            if !self.selfcheck_done {
                timeout = timeout.min(due.saturating_duration_since(now));
            }
        }
        timeout.max(Duration::from_millis(1))
    }

    fn cleanup(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(mut stats) = self.stats.take() {
            stats.finish(Instant::now());
        }
        for client in &self.clients {
            shutdown_stream(&client.writer);
        }
        self.clients.clear();
        for mut child in self.children.drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
        if self.options.socket.exists() {
            let _ = std::fs::remove_file(&self.options.socket);
        }
        if !self.options.quiet {
            println!(
                "pagh-desktop: остановлен, кадров отрисовано {}",
                self.backend.frames()
            );
            if self.backend.as_fbdev().is_some() {
                println!("pagh-desktop: fbdev closed; screen takeover remains armed (ABI one-way)");
            }
            println!(
                "pagh-desktop: stats {}",
                self.counters.snapshot().log_line()
            );
        }
    }

    /// Отправить приветствие клиенту без сервера (для unit-тестов протокола).
    pub fn welcome_for(&self, client_id: u64) -> Welcome {
        Welcome {
            client_id,
            screen: crate::geometry::Size::new(
                self.compositor.screen().w as u32,
                self.compositor.screen().h as u32,
            ),
            flags: WELCOME_FORMAT_XRGB8888 | WELCOME_FORMAT_RGB565 | WELCOME_SHM_SURFACE,
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if self.options.socket.exists() {
            let _ = std::fs::remove_file(&self.options.socket);
        }
    }
}

fn button_name(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Middle => "middle",
        MouseButton::Right => "right",
        MouseButton::Other(_) => "other",
    }
}

fn shutdown_stream(writer: &Arc<Mutex<UnixStream>>) {
    if let Ok(guard) = writer.lock() {
        let _ = guard.shutdown(std::net::Shutdown::Both);
    }
}

fn read_loop(
    conn: u64,
    stream: UnixStream,
    tx: Sender<Incoming>,
    running: Arc<AtomicBool>,
    quiet: bool,
) {
    let mut decoder = Decoder::new();
    let mut buffer = [0u8; 64 * 1024];
    // memfd'ы приходят ancillary-данными; SHM_OFFER забирает по одному fd.
    let mut pending_fds: std::collections::VecDeque<OwnedFd> = std::collections::VecDeque::new();
    'read: loop {
        if !running.load(Ordering::SeqCst) {
            break;
        }
        match crate::shm::recv_with_fds(&stream, &mut buffer) {
            Ok((0, _)) => break,
            Ok((n, fds)) => {
                pending_fds.extend(fds);
                // Защита от клиента, засыпающего сервер fd'ами без offer'ов.
                if pending_fds.len() > 8 {
                    if !quiet {
                        eprintln!(
                            "pagh-desktop: conn={conn}: слишком много SCM_RIGHTS fd без shm_offer"
                        );
                    }
                    let _ = tx.send(Incoming::ProtocolError {
                        conn,
                        detail: "слишком много SCM_RIGHTS fd без shm_offer".to_string(),
                    });
                    return;
                }
                if let Err(err) = decoder.push(&buffer[..n]) {
                    if !quiet {
                        eprintln!("pagh-desktop: conn={conn}: decode: {err}");
                    }
                    let _ = tx.send(Incoming::ProtocolError {
                        conn,
                        detail: err.to_string(),
                    });
                    return;
                }
                loop {
                    match decoder.next_message() {
                        Ok(Some(message)) => {
                            // memfd отображаем ЗДЕСЬ: fd, установленный
                            // `recvmsg`, живёт в таблице этого треда (у каждого
                            // треда своя копия fd-таблицы, `docs/procfs.md`
                            // §13.7 #22), и главный поток его не увидит. Через
                            // канал едет готовое `ShmMapping`.
                            let shm = if let Message::ShmOffer {
                                width,
                                height,
                                format,
                                stride,
                                ..
                            } = message
                            {
                                match pending_fds.pop_front() {
                                    Some(fd) => {
                                        // Невалидные поля отсеет главный поток
                                        // (у него — точный код ошибки); тогда fd
                                        // просто закрывается здесь же.
                                        match crate::shm::offer_required_len(
                                            width, height, format, stride,
                                        ) {
                                            Some(needed) => {
                                                Some(match crate::shm::map_readonly(fd, needed) {
                                                    Ok(mapping) => ShmArrival::Mapped(mapping),
                                                    Err(err) => {
                                                        eprintln!(
                                                            "pagh-desktop: conn={conn}: shm_offer: \
                                                             отображение memfd: {err}"
                                                        );
                                                        ShmArrival::Failed(err)
                                                    }
                                                })
                                            }
                                            // Поля вне пределов: fd закрывается
                                            // здесь, сообщение всё равно уходит —
                                            // точную ошибку клиенту скажет
                                            // главный поток (проверки до fd).
                                            None => {
                                                drop(fd);
                                                None
                                            }
                                        }
                                    }
                                    None => {
                                        eprintln!(
                                            "pagh-desktop: conn={conn}: shm_offer без SCM_RIGHTS fd"
                                        );
                                        let _ = tx.send(Incoming::ProtocolError {
                                            conn,
                                            detail: "shm_offer без SCM_RIGHTS fd".to_string(),
                                        });
                                        return;
                                    }
                                }
                            } else {
                                None
                            };
                            if tx.send(Incoming::Client { conn, message, shm }).is_err() {
                                break 'read;
                            }
                        }
                        Ok(None) => break,
                        Err(err) => {
                            if !quiet {
                                eprintln!("pagh-desktop: conn={conn}: decode: {err}");
                            }
                            let _ = tx.send(Incoming::ProtocolError {
                                conn,
                                detail: err.to_string(),
                            });
                            return;
                        }
                    }
                }
            }
            Err(err)
                if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::TimedOut =>
            {
                thread::sleep(Duration::from_millis(2));
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {
                // SIGTTOU/SIGCONT и прочие сигналы прерывают recvmsg; это не
                // потеря клиента — повторяем, а не рвём соединение.
                thread::sleep(Duration::from_millis(2));
            }
            Err(err) => {
                // Молчаливый выход читающего потока — потеря клиента без
                // строки в логе, поэтому причину печатаем всегда — КРОМЕ
                // --quiet: фоновый композитор с --quiet не должен писать в
                // управляющий терминал (TOSTOP останавливает его SIGTTOU,
                // и клиенты получают «handshake: timed out»; e2e это ловит).
                if !quiet {
                    eprintln!("pagh-desktop: read_loop conn={conn} failed: {err}");
                }
                break;
            }
        }
    }
    let _ = tx.send(Incoming::Disconnected(conn));
}

/// Утилита для тестов: уникальный путь сокета.
pub fn unique_socket_path(tag: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "pagh-desktop-{tag}-{}-{nanos}.sock",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headless_options(tag: &str) -> ServerOptions {
        ServerOptions {
            socket: unique_socket_path(tag),
            width: 640,
            height: 480,
            quiet: true,
            backend: OutputChoice::Headless,
            input: InputChoice::None,
            ..ServerOptions::default()
        }
    }

    #[test]
    fn auto_backend_falls_back_to_headless_without_fb0() {
        let options = ServerOptions {
            backend: OutputChoice::Auto,
            fbdev_path: PathBuf::from("/dev/definitely-no-fb0"),
            ..headless_options("auto")
        };
        let socket = options.socket.clone();
        let server = Server::bind(options).expect("auto должен откатиться в headless");
        assert_eq!(server.backend.kind(), BackendKind::Headless);
        assert_eq!(server.compositor.screen().w, 640);
        assert!(server.input.is_none());
        drop(server);
        assert!(!socket.exists(), "сокет должен удаляться при Drop");
    }

    #[test]
    fn explicit_fbdev_without_device_is_an_error() {
        let options = ServerOptions {
            backend: OutputChoice::Fbdev,
            fbdev_path: PathBuf::from("/dev/definitely-no-fb0"),
            ..headless_options("nofb")
        };
        let socket = options.socket.clone();
        match Server::bind(options) {
            Ok(_) => panic!("явный fbdev без устройства должен падать"),
            Err(err) => assert!(matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            )),
        }
        assert!(!socket.exists(), "неудачный bind не должен оставлять сокет");
    }

    #[test]
    fn demo_window_is_committed_without_a_client() {
        let options = ServerOptions {
            demo_window: true,
            ..headless_options("demo-window")
        };
        let mut server = Server::bind(options).unwrap();
        server.ensure_demo_window();
        assert_eq!(server.compositor.windows().len(), 1);
        let window = &server.compositor.windows()[0];
        assert_eq!(window.id, 1);
        assert!(window.surface.is_some());
        assert_eq!(window.content_size.w, 640);
        let _ = server.compositor.render_full();
        assert!(server.compositor.frame().get(100, 100) != 0);
    }

    /// Без `--demo-window` сервер не рисует встроенное demo-окно: обычный
    /// образ получает первое окно только от реального клиента.
    #[test]
    fn demo_window_is_not_created_without_the_flag() {
        let mut server = Server::bind(headless_options("no-demo-window")).unwrap();
        server.ensure_demo_window();
        assert!(
            server.compositor.windows().is_empty(),
            "demo-окно появилось без --demo-window"
        );
    }

    /// Рецепт обычного образа (`desktop-autorun` -> `/mnt/etc/autorun`) обязан
    /// задавать РЕАЛЬНЫЕ цели и не содержать demo-флагов: иначе первый экран
    /// снова окажется фейковым терминалом `pagh-desktop-demo`. Тест держит
    /// файл-рецепт и параметры запуска в согласии.
    #[test]
    fn ordinary_autorun_recipe_uses_real_targets_only() {
        let recipe = include_str!("../desktop-autorun");
        // Kernel `run_autorun` исполняет непустые строки и пропускает `#`:
        // запрет demo-флагов проверяется по исполняемым строкам, а лимит — по
        // всему файлу, потому что ядро читает его целиком (512 байт).
        let active: String = recipe
            .lines()
            .filter(|line| {
                let line = line.trim();
                !line.is_empty() && !line.starts_with('#')
            })
            .collect::<Vec<_>>()
            .join("\n");
        for needle in [
            "--launch-target files=",
            "--launch-target terminal=",
            "--autostart files",
        ] {
            assert!(
                active.contains(needle),
                "в рецепте нет {needle:?}: {active}"
            );
        }
        for banned in ["--spawn-demo", "--demo-window", "--demo-binary"] {
            assert!(
                !active.contains(banned),
                "demo-флаг {banned:?} в обычном рецепте: {active}"
            );
        }
        assert!(
            recipe.len() <= 512,
            "рецепт {} байт: kernel autorun читает только 512",
            recipe.len()
        );
    }

    /// Диагностика обычного пути называет приложение и подсказывает
    /// `--launch-target`; demo-клиент не предлагается.
    #[test]
    fn unconfigured_launch_message_points_at_launch_target() {
        let message = unconfigured_launch_message("terminal");
        assert!(message.contains("terminal"), "{message}");
        assert!(message.contains("--launch-target terminal="), "{message}");
        assert!(
            !message.contains("spawn_demo выключен"),
            "старая внутренняя причина вместо подсказки: {message}"
        );
    }

    #[test]
    fn welcome_reports_the_actual_screen_size() {
        let server = Server::bind(headless_options("welcome")).unwrap();
        let welcome = server.welcome_for(7);
        assert_eq!(welcome.screen.w, 640);
        assert_eq!(welcome.screen.h, 480);
    }

    #[test]
    fn headless_options_keep_input_disabled() {
        let server = Server::bind(headless_options("noinput")).unwrap();
        assert!(server.input.is_none());
        assert_eq!(server.backend.kind(), BackendKind::Headless);
    }
}
