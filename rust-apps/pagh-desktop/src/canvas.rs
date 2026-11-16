//! Программный RGBA-холст: примитивы, текст, тени и блит поверхностей.
//!
//! Холст хранит непрозрачные пиксели `0x00RRGGBB`. Все операции уважают текущую
//! область отсечения (damage): в неё попадают только изменённые прямоугольники, поэтому
//! повторная отрисовка сцены обновляет лишь повреждённые пиксели.

use crate::color::{self, Color, TRANSPARENT};
use crate::font::{GLYPH_HEIGHT, GLYPH_WIDTH};
use crate::geometry::{Point, Rect, Size};
use crate::glyph::GlyphAtlas;
use crate::surface::Surface;

/// Программный холст кадра.
pub struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<Color>,
    clip: Vec<Rect>,
    clip_active: bool,
    glyphs: GlyphAtlas,
}

impl Clone for Canvas {
    fn clone(&self) -> Self {
        // Кэш глифов не клонируем: он нужен только для рисования и быстро
        // восстановится; копия холста используется тестами/снапшотами.
        Self {
            width: self.width,
            height: self.height,
            pixels: self.pixels.clone(),
            clip: self.clip.clone(),
            clip_active: self.clip_active,
            glyphs: GlyphAtlas::default(),
        }
    }
}

impl std::fmt::Debug for Canvas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Canvas")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("clip_active", &self.clip_active)
            .field("clip", &self.clip)
            .field("glyph_misses", &self.glyphs.misses())
            .finish_non_exhaustive()
    }
}

impl Canvas {
    pub fn new(width: u32, height: u32, fill: Color) -> Self {
        Self::with_backing(width, height, fill | 0xFF00_0000)
    }

    /// Прозрачный холст для кэшируемых слоёв: незаписанные пиксели остаются 0.
    pub fn new_transparent(width: u32, height: u32) -> Self {
        Self::with_backing(width, height, 0)
    }

    fn with_backing(width: u32, height: u32, fill: Color) -> Self {
        Self {
            width,
            height,
            pixels: vec![fill; (width as usize) * (height as usize)],
            clip: Vec::new(),
            clip_active: false,
            glyphs: GlyphAtlas::default(),
        }
    }

    /// Статистика glyph-кэша (для тестов и счётчиков).
    pub fn glyph_hits(&self) -> u64 {
        self.glyphs.hits()
    }

    pub fn glyph_misses(&self) -> u64 {
        self.glyphs.misses()
    }

    pub fn glyph_cache_len(&self) -> usize {
        self.glyphs.len()
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width as i32, self.height as i32)
    }

    pub fn pixels(&self) -> &[Color] {
        &self.pixels
    }

    pub fn pixels_mut(&mut self) -> &mut [Color] {
        &mut self.pixels
    }

    /// Ограничить последующую отрисовку набором прямоугольников.
    pub fn set_clip(&mut self, rects: &[Rect]) {
        self.clip.clear();
        for rect in rects {
            if let Some(r) = rect.intersection(self.bounds()) {
                self.clip.push(r);
            }
        }
        self.clip_active = true;
    }

    pub fn clear_clip(&mut self) {
        self.clip_active = false;
        self.clip.clear();
    }

    pub fn clip(&self) -> &[Rect] {
        if self.clip_active {
            &self.clip
        } else {
            &[]
        }
    }

    fn in_clip(&self, x: i32, y: i32) -> bool {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return false;
        }
        if !self.clip_active {
            return true;
        }
        let p = Point::new(x, y);
        self.clip.iter().any(|r| r.contains(p))
    }

    pub fn get(&self, x: i32, y: i32) -> Color {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return TRANSPARENT;
        }
        self.pixels[(y as usize) * (self.width as usize) + (x as usize)]
    }

    pub fn put(&mut self, x: i32, y: i32, color: Color) {
        if self.in_clip(x, y) {
            self.pixels[(y as usize) * (self.width as usize) + (x as usize)] = color | 0xFF00_0000;
        }
    }

    pub fn blend(&mut self, x: i32, y: i32, color: Color, opacity: u8) {
        if !self.in_clip(x, y) {
            return;
        }
        let idx = (y as usize) * (self.width as usize) + (x as usize);
        self.pixels[idx] = color::blend(self.pixels[idx], color, opacity);
    }

    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        self.fill_rect_alpha(rect, color, 255);
    }

    pub fn fill_rect_alpha(&mut self, rect: Rect, color: Color, opacity: u8) {
        let Some(rect) = rect.intersection(self.bounds()) else {
            return;
        };
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                self.blend(x, y, color, opacity);
            }
        }
    }

    /// Вертикальный градиент; `top` в строке `rect.y`, `bottom` — в последней строке.
    pub fn fill_gradient(&mut self, rect: Rect, top: Color, bottom: Color) {
        let Some(rect) = rect.intersection(self.bounds()) else {
            return;
        };
        let span = (rect.h - 1).max(1) as u32;
        for y in rect.y..rect.bottom() {
            let t = (((y - rect.y) as u32 * 255) / span).min(255) as u8;
            let row = color::lerp(top, bottom, t);
            for x in rect.x..rect.right() {
                self.put(x, y, row);
            }
        }
    }

    /// Заливка прямоугольника со скруглёнными углами.
    pub fn fill_rounded_rect(&mut self, rect: Rect, radius: i32, color: Color, opacity: u8) {
        let Some(rect) = rect.intersection(self.bounds()) else {
            return;
        };
        let radius = radius.max(0).min(rect.w / 2).min(rect.h / 2);
        for y in rect.y..rect.bottom() {
            let dy = (y - rect.y).min(rect.bottom() - 1 - y);
            let inset = if dy < radius {
                let r = radius as f64;
                let d = (radius - dy) as f64;
                (r - (r * r - d * d).sqrt()).round() as i32
            } else {
                0
            };
            for x in (rect.x + inset)..(rect.right() - inset) {
                self.blend(x, y, color, opacity);
            }
        }
    }

    /// Контур скруглённого прямоугольника толщиной 1 пиксель.
    pub fn stroke_rounded_rect(&mut self, rect: Rect, radius: i32, color: Color, opacity: u8) {
        if rect.is_empty() {
            return;
        }
        let inner = rect.inset(1);
        for y in rect.y..rect.bottom() {
            let dy = (y - rect.y).min(rect.bottom() - 1 - y);
            let inset = if dy < radius.max(0) {
                let r = radius as f64;
                let d = (radius - dy) as f64;
                (r - (r * r - d * d).sqrt()).round() as i32
            } else {
                0
            };
            for x in [rect.x + inset, rect.right() - 1 - inset] {
                if !inner.contains(Point::new(x, y)) {
                    self.blend(x, y, color, opacity);
                }
            }
        }
        for x in rect.x..rect.right() {
            let dx = (x - rect.x).min(rect.right() - 1 - x);
            let inset = if dx < radius.max(0) {
                let r = radius as f64;
                let d = (radius - dx) as f64;
                (r - (r * r - d * d).sqrt()).round() as i32
            } else {
                0
            };
            for y in [rect.y + inset, rect.bottom() - 1 - inset] {
                if !inner.contains(Point::new(x, y)) {
                    self.blend(x, y, color, opacity);
                }
            }
        }
    }

    /// Мягкая тень под окном/меню: несколько расширяющихся полупрозрачных слоёв.
    pub fn shadow(&mut self, rect: Rect, radius: i32, layers: i32, opacity_step: u8) {
        for layer in (1..=layers.max(1)).rev() {
            let spread = layer;
            let alpha = opacity_step.saturating_sub(layer as u8).max(2);
            let layer_rect = rect.inflate(spread).offset(0, spread / 2 + 2);
            self.fill_rounded_rect(layer_rect, radius + spread, color::BLACK, alpha);
        }
    }

    pub fn hline(&mut self, x0: i32, x1: i32, y: i32, color: Color) {
        if x1 <= x0 {
            return;
        }
        for x in x0..x1 {
            self.blend(x, y, color, 255);
        }
    }

    pub fn vline(&mut self, x: i32, y0: i32, y1: i32, color: Color) {
        if y1 <= y0 {
            return;
        }
        for y in y0..y1 {
            self.blend(x, y, color, 255);
        }
    }

    pub fn line(&mut self, a: Point, b: Point, color: Color, thickness: i32) {
        let (mut x0, mut y0, x1, y1) = (a.x, a.y, b.x, b.y);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let t = thickness.max(1);
        loop {
            for oy in 0..t {
                for ox in 0..t {
                    self.blend(x0 + ox, y0 + oy, color, 255);
                }
            }
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    /// Радиальное свечение с квадратичным затуханием (обои, подсветка).
    pub fn radial_glow(&mut self, center: Point, radius: i32, color: Color, max_alpha: u8) {
        let radius = radius.max(1);
        let r2 = (radius as i64) * (radius as i64);
        for y in (center.y - radius)..=(center.y + radius) {
            for x in (center.x - radius)..=(center.x + radius) {
                let dx = (x - center.x) as i64;
                let dy = (y - center.y) as i64;
                let d2 = dx * dx + dy * dy;
                if d2 > r2 {
                    continue;
                }
                let t = 1.0 - (d2 as f64).sqrt() / radius as f64;
                let alpha = (t * t * max_alpha as f64).round() as u8;
                if alpha > 0 {
                    self.blend(x, y, color, alpha);
                }
            }
        }
    }

    pub fn fill_circle(&mut self, center: Point, radius: i32, color: Color, opacity: u8) {
        let r2 = (radius.max(0) as i64) * (radius.max(0) as i64);
        for y in (center.y - radius)..=(center.y + radius) {
            for x in (center.x - radius)..=(center.x + radius) {
                let dx = (x - center.x) as i64;
                let dy = (y - center.y) as i64;
                if dx * dx + dy * dy <= r2 {
                    self.blend(x, y, color, opacity);
                }
            }
        }
    }

    pub fn circle_outline(&mut self, center: Point, radius: i32, color: Color, thickness: i32) {
        let steps = (radius.max(1) * 8) as usize;
        for i in 0..steps {
            let angle = (i as f64) * std::f64::consts::TAU / steps as f64;
            let x = center.x + (radius as f64 * angle.cos()).round() as i32;
            let y = center.y + (radius as f64 * angle.sin()).round() as i32;
            self.fill_circle(Point::new(x, y), thickness.max(1), color, 255);
        }
    }

    /// Дуга окружности в градусах (0° — вправо, по часовой стрелке, ось Y вниз).
    pub fn arc(
        &mut self,
        center: Point,
        radius: i32,
        start_deg: f64,
        end_deg: f64,
        color: Color,
        thickness: i32,
    ) {
        let span = (end_deg - start_deg).abs();
        let steps = ((span / 6.0).ceil() as i32).max(2);
        let mut prev: Option<Point> = None;
        for i in 0..=steps {
            let t = i as f64 / steps as f64;
            let angle = (start_deg + (end_deg - start_deg) * t).to_radians();
            let p = Point::new(
                center.x + (radius as f64 * angle.cos()).round() as i32,
                center.y + (radius as f64 * angle.sin()).round() as i32,
            );
            if let Some(prev) = prev {
                self.line(prev, p, color, thickness);
            }
            prev = Some(p);
        }
    }

    /// Заливка выпуклого/невыпуклого полигона методом scanline.
    pub fn fill_polygon(&mut self, points: &[Point], color: Color, opacity: u8) {
        if points.len() < 3 {
            return;
        }
        let min_y = points.iter().map(|p| p.y).min().unwrap_or(0);
        let max_y = points.iter().map(|p| p.y).max().unwrap_or(0);
        for y in min_y..=max_y {
            let scan = y as f64 + 0.5;
            let mut xs: Vec<f64> = Vec::new();
            for i in 0..points.len() {
                let a = points[i];
                let b = points[(i + 1) % points.len()];
                let (ay, by) = (a.y as f64, b.y as f64);
                if (ay <= scan && scan < by) || (by <= scan && scan < ay) {
                    let t = (scan - ay) / (by - ay);
                    xs.push(a.x as f64 + t * (b.x - a.x) as f64);
                }
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            for pair in xs.chunks(2) {
                if pair.len() == 2 {
                    let x0 = pair[0].ceil() as i32;
                    let x1 = pair[1].floor() as i32;
                    for x in x0..=x1 {
                        self.blend(x, y, color, opacity);
                    }
                }
            }
        }
    }

    /// Нарисовать строку встроенным шрифтом. Возвращает ширину в пикселях.
    pub fn text(&mut self, x: i32, y: i32, scale: i32, color: Color, text: &str) -> i32 {
        self.text_alpha(x, y, scale, color, 255, text)
    }

    /// То же, но с общей прозрачностью (подписи в неактивных окнах, disabled-пункты меню).
    pub fn text_alpha(
        &mut self,
        x: i32,
        y: i32,
        scale: i32,
        color: Color,
        opacity: u8,
        text: &str,
    ) -> i32 {
        let scale = scale.max(1);
        let mut cursor = x;
        for ch in text.chars() {
            if ch == '\n' {
                cursor = x;
                // Переводы строк обрабатывает draw_text; здесь символ игнорируется.
                continue;
            }
            // ASCII + кириллица; неизвестный символ показываем как `?`.
            // Маска глифа кэшируется («glyph atlas»), runs — готовые горизонтальные
            // отрезки пикселей, без перебора 16×8 бит на каждом кадре.
            let Some(mask) = self.glyphs.mask(ch, scale) else {
                continue;
            };
            for (row, runs) in mask.rows().iter().enumerate() {
                let py = y + row as i32;
                for &(dx, len) in runs {
                    let px = cursor + dx;
                    if opacity == 255 {
                        for i in 0..len {
                            self.put(px + i, py, color);
                        }
                    } else {
                        for i in 0..len {
                            self.blend(px + i, py, color, opacity);
                        }
                    }
                }
            }
            cursor += GLYPH_WIDTH * scale;
        }
        cursor - x
    }

    /// Многострочный текст; `\n` начинает новую строку с тем же отступом.
    pub fn draw_text(&mut self, x: i32, y: i32, scale: i32, color: Color, text: &str) {
        let line_height = GLYPH_HEIGHT * scale.max(1);
        for (line, part) in text.split('\n').enumerate() {
            self.text(x, y + line as i32 * line_height, scale, color, part);
        }
    }

    /// Блит поверхности клиента в прямоугольник назначения (nearest-neighbour).
    ///
    /// Если размеры не совпадают (клиент ещё не прислал кадр под новый configure),
    /// изображение масштабируется; так живой resize выглядит корректно. Для
    /// совпадающих размеров и `opacity=255` (непрозрачный XRGB/RGB565-путь)
    /// строки копируются `copy_from_slice`, без per-pixel blend.
    pub fn blit_surface(&mut self, surface: &Surface, dst: Rect, opacity: u8) {
        let Some(dst) = dst.intersection(self.bounds()) else {
            return;
        };
        if surface.width == 0 || surface.height == 0 {
            return;
        }
        let direct = surface.width == dst.w as u32 && surface.height == dst.h as u32;
        let one_to_one = direct && opacity == 255;
        let src_pixels = surface.pixels();
        let paint = |target: &mut Canvas, area: Rect| {
            if area.is_empty() {
                return;
            }
            if one_to_one {
                let src_x = area.x - dst.x;
                let src_y = area.y - dst.y;
                for row in 0..area.h as usize {
                    let s = (src_y as usize + row) * surface.width as usize + src_x as usize;
                    let d = (area.y as usize + row) * target.width as usize + area.x as usize;
                    target.pixels[d..d + area.w as usize]
                        .copy_from_slice(&src_pixels[s..s + area.w as usize]);
                }
                return;
            }
            for y in area.y..area.bottom() {
                let sy = if direct {
                    (y - dst.y) as u32
                } else {
                    (((y - dst.y) as u64 * surface.height as u64 / dst.h as u64)
                        .min(surface.height as u64 - 1)) as u32
                };
                for x in area.x..area.right() {
                    let sx = if direct {
                        (x - dst.x) as u32
                    } else {
                        (((x - dst.x) as u64 * surface.width as u64 / dst.w as u64)
                            .min(surface.width as u64 - 1)) as u32
                    };
                    // Форматы поверхностей непрозрачные; альфа приходит только из opacity.
                    let src = surface.get(sx, sy) | 0xFF00_0000;
                    if opacity == 255 {
                        target.pixels[(y as usize) * target.width as usize + x as usize] = src;
                    } else {
                        target.blend(x, y, src, opacity);
                    }
                }
            }
        };
        if self.clip_active {
            for index in 0..self.clip.len() {
                let clip = self.clip[index];
                if let Some(area) = dst.intersection(clip) {
                    paint(self, area);
                }
            }
        } else {
            paint(self, dst);
        }
    }

    /// Скопировать непрозрачную область из другого холста (кэш обоев/панели).
    ///
    /// Копирование идёт построчно через `copy_from_slice` и только в пределах
    /// текущего damage-отсечения.
    pub fn blit_opaque(&mut self, src: &Canvas, src_rect: Rect, dst: Point) {
        let Some(src_rect) = src_rect.intersection(src.bounds()) else {
            return;
        };
        let dst_rect = Rect::new(dst.x, dst.y, src_rect.w, src_rect.h);
        let Some(dst_rect) = dst_rect.intersection(self.bounds()) else {
            return;
        };
        let copy = |target: &mut Canvas, area: Rect| {
            let src_x = src_rect.x + (area.x - dst.x);
            let src_y = src_rect.y + (area.y - dst.y);
            for row in 0..area.h as usize {
                let s = (src_y as usize + row) * src.width as usize + src_x as usize;
                let d = (area.y as usize + row) * target.width as usize + area.x as usize;
                target.pixels[d..d + area.w as usize]
                    .copy_from_slice(&src.pixels[s..s + area.w as usize]);
            }
        };
        if self.clip_active {
            for index in 0..self.clip.len() {
                let clip = self.clip[index];
                if let Some(area) = dst_rect.intersection(clip) {
                    copy(self, area);
                }
            }
        } else {
            copy(self, dst_rect);
        }
    }

    /// Наложить кэшированный слой с прозрачными пикселями (`0` = не рисовать).
    pub fn blit_layer(&mut self, layer: &Canvas, dst: Point) {
        let dst_rect = Rect::new(dst.x, dst.y, layer.width as i32, layer.height as i32);
        let Some(dst_rect) = dst_rect.intersection(self.bounds()) else {
            return;
        };
        let copy = |target: &mut Canvas, area: Rect| {
            let layer_x = area.x - dst.x;
            let layer_y = area.y - dst.y;
            for row in 0..area.h as usize {
                let srow = (layer_y as usize + row) * layer.width as usize;
                let drow = (area.y as usize + row) * target.width as usize;
                let mut x = 0usize;
                while x < area.w as usize {
                    let sx = layer_x as usize + x;
                    if layer.pixels[srow + sx] == 0 {
                        x += 1;
                        continue;
                    }
                    let start = x;
                    while x < area.w as usize && layer.pixels[srow + layer_x as usize + x] != 0 {
                        x += 1;
                    }
                    let d = drow + area.x as usize + start;
                    target.pixels[d..d + (x - start)].copy_from_slice(
                        &layer.pixels[srow + layer_x as usize + start..srow + layer_x as usize + x],
                    );
                }
            }
        };
        if self.clip_active {
            for index in 0..self.clip.len() {
                let clip = self.clip[index];
                if let Some(area) = dst_rect.intersection(clip) {
                    copy(self, area);
                }
            }
        } else {
            copy(self, dst_rect);
        }
    }

    /// Скопировать участок холста (используется для проверки damage в тестах).
    pub fn snapshot(&self) -> Canvas {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::rgb;

    #[test]
    fn clip_limits_writes() {
        let mut c = Canvas::new(10, 10, rgb(1, 2, 3));
        c.set_clip(&[Rect::new(2, 2, 3, 3)]);
        c.fill_rect(Rect::new(0, 0, 10, 10), rgb(9, 9, 9));
        assert_eq!(c.get(0, 0), rgb(1, 2, 3));
        assert_eq!(c.get(2, 2), rgb(9, 9, 9));
        assert_eq!(c.get(4, 4), rgb(9, 9, 9));
        assert_eq!(c.get(5, 5), rgb(1, 2, 3));
        c.clear_clip();
        c.put(0, 0, rgb(7, 7, 7));
        assert_eq!(c.get(0, 0), rgb(7, 7, 7));
    }

    #[test]
    fn rounded_rect_clears_corners() {
        let mut c = Canvas::new(20, 20, rgb(0, 0, 0));
        c.fill_rounded_rect(Rect::new(0, 0, 20, 20), 8, rgb(255, 0, 0), 255);
        assert_eq!(c.get(0, 0), rgb(0, 0, 0));
        assert_eq!(c.get(10, 10), rgb(255, 0, 0));
        assert_eq!(c.get(10, 0), rgb(255, 0, 0));
    }

    #[test]
    fn text_draws_pixels() {
        let mut c = Canvas::new(40, 20, rgb(0, 0, 0));
        let w = c.text(2, 2, 1, rgb(255, 255, 255), "A");
        assert_eq!(w, 8);
        let lit = c.pixels().iter().filter(|p| **p != 0).count();
        assert!(lit > 5, "буква A должна оставить пиксели, получено {lit}");
    }

    #[test]
    fn opaque_blit_matches_per_pixel_blend() {
        let source = Canvas::new(8, 6, rgb(10, 20, 30));
        let mut fast = Canvas::new(8, 6, rgb(1, 2, 3));
        let mut slow = Canvas::new(8, 6, rgb(1, 2, 3));
        fast.blit_opaque(&source, source.bounds(), Point::new(0, 0));
        for y in 0..6 {
            for x in 0..8 {
                slow.blend(x, y, rgb(10, 20, 30), 255);
            }
        }
        assert_eq!(fast.pixels(), slow.pixels());
    }

    #[test]
    fn cached_layer_blit_skips_transparent_pixels() {
        let mut layer = Canvas::new_transparent(4, 4);
        layer.put(1, 2, rgb(200, 100, 50));
        let mut canvas = Canvas::new(4, 4, rgb(1, 2, 3));
        canvas.blit_layer(&layer, Point::new(0, 0));
        assert_eq!(canvas.get(1, 2), rgb(200, 100, 50));
        assert_eq!(canvas.get(0, 0), rgb(1, 2, 3));
        assert_eq!(canvas.get(2, 2), rgb(1, 2, 3));
    }

    #[test]
    fn clipped_blit_only_touches_damage() {
        let source = Canvas::new(8, 8, rgb(9, 9, 9));
        let mut canvas = Canvas::new(8, 8, rgb(1, 2, 3));
        canvas.set_clip(&[Rect::new(2, 2, 2, 2)]);
        canvas.blit_opaque(&source, source.bounds(), Point::new(0, 0));
        canvas.clear_clip();
        assert_eq!(canvas.get(3, 3), rgb(9, 9, 9));
        assert_eq!(canvas.get(0, 0), rgb(1, 2, 3));
        assert_eq!(canvas.get(5, 5), rgb(1, 2, 3));
    }

    #[test]
    fn polygon_fills_interior() {
        let mut c = Canvas::new(30, 30, rgb(0, 0, 0));
        c.fill_polygon(
            &[Point::new(5, 5), Point::new(25, 5), Point::new(15, 25)],
            rgb(0, 255, 0),
            255,
        );
        assert_eq!(c.get(15, 10), rgb(0, 255, 0));
        assert_eq!(c.get(2, 2), rgb(0, 0, 0));
    }
}
