//! Встроенный растровый шрифт и отрисовка текста.
//!
//! `pagh-ui` не тянет зависимостей и не читает TTF в рантайме: глифы лежат в
//! [`crate::font_data`] как 4-битная альфа-карта, сгенерированная из DejaVu Sans
//! 13 px ([`crate::font_data::FONT_NAME`]). Поддерживаются ASCII, русский
//! алфавит, «» и типографские знаки, которые есть в меню Files.

use crate::canvas::Canvas;
use crate::color::Color;
use crate::font_data::{self, Glyph};
use crate::geom::{Point, Rect};

/// Выравнивание текста в прямоугольнике.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

/// Высота базовой линии над верхом строки.
pub fn ascent() -> i32 {
    font_data::ASCENT
}

/// Высота выносных элементов ниже базовой линии.
pub fn descent() -> i32 {
    font_data::DESCENT
}

/// Высота строки, используемая раскладкой.
pub fn line_height() -> i32 {
    font_data::LINE_HEIGHT
}

/// Имя встроенного шрифта.
pub fn name() -> &'static str {
    font_data::FONT_NAME
}

/// Глиф символа; для отсутствующих символов — `None`.
pub fn glyph(ch: char) -> Option<&'static Glyph> {
    font_data::GLYPHS
        .binary_search_by_key(&ch, |g: &Glyph| g.ch)
        .ok()
        .map(|index| &font_data::GLYPHS[index])
}

/// Ширина строки в пикселях (без кернинга, как и при отрисовке).
pub fn measure(text: &str) -> i32 {
    text.chars()
        .map(|ch| glyph_or_fallback(ch).advance as i32)
        .sum()
}

/// Ширина одного символа.
pub fn measure_char(ch: char) -> i32 {
    glyph_or_fallback(ch).advance as i32
}

fn glyph_or_fallback(ch: char) -> &'static Glyph {
    glyph(ch).unwrap_or_else(|| {
        font_data::GLYPHS
            .binary_search_by_key(&'?', |g: &Glyph| g.ch)
            .ok()
            .map(|index| &font_data::GLYPHS[index])
            .expect("шрифт должен содержать '?'")
    })
}

/// Нарисовать строку от базовой линии. Возвращает ширину строки.
pub fn draw(canvas: &mut Canvas, pen_x: i32, baseline: i32, text: &str, color: Color) -> i32 {
    let mut x = pen_x;
    for ch in text.chars() {
        if ch == ' ' || ch == '\u{00a0}' {
            x += glyph_or_fallback(ch).advance as i32;
            continue;
        }
        draw_glyph(canvas, glyph_or_fallback(ch), x, baseline, color);
        x += glyph_or_fallback(ch).advance as i32;
    }
    x - pen_x
}

fn draw_glyph(canvas: &mut Canvas, glyph: &Glyph, pen_x: i32, baseline: i32, color: Color) {
    let width = glyph.width as usize;
    if width == 0 || glyph.height == 0 {
        return;
    }
    let row_bytes = width.div_ceil(2);
    let origin_x = pen_x + glyph.left as i32;
    let origin_y = baseline + glyph.top as i32;
    let alpha = &font_data::ALPHA[glyph.offset as usize..];
    for row in 0..glyph.height as usize {
        let row_start = row * row_bytes;
        for col in 0..width {
            let byte = alpha[row_start + col / 2];
            let nibble = if col % 2 == 0 { byte >> 4 } else { byte & 0x0F };
            if nibble == 0 {
                continue;
            }
            let a = (nibble as u16 * 17) as u8;
            canvas.blend_pixel(
                Point::new(origin_x + col as i32, origin_y + row as i32),
                Color::rgba(
                    color.r,
                    color.g,
                    color.b,
                    ((color.a as u16 * a as u16) / 255) as u8,
                ),
            );
        }
    }
}

/// Базовая линия для вертикально центрированной строки в `rect`.
pub fn baseline_for(rect: Rect) -> i32 {
    rect.y + (rect.h - line_height()) / 2 + ascent()
}

/// Нарисовать строку в прямоугольнике с выравниванием по горизонтали.
pub fn draw_aligned(
    canvas: &mut Canvas,
    rect: Rect,
    text: &str,
    color: Color,
    align: TextAlign,
) -> i32 {
    let width = measure(text);
    let x = match align {
        TextAlign::Left => rect.x,
        TextAlign::Center => rect.x + (rect.w - width) / 2,
        TextAlign::Right => rect.right() - width,
    };
    draw(canvas, x, baseline_for(rect), text, color);
    width
}

/// Строка, обрезанная до ширины с многоточием `…`.
pub fn ellipsize(text: &str, max_width: i32) -> String {
    if max_width <= 0 {
        return String::new();
    }
    if measure(text) <= max_width {
        return text.to_string();
    }
    let ellipsis = '…';
    let ellipsis_w = measure_char(ellipsis);
    if ellipsis_w > max_width {
        return String::new();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let advance = measure_char(ch);
        if width + advance + ellipsis_w > max_width {
            break;
        }
        out.push(ch);
        width += advance;
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out.push(ellipsis);
    out
}

/// Разбить текст на строки по ширине (по словам) и вернуть их.
pub fn wrap(text: &str, max_width: i32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if measure(&candidate) <= max_width || current.is_empty() {
            current = candidate;
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Нарисовать многострочный текст; возвращает занятую высоту.
pub fn draw_wrapped(
    canvas: &mut Canvas,
    rect: Rect,
    text: &str,
    color: Color,
    line_gap: i32,
) -> i32 {
    let mut y = rect.y;
    for line in wrap(text, rect.w) {
        draw(canvas, rect.x, y + ascent(), &line, color);
        y += line_height() + line_gap;
    }
    y - rect.y
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_has_ascii_and_cyrillic() {
        for ch in ['A', 'z', '0', 'Я', 'ж', 'Ё', 'ё', '«', '»', '…', '·'] {
            assert!(glyph(ch).is_some(), "нет глифа для {ch:?}");
        }
        assert_eq!(name(), "DejaVu Sans 13");
    }

    #[test]
    fn measure_is_positive_and_monotonic() {
        assert!(measure("Hello") > 0);
        assert!(measure("Hello, world") > measure("Hello"));
        assert_eq!(measure(""), 0);
        assert!(measure("Привет") > 0);
    }

    #[test]
    fn draw_writes_pixels() {
        let mut canvas = Canvas::new(80, 24);
        let width = draw(&mut canvas, 2, 16, "Files", Color::WHITE);
        assert!(width > 0);
        let ink = (0..80)
            .flat_map(|x| (0..24).map(move |y| Point::new(x, y)))
            .any(|p| canvas.pixel(p).a > 0);
        assert!(ink, "текст не отрисован");
    }

    #[test]
    fn unknown_glyph_falls_back_to_question_mark() {
        assert_eq!(measure("\u{4e00}"), measure_char('?'));
    }

    #[test]
    fn ellipsize_fits_into_width() {
        let full = "Очень длинное название папки";
        let fitted = ellipsize(full, 80);
        assert!(measure(&fitted) <= 80, "fitted = {fitted:?}");
        assert!(fitted.ends_with('…'));
        assert_eq!(ellipsize("ok", 80), "ok");
        assert_eq!(ellipsize(full, 0), "");
    }

    #[test]
    fn wrap_splits_words() {
        let lines = wrap("Открыть терминал в этой папке", 70);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(measure(line) <= 70, "line = {line:?}");
        }
    }
}
