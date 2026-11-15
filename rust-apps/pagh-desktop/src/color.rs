//! Цвета и программное альфа-смешивание.
//!
//! Внутренний формат пикселя — `0xAARRGGBB`; итоговый кадр непрозрачный, поэтому альфа
//! результата всегда 255. Цвета интерфейса задаются в [`theme`](crate::theme).

/// Упакованный цвет `0xAARRGGBB`.
pub type Color = u32;

pub const TRANSPARENT: Color = 0x0000_0000;
pub const BLACK: Color = 0xFF00_0000;
pub const WHITE: Color = 0xFFFF_FFFF;

pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
    0xFF00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

pub fn red(color: Color) -> u8 {
    ((color >> 16) & 0xFF) as u8
}

pub fn green(color: Color) -> u8 {
    ((color >> 8) & 0xFF) as u8
}

pub fn blue(color: Color) -> u8 {
    (color & 0xFF) as u8
}

pub fn alpha(color: Color) -> u8 {
    ((color >> 24) & 0xFF) as u8
}

/// Линейная интерполяция двух цветов; `t` — 0..=255.
pub fn lerp(a: Color, b: Color, t: u8) -> Color {
    if t == 0 {
        return a;
    }
    if t == 255 {
        return b;
    }
    let t = t as u32;
    let inv = 255 - t;
    let mix = |sa: u32, sb: u32| (sa * inv + sb * t + 127) / 255;
    rgba(
        mix(red(a) as u32, red(b) as u32) as u8,
        mix(green(a) as u32, green(b) as u32) as u8,
        mix(blue(a) as u32, blue(b) as u32) as u8,
        mix(alpha(a) as u32, alpha(b) as u32) as u8,
    )
}

/// Наложить `src` на непрозрачный `dst`; возвращает непрозрачный результат.
///
/// `opacity` (0..=255) дополнительно масштабирует альфу источника — так рисуются
/// полупрозрачные тени, подсветка и всплывающие панели.
pub fn blend(dst: Color, src: Color, opacity: u8) -> Color {
    let sa = ((alpha(src) as u32 * opacity as u32) + 127) / 255;
    if sa == 0 {
        return dst;
    }
    if sa == 255 && opacity == 255 {
        return src | 0xFF00_0000;
    }
    let inv = 255 - sa;
    let mix = |d: u8, s: u8| ((s as u32 * sa + d as u32 * inv + 127) / 255) as u8;
    rgb(
        mix(red(dst), red(src)),
        mix(green(dst), green(src)),
        mix(blue(dst), blue(src)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_opaque_source() {
        let dst = rgb(10, 20, 30);
        let src = rgb(200, 100, 50);
        assert_eq!(blend(dst, src, 255), src);
    }

    #[test]
    fn blend_half_alpha() {
        let dst = BLACK;
        let src = rgba(255, 255, 255, 128);
        let out = blend(dst, src, 255);
        assert_eq!(red(out), 128);
        assert_eq!(green(out), 128);
        assert_eq!(blue(out), 128);
    }

    #[test]
    fn lerp_endpoints() {
        let a = rgb(0, 0, 0);
        let b = rgb(255, 10, 20);
        assert_eq!(lerp(a, b, 0), a);
        assert_eq!(lerp(a, b, 255), b);
        assert_eq!(lerp(a, b, 128), rgb(128, 5, 10));
    }
}
