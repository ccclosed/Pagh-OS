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

/// `PROT_READ`.
const PROT_READ: c_int = 0x1;
/// `MAP_SHARED` — единственный честный вариант для поверхности ядра: копия не
/// нужна, ядро владеет записью.
const MAP_SHARED: c_int = 0x1;

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

/// Почему устройство не открылось.
pub enum OpenError {
    /// Устройства нет в системе (`ENOENT`): ядро без `/dev/vtfb1`.
    Missing,
    /// Устройство есть, но работать с ним нельзя — честный текст для окна.
    Failed(String),
}

/// Открыть пиксельную поверхность сессии: `mmap` основной путь, `read()` fallback.
///
/// `mmap`-отказ (не mmap-устройство, `ENOSYS`/`ENOTTY`/`EINVAL`) не является
/// фатальным: контракт разрешает `read()`-снимки, поэтому клиент деградирует на
/// них и говорит об этом в stderr.  Отсутствие самого устройства — не деградация,
/// а [`OpenError::Missing`], по которому вызывающий откатывается на pipe-сессию.
pub fn open_frame_source(path: &str) -> Result<Box<dyn FrameSource>, OpenError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(OpenError::Missing),
        Err(error) => {
            return Err(OpenError::Failed(format!(
                "{path}: {error} (the kernel shell-session surface is not readable by this process)"
            )))
        }
    };
    match MmapSurface::new(file) {
        Ok(surface) => Ok(Box::new(surface)),
        Err(reason) => {
            eprintln!(
                "pagh-terminal: mmap of {path} unavailable ({reason}); falling back to read() snapshots"
            );
            match ReadFrameSource::open(path) {
                Ok(source) => Ok(Box::new(source)),
                Err(error) => Err(OpenError::Failed(format!("{path}: {}", error.message()))),
            }
        }
    }
}

/// Пиксельная поверхность сессии через `mmap`.
struct MmapSurface {
    /// Держим fd открытым: закрытие `fd` при живом отображении — законно, но
    /// повторный ioctl `generation` делается по этому же fd.
    file: File,
    base: *mut u8,
    len: usize,
    info: VtFbInfo,
}

impl MmapSurface {
    fn new(file: File) -> Result<MmapSurface, String> {
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
        Ok(MmapSurface {
            file,
            base: base as *mut u8,
            len,
            info,
        })
    }
}

impl FrameSource for MmapSurface {
    fn poll(&mut self) -> Result<VtFbInfo, VtFbError> {
        let info = query_info(&self.file)?;
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
        // SAFETY: отображение создано `mmap` в `new` и ещё не снималось.
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
