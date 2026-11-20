//! Окно клиента: геометрия рамки, заголовок, кнопки v/^/x, resize-угол.

use crate::geometry::{Point, Rect, Size};
use crate::surface::{PixelFormat, Surface};

/// Высота заголовка окна.
pub const TITLEBAR_HEIGHT: i32 = 30;
/// Толщина рамки вокруг содержимого.
pub const BORDER: i32 = 1;
/// Размер квадратной кнопки заголовка.
pub const BUTTON_SIZE: i32 = 22;
/// Зона захвата правого нижнего угла для resize.
pub const RESIZE_HANDLE: i32 = 18;
/// Минимальный размер клиентской области при resize.
pub const MIN_CONTENT: Size = Size::new(320, 200);

const BUTTON_GAP: i32 = 4;
const RIGHT_PADDING: i32 = 8;

/// Часть окна под указателем.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowPart {
    TitleBar,
    Minimize,
    Maximize,
    Close,
    ResizeCorner,
    Content,
}

/// Окно верхнего уровня.
#[derive(Clone, Debug)]
pub struct Window {
    pub id: u32,
    pub client_id: u64,
    pub title: String,
    /// Внешняя рамка, включая заголовок и рамку.
    pub frame: Rect,
    /// Размер клиентской области, заданный последним `configure`.
    pub content_size: Size,
    pub format: PixelFormat,
    pub surface: Option<Surface>,
    pub focused: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub restore_frame: Option<Rect>,
    pub created_seq: u64,
}

impl Window {
    pub fn new(
        id: u32,
        client_id: u64,
        title: impl Into<String>,
        position: Point,
        content_size: Size,
        format: PixelFormat,
        created_seq: u64,
    ) -> Self {
        let frame = Rect::new(
            position.x,
            position.y,
            content_size.w as i32 + BORDER * 2,
            content_size.h as i32 + TITLEBAR_HEIGHT + BORDER * 2,
        );
        Self {
            id,
            client_id,
            title: title.into(),
            frame,
            content_size,
            format,
            surface: None,
            focused: false,
            minimized: false,
            maximized: false,
            restore_frame: None,
            created_seq,
        }
    }

    pub fn content_rect(&self) -> Rect {
        content_rect_for(self.frame)
    }

    pub fn titlebar_rect(&self) -> Rect {
        titlebar_rect_for(self.frame)
    }

    pub fn close_rect(&self) -> Rect {
        button_rect_for(self.frame, 0)
    }

    pub fn maximize_rect(&self) -> Rect {
        button_rect_for(self.frame, 1)
    }

    pub fn minimize_rect(&self) -> Rect {
        button_rect_for(self.frame, 2)
    }

    pub fn resize_corner_rect(&self) -> Rect {
        resize_corner_rect_for(self.frame)
    }

    /// Где находится точка внутри окна; `None` — снаружи или окно свёрнуто.
    pub fn part_at(&self, point: Point) -> Option<WindowPart> {
        if self.minimized || !self.frame.contains(point) {
            return None;
        }
        if self.close_rect().contains(point) {
            return Some(WindowPart::Close);
        }
        if self.maximize_rect().contains(point) {
            return Some(WindowPart::Maximize);
        }
        if self.minimize_rect().contains(point) {
            return Some(WindowPart::Minimize);
        }
        if self.resize_corner_rect().contains(point) {
            return Some(WindowPart::ResizeCorner);
        }
        if self.titlebar_rect().contains(point) {
            return Some(WindowPart::TitleBar);
        }
        if self.content_rect().contains(point) {
            return Some(WindowPart::Content);
        }
        // Рамка вокруг содержимого.
        Some(WindowPart::TitleBar)
    }

    pub fn set_position(&mut self, position: Point) {
        self.frame.x = position.x;
        self.frame.y = position.y;
    }

    /// Задать размер клиентской области (серверная сторона resize/maximize).
    pub fn set_content_size(&mut self, size: Size) {
        self.content_size = size;
        self.frame.w = size.w as i32 + BORDER * 2;
        self.frame.h = size.h as i32 + TITLEBAR_HEIGHT + BORDER * 2;
    }

    /// Обрезать позицию так, чтобы заголовок оставался на экране.
    pub fn clamp_position(&mut self, screen: Rect, panel_height: i32) {
        let min_visible = 80;
        let max_x = screen.right() - min_visible;
        let min_x = -(self.frame.w - min_visible);
        let max_y = screen.bottom() - panel_height - TITLEBAR_HEIGHT - BORDER;
        let min_y = 0;
        self.frame.x = self.frame.x.clamp(min_x, max_x);
        self.frame.y = self.frame.y.clamp(min_y, max_y);
    }

    pub fn title_text(&self) -> String {
        title_text_for(&self.title, self.frame)
    }

    pub fn titlebar_color(&self, theme: &crate::theme::Theme) -> crate::color::Color {
        if self.focused {
            theme.titlebar_active
        } else {
            theme.titlebar_inactive
        }
    }

    pub fn border_color(&self, theme: &crate::theme::Theme) -> crate::color::Color {
        if self.focused {
            theme.window_border_active
        } else {
            theme.window_border_inactive
        }
    }
}

/// Клиентская область для заданной внешней рамки (без `Window`).
pub fn content_rect_for(frame: Rect) -> Rect {
    Rect::new(
        frame.x + BORDER,
        frame.y + BORDER + TITLEBAR_HEIGHT,
        frame.w - BORDER * 2,
        frame.h - BORDER * 2 - TITLEBAR_HEIGHT,
    )
}

/// Полоса заголовка для заданной внешней рамки.
pub fn titlebar_rect_for(frame: Rect) -> Rect {
    Rect::new(
        frame.x + BORDER,
        frame.y + BORDER,
        frame.w - BORDER * 2,
        TITLEBAR_HEIGHT,
    )
}

/// Кнопка заголовка (0 — close, 1 — maximize, 2 — minimize) для рамки.
pub fn button_rect_for(frame: Rect, index: i32) -> Rect {
    let bar = titlebar_rect_for(frame);
    let y = bar.y + (bar.h - BUTTON_SIZE) / 2;
    let x = bar.right() - RIGHT_PADDING - BUTTON_SIZE - index * (BUTTON_SIZE + BUTTON_GAP);
    Rect::new(x, y, BUTTON_SIZE, BUTTON_SIZE)
}

/// Угол resize для заданной рамки.
pub fn resize_corner_rect_for(frame: Rect) -> Rect {
    Rect::new(
        frame.right() - RESIZE_HANDLE,
        frame.bottom() - RESIZE_HANDLE,
        RESIZE_HANDLE,
        RESIZE_HANDLE,
    )
}

/// Обрезанный заголовок окна для рамки заданной ширины.
pub fn title_text_for(title: &str, frame: Rect) -> String {
    let max_chars =
        ((frame.w - 3 * (BUTTON_SIZE + BUTTON_GAP) - RIGHT_PADDING * 2 - 16) / 8).max(4) as usize;
    truncate_text(title, max_chars)
}

/// Обрезать строку по количеству символов, добавляя многоточие из трёх точек.
pub fn truncate_text(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let mut out: String = chars[..max_chars - 3].iter().collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Window {
        Window::new(
            1,
            10,
            "Terminal — pagh",
            Point::new(100, 80),
            Size::new(640, 400),
            PixelFormat::Xrgb8888,
            1,
        )
    }

    #[test]
    fn geometry_matches_content_size() {
        let window = sample();
        assert_eq!(window.frame, Rect::new(100, 80, 642, 432));
        assert_eq!(window.content_rect(), Rect::new(101, 111, 640, 400));
        assert_eq!(window.titlebar_rect().h, TITLEBAR_HEIGHT);
        assert_eq!(
            window.part_at(Point::new(110, 90)),
            Some(WindowPart::TitleBar)
        );
        assert_eq!(
            window.part_at(Point::new(110, 120)),
            Some(WindowPart::Content)
        );
        assert_eq!(window.part_at(Point::new(50, 50)), None);
    }

    #[test]
    fn buttons_are_inside_titlebar_and_distinct() {
        let window = sample();
        let buttons = [
            window.close_rect(),
            window.maximize_rect(),
            window.minimize_rect(),
        ];
        for button in buttons {
            assert!(window.titlebar_rect().contains_rect(button));
            assert_eq!(window.part_at(button.center()), {
                if button == window.close_rect() {
                    Some(WindowPart::Close)
                } else if button == window.maximize_rect() {
                    Some(WindowPart::Maximize)
                } else {
                    Some(WindowPart::Minimize)
                }
            });
        }
        assert!(!buttons[0].intersects(buttons[1]));
        assert!(!buttons[1].intersects(buttons[2]));
    }

    #[test]
    fn resize_changes_content_and_frame() {
        let mut window = sample();
        window.set_content_size(Size::new(800, 500));
        assert_eq!(window.frame.w, 802);
        assert_eq!(window.frame.h, 532);
        assert_eq!(window.content_rect().w, 800);
        assert_eq!(window.content_rect().h, 500);
    }

    #[test]
    fn minimized_window_ignores_hits() {
        let mut window = sample();
        window.minimized = true;
        assert_eq!(window.part_at(Point::new(110, 90)), None);
    }

    #[test]
    fn title_is_truncated() {
        assert_eq!(truncate_text("short", 10), "short");
        assert_eq!(truncate_text("abcdefgh", 6), "abc...");
        assert_eq!(truncate_text("abcdefgh", 2), "..");
    }
}
