//! Нижняя icon-only панель (44 px): часы/дата, сеть, лаунчер и чипы окон.

use crate::canvas::Canvas;
use crate::color::Color;
use crate::geometry::{Point, Rect};
use crate::theme::Theme;

/// Высота панели в пикселях.
pub const PANEL_HEIGHT: i32 = 44;
const PADDING: i32 = 8;
const ICON_CELL: i32 = 44;
const CLOCK_WIDTH: i32 = 104;
const NETWORK_WIDTH: i32 = 36;
const CLOCK_FONT_SCALE: i32 = 1;

/// Иконки приложений на панели.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelIcon {
    Launcher,
    Terminal,
    Files,
    Paint,
}

impl PanelIcon {
    pub const ALL: [PanelIcon; 4] = [
        PanelIcon::Launcher,
        PanelIcon::Terminal,
        PanelIcon::Files,
        PanelIcon::Paint,
    ];

    pub fn index(self) -> usize {
        match self {
            Self::Launcher => 0,
            Self::Terminal => 1,
            Self::Files => 2,
            Self::Paint => 3,
        }
    }
}

/// Что находится под указателем в панели.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelHit {
    Icon(PanelIcon),
    WindowChip(u32),
    Network,
    Clock,
    Background,
}

/// Подсветка на панели.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelHover {
    Icon(usize),
    WindowChip(u32),
    Network,
    Clock,
}

pub fn panel_rect(screen: Rect) -> Rect {
    Rect::new(
        screen.x,
        screen.bottom() - PANEL_HEIGHT,
        screen.w,
        PANEL_HEIGHT,
    )
}

pub fn icon_rect(screen: Rect, index: usize) -> Rect {
    let panel = panel_rect(screen);
    Rect::new(
        panel.x + PADDING + index as i32 * ICON_CELL,
        panel.y + (PANEL_HEIGHT - ICON_CELL) / 2,
        ICON_CELL,
        ICON_CELL,
    )
}

pub fn clock_rect(screen: Rect) -> Rect {
    let panel = panel_rect(screen);
    Rect::new(
        panel.right() - PADDING - CLOCK_WIDTH,
        panel.y + 2,
        CLOCK_WIDTH,
        PANEL_HEIGHT - 4,
    )
}

pub fn network_rect(screen: Rect) -> Rect {
    let clock = clock_rect(screen);
    Rect::new(
        clock.x - 10 - NETWORK_WIDTH,
        panel_rect(screen).y + (PANEL_HEIGHT - NETWORK_WIDTH) / 2,
        NETWORK_WIDTH,
        NETWORK_WIDTH,
    )
}

/// Область, отведённая под чипы свёрнутых/открытых окон.
pub fn chip_area(screen: Rect) -> Rect {
    let panel = panel_rect(screen);
    let first = icon_rect(screen, PanelIcon::ALL.len()).x + PADDING;
    let last = network_rect(screen).x - 12;
    Rect::new(first, panel.y + 7, (last - first).max(0), PANEL_HEIGHT - 14)
}

pub const CHIP_WIDTH: i32 = 30;
const CHIP_GAP: i32 = 4;

pub fn chip_rect(screen: Rect, slot: usize) -> Rect {
    let area = chip_area(screen);
    Rect::new(
        area.x + slot as i32 * (CHIP_WIDTH + CHIP_GAP),
        area.y,
        CHIP_WIDTH,
        area.h,
    )
}

/// Состояние панели: часы, дата и доступность сети.
#[derive(Clone, Debug)]
pub struct Panel {
    pub time: String,
    pub date: String,
    pub network_online: bool,
    pub hover: Option<PanelHover>,
}

impl Panel {
    pub fn new(time: impl Into<String>, date: impl Into<String>) -> Self {
        Self {
            time: time.into(),
            date: date.into(),
            network_online: true,
            hover: None,
        }
    }

    /// Статическая геометрия без динамических чипов окон.
    pub fn hit_static(&self, screen: Rect, point: Point) -> Option<PanelHit> {
        if !panel_rect(screen).contains(point) {
            return None;
        }
        for (index, icon) in PanelIcon::ALL.iter().enumerate() {
            if icon_rect(screen, index).contains(point) {
                return Some(PanelHit::Icon(*icon));
            }
        }
        if network_rect(screen).contains(point) {
            return Some(PanelHit::Network);
        }
        if clock_rect(screen).contains(point) {
            return Some(PanelHit::Clock);
        }
        Some(PanelHit::Background)
    }

    /// Чип окна под указателем; `chips` — (window_id, подпись) в порядке отрисовки.
    pub fn hit_chip(
        &self,
        screen: Rect,
        point: Point,
        chips: &[(u32, String, bool)],
    ) -> Option<u32> {
        if !chip_area(screen).contains(point) {
            return None;
        }
        for (slot, (id, _, _)) in chips.iter().enumerate() {
            if chip_rect(screen, slot).contains(point) {
                return Some(*id);
            }
        }
        None
    }

    /// Нарисовать панель. `chips` — (window_id, подпись, окно сфокусировано).
    pub fn draw(
        &self,
        canvas: &mut Canvas,
        screen: Rect,
        theme: &Theme,
        hover: Option<PanelHover>,
        chips: &[(u32, String, bool)],
    ) {
        let panel = panel_rect(screen);
        canvas.fill_gradient(panel, theme.panel_bg_alt, theme.panel_bg);
        canvas.fill_rect(Rect::new(panel.x, panel.y, panel.w, 1), theme.panel_border);

        for (index, icon) in PanelIcon::ALL.iter().enumerate() {
            let rect = icon_rect(screen, index);
            if hover == Some(PanelHover::Icon(index)) {
                canvas.fill_rounded_rect(rect.inset(4), 8, theme.panel_hover, 220);
            }
            draw_icon(canvas, *icon, rect, theme);
        }

        for (slot, (id, label, focused)) in chips.iter().enumerate() {
            let rect = chip_rect(screen, slot);
            if rect.right() > network_rect(screen).x - 6 {
                break;
            }
            let hovered = hover == Some(PanelHover::WindowChip(*id));
            let bg = if hovered {
                theme.panel_hover
            } else {
                theme.chip_bg
            };
            canvas.fill_rounded_rect(rect, 7, bg, 240);
            let border = if *focused {
                theme.accent
            } else {
                theme.panel_border
            };
            canvas.stroke_rounded_rect(rect, 7, border, 255);
            let ch = label.chars().next().unwrap_or('?').to_ascii_uppercase();
            let mut text = [0u8; 4];
            let s = ch.encode_utf8(&mut text);
            let x = rect.x + (rect.w - 8) / 2;
            canvas.text(x, rect.y + (rect.h - 16) / 2, 1, theme.chip_text, s);
        }

        // Сеть: иконка Wi-Fi. Выключенная сеть приглушена.
        let net = network_rect(screen);
        if hover == Some(PanelHover::Network) {
            canvas.fill_rounded_rect(net.inset(2), 8, theme.panel_hover, 220);
        }
        draw_network(canvas, net, theme, self.network_online);

        // Часы и дата, выровненные по правому краю блока.
        let clock = clock_rect(screen);
        if hover == Some(PanelHover::Clock) {
            canvas.fill_rounded_rect(clock.inset(2), 8, theme.panel_hover, 160);
        }
        let time_w = self.time.chars().count() as i32 * 8 * CLOCK_FONT_SCALE;
        let date_w = self.date.chars().count() as i32 * 8 * CLOCK_FONT_SCALE;
        canvas.text(
            clock.right() - 4 - time_w,
            clock.y + 3,
            CLOCK_FONT_SCALE,
            theme.panel_text,
            &self.time,
        );
        canvas.text(
            clock.right() - 4 - date_w,
            clock.y + 22,
            CLOCK_FONT_SCALE,
            theme.panel_text_dim,
            &self.date,
        );
    }
}

fn draw_icon(canvas: &mut Canvas, icon: PanelIcon, cell: Rect, theme: &Theme) {
    let center = cell.center();
    match icon {
        PanelIcon::Launcher => {
            // 3x3 сетка квадратиков.
            let size = 4;
            let gap = 3;
            let total = size * 3 + gap * 2;
            let x0 = center.x - total / 2;
            let y0 = center.y - total / 2;
            for row in 0..3 {
                for col in 0..3 {
                    let color = if row == 1 && col == 1 {
                        theme.accent
                    } else {
                        theme.panel_icon
                    };
                    canvas.fill_rounded_rect(
                        Rect::new(x0 + col * (size + gap), y0 + row * (size + gap), size, size),
                        1,
                        color,
                        255,
                    );
                }
            }
        }
        PanelIcon::Terminal => {
            let r = Rect::new(center.x - 12, center.y - 9, 24, 18);
            canvas.fill_rounded_rect(r, 4, theme.panel_bg, 255);
            canvas.stroke_rounded_rect(r, 4, theme.panel_icon, 255);
            canvas.line(
                Point::new(center.x - 7, center.y - 4),
                Point::new(center.x - 3, center.y),
                theme.accent,
                2,
            );
            canvas.line(
                Point::new(center.x - 3, center.y),
                Point::new(center.x - 7, center.y + 4),
                theme.accent,
                2,
            );
            canvas.hline(center.x - 1, center.x + 6, center.y + 4, theme.panel_icon);
        }
        PanelIcon::Files => {
            let body = Rect::new(center.x - 11, center.y - 8, 22, 16);
            canvas.fill_rounded_rect(body, 3, theme.accent_dim, 255);
            canvas.fill_rounded_rect(
                Rect::new(center.x - 11, center.y - 11, 10, 5),
                2,
                theme.accent_dim,
                255,
            );
            canvas.fill_rounded_rect(body.inset(2), 2, theme.panel_bg_alt, 255);
            canvas.hline(body.x + 4, body.right() - 4, body.y + 6, theme.panel_icon);
        }
        PanelIcon::Paint => {
            canvas.fill_circle(center, 9, theme.accent_dim, 255);
            canvas.fill_circle(Point::new(center.x, center.y), 5, theme.panel_bg, 255);
            for (dx, dy, color) in [
                (-5, -4, crate::color::rgb(232, 92, 92)),
                (5, -4, crate::color::rgb(235, 200, 90)),
                (0, 5, theme.accent),
            ] {
                canvas.fill_circle(Point::new(center.x + dx, center.y + dy), 2, color, 255);
            }
        }
    }
}

fn draw_network(canvas: &mut Canvas, rect: Rect, theme: &Theme, online: bool) {
    let color: Color = if online {
        theme.panel_icon
    } else {
        theme.panel_icon_dim
    };
    let center = Point::new(rect.center().x, rect.bottom() - 8);
    canvas.fill_circle(center, 2, if online { theme.accent } else { color }, 255);
    for (radius, thickness) in [(6, 2), (11, 2), (16, 2)] {
        canvas.arc(center, radius, 218.0, 322.0, color, thickness);
    }
    if !online {
        canvas.line(
            Point::new(rect.x + 6, rect.bottom() - 6),
            Point::new(rect.right() - 6, rect.y + 6),
            theme.button_close,
            2,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect::new(0, 0, 1280, 720);

    #[test]
    fn panel_is_44px_at_bottom() {
        let panel = panel_rect(SCREEN);
        assert_eq!(panel.y, 720 - 44);
        assert_eq!(panel.h, 44);
        assert_eq!(panel.w, 1280);
    }

    #[test]
    fn controls_do_not_overlap() {
        let clock = clock_rect(SCREEN);
        let network = network_rect(SCREEN);
        let area = chip_area(SCREEN);
        assert!(clock.contains(clock.center()));
        assert!(network.right() < clock.x);
        assert!(area.right() < network.x);
        assert!(area.x > icon_rect(SCREEN, 3).right());
        for i in 0..4 {
            assert!(!icon_rect(SCREEN, i).intersects(icon_rect(SCREEN, i + 1)));
        }
    }

    #[test]
    fn static_hits_resolve_controls() {
        let panel = Panel::new("23:38", "15.11.2026");
        assert_eq!(
            panel.hit_static(SCREEN, icon_rect(SCREEN, 1).center()),
            Some(PanelHit::Icon(PanelIcon::Terminal))
        );
        assert_eq!(
            panel.hit_static(SCREEN, network_rect(SCREEN).center()),
            Some(PanelHit::Network)
        );
        assert_eq!(
            panel.hit_static(SCREEN, clock_rect(SCREEN).center()),
            Some(PanelHit::Clock)
        );
        assert_eq!(
            panel.hit_static(SCREEN, Point::new(500, 700)),
            Some(PanelHit::Background)
        );
        assert_eq!(panel.hit_static(SCREEN, Point::new(500, 600)), None);
    }

    #[test]
    fn draws_clock_and_network_pixels() {
        let mut canvas = Canvas::new(1280, 720, crate::color::rgb(0, 0, 0));
        let panel = Panel::new("23:38", "15.11.2026");
        panel.draw(&mut canvas, SCREEN, &Theme::default(), None, &[]);
        // Часы и дата оставляют пиксели в своём блоке.
        let clock = clock_rect(SCREEN);
        let lit = canvas
            .pixels()
            .chunks(1280)
            .skip(clock.y as usize)
            .take(clock.h as usize)
            .flat_map(|row| &row[clock.x as usize..clock.right() as usize])
            .filter(|p| crate::color::red(**p) > 180)
            .count();
        assert!(
            lit > 40,
            "часы и дата должны быть нарисованы, пикселей {lit}"
        );
    }
}
