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
//! * нет PTY/termios/job-control: `Ctrl+C`/`Ctrl+Z` — просто байты в
//!   `/dev/vtinput1`, а `TIOCGWINSZ`/SIGWINCH неоткуда взять (размер знает
//!   только ядро);
//! * клиент не наблюдает DEC-режимы (пиксели не говорят, хочет ли сессия
//!   application-cursor keys), поэтому стрелки кодируются в normal-режиме
//!   (`KeyModes::default()`), как kernel-shell и ожидает;
//! * scrollback и selection остаются в ядре: колесо и протяжка мышью в окно не
//!   транслируются;
//! * при downscale текст режется (nearest neighbour, без фильтра) — выбор
//!   политики см. [`fit_placement`].

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::font::{CELL_H, CELL_W};
use crate::keys::{self, KeyModes};
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
}

impl FileInput {
    /// Голый fd для `ioctl` (фокус клавиатуры). `None` после `close` перед
    /// общим близнецом-`respawn` — ioctl тогда просто некуда слать.
    pub fn file(&self) -> Option<&File> {
        self.file.as_ref()
    }
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

/// Раскладка кадра сессии внутри окна: `dst`-прямоугольник и его смещение.
///
/// Политика (одна, документированная, без «умных» эвристик):
///
/// * окно **не меньше** поверхности — блит 1:1 по центру, без ресемплинга
///   (пиксель в пиксель: текст ядра не мылится);
/// * окно **меньше** — downscale с сохранением пропорций (nearest neighbour,
///   целочисленная арифметика) и центрирование; края заливаются фоном темы;
/// * **обрезки нет** никогда: вся поверхность сессии видна целиком.  Обрезка
///   спрятала бы часть вывода shell'а (ячейки 8×18 px, окно на 900×600 почти
///   всегда меньше типичного kernel-fb), а потерянный вывод хуже, чем
///   потерянная строка пикселей;
/// * **апскейла нет**: увеличение окна не растягивает кадр до мыла, а
///   показывает его 1:1 по центру.
pub fn fit_placement(
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Option<FitPlacement> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return None;
    }
    let (width, height) = if src_w <= dst_w && src_h <= dst_h {
        (src_w, src_h)
    } else if (src_w as u64) * (dst_h as u64) >= (src_h as u64) * (dst_w as u64) {
        // Поверхность «шире» окна: упираемся в ширину.
        let height = ((src_h as u64 * dst_w as u64) / src_w as u64) as usize;
        (dst_w, height.clamp(1, dst_h))
    } else {
        let width = ((src_w as u64 * dst_h as u64) / src_h as u64) as usize;
        (width.clamp(1, dst_w), dst_h)
    };
    Some(FitPlacement {
        x: (dst_w - width) / 2,
        y: (dst_h - height) / 2,
        width,
        height,
    })
}

/// Прямоугольник вывода кадра в окне (см. [`fit_placement`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FitPlacement {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

/// Блит XRGB-пикселей ядра в RGBA-канву окна.
///
/// Байты в памяти — `B, G, R, X` (XRGB8888 little-endian, как формат 1 PDP/1).
/// Источник короче объявленного `stride * height` — честная ошибка
/// [`VtFbError::ShortRead`], а не «нарисуем что-нибудь».
pub fn blit_xrgb(
    src: &[u8],
    info: &VtFbInfo,
    canvas: &mut Canvas,
    place: &FitPlacement,
) -> Result<(), VtFbError> {
    let src_w = info.width as usize;
    let src_h = info.height as usize;
    let stride = info.stride as usize;
    if place.width == 0 || place.height == 0 {
        return Ok(());
    }
    for dy in 0..place.height {
        let sy = dy * src_h / place.height;
        let row = sy.saturating_mul(stride);
        for dx in 0..place.width {
            let sx = dx * src_w / place.width;
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
        })
    }

    pub fn info(&self) -> VtFbInfo {
        self.info
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
        Ok(self.info)
    }

    /// Перерисовать окно, если ядро опубликовало новый кадр.
    pub fn draw_if_changed(
        &mut self,
        canvas: &mut Canvas,
        theme: &Theme,
    ) -> Result<Draw, VtFbError> {
        let info = self.source.poll()?;
        if !self.full && Some(info.generation) == self.last_generation && info == self.info {
            return Ok(Draw::Unchanged);
        }
        let Some(place) = fit_placement(
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
/// Знак **не меняется**: `Axis.vertical` — это уже тот `REL_WHEEL`, который
/// ядро выпустило в `/dev/input/event0` из своего PS/2-декодера
/// (`ps2_mouse::decode_packet`), а `/dev/vtmouse{N}` обязан воспроизвести его
/// один в один. Инверсия здесь давала бы колесо наоборот: прокрутка «вверх»
/// листала бы к свежему тексту.
pub fn wheel_from_axis(vertical: i32) -> i32 {
    vertical
}

/// Координаты окна (content rect) → пиксели поверхности сессии.
///
/// Обратное к [`blit_xrgb`]: окно показывает `place`-прямоугольник поверхности,
/// поэтому `sx = (x - place.x) * src_w / place.width`. Точки в letterbox'е
/// (полях) прижимаются к краю: колесо/клик в поле не должны уводить указатель
/// сессии за поверхность. Нулевые размеры (сессия без кадра) дают `(0, 0)`.
pub fn window_to_surface(x: i32, y: i32, info: &VtFbInfo, place: &FitPlacement) -> (i32, i32) {
    let src_w = info.width as i64;
    let src_h = info.height as i64;
    if src_w <= 0 || src_h <= 0 || place.width == 0 || place.height == 0 {
        return (0, 0);
    }
    let rel_x = (x - place.x as i32).clamp(0, place.width.saturating_sub(1) as i32) as i64;
    let rel_y = (y - place.y as i32).clamp(0, place.height.saturating_sub(1) as i32) as i64;
    let sx = rel_x * src_w / place.width as i64;
    let sy = rel_y * src_h / place.height as i64;
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
    fn fit_placement_never_crops_and_never_upscales() {
        // 1:1 (окно больше поверхности) — по центру, без растяжения.
        let place = fit_placement(640, 400, 900, 600).unwrap();
        assert_eq!(
            place,
            FitPlacement {
                x: 130,
                y: 100,
                width: 640,
                height: 400
            }
        );
        // Точно в размер — без смещений.
        let place = fit_placement(640, 400, 640, 400).unwrap();
        assert_eq!(
            place,
            FitPlacement {
                x: 0,
                y: 0,
                width: 640,
                height: 400
            }
        );
        // Окно меньше: пропорциональный downscale, обе оси в пределах окна.
        let place = fit_placement(1024, 768, 900, 600).unwrap();
        assert!(place.width <= 900 && place.height <= 600);
        assert_eq!(place.width, 800);
        assert_eq!(place.height, 600);
        assert_eq!(place.x, 50);
        // Очень широкое окно: упираемся в высоту.
        let place = fit_placement(1000, 100, 400, 400).unwrap();
        assert_eq!(place.height, 40);
        assert_eq!(place.width, 400);
        // Ноль с любой стороны — рисовать нечего, но не паника.
        assert!(fit_placement(0, 10, 10, 10).is_none());
        assert!(fit_placement(10, 10, 0, 10).is_none());
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
