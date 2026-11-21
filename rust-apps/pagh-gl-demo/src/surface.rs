//! Клиентская разделяемая поверхность: `memfd` + `mmap(MAP_SHARED)` через
//! `pagh_ui::shm` (клиентская половина `ShmSurface`).
//!
//! Поверхность — плотный `XRGB8888` (`stride == width*4`), ровно тот формат,
//! который пишет TinyGL (`0x00RRGGBB` little-endian). Поэтому кадр рендерится
//! **прямо в mmap**: `ZB_open(..., frame_buffer)` получает указатель на это
//! отображение, копии кадра нет, композитору едут только `COMMIT`+`DAMAGE`.

use std::io;

use pagh_ui::protocol::FrameFormat;
use pagh_ui::shm::{MappedBuffer, ShmSurface};

/// memfd-поверхность окна: memfd + mmap + геометрия.
///
/// Живёт в демо, а не в `pagh-ui`: `ShmFrame` тулкита оборачивает отображение
/// в `Canvas` (RGBA-примитивы), а GL нужен сырой указатель на пиксели.
pub struct SharedFrame {
    surface: ShmSurface,
    mapping: MappedBuffer,
    width: u32,
    height: u32,
}

impl SharedFrame {
    /// Создать `memfd`, `ftruncate(w*h*4)` и `mmap(MAP_SHARED)`.
    pub fn create(name: &str, width: u32, height: u32) -> io::Result<SharedFrame> {
        if width == 0 || height == 0 || width % 4 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("GL surface needs width % 4 == 0 and non-zero size; got {width}x{height}"),
            ));
        }
        let surface = ShmSurface::create(name, width, height, FrameFormat::Xrgb8888)?;
        let mapping = surface.map()?;
        Ok(SharedFrame {
            surface,
            mapping,
            width,
            height,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn stride(&self) -> u32 {
        self.surface.stride()
    }

    pub fn fd(&self) -> std::os::fd::RawFd {
        self.surface.fd()
    }

    /// Пиксели поверхности как `u32` (`0x00RRGGBB`).
    ///
    /// # Safety-инвариант (проверен конструктором и типом `ShmSurface`)
    ///
    /// * `mmap` выравнивает начало на страницу, поэтому `u32`-выравнивание есть;
    /// * `stride == width*4`, `len == stride*height == width*height*4` — делится
    ///   на 4 без остатка;
    /// * `&mut self` даёт эксклюзивный доступ на время borrow, а `MappedBuffer`
    ///   не даёт создать второе отображение того же объекта в этом типе.
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        let bytes = self.mapping.as_mut_slice();
        debug_assert_eq!(bytes.len() % 4, 0);
        let len = bytes.len() / 4;
        // SAFETY: см. инвариант в doc-комментарии: выравнивание mmap,
        // плотный stride и эксклюзивность `&mut self`.
        unsafe { std::slice::from_raw_parts_mut(bytes.as_mut_ptr().cast::<u32>(), len) }
    }

    /// Только чтение пикселей (для дайджеста/дампа после рендера).
    pub fn pixels(&self) -> &[u32] {
        let bytes = self.mapping.as_slice();
        let len = bytes.len() / 4;
        // SAFETY: mapping выровнен, длина кратна 4; `&self` не даёт писать.
        unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u32>(), len) }
    }
}
