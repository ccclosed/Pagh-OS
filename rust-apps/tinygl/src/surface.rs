// Derived from TinyGL 0.4.1 (c) 1997-2022 Fabrice Bellard, MIT — see LICENSE-TinyGL.
//! Пиксельная поверхность, в которую рендерит TinyGL.
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

use std::fmt;

/// Предел стороны поверхности. Совпадает с `MAX_SURFACE_DIM` композитора
/// (`rust-apps/pagh-desktop/src/surface.rs`), чтобы GL-клиент не мог заказать
/// окно, которое композитор всё равно отвергнет.
pub const MAX_SURFACE_DIM: u32 = 4096;

/// Ошибка валидации поверхности/контекста. Все варианты — **наши** отказы до
/// входа в C: ни один не полагается на `gl_fatal_error`/`exit(1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FbError {
    /// Ширина не кратна 4 (TinyGL `ZB_resize` молча срезал бы `xsize & ~3`).
    WidthNotMultipleOfFour { width: u32 },
    /// Нулевая или слишком большая сторона (`> MAX_SURFACE_DIM`).
    BadDimensions { width: u32, height: u32 },
    /// `pixels.len() != width * height` — буфер не плотный (`stride != w*4`).
    BufferLenMismatch { expected: usize, actual: usize },
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
        if width == 0 || height == 0 || width > MAX_SURFACE_DIM || height > MAX_SURFACE_DIM {
            return Err(FbError::BadDimensions { width, height });
        }
        if !width.is_multiple_of(4) {
            return Err(FbError::WidthNotMultipleOfFour { width });
        }
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

    pub fn pixels(&self) -> &[u32] {
        self.pixels
    }

    pub fn pixels_mut(&mut self) -> &mut [u32] {
        self.pixels
    }

    /// Указатель для `ZB_open`; C пишет в этот буфер, пока `Surface` жив.
    pub(crate) fn as_mut_ptr(&mut self) -> *mut u32 {
        self.pixels.as_mut_ptr()
    }
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
    }
}
