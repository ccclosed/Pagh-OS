// Derived from TinyGL 0.4.1 (c) 1997-2022 Fabrice Bellard, MIT — see LICENSE-TinyGL.
//! Пиксельная поверхность, в которую рендерит TinyGL, и общий тип ошибок.
//!
//! TinyGL **не выражает stride**: `zbuffer.c` считает
//! `linesize = (xsize * PSZB + 3) & ~3`, а при `PSZB = 4` (RENDER_BITS 32)
//! это ровно `width * 4` байт. Паддинга, как в PDP/1 (`stride` до `+4096`),
//! быть не может. Поэтому `Surface` принимает только **плотный** буфер:
//! `pixels.len() == width * height`; всё остальное — `FbError` до вызова C,
//! а не молчаливое ужимание/обрезка.
//!
//! Вторая upstream-особенность: `ZB_resize` делает `xsize &= ~3`, то есть
//! ширина, не кратная 4, молча теряет правые пиксели. `Surface::new` отвергает
//! такую ширину заранее.
//!
//! ## Resize
//!
//! Буфер принадлежит вызывающему (`&mut [u32]`), поэтому увеличить его из
//! `Context` нельзя: [`Surface::check_resize`] разрешает только размеры, чья
//! площадь не превышает **исходную** ёмкость буфера. Для настоящего роста
//! (новый memfd на большее окно) нужен новый `Surface` + `Context` — именно
//! так работает путь композитора (спека §4.4); `ZB_resize` здесь — для
//! уменьшения/смены формы в пределах той же памяти.

use std::fmt;

/// Предел стороны поверхности. Совпадает с `MAX_SURFACE_DIM` композитора
/// (`rust-apps/pagh-desktop/src/surface.rs`), чтобы GL-клиент не мог заказать
/// окно, которое композитор всё равно отвергнет.
pub const MAX_SURFACE_DIM: u32 = 4096;

/// Ошибка валидации поверхности, состояния и параметров GL-обёртки. Все
/// варианты — **наши** отказы до опасного входа в C: ни один не полагается на
/// `gl_fatal_error`/`exit(1)` и не приводит к C-`assert`.
///
/// Имя `FbError` историческое (PR1 проверял только framebuffer); в PR2 тип
/// стал общим для крейта.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FbError {
    /// Ширина не кратна 4 (TinyGL `ZB_resize` молча срезал бы `xsize & ~3`).
    WidthNotMultipleOfFour { width: u32 },
    /// Нулевая или слишком большая сторона (`> MAX_SURFACE_DIM`).
    BadDimensions { width: u32, height: u32 },
    /// `pixels.len() != width * height` — буфер не плотный (`stride != w*4`).
    BufferLenMismatch { expected: usize, actual: usize },
    /// `resize` в площадь больше исходного буфера: памяти на такой размер нет.
    ResizeTooSmall { required: usize, available: usize },
    /// `glViewport` с нулевой/слишком большой стороной (upstream — `exit(1)`).
    BadViewport { width: u32, height: u32 },
    /// Матричный стек исчерпан (upstream `glPushMatrix` — C-`assert`).
    MatrixStackOverflow { mode: &'static str },
    /// `glPopMatrix` на пустом стеке (upstream — C-`assert`).
    MatrixStackUnderflow { mode: &'static str },
    /// NaN/∞ в валидируемом параметре (матрица, трансформация, projection):
    /// C-каст float→int — UB. Вершины/цвета на горячем пути проверяются
    /// `debug_assert!` (см. README, «ограничения»).
    NonFinite { what: &'static str },
    /// Вырожденный `glFrustum` (нулевой размер/неположительный near).
    BadProjection { reason: &'static str },
    /// Невалидные параметры `perspective` (наш GLU-эквивалент).
    BadPerspective { reason: &'static str },
    /// `begin` при уже открытом `glBegin` (upstream — C-`assert`).
    BeginNested,
    /// `end` без `begin` (upstream — C-`assert`).
    EndWithoutBegin,
    /// `ZB_open` вернул `NULL` (не удалось выделить Z-буфер).
    OpenFailed,
    /// В процессе уже живёт `Context`: `gl_ctx` у TinyGL глобальный.
    ContextAlreadyLive,
}

impl fmt::Display for FbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FbError::WidthNotMultipleOfFour { width } => write!(
                f,
                "surface width {width} must be a multiple of 4 (TinyGL truncates xsize & ~3)"
            ),
            FbError::BadDimensions { width, height } => write!(
                f,
                "surface dimensions {width}x{height} must be in 1..={MAX_SURFACE_DIM}"
            ),
            FbError::BufferLenMismatch { expected, actual } => write!(
                f,
                "surface buffer has {actual} pixels, expected exactly {expected} (stride must be width*4)"
            ),
            FbError::ResizeTooSmall {
                required,
                available,
            } => write!(
                f,
                "resize needs {required} pixels but the existing buffer holds {available}; \
                 grow via a new Surface/Context (new memfd), not ZB_resize"
            ),
            FbError::BadViewport { width, height } => write!(
                f,
                "viewport {width}x{height} must be in 1..={MAX_SURFACE_DIM} (upstream glViewport exits on <= 0)"
            ),
            FbError::MatrixStackOverflow { mode } => {
                write!(f, "{mode} matrix stack is full; pop before push")
            }
            FbError::MatrixStackUnderflow { mode } => {
                write!(f, "{mode} matrix stack is empty; push before pop")
            }
            FbError::NonFinite { what } => {
                write!(f, "{what} contains NaN or infinity (C float→int cast is UB)")
            }
            FbError::BadProjection { reason } => write!(f, "invalid frustum: {reason}"),
            FbError::BadPerspective { reason } => write!(f, "invalid perspective: {reason}"),
            FbError::BeginNested => write!(f, "glBegin is already open; glEnd it first"),
            FbError::EndWithoutBegin => write!(f, "glEnd without a matching glBegin"),
            FbError::OpenFailed => write!(f, "ZB_open failed (could not allocate Z buffer)"),
            FbError::ContextAlreadyLive => write!(
                f,
                "another TinyGL Context is alive in this process (gl_ctx is global, no Send/Sync)"
            ),
        }
    }
}

impl std::error::Error for FbError {}

/// Плотная пиксельная поверхность `0x00RRGGBB` (XRGB8888), принадлежащая
/// вызывающему. `Context` держит `&mut` на неё, пока жив, поэтому перевыделить
/// буфер под ним безопасно нельзя.
pub struct Surface<'a> {
    pixels: &'a mut [u32],
    width: u32,
    height: u32,
}

impl<'a> Surface<'a> {
    /// Проверить буфер и размеры. `pixels.len()` обязан быть ровно `w*h`.
    pub fn new(pixels: &'a mut [u32], width: u32, height: u32) -> Result<Self, FbError> {
        validate_dimensions(width, height)?;
        let expected = (width as usize) * (height as usize);
        if pixels.len() != expected {
            return Err(FbError::BufferLenMismatch {
                expected,
                actual: pixels.len(),
            });
        }
        Ok(Surface {
            pixels,
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

    /// Плотный stride в байтах — всегда `width * 4`.
    pub fn stride_bytes(&self) -> usize {
        (self.width as usize) * 4
    }

    /// Видимая площадь в пикселях. После `resize` меньше исходной ёмкости
    /// `pixels`: хвост буфера остаётся у вызывающего, но GL его не видит.
    pub fn area(&self) -> usize {
        (self.width as usize) * (self.height as usize)
    }

    /// Пиксели видимой области (ровно `width * height`).
    pub fn pixels(&self) -> &[u32] {
        &self.pixels[..self.area()]
    }

    /// Изменяемая видимая область.
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        let area = self.area();
        &mut self.pixels[..area]
    }

    /// Проверить размеры для [`Context::resize`](crate::Context::resize), ничего
    /// не меняя. Площадь не может превышать исходную ёмкость буфера.
    pub(crate) fn check_resize(&self, width: u32, height: u32) -> Result<(), FbError> {
        validate_dimensions(width, height)?;
        let required = (width as usize) * (height as usize);
        if required > self.pixels.len() {
            return Err(FbError::ResizeTooSmall {
                required,
                available: self.pixels.len(),
            });
        }
        Ok(())
    }

    /// Применить уже проверенный `check_resize`.
    pub(crate) fn apply_resize(&mut self, width: u32, height: u32) {
        debug_assert!((width as usize) * (height as usize) <= self.pixels.len());
        self.width = width;
        self.height = height;
    }

    /// Указатель для `ZB_open`/`ZB_resize`; C пишет в этот буфер, пока
    /// `Surface` жив.
    pub(crate) fn as_mut_ptr(&mut self) -> *mut u32 {
        self.pixels.as_mut_ptr()
    }
}

/// Общая проверка размеров: `1..=MAX_SURFACE_DIM`, ширина кратна 4.
fn validate_dimensions(width: u32, height: u32) -> Result<(), FbError> {
    if width == 0 || height == 0 || width > MAX_SURFACE_DIM || height > MAX_SURFACE_DIM {
        return Err(FbError::BadDimensions { width, height });
    }
    if !width.is_multiple_of(4) {
        return Err(FbError::WidthNotMultipleOfFour { width });
    }
    Ok(())
}

impl fmt::Debug for Surface<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Surface")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride_bytes", &self.stride_bytes())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_width_not_multiple_of_four() {
        let mut fb = vec![0u32; 65 * 64];
        assert_eq!(
            Surface::new(&mut fb, 65, 64).unwrap_err(),
            FbError::WidthNotMultipleOfFour { width: 65 }
        );
    }

    #[test]
    fn rejects_zero_and_huge_dimensions() {
        let mut fb = vec![0u32; 0];
        assert_eq!(
            Surface::new(&mut fb, 0, 64).unwrap_err(),
            FbError::BadDimensions {
                width: 0,
                height: 64
            }
        );
        let mut fb = vec![0u32; 4 * 4];
        assert_eq!(
            Surface::new(&mut fb, MAX_SURFACE_DIM + 4, 4).unwrap_err(),
            FbError::BadDimensions {
                width: MAX_SURFACE_DIM + 4,
                height: 4
            }
        );
    }

    #[test]
    fn rejects_small_buffer() {
        let mut fb = vec![0u32; 64 * 63];
        assert_eq!(
            Surface::new(&mut fb, 64, 64).unwrap_err(),
            FbError::BufferLenMismatch {
                expected: 64 * 64,
                actual: 64 * 63
            }
        );
    }

    #[test]
    fn rejects_padded_stride() {
        // PDP/1 разрешает stride > w*4, TinyGL — нет: лишние пиксели в строке
        // он не увидит, поэтому такой буфер отвергается, а не «ужимается».
        let mut fb = vec![0u32; 64 * 64 + 64];
        assert_eq!(
            Surface::new(&mut fb, 64, 64).unwrap_err(),
            FbError::BufferLenMismatch {
                expected: 64 * 64,
                actual: 64 * 64 + 64
            }
        );
    }

    #[test]
    fn accepts_dense_buffer() {
        let mut fb = vec![0u32; 64 * 64];
        let surface = Surface::new(&mut fb, 64, 64).expect("dense 64x64");
        assert_eq!(surface.stride_bytes(), 256);
        assert_eq!(surface.area(), 64 * 64);
    }

    #[test]
    fn resize_stays_within_capacity() {
        let mut fb = vec![0u32; 64 * 64];
        let mut surface = Surface::new(&mut fb, 64, 64).expect("dense 64x64");
        assert_eq!(surface.check_resize(32, 64), Ok(()));
        surface.apply_resize(32, 64);
        assert_eq!(surface.width(), 32);
        assert_eq!(surface.pixels().len(), 32 * 64);

        assert_eq!(
            surface.check_resize(128, 64),
            Err(FbError::ResizeTooSmall {
                required: 128 * 64,
                available: 64 * 64
            })
        );
        assert_eq!(
            surface.check_resize(33, 16),
            Err(FbError::WidthNotMultipleOfFour { width: 33 })
        );
    }
}
