//! Векторные иконки тулкита.
//!
//! Иконки рисуются примитивами [`Canvas`] в нормированной сетке 16×16 и
//! масштабируются под целевой прямоугольник, поэтому не зависят от DPI и не
//! требуют файлов ресурсов. Цветные иконки папки/файла используют палитру темы.

use crate::canvas::Canvas;
use crate::color::Color;
use crate::geom::{Point, Rect};
use crate::theme::Palette;

/// Идентификатор иконки.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Icon {
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    ChevronDown,
    Back,
    Forward,
    Search,
    MenuDots,
    Close,
    Plus,
    Check,
    Home,
    Desktop,
    Documents,
    Downloads,
    Pictures,
    Music,
    Videos,
    Trash,
    Network,
    Device,
    Folder,
    File,
    Terminal,
    Link,
}

/// Нарисовать монохромную иконку в `rect` цветом `color`.
pub fn draw(canvas: &mut Canvas, icon: Icon, rect: Rect, color: Color, palette: &Palette) {
    if rect.is_empty() || color.a == 0 {
        return;
    }
    let pen = Pen::new(rect);
    let w = thickness(rect);
    match icon {
        Icon::ChevronLeft => {
            canvas.line(pen.p(10.0, 3.5), pen.p(5.5, 8.0), w, color);
            canvas.line(pen.p(5.5, 8.0), pen.p(10.0, 12.5), w, color);
        }
        Icon::ChevronRight => {
            canvas.line(pen.p(6.0, 3.5), pen.p(10.5, 8.0), w, color);
            canvas.line(pen.p(10.5, 8.0), pen.p(6.0, 12.5), w, color);
        }
        Icon::ChevronUp => {
            canvas.line(pen.p(3.5, 10.0), pen.p(8.0, 5.5), w, color);
            canvas.line(pen.p(8.0, 5.5), pen.p(12.5, 10.0), w, color);
        }
        Icon::ChevronDown => {
            canvas.line(pen.p(3.5, 6.0), pen.p(8.0, 10.5), w, color);
            canvas.line(pen.p(8.0, 10.5), pen.p(12.5, 6.0), w, color);
        }
        Icon::Back => {
            canvas.line(pen.p(9.5, 3.5), pen.p(5.0, 8.0), w, color);
            canvas.line(pen.p(5.0, 8.0), pen.p(9.5, 12.5), w, color);
        }
        Icon::Forward => {
            canvas.line(pen.p(6.5, 3.5), pen.p(11.0, 8.0), w, color);
            canvas.line(pen.p(11.0, 8.0), pen.p(6.5, 12.5), w, color);
        }
        Icon::Search => {
            canvas.stroke_circle(pen.p(7.0, 7.0), pen.span(4.2), w, color);
            canvas.line(pen.p(10.2, 10.2), pen.p(14.0, 14.0), w, color);
        }
        Icon::MenuDots => {
            let r = pen.span(1.15).max(1);
            for y in [3.0, 8.0, 13.0] {
                canvas.fill_circle(pen.p(8.0, y), r, color);
            }
        }
        Icon::Close => {
            canvas.line(pen.p(3.8, 3.8), pen.p(12.2, 12.2), w, color);
            canvas.line(pen.p(12.2, 3.8), pen.p(3.8, 12.2), w, color);
        }
        Icon::Plus => {
            canvas.line(pen.p(8.0, 3.0), pen.p(8.0, 13.0), w, color);
            canvas.line(pen.p(3.0, 8.0), pen.p(13.0, 8.0), w, color);
        }
        Icon::Check => {
            canvas.line(pen.p(3.0, 8.5), pen.p(6.5, 12.0), w, color);
            canvas.line(pen.p(6.5, 12.0), pen.p(13.0, 4.0), w, color);
        }
        Icon::Home => {
            canvas.line(pen.p(2.0, 8.0), pen.p(8.0, 2.0), w, color);
            canvas.line(pen.p(8.0, 2.0), pen.p(14.0, 8.0), w, color);
            canvas.line(pen.p(3.5, 7.5), pen.p(3.5, 14.0), w, color);
            canvas.line(pen.p(12.5, 7.5), pen.p(12.5, 14.0), w, color);
            canvas.line(pen.p(3.5, 14.0), pen.p(12.5, 14.0), w, color);
            canvas.stroke_rounded(
                Rect::new(
                    pen.px(6.5),
                    pen.py(9.5),
                    pen.span(3.0).max(2),
                    pen.span(4.5).max(2),
                ),
                pen.span(0.8),
                w.max(1),
                color,
            );
        }
        Icon::Desktop => {
            canvas.stroke_rounded(
                Rect::new(pen.px(1.5), pen.py(3.0), pen.span(13.0), pen.span(8.0)),
                pen.span(1.0),
                w,
                color,
            );
            canvas.line(pen.p(8.0, 11.0), pen.p(8.0, 13.5), w, color);
            canvas.line(pen.p(5.0, 13.5), pen.p(11.0, 13.5), w, color);
        }
        Icon::Documents => {
            canvas.stroke_rounded(
                Rect::new(pen.px(3.0), pen.py(2.0), pen.span(10.0), pen.span(12.0)),
                pen.span(1.0),
                w,
                color,
            );
            for y in [5.5, 8.0, 10.5] {
                canvas.line(pen.p(5.0, y), pen.p(11.0, y), 1, color);
            }
        }
        Icon::Downloads => {
            canvas.line(pen.p(8.0, 2.0), pen.p(8.0, 9.5), w, color);
            canvas.line(pen.p(4.5, 6.0), pen.p(8.0, 9.5), w, color);
            canvas.line(pen.p(11.5, 6.0), pen.p(8.0, 9.5), w, color);
            canvas.line(pen.p(2.5, 11.5), pen.p(13.5, 11.5), w, color);
            canvas.line(pen.p(2.5, 11.5), pen.p(2.5, 14.0), w, color);
            canvas.line(pen.p(13.5, 11.5), pen.p(13.5, 14.0), w, color);
            canvas.line(pen.p(2.5, 14.0), pen.p(13.5, 14.0), w, color);
        }
        Icon::Pictures => {
            canvas.stroke_rounded(
                Rect::new(pen.px(1.5), pen.py(2.5), pen.span(13.0), pen.span(11.0)),
                pen.span(1.0),
                w,
                color,
            );
            canvas.fill_triangle(pen.p(3.0, 12.0), pen.p(7.0, 5.5), pen.p(11.0, 12.0), color);
            canvas.fill_circle(pen.p(11.5, 6.0), pen.span(1.3).max(1), color);
        }
        Icon::Music => {
            canvas.fill_circle(pen.p(4.5, 12.0), pen.span(2.2).max(1), color);
            canvas.fill_circle(pen.p(11.5, 10.5), pen.span(2.2).max(1), color);
            canvas.line(pen.p(6.6, 12.0), pen.p(6.6, 3.5), w, color);
            canvas.line(pen.p(13.6, 10.5), pen.p(13.6, 2.0), w, color);
            canvas.line(pen.p(6.6, 3.5), pen.p(13.6, 2.0), w, color);
        }
        Icon::Videos => {
            canvas.stroke_rounded(
                Rect::new(pen.px(1.5), pen.py(3.5), pen.span(13.0), pen.span(9.5)),
                pen.span(1.0),
                w,
                color,
            );
            canvas.fill_triangle(pen.p(6.5, 5.5), pen.p(6.5, 11.0), pen.p(11.5, 8.25), color);
        }
        Icon::Trash => {
            canvas.line(pen.p(2.5, 4.0), pen.p(13.5, 4.0), w, color);
            canvas.line(pen.p(6.0, 2.0), pen.p(10.0, 2.0), w, color);
            canvas.stroke_rounded(
                Rect::new(pen.px(3.5), pen.py(4.5), pen.span(9.0), pen.span(9.5)),
                pen.span(1.0),
                w,
                color,
            );
            canvas.line(pen.p(6.5, 6.5), pen.p(6.5, 12.0), 1, color);
            canvas.line(pen.p(9.5, 6.5), pen.p(9.5, 12.0), 1, color);
        }
        Icon::Network => {
            canvas.stroke_circle(pen.p(8.0, 8.0), pen.span(6.0), w, color);
            canvas.line(pen.p(2.0, 8.0), pen.p(14.0, 8.0), 1, color);
            canvas.line(pen.p(8.0, 2.0), pen.p(8.0, 14.0), 1, color);
            canvas.line(pen.p(4.0, 5.0), pen.p(12.0, 5.0), 1, color);
            canvas.line(pen.p(4.0, 11.0), pen.p(12.0, 11.0), 1, color);
        }
        Icon::Device => {
            canvas.stroke_rounded(
                Rect::new(pen.px(1.5), pen.py(4.0), pen.span(13.0), pen.span(8.0)),
                pen.span(1.5),
                w,
                color,
            );
            canvas.fill_circle(pen.p(11.5, 8.0), pen.span(1.0).max(1), color);
            canvas.line(pen.p(4.0, 8.0), pen.p(8.0, 8.0), 1, color);
        }
        Icon::Terminal => {
            canvas.stroke_rounded(
                Rect::new(pen.px(1.5), pen.py(2.5), pen.span(13.0), pen.span(11.0)),
                pen.span(1.0),
                w,
                color,
            );
            canvas.line(pen.p(4.0, 6.0), pen.p(6.5, 8.25), w, color);
            canvas.line(pen.p(6.5, 8.25), pen.p(4.0, 10.5), w, color);
            canvas.line(pen.p(8.5, 11.0), pen.p(12.0, 11.0), w, color);
        }
        Icon::Folder => draw_folder(canvas, rect, palette),
        Icon::File => draw_file(canvas, rect, palette),
        Icon::Link => {
            canvas.stroke_rounded(
                Rect::new(pen.px(2.0), pen.py(9.0), pen.span(7.0), pen.span(5.0)),
                pen.span(2.5),
                w,
                color,
            );
            canvas.stroke_rounded(
                Rect::new(pen.px(7.0), pen.py(2.0), pen.span(7.0), pen.span(5.0)),
                pen.span(2.5),
                w,
                color,
            );
            canvas.line(pen.p(6.0, 10.0), pen.p(10.0, 6.0), w, color);
        }
    }
}

/// Цветная иконка папки: синяя папка с листом бумаги внутри.
pub fn draw_folder(canvas: &mut Canvas, rect: Rect, palette: &Palette) {
    if rect.is_empty() {
        return;
    }
    let pen = Pen::new(rect);
    let radius = pen.span(1.6).max(1);
    let body_top = pen.py(3.0);
    let bottom = pen.py(15.5);
    let body = Rect::new(pen.px(0.5), body_top, pen.span(15.0), bottom - body_top);

    // Язычок папки.
    canvas.fill_rounded(
        Rect::new(pen.px(0.5), pen.py(1.0), pen.span(6.0), pen.span(2.5)),
        radius,
        palette.folder_back,
    );
    // Задняя стенка.
    canvas.fill_rounded(body, radius, palette.folder_back);
    // Лист бумаги, выглядывающий из папки.
    let paper = Rect::new(pen.px(3.0), pen.py(1.5), pen.span(10.0), pen.span(8.0));
    canvas.fill_rounded(paper, pen.span(0.7), palette.paper);
    let line = pen.span(0.55).max(1);
    canvas.fill(
        Rect::new(
            paper.x + pen.span(0.8),
            paper.y + pen.span(1.4),
            paper.w - pen.span(1.6),
            line,
        ),
        palette.paper_line,
    );
    canvas.fill(
        Rect::new(
            paper.x + pen.span(0.8),
            paper.y + pen.span(3.2),
            (paper.w as f32 * 0.62) as i32,
            line,
        ),
        palette.paper_line,
    );
    canvas.fill(
        Rect::new(
            paper.x + pen.span(0.8),
            paper.y + pen.span(5.0),
            (paper.w as f32 * 0.78) as i32,
            line,
        ),
        palette.paper_line,
    );
    // Передняя стенка папки.
    let front_top = pen.py(8.0);
    canvas.fill_rounded(
        Rect::new(pen.px(0.5), front_top, pen.span(15.0), bottom - front_top),
        radius,
        palette.folder_front,
    );
    // Блик у нижнего края.
    canvas.fill_rounded(
        Rect::new(
            pen.px(1.5),
            bottom - pen.span(1.6),
            pen.span(13.0),
            pen.span(0.6).max(1),
        ),
        pen.span(0.3),
        palette.folder_line.with_alpha(140),
    );
}

/// Иконка файла: лист с загнутым углом.
pub fn draw_file(canvas: &mut Canvas, rect: Rect, palette: &Palette) {
    if rect.is_empty() {
        return;
    }
    let pen = Pen::new(rect);
    let radius = pen.span(0.8).max(1);
    let page = Rect::new(pen.px(3.0), pen.py(1.0), pen.span(10.0), pen.span(14.0));
    canvas.fill_rounded(page, radius, palette.paper);
    // Загнутый угол.
    let fold = pen.span(3.2).max(2);
    canvas.fill_triangle(
        Point::new(page.right() - fold, page.y),
        Point::new(page.right(), page.y + fold),
        Point::new(page.right() - fold, page.y + fold),
        palette.paper_line,
    );
    for (index, y) in [5.0, 7.5, 10.0, 12.5].iter().enumerate() {
        let width = if index == 3 {
            page.w as f32 * 0.55
        } else {
            page.w as f32 * 0.75
        };
        canvas.fill(
            Rect::new(
                page.x + pen.span(1.0),
                pen.py(*y),
                width as i32,
                pen.span(0.55).max(1),
            ),
            palette.paper_line,
        );
    }
}

/// Толщина штриха для иконки данного размера.
fn thickness(rect: Rect) -> i32 {
    (rect.w.min(rect.h) / 8).clamp(1, 3)
}

/// Утилиты нормированной сетки 16×16.
struct Pen {
    x: i32,
    y: i32,
    scale: f32,
}

impl Pen {
    fn new(rect: Rect) -> Pen {
        let side = rect.w.min(rect.h);
        let scale = side as f32 / 16.0;
        Pen {
            x: rect.x + (rect.w - side) / 2,
            y: rect.y + (rect.h - side) / 2,
            scale,
        }
    }

    fn px(&self, units: f32) -> i32 {
        self.x + (units * self.scale).round() as i32
    }

    fn py(&self, units: f32) -> i32 {
        self.y + (units * self.scale).round() as i32
    }

    fn p(&self, x: f32, y: f32) -> Point {
        Point::new(self.px(x), self.py(y))
    }

    fn span(&self, units: f32) -> i32 {
        ((units * self.scale).round() as i32).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ink(canvas: &Canvas) -> usize {
        (0..canvas.height())
            .flat_map(|y| (0..canvas.width()).map(move |x| Point::new(x, y)))
            .filter(|p| canvas.pixel(*p).a > 0)
            .count()
    }

    #[test]
    fn every_icon_draws_something() {
        let palette = Palette::default();
        let icons = [
            Icon::ChevronLeft,
            Icon::ChevronRight,
            Icon::ChevronUp,
            Icon::ChevronDown,
            Icon::Back,
            Icon::Forward,
            Icon::Search,
            Icon::MenuDots,
            Icon::Close,
            Icon::Plus,
            Icon::Check,
            Icon::Home,
            Icon::Desktop,
            Icon::Documents,
            Icon::Downloads,
            Icon::Pictures,
            Icon::Music,
            Icon::Videos,
            Icon::Trash,
            Icon::Network,
            Icon::Device,
            Icon::Folder,
            Icon::File,
            Icon::Terminal,
            Icon::Link,
        ];
        for icon in icons {
            let mut canvas = Canvas::new(20, 20);
            draw(
                &mut canvas,
                icon,
                Rect::new(2, 2, 16, 16),
                Color::WHITE,
                &palette,
            );
            assert!(ink(&canvas) > 4, "иконка {icon:?} ничего не нарисовала");
        }
    }

    #[test]
    fn folder_is_coloured() {
        let palette = Palette::default();
        let mut canvas = Canvas::new(40, 40);
        draw_folder(&mut canvas, Rect::new(0, 0, 40, 40), &palette);
        let front = canvas.pixel(Point::new(20, 34));
        assert_eq!(front, palette.folder_front);
        assert!(
            canvas.pixel(Point::new(20, 12)) == palette.paper
                || canvas.pixel(Point::new(20, 12)).a > 0
        );
        assert_eq!(canvas.pixel(Point::new(0, 0)), Color::TRANSPARENT);
    }

    #[test]
    fn icons_stay_inside_rect() {
        let mut canvas = Canvas::new(24, 24);
        draw(
            &mut canvas,
            Icon::Search,
            Rect::new(4, 4, 16, 16),
            Color::rgb(255, 0, 0),
            &Palette::default(),
        );
        for y in 0..24 {
            for x in 0..24 {
                if canvas.pixel(Point::new(x, y)).a > 0 {
                    assert!((3..=21).contains(&x) && (3..=21).contains(&y));
                }
            }
        }
    }
}
