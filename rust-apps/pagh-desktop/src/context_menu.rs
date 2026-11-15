//! Контекстное меню рабочего стола и окна.

use crate::canvas::Canvas;
use crate::font::GLYPH_WIDTH;
use crate::geometry::{Point, Rect};
use crate::theme::Theme;

/// Действие пункта меню.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    OpenTerminal,
    OpenFiles,
    OpenPaint,
    CloseWindow,
    MinimizeWindow,
    MaximizeWindow,
    MoveWindow,
    PanelSettings,
    ChangeWallpaper,
    Separator,
}

/// Пункт меню.
#[derive(Clone, Debug)]
pub struct MenuItem {
    pub label: &'static str,
    pub action: MenuAction,
    pub enabled: bool,
}

impl MenuItem {
    pub fn new(label: &'static str, action: MenuAction) -> Self {
        Self {
            label,
            action,
            enabled: true,
        }
    }

    pub fn disabled(label: &'static str, action: MenuAction) -> Self {
        Self {
            label,
            action,
            enabled: false,
        }
    }

    fn separator() -> Self {
        Self {
            label: "",
            action: MenuAction::Separator,
            enabled: false,
        }
    }
}

const ITEM_HEIGHT: i32 = 30;
const SEPARATOR_HEIGHT: i32 = 11;
const PADDING: i32 = 6;
const LABEL_LEFT: i32 = 14;
const MIN_WIDTH: i32 = 208;

/// Открытое контекстное меню.
#[derive(Clone, Debug)]
pub struct ContextMenu {
    pub rect: Rect,
    pub items: Vec<MenuItem>,
    pub hover: Option<usize>,
    /// Окно, к которому относится меню (`None` — рабочий стол).
    pub target_window: Option<u32>,
    pub position: Point,
}

impl ContextMenu {
    pub fn desktop(position: Point) -> Self {
        Self {
            rect: Rect::ZERO,
            items: vec![
                MenuItem::new("Open Terminal", MenuAction::OpenTerminal),
                MenuItem::new("Open Files", MenuAction::OpenFiles),
                MenuItem::new("Open Paint", MenuAction::OpenPaint),
                MenuItem::separator(),
                MenuItem::new("Change Wallpaper", MenuAction::ChangeWallpaper),
                MenuItem::disabled("Display Settings", MenuAction::PanelSettings),
            ],
            hover: None,
            target_window: None,
            position,
        }
        .laid_out()
    }

    pub fn for_window(position: Point, window_id: u32, maximized: bool) -> Self {
        Self {
            rect: Rect::ZERO,
            items: vec![
                MenuItem::new("Move", MenuAction::MoveWindow),
                MenuItem::new("Minimize", MenuAction::MinimizeWindow),
                MenuItem::new(
                    if maximized { "Restore" } else { "Maximize" },
                    MenuAction::MaximizeWindow,
                ),
                MenuItem::separator(),
                MenuItem::new("Close", MenuAction::CloseWindow),
            ],
            hover: None,
            target_window: Some(window_id),
            position,
        }
        .laid_out()
    }

    fn laid_out(mut self) -> Self {
        let width = self
            .items
            .iter()
            .map(|item| item.label.chars().count() as i32 * GLYPH_WIDTH + LABEL_LEFT + 18)
            .max()
            .unwrap_or(MIN_WIDTH)
            .max(MIN_WIDTH);
        let height = PADDING * 2
            + self
                .items
                .iter()
                .map(|item| {
                    if matches!(item.action, MenuAction::Separator) {
                        SEPARATOR_HEIGHT
                    } else {
                        ITEM_HEIGHT
                    }
                })
                .sum::<i32>();
        self.rect = Rect::new(self.position.x, self.position.y, width, height);
        self
    }

    pub fn clamp_to(&mut self, screen: Rect) {
        if self.rect.right() > screen.right() {
            self.rect.x = (screen.right() - self.rect.w).max(0);
        }
        if self.rect.bottom() > screen.bottom() {
            self.rect.y = (screen.bottom() - self.rect.h).max(0);
        }
        self.rect.x = self.rect.x.max(0);
        self.rect.y = self.rect.y.max(0);
    }

    /// Индекс пункта под точкой (разделители не выбираются).
    pub fn item_at(&self, point: Point) -> Option<usize> {
        if !self.rect.contains(point) {
            return None;
        }
        let mut y = self.rect.y + PADDING;
        for (index, item) in self.items.iter().enumerate() {
            let height = if matches!(item.action, MenuAction::Separator) {
                SEPARATOR_HEIGHT
            } else {
                ITEM_HEIGHT
            };
            let item_rect = Rect::new(self.rect.x, y, self.rect.w, height);
            if item_rect.contains(point) {
                if item.enabled && !matches!(item.action, MenuAction::Separator) {
                    return Some(index);
                }
                return None;
            }
            y += height;
        }
        None
    }

    pub fn item_rect(&self, index: usize) -> Rect {
        let mut y = self.rect.y + PADDING;
        for (i, item) in self.items.iter().enumerate() {
            let height = if matches!(item.action, MenuAction::Separator) {
                SEPARATOR_HEIGHT
            } else {
                ITEM_HEIGHT
            };
            if i == index {
                return Rect::new(self.rect.x, y, self.rect.w, height);
            }
            y += height;
        }
        Rect::ZERO
    }

    pub fn action_at(&self, point: Point) -> Option<MenuAction> {
        self.item_at(point).map(|index| self.items[index].action)
    }

    pub fn draw(&self, canvas: &mut Canvas, theme: &Theme) {
        canvas.shadow(self.rect, 8, 8, 16);
        canvas.fill_rounded_rect(self.rect, 8, theme.menu_bg, 250);
        canvas.stroke_rounded_rect(self.rect, 8, theme.menu_border, 255);
        let mut y = self.rect.y + PADDING;
        for (index, item) in self.items.iter().enumerate() {
            if matches!(item.action, MenuAction::Separator) {
                canvas.hline(
                    self.rect.x + 10,
                    self.rect.right() - 10,
                    y + SEPARATOR_HEIGHT / 2,
                    theme.separator,
                );
                y += SEPARATOR_HEIGHT;
                continue;
            }
            if self.hover == Some(index) {
                canvas.fill_rounded_rect(
                    Rect::new(self.rect.x + 3, y, self.rect.w - 6, ITEM_HEIGHT),
                    5,
                    theme.menu_hover,
                    220,
                );
            }
            let color = if item.enabled {
                theme.menu_text
            } else {
                theme.menu_text_dim
            };
            canvas.text(
                self.rect.x + LABEL_LEFT,
                y + (ITEM_HEIGHT - 16) / 2,
                1,
                color,
                item.label,
            );
            y += ITEM_HEIGHT;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_layout_contains_items() {
        let menu = ContextMenu::desktop(Point::new(100, 100));
        assert_eq!(menu.items.len(), 6);
        assert_eq!(menu.rect.x, 100);
        assert_eq!(menu.rect.y, 100);
        assert_eq!(menu.item_at(Point::new(110, 110)), Some(0));
        assert_eq!(
            menu.action_at(Point::new(110, 110)),
            Some(MenuAction::OpenTerminal)
        );
        let first = menu.item_rect(0);
        assert_eq!(first.h, ITEM_HEIGHT);
        // Разделитель не выбирается.
        let sep = menu.item_rect(3);
        assert_eq!(menu.item_at(sep.center()), None);
    }

    #[test]
    fn disabled_items_are_not_selected() {
        let menu = ContextMenu::desktop(Point::new(0, 0));
        let display = menu
            .items
            .iter()
            .position(|i| i.label == "Display Settings")
            .unwrap();
        let rect = menu.item_rect(display);
        assert_eq!(menu.item_at(rect.center()), None);
    }

    #[test]
    fn window_menu_actions() {
        let menu = ContextMenu::for_window(Point::new(0, 0), 42, false);
        assert_eq!(menu.target_window, Some(42));
        assert_eq!(
            menu.action_at(menu.item_rect(0).center()),
            Some(MenuAction::MoveWindow)
        );
        assert_eq!(
            menu.action_at(menu.item_rect(4).center()),
            Some(MenuAction::CloseWindow)
        );
    }

    #[test]
    fn clamp_keeps_menu_on_screen() {
        let mut menu = ContextMenu::desktop(Point::new(1200, 700));
        menu.clamp_to(Rect::new(0, 0, 1280, 720));
        assert_eq!(menu.rect.right(), 1280);
        assert!(menu.rect.bottom() <= 720);
    }

    #[test]
    fn draws_visible_menu() {
        let mut canvas = Canvas::new(400, 400, crate::color::rgb(0, 0, 0));
        let menu = ContextMenu::desktop(Point::new(20, 20));
        menu.draw(&mut canvas, &Theme::default());
        let center = menu.rect.center();
        assert_ne!(canvas.get(center.x, center.y), 0);
    }
}
