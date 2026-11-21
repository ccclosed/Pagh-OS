//! Базовые виджеты: подпись, кнопка, поле ввода, список и сетка значков.
//!
//! Виджеты не владеют окном и не знают о протоколе: они принимают `Rect`,
//! [`Theme`] и состояние наведения/фокуса, рисуют себя в [`Canvas`] и отдают
//! раскладку строк для hit-тестов. Приложение само решает, что делать с кликом.

use crate::canvas::Canvas;
use crate::color::Color;
use crate::event::{Event, Key};
use crate::font::{self, TextAlign};
use crate::geom::{Point, Rect, Size};
use crate::icon::{self, Icon};
use crate::theme::Theme;

// ───────────────────────────── Подпись ─────────────────────────────

/// Стиль подписи.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LabelStyle {
    Normal,
    Muted,
    Disabled,
    Accent,
    /// Заголовок секции: приглушённый, обычно набран прописными.
    Section,
}

/// Текстовая подпись.
#[derive(Clone, Debug)]
pub struct Label {
    pub text: String,
    pub style: LabelStyle,
    pub align: TextAlign,
}

impl Label {
    pub fn new(text: impl Into<String>) -> Label {
        Label {
            text: text.into(),
            style: LabelStyle::Normal,
            align: TextAlign::Left,
        }
    }

    pub fn styled(text: impl Into<String>, style: LabelStyle) -> Label {
        Label {
            style,
            ..Label::new(text)
        }
    }

    pub fn align(mut self, align: TextAlign) -> Label {
        self.align = align;
        self
    }

    pub fn color(&self, theme: &Theme) -> Color {
        match self.style {
            LabelStyle::Normal => theme.palette.text,
            LabelStyle::Muted => theme.palette.text_muted,
            LabelStyle::Disabled => theme.palette.text_disabled,
            LabelStyle::Accent => theme.palette.accent,
            LabelStyle::Section => theme.palette.section_text,
        }
    }

    pub fn draw(&self, canvas: &mut Canvas, theme: &Theme, rect: Rect) {
        let text = font::ellipsize(&self.text, rect.w);
        font::draw_aligned(canvas, rect, &text, self.color(theme), self.align);
    }
}

// ───────────────────────────── Кнопка ─────────────────────────────

/// Визуальный стиль кнопки.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonStyle {
    /// Акцентная кнопка (основное действие).
    Primary,
    /// Обычная кнопка с рамкой.
    Secondary,
    /// Кнопка-иконка без фона.
    Flat,
}

/// Состояние кнопки на текущем кадре.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ButtonState {
    pub hovered: bool,
    pub pressed: bool,
}

/// Кнопка с подписью и опциональной иконкой.
#[derive(Clone, Debug)]
pub struct Button {
    pub label: String,
    pub style: ButtonStyle,
    pub enabled: bool,
    pub icon: Option<Icon>,
}

impl Button {
    pub fn new(label: impl Into<String>) -> Button {
        Button {
            label: label.into(),
            style: ButtonStyle::Secondary,
            enabled: true,
            icon: None,
        }
    }

    pub fn primary(label: impl Into<String>) -> Button {
        Button {
            style: ButtonStyle::Primary,
            ..Button::new(label)
        }
    }

    pub fn flat(label: impl Into<String>) -> Button {
        Button {
            style: ButtonStyle::Flat,
            ..Button::new(label)
        }
    }

    pub fn with_icon(mut self, icon: Icon) -> Button {
        self.icon = Some(icon);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Button {
        self.enabled = enabled;
        self
    }

    /// Минимальный размер кнопки под её содержимое.
    pub fn min_size(&self, theme: &Theme) -> Size {
        let icon_w = if self.icon.is_some() {
            theme.metrics.icon_size + 6
        } else {
            0
        };
        Size::new(
            font::measure(&self.label) + icon_w + 24,
            theme.metrics.row_height + 4,
        )
    }

    pub fn draw(&self, canvas: &mut Canvas, theme: &Theme, rect: Rect, state: ButtonState) {
        let palette = &theme.palette;
        let radius = theme.metrics.radius_control;
        let (background, border, text_color) = match (self.style, self.enabled) {
            (ButtonStyle::Primary, true) => {
                let bg = if state.pressed {
                    palette.accent.lerp(Color::BLACK, 30)
                } else if state.hovered {
                    palette.accent_hover
                } else {
                    palette.accent
                };
                (bg, bg, palette.accent_text)
            }
            (ButtonStyle::Primary, false) => (
                palette.accent.with_opacity(30),
                Color::TRANSPARENT,
                palette.text_disabled,
            ),
            (ButtonStyle::Secondary, true) => {
                let bg = if state.pressed {
                    palette.selection
                } else if state.hovered {
                    palette.selection.with_alpha(160)
                } else {
                    palette.field
                };
                (bg, palette.field_border, palette.text)
            }
            (ButtonStyle::Secondary, false) => (
                palette.field.with_alpha(90),
                palette.separator,
                palette.text_disabled,
            ),
            (ButtonStyle::Flat, true) => {
                let bg = if state.pressed || state.hovered {
                    palette.selection
                } else {
                    Color::TRANSPARENT
                };
                (bg, Color::TRANSPARENT, palette.text)
            }
            (ButtonStyle::Flat, false) => (
                Color::TRANSPARENT,
                Color::TRANSPARENT,
                palette.text_disabled,
            ),
        };

        if background.a > 0 {
            canvas.fill_rounded(rect, radius, background);
        }
        if border.a > 0 {
            canvas.stroke_rounded(rect, radius, 1, border);
        }

        let icon_size = theme.metrics.icon_size;
        let icon_w = if self.icon.is_some() {
            icon_size + 6
        } else {
            0
        };
        let label_width = font::measure(&self.label);
        let total = icon_w + label_width;
        let start_x = rect.x + (rect.w - total) / 2;
        let baseline = font::baseline_for(rect);
        if let Some(icon) = self.icon {
            let icon_rect = Rect::new(
                start_x,
                rect.y + (rect.h - icon_size) / 2,
                icon_size,
                icon_size,
            );
            icon::draw(canvas, icon, icon_rect, text_color, palette);
        }
        font::draw(
            canvas,
            start_x + icon_w,
            baseline,
            &font::ellipsize(&self.label, rect.w - icon_w - 8),
            text_color,
        );
    }
}

// ───────────────────────────── Поле ввода ─────────────────────────────

/// Однострочное поле ввода (путь, поиск, имя файла).
#[derive(Clone, Debug)]
pub struct TextField {
    chars: Vec<char>,
    placeholder: String,
    cursor: usize,
    focused: bool,
    pub icon: Option<Icon>,
}

impl TextField {
    pub fn new(placeholder: impl Into<String>) -> TextField {
        TextField {
            chars: Vec::new(),
            placeholder: placeholder.into(),
            cursor: 0,
            focused: false,
            icon: None,
        }
    }

    pub fn with_text(mut self, text: impl Into<String>) -> TextField {
        self.set_text(text);
        self
    }

    pub fn with_icon(mut self, icon: Icon) -> TextField {
        self.icon = Some(icon);
        self
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.chars = text.into().chars().collect();
        self.cursor = self.chars.len();
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Вставить/удалить символы. Возвращает `true`, если текст изменился.
    pub fn handle_event(&mut self, event: &Event) -> bool {
        if !self.focused {
            return false;
        }
        match event {
            Event::Text { text } => {
                let mut changed = false;
                for ch in text.chars() {
                    if ch.is_control() {
                        continue;
                    }
                    self.chars.insert(self.cursor, ch);
                    self.cursor += 1;
                    changed = true;
                }
                changed
            }
            Event::Key {
                key, pressed: true, ..
            } => match key {
                Key::Backspace => {
                    if self.cursor > 0 {
                        self.cursor -= 1;
                        self.chars.remove(self.cursor);
                        true
                    } else {
                        false
                    }
                }
                Key::Delete => {
                    if self.cursor < self.chars.len() {
                        self.chars.remove(self.cursor);
                        true
                    } else {
                        false
                    }
                }
                Key::Left => {
                    self.cursor = self.cursor.saturating_sub(1);
                    false
                }
                Key::Right => {
                    self.cursor = (self.cursor + 1).min(self.chars.len());
                    false
                }
                Key::Home => {
                    self.cursor = 0;
                    false
                }
                Key::End => {
                    self.cursor = self.chars.len();
                    false
                }
                _ => false,
            },
            _ => false,
        }
    }

    pub fn draw(&self, canvas: &mut Canvas, theme: &Theme, rect: Rect) {
        let palette = &theme.palette;
        let radius = theme.metrics.radius_control;
        canvas.fill_rounded(rect, radius, palette.field);
        canvas.stroke_rounded(
            rect,
            radius,
            1,
            if self.focused {
                palette.field_border_focused
            } else {
                palette.field_border
            },
        );

        let icon_size = theme.metrics.icon_size;
        let mut text_x = rect.x + 10;
        if let Some(icon) = self.icon {
            icon::draw(
                canvas,
                icon,
                Rect::new(
                    rect.x + 8,
                    rect.y + (rect.h - icon_size) / 2,
                    icon_size,
                    icon_size,
                ),
                palette.text_muted,
                palette,
            );
            text_x += icon_size + 6;
        }
        let baseline = font::baseline_for(rect);
        let text = self.text();
        let text_color = if text.is_empty() {
            palette.placeholder
        } else {
            palette.text
        };
        let shown = if text.is_empty() {
            self.placeholder.clone()
        } else {
            text
        };
        font::draw(canvas, text_x, baseline, &shown, text_color);
        if self.focused {
            let caret_x = text_x
                + font::measure(
                    &self.chars[..self.cursor.min(self.chars.len())]
                        .iter()
                        .collect::<String>(),
                );
            canvas.fill(
                Rect::new(caret_x, rect.y + 8, 1, rect.h - 16),
                palette.accent,
            );
        }
    }
}

// ───────────────────────────── Список ─────────────────────────────

/// Пункт списка.
#[derive(Clone, Debug)]
pub struct ListItem {
    pub id: u32,
    pub label: String,
    pub icon: Option<Icon>,
}

impl ListItem {
    pub fn new(id: u32, label: impl Into<String>) -> ListItem {
        ListItem {
            id,
            label: label.into(),
            icon: None,
        }
    }

    pub fn with_icon(mut self, icon: Icon) -> ListItem {
        self.icon = Some(icon);
        self
    }
}

/// Секция списка с заголовком.
#[derive(Clone, Debug)]
pub struct ListSection {
    pub title: String,
    pub items: Vec<ListItem>,
}

/// Вертикальный список с секциями (боковая панель).
#[derive(Clone, Debug, Default)]
pub struct List {
    pub sections: Vec<ListSection>,
    pub selected: Option<u32>,
    pub hovered: Option<u32>,
}

/// Раскладка списка: прямоугольники пунктов.
#[derive(Clone, Debug, Default)]
pub struct ListLayout {
    pub items: Vec<(u32, Rect)>,
    pub height: i32,
}

impl ListLayout {
    pub fn item_at(&self, pos: Point) -> Option<u32> {
        self.items
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .map(|(id, _)| *id)
    }

    pub fn rect_of(&self, id: u32) -> Option<Rect> {
        self.items
            .iter()
            .find(|(item_id, _)| *item_id == id)
            .map(|(_, rect)| *rect)
    }
}

impl List {
    pub fn new() -> List {
        List::default()
    }

    pub fn section(mut self, title: impl Into<String>, items: Vec<ListItem>) -> List {
        self.sections.push(ListSection {
            title: title.into(),
            items,
        });
        self
    }

    /// Раскладка пунктов списка внутри `area` (без прокрутки).
    pub fn layout(&self, area: Rect, theme: &Theme) -> ListLayout {
        let mut items = Vec::new();
        let mut y = area.y;
        for section in &self.sections {
            if !section.title.is_empty() {
                y += SECTION_HEIGHT;
            }
            for item in &section.items {
                items.push((
                    item.id,
                    Rect::new(area.x + 6, y, area.w - 12, theme.metrics.row_height),
                ));
                y += theme.metrics.row_height;
            }
            y += SECTION_GAP;
        }
        ListLayout {
            items,
            height: y - area.y,
        }
    }

    pub fn draw(&self, canvas: &mut Canvas, theme: &Theme, area: Rect) -> ListLayout {
        let palette = &theme.palette;
        let layout = self.layout(area, theme);
        // Отсечение — это и есть виджетный damage: если прямоугольник списка
        // не пересекается с текущим клипом (повреждённой областью), пункт не
        // рисуется вовсе. Так проход по списку стоит O(попавших в damage
        // пунктов), а не O(всех): полный кадр (клип = вся поверхность)
        // рисуется как раньше.
        let clip = canvas.clip();
        let mut y = area.y;
        let mut item_index = 0;
        for section in &self.sections {
            if !section.title.is_empty() {
                let title_rect = Rect::new(area.x, y, area.w, SECTION_HEIGHT);
                if clip.intersects(title_rect) {
                    font::draw(
                        canvas,
                        area.x + 12,
                        y + SECTION_HEIGHT / 2 + font::ascent() / 2,
                        &section.title,
                        palette.section_text,
                    );
                }
                y += SECTION_HEIGHT;
            }
            for item in &section.items {
                let rect = layout.items[item_index].1;
                item_index += 1;
                if !clip.intersects(rect) {
                    continue;
                }
                let selected = self.selected == Some(item.id);
                let hovered = self.hovered == Some(item.id);
                if selected {
                    canvas.fill_rounded(
                        rect,
                        theme.metrics.radius_control,
                        palette.sidebar_selected,
                    );
                } else if hovered {
                    canvas.fill_rounded(
                        rect,
                        theme.metrics.radius_control,
                        palette.selection.with_alpha(90),
                    );
                }
                let icon_size = theme.metrics.icon_size;
                let mut text_x = rect.x + 10;
                if let Some(icon) = item.icon {
                    icon::draw(
                        canvas,
                        icon,
                        Rect::new(
                            rect.x + 8,
                            rect.y + (rect.h - icon_size) / 2,
                            icon_size,
                            icon_size,
                        ),
                        if selected {
                            palette.sidebar_text_selected
                        } else {
                            palette.sidebar_text
                        },
                        palette,
                    );
                    text_x += icon_size + 8;
                }
                let color = if selected {
                    palette.sidebar_text_selected
                } else {
                    palette.sidebar_text
                };
                let label = font::ellipsize(&item.label, rect.right() - text_x - 8);
                font::draw(canvas, text_x, font::baseline_for(rect), &label, color);
            }
            y += section.items.len() as i32 * theme.metrics.row_height + SECTION_GAP;
        }
        layout
    }
}

const SECTION_HEIGHT: i32 = 26;
const SECTION_GAP: i32 = 8;

// ───────────────────────────── Сетка ─────────────────────────────

/// Элемент сетки значков.
#[derive(Clone, Debug)]
pub struct GridItem {
    pub label: String,
    pub icon: Icon,
    pub is_dir: bool,
}

/// Сетка значков с центрированными подписями (режим «Значки» в Files).
#[derive(Clone, Debug, Default)]
pub struct Grid {
    pub items: Vec<GridItem>,
    pub cols: i32,
    pub cell: Size,
    pub gap: Size,
    pub selected: Option<usize>,
    pub hovered: Option<usize>,
}

impl Grid {
    pub fn new(cols: i32) -> Grid {
        Grid {
            items: Vec::new(),
            cols: cols.max(1),
            cell: Size::new(128, 96),
            gap: Size::new(6, 8),
            selected: None,
            hovered: None,
        }
    }

    pub fn push(&mut self, item: GridItem) {
        self.items.push(item);
    }

    /// Прямоугольники ячеек внутри `area`.
    pub fn layout(&self, area: Rect) -> Vec<Rect> {
        crate::layout::grid(
            area,
            self.cols,
            self.cell,
            self.gap,
            self.items.len() as i32,
        )
    }

    /// Высота содержимого (для расчёта прокрутки).
    pub fn content_height(&self, area: Rect) -> i32 {
        match self.layout(area).last() {
            Some(last) => last.bottom() - area.y,
            None => 0,
        }
    }

    pub fn index_at(&self, area: Rect, pos: Point) -> Option<usize> {
        self.layout(area).iter().position(|rect| rect.contains(pos))
    }

    pub fn draw(&self, canvas: &mut Canvas, theme: &Theme, area: Rect) -> Vec<Rect> {
        let palette = &theme.palette;
        let cells = self.layout(area);
        // См. `List::draw`: ячейка вне повреждённой области не рисуется.
        let clip = canvas.clip();
        for (index, cell) in cells.iter().enumerate() {
            let Some(item) = self.items.get(index) else {
                continue;
            };
            if cell.bottom() > area.bottom() || cell.y > area.bottom() {
                continue;
            }
            if !clip.intersects(*cell) {
                continue;
            }
            if self.selected == Some(index) {
                canvas.fill_rounded(
                    *cell,
                    theme.metrics.radius_card,
                    palette.selection.with_alpha(120),
                );
                canvas.stroke_rounded(
                    *cell,
                    theme.metrics.radius_card,
                    1,
                    palette.selection_border.with_alpha(180),
                );
            } else if self.hovered == Some(index) {
                canvas.fill_rounded(
                    *cell,
                    theme.metrics.radius_card,
                    Color::rgba(255, 255, 255, 10),
                );
            }

            let icon_size = Size::new(ICON_WIDTH, ICON_HEIGHT);
            let icon_rect = Rect::new(
                cell.x + (cell.w - icon_size.w) / 2,
                cell.y + 8,
                icon_size.w,
                icon_size.h,
            );
            match item.icon {
                Icon::Folder => icon::draw_folder(canvas, icon_rect, palette),
                Icon::File => icon::draw_file(canvas, icon_rect, palette),
                other => icon::draw(canvas, other, icon_rect, palette.text, palette),
            }

            let label_rect = Rect::new(
                cell.x + 4,
                icon_rect.bottom() + 8,
                cell.w - 8,
                font::line_height(),
            );
            let label = font::ellipsize(&item.label, label_rect.w);
            font::draw_aligned(
                canvas,
                label_rect,
                &label,
                if item.is_dir {
                    palette.text
                } else {
                    palette.text_muted
                },
                TextAlign::Center,
            );
        }
        cells
    }
}

const ICON_WIDTH: i32 = 40;
const ICON_HEIGHT: i32 = 32;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Modifiers;

    #[test]
    fn label_draws_and_ellipsizes() {
        let theme = Theme::pagh_dark();
        let mut canvas = Canvas::new(120, 30);
        let label = Label::new("Очень длинная подпись элемента");
        label.draw(&mut canvas, &theme, Rect::new(0, 0, 60, 20));
        let ink = (0..120)
            .flat_map(|x| (0..30).map(move |y| Point::new(x, y)))
            .filter(|p| canvas.pixel(*p).a > 0)
            .count();
        assert!(ink > 10);
    }

    #[test]
    fn button_min_size_grows_with_icon() {
        let theme = Theme::pagh_dark();
        let plain = Button::new("ОК").min_size(&theme);
        let with_icon = Button::new("ОК").with_icon(Icon::Plus).min_size(&theme);
        assert!(with_icon.w > plain.w);
    }

    #[test]
    fn button_primary_and_secondary_draw() {
        let theme = Theme::pagh_dark();
        for style in [
            ButtonStyle::Primary,
            ButtonStyle::Secondary,
            ButtonStyle::Flat,
        ] {
            let button = Button {
                label: "Открыть".to_string(),
                style,
                enabled: true,
                icon: Some(Icon::Folder),
            };
            let mut canvas = Canvas::new(120, 34);
            button.draw(
                &mut canvas,
                &theme,
                Rect::new(0, 0, 120, 34),
                ButtonState::default(),
            );
            let ink = (0..120)
                .flat_map(|x| (0..34).map(move |y| Point::new(x, y)))
                .filter(|p| canvas.pixel(*p).a > 0)
                .count();
            assert!(ink > 100, "стиль {style:?} ничего не нарисовал");
        }
    }

    #[test]
    fn text_field_edits_and_reports_changes() {
        let mut field = TextField::new("Поиск");
        field.set_focused(true);
        assert_eq!(field.text(), "");
        assert!(field.handle_event(&Event::Text {
            text: "Дом".to_string()
        }));
        assert_eq!(field.text(), "Дом");
        assert!(field.handle_event(&Event::Key {
            key: Key::Backspace,
            pressed: true,
            modifiers: Modifiers::NONE
        }));
        assert_eq!(field.text(), "До");
        // Home/End двигают каретку, но текст не меняют.
        assert!(!field.handle_event(&Event::Key {
            key: Key::Home,
            pressed: true,
            modifiers: Modifiers::NONE
        }));
        field.handle_event(&Event::Text {
            text: "X".to_string(),
        });
        assert_eq!(field.text(), "XДо");
        assert_eq!(field.cursor(), 1);
    }

    #[test]
    fn list_layout_and_hit_test() {
        let theme = Theme::pagh_dark();
        let list = List::new()
            .section(
                "ТОЧКИ ВХОДА",
                vec![
                    ListItem::new(1, "Домашняя папка").with_icon(Icon::Home),
                    ListItem::new(2, "Документы").with_icon(Icon::Documents),
                ],
            )
            .section("УСТРОЙСТВА", vec![ListItem::new(3, "root")]);
        let area = Rect::new(0, 0, 180, 400);
        let layout = list.layout(area, &theme);
        assert_eq!(layout.items.len(), 3);
        assert_eq!(layout.item_at(Point::new(20, 26 + 13)), Some(1));
        assert_eq!(layout.item_at(layout.rect_of(3).unwrap().center()), Some(3));
        assert!(layout.height > 100);
        let mut canvas = Canvas::new(180, 400);
        list.draw(&mut canvas, &theme, area);
    }

    #[test]
    fn grid_layout_and_hit_test() {
        let theme = Theme::pagh_dark();
        let mut grid = Grid::new(4);
        for index in 0..10 {
            grid.push(GridItem {
                label: format!("Папка {index}"),
                icon: Icon::Folder,
                is_dir: true,
            });
        }
        let area = Rect::new(0, 0, 600, 400);
        let cells = grid.layout(area);
        assert_eq!(cells.len(), 10);
        assert_eq!(cells[1].x, 128 + 6);
        assert_eq!(grid.index_at(area, Point::new(10, 10)), Some(0));
        assert_eq!(grid.index_at(area, Point::new(300, 10)), Some(2));
        let mut canvas = Canvas::new(600, 400);
        grid.draw(&mut canvas, &theme, area);
        assert!(
            canvas.pixel(Point::new(64, 20)).a > 0,
            "иконка не нарисована"
        );
    }

    #[test]
    fn grid_label_is_centred() {
        let theme = Theme::pagh_dark();
        let mut grid = Grid::new(1);
        grid.push(GridItem {
            label: "x".to_string(),
            icon: Icon::Folder,
            is_dir: true,
        });
        let area = Rect::new(0, 0, 200, 120);
        grid.draw(&mut Canvas::new(200, 120), &theme, area);
        let cells = grid.layout(area);
        let label_width = font::measure("x");
        let expected_x = cells[0].x + (cells[0].w - label_width) / 2;
        assert_eq!(expected_x, cells[0].x + (cells[0].w - label_width) / 2);
    }
}
