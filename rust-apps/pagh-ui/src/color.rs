//! Цвет: упаковка RGBA, смешивание и интерполяция.
//!
//! Пиксели поверхностей хранятся как straight-alpha RGBA8888 (не premultiplied):
//! так проще описывать полупрозрачные меню/тени и отдавать буфер в протокол.

/// Цвет RGBA8888 с обычной (straight) альфой.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Color = Color::rgba(0, 0, 0, 0);
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const WHITE: Color = Color::rgb(255, 255, 255);

    /// Непрозрачный цвет.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r, g, b, a }
    }

    /// Цвет из `0xRRGGBB`.
    pub const fn hex(value: u32) -> Color {
        Color::rgb(
            ((value >> 16) & 0xFF) as u8,
            ((value >> 8) & 0xFF) as u8,
            (value & 0xFF) as u8,
        )
    }

    /// Цвет из `0xRRGGBBAA`.
    pub const fn hexa(value: u32) -> Color {
        Color::rgba(
            ((value >> 24) & 0xFF) as u8,
            ((value >> 16) & 0xFF) as u8,
            ((value >> 8) & 0xFF) as u8,
            (value & 0xFF) as u8,
        )
    }

    /// Тот же цвет с другой альфой.
    pub const fn with_alpha(self, a: u8) -> Color {
        Color { a, ..self }
    }

    /// Полупрозрачная версия: `a` — альфа в процентах (0..=100).
    pub const fn with_opacity(self, percent: u8) -> Color {
        let a = (percent as u32 * self.a as u32 / 100) as u8;
        Color { a, ..self }
    }

    /// Упаковка в `0xAARRGGBB` (формат XRGB8888/ARGB8888 для протокола).
    pub const fn to_argb32(self) -> u32 {
        (self.a as u32) << 24 | (self.r as u32) << 16 | (self.g as u32) << 8 | self.b as u32
    }

    /// Упаковка в `0x00RRGGBB`.
    pub const fn to_rgb32(self) -> u32 {
        (self.r as u32) << 16 | (self.g as u32) << 8 | self.b as u32
    }

    pub const fn is_transparent(self) -> bool {
        self.a == 0
    }

    /// Смешивание `self` поверх `dst` (обе straight-alpha).
    ///
    /// Результат — тоже straight-alpha: `a = sa + da*(1-sa)`.
    pub fn blend_over(self, dst: Color) -> Color {
        if self.a == 255 {
            return self;
        }
        if dst.a == 0 {
            return self;
        }
        let sa = self.a as u32;
        let da = dst.a as u32;
        let out_a = sa + da * (255 - sa) / 255;
        if out_a == 0 {
            return Color::TRANSPARENT;
        }
        let mix = |s: u8, d: u8| -> u8 {
            let s = s as u32 * sa;
            let d = d as u32 * da * (255 - sa) / 255;
            (((s + d) + out_a / 2) / out_a) as u8
        };
        Color::rgba(
            mix(self.r, dst.r),
            mix(self.g, dst.g),
            mix(self.b, dst.b),
            out_a as u8,
        )
    }

    /// Интерполяция двух цветов: `t` = 0 даёт `self`, 255 — `other`.
    pub fn lerp(self, other: Color, t: u8) -> Color {
        let t = t as u32;
        let inv = 255 - t;
        let f = |a: u8, b: u8| -> u8 { ((a as u32 * inv + b as u32 * t) / 255) as u8 };
        Color::rgba(
            f(self.r, other.r),
            f(self.g, other.g),
            f(self.b, other.b),
            f(self.a, other.a),
        )
    }

    /// Превращает straight-alpha цвет в premultiplied для отдачи в framebuffer.
    pub fn premultiplied(self) -> Color {
        let a = self.a as u32;
        Color::rgba(
            (self.r as u32 * a / 255) as u8,
            (self.g as u32 * a / 255) as u8,
            (self.b as u32 * a / 255) as u8,
            self.a,
        )
    }

    /// Цвет из байтов RGBA.
    pub fn from_rgba_bytes(bytes: [u8; 4]) -> Color {
        Color::rgba(bytes[0], bytes[1], bytes[2], bytes[3])
    }

    pub fn to_rgba_bytes(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_source_replaces_destination() {
        let red = Color::rgb(255, 0, 0);
        let blue = Color::rgb(0, 0, 255);
        assert_eq!(red.blend_over(blue), red);
    }

    #[test]
    fn transparent_source_keeps_destination() {
        let blue = Color::rgb(0, 0, 255);
        assert_eq!(Color::TRANSPARENT.blend_over(blue), blue);
    }

    #[test]
    fn half_alpha_over_black_is_half_intensity() {
        let src = Color::rgba(255, 255, 255, 128);
        let out = src.blend_over(Color::BLACK);
        assert_eq!(out.a, 255);
        assert!((126..=129).contains(&out.r), "blended red = {}", out.r);
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(Color::hex(0x3788c8), Color::rgb(0x37, 0x88, 0xc8));
        assert_eq!(Color::hexa(0x102c30ff), Color::rgb(0x10, 0x2c, 0x30));
        assert_eq!(Color::hexa(0x102c3000).a, 0);
    }

    #[test]
    fn lerp_endpoints() {
        let a = Color::rgb(0, 0, 0);
        let b = Color::rgb(200, 100, 50);
        assert_eq!(a.lerp(b, 0), a);
        assert_eq!(a.lerp(b, 255), b);
        let mid = a.lerp(b, 128);
        assert!((98..=102).contains(&mid.r));
    }

    #[test]
    fn argb_packing() {
        let c = Color::rgba(0x11, 0x22, 0x33, 0x44);
        assert_eq!(c.to_argb32(), 0x44112233);
        assert_eq!(c.to_rgb32(), 0x00112233);
    }
}
