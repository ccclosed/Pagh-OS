//! Оформление окна: заголовок, кнопки свернуть/развернуть/закрыть, контент.
//!
//! Кнопки заголовка повторяют концепт: `v` (свернуть), `^` (развернуть) и `x`
//! (закрыть). Окно рисуется тенью, скруглённым фоном и однопиксельной рамкой;
//! композитор может рисовать его поверх обоев как готовую поверхность.

use crate::canvas::Canvas;
use crate::color::Color;
use crate::font;
use crate::geom::{Point, Rect};
use crate::icon::{self, Icon};
use crate::theme::Theme;

/// Кнопка в заголовке окна.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChromeButton {
    Minimize,
    Maximize,
    Close,
}

impl ChromeButton {
    /// Иконка кнопки.
    pub fn icon(self) -> Icon {
        match self {
            ChromeButton::Minimize => Icon::ChevronDown,
            ChromeButton::Maximize => Icon::ChevronUp,
            ChromeButton::Close => Icon::Close,
        }
    }
}

/// Все кнопки заголовка слева направо.
pub const CHROME_BUTTONS: [ChromeButton; 3] = [
    ChromeButton::Minimize,
    ChromeButton::Maximize,
    ChromeButton::Close,
];

/// Раскладка окна и его заголовка.
#[derive(Clone, Debug)]
pub struct ChromeLayout {
    pub window: Rect,
    pub titlebar: Rect,
    pub title_rect: Rect,
    pub content: Rect,
    pub buttons: Vec<(ChromeButton, Rect)>,
}

impl ChromeLayout {
    /// Кнопка заголовка под точкой.
    pub fn button_at(&self, point: Point) -> Option<ChromeButton> {
        self.buttons
            .iter()
            .find(|(_, rect)| rect.contains(point))
            .map(|(button, _)| *button)
    }

    pub fn button_rect(&self, button: ChromeButton) -> Option<Rect> {
        self.buttons
            .iter()
            .find(|(candidate, _)| *candidate == button)
            .map(|(_, rect)| *rect)
    }
}

/// Параметры отрисовки окна.
#[derive(Clone, Copy, Debug)]
pub struct WindowStyle<'a> {
    pub title: &'a str,
    pub focused: bool,
    /// Кнопка под курсором (подсвечивается).
    pub hovered: Option<ChromeButton>,
}

impl<'a> WindowStyle<'a> {
    pub fn new(title: &'a str) -> WindowStyle<'a> {
        WindowStyle {
            title,
            focused: true,
            hovered: None,
        }
    }

    pub fn focused(mut self, focused: bool) -> WindowStyle<'a> {
        self.focused = focused;
        self
    }

    pub fn hovered(mut self, hovered: Option<ChromeButton>) -> WindowStyle<'a> {
        self.hovered = hovered;
        self
    }
}

const BUTTON_SIZE: i32 = 30;
const BUTTON_MARGIN: i32 = 6;

/// Геометрия окна (заголовок, контент, кнопки) без отрисовки.
pub fn layout_window(theme: &Theme, rect: Rect) -> ChromeLayout {
    let (titlebar, content) = rect.split_top(theme.metrics.title_height);
    let mut buttons = Vec::with_capacity(CHROME_BUTTONS.len());
    let mut x = rect.right() - BUTTON_MARGIN - BUTTON_SIZE;
    for button in CHROME_BUTTONS.iter().rev() {
        buttons.insert(
            0,
            (
                *button,
                Rect::new(
                    x,
                    titlebar.y + (titlebar.h - BUTTON_SIZE) / 2,
                    BUTTON_SIZE,
                    BUTTON_SIZE,
                ),
            ),
        );
        x -= BUTTON_SIZE;
    }
    let title_rect = Rect::new(
        titlebar.x + 14,
        titlebar.y,
        (buttons
            .first()
            .map(|(_, rect)| rect.x)
            .unwrap_or(titlebar.right())
            - titlebar.x
            - 28)
            .max(0),
        titlebar.h,
    );
    ChromeLayout {
        window: rect,
        titlebar,
        title_rect,
        content,
        buttons,
    }
}

/// Нарисовать окно и вернуть его раскладку.
pub fn draw_window(
    canvas: &mut Canvas,
    theme: &Theme,
    rect: Rect,
    style: WindowStyle<'_>,
) -> ChromeLayout {
    let palette = &theme.palette;
    let metrics = &theme.metrics;
    let radius = metrics.radius_window;

    let layout = layout_window(theme, rect);
    canvas.soft_shadow(rect, radius, 22, palette.shadow);
    canvas.fill_rounded(rect, radius, palette.window);

    let titlebar = layout.titlebar;
    let title_color = if style.focused {
        palette.titlebar
    } else {
        palette.titlebar_inactive
    };
    canvas.fill_rounded(titlebar, radius, title_color);
    // Нижние углы заголовка прямые.
    canvas.fill(
        Rect::new(titlebar.x, titlebar.bottom() - radius, titlebar.w, radius),
        title_color,
    );

    // Кнопки справа: v ^ x, как в концепте.
    for (button, button_rect) in &layout.buttons {
        let hovered = style.hovered == Some(*button);
        if hovered {
            let background = if *button == ChromeButton::Close {
                Color::hex(0xc0392b)
            } else {
                Color::rgba(255, 255, 255, 18)
            };
            canvas.fill_rounded(button_rect.inset(4), metrics.radius_control, background);
        }
        let icon_color = if hovered {
            Color::WHITE
        } else if style.focused {
            palette.title_text
        } else {
            palette.text_muted
        };
        let icon_size = metrics.icon_size;
        icon::draw(
            canvas,
            button.icon(),
            Rect::new(
                button_rect.x + (button_rect.w - icon_size) / 2,
                button_rect.y + (button_rect.h - icon_size) / 2,
                icon_size,
                icon_size,
            ),
            icon_color,
            palette,
        );
    }

    let title_rect = layout.title_rect;
    let title = font::ellipsize(style.title, title_rect.w);
    font::draw_aligned(
        canvas,
        title_rect,
        &title,
        if style.focused {
            palette.title_text
        } else {
            palette.text_muted
        },
        crate::font::TextAlign::Left,
    );

    canvas.stroke_rounded(rect, radius, 1, palette.window_border);

    layout
}

/// Нарисовать строку состояния внизу окна.
pub fn draw_statusbar(canvas: &mut Canvas, theme: &Theme, rect: Rect, left: &str, right: &str) {
    let palette = &theme.palette;
    canvas.fill(rect, palette.status);
    canvas.hline(rect.y, rect.x, rect.right(), palette.separator);
    let padding = theme.metrics.content_padding;
    let left_rect = Rect::new(rect.x + padding, rect.y, rect.w / 2, rect.h);
    let right_rect = Rect::new(rect.x + rect.w / 2, rect.y, rect.w / 2 - padding, rect.h);
    font::draw_aligned(
        canvas,
        left_rect,
        &font::ellipsize(left, left_rect.w),
        palette.status_text,
        crate::font::TextAlign::Left,
    );
    font::draw_aligned(
        canvas,
        right_rect,
        &font::ellipsize(right, right_rect.w),
        palette.status_text,
        crate::font::TextAlign::Right,
    );
}

/// Строка состояния по умолчанию (окно без контента) — для демо темы.
pub fn draw_panel(canvas: &mut Canvas, theme: &Theme, rect: Rect, clock: &str, date: &str) {
    let palette = &theme.palette;
    canvas.fill(rect, palette.desktop.lerp(Color::BLACK, 10));
    canvas.hline(rect.y, rect.x, rect.right(), palette.separator);
    let clock_rect = Rect::new(rect.right() - 90, rect.y + 4, 80, font::line_height());
    font::draw_aligned(
        canvas,
        clock_rect,
        clock,
        palette.text,
        crate::font::TextAlign::Right,
    );
    let date_rect = Rect::new(
        rect.right() - 90,
        rect.y + 4 + font::line_height(),
        80,
        font::line_height(),
    );
    font::draw_aligned(
        canvas,
        date_rect,
        date,
        palette.text_muted,
        crate::font::TextAlign::Right,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Point;

    #[test]
    fn window_layout_splits_titlebar_and_content() {
        let theme = Theme::pagh_dark();
        let mut canvas = Canvas::new(640, 480);
        let layout = draw_window(
            &mut canvas,
            &theme,
            Rect::new(10, 10, 620, 460),
            WindowStyle::new("Домашняя папка — Files"),
        );
        assert_eq!(layout.titlebar.h, theme.metrics.title_height);
        assert_eq!(layout.content.y, layout.titlebar.bottom());
        assert_eq!(layout.content.bottom(), 470);
        assert_eq!(layout.buttons.len(), 3);
    }

    #[test]
    fn chrome_button_hit_testing() {
        let theme = Theme::pagh_dark();
        let mut canvas = Canvas::new(400, 300);
        let layout = draw_window(
            &mut canvas,
            &theme,
            Rect::new(0, 0, 400, 300),
            WindowStyle::new("x"),
        );
        let close = layout.button_rect(ChromeButton::Close).unwrap();
        assert_eq!(layout.button_at(close.center()), Some(ChromeButton::Close));
        let minimize = layout.button_rect(ChromeButton::Minimize).unwrap();
        assert_eq!(
            layout.button_at(minimize.center()),
            Some(ChromeButton::Minimize)
        );
        assert_eq!(layout.button_at(Point::new(10, 10)), None);
        // Кнопки не перекрываются и лежат в заголовке.
        let mut rects: Vec<Rect> = layout.buttons.iter().map(|(_, rect)| *rect).collect();
        rects.sort_by_key(|rect| rect.x);
        for pair in rects.windows(2) {
            assert!(pair[0].right() <= pair[1].x);
        }
    }

    #[test]
    fn close_button_hover_is_red() {
        let theme = Theme::pagh_dark();
        let mut canvas = Canvas::new(300, 200);
        let layout = draw_window(
            &mut canvas,
            &theme,
            Rect::new(0, 0, 300, 200),
            WindowStyle::new("x").hovered(Some(ChromeButton::Close)),
        );
        let close = layout.button_rect(ChromeButton::Close).unwrap();
        let px = canvas.pixel(Point::new(close.x + 6, close.y + 8));
        assert!(px.r > px.g, "hover close должен быть красным: {:?}", px);
    }

    #[test]
    fn statusbar_draws_both_ends() {
        let theme = Theme::pagh_dark();
        let mut canvas = Canvas::new(400, 30);
        draw_statusbar(
            &mut canvas,
            &theme,
            Rect::new(0, 0, 400, 28),
            "24 папки, 4 файла · 76,3 МБ",
            "Домашняя папка",
        );
        let ink_left = (0..200)
            .flat_map(|x| (0..28).map(move |y| Point::new(x, y)))
            .any(|p| canvas.pixel(p) != theme.palette.status);
        let ink_right = (200..400)
            .flat_map(|x| (0..28).map(move |y| Point::new(x, y)))
            .any(|p| canvas.pixel(p) != theme.palette.status);
        assert!(ink_left && ink_right);
    }

    #[test]
    fn buttons_are_right_aligned() {
        let theme = Theme::pagh_dark();
        let mut canvas = Canvas::new(500, 200);
        let layout = draw_window(
            &mut canvas,
            &theme,
            Rect::new(0, 0, 500, 200),
            WindowStyle::new("t"),
        );
        let close = layout.button_rect(ChromeButton::Close).unwrap();
        assert!(close.right() <= 500 && close.right() > 500 - 40);
    }
}
