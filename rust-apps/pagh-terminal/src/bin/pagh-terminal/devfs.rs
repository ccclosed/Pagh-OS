//! Реальные устройства режима kernel-session: `/dev/vtfb1` + `/dev/vtinput1`.
//!
//! Весь `unsafe` крейта сосредоточен здесь: `lib.rs` объявляет
//! `#![forbid(unsafe_code)]`, а `mmap`/`ioctl`/`munmap` безопасными не бывают.
//! Слой намеренно тонкий — он умеет ровно три вещи: спросить геометрию через
//! ioctl `0x4700`, отобразить пиксели `mmap(PROT_READ, MAP_SHARED)` и, если
//! поверхность не mmap'ится, откатиться на `read()`-снимки
//! (`kernelsession::ReadFrameSource`, он же покрыт host-тестами).
//!
//! Вся логика (валидация заголовка, `generation`-гейт, блит, сообщения об
//! ошибках) живёт в lib и проверяется без устройств: на хосте `/dev/vtfb1` нет.

use std::ffi::{c_int, c_ulong, c_void};
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;

use pagh_terminal::kernelsession::{
    FrameSource, ReadFrameSource, VtFbError, VtFbInfo, INFO_LEN, VTFB_IOCTL_GET_INFO,
};

/// `ioctl(fd, VTINPUT_SET_FOCUS, on)` — сообщить ядру, что окно получило (или
/// потеряло) клавиатуру. Пока фокус у окна, физические клавиши не кладутся в
/// boot-кольцо: клиент возвращает их через `/dev/vtinput{N}`.
pub const VTINPUT_IOCTL_SET_FOCUS: c_ulong = 0x4710;

/// `PROT_READ`.
const PROT_READ: c_int = 0x1;
/// `MAP_SHARED` — единственный честный вариант для поверхности ядра: копия не
/// нужна, ядро владеет записью.
const MAP_SHARED: c_int = 0x1;
/// `TIOCSWINSZ` — сообщить ядру размер терминала в клетках (Linux UAPI).
const TIOCSWINSZ: c_ulong = 0x5414;

/// `struct winsize` UAPI: четыре `u16` (`ws_row`, `ws_col`, `ws_xpixel`,
/// `ws_ypixel`). `#[repr(C)]`, как в Linux.
#[repr(C)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

/// Сообщить ядру размер терминала окна: окно resize → `winsize` → `SIGWINCH`.
///
/// `file` — `/dev/vtinput1` (`O_WRONLY`); ioctl на нём адресует терминал
/// сессии 1, а не stdin вызывающего. `ENOTTY` означает ядро без per-session
/// winsize (старое) — вызывающий логирует и продолжает, это не фатально.
pub fn set_winsize(file: &File, cols: u16, rows: u16) -> io::Result<()> {
    let ws = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `file` — живой fd; ioctl получает указатель на локальную
    // `#[repr(C)]`-структуру ровно того размера, который читает ядро (8 байт).
    let rc = unsafe {
        ioctl(
            file.as_raw_fd(),
            TIOCSWINSZ,
            &ws as *const Winsize as *mut c_void,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, arg: *mut c_void) -> c_int;
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
}

/// Передать ядру фокус клавиатуры окна (`0x4710`).
///
/// Старое ядро без этого ioctl отвечает `ENOTTY` — вызывающий решает, сообщить
/// ли об этом (ввод при этом продолжает работать, просто boot-shell тоже видит
/// клавиши).
pub fn set_focus(file: &File, focused: bool) -> io::Result<()> {
    // SAFETY: ioctl-запрос определён нашим ядром для fd устройства ввода;
    // аргумент — скаляр (0/1), не указатель.
    let rc = unsafe {
        ioctl(
            file.as_raw_fd(),
            VTINPUT_IOCTL_SET_FOCUS,
            focused as usize as *mut c_void,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Открытая поверхность плюс одна строка о том, **как** получены пиксели.
///
/// Строку пишет вызывающий (bin), а не этот модуль: приложение может быть
/// запущено композитором в фоновой группе консоли, где TOSTOP останавливает
/// первого же писателя в stderr, поэтому место записи — политика клиента
/// (`--log-file`), а не устройство.
pub struct OpenedSurface {
    pub source: Box<dyn FrameSource>,
    pub note: String,
}

/// Почему устройство не открылось.
pub enum OpenError {
    /// Устройства нет в системе (`ENOENT`): ядро без `/dev/vtfb1`.
    Missing,
    /// Сессию уже держит другое окно (`EBUSY`): поверхность, ввод и
    /// scrollback не делятся. Клиент пробует следующий слот.
    Busy,
    /// Устройство есть, но работать с ним нельзя — честный текст для окна.
    Failed(String),
}

/// Открыть пиксельную поверхность сессии: `mmap` основной путь, `read()` fallback.
///
/// `mmap`-отказ (не mmap-устройство, `ENOSYS`/`ENOTTY`/`EINVAL`) не является
/// фатальным: контракт разрешает `read()`-снимки, поэтому клиент деградирует на
/// них и говорит об этом в stderr.  Отсутствие самого устройства — не деградация,
/// а [`OpenError::Missing`], по которому вызывающий откатывается на pipe-сессию.
pub fn open_frame_source(path: &str) -> Result<OpenedSurface, OpenError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(OpenError::Missing),
        // EBUSY = 16: сессия занята другим окном (несколько окон — независимые
        // сессии, см. `docs/terminal-session.md`).
        Err(error) if error.raw_os_error() == Some(16) => return Err(OpenError::Busy),
        Err(error) => {
            return Err(OpenError::Failed(format!(
                "{path}: {error} (the kernel shell-session surface is not readable by this process)"
            )))
        }
    };
    match MmapSurface::new(file, path) {
        Ok((surface, note)) => Ok(OpenedSurface {
            source: Box::new(surface),
            note,
        }),
        Err(reason) => match ReadFrameSource::open(path) {
            Ok(source) => Ok(OpenedSurface {
                source: Box::new(source),
                note: format!(
                    "mmap of {path} unavailable ({reason}); falling back to read() snapshots"
                ),
            }),
            Err(error) => Err(OpenError::Failed(format!("{path}: {}", error.message()))),
        },
    }
}

/// Пиксельная поверхность сессии через `mmap`.
struct MmapSurface {
    /// Держим fd открытым: закрытие `fd` при живом отображении — законно, но
    /// повторный ioctl `generation` делается по этому же fd.
    /// `None` — fd закрыт (между `ENODEV` и respawn).
    file: Option<File>,
    /// Путь нужен для `respawn`: после `ENODEV` сессию поднимает новый `open`.
    path: String,
    base: *mut u8,
    len: usize,
    info: VtFbInfo,
}

impl MmapSurface {
    fn new(file: File, path: &str) -> Result<(MmapSurface, String), String> {
        let info = query_info(&file)
            .map_err(|error| format!("ioctl 0x{VTFB_IOCTL_GET_INFO:04x}: {error}"))?;
        let len = info.pixels_len();
        if len == 0 {
            return Err("the kernel reported a zero-byte framebuffer".to_string());
        }
        // SAFETY: PROT_READ+MAP_SHARED на fd, который живёт в `file`; offset 0 —
        // контракт поверхности. При провале ядро возвращает MAP_FAILED, который
        // проверяется до любого чтения.
        let base = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if base as isize == -1 {
            return Err(format!(
                "mmap({len} bytes) failed: {}",
                io::Error::last_os_error()
            ));
        }
        // Одна строка на открытие: по ней видно, что поверхность получена
        // именно общим отображением (а не read()-снимками), и какая геометрия
        // объявлена ядром. Клиент пишет её в свой лог, поэтому live-прогон
        // доказывает путь получения пикселей без отладчика.
        let note = format!(
            "kernel surface {path}: mmap {len} bytes PROT_READ|MAP_SHARED \
             (ioctl 0x{VTFB_IOCTL_GET_INFO:04x} {}x{} stride {} bpp {})",
            info.width, info.height, info.stride, info.bpp
        );
        Ok((
            MmapSurface {
                file: Some(file),
                path: path.to_string(),
                base: base as *mut u8,
                len,
                info,
            },
            note,
        ))
    }
}

impl FrameSource for MmapSurface {
    fn poll(&mut self) -> Result<VtFbInfo, VtFbError> {
        let Some(file) = self.file.as_ref() else {
            return Err(VtFbError::Ended);
        };
        let info = query_info(file)?;
        if info.pixels_len() > self.len {
            // Разрешение kernel-VT сменилось на живой сессии — это контрактный
            // разрыв, а не повод читать за отображением.
            return Err(VtFbError::BadInfo(
                "framebuffer grew past the mapping (relaunch the window)",
            ));
        }
        self.info = info;
        Ok(info)
    }

    fn close(&mut self) -> Result<(), VtFbError> {
        // Снять отображение и закрыть fd. Вызывается ДО любого нового `open`:
        // `close` пережившего сессию fd после `open` новой уменьшил бы счётчик
        // открытых устройств до нуля, и ядро попросило бы новую сессию выйти.
        if !self.base.is_null() && self.len > 0 {
            // SAFETY: отображение создано в `new`/`respawn` и ещё не снималось.
            unsafe {
                munmap(self.base as *mut c_void, self.len);
            }
        }
        self.base = std::ptr::null_mut();
        self.len = 0;
        self.file = None;
        Ok(())
    }

    fn respawn(&mut self) -> Result<(), VtFbError> {
        // `close` уже закрыл старый fd (общий порядок — см. `FrameSource::close`),
        // поэтому здесь только новый open + mmap: open и спавнит сессию.
        let file = File::open(&self.path)?;
        let info = query_info(&file)?;
        let len = info.pixels_len();
        if len == 0 {
            return Err(VtFbError::BadInfo(
                "the kernel reported a zero-byte framebuffer",
            ));
        }
        // SAFETY: PROT_READ+MAP_SHARED на fd, который живёт в `file`;
        // MAP_FAILED проверяется до любого чтения (как в `new`).
        let base = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if base as isize == -1 {
            return Err(VtFbError::Io(io::Error::last_os_error()));
        }
        self.file = Some(file);
        self.base = base as *mut u8;
        self.len = len;
        self.info = info;
        Ok(())
    }

    fn with_pixels(&mut self, f: &mut dyn FnMut(&[u8])) -> Result<(), VtFbError> {
        let len = self.info.pixels_len().min(self.len);
        // SAFETY: `base..base+len` — отображение, созданное в `new` и живое до
        // `Drop`; ядро пишет туда, мы только читаем (PROT_READ).
        let pixels = unsafe { std::slice::from_raw_parts(self.base, len) };
        f(pixels);
        Ok(())
    }
}

impl Drop for MmapSurface {
    fn drop(&mut self) {
        if self.base.is_null() || self.len == 0 {
            return;
        }
        // SAFETY: отображение создано `mmap` в `new`/`respawn` и ещё не снималось.
        unsafe {
            munmap(self.base as *mut c_void, self.len);
        }
    }
}

/// ioctl `VTFBIOGET_INFO` → [`VtFbInfo`].
fn query_info(file: &File) -> Result<VtFbInfo, VtFbError> {
    let mut header = [0u8; INFO_LEN];
    // SAFETY: буфер ровно 24 байта — столько ядро пишет по контракту; ioctl
    // получает живой fd из `file`.
    let rc = unsafe {
        ioctl(
            file.as_raw_fd(),
            VTFB_IOCTL_GET_INFO as c_ulong,
            header.as_mut_ptr() as *mut c_void,
        )
    };
    if rc < 0 {
        return Err(VtFbError::from(io::Error::last_os_error()));
    }
    VtFbInfo::parse(&header)
}
