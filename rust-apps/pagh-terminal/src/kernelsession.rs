//! Kernel shell-session surface: `/dev/vtfb1` (pixels) + `/dev/vtinput1` (keys).
//!
//! Это **основной** режим окна терминала в desktop B: окно не запускает
//! собственный userspace-shell и не имитирует команды.  Ядро владеет второй
//! shell-сессией (`pf`, `ps`, `ppkg`, `lxrun`, `disk`, `nano`, history…), рисует
//! её в собственную пиксельную поверхность, а клиент только блитит пиксели в
//! окно и передаёт ввод обратно. Полный набор команд, эхо, редактирование строки
//! и scrollback делает ядро — здесь нет ни одной команды и ни одного байта
//! «притворства».
//!
//! Контракт устройств (зафиксирован координатором; kernel-side реализация идёт
//! параллельно, live-интеграцию делает координатор):
//!
//! ```text
//! /dev/vtfb1 (read-only)
//!   ioctl 0x4700 (VTFBIOGET_INFO) -> 24 байта LE:
//!       u32 width, u32 height, u32 stride, u32 bpp(=32), u64 generation
//!   mmap(NULL, stride*height, PROT_READ, MAP_SHARED, fd, 0) -> пиксели 32bpp XRGB
//!   read() fallback: те же 24 байта заголовка + пиксели
//!   generation растёт на каждый flush сессии; ENODEV после `exit` в сессии
//!   (первый open любого из двух устройств спавнит сессию ядра заново)
//!
//! /dev/vtinput1 (write-only)
//!   VT-байты клавиатуры — тот же формат, что у `/dev/vtinput`
//!   (`keys::encode_key`, см. `vtconsole::send_input`)
//! ```
//!
//! Почему lib, а не bin: `lib.rs` объявляет `#![forbid(unsafe_code)]`, а mmap и
//! ioctl — unsafe. Поэтому здесь живёт вся **безопасная** логика (разбор
//! заголовка, валидация геометрии, решение о перерисовке по `generation`, блит,
//! разбор ошибок, кодирование клавиш), а неустранимо-небезопасный слой
//! (`ioctl`/`mmap`/`munmap`) — в `src/bin/pagh-terminal/devfs.rs`.  Благодаря
//! этому режим целиком прогоняется host-тестами на mock-устройстве: на хосте
//! `/dev/vtfb1` нет и быть не может.
//!
//! Честные границы v1 (подробнее — `TERMINAL.md`):
//!
//! * одно окно = одна сессия ядра; второй клиент получает ту же поверхность;
//! * нет PTY и полного `struct termios`: управляющий терминал — kernel-консоль
//!   сессии. `Ctrl+C`/`Ctrl+Z` клиент шлёт байтами в `/dev/vtinput1`, а `ISIG`
//!   и signals применяет ядро к foreground-группе сессии; `winsize` клиент
//!   сообщает через `TIOCSWINSZ` (окно resize → [`winsize_cells`] → SIGWINCH),
//!   но пиксельная поверхность остаётся размером с экран и масштабируется;
//! * клиент не наблюдает DEC-режимы (пиксели не говорят, хочет ли сессия
//!   application-cursor keys), поэтому стрелки кодируются в normal-режиме
//!   (`KeyModes::default()`), как kernel-shell и ожидает;
//! * scrollback и selection остаются в ядре: протяжка мышью в окно не
//!   транслируется, а колесо при 1:1-вьюпорте прокручивает сам вьюпорт (см.
//!   [`View`]); kernel-scrollback остаётся на PageUp/PageDown в shell'е;
//! * по умолчанию показ **1:1**: окно меньше поверхности — не дробный
//!   downscale, а полноразмерный вьюпорт с прокруткой (колесо,
//!   `Ctrl+Shift`+стрелки/PageUp/PageDown/Home/End).  Вьюпорт **следует за
//!   живым низом** ([`View`]): новый вывод и приглашение видны без прокрутки;
//!   прокрутка вверх останавливает следование, `Ctrl+Shift+End` или возврат
//!   колесом к низу — возобновляет.  Целочисленный зум-обзор (2×, 3×, …) —
//!   необязательный режим (`--scale 2`, `Ctrl+Shift+Z`); дробного масштаба нет
//!   нигде — он ставил штрихи шрифта между пикселями окна и делал текст
//!   нечитаемым.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::font::{CELL_H, CELL_W};
use crate::keys::{self, KeyModes};
use crate::protocol::mods;
use crate::render::{render_screen_into, Canvas, RenderOptions, Theme, VIEWPORT_PADDING};
use crate::Terminal;

/// `VTFBIOGET_INFO` — ioctl, отдающий [`VtFbInfo`] (24 байта little-endian).
pub const VTFB_IOCTL_GET_INFO: u32 = 0x4700;
/// Размер структуры [`VtFbInfo`] на проводе.
pub const INFO_LEN: usize = 24;
/// Единственный поддерживаемый формат пикселя: 32bpp XRGB (младший байт — B).
pub const BPP: u32 = 32;
/// Геометрический предел: защита от мусорного ioctl-ответа, до аллокаций.
pub const MAX_DIM: u32 = 8192;
/// Предел одного кадра в байтах (32 МиБ ≈ 8192×1024×4) — до mmap/чтения.
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// Значение errno `ENODEV`: сессия ядра завершилась (`exit` в shell'е).
const ENODEV: i32 = 19;

/// Устройства нет в системе (старое ядро без `/dev/vtfb1`).
pub fn is_missing_device(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

/// `ENODEV`: сессия ядра завершилась; повторный `open` спавнит новую.
pub fn is_session_end(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ENODEV)
}

/// Заголовок поверхности сессии ядра (ioctl-ответ или префикс `read()`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VtFbInfo {
    pub width: u32,
    pub height: u32,
    /// Байт на строку; ядро может выравнивать строки, поэтому `>= width*4`.
    pub stride: u32,
    pub bpp: u32,
    /// Растёт на каждый flush сессии — единственный сигнал «кадр изменился».
    pub generation: u64,
}

impl VtFbInfo {
    /// Разобрать 24 байта little-endian и провалидировать геометрию.
    ///
    /// Валидация стоит **до** любого `mmap`/чтения пикселей: мусорный ответ
    /// ioctl не должен превращаться в гигантскую аллокацию или выход за
    /// отображение.
    pub fn parse(bytes: &[u8]) -> Result<VtFbInfo, VtFbError> {
        if bytes.len() < INFO_LEN {
            return Err(VtFbError::ShortRead {
                need: INFO_LEN,
                have: bytes.len(),
            });
        }
        let info = VtFbInfo {
            width: read_u32(bytes, 0),
            height: read_u32(bytes, 4),
            stride: read_u32(bytes, 8),
            bpp: read_u32(bytes, 12),
            generation: read_u64(bytes, 16),
        };
        info.validate()?;
        Ok(info)
    }

    /// Обратное к [`VtFbInfo::parse`] — для mock-устройств и тестовых файлов.
    pub fn encode(self) -> [u8; INFO_LEN] {
        let mut bytes = [0u8; INFO_LEN];
        bytes[0..4].copy_from_slice(&self.width.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.height.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.stride.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.bpp.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.generation.to_le_bytes());
        bytes
    }

    /// Проверить контракт: 32bpp, непустые размеры, `stride >= width*4`, пределы.
    pub fn validate(&self) -> Result<(), VtFbError> {
        if self.bpp != BPP {
            return Err(VtFbError::BadInfo("bpp must be 32"));
        }
        if self.width == 0 || self.height == 0 {
            return Err(VtFbError::BadInfo("width/height must be non-zero"));
        }
        if self.width > MAX_DIM || self.height > MAX_DIM {
            return Err(VtFbError::BadInfo("framebuffer exceeds 8192x8192"));
        }
        let min_stride = (self.width as u64) * 4;
        if (self.stride as u64) < min_stride {
            return Err(VtFbError::BadInfo("stride is smaller than width * 4"));
        }
        if (self.stride as u64) > (MAX_DIM as u64) * 4 {
            return Err(VtFbError::BadInfo("stride exceeds 8192 * 4"));
        }
        if self.pixels_len() > MAX_FRAME_BYTES {
            return Err(VtFbError::TooLarge);
        }
        Ok(())
    }

    /// Байт в одном кадре: `stride * height`.
    pub fn pixels_len(&self) -> usize {
        (self.stride as usize).saturating_mul(self.height as usize)
    }
}

/// Ошибка поверхности/сессии. `Ended` — не сбой, а событие: shell завершился.
#[derive(Debug)]
pub enum VtFbError {
    Io(io::Error),
    /// `ENODEV`: сессия ядра завершилась; следующий open — новая сессия.
    Ended,
    BadInfo(&'static str),
    ShortRead {
        need: usize,
        have: usize,
    },
    TooLarge,
}

impl VtFbError {
    pub fn is_ended(&self) -> bool {
        matches!(self, VtFbError::Ended)
    }

    /// Честный текст для окна и stderr: что именно случилось и что делать.
    pub fn message(&self) -> String {
        match self {
            VtFbError::Io(error) => format!("device I/O error: {error}"),
            VtFbError::Ended => {
                "the kernel shell session ended (ENODEV). The next open starts a new one — \
                 press Enter in the window to restart the session there."
                    .to_string()
            }
            VtFbError::BadInfo(what) => format!("kernel framebuffer contract violation: {what}"),
            VtFbError::ShortRead { need, have } => {
                format!("short frame from the kernel: need {need} bytes, got {have}")
            }
            VtFbError::TooLarge => "kernel framebuffer exceeds the supported size".to_string(),
        }
    }
}

impl std::fmt::Display for VtFbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for VtFbError {}

impl From<io::Error> for VtFbError {
    fn from(error: io::Error) -> VtFbError {
        if is_session_end(&error) {
            VtFbError::Ended
        } else {
            VtFbError::Io(error)
        }
    }
}

/// Источник кадров сессии ядра: `mmap`-поверхность или `read()`-fallback.
///
/// Трейт сознательно двухшаговый, как и контракт: `poll()` дёшев (один ioctl
/// или 24 байта `pread`) и отвечает «изменился ли кадр», а пиксели читаются
/// только когда generation действительно вырос.
pub trait FrameSource {
    /// Текущая геометрия и generation.
    fn poll(&mut self) -> Result<VtFbInfo, VtFbError>;
    /// Отдать кадр (ровно `stride * height` байт) замыканию.
    ///
    /// Замыкание, а не возвращаемый `&[u8]`: у mmap-источника нет владеющего
    /// буфера, а у read-fallback он есть, и оба обязаны выглядеть одинаково для
    /// безопасного кода lib-крейта (`#![forbid(unsafe_code)]`).
    fn with_pixels(&mut self, f: &mut dyn FnMut(&[u8])) -> Result<(), VtFbError>;

    /// Переоткрыть поверхность после `ENODEV` — ядро спавнит новую сессию на
    /// первом `open`. Устройство переоткрывается, а окно клиента остаётся тем
    /// же: перезапуск сессии не должен требовать нового окна (иначе композитор
    /// получает `duplicate window` на тот же id).
    /// Закрыть устройство перед respawn — **вместе** со всеми остальными.
    ///
    /// Несущий порядок: ядро сбрасывает счётчик открытых fd сессии при её
    /// выходе, а `close` переживших сессию fd считает заново. Если закрыть один
    /// старый fd уже после `open` нового, счётчик дойдёт до нуля и ядро
    /// попросит только что поднятую сессию выйти. Поэтому клиент сначала
    /// закрывает ВСЕ старые устройства, и только потом открывает новые.
    fn close(&mut self) -> Result<(), VtFbError> {
        Ok(())
    }

    /// Переоткрыть поверхность после `ENODEV` — ядро спавнит новую сессию на
    /// первом `open`. Устройство переоткрывается, а окно клиента остаётся тем
    /// же: перезапуск сессии не должен требовать нового окна (иначе композитор
    /// получает `duplicate window` на тот же id).
    fn respawn(&mut self) -> Result<(), VtFbError> {
        Err(VtFbError::Ended)
    }
}

/// Приёмник VT-байтов ввода (в бою — `/dev/vtinput1`).
pub trait InputSink {
    fn send(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Закрыть устройство (перед общим close-then-open respawn).
    fn close(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Переоткрыть устройство после `ENODEV` (новая сессия ядра).
    fn respawn(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<T: FrameSource + ?Sized> FrameSource for Box<T> {
    fn poll(&mut self) -> Result<VtFbInfo, VtFbError> {
        (**self).poll()
    }

    fn with_pixels(&mut self, f: &mut dyn FnMut(&[u8])) -> Result<(), VtFbError> {
        (**self).with_pixels(f)
    }

    // Проброс обязателен: без него `Box<dyn FrameSource>` (как в bin) берёт
    // ДЕФОЛТНЫЕ `close`/`respawn` из трейта, и respawn молча отвечает `Ended`,
    // не доходя до настоящего устройства — «окно перезапускается и сразу
    // умирает» без единой ошибки от ядра.
    fn close(&mut self) -> Result<(), VtFbError> {
        (**self).close()
    }

    fn respawn(&mut self) -> Result<(), VtFbError> {
        (**self).respawn()
    }
}

/// `read()`-fallback поверхности: `pread` заголовка + пикселей.
///
/// Реализация в lib, а не в bin, намеренно: она полностью безопасна (std
/// `FileExt::read_at`), поэтому host-тест гоняет её на обычном файле — без
/// `/dev/vtfb1` и без ядра.  Именно этот путь используют ядра, где mmap
/// поверхности не поддержан, и он же документирован в `TERMINAL.md`.
pub struct ReadFrameSource {
    file: File,
    path: PathBuf,
    info: VtFbInfo,
    buffer: Vec<u8>,
}

impl ReadFrameSource {
    pub fn open(path: impl AsRef<Path>) -> Result<ReadFrameSource, VtFbError> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let mut header = [0u8; INFO_LEN];
        read_exact_at(&file, &mut header, 0)?;
        let info = VtFbInfo::parse(&header)?;
        Ok(ReadFrameSource {
            file,
            path,
            info,
            buffer: vec![0; info.pixels_len()],
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl FrameSource for ReadFrameSource {
    fn poll(&mut self) -> Result<VtFbInfo, VtFbError> {
        let mut header = [0u8; INFO_LEN];
        read_exact_at(&self.file, &mut header, 0)?;
        self.info = VtFbInfo::parse(&header)?;
        Ok(self.info)
    }

    fn respawn(&mut self) -> Result<(), VtFbError> {
        // Свежий open — это и есть respawn: ядро поднимает сессию на первом
        // open устройства, а старый fd остался от завершившейся сессии.
        let file = File::open(&self.path)?;
        let mut header = [0u8; INFO_LEN];
        read_exact_at(&file, &mut header, 0)?;
        self.file = file;
        self.info = VtFbInfo::parse(&header)?;
        Ok(())
    }

    fn with_pixels(&mut self, f: &mut dyn FnMut(&[u8])) -> Result<(), VtFbError> {
        // Один связный снимок: заголовок и пиксели читаются из offset 0 в
        // одном проходе, поэтому geometry/generation принадлежат тем же
        // пикселям, которые уйдут в кадр.
        let mut header = [0u8; INFO_LEN];
        read_exact_at(&self.file, &mut header, 0)?;
        let info = VtFbInfo::parse(&header)?;
        let len = info.pixels_len();
        if len > MAX_FRAME_BYTES {
            return Err(VtFbError::TooLarge);
        }
        self.buffer.resize(len, 0);
        read_exact_at(&self.file, &mut self.buffer, INFO_LEN as u64)?;
        self.info = info;
        f(&self.buffer);
        Ok(())
    }
}

/// `InputSink` поверх обычного файла (`/dev/vtinput1`).
pub struct FileInput {
    /// `None` — устройство закрыто (например, между `ENODEV` и respawn).
    file: Option<File>,
    path: PathBuf,
}

impl FileInput {
    pub fn open(path: impl AsRef<Path>) -> io::Result<FileInput> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().write(true).open(&path)?;
        Ok(FileInput {
            file: Some(file),
            path,
        })
    }

    /// Дескриптор `/dev/vtinput1`, если устройство открыто: resize-путь шлёт по
    /// нему `TIOCSWINSZ` (сам `ioctl` — unsafe, поэтому он в `devfs` рядом с
    /// `mmap`). `None` — устройство закрыто между `ENODEV` и respawn (#174),
    /// поэтому resize в этот момент пропускается, а не паникует.
    pub fn file(&self) -> Option<&File> {
        self.file.as_ref()
    }
}

/// Размер терминала в клетках ядра (8×18 px): `(cols, rows)`.
///
/// Окно может быть меньше/больше поверхности: `cols/rows` — это сколько клеток
/// помещается в клиентскую область окна, то есть достоверный `winsize` для
/// приложения внутри сессии (`TIOCGWINSZ`). Деление целочисленное, минимум 1:
/// нулевой размер заставил бы TUI делить на ноль. Значения клампятся в `u16`
/// (`struct winsize` — четыре `u16`), пиксели не клампятся.
pub fn winsize_cells(width: u32, height: u32) -> (u16, u16) {
    let cols = (width as usize / CELL_W).max(1).min(u16::MAX as usize) as u16;
    let rows = (height as usize / CELL_H).max(1).min(u16::MAX as usize) as u16;
    (cols, rows)
}

impl InputSink for FileInput {
    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        // Без flush: это символьное устройство, `write` уже отдал байты ядру.
        match self.file.as_mut() {
            Some(file) => file.write_all(bytes),
            None => Err(io::Error::from_raw_os_error(ENODEV)),
        }
    }

    fn close(&mut self) -> io::Result<()> {
        self.file = None;
        Ok(())
    }

    fn respawn(&mut self) -> io::Result<()> {
        // К этому моменту `close` уже закрыл старый fd (ядро сбрасывает счётчик
        // при выходе сессии), поэтому новый `open` — единственный: он и спавнит
        // сессию, и не даёт «позднему» close счётчику упасть до нуля.
        self.file = Some(OpenOptions::new().write(true).open(&self.path)?);
        Ok(())
    }
}

/// Наибольший целый делитель кадра, который ещё помещается в окно.
///
/// `1` — поверхность помещается целиком, блит пиксель в пиксель.  Иначе —
/// минимальный `n >= 2`, при котором `src / n` влезает в окно, то есть
/// `max(ceil(src_w/dst_w), ceil(src_h/dst_h))`.  Дробного шага здесь нет
/// намеренно: именно он ставил штрихи шрифта ядра между пикселями окна.
fn integer_downscale(src_w: usize, src_h: usize, dst_w: usize, dst_h: usize) -> usize {
    if dst_w >= src_w && dst_h >= src_h {
        return 1;
    }
    let by_w = src_w.div_ceil(dst_w);
    let by_h = src_h.div_ceil(dst_h);
    by_w.max(by_h).max(1)
}

/// Раскладка кадра сессии внутри окна: `dst`-прямоугольник и его источник.
///
/// `src_*` — вырез поверхности, который попал в `(x, y, width, height)`.
/// Для 1:1-вьюпорта это окно-размерный вырез с `src_width == width`
/// (сдвиг `src_x/src_y` — прокрутка), для целочисленного зума — вся
/// поверхность (`src_width/src_height == info.width/height`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FitPlacement {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    /// Левый верхний пиксель поверхности, показанный в `(x, y)`.
    pub src_x: usize,
    pub src_y: usize,
    /// Размер выреза поверхности, отображённого в `width × height`.
    pub src_width: usize,
    pub src_height: usize,
}

/// Как окно показывает поверхность сессии.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScaleMode {
    /// **По умолчанию:** 1:1 пиксель в пиксель.  Если окно меньше поверхности,
    /// виден вьюпорт размером с окно, который прокручивается (`View::pan_by`),
    /// а не сжимается дробно: текст остаётся полноразмерным и читаемым.
    #[default]
    OneToOne,
    /// Необязательный обзор: вся поверхность целиком, но только в целое
    /// число раз меньше (2×, 3×, …).  Дробного шага нет и здесь.
    Integer,
}

/// Раскладка всего кадра при целочисленном зуме ([`ScaleMode::Integer`]).
///
/// Поверхность влезает целиком: `src = (0, 0, src_w, src_h)`, окно получает
/// уменьшенный в целое число раз кадр по центру.  Дробного масштаба нет:
/// именно он ставил штрихи шрифта 8×18 между пикселями окна.
pub fn fit_placement(
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Option<FitPlacement> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return None;
    }
    let scale = integer_downscale(src_w, src_h, dst_w, dst_h);
    // `src / scale <= dst` по построению `scale`; `min(dst)` — страховка от
    // целочисленного округления на вырожденных размерах, а `max(1)` не даёт
    // получить пустой прямоугольник.
    let width = (src_w / scale).max(1).min(dst_w);
    let height = (src_h / scale).max(1).min(dst_h);
    Some(FitPlacement {
        x: (dst_w - width) / 2,
        y: (dst_h - height) / 2,
        width,
        height,
        src_x: 0,
        src_y: 0,
        src_width: src_w,
        src_height: src_h,
    })
}

/// Максимальная прокрутка 1:1-вьюпорта по каждой оси (0 — ось помещается).
fn max_scroll(src_w: usize, src_h: usize, dst_w: usize, dst_h: usize) -> (usize, usize) {
    (src_w.saturating_sub(dst_w), src_h.saturating_sub(dst_h))
}

/// Состояние показа поверхности клиентом: режим, прокрутка 1:1-вьюпорта и
/// автоследование за нижним краем.
///
/// Живёт целиком в клиенте: ядро по-прежнему владеет поверхностью размером с
/// экран и ничего не знает про окно/прокрутку (ABI `/dev/vtfb1` не меняется).
///
/// **Автоследование (стандартная семантика терминала).**  Пока `follow` включён,
/// 1:1-вьюпорт стоит на нижнем крае поверхности: новые строки shell'а и
/// приглашение (`prompt`) всегда видны без ручной прокрутки.  Прокрутка вверх
/// (`wheel` вверх, `Ctrl+Shift+Up/PageUp/Home`) снимает `follow`, чтобы чтение
/// истории не «дёргало» обратно; возврат к нижнему краю (`wheel` вниз до упора,
/// `Ctrl+Shift+End`) снова включает следование.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    mode: ScaleMode,
    scroll_x: usize,
    scroll_y: usize,
    /// Следовать за нижним краем поверхности (сбрасывается прокруткой вверх).
    follow: bool,
}

impl Default for View {
    fn default() -> View {
        View {
            mode: ScaleMode::OneToOne,
            scroll_x: 0,
            scroll_y: 0,
            follow: true,
        }
    }
}

impl View {
    pub fn mode(&self) -> ScaleMode {
        self.mode
    }

    /// Включено ли автоследование за нижним краем.
    pub fn follows(&self) -> bool {
        self.follow
    }

    /// Последняя заданная прокрутка в пикселях поверхности.
    ///
    /// Пока `follow` включён, фактическая позиция — нижний край; для неё есть
    /// [`View::effective_scroll`] (и [`KernelFrame::scroll`]).
    pub fn scroll(&self) -> (usize, usize) {
        (self.scroll_x, self.scroll_y)
    }

    /// Фактическая прокрутка для данной геометрии: нижний край при `follow`.
    pub fn effective_scroll(
        &self,
        src_w: usize,
        src_h: usize,
        dst_w: usize,
        dst_h: usize,
    ) -> (usize, usize) {
        if self.mode == ScaleMode::Integer {
            return (0, 0);
        }
        let (max_x, max_y) = max_scroll(src_w, src_h, dst_w, dst_h);
        let x = self.scroll_x.min(max_x);
        let y = if self.follow {
            max_y
        } else {
            self.scroll_y.min(max_y)
        };
        (x, y)
    }

    /// Сменить режим; прокрутка сбрасывается, автоследование включается.
    ///
    /// Целочисленный зум показывает весь кадр; после возврата к 1:1 начинать
    /// с живого низа (приглашение видно) — предсказуемо.
    pub fn set_mode(&mut self, mode: ScaleMode) {
        if self.mode != mode {
            self.mode = mode;
            self.scroll_x = 0;
            self.scroll_y = 0;
            self.follow = true;
        }
    }

    /// Прижать прокрутку к новой геометрии (resize окна или поверхности).
    ///
    /// Resize: автоследование остаётся на нижнем крае; ручная прокрутка
    /// сохраняется, но клампится к новым границам.  Окно не меньше
    /// поверхности — смещение 0 (следовать не за чем).
    pub fn reclamp(&mut self, src_w: usize, src_h: usize, dst_w: usize, dst_h: usize) {
        if self.mode == ScaleMode::Integer {
            self.scroll_x = 0;
            self.scroll_y = 0;
            return;
        }
        let (max_x, max_y) = max_scroll(src_w, src_h, dst_w, dst_h);
        self.scroll_x = self.scroll_x.min(max_x);
        self.scroll_y = if self.follow {
            max_y
        } else {
            self.scroll_y.min(max_y)
        };
    }

    /// Раскладка кадра для текущего режима и геометрии.
    ///
    /// 1:1: вырез `min(surface, window)` по каждой оси, центрированный по той
    /// оси, где поверхность меньше окна; по умолчанию — нижний край
    /// ([`View`]).
    /// Integer: вся поверхность через [`fit_placement`] (целый делитель).
    pub fn placement(
        &self,
        src_w: usize,
        src_h: usize,
        dst_w: usize,
        dst_h: usize,
    ) -> Option<FitPlacement> {
        if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
            return None;
        }
        if self.mode == ScaleMode::Integer {
            return fit_placement(src_w, src_h, dst_w, dst_h);
        }
        let view_w = src_w.min(dst_w);
        let view_h = src_h.min(dst_h);
        let (src_x, src_y) = self.effective_scroll(src_w, src_h, dst_w, dst_h);
        Some(FitPlacement {
            x: (dst_w - view_w) / 2,
            y: (dst_h - view_h) / 2,
            width: view_w,
            height: view_h,
            src_x,
            src_y,
            src_width: view_w,
            src_height: view_h,
        })
    }

    /// Прокрутить 1:1-вьюпорт на `dx`/`dy` пикселей поверхности.
    ///
    /// Стартует с фактической позиции (при автоследовании — с нижнего края).
    /// Возвращает `true`, если видимая позиция действительно изменилась.
    /// Прокрутка вверх выключает автоследование; возврат к нижнему краю —
    /// включает.  В режиме [`ScaleMode::Integer`] прокрутки нет (весь кадр
    /// виден целиком) — всегда `false`.
    pub fn pan_by(
        &mut self,
        dx: i32,
        dy: i32,
        src_w: usize,
        src_h: usize,
        dst_w: usize,
        dst_h: usize,
    ) -> bool {
        if self.mode == ScaleMode::Integer || src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
            return false;
        }
        let (max_x, max_y) = max_scroll(src_w, src_h, dst_w, dst_h);
        let (from_x, from_y) = self.effective_scroll(src_w, src_h, dst_w, dst_h);
        let x = (from_x as i64 + dx as i64).clamp(0, max_x as i64) as usize;
        let y = (from_y as i64 + dy as i64).clamp(0, max_y as i64) as usize;
        self.scroll_x = x;
        self.scroll_y = y;
        // Автоследование возвращается только на самом нижнем крае.
        self.follow = y == max_y;
        (x, y) != (from_x, from_y)
    }

    /// Прокрутить вьюпорт в абсолютную позицию (Home/End), прижатую к краям.
    ///
    /// `End` (нижний край) снова включает автоследование, `Home` — выключает.
    pub fn pan_to(
        &mut self,
        x: usize,
        y: usize,
        src_w: usize,
        src_h: usize,
        dst_w: usize,
        dst_h: usize,
    ) -> bool {
        if self.mode == ScaleMode::Integer || src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
            return false;
        }
        let (max_x, max_y) = max_scroll(src_w, src_h, dst_w, dst_h);
        let (from_x, from_y) = self.effective_scroll(src_w, src_h, dst_w, dst_h);
        let x = x.min(max_x);
        let y = y.min(max_y);
        self.scroll_x = x;
        self.scroll_y = y;
        self.follow = y == max_y;
        (x, y) != (from_x, from_y)
    }
}

/// Сколько пикселей поверхности прокручивает один щелчок колеса.
///
/// Колесо в этом окне прокручивает **вьюпорт** (поверхность ядра больше
/// окна), а не kernel-scrollback: без этого увидеть правую/нижнюю часть
/// кадра было бы нечем.  Шаг — три клетки, как в обычном терминале.
pub const WHEEL_PAN_X: i32 = 3 * CELL_W as i32;
/// Вертикальный шаг колеса в пикселях поверхности.
pub const WHEEL_PAN_Y: i32 = 3 * CELL_H as i32;

/// Шаг прокрутки 1:1-вьюпорта, запрошенный клавиатурой.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanStep {
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

/// `Ctrl+Shift+…` → прокрутка вьюпорта; `None` — клавиша уходит shell'у.
///
/// Простые стрелки/PageUp/PageDown остаются у shell'а (редактирование строки,
/// history, kernel-scrollback): перехват без модификатора сломал бы ввод.
/// Поэтому панорама окна — только с `Ctrl+Shift`, а `Ctrl+Shift+Z` — переключение
/// 1:1 ↔ целочисленный обзор ([`zoom_toggle_key`]).
pub fn pan_step_for_key(code: u32, pressed: bool, modifiers: u32) -> Option<PanStep> {
    if !pressed || modifiers & (mods::CTRL | mods::SHIFT) != (mods::CTRL | mods::SHIFT) {
        return None;
    }
    use keys::evdev as key;
    match code {
        key::KEY_LEFT => Some(PanStep::Left),
        key::KEY_RIGHT => Some(PanStep::Right),
        key::KEY_UP => Some(PanStep::Up),
        key::KEY_DOWN => Some(PanStep::Down),
        key::KEY_PAGEUP => Some(PanStep::PageUp),
        key::KEY_PAGEDOWN => Some(PanStep::PageDown),
        key::KEY_HOME => Some(PanStep::Home),
        key::KEY_END => Some(PanStep::End),
        _ => None,
    }
}

/// `Ctrl+Shift+Z` — переключить 1:1 ↔ целочисленный зум-обзор.
pub fn zoom_toggle_key(code: u32, pressed: bool, modifiers: u32) -> bool {
    pressed
        && code == keys::evdev::KEY_Z
        && modifiers & (mods::CTRL | mods::SHIFT) == (mods::CTRL | mods::SHIFT)
}

/// Индекс пикселя-источника для индекса `d` в окне.
///
/// Только целочисленное деление (floor): каждый пиксель окна копирует ровно
/// один пиксель поверхности, смешения нет.  При точном целом делителе `n`
/// (`dst_len == src_len / n`) это ровно `d * n`.
fn sample_index(d: usize, src_len: usize, dst_len: usize) -> usize {
    (d * src_len / dst_len).min(src_len.saturating_sub(1))
}

/// Блит XRGB-пикселей ядра в RGBA-канву окна.
///
/// Байты в памяти — `B, G, R, X` (XRGB8888 little-endian, как формат 1 PDP/1).
/// Масштаб — только целый (`fit_placement`/1:1-вьюпорт): выборка nearest
/// neighbour по целочисленному индексу, без фильтра и без дробной
/// интерполяции.  `place.src_*` задаёт вырез поверхности: для 1:1 это
/// прокрученное окно-размерное окно, для Integer — вся поверхность.
/// Источник короче объявленного `stride * height` — честная ошибка
/// [`VtFbError::ShortRead`], а не «нарисуем что-нибудь».
pub fn blit_xrgb(
    src: &[u8],
    info: &VtFbInfo,
    canvas: &mut Canvas,
    place: &FitPlacement,
) -> Result<(), VtFbError> {
    let stride = info.stride as usize;
    if place.width == 0 || place.height == 0 {
        return Ok(());
    }
    for dy in 0..place.height {
        let sy = place.src_y + sample_index(dy, place.src_height, place.height);
        let row = sy.saturating_mul(stride);
        for dx in 0..place.width {
            let sx = place.src_x + sample_index(dx, place.src_width, place.width);
            let index = row + sx * 4;
            let Some(pixel) = src.get(index..index + 4) else {
                return Err(VtFbError::ShortRead {
                    need: row + sx * 4 + 4,
                    have: src.len(),
                });
            };
            canvas.set_pixel_rgba(
                place.x + dx,
                place.y + dy,
                [pixel[2], pixel[1], pixel[0], 255],
            );
        }
    }
    Ok(())
}

/// Результат [`KernelFrame::draw_if_changed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Draw {
    /// `generation` не менялась — кадр не трогали.
    Unchanged,
    /// Кадр перерисован; `generation` — та, что попала в окно.
    Drawn { generation: u64 },
}

/// Драйвер кадра сессии: `generation`-гейт, блит, полная перерисовка при
/// повреждении/ресайзе.
///
/// Контракт ядра не даёт seqlock (в отличие от `/dev/vtconsole0`), поэтому
/// кадр может быть «порван» flush'ем посередине блита. Реакция — полный блит
/// на следующем изменении `generation`: следующий кадр непротиворечив целиком,
/// потому что сессия публикует его атомарно по `generation`. Никаких
/// частичных damage от ядра v1 не приходит.
pub struct KernelFrame<S: FrameSource> {
    source: S,
    info: VtFbInfo,
    last_generation: Option<u64>,
    full: bool,
    /// Размер клиентской области окна (content rect) для 1:1-вьюпорта.
    window: (usize, usize),
    /// Режим показа + прокрутка; целиком клиентское состояние.
    view: View,
}

impl<S: FrameSource> KernelFrame<S> {
    /// Открыть драйвер: первый `poll()` валидирует геометрию до первого блита.
    pub fn open(mut source: S) -> Result<KernelFrame<S>, VtFbError> {
        let info = source.poll()?;
        Ok(KernelFrame {
            source,
            info,
            last_generation: None,
            full: true,
            window: (0, 0),
            view: View::default(),
        })
    }

    pub fn info(&self) -> VtFbInfo {
        self.info
    }

    /// Текущий режим показа (1:1 или целочисленный зум).
    pub fn view(&self) -> View {
        self.view
    }

    /// Размер окна, под который сейчас считается раскладка.
    pub fn window_size(&self) -> (usize, usize) {
        self.window
    }

    /// Фактическая прокрутка с учётом автоследования за нижним краем.
    pub fn scroll(&self) -> (usize, usize) {
        self.view.effective_scroll(
            self.info.width as usize,
            self.info.height as usize,
            self.window.0,
            self.window.1,
        )
    }

    /// Включено ли автоследование (новый вывод держит вьюпорт на нижнем крае).
    pub fn following(&self) -> bool {
        self.view.follows()
    }

    /// Переключить режим показа; следующий кадр перерисовывается полностью.
    pub fn set_view_mode(&mut self, mode: ScaleMode) {
        self.view.set_mode(mode);
        self.invalidate();
    }

    /// Переключить 1:1 ↔ целочисленный зум; возвращает новый режим.
    pub fn toggle_view_mode(&mut self) -> ScaleMode {
        let next = match self.view.mode() {
            ScaleMode::OneToOne => ScaleMode::Integer,
            ScaleMode::Integer => ScaleMode::OneToOne,
        };
        self.set_view_mode(next);
        next
    }

    /// Сообщить новый размер окна (`configure`/resize).
    ///
    /// Прокрутка прижимается к новой геометрии: выросшее окно отдаёт лишнее
    /// смещение, окно не меньше поверхности сбрасывает его в ноль.  Следующий
    /// `draw_if_changed` блитит полный кадр.
    pub fn set_window(&mut self, width: usize, height: usize) {
        self.window = (width, height);
        self.view.reclamp(
            self.info.width as usize,
            self.info.height as usize,
            width,
            height,
        );
        self.invalidate();
    }

    /// Раскладка для текущего размера окна (для указателя/тестов).
    pub fn placement(&self) -> Option<FitPlacement> {
        self.view.placement(
            self.info.width as usize,
            self.info.height as usize,
            self.window.0,
            self.window.1,
        )
    }

    /// Есть ли что прокручивать: поверхность больше окна хотя бы по одной оси
    /// и режим — 1:1.
    pub fn pannable(&self) -> bool {
        self.view.mode() == ScaleMode::OneToOne
            && (self.info.width as usize > self.window.0
                || self.info.height as usize > self.window.1)
    }

    /// Прокрутить 1:1-вьюпорт; `true` — перерисовать кадр.
    pub fn pan_by(&mut self, dx: i32, dy: i32) -> bool {
        let changed = self.view.pan_by(
            dx,
            dy,
            self.info.width as usize,
            self.info.height as usize,
            self.window.0,
            self.window.1,
        );
        if changed {
            self.invalidate();
        }
        changed
    }

    /// Прокрутить 1:1-вьюпорт в абсолютную позицию (Home/End).
    pub fn pan_to(&mut self, x: usize, y: usize) -> bool {
        let changed = self.view.pan_to(
            x,
            y,
            self.info.width as usize,
            self.info.height as usize,
            self.window.0,
            self.window.1,
        );
        if changed {
            self.invalidate();
        }
        changed
    }

    /// Применить клавиатурный шаг прокрутки 1:1-вьюпорта.
    ///
    /// Стрелки — одна клетка, PageUp/PageDown — почти весь видимый рост,
    /// Home/End — край поверхности.  `true` — кадр изменился и его надо
    /// перерисовать.
    pub fn pan_step(&mut self, step: PanStep) -> bool {
        let page_y = self.window.1.saturating_sub(CELL_H).max(CELL_H) as i32;
        match step {
            PanStep::Left => self.pan_by(-(CELL_W as i32), 0),
            PanStep::Right => self.pan_by(CELL_W as i32, 0),
            PanStep::Up => self.pan_by(0, -(CELL_H as i32)),
            PanStep::Down => self.pan_by(0, CELL_H as i32),
            PanStep::PageUp => self.pan_by(0, -page_y),
            PanStep::PageDown => self.pan_by(0, page_y),
            PanStep::Home => self.pan_to(0, 0),
            // End — на живой низ, горизонталь не трогаем (в 1:1 это X=0).
            PanStep::End => {
                let (x, _) = self.scroll();
                self.pan_to(x, usize::MAX)
            }
        }
    }

    /// Забыть generation: следующий кадр блитится полностью (ресайз окна,
    /// пересоздание поверхности, возврат фокуса).
    pub fn invalidate(&mut self) {
        self.full = true;
    }

    /// Закрыть поверхность (перед общим close-then-open respawn).
    pub fn close_source(&mut self) -> Result<(), VtFbError> {
        self.source.close()
    }

    /// Поднять новую сессию ядра на месте той, что завершилась (`ENODEV`).
    ///
    /// Окно клиента не пересоздаётся: переоткрывается только устройство, поэтому
    /// композитор не видит второго `create` и `duplicate window` невозможен.
    pub fn respawn(&mut self) -> Result<VtFbInfo, VtFbError> {
        self.source.respawn()?;
        self.last_generation = None;
        self.full = true;
        self.info = self.source.poll()?;
        // Новая сессия может сообщить другую геометрию: прокрутка не должна
        // выйти за её границы.
        self.view.reclamp(
            self.info.width as usize,
            self.info.height as usize,
            self.window.0,
            self.window.1,
        );
        Ok(self.info)
    }

    /// Перерисовать окно, если ядро опубликовало новый кадр.
    pub fn draw_if_changed(
        &mut self,
        canvas: &mut Canvas,
        theme: &Theme,
    ) -> Result<Draw, VtFbError> {
        let info = self.source.poll()?;
        // Окно могло измениться без `set_window` (первый кадр, прямой вызов):
        // раскладка обязана считаться от фактического холста, а прокрутка —
        // прижиматься к нему.
        let window = (canvas.width, canvas.height);
        if window != self.window {
            self.window = window;
            self.view.reclamp(
                info.width as usize,
                info.height as usize,
                window.0,
                window.1,
            );
            self.full = true;
        }
        if !self.full && Some(info.generation) == self.last_generation && info == self.info {
            return Ok(Draw::Unchanged);
        }
        let Some(place) = self.view.placement(
            info.width as usize,
            info.height as usize,
            canvas.width,
            canvas.height,
        ) else {
            return Ok(Draw::Unchanged);
        };
        canvas.fill(theme.bg);
        let need = info.pixels_len();
        let mut shortfall = None;
        self.source.with_pixels(&mut |pixels| {
            if pixels.len() < need {
                shortfall = Some(pixels.len());
                return;
            }
            if let Err(error) = blit_xrgb(pixels, &info, canvas, &place) {
                shortfall = Some(match error {
                    VtFbError::ShortRead { have, .. } => have,
                    _ => 0,
                });
            }
        })?;
        if let Some(have) = shortfall {
            return Err(VtFbError::ShortRead { need, have });
        }
        self.info = info;
        self.last_generation = Some(info.generation);
        self.full = false;
        Ok(Draw::Drawn {
            generation: info.generation,
        })
    }
}

/// Кодирование клавиши окна в VT-байты для `/dev/vtinput1`.
///
/// Режимы — `KeyModes::default()`: поверхность сессии пиксельная, DEC-режимы
/// (`?1` application cursor, bracketed paste, mouse) из неё не наблюдаемы.
/// Kernel-shell не включает application-cursor, поэтому normal-стрелки —
/// корректный и единственно возможный выбор v1; ограничение названо в
/// `TERMINAL.md`.
pub fn encode_key_bytes(code: u32, pressed: bool, modifiers: u32) -> Vec<u8> {
    keys::encode_key(code, pressed, modifiers, &KeyModes::default())
}

/// Пересылка клавиш окна в приёмник ввода.
pub struct KeyForwarder<S: InputSink> {
    sink: S,
    modes: KeyModes,
}

impl<S: InputSink> KeyForwarder<S> {
    pub fn new(sink: S) -> KeyForwarder<S> {
        KeyForwarder {
            sink,
            modes: KeyModes::default(),
        }
    }

    /// Отправить одно событие клавиши; возвращает число записанных байт.
    ///
    /// Release и чистые модификаторы кодируются в пустой вектор и не пишут
    /// ничего: пустой `write` на устройстве ввода — лишний сисколл и
    /// (теоретически) ложное «событие» для ядра.
    pub fn handle_key(&mut self, code: u32, pressed: bool, modifiers: u32) -> io::Result<usize> {
        let bytes = keys::encode_key(code, pressed, modifiers, &self.modes);
        if bytes.is_empty() {
            return Ok(0);
        }
        self.sink.send(&bytes)?;
        Ok(bytes.len())
    }

    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// Закрыть устройство ввода (перед общим close-then-open respawn).
    pub fn close(&mut self) -> io::Result<()> {
        self.sink.close()
    }

    /// Переоткрыть устройство ввода для новой сессии.
    pub fn respawn(&mut self) -> io::Result<()> {
        self.sink.respawn()
    }
}

/// Длина записи указателя `/dev/vtmouse{N}` (ABI, см. `drivers/vt_mouse_abi`).
pub const MOUSE_EVENT_LEN: usize = 16;
/// Бит левой кнопки в записи указателя.
pub const MOUSE_BTN_LEFT: u32 = 1;
/// Бит правой кнопки.
pub const MOUSE_BTN_RIGHT: u32 = 2;
/// Бит средней кнопки.
pub const MOUSE_BTN_MIDDLE: u32 = 4;

/// Одна запись указателя сессии (16 байт little-endian).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseEvent {
    /// X в пикселях поверхности сессии (не окна).
    pub x: i32,
    /// Y в пикселях поверхности сессии.
    pub y: i32,
    /// Абсолютное состояние кнопок ([`MOUSE_BTN_LEFT`] и т. д.).
    pub buttons: u32,
    /// Вертикальные щелчки с прошлой записи: `+` — от пользователя (старее).
    pub wheel: i32,
}

/// Закодировать запись указателя (16 байт LE) — контракт `/dev/vtmouse{N}`.
pub fn encode_mouse_event(event: &MouseEvent) -> [u8; MOUSE_EVENT_LEN] {
    let mut out = [0u8; MOUSE_EVENT_LEN];
    out[0..4].copy_from_slice(&event.x.to_le_bytes());
    out[4..8].copy_from_slice(&event.y.to_le_bytes());
    out[8..12].copy_from_slice(&event.buttons.to_le_bytes());
    out[12..16].copy_from_slice(&event.wheel.to_le_bytes());
    out
}

/// Маска удерживаемых кнопок PDP/1 (`buttons::MASK_*`) → биты ядра.
///
/// PDP/1 и vtmouse называют кнопки по-разному: первый — битовой маской
/// `MASK_LEFT/MIDDLE/RIGHT`, второй — `1/2/4` в порядке ЛКМ/ПКМ/СКМ. Ошибка
/// здесь — «нажал левую, выделилась правая», поэтому перевод один и тестируется.
pub fn buttons_to_mask(buttons: u32) -> u32 {
    use crate::protocol::buttons;
    let mut mask = 0;
    if buttons & buttons::MASK_LEFT != 0 {
        mask |= MOUSE_BTN_LEFT;
    }
    if buttons & buttons::MASK_RIGHT != 0 {
        mask |= MOUSE_BTN_RIGHT;
    }
    if buttons & buttons::MASK_MIDDLE != 0 {
        mask |= MOUSE_BTN_MIDDLE;
    }
    mask
}

/// Колесо PDP/1 (`REL_WHEEL`/`Axis.vertical`) → колесо ядра.
///
/// Знак инвертируется, и это проверено живьём (tools/e2e.py termux): при
/// передаче «как есть» колесо «вверх» не листало историю (frame diff 49), а
/// «вниз» уезжало в неё (2656). `Axis.vertical` здесь имеет знак QEMU
/// (`wheel-up` → отрицательный `REL_WHEEL`), а kernel-shell ждёт
/// PS/2-соглашение IntelliMouse, в котором «от пользователя» — отрицательное
/// приращение `ps2_mouse`, и считает `lines = -delta`. Одна инверсия в клиенте
/// согласует оба: колесо вверх листает к старым строкам.
pub fn wheel_from_axis(vertical: i32) -> i32 {
    -vertical
}

/// Координаты окна (content rect) → пиксели поверхности сессии.
///
/// Обратное к [`blit_xrgb`]: окно показывает `place`-вырез поверхности
/// (`src_x/src_y`, `src_width/src_height`), поэтому
/// `sx = src_x + (x - place.x) * src_width / place.width`.  Для 1:1-вьюпорта
/// `src_width == width`, то есть `sx = src_x + (x - place.x)` — клик попадает
/// ровно в тот пиксель, который виден.  Точки в letterbox'е (полях)
/// прижимаются к краю видимого выреза, а результат — к границам поверхности:
/// колесо/клик в поле не должны уводить указатель сессии за поверхность.
/// Нулевые размеры (сессия без кадра) дают `(0, 0)`.
pub fn window_to_surface(x: i32, y: i32, info: &VtFbInfo, place: &FitPlacement) -> (i32, i32) {
    let src_w = info.width as i64;
    let src_h = info.height as i64;
    if src_w <= 0 || src_h <= 0 || place.width == 0 || place.height == 0 {
        return (0, 0);
    }
    let rel_x = (x - place.x as i32).clamp(0, place.width.saturating_sub(1) as i32) as i64;
    let rel_y = (y - place.y as i32).clamp(0, place.height.saturating_sub(1) as i32) as i64;
    let sx = place.src_x as i64 + rel_x * place.src_width as i64 / place.width as i64;
    let sy = place.src_y as i64 + rel_y * place.src_height as i64 / place.height as i64;
    (sx.clamp(0, src_w - 1) as i32, sy.clamp(0, src_h - 1) as i32)
}

/// Пересылка указателя окна в `/dev/vtmouse{N}`.
///
/// Состояние (позиция/кнопки) живёт здесь, потому что PDP/1 присылает и
/// движение, и нажатия с абсолютной маской — ядру нужна одна запись на каждое
/// изменение, а колесо отдельной записью с дельтой.
pub struct MouseForwarder<S: InputSink> {
    sink: S,
    width: u32,
    height: u32,
    place: FitPlacement,
    x: i32,
    y: i32,
    buttons: u32,
}

impl<S: InputSink> MouseForwarder<S> {
    pub fn new(sink: S, info: &VtFbInfo, place: FitPlacement) -> MouseForwarder<S> {
        MouseForwarder {
            sink,
            width: info.width,
            height: info.height,
            place,
            x: 0,
            y: 0,
            buttons: 0,
        }
    }

    /// Новая геометрия окна/поверхности (Configure, ресайз).
    pub fn set_geometry(&mut self, info: &VtFbInfo, place: FitPlacement) {
        self.width = info.width;
        self.height = info.height;
        self.place = place;
    }

    /// Последняя точка в пикселях поверхности (диагностика/тесты).
    pub fn pointer(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    fn update(&mut self, x: i32, y: i32, buttons: u32) {
        let info = VtFbInfo {
            width: self.width,
            height: self.height,
            stride: self.width.saturating_mul(4),
            bpp: 32,
            generation: 0,
        };
        let (sx, sy) = window_to_surface(x, y, &info, &self.place);
        self.x = sx;
        self.y = sy;
        self.buttons = buttons_to_mask(buttons);
    }

    fn flush(&mut self, wheel: i32) -> io::Result<()> {
        let event = MouseEvent {
            x: self.x,
            y: self.y,
            buttons: self.buttons,
            wheel,
        };
        self.sink.send(&encode_mouse_event(&event))
    }

    /// Движение указателя внутри окна (без изменения кнопок).
    pub fn motion(&mut self, x: i32, y: i32, buttons: u32) -> io::Result<()> {
        self.update(x, y, buttons);
        self.flush(0)
    }

    /// Нажатие/отпускание кнопки: запись несёт новую маску.
    pub fn button(&mut self, x: i32, y: i32, buttons: u32) -> io::Result<()> {
        self.update(x, y, buttons);
        self.flush(0)
    }

    /// Прокрутка колеса: запись несёт дельту в соглашении ядра.
    pub fn axis(&mut self, x: i32, y: i32, buttons: u32, vertical: i32) -> io::Result<()> {
        self.update(x, y, buttons);
        self.flush(wheel_from_axis(vertical))
    }

    /// Приёмник (для тестов: прочитать записанное).
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// Закрыть устройство указателя перед общим close-then-open respawn.
    pub fn close(&mut self) -> io::Result<()> {
        self.sink.close()
    }

    /// Переоткрыть устройство указателя для новой сессии (тот же слот).
    pub fn respawn(&mut self) -> io::Result<()> {
        self.sink.respawn()
    }
}

/// Нарисовать честное сообщение (ошибка устройства, конец сессии) в окне.
///
/// Окно режима kernel-session — пиксельная поверхность, поэтому «напечатать
/// текст» можно только собственным рендерером: собираем обычный `Terminal`,
/// кормим строки и рисуем его в канву.  Это не имитация shell'а — это
/// сообщение клиента о том, что сессии больше нет.
pub fn render_notice(canvas: &mut Canvas, theme: &Theme, lines: &[&str]) {
    let pad = VIEWPORT_PADDING * 2;
    let cols = (canvas.width.saturating_sub(pad) / CELL_W).max(8);
    let rows = (canvas.height.saturating_sub(pad) / CELL_H).max(2);
    let mut terminal = Terminal::new(cols, rows);
    terminal.screen.set_default_colors(theme.fg, theme.bg);
    for line in lines {
        terminal.feed_str(line);
        terminal.feed_str("\r\n");
    }
    canvas.fill(theme.bg);
    render_screen_into(
        canvas,
        VIEWPORT_PADDING,
        VIEWPORT_PADDING,
        &terminal.screen,
        theme,
        &RenderOptions {
            focused: true,
            ..RenderOptions::default()
        },
    );
}

/// Прочитать ровно `buf.len()` байт по смещению (pread; файл не сдвигается).
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> Result<(), VtFbError> {
    let mut done = 0;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], offset + done as u64) {
            Ok(0) => {
                return Err(VtFbError::ShortRead {
                    need: buf.len(),
                    have: done,
                })
            }
            Ok(n) => done += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(VtFbError::from(error)),
        }
    }
    Ok(())
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// Заголовок + пиксели одной поверхности — общий конструктор для mock'ов и
/// тестовых `read()`-файлов.
pub fn encode_snapshot(info: VtFbInfo, pixels: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(INFO_LEN + pixels.len());
    bytes.extend_from_slice(&info.encode());
    bytes.extend_from_slice(pixels);
    bytes
}

/// Заполнить кадр `stride*height` одним цветом XRGB (тестовый хелпер).
pub fn solid_pixels(info: &VtFbInfo, rgb: (u8, u8, u8)) -> Vec<u8> {
    let mut pixels = vec![0u8; info.pixels_len()];
    for pixel in pixels.chunks_exact_mut(4) {
        pixel[0] = rgb.2;
        pixel[1] = rgb.1;
        pixel[2] = rgb.0;
        pixel[3] = 0;
    }
    pixels
}

/// `Read`-обёртка для тестов: прочитать весь тестовый файл как снимок.
pub fn read_snapshot(path: impl AsRef<Path>) -> Result<Vec<u8>, VtFbError> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(width: u32, height: u32, generation: u64) -> VtFbInfo {
        VtFbInfo {
            width,
            height,
            stride: width * 4,
            bpp: 32,
            generation,
        }
    }

    #[test]
    fn header_round_trips_through_24_bytes() {
        let info = VtFbInfo {
            width: 640,
            height: 480,
            stride: 2560,
            bpp: 32,
            generation: 0x1122_3344_5566_7788,
        };
        let parsed = VtFbInfo::parse(&info.encode()).unwrap();
        assert_eq!(parsed, info);
        assert_eq!(info.encode().len(), INFO_LEN);
        assert_eq!(parsed.pixels_len(), 2560 * 480);
    }

    #[test]
    fn header_validation_names_the_violation() {
        fn check(mutate: impl Fn(&mut VtFbInfo)) -> String {
            let mut info = info(8, 8, 1);
            mutate(&mut info);
            match VtFbInfo::parse(&info.encode()) {
                Err(error) => error.message(),
                Ok(_) => panic!("geometry must be rejected"),
            }
        }
        assert!(check(|i| i.bpp = 16).contains("bpp must be 32"));
        assert!(check(|i| i.width = 0).contains("non-zero"));
        assert!(check(|i| i.stride = 8).contains("stride is smaller"));
        assert!(check(|i| i.height = 40_000).contains("exceeds 8192x8192"));
        assert!(check(|i| i.width = 9000).contains("exceeds 8192x8192"));
    }

    #[test]
    fn short_header_is_a_short_read() {
        let bytes = info(4, 4, 0).encode();
        match VtFbInfo::parse(&bytes[..INFO_LEN - 1]) {
            Err(VtFbError::ShortRead { need, have }) => {
                assert_eq!(need, INFO_LEN);
                assert_eq!(have, INFO_LEN - 1);
            }
            other => panic!("expected ShortRead, got {other:?}"),
        }
    }

    #[test]
    fn fit_placement_is_one_to_one_when_the_window_fits() {
        // 1:1 (окно больше поверхности) — по центру, без растяжения.
        let place = fit_placement(640, 400, 900, 600).unwrap();
        assert_eq!(
            (
                place.x,
                place.y,
                place.width,
                place.height,
                place.src_x,
                place.src_y,
                place.src_width,
                place.src_height
            ),
            (130, 100, 640, 400, 0, 0, 640, 400)
        );
        // Ровно в размер — без смещений и без масштаба.
        let place = fit_placement(640, 400, 640, 400).unwrap();
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (0, 0, 640, 400)
        );
        // 1280×800 в окне 1400×900: 1:1, поля по 60/50 px.
        let place = fit_placement(1280, 800, 1400, 900).unwrap();
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (60, 50, 1280, 800)
        );
        // Широкое окно с запасом по высоте тоже 1:1, а не апскейл.
        let place = fit_placement(640, 400, 1024, 768).unwrap();
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (192, 184, 640, 400)
        );
    }

    #[test]
    fn fit_placement_downscales_by_an_integer_factor_only() {
        // 1280×800 в 900×600: дробный шаг был 0.70; целый — ровно 2×.
        let place = fit_placement(1280, 800, 900, 600).unwrap();
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (130, 100, 640, 400)
        );
        // 1180×700 (штатное окно e2e) — тот же целый 2×, а не 0.92.
        let place = fit_placement(1280, 800, 1180, 700).unwrap();
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (270, 150, 640, 400)
        );
        // 1024×768 в 900×600: 512×384 по центру.
        let place = fit_placement(1024, 768, 900, 600).unwrap();
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (194, 108, 512, 384)
        );
        // Очень широкое окно: упираемся в высоту, делитель 3.
        let place = fit_placement(1000, 100, 400, 400).unwrap();
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (33, 183, 333, 33)
        );
        // Ровно вдвое меньшее окно — ровно 2×.
        let place = fit_placement(1280, 800, 640, 400).unwrap();
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (0, 0, 640, 400)
        );
        // Вчетверо меньшее окно — ровно 4×.
        let place = fit_placement(1280, 800, 320, 200).unwrap();
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (0, 0, 320, 200)
        );
    }

    #[test]
    fn fit_placement_never_upscales_never_crops_and_never_panics() {
        // Ноль с любой стороны — рисовать нечего, но не паника.
        assert!(fit_placement(0, 10, 10, 10).is_none());
        assert!(fit_placement(10, 10, 0, 10).is_none());
        assert!(fit_placement(10, 0, 10, 10).is_none());
        assert!(fit_placement(10, 10, 10, 0).is_none());

        for &(sw, sh) in &[(1, 1), (16, 8), (640, 400), (1280, 800), (1024, 768)] {
            for &(dw, dh) in &[
                (1, 1),
                (3, 2),
                (100, 1),
                (200, 150),
                (640, 400),
                (900, 600),
                (1180, 700),
                (1280, 800),
                (1400, 900),
                (4096, 4096),
            ] {
                let place = fit_placement(sw, sh, dw, dh).expect("non-zero sizes fit");
                // Никогда не апскейл: кадр не больше поверхности.
                assert!(place.width <= sw, "{sw}x{sh} in {dw}x{dh}: {place:?}");
                assert!(place.height <= sh, "{sw}x{sh} in {dw}x{dh}: {place:?}");
                // Никогда не кроп: кадр целиком внутри окна.
                assert!(place.x + place.width <= dw, "{sw}x{sh} in {dw}x{dh}");
                assert!(place.y + place.height <= dh, "{sw}x{sh} in {dw}x{dh}");
                // Масштаб — либо 1:1, либо целый делитель, и он покрывает всю
                // поверхность с точностью до округления вниз (< делителя).
                if dw >= sw && dh >= sh {
                    assert_eq!((place.width, place.height), (sw, sh));
                } else {
                    let scale = integer_downscale(sw, sh, dw, dh);
                    assert!(scale >= 2, "{sw}x{sh} in {dw}x{dh}: scale {scale}");
                    assert_eq!(place.width, (sw / scale).max(1).min(dw));
                    assert_eq!(place.height, (sh / scale).max(1).min(dh));
                    // Ни один пиксель поверхности не потерян незамеченным:
                    // остаток меньше одного шага делителя.  Клампированный к
                    // 1 px вырожденный случай сюда не входит по построению.
                    if place.width > 1 {
                        assert!(sw - place.width * scale < scale);
                    }
                    if place.height > 1 {
                        assert!(sh - place.height * scale < scale);
                    }
                }
            }
        }
    }

    #[test]
    fn view_defaults_to_full_size_one_to_one_following_the_bottom() {
        let view = View::default();
        assert_eq!(view.mode(), ScaleMode::OneToOne);
        assert!(view.follows());
        // Нижний край: приглашение/новый вывод видны без ручной прокрутки.
        assert_eq!(view.effective_scroll(1280, 800, 900, 600), (0, 200));
        let place = view.placement(1280, 800, 900, 600).unwrap();
        // Окно целиком занято 1:1-вырезом: пиксель в пиксель, без масштаба.
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (0, 0, 900, 600)
        );
        assert_eq!(
            (place.src_x, place.src_y, place.src_width, place.src_height),
            (0, 200, 900, 600)
        );
        // 1:1: вырез не растягивается/сжимается.
        assert_eq!(place.src_width, place.width);
        assert_eq!(place.src_height, place.height);
        // Хранимая позиция до первого resize/pan ещё 0, но фактическая — низ.
        assert_eq!(view.scroll(), (0, 0));
    }

    #[test]
    fn view_pan_up_stops_following_and_bottom_resumes_it() {
        let mut view = View::default();
        // Wheel-up: уходим от живого низа, автоследование выключается.
        assert!(view.pan_by(0, -100, 1280, 800, 900, 600));
        assert_eq!(view.scroll(), (0, 100));
        assert!(!view.follows());
        // Дальше вверх — Home.
        assert!(view.pan_by(0, -10_000, 1280, 800, 900, 600));
        assert_eq!(view.scroll(), (0, 0));
        assert!(!view.follows());
        // Нулевая дельта кадр не меняет.
        assert!(!view.pan_by(0, 0, 1280, 800, 900, 600));
        // Вниз до упора — снова автоследование.
        assert!(view.pan_by(0, 10_000, 1280, 800, 900, 600));
        assert_eq!(view.scroll(), (0, 200));
        assert!(view.follows());
        // Горизонтальная прокрутка на нижнем крае не отключает следование.
        assert!(view.pan_by(10_000, 0, 1280, 800, 900, 600));
        assert_eq!(view.scroll(), (380, 200));
        assert!(view.follows());
        // Ctrl+Shift+End — абсолютный нижний край, следование включено.
        assert!(view.pan_to(0, 0, 1280, 800, 900, 600));
        assert!(!view.follows());
        assert!(view.pan_to(0, usize::MAX, 1280, 800, 900, 600));
        assert_eq!(view.scroll(), (0, 200));
        assert!(view.follows());
    }

    #[test]
    fn view_new_output_keeps_following_until_the_user_pans_up() {
        // Пока follow включён, фактическая позиция — всегда нижний край, в том
        // числе после появления нового вывода (новая generation).
        let mut view = View::default();
        assert_eq!(view.effective_scroll(1280, 800, 900, 600), (0, 200));
        view.reclamp(1280, 800, 900, 600);
        assert_eq!(view.effective_scroll(1280, 800, 900, 600), (0, 200));
        // Пользователь ушёл в историю: новый вывод не дёргает обратно.
        assert!(view.pan_by(0, -80, 1280, 800, 900, 600));
        assert_eq!(view.effective_scroll(1280, 800, 900, 600), (0, 120));
        assert!(!view.follows());
        view.reclamp(1280, 800, 900, 600);
        assert_eq!(
            view.effective_scroll(1280, 800, 900, 600),
            (0, 120),
            "a generation change must not yank a user reading history"
        );
        // End возвращает к живому низу и возобновляет следование.
        assert!(view.pan_to(0, usize::MAX, 1280, 800, 900, 600));
        assert_eq!(view.effective_scroll(1280, 800, 900, 600), (0, 200));
        assert!(view.follows());
    }

    #[test]
    fn view_centers_axes_that_fit_inside_the_window() {
        // Окно шире поверхности, но ниже: по X кадр целиком и по центру,
        // по Y — прокручиваемый 1:1-вырез, по умолчанию на нижнем крае.
        let mut view = View::default();
        let place = view.placement(1280, 800, 1400, 700).unwrap();
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (60, 0, 1280, 700)
        );
        assert_eq!(
            (place.src_x, place.src_y, place.src_width, place.src_height),
            (0, 100, 1280, 700)
        );
        // По X прокрутки нет — дельта игнорируется, кадр не трогается.
        assert!(!view.pan_by(100, 0, 1280, 800, 1400, 700));
        // Вверх по Y — история, следование выключается.
        assert!(view.pan_by(0, -40, 1280, 800, 1400, 700));
        assert_eq!(view.scroll(), (0, 60));
        assert!(!view.follows());

        // Окно выше поверхности, но уже: по Y целиком и по центру (follow —
        // no-op), по X — вырез.
        let mut view = View::default();
        assert!(view.pan_by(100, 0, 1280, 800, 900, 900));
        let place = view.placement(1280, 800, 900, 900).unwrap();
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (0, 50, 900, 800)
        );
        assert_eq!((place.src_x, place.src_y), (100, 0));
        // Y помещается целиком, поэтому follow остаётся (no-op).
        assert!(view.follows());
    }

    #[test]
    fn view_resize_keeps_follow_at_bottom_and_preserves_user_offset() {
        // Автоследование: после resize остаёмся на нижнем крае.
        let mut view = View::default();
        view.reclamp(1280, 800, 1180, 700);
        assert_eq!(view.scroll(), (0, 100));
        assert!(view.follows());
        // Пользователь ушёл в историю: resize сохраняет его смещение (кламп).
        assert!(view.pan_to(20, 40, 1280, 800, 1180, 700));
        assert!(!view.follows());
        view.reclamp(1280, 800, 900, 500);
        assert_eq!(view.scroll(), (20, 40));
        // Окно не меньше поверхности: смещение обнуляется, кадр по центру.
        view.reclamp(1280, 800, 1400, 900);
        assert_eq!(view.scroll(), (0, 0));
        assert!(
            !view.follows(),
            "user choice is preserved, not silently reset"
        );
        let place = view.placement(1280, 800, 1400, 900).unwrap();
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (60, 50, 1280, 800)
        );
        // End возвращает следование; уменьшение окна снова держит низ.
        view.pan_to(0, usize::MAX, 1280, 800, 1400, 900);
        assert!(view.follows());
        view.reclamp(1280, 800, 900, 600);
        assert_eq!(view.scroll(), (0, 200));
    }

    #[test]
    fn view_whole_frame_makes_follow_a_no_op() {
        let mut view = View::default();
        assert!(view.follows());
        assert_eq!(view.effective_scroll(1280, 800, 1400, 900), (0, 0));
        let place = view.placement(1280, 800, 1400, 900).unwrap();
        assert_eq!(
            (place.src_x, place.src_y, place.src_width, place.src_height),
            (0, 0, 1280, 800)
        );
        assert!(!view.pan_by(0, 100, 1280, 800, 1400, 900));
        assert_eq!(view.effective_scroll(1280, 800, 1400, 900), (0, 0));
        view.reclamp(1280, 800, 1400, 900);
        assert_eq!(view.scroll(), (0, 0));
        assert!(view.follows());
    }

    #[test]
    fn view_integer_mode_is_optional_and_has_no_scroll() {
        let mut view = View::default();
        view.set_mode(ScaleMode::Integer);
        assert_eq!(view.mode(), ScaleMode::Integer);
        let place = view.placement(1280, 800, 900, 600).unwrap();
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (130, 100, 640, 400)
        );
        assert_eq!(
            (place.src_x, place.src_y, place.src_width, place.src_height),
            (0, 0, 1280, 800)
        );
        // В зум-обзоре весь кадр виден целиком: прокрутки и follow нет.
        assert!(!view.pan_by(100, 100, 1280, 800, 900, 600));
        assert!(!view.pan_to(100, 100, 1280, 800, 900, 600));
        assert_eq!(view.scroll(), (0, 0));
        // Возврат к 1:1 начинается с живого низа (приглашение видно).
        view.set_mode(ScaleMode::OneToOne);
        assert_eq!(view.placement(1280, 800, 900, 600).unwrap().src_y, 200);
        assert!(view.follows());
    }

    #[test]
    fn blit_one_to_one_viewport_copies_pixels_at_the_scroll_offset() {
        // Поверхность 8×6 с уникальным цветом на пиксель; окно 4×3,
        // прокрученное в (2, 1).  Пиксель окна обязан быть ровно пикселем
        // поверхности, без масштаба и смешения.
        let info = info(8, 6, 0);
        let mut src = vec![0u8; info.pixels_len()];
        for y in 0..6usize {
            for x in 0..8usize {
                let i = (y * 8 + x) * 4;
                src[i] = 0;
                src[i + 1] = y as u8;
                src[i + 2] = x as u8;
            }
        }
        let mut view = View::default();
        assert!(view.pan_to(2, 1, 8, 6, 4, 3));
        let place = view.placement(8, 6, 4, 3).unwrap();
        assert_eq!((place.x, place.y, place.width, place.height), (0, 0, 4, 3));
        assert_eq!(
            (place.src_x, place.src_y, place.src_width, place.src_height),
            (2, 1, 4, 3)
        );
        let mut canvas = Canvas::new(4, 3, crate::cell::Rgb(9, 9, 9));
        blit_xrgb(&src, &info, &mut canvas, &place).unwrap();
        for dy in 0..3 {
            for dx in 0..4 {
                assert_eq!(
                    canvas.pixel(dx, dy),
                    [(2 + dx) as u8, (1 + dy) as u8, 0, 255],
                    "dest ({dx},{dy}) must copy source ({}, {})",
                    2 + dx,
                    1 + dy
                );
            }
        }
    }

    #[test]
    fn window_to_surface_follows_the_viewport_offset() {
        let info = info(1280, 800, 0);
        let mut view = View::default();
        assert!(view.pan_to(200, 100, 1280, 800, 900, 600));
        let place = view.placement(1280, 800, 900, 600).unwrap();
        assert_eq!(window_to_surface(0, 0, &info, &place), (200, 100));
        assert_eq!(window_to_surface(899, 599, &info, &place), (1099, 699));
        // Поле по центрированной оси прижимается к краю выреза.
        let mut view = View::default();
        assert!(view.pan_to(0, 0, 1280, 800, 1400, 700));
        let place = view.placement(1280, 800, 1400, 700).unwrap();
        assert_eq!(window_to_surface(60, 0, &info, &place), (0, 0));
        assert_eq!(window_to_surface(0, 0, &info, &place), (0, 0));
    }

    #[test]
    fn pan_keys_require_ctrl_shift_and_cover_the_viewport() {
        use crate::keys::evdev as key;
        let both = mods::CTRL | mods::SHIFT;
        assert_eq!(
            pan_step_for_key(key::KEY_LEFT, true, both),
            Some(PanStep::Left)
        );
        assert_eq!(
            pan_step_for_key(key::KEY_RIGHT, true, both),
            Some(PanStep::Right)
        );
        assert_eq!(pan_step_for_key(key::KEY_UP, true, both), Some(PanStep::Up));
        assert_eq!(
            pan_step_for_key(key::KEY_DOWN, true, both),
            Some(PanStep::Down)
        );
        assert_eq!(
            pan_step_for_key(key::KEY_PAGEUP, true, both),
            Some(PanStep::PageUp)
        );
        assert_eq!(
            pan_step_for_key(key::KEY_PAGEDOWN, true, both),
            Some(PanStep::PageDown)
        );
        assert_eq!(
            pan_step_for_key(key::KEY_HOME, true, both),
            Some(PanStep::Home)
        );
        assert_eq!(
            pan_step_for_key(key::KEY_END, true, both),
            Some(PanStep::End)
        );
        // Без Ctrl+Shift клавиши принадлежат shell'у — не перехватываем.
        assert_eq!(pan_step_for_key(key::KEY_LEFT, true, mods::CTRL), None);
        assert_eq!(pan_step_for_key(key::KEY_LEFT, true, mods::SHIFT), None);
        assert_eq!(pan_step_for_key(key::KEY_LEFT, false, both), None);
        // Не-панорамная клавиша с теми же модификаторами не перехватывается.
        assert_eq!(pan_step_for_key(key::KEY_A, true, both), None);
    }

    #[test]
    fn zoom_toggle_is_ctrl_shift_z() {
        let both = mods::CTRL | mods::SHIFT;
        assert!(zoom_toggle_key(keys::evdev::KEY_Z, true, both));
        assert!(!zoom_toggle_key(keys::evdev::KEY_Z, false, both));
        assert!(!zoom_toggle_key(keys::evdev::KEY_Z, true, mods::CTRL));
        assert!(!zoom_toggle_key(keys::evdev::KEY_Z, true, mods::SHIFT));
        assert!(!zoom_toggle_key(keys::evdev::KEY_A, true, both));
    }

    #[test]
    fn blit_copies_xrgb_into_rgba() {
        let info = info(2, 1, 0);
        // Пиксель 0: XRGB 0x00112233 (память B,G,R,X); пиксель 1: красный.
        let src = vec![0x33, 0x22, 0x11, 0x00, 0x00, 0x00, 0xFF, 0x00];
        let mut canvas = Canvas::new(4, 3, crate::cell::Rgb(1, 2, 3));
        let place = fit_placement(2, 1, 4, 3).unwrap();
        blit_xrgb(&src, &info, &mut canvas, &place).unwrap();
        assert_eq!(canvas.pixel(1, 1), [0x11, 0x22, 0x33, 255]);
        assert_eq!(canvas.pixel(2, 1), [0xFF, 0x00, 0x00, 255]);
        // Поля вокруг кадра остались фоном темы.
        assert_eq!(canvas.pixel(0, 0), [1, 2, 3, 255]);
    }

    #[test]
    fn blit_integer_downscale_is_nearest_neighbour_not_a_blend() {
        // Поверхность 6×4, окно 3×2: ровно 2× по обеим осям.
        let info = info(6, 4, 0);
        // Каждый пиксель уникален: (r, g, b) = (x, y, 0), память B,G,R,X.
        let mut src = vec![0u8; info.pixels_len()];
        for y in 0..4usize {
            for x in 0..6usize {
                let i = (y * 6 + x) * 4;
                src[i] = 0;
                src[i + 1] = y as u8;
                src[i + 2] = x as u8;
            }
        }
        let mut canvas = Canvas::new(3, 2, crate::cell::Rgb(9, 9, 9));
        let place = fit_placement(6, 4, 3, 2).unwrap();
        assert_eq!((place.x, place.y, place.width, place.height), (0, 0, 3, 2));
        blit_xrgb(&src, &info, &mut canvas, &place).unwrap();
        // Выборка d * 2 по обеим осям, цвет источника без смешения.
        for dy in 0..2 {
            for dx in 0..3 {
                assert_eq!(
                    canvas.pixel(dx, dy),
                    [(dx * 2) as u8, (dy * 2) as u8, 0, 255],
                    "dest ({dx},{dy}) must copy source ({}, {}) verbatim",
                    dx * 2,
                    dy * 2
                );
            }
        }
    }

    #[test]
    fn sample_index_maps_every_destination_pixel_into_the_source() {
        assert_eq!(sample_index(0, 800, 400), 0);
        assert_eq!(sample_index(399, 800, 400), 798);
        // Нецелый делитель: floor-шаг, но всегда внутри источника.
        for d in 0..1000usize {
            assert!(sample_index(d, 1280, 426) < 1280);
            assert!(sample_index(d, 800, 100) < 800);
        }
        assert_eq!(sample_index(999, 800, 100), 799);
    }

    #[test]
    fn blit_rejects_a_truncated_source() {
        let info = info(2, 2, 0);
        let src = vec![0u8; 8];
        let mut canvas = Canvas::new(2, 2, crate::cell::Rgb(0, 0, 0));
        let place = fit_placement(2, 2, 2, 2).unwrap();
        match blit_xrgb(&src, &info, &mut canvas, &place) {
            Err(VtFbError::ShortRead { .. }) => {}
            other => panic!("expected ShortRead, got {other:?}"),
        }
    }

    #[test]
    fn errno_helpers_match_the_contract() {
        assert!(is_session_end(&io::Error::from_raw_os_error(ENODEV)));
        assert!(!is_session_end(&io::Error::from_raw_os_error(2)));
        assert!(is_missing_device(&io::Error::from_raw_os_error(2)));
        assert!(!is_missing_device(&io::Error::from_raw_os_error(ENODEV)));
        assert!(VtFbError::from(io::Error::from_raw_os_error(ENODEV)).is_ended());
        assert!(!VtFbError::from(io::Error::from_raw_os_error(13)).is_ended());
    }

    #[test]
    fn solid_pixels_and_snapshot_helpers_agree() {
        let info = info(2, 2, 7);
        let pixels = solid_pixels(&info, (0x11, 0x22, 0x33));
        assert_eq!(pixels.len(), 16);
        let snapshot = encode_snapshot(info, &pixels);
        assert_eq!(&snapshot[..INFO_LEN], &info.encode());
        assert_eq!(VtFbInfo::parse(&snapshot[..INFO_LEN]).unwrap(), info);
    }
}
