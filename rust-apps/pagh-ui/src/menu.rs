//! Контекстные меню и подменю.
//!
//! Меню — это данные плюс чистая функция раскладки/отрисовки. Приложение
//! хранит [`Menu`] и состояние (какой пункт под курсором, открыто ли подменю),
//! а тулкит возвращает [`MenuLayout`] с прямоугольниками строк для hit-тестов и
//! точкой открытия подменю. Ширина меню считается по содержимому, как в
//! Dolphin/KDE: колонка иконок, подписи, горячие клавиши, стрелка подменю.

use crate::canvas::Canvas;
use crate::color::Color;
use crate::font;
use crate::geom::{Point, Rect, Size};
use crate::icon::{self, Icon};
use crate::theme::Theme;

const SEPARATOR_HEIGHT: i32 = 9;
const ICON_COLUMN: i32 = 22;
const SHORTCUT_GAP: i32 = 26;
const ARROW_COLUMN: i32 = 18;
const ITEM_PADDING: i32 = 3;
const PAD_X: i32 = 12;

/// Пункт меню.
#[derive(Clone, Debug)]
pub enum MenuEntry {
    /// Обычное действие с идентификатором.
    Action {
        id: u32,
        label: String,
        icon: Option<Icon>,
        shortcut: Option<String>,
        enabled: bool,
    },
    /// Пункт с вложенным меню.
    Submenu {
        label: String,
        icon: Option<Icon>,
        menu: Menu,
        enabled: bool,
    },
    /// Горизонтальная черта-разделитель.
    Separator,
}

impl MenuEntry {
    /// Создать действие.
    pub fn action(id: u32, label: impl Into<String>) -> MenuEntry {
        MenuEntry::Action {
            id,
            label: label.into(),
            icon: None,
            shortcut: None,
            enabled: true,
        }
    }

    /// Действие с иконкой.
    pub fn action_icon(id: u32, label: impl Into<String>, icon: Icon) -> MenuEntry {
        match MenuEntry::action(id, label) {
            MenuEntry::Action {
                id,
                label,
                shortcut,
                enabled,
                ..
            } => MenuEntry::Action {
                id,
                label,
                icon: Some(icon),
                shortcut,
                enabled,
            },
            other => other,
        }
    }

    /// Действие с горячей клавишей.
    pub fn action_shortcut(
        id: u32,
        label: impl Into<String>,
        shortcut: impl Into<String>,
    ) -> MenuEntry {
        match MenuEntry::action(id, label) {
            MenuEntry::Action {
                id,
                label,
                icon,
                enabled,
                ..
            } => MenuEntry::Action {
                id,
                label,
                icon,
                shortcut: Some(shortcut.into()),
                enabled,
            },
            other => other,
        }
    }

    /// Сделать пункт недоступным.
    pub fn disabled(self) -> MenuEntry {
        match self {
            MenuEntry::Action {
                id,
                label,
                icon,
                shortcut,
                ..
            } => MenuEntry::Action {
                id,
                label,
                icon,
                shortcut,
                enabled: false,
            },
            MenuEntry::Submenu {
                label, icon, menu, ..
            } => MenuEntry::Submenu {
                label,
                icon,
                menu,
                enabled: false,
            },
            other => other,
        }
    }

    /// Пункт с подменю.
    pub fn submenu(label: impl Into<String>, menu: Menu) -> MenuEntry {
        MenuEntry::Submenu {
            label: label.into(),
            icon: None,
            menu,
            enabled: true,
        }
    }

    /// Пункт с подменю и иконкой.
    pub fn submenu_icon(label: impl Into<String>, icon: Icon, menu: Menu) -> MenuEntry {
        MenuEntry::Submenu {
            label: label.into(),
            icon: Some(icon),
            menu,
            enabled: true,
        }
    }

    pub fn label(&self) -> Option<&str> {
        match self {
            MenuEntry::Action { label, .. } | MenuEntry::Submenu { label, .. } => Some(label),
            MenuEntry::Separator => None,
        }
    }

    pub fn is_enabled(&self) -> bool {
        match self {
            MenuEntry::Action { enabled, .. } => *enabled,
            MenuEntry::Submenu { enabled, .. } => *enabled,
            MenuEntry::Separator => false,
        }
    }

    pub fn submenu_of(&self) -> Option<&Menu> {
        match self {
            MenuEntry::Submenu { menu, .. } => Some(menu),
            _ => None,
        }
    }

    pub fn id(&self) -> Option<u32> {
        match self {
            MenuEntry::Action { id, .. } => Some(*id),
            _ => None,
        }
    }
}

/// Меню: упорядоченный список пунктов.
#[derive(Clone, Debug, Default)]
pub struct Menu {
    pub entries: Vec<MenuEntry>,
}

impl Menu {
    pub fn new() -> Menu {
        Menu::default()
    }

    pub fn from_entries(entries: Vec<MenuEntry>) -> Menu {
        Menu { entries }
    }

    pub fn push(mut self, entry: MenuEntry) -> Menu {
        self.entries.push(entry);
        self
    }

    /// Найти пункт по идентификатору действия.
    pub fn find(&self, id: u32) -> Option<&MenuEntry> {
        self.entries.iter().find(|entry| entry.id() == Some(id))
    }

    /// Доступно ли действие (для включения пункта).
    pub fn is_enabled(&self, id: u32) -> bool {
        self.find(id)
            .map(|entry| entry.is_enabled())
            .unwrap_or(false)
    }

    fn has_icons(&self) -> bool {
        self.entries.iter().any(|entry| match entry {
            MenuEntry::Action { icon, .. } | MenuEntry::Submenu { icon, .. } => icon.is_some(),
            MenuEntry::Separator => false,
        })
    }

    fn has_shortcuts(&self) -> bool {
        self.entries.iter().any(|entry| {
            matches!(
                entry,
                MenuEntry::Action {
                    shortcut: Some(_),
                    ..
                }
            )
        })
    }

    fn has_submenus(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry, MenuEntry::Submenu { .. }))
    }

    fn shortcut_width(&self) -> i32 {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Action {
                    shortcut: Some(text),
                    ..
                } => Some(font::measure(text)),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }
}

/// Строка меню в раскладке.
#[derive(Clone, Copy, Debug)]
pub struct MenuRow {
    pub index: usize,
    pub rect: Rect,
}

/// Результат раскладки/отрисовки меню.
#[derive(Clone, Debug, Default)]
pub struct MenuLayout {
    pub rect: Rect,
    pub rows: Vec<MenuRow>,
}

impl MenuLayout {
    /// Индекс пункта под точкой (включая подменю; разделители не попадают).
    pub fn hit(&self, point: Point) -> Option<usize> {
        if !self.rect.contains(point) {
            return None;
        }
        self.rows
            .iter()
            .find(|row| row.rect.contains(point))
            .map(|row| row.index)
    }

    pub fn row(&self, index: usize) -> Option<Rect> {
        self.rows
            .iter()
            .find(|row| row.index == index)
            .map(|row| row.rect)
    }

    /// Точка открытия подменю рядом со строкой `index`.
    pub fn submenu_origin(&self, index: usize) -> Option<Point> {
        self.row(index)
            .map(|row| Point::new(self.rect.right() - 6, row.y - ITEM_PADDING))
    }
}

/// Размер меню по его содержимому.
pub fn measure(theme: &Theme, menu: &Menu) -> Size {
    let metrics = &theme.metrics;
    let icon_column = if menu.has_icons() { ICON_COLUMN } else { 0 };
    let arrow_column = if menu.has_submenus() { ARROW_COLUMN } else { 0 };
    let shortcut_column = if menu.has_shortcuts() {
        menu.shortcut_width() + SHORTCUT_GAP
    } else {
        0
    };
    let mut label_width = 0;
    let mut height = metrics.menu_padding * 2;
    for entry in &menu.entries {
        match entry {
            MenuEntry::Separator => height += SEPARATOR_HEIGHT,
            MenuEntry::Action { label, .. } | MenuEntry::Submenu { label, .. } => {
                height += metrics.menu_row_height;
                label_width = label_width.max(font::measure(label));
            }
        }
    }
    let width = (PAD_X * 2 + icon_column + label_width + shortcut_column + arrow_column)
        .max(metrics.menu_min_width);
    Size::new(width, height)
}

/// Геометрия меню без отрисовки (для hit-тестов в приложении).
pub fn layout(theme: &Theme, menu: &Menu, origin: Point) -> MenuLayout {
    let size = measure(theme, menu);
    let rect = Rect::new(origin.x, origin.y, size.w, size.h);
    let mut rows = Vec::new();
    let mut y = rect.y + theme.metrics.menu_padding;
    for (index, entry) in menu.entries.iter().enumerate() {
        match entry {
            MenuEntry::Separator => y += SEPARATOR_HEIGHT,
            _ => {
                rows.push(MenuRow {
                    index,
                    rect: Rect::new(
                        rect.x + ITEM_PADDING,
                        y,
                        rect.w - ITEM_PADDING * 2,
                        theme.metrics.menu_row_height,
                    ),
                });
                y += theme.metrics.menu_row_height;
            }
        }
    }
    MenuLayout { rect, rows }
}

/// Нарисовать меню; `hovered` — индекс пункта под курсором.
pub fn draw(
    canvas: &mut Canvas,
    theme: &Theme,
    menu: &Menu,
    origin: Point,
    hovered: Option<usize>,
) -> MenuLayout {
    let palette = &theme.palette;
    let metrics = &theme.metrics;
    let result = layout(theme, menu, origin);
    let rect = result.rect;
    let radius = metrics.radius_card;
    canvas.soft_shadow(rect, radius, 18, palette.shadow);
    canvas.fill_rounded(rect, radius, palette.menu);
    canvas.stroke_rounded(rect, radius, 1, palette.menu_border);

    let icon_column = if menu.has_icons() { ICON_COLUMN } else { 0 };
    let shortcut_column = if menu.has_shortcuts() {
        menu.shortcut_width() + SHORTCUT_GAP
    } else {
        0
    };
    let text_x = rect.x + PAD_X + icon_column;
    let label_max = rect.w
        - PAD_X * 2
        - icon_column
        - shortcut_column
        - if menu.has_submenus() { ARROW_COLUMN } else { 0 };

    let mut y = rect.y + metrics.menu_padding;
    for (index, entry) in menu.entries.iter().enumerate() {
        match entry {
            MenuEntry::Separator => {
                canvas.hline(
                    y + SEPARATOR_HEIGHT / 2,
                    rect.x + 8,
                    rect.right() - 8,
                    palette.separator,
                );
                y += SEPARATOR_HEIGHT;
            }
            MenuEntry::Action {
                label,
                shortcut,
                icon,
                enabled,
                ..
            } => {
                let row_rect = Rect::new(
                    rect.x + ITEM_PADDING,
                    y,
                    rect.w - ITEM_PADDING * 2,
                    metrics.menu_row_height,
                );
                let is_hovered = hovered == Some(index) && *enabled;
                if is_hovered {
                    canvas.fill_rounded(row_rect, metrics.radius_control, palette.menu_hover);
                }
                let text_color = if *enabled {
                    palette.menu_text
                } else {
                    palette.menu_text_disabled
                };
                let icon_color = if is_hovered {
                    palette.accent
                } else if *enabled {
                    palette.text_muted
                } else {
                    palette.menu_text_disabled
                };
                if let Some(icon) = icon {
                    icon::draw(
                        canvas,
                        *icon,
                        Rect::new(
                            rect.x + PAD_X - 2,
                            y + (metrics.menu_row_height - metrics.icon_size) / 2,
                            metrics.icon_size,
                            metrics.icon_size,
                        ),
                        icon_color,
                        palette,
                    );
                }
                let baseline = y + metrics.menu_row_height / 2 + font::ascent() / 2;
                font::draw(
                    canvas,
                    text_x,
                    baseline,
                    &font::ellipsize(label, label_max),
                    text_color,
                );
                if let Some(shortcut) = shortcut {
                    let shortcut_w = font::measure(shortcut);
                    font::draw(
                        canvas,
                        rect.right()
                            - PAD_X
                            - if menu.has_submenus() { ARROW_COLUMN } else { 0 }
                            - shortcut_w,
                        baseline,
                        shortcut,
                        if *enabled {
                            palette.menu_shortcut
                        } else {
                            palette.menu_text_disabled
                        },
                    );
                }
                let _ = index;
                y += metrics.menu_row_height;
            }
            MenuEntry::Submenu {
                label,
                icon,
                enabled,
                ..
            } => {
                let row_rect = Rect::new(
                    rect.x + ITEM_PADDING,
                    y,
                    rect.w - ITEM_PADDING * 2,
                    metrics.menu_row_height,
                );
                let is_hovered = hovered == Some(index) && *enabled;
                if is_hovered {
                    canvas.fill_rounded(row_rect, metrics.radius_control, palette.menu_hover);
                }
                if let Some(icon) = icon {
                    icon::draw(
                        canvas,
                        *icon,
                        Rect::new(
                            rect.x + PAD_X - 2,
                            y + (metrics.menu_row_height - metrics.icon_size) / 2,
                            metrics.icon_size,
                            metrics.icon_size,
                        ),
                        if is_hovered {
                            palette.accent
                        } else if *enabled {
                            palette.text_muted
                        } else {
                            palette.menu_text_disabled
                        },
                        palette,
                    );
                }
                let baseline = y + metrics.menu_row_height / 2 + font::ascent() / 2;
                font::draw(
                    canvas,
                    text_x,
                    baseline,
                    &font::ellipsize(label, label_max),
                    if *enabled {
                        palette.menu_text
                    } else {
                        palette.menu_text_disabled
                    },
                );
                let arrow_size = 10;
                icon::draw(
                    canvas,
                    Icon::ChevronRight,
                    Rect::new(
                        rect.right() - PAD_X - arrow_size,
                        y + (metrics.menu_row_height - arrow_size) / 2,
                        arrow_size,
                        arrow_size,
                    ),
                    if is_hovered {
                        palette.accent
                    } else if *enabled {
                        palette.menu_shortcut
                    } else {
                        palette.menu_text_disabled
                    },
                    palette,
                );
                let _ = index;
                y += metrics.menu_row_height;
            }
        }
    }
    let _ = Color::TRANSPARENT;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn sample_menu() -> Menu {
        Menu::from_entries(vec![
            MenuEntry::action(1, "Создать"),
            MenuEntry::submenu(
                "Создать",
                Menu::from_entries(vec![
                    MenuEntry::action(11, "Папку…"),
                    MenuEntry::action(12, "Текстовый файл…"),
                    MenuEntry::Separator,
                    MenuEntry::action(13, "Ярлык для адреса в Интернете (URL)…"),
                ]),
            ),
            MenuEntry::action_shortcut(2, "Вставить содержимое буфера обмена…", "Ctrl+V"),
            MenuEntry::Separator,
            MenuEntry::action_shortcut(3, "Свойства", "Alt+Return").disabled(),
        ])
    }

    #[test]
    fn measure_grows_with_content() {
        let theme = Theme::pagh_dark();
        let small = Menu::from_entries(vec![MenuEntry::action(1, "ОК")]);
        let big = sample_menu();
        assert!(measure(&theme, &big).w > measure(&theme, &small).w);
        assert!(measure(&theme, &big).h > measure(&theme, &small).h);
        assert!(measure(&theme, &small).w >= theme.metrics.menu_min_width);
    }

    #[test]
    fn layout_rows_and_hit_test() {
        let theme = Theme::pagh_dark();
        let menu = sample_menu();
        let mut canvas = Canvas::new(400, 400);
        let layout = draw(&mut canvas, &theme, &menu, Point::new(10, 10), Some(1));
        assert_eq!(layout.rows.len(), 4); // 3 действия + подменю; разделитель не строка
        assert_eq!(
            layout.hit(Point::new(20, 10 + theme.metrics.menu_padding + 2)),
            Some(0)
        );
        let submenu_row = layout.row(1).unwrap();
        assert_eq!(layout.hit(submenu_row.origin().offset(4, 4)), Some(1));
        assert_eq!(layout.hit(Point::new(1000, 1000)), None);
        let origin = layout.submenu_origin(1).unwrap();
        assert_eq!(origin.x, layout.rect.right() - 6);
    }

    #[test]
    fn disabled_action_is_not_hovered() {
        let theme = Theme::pagh_dark();
        let menu = Menu::from_entries(vec![MenuEntry::action(1, "Нельзя").disabled()]);
        let mut canvas = Canvas::new(300, 120);
        let layout = draw(&mut canvas, &theme, &menu, Point::new(0, 0), Some(0));
        let row = layout.row(0).unwrap();
        assert_eq!(
            canvas.pixel(Point::new(row.x + 4, row.center().y)),
            theme.palette.menu
        );
    }

    #[test]
    fn separator_is_drawn() {
        let theme = Theme::pagh_dark();
        let menu = Menu::from_entries(vec![
            MenuEntry::action(1, "Раз"),
            MenuEntry::Separator,
            MenuEntry::action(2, "Два"),
        ]);
        let mut canvas = Canvas::new(300, 160);
        let layout = draw(&mut canvas, &theme, &menu, Point::new(0, 0), None);
        assert_eq!(layout.rows.len(), 2);
        assert!(layout.row(2).unwrap().y >= layout.row(0).unwrap().bottom());
    }

    #[test]
    fn submenu_is_reachable() {
        let menu = sample_menu();
        let sub = menu
            .entries
            .iter()
            .find_map(|entry| entry.submenu_of())
            .expect("подменю");
        assert_eq!(sub.entries.len(), 4);
        assert_eq!(
            sub.find(11).map(|entry| entry.label()),
            Some(Some("Папку…"))
        );
    }
}
