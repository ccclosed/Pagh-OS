//! Статический слой кадра: кэш неизменяемых частей поверхности.
//!
//! Инкрементальный кадр состоит из двух проходов: **основа**
//! ([`StaticLayer`] — фон, панель, боковая панель, строка состояния) и
//! **динамика** (текст, иконки, подсветка, меню). Поврежённый прямоугольник
//! восстанавливается из основы побайтовой копией
//! ([`Canvas::copy_rect`]) и поверх него рисуется только динамика,
//! обрезанная тем же прямоугольником. Пиксельный объём работы — O(площадь
//! damage), а не O(площадь поверхности); сама основа перерисовывается только
//! при смене размера/темы (см. `docs/desktop-render-spec.md` §4 C2).
//!
//! Контракт: колбэк [`StaticLayer::rebuild`] обязан закрасить **всю**
//! поверхность слоя — слой не хранит «прозрачных» дыр, иначе восстановление
//! прямоугольника вернуло бы устаревшие пиксели. Слой не знает, когда он
//! устарел: вызывающий зовёт [`StaticLayer::invalidate`] (resize, смена темы,
//! первый кадр) и затем перестраивает.
//!
//! ```
//! use pagh_ui::canvas::Canvas;
//! use pagh_ui::color::Color;
//! use pagh_ui::geom::{Rect, Size};
//! use pagh_ui::layer::StaticLayer;
//!
//! let mut layer = StaticLayer::new(Size::new(64, 32));
//! layer.rebuild(|canvas| canvas.clear(Color::BLACK));
//! let mut frame = Canvas::filled(64, 32, Color::WHITE);
//! assert!(layer.restore(&mut frame, Rect::new(8, 8, 16, 16)));
//! assert_eq!(frame.pixel(pagh_ui::geom::Point::new(8, 8)), Color::BLACK);
//! ```

use crate::canvas::{Canvas, PixelOrder};
use crate::color::Color;
use crate::geom::{Rect, Size};

/// Кэш статической основы кадра.
pub struct StaticLayer {
    canvas: Canvas,
    valid: bool,
}

impl StaticLayer {
    /// Слой под поверхность `size` в формате straight-alpha RGBA.
    ///
    /// Для разделяемой поверхности (XRGB8888/RGB565) создавайте слой
    /// [`StaticLayer::with_order`] — тогда восстановление прямоугольника
    /// остаётся побайтовой копией.
    pub fn new(size: Size) -> StaticLayer {
        StaticLayer::with_order(size, PixelOrder::Rgba8888)
    }

    /// Слой в заданном порядке пикселей (под формат целевой поверхности).
    pub fn with_order(size: Size, order: PixelOrder) -> StaticLayer {
        StaticLayer {
            canvas: Canvas::with_order(size.w.max(0), size.h.max(0), order),
            valid: false,
        }
    }

    /// Размер слоя.
    pub fn size(&self) -> Size {
        self.canvas.size()
    }

    /// Слой заполнен свежей основой.
    pub fn is_valid(&self) -> bool {
        self.valid
    }

    /// Пометить основу устаревшей (resize, смена темы, первый кадр).
    pub fn invalidate(&mut self) {
        self.valid = false;
    }

    /// Готовые пиксели основы (для [`StaticLayer::restore`] и для отладки).
    pub fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    /// Перестроить основу: `draw` обязан закрасить всю поверхность.
    pub fn rebuild(&mut self, draw: impl FnOnce(&mut Canvas)) {
        self.canvas.clear(Color::TRANSPARENT);
        draw(&mut self.canvas);
        self.valid = true;
    }

    /// Вернуть пиксели основы в `dst` внутри `rect`.
    ///
    /// `false` — слой невалиден или другого размера; вызывающий обязан
    /// перерисовать кадр целиком, а не продолжать инкрементально.
    pub fn restore(&self, dst: &mut Canvas, rect: Rect) -> bool {
        if !self.valid || dst.size() != self.canvas.size() {
            return false;
        }
        dst.copy_rect(&self.canvas, rect);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Point;

    #[test]
    fn fresh_layer_is_invalid() {
        let layer = StaticLayer::new(Size::new(32, 16));
        assert!(!layer.is_valid());
        assert_eq!(layer.size(), Size::new(32, 16));
        let mut dst = Canvas::filled(32, 16, Color::WHITE);
        // Невалидный слой не раздаёт пиксели: иначе кадр потерял бы фон.
        assert!(!layer.restore(&mut dst, Rect::new(0, 0, 4, 4)));
    }

    #[test]
    fn layer_keeps_the_surface_pixel_order() {
        use crate::protocol::FrameFormat;
        let mut layer = StaticLayer::with_order(Size::new(8, 8), PixelOrder::Xrgb8888);
        assert_eq!(layer.canvas().frame_format(), Some(FrameFormat::Xrgb8888));
        layer.rebuild(|canvas| canvas.clear(Color::rgb(1, 2, 3)));
        let mut dst = Canvas::with_order(8, 8, PixelOrder::Xrgb8888);
        assert!(layer.restore(&mut dst, Rect::new(0, 0, 8, 8)));
        assert_eq!(dst.pixel(Point::new(5, 5)), Color::rgb(1, 2, 3));
    }

    #[test]
    fn rebuild_marks_valid_and_restore_returns_base() {
        let mut layer = StaticLayer::new(Size::new(32, 16));
        layer.rebuild(|canvas| canvas.clear(Color::BLACK));
        assert!(layer.is_valid());
        let mut dst = Canvas::filled(32, 16, Color::WHITE);
        assert!(layer.restore(&mut dst, Rect::new(4, 4, 8, 8)));
        assert_eq!(dst.pixel(Point::new(4, 4)), Color::BLACK);
        assert_eq!(dst.pixel(Point::new(11, 11)), Color::BLACK);
        assert_eq!(dst.pixel(Point::new(12, 12)), Color::WHITE);
        assert_eq!(dst.pixel(Point::new(3, 3)), Color::WHITE);
    }

    #[test]
    fn invalidate_forces_a_rebuild() {
        let mut layer = StaticLayer::new(Size::new(16, 16));
        layer.rebuild(|canvas| canvas.clear(Color::BLACK));
        layer.invalidate();
        assert!(!layer.is_valid());
        let mut dst = Canvas::filled(16, 16, Color::WHITE);
        assert!(!layer.restore(&mut dst, Rect::new(0, 0, 8, 8)));
        layer.rebuild(|canvas| canvas.clear(Color::rgb(1, 2, 3)));
        assert!(layer.restore(&mut dst, Rect::new(0, 0, 8, 8)));
        assert_eq!(dst.pixel(Point::new(0, 0)), Color::rgb(1, 2, 3));
    }

    #[test]
    fn restore_refuses_a_different_geometry() {
        let mut layer = StaticLayer::new(Size::new(16, 16));
        layer.rebuild(|canvas| canvas.clear(Color::BLACK));
        let mut dst = Canvas::filled(32, 16, Color::WHITE);
        assert!(!layer.restore(&mut dst, Rect::new(0, 0, 8, 8)));
    }
}
