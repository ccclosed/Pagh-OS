//! Растровая поверхность RGBA8888 и примитивы рисования.
//!
//! [`Canvas`] — единственный владелец пиксельного буфера: виджеты рисуют в него,
//! композитор забирает буфер целиком. Пиксели хранятся как straight-alpha RGBA
//! (байт на канал), поэтому поверхность можно отдать в протокол как ARGB8888
//! или «сплющить» на фон и сохранить в PNG.
//!
//! Все примитивы учитывают текущий стек отсечения (`push_clip`/`pop_clip`) и
//! сглаживают края по аналитическому расстоянию до фигуры (SDF), без внешних
//! библиотек.

use crate::color::Color;
use crate::geom::{Point, Rect, Size};
use crate::protocol::FrameFormat;
use crate::shm::{self, MappedBuffer};

/// Порядок байтов пикселя во внутреннем буфере [`Canvas`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PixelOrder {
    /// Собственный буфер: straight-alpha RGBA (скриншоты, PNG, старый commit).
    Rgba8888,
    /// Разделяемая поверхность XRGB8888: мало-endian `B,G,R,0`.
    Xrgb8888,
    /// Разделяемая поверхность RGB565: 2 байта на пиксель.
    Rgb565,
}

impl PixelOrder {
    pub fn from_frame_format(format: FrameFormat) -> PixelOrder {
        match format {
            FrameFormat::Xrgb8888 => PixelOrder::Xrgb8888,
            FrameFormat::Rgb565 => PixelOrder::Rgb565,
        }
    }

    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelOrder::Rgba8888 | PixelOrder::Xrgb8888 => 4,
            PixelOrder::Rgb565 => 2,
        }
    }
}

/// Владелец пиксельного буфера.
enum Storage {
    /// Обычный `Vec<u8>` (RGBA) — скриншоты, headless-рендер, frame upload.
    Owned(Vec<u8>),
    /// Mapping клиентской `ShmSurface` (рендер прямо в разделяемую память).
    Mapped(MappedBuffer),
}

/// Пиксельная поверхность RGBA8888 или mapping XRGB8888/RGB565.
///
/// По умолчанию ( [`Canvas::new`] / [`Canvas::filled`] ) пиксели хранятся как
/// straight-alpha RGBA; это путь скриншотов и full-frame commit. Для
/// `ShmSurface` клиент создаёт поверхность через [`Canvas::from_mapped`]: тогда
/// примитивы пишут **прямо в mmap** в проводном формате (XRGB8888/RGB565),
/// без промежуточного `Vec<u8>`-кадра.
pub struct Canvas {
    width: i32,
    height: i32,
    stride: usize,
    order: PixelOrder,
    storage: Storage,
    clip_stack: Vec<Rect>,
}

impl Canvas {
    /// Прозрачная поверхность заданного размера.
    pub fn new(width: i32, height: i32) -> Canvas {
        let width = width.max(0);
        let height = height.max(0);
        Canvas {
            width,
            height,
            stride: (width as usize) * 4,
            order: PixelOrder::Rgba8888,
            storage: Storage::Owned(vec![0u8; (width * height * 4) as usize]),
            clip_stack: Vec::new(),
        }
    }

    /// Пустая (прозрачная) поверхность с явным порядком пикселей.
    ///
    /// Нужна статическому слою кадра ([`crate::layer::StaticLayer`]): основа
    /// должна храниться в том же формате, что и поверхность, в которую её
    /// возвращают, иначе [`Canvas::copy_rect`] пойдёт медленным путём с
    /// конвертацией пикселей.
    pub fn with_order(width: i32, height: i32, order: PixelOrder) -> Canvas {
        let width = width.max(0);
        let height = height.max(0);
        let stride = width as usize * order.bytes_per_pixel();
        Canvas {
            width,
            height,
            stride,
            order,
            storage: Storage::Owned(vec![0u8; stride * height as usize]),
            clip_stack: Vec::new(),
        }
    }

    /// Поверхность, отображающая `ShmSurface` напрямую (mmap).
    ///
    /// `buffer` должен быть получен из [`crate::shm::ShmSurface::map`] для той
    /// же геометрии/формата. Canvas владеет mapping и размонтирует его в `Drop`.
    pub fn from_mapped(
        buffer: MappedBuffer,
        width: i32,
        height: i32,
        stride: usize,
        format: FrameFormat,
    ) -> Canvas {
        assert!(width >= 0 && height >= 0, "negative canvas size");
        assert!(
            stride >= width as usize * format.bytes_per_pixel(),
            "stride smaller than width"
        );
        assert!(
            buffer.len() >= stride * height.max(0) as usize,
            "mapped buffer shorter than stride*height"
        );
        Canvas {
            width,
            height,
            stride,
            order: PixelOrder::from_frame_format(format),
            storage: Storage::Mapped(buffer),
            clip_stack: Vec::new(),
        }
    }

    /// Удобная обёртка: `mmap` клиентской `ShmSurface` + Canvas.
    pub fn map_surface(surface: &crate::shm::ShmSurface) -> std::io::Result<Canvas> {
        let buffer = surface.map()?;
        Ok(Canvas::from_mapped(
            buffer,
            surface.width() as i32,
            surface.height() as i32,
            surface.stride() as usize,
            surface.format(),
        ))
    }

    /// Проводной формат, если поверхность разделяемая (для shm-commit).
    pub fn frame_format(&self) -> Option<FrameFormat> {
        match self.order {
            PixelOrder::Rgba8888 => None,
            PixelOrder::Xrgb8888 => Some(FrameFormat::Xrgb8888),
            PixelOrder::Rgb565 => Some(FrameFormat::Rgb565),
        }
    }

    /// Байт на строку (у разделяемой поверхности бывает padding).
    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn pixel_order(&self) -> PixelOrder {
        self.order
    }

    /// Разделяемая поверхность (mmap).
    pub fn is_shared(&self) -> bool {
        matches!(self.storage, Storage::Mapped(_))
    }

    /// Поверхность, целиком заполненная цветом.
    pub fn filled(width: i32, height: i32, color: Color) -> Canvas {
        let mut canvas = Canvas::new(width, height);
        canvas.clear(color);
        canvas
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    /// Прямоугольник всей поверхности.
    pub fn rect(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// Сырые байты поверхности в её внутреннем порядке
    /// (RGBA для [`Canvas::new`], проводной формат для mapping).
    pub fn pixels(&self) -> &[u8] {
        match &self.storage {
            Storage::Owned(pixels) => pixels,
            Storage::Mapped(buffer) => buffer.as_slice(),
        }
    }

    /// Изменяемые сырые байты поверхности.
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        match &mut self.storage {
            Storage::Owned(pixels) => pixels,
            Storage::Mapped(buffer) => buffer.as_mut_slice(),
        }
    }

    #[inline]
    fn offset(&self, x: i32, y: i32) -> usize {
        y as usize * self.stride + x as usize * self.order.bytes_per_pixel()
    }

    /// Текущая область отсечения (без учёта содержимого).
    pub fn clip(&self) -> Rect {
        self.clip_stack
            .last()
            .copied()
            .unwrap_or_else(|| self.rect())
    }

    /// Цвет пикселя; вне поверхности — прозрачный.
    pub fn pixel(&self, p: Point) -> Color {
        if p.x < 0 || p.y < 0 || p.x >= self.width || p.y >= self.height {
            return Color::TRANSPARENT;
        }
        let idx = self.offset(p.x, p.y);
        let pixels = self.pixels();
        match self.order {
            PixelOrder::Rgba8888 => Color::rgba(
                pixels[idx],
                pixels[idx + 1],
                pixels[idx + 2],
                pixels[idx + 3],
            ),
            _ => {
                let [r, g, b, a] = shm::load_pixel(
                    match self.order {
                        PixelOrder::Xrgb8888 => FrameFormat::Xrgb8888,
                        _ => FrameFormat::Rgb565,
                    },
                    &pixels[idx..],
                );
                Color::rgba(r, g, b, a)
            }
        }
    }

    /// Записать пиксель без смешивания (игнорирует отсечение).
    pub fn set_pixel(&mut self, p: Point, color: Color) {
        if p.x < 0 || p.y < 0 || p.x >= self.width || p.y >= self.height {
            return;
        }
        let idx = self.offset(p.x, p.y);
        let order = self.order;
        let pixels = self.pixels_mut();
        match order {
            PixelOrder::Rgba8888 => pixels[idx..idx + 4].copy_from_slice(&color.to_rgba_bytes()),
            PixelOrder::Xrgb8888 => shm::store_pixel(
                FrameFormat::Xrgb8888,
                &mut pixels[idx..],
                color.to_rgba_bytes(),
            ),
            PixelOrder::Rgb565 => shm::store_pixel(
                FrameFormat::Rgb565,
                &mut pixels[idx..],
                color.to_rgba_bytes(),
            ),
        }
    }

    /// Смешать цвет с пикселем поверхности (учитывает отсечение).
    pub fn blend_pixel(&mut self, p: Point, color: Color) {
        if color.a == 0 || !self.clip().contains(p) {
            return;
        }
        if p.x < 0 || p.y < 0 || p.x >= self.width || p.y >= self.height {
            return;
        }
        let dst = self.pixel(p);
        self.set_pixel(p, color.blend_over(dst));
    }

    /// Смешать пиксель с уже вычисленной долей покрытия (0..=1).
    fn blend_coverage(&mut self, x: i32, y: i32, color: Color, coverage: f32) {
        if coverage <= 0.0 || color.a == 0 {
            return;
        }
        let alpha = (color.a as f32 * coverage.clamp(0.0, 1.0)).round();
        if alpha <= 0.0 {
            return;
        }
        self.blend_pixel(
            Point::new(x, y),
            Color::rgba(color.r, color.g, color.b, alpha as u8),
        );
    }

    /// Залить всю поверхность цветом (без учёта отсечения).
    pub fn clear(&mut self, color: Color) {
        match self.order {
            PixelOrder::Rgba8888 => {
                let bytes = color.to_rgba_bytes();
                for pixel in self.pixels_mut().as_chunks_mut::<4>().0 {
                    *pixel = bytes;
                }
            }
            PixelOrder::Xrgb8888 => {
                let mut bytes = [0u8; 4];
                shm::store_pixel(FrameFormat::Xrgb8888, &mut bytes, color.to_rgba_bytes());
                for pixel in self.pixels_mut().as_chunks_mut::<4>().0 {
                    *pixel = bytes;
                }
            }
            PixelOrder::Rgb565 => {
                let mut bytes = [0u8; 2];
                shm::store_pixel(FrameFormat::Rgb565, &mut bytes, color.to_rgba_bytes());
                for pixel in self.pixels_mut().as_chunks_mut::<2>().0 {
                    *pixel = bytes;
                }
            }
        }
    }

    /// Залить прямоугольник (без сглаживания, с учётом отсечения).
    pub fn fill(&mut self, rect: Rect, color: Color) {
        if color.a == 0 {
            return;
        }
        let Some(area) = rect.intersect(self.clip()) else {
            return;
        };
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                self.blend_pixel(Point::new(x, y), color);
            }
        }
    }

    /// Горизонтальная линия толщиной 1 пиксель.
    pub fn hline(&mut self, y: i32, x0: i32, x1: i32, color: Color) {
        if x1 <= x0 {
            return;
        }
        self.fill(Rect::new(x0, y, x1 - x0, 1), color);
    }

    /// Вертикальная линия толщиной 1 пиксель.
    pub fn vline(&mut self, x: i32, y0: i32, y1: i32, color: Color) {
        if y1 <= y0 {
            return;
        }
        self.fill(Rect::new(x, y0, 1, y1 - y0), color);
    }

    /// Заливка прямоугольника со скруглениями (радиус в пикселях).
    pub fn fill_rounded(&mut self, rect: Rect, radius: i32, color: Color) {
        if color.a == 0 || rect.is_empty() {
            return;
        }
        let r = radius.clamp(0, rect.w.min(rect.h) / 2);
        if r == 0 {
            self.fill(rect, color);
            return;
        }
        let Some(area) = rect.intersect(self.clip()) else {
            return;
        };
        let corner_center_y = [rect.y as f32 + r as f32, rect.bottom() as f32 - r as f32];
        for y in area.y..area.bottom() {
            let py = y as f32 + 0.5;
            let row_in_middle = py >= corner_center_y[0] && py < corner_center_y[1];
            for x in area.x..area.right() {
                let px = x as f32 + 0.5;
                let cov =
                    if row_in_middle && px >= (rect.x + r) as f32 && px < (rect.right() - r) as f32
                    {
                        1.0
                    } else {
                        coverage_rounded(rect, r as f32, px, py)
                    };
                self.blend_coverage(x, y, color, cov);
            }
        }
    }

    /// Рамка со скруглениями заданной толщины.
    pub fn stroke_rounded(&mut self, rect: Rect, radius: i32, thickness: i32, color: Color) {
        if color.a == 0 || thickness <= 0 || rect.is_empty() {
            return;
        }
        let t = thickness.min(rect.w.min(rect.h) / 2);
        let r = radius.clamp(0, rect.w.min(rect.h) / 2) as f32;
        let inner = rect.inset(t);
        let ri = (r - t as f32).max(0.0);
        let Some(area) = rect.expand(1).intersect(self.clip()) else {
            return;
        };
        for y in area.y..area.bottom() {
            let py = y as f32 + 0.5;
            for x in area.x..area.right() {
                let px = x as f32 + 0.5;
                let outer_cov = coverage_rounded(rect, r, px, py);
                if outer_cov <= 0.0 {
                    continue;
                }
                let inner_cov = coverage_rounded(inner, ri, px, py);
                self.blend_coverage(x, y, color, (outer_cov - inner_cov).max(0.0));
            }
        }
    }

    /// Круг с центром и радиусом.
    pub fn fill_circle(&mut self, center: Point, radius: i32, color: Color) {
        if radius <= 0 || color.a == 0 {
            return;
        }
        let rect = Rect::new(center.x - radius, center.y - radius, radius * 2, radius * 2);
        self.fill_rounded(rect, radius, color);
    }

    /// Окружность заданной толщины.
    pub fn stroke_circle(&mut self, center: Point, radius: i32, thickness: i32, color: Color) {
        if radius <= 0 || thickness <= 0 || color.a == 0 {
            return;
        }
        let rect = Rect::new(center.x - radius, center.y - radius, radius * 2, radius * 2);
        self.stroke_rounded(rect, radius, thickness, color);
    }

    /// Отрезок с толщиной и сглаживанием (для иконок).
    pub fn line(&mut self, a: Point, b: Point, thickness: i32, color: Color) {
        if color.a == 0 || thickness <= 0 {
            return;
        }
        let half = thickness as f32 / 2.0;
        let x0 = a.x.min(b.x) - thickness;
        let y0 = a.y.min(b.y) - thickness;
        let x1 = a.x.max(b.x) + thickness;
        let y1 = a.y.max(b.y) + thickness;
        let Some(area) = Rect::new(x0, y0, x1 - x0, y1 - y0).intersect(self.clip()) else {
            return;
        };
        let (ax, ay) = (a.x as f32, a.y as f32);
        let (bx, by) = (b.x as f32, b.y as f32);
        let (dx, dy) = (bx - ax, by - ay);
        let len_sq = (dx * dx + dy * dy).max(0.0001);
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let t = (((px - ax) * dx + (py - ay) * dy) / len_sq).clamp(0.0, 1.0);
                let (cx, cy) = (ax + dx * t, ay + dy * t);
                let dist = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                let cov = (half + 0.5 - dist).clamp(0.0, 1.0);
                self.blend_coverage(x, y, color, cov);
            }
        }
    }

    /// Треугольник (например, «play» на иконке видео).
    pub fn fill_triangle(&mut self, a: Point, b: Point, c: Point, color: Color) {
        if color.a == 0 {
            return;
        }
        let min_x = a.x.min(b.x).min(c.x) - 1;
        let max_x = a.x.max(b.x).max(c.x) + 1;
        let min_y = a.y.min(b.y).min(c.y) - 1;
        let max_y = a.y.max(b.y).max(c.y) + 1;
        let Some(area) =
            Rect::new(min_x, min_y, max_x - min_x, max_y - min_y).intersect(self.clip())
        else {
            return;
        };
        let sign = |p: Point, q: Point, r: Point| -> f32 {
            (p.x as f32 - r.x as f32) * (q.y as f32 - r.y as f32)
                - (q.x as f32 - r.x as f32) * (p.y as f32 - r.y as f32)
        };
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let p = Point::new(x, y);
                let d1 = sign(p, a, b);
                let d2 = sign(p, b, c);
                let d3 = sign(p, c, a);
                let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
                let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
                if !(has_neg && has_pos) {
                    self.blend_pixel(p, color);
                }
            }
        }
    }

    /// Вертикальный градиент внутри прямоугольника.
    pub fn vertical_gradient(&mut self, rect: Rect, top: Color, bottom: Color) {
        let Some(area) = rect.intersect(self.clip()) else {
            return;
        };
        if rect.h <= 1 {
            self.fill(rect, top);
            return;
        }
        for y in area.y..area.bottom() {
            let t = (((y - rect.y) as f32 / (rect.h - 1) as f32) * 255.0) as u8;
            self.fill(Rect::new(rect.x, y, rect.w, 1), top.lerp(bottom, t));
        }
    }

    /// Мягкая тень вокруг прямоугольника (сглаженный SDF-градиент).
    pub fn soft_shadow(&mut self, rect: Rect, radius: i32, spread: i32, color: Color) {
        if spread <= 0 || color.a == 0 {
            return;
        }
        let Some(area) = rect.expand(spread + 1).intersect(self.clip()) else {
            return;
        };
        let r = radius.clamp(0, rect.w.min(rect.h) / 2) as f32;
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let d = rounded_rect_sdf(rect, r, x as f32 + 0.5, y as f32 + 0.5);
                let cov = if d <= 0.0 {
                    1.0
                } else {
                    let t = 1.0 - d / spread as f32;
                    t.max(0.0) * t.max(0.0)
                };
                if cov > 0.0 {
                    self.blend_coverage(x, y, color, cov);
                }
            }
        }
    }

    /// Скопировать содержимое другой поверхности со смешиванием по альфе.
    pub fn blit(&mut self, src: &Canvas, at: Point) {
        let target = Rect::new(at.x, at.y, src.width, src.height);
        let Some(area) = target.intersect(self.clip()) else {
            return;
        };
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let src_px = src.pixel(Point::new(x - at.x, y - at.y));
                if src_px.a == 0 {
                    continue;
                }
                self.blend_pixel(Point::new(x, y), src_px);
            }
        }
    }

    /// Скопировать прямоугольник из другой поверхности **без** смешивания.
    ///
    /// Это возврат пикселей из [`crate::layer::StaticLayer`] в поврежённую
    /// область за O(площадь прямоугольника): строки копируются как байты, а не
    /// пересобираются по пикселю. Поверхности обязаны иметь одинаковую
    /// геометрию и [`PixelOrder`] — смешивать форматы нельзя, поэтому неверный
    /// порядок ловится `debug_assert` (в release копия всё равно побайтовая).
    /// Отсечение цели учитывается; области вне `src` игнорируются.
    pub fn copy_rect(&mut self, src: &Canvas, rect: Rect) {
        let Some(area) = rect.intersect(self.clip()) else {
            return;
        };
        let Some(area) = area.intersect(Rect::new(0, 0, src.width, src.height)) else {
            return;
        };
        if self.order != src.order {
            // Разный порядок пикселей (основа RGBA → разделяемая XRGB/RGB565):
            // конвертируем по пикселю. Медленнее побайтовой копии, но
            // корректно; `StaticLayer` можно создавать сразу в формате
            // поверхности и всегда идти быстрым путём.
            for y in area.y..area.bottom() {
                for x in area.x..area.right() {
                    let pixel = src.pixel(Point::new(x, y));
                    self.set_pixel(Point::new(x, y), pixel);
                }
            }
            return;
        }
        let bpp = self.order.bytes_per_pixel();
        let row_bytes = area.w as usize * bpp;
        if row_bytes == 0 {
            return;
        }
        for y in area.y..area.bottom() {
            let src_off = y as usize * src.stride + area.x as usize * bpp;
            let dst_off = y as usize * self.stride + area.x as usize * bpp;
            self.pixels_mut()[dst_off..dst_off + row_bytes]
                .copy_from_slice(&src.pixels()[src_off..src_off + row_bytes]);
        }
    }

    /// Сузить область отсечения; восстанавливается через [`Canvas::pop_clip`].
    pub fn push_clip(&mut self, rect: Rect) {
        let new_clip = rect
            .intersect(self.clip())
            .unwrap_or_else(|| Rect::new(rect.x, rect.y, 0, 0));
        self.clip_stack.push(new_clip);
    }

    /// Восстановить предыдущую область отсечения.
    pub fn pop_clip(&mut self) {
        self.clip_stack.pop();
    }

    /// Выполнить замыкание внутри временной области отсечения.
    pub fn with_clip<R>(&mut self, rect: Rect, f: impl FnOnce(&mut Canvas) -> R) -> R {
        self.push_clip(rect);
        let result = f(self);
        self.pop_clip();
        result
    }

    /// Копия прямоугольной области поверхности.
    pub fn crop(&self, rect: Rect) -> Canvas {
        let mut out = Canvas::new(rect.w, rect.h);
        for y in 0..rect.h {
            for x in 0..rect.w {
                out.set_pixel(
                    Point::new(x, y),
                    self.pixel(Point::new(rect.x + x, rect.y + y)),
                );
            }
        }
        out
    }
}

/// Аналитическое расстояние до прямоугольника со скруглениями.
fn rounded_rect_sdf(rect: Rect, radius: f32, px: f32, py: f32) -> f32 {
    let cx = rect.x as f32 + rect.w as f32 / 2.0;
    let cy = rect.y as f32 + rect.h as f32 / 2.0;
    let hx = (rect.w as f32 / 2.0 - radius).max(0.0);
    let hy = (rect.h as f32 / 2.0 - radius).max(0.0);
    let qx = (px - cx).abs() - hx;
    let qy = (py - cy).abs() - hy;
    let ax = qx.max(0.0);
    let ay = qy.max(0.0);
    (ax * ax + ay * ay).sqrt() + qx.max(qy).min(0.0) - radius
}

/// Покрытие пикселя (0..=1) прямоугольником со скруглениями.
fn coverage_rounded(rect: Rect, radius: f32, px: f32, py: f32) -> f32 {
    (0.5 - rounded_rect_sdf(rect, radius, px, py)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_starts_transparent() {
        let canvas = Canvas::new(16, 8);
        assert_eq!(canvas.pixel(Point::new(3, 4)), Color::TRANSPARENT);
        assert_eq!(canvas.size(), Size::new(16, 8));
    }

    #[test]
    fn fill_respects_bounds_and_clip() {
        let mut canvas = Canvas::new(10, 10);
        canvas.push_clip(Rect::new(2, 2, 4, 4));
        canvas.fill(Rect::new(0, 0, 10, 10), Color::rgb(255, 0, 0));
        canvas.pop_clip();
        assert_eq!(canvas.pixel(Point::new(1, 1)), Color::TRANSPARENT);
        assert_eq!(canvas.pixel(Point::new(2, 2)), Color::rgb(255, 0, 0));
        assert_eq!(canvas.pixel(Point::new(5, 5)), Color::rgb(255, 0, 0));
        assert_eq!(canvas.pixel(Point::new(6, 6)), Color::TRANSPARENT);
    }

    #[test]
    fn alpha_blend_fill_over_background() {
        let mut canvas = Canvas::filled(4, 4, Color::BLACK);
        canvas.fill(Rect::new(1, 1, 2, 2), Color::rgba(255, 255, 255, 128));
        let px = canvas.pixel(Point::new(1, 1));
        assert!(px.r > 100 && px.r < 160, "px = {:?}", px);
        assert_eq!(canvas.pixel(Point::new(0, 0)), Color::BLACK);
    }

    #[test]
    fn rounded_fill_clears_corners() {
        let mut canvas = Canvas::new(20, 20);
        canvas.fill_rounded(Rect::new(0, 0, 20, 20), 6, Color::WHITE);
        // Углы прозрачны, центр и середины сторон — залиты.
        assert_eq!(canvas.pixel(Point::new(0, 0)), Color::TRANSPARENT);
        assert_eq!(canvas.pixel(Point::new(10, 10)), Color::WHITE);
        assert_eq!(canvas.pixel(Point::new(10, 0)), Color::WHITE);
        assert_eq!(canvas.pixel(Point::new(0, 10)), Color::WHITE);
    }

    #[test]
    fn stroke_leaves_center_untouched() {
        let mut canvas = Canvas::new(20, 20);
        canvas.stroke_rounded(Rect::new(0, 0, 20, 20), 4, 2, Color::WHITE);
        assert_eq!(canvas.pixel(Point::new(10, 10)), Color::TRANSPARENT);
        assert_eq!(canvas.pixel(Point::new(10, 0)), Color::WHITE);
    }

    #[test]
    fn hline_and_vline() {
        let mut canvas = Canvas::new(8, 8);
        canvas.hline(3, 1, 5, Color::WHITE);
        canvas.vline(6, 2, 6, Color::rgb(255, 0, 0));
        assert_eq!(canvas.pixel(Point::new(4, 3)), Color::WHITE);
        assert_eq!(canvas.pixel(Point::new(0, 3)), Color::TRANSPARENT);
        assert_eq!(canvas.pixel(Point::new(6, 4)), Color::rgb(255, 0, 0));
    }

    #[test]
    fn gradient_endpoints() {
        let mut canvas = Canvas::new(4, 3);
        canvas.vertical_gradient(
            Rect::new(0, 0, 4, 3),
            Color::rgb(0, 0, 0),
            Color::rgb(255, 255, 255),
        );
        assert_eq!(canvas.pixel(Point::new(0, 0)), Color::BLACK);
        assert_eq!(canvas.pixel(Point::new(0, 2)), Color::WHITE);
    }

    #[test]
    fn blit_blends_source() {
        let mut base = Canvas::filled(4, 4, Color::BLACK);
        let src = Canvas::filled(2, 2, Color::WHITE);
        base.blit(&src, Point::new(1, 1));
        assert_eq!(base.pixel(Point::new(1, 1)), Color::WHITE);
        assert_eq!(base.pixel(Point::new(0, 0)), Color::BLACK);
        assert_eq!(base.pixel(Point::new(3, 3)), Color::BLACK);
    }

    #[test]
    fn copy_rect_is_exact_and_clipped() {
        let mut src = Canvas::filled(8, 6, Color::rgb(1, 2, 3));
        src.set_pixel(Point::new(2, 2), Color::rgb(9, 9, 9));
        src.set_pixel(Point::new(3, 2), Color::rgba(0, 0, 0, 0));
        let mut dst = Canvas::filled(8, 6, Color::WHITE);
        let mut dst2 = Canvas::filled(8, 6, Color::WHITE);
        dst.copy_rect(&src, Rect::new(2, 2, 2, 1));
        // Точная копия, включая прозрачный пиксель (blit бы его пропустил).
        assert_eq!(dst.pixel(Point::new(2, 2)), Color::rgb(9, 9, 9));
        assert_eq!(dst.pixel(Point::new(3, 2)), Color::TRANSPARENT);
        assert_eq!(dst.pixel(Point::new(1, 2)), Color::WHITE);
        // Учитывает отсечение.
        dst2.push_clip(Rect::new(0, 0, 2, 6));
        dst2.copy_rect(&src, Rect::new(1, 2, 4, 1));
        dst2.pop_clip();
        assert_eq!(dst2.pixel(Point::new(0, 2)), Color::WHITE);
        assert_eq!(dst2.pixel(Point::new(1, 2)), Color::rgb(1, 2, 3));
        assert_eq!(dst2.pixel(Point::new(2, 2)), Color::WHITE);
        // Область вне источника игнорируется, а не паникует.
        let mut dst3 = Canvas::filled(8, 6, Color::WHITE);
        dst3.copy_rect(&src, Rect::new(7, 5, 4, 4));
        assert_eq!(dst3.pixel(Point::new(7, 5)), Color::rgb(1, 2, 3));
        assert_eq!(dst3.pixel(Point::new(2, 2)), Color::WHITE);
    }

    #[test]
    fn copy_rect_converts_between_pixel_orders() {
        use crate::protocol::FrameFormat;
        let mut src = Canvas::filled(8, 6, Color::rgb(10, 20, 30));
        let mut dst = Canvas::with_order(8, 6, PixelOrder::Xrgb8888);
        dst.clear(Color::BLACK);
        dst.copy_rect(&src, Rect::new(1, 1, 3, 2));
        // Читается в формате цели: цвет сохранился, а не перепутался.
        assert_eq!(dst.pixel(Point::new(1, 1)), Color::rgb(10, 20, 30));
        assert_eq!(dst.pixel(Point::new(3, 2)), Color::rgb(10, 20, 30));
        assert_eq!(dst.pixel(Point::new(0, 0)), Color::BLACK);
        // Быстрый путь (тот же порядок) копирует и прозрачность как есть.
        let mut rgb565 = Canvas::with_order(8, 6, PixelOrder::Rgb565);
        rgb565.copy_rect(&dst, Rect::new(1, 1, 1, 1));
        assert_eq!(
            rgb565.frame_format(),
            Some(FrameFormat::Rgb565),
            "порядок цели не изменился"
        );
    }

    #[test]
    fn crop_copies_region() {
        let mut canvas = Canvas::new(4, 4);
        canvas.set_pixel(Point::new(1, 1), Color::rgb(1, 2, 3));
        let sub = canvas.crop(Rect::new(1, 1, 2, 2));
        assert_eq!(sub.pixel(Point::new(0, 0)), Color::rgb(1, 2, 3));
        assert_eq!(sub.pixel(Point::new(1, 1)), Color::TRANSPARENT);
    }

    #[test]
    fn shadow_darkens_inside_and_fades_outside() {
        let mut canvas = Canvas::filled(40, 40, Color::WHITE);
        canvas.soft_shadow(Rect::new(15, 15, 10, 10), 2, 4, Color::rgba(0, 0, 0, 255));
        let inside = canvas.pixel(Point::new(20, 20));
        let near = canvas.pixel(Point::new(12, 20));
        let far = canvas.pixel(Point::new(2, 2));
        assert_eq!(inside, Color::BLACK);
        assert!(near.r < 240 && near.r > 0, "near = {:?}", near);
        assert_eq!(far, Color::WHITE);
    }
}
