//! Клиентская поверхность (кадр окна) и форматы пикселей протокола.

use crate::canvas::Canvas;
use crate::geometry::{Rect, Size};

/// Максимальная сторона поверхности, принимаемая сервером.
pub const MAX_SURFACE_DIM: u32 = 4096;
/// Максимальный stride сверх минимального (выравнивание/паддинг строки).
pub const MAX_STRIDE_PADDING: u32 = 4096;

/// Формат пикселей в `create`/`commit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// `u32` little-endian `0x00RRGGBB`; альфа игнорируется (непрозрачный).
    Xrgb8888,
    /// `u16` little-endian `rrrrrggggggbbbbb`; альфа отсутствует.
    Rgb565,
}

impl PixelFormat {
    pub const XRGB8888_WIRE: u32 = 1;
    pub const RGB565_WIRE: u32 = 2;

    pub fn from_wire(value: u32) -> Option<Self> {
        match value {
            Self::XRGB8888_WIRE => Some(Self::Xrgb8888),
            Self::RGB565_WIRE => Some(Self::Rgb565),
            _ => None,
        }
    }

    pub fn to_wire(self) -> u32 {
        match self {
            Self::Xrgb8888 => Self::XRGB8888_WIRE,
            Self::Rgb565 => Self::RGB565_WIRE,
        }
    }

    pub fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Xrgb8888 => 4,
            Self::Rgb565 => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Xrgb8888 => "xrgb8888",
            Self::Rgb565 => "rgb565",
        }
    }
}

/// Ошибка разбора поверхности.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceError {
    InvalidSize { width: u32, height: u32 },
    UnknownFormat(u32),
    StrideTooSmall { stride: u32, minimum: u32 },
    StrideTooLarge { stride: u32, maximum: u32 },
    DataLength { expected: usize, actual: usize },
}

impl std::fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSize { width, height } => {
                write!(f, "недопустимый размер поверхности {width}x{height}")
            }
            Self::UnknownFormat(format) => write!(f, "неизвестный формат пикселей {format}"),
            Self::StrideTooSmall { stride, minimum } => {
                write!(f, "stride {stride} меньше минимального {minimum}")
            }
            Self::StrideTooLarge { stride, maximum } => {
                write!(f, "stride {stride} больше максимального {maximum}")
            }
            Self::DataLength { expected, actual } => {
                write!(f, "длина кадра {actual}, ожидалось {expected}")
            }
        }
    }
}

impl std::error::Error for SurfaceError {}

/// Кадр клиента: непрозрачные пиксели `0x00RRGGBB` независимо от формата на проводе.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pixels: Vec<u32>,
}

impl Surface {
    pub fn new(width: u32, height: u32, format: PixelFormat) -> Result<Self, SurfaceError> {
        if width == 0 || height == 0 || width > MAX_SURFACE_DIM || height > MAX_SURFACE_DIM {
            return Err(SurfaceError::InvalidSize { width, height });
        }
        Ok(Self {
            width,
            height,
            format,
            pixels: vec![0; (width as usize) * (height as usize)],
        })
    }

    pub fn filled(width: u32, height: u32, format: PixelFormat, color: u32) -> Self {
        let mut surface = Self::new(width, height, format).expect("размер проверен вызывающим");
        surface.pixels.fill((color & 0x00FF_FFFF) | 0xFF00_0000);
        surface
    }

    /// Построить поверхность из холста (используется демо-клиентами и тестами).
    pub fn from_canvas(canvas: &Canvas, format: PixelFormat) -> Self {
        Self {
            width: canvas.width(),
            height: canvas.height(),
            format,
            pixels: canvas
                .pixels()
                .iter()
                .map(|pixel| pixel | 0xFF00_0000)
                .collect(),
        }
    }

    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    pub fn rect(&self) -> Rect {
        Rect::new(0, 0, self.width as i32, self.height as i32)
    }

    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }

    pub fn get(&self, x: u32, y: u32) -> u32 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.pixels[(y as usize) * (self.width as usize) + (x as usize)]
    }

    pub fn set(&mut self, x: u32, y: u32, color: u32) {
        if x < self.width && y < self.height {
            self.pixels[(y as usize) * (self.width as usize) + (x as usize)] =
                (color & 0x00FF_FFFF) | 0xFF00_0000;
        }
    }

    /// Разобрать кадр с провода, проверив размеры, stride и длину данных.
    pub fn from_wire(
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
        data: &[u8],
    ) -> Result<Self, SurfaceError> {
        let format = PixelFormat::from_wire(format).ok_or(SurfaceError::UnknownFormat(format))?;
        if width == 0 || height == 0 || width > MAX_SURFACE_DIM || height > MAX_SURFACE_DIM {
            return Err(SurfaceError::InvalidSize { width, height });
        }
        let bpp = format.bytes_per_pixel();
        let minimum = width.saturating_mul(bpp);
        if stride < minimum {
            return Err(SurfaceError::StrideTooSmall { stride, minimum });
        }
        if stride > minimum.saturating_add(MAX_STRIDE_PADDING) {
            return Err(SurfaceError::StrideTooLarge {
                stride,
                maximum: minimum + MAX_STRIDE_PADDING,
            });
        }
        let expected = (stride as usize).saturating_mul(height as usize);
        if data.len() != expected {
            return Err(SurfaceError::DataLength {
                expected,
                actual: data.len(),
            });
        }
        let mut pixels = vec![0u32; (width as usize) * (height as usize)];
        let row = stride as usize;
        match format {
            PixelFormat::Xrgb8888 => {
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let off = y * row + x * 4;
                        let value = u32::from_le_bytes([
                            data[off],
                            data[off + 1],
                            data[off + 2],
                            data[off + 3],
                        ]);
                        pixels[y * width as usize + x] = (value & 0x00FF_FFFF) | 0xFF00_0000;
                    }
                }
            }
            PixelFormat::Rgb565 => {
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let off = y * row + x * 2;
                        let value = u16::from_le_bytes([data[off], data[off + 1]]);
                        pixels[y * width as usize + x] = rgb565_to_rgb(value) | 0xFF00_0000;
                    }
                }
            }
        }
        Ok(Self {
            width,
            height,
            format,
            pixels,
        })
    }

    /// Сериализовать кадр для `commit` (stride = width * bytes_per_pixel, без паддинга).
    pub fn to_wire(&self) -> Vec<u8> {
        let bpp = self.format.bytes_per_pixel() as usize;
        let mut out = Vec::with_capacity(self.pixels.len() * bpp);
        match self.format {
            PixelFormat::Xrgb8888 => {
                for pixel in &self.pixels {
                    out.extend_from_slice(&(pixel & 0x00FF_FFFF).to_le_bytes());
                }
            }
            PixelFormat::Rgb565 => {
                for pixel in &self.pixels {
                    out.extend_from_slice(&rgb_to_rgb565(*pixel).to_le_bytes());
                }
            }
        }
        out
    }

    pub fn stride(&self) -> u32 {
        self.width * self.format.bytes_per_pixel()
    }
}

pub fn rgb565_to_rgb(value: u16) -> u32 {
    let r5 = ((value >> 11) & 0x1F) as u32;
    let g6 = ((value >> 5) & 0x3F) as u32;
    let b5 = (value & 0x1F) as u32;
    let r = (r5 << 3) | (r5 >> 2);
    let g = (g6 << 2) | (g6 >> 4);
    let b = (b5 << 3) | (b5 >> 2);
    (r << 16) | (g << 8) | b
}

pub fn rgb_to_rgb565(color: u32) -> u16 {
    let r = ((color >> 16) & 0xFF) as u16;
    let g = ((color >> 8) & 0xFF) as u16;
    let b = (color & 0xFF) as u16;
    ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::rgb;

    #[test]
    fn rgb565_roundtrip_is_close() {
        for color in [
            rgb(0, 0, 0),
            rgb(255, 255, 255),
            rgb(31, 63, 31),
            rgb(120, 80, 200),
        ] {
            let decoded = rgb565_to_rgb(rgb_to_rgb565(color));
            for shift in [16, 8, 0] {
                let a = (color >> shift) & 0xFF;
                let b = (decoded >> shift) & 0xFF;
                assert!(a.abs_diff(b) <= 8, "канал {shift}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn xrgb_roundtrip_respects_stride_padding() {
        let data = [
            0x11, 0x22, 0x33, 0x00, 0xAA, 0xBB, 0xCC, 0x00, 0xEE,
            0xEE, // паддинг строки
            0x44, 0x55, 0x66, 0x00, 0x77, 0x88, 0x99, 0x00, 0xEE, 0xEE,
        ];
        let surface = Surface::from_wire(2, 2, 10, PixelFormat::XRGB8888_WIRE, &data).unwrap();
        assert_eq!(surface.get(0, 0), 0xFF33_2211);
        assert_eq!(surface.get(1, 1), 0xFF99_8877);
        let wire = surface.to_wire();
        assert_eq!(wire.len(), 16);
        assert_eq!(&wire[0..4], &[0x11, 0x22, 0x33, 0x00]);
    }

    #[test]
    fn wire_pixels_are_opaque_for_compositing() {
        let data = [0x00, 0x00, 0xFF, 0x00];
        let surface = Surface::from_wire(1, 1, 4, PixelFormat::XRGB8888_WIRE, &data).unwrap();
        assert_eq!(surface.get(0, 0) >> 24, 0xFF);
        assert_eq!(surface.get(0, 0) & 0x00FF_FFFF, 0x00FF_0000);
    }

    #[test]
    fn rejects_bad_layout() {
        assert_eq!(
            Surface::from_wire(0, 10, 40, 1, &[]),
            Err(SurfaceError::InvalidSize {
                width: 0,
                height: 10
            })
        );
        assert_eq!(
            Surface::from_wire(10, 10, 10, 1, &vec![0; 100]),
            Err(SurfaceError::StrideTooSmall {
                stride: 10,
                minimum: 40
            })
        );
        assert_eq!(
            Surface::from_wire(2, 2, 8, 1, &[0; 10]),
            Err(SurfaceError::DataLength {
                expected: 16,
                actual: 10
            })
        );
        assert_eq!(
            Surface::from_wire(1, 1, 4, 99, &[0; 4]),
            Err(SurfaceError::UnknownFormat(99))
        );
    }
}
