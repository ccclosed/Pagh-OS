//! Демонстрационные сцены тулкита для headless-скриншотов и тестов.
//!
//! Сцены собираются только из публичного API `pagh-ui`, поэтому они же служат
//! smoke-тестом композиции виджетов и эталоном для screenshot-тестов.

use crate::canvas::Canvas;
use crate::chrome::{self, WindowStyle};
use crate::font::{self, TextAlign};
use crate::geom::{Point, Rect, Size};
use crate::icon::{self, Icon};
use crate::layout;
use crate::menu::{self, Menu, MenuEntry};
use crate::theme::Theme;
use crate::widgets::{
    Button, ButtonState, Grid, GridItem, Label, LabelStyle, List, ListItem, TextField,
};

pub fn gallery(theme: &Theme) -> Canvas {
    let size = Size::new(980, 600);
    let mut canvas = Canvas::filled(size.w, size.h, theme.palette.desktop);
    let layout = chrome::draw_window(
        &mut canvas,
        theme,
        Rect::new(20, 20, size.w - 40, size.h - 40),
        WindowStyle::new("Pagh UI — виджеты"),
    );
    let content = layout.content.inset_by(crate::Insets::symmetric(18, 16));
    let (left, right) = content.split_left(520);

    // ── Левая колонка: подписи, кнопки, поля ввода, иконки ──
    let mut y = left.y;
    Label::new("Текст").draw(&mut canvas, theme, Rect::new(left.x, y, left.w, 20));
    y += 26;
    for (style, text) in [
        (LabelStyle::Normal, "Обычный текст интерфейса"),
        (LabelStyle::Muted, "Приглушённый текст"),
        (LabelStyle::Disabled, "Недоступный текст"),
        (LabelStyle::Accent, "Акцентный текст"),
        (LabelStyle::Section, "ЗАГОЛОВОК СЕКЦИИ"),
    ] {
        Label::styled(text, style).draw(&mut canvas, theme, Rect::new(left.x, y, left.w, 20));
        y += 21;
    }

    y += 12;
    Label::new("Кнопки").draw(&mut canvas, theme, Rect::new(left.x, y, left.w, 20));
    y += 26;
    let buttons = [
        Button::primary("Открыть").with_icon(Icon::Folder),
        Button::new("Отмена"),
        Button::flat("Ещё").with_icon(Icon::MenuDots),
    ];
    let sizes: Vec<i32> = buttons
        .iter()
        .map(|button| button.min_size(theme).w + 12)
        .collect();
    let button_rects = layout::fixed(
        Rect::new(left.x, y, left.w, 34),
        layout::Axis::Horizontal,
        10,
        &sizes,
    );
    for (index, button) in buttons.iter().enumerate() {
        let state = ButtonState {
            hovered: index == 1,
            pressed: false,
        };
        button.draw(&mut canvas, theme, button_rects[index], state);
    }
    let disabled = Button::primary("Недоступно").enabled(false);
    disabled.draw(
        &mut canvas,
        theme,
        Rect::new(left.x, y + 44, disabled.min_size(theme).w + 12, 34),
        ButtonState::default(),
    );

    y += 96;
    Label::new("Поля ввода").draw(&mut canvas, theme, Rect::new(left.x, y, left.w, 20));
    y += 26;
    let path = TextField::new("Путь")
        .with_text("/home/user/Документы")
        .with_icon(Icon::Folder);
    path.draw(&mut canvas, theme, Rect::new(left.x, y, left.w, 32));
    y += 40;
    let mut search = TextField::new("Поиск по папке").with_icon(Icon::Search);
    search.set_focused(true);
    search.draw(&mut canvas, theme, Rect::new(left.x, y, left.w, 32));

    y += 52;
    Label::new("Иконки").draw(&mut canvas, theme, Rect::new(left.x, y, left.w, 20));
    y += 28;
    let icons = [
        Icon::Home,
        Icon::Documents,
        Icon::Downloads,
        Icon::Pictures,
        Icon::Music,
        Icon::Videos,
        Icon::Trash,
        Icon::Network,
        Icon::Device,
        Icon::Terminal,
        Icon::Search,
        Icon::MenuDots,
        Icon::Plus,
        Icon::Link,
    ];
    for (index, icon) in icons.iter().enumerate() {
        let col = (index % 10) as i32;
        let row = (index / 10) as i32;
        icon::draw(
            &mut canvas,
            *icon,
            Rect::new(left.x + col * 48, y + row * 36, 20, 20),
            theme.palette.text_muted,
            &theme.palette,
        );
    }

    // ── Правая колонка: список и сетка ──
    let mut list = List::new()
        .section(
            "ТОЧКИ ВХОДА",
            vec![
                ListItem::new(1, "Домашняя папка").with_icon(Icon::Home),
                ListItem::new(2, "Документы").with_icon(Icon::Documents),
                ListItem::new(3, "Загрузки").with_icon(Icon::Downloads),
                ListItem::new(4, "Изображения").with_icon(Icon::Pictures),
                ListItem::new(5, "Корзина").with_icon(Icon::Trash),
            ],
        )
        .section(
            "УСТРОЙСТВА",
            vec![ListItem::new(6, "boot"), ListItem::new(7, "root")],
        );
    list.selected = Some(1);
    list.hovered = Some(3);
    list.draw(
        &mut canvas,
        theme,
        Rect::new(right.x + 16, right.y, 250, right.h),
    );

    Label::styled("Сетка: центрированные подписи", LabelStyle::Muted).draw(
        &mut canvas,
        theme,
        Rect::new(right.x + 16, right.y + 270, 250, 18),
    );
    let mut grid = Grid::new(2);
    for (label, icon, is_dir) in [
        ("Документы", Icon::Folder, true),
        ("Изображения", Icon::Folder, true),
        ("Заметки.txt", Icon::File, false),
        ("Терминал", Icon::Terminal, false),
    ] {
        grid.push(GridItem {
            label: label.to_string(),
            icon,
            is_dir,
        });
    }
    grid.cell = Size::new(140, 92);
    grid.gap = Size::new(8, 6);
    grid.selected = Some(1);
    grid.hovered = Some(0);
    grid.draw(
        &mut canvas,
        theme,
        Rect::new(right.x + 16, right.y + 294, 300, 190),
    );

    // Строка состояния окна.
    chrome::draw_statusbar(
        &mut canvas,
        theme,
        Rect::new(
            layout.content.x,
            layout.content.bottom() - 28,
            layout.content.w,
            28,
        ),
        "pagh-ui: витрина виджетов",
        "Тема Pagh dark",
    );
    canvas
}

pub fn menu(theme: &Theme) -> Canvas {
    let size = Size::new(1100, 700);
    let mut canvas = Canvas::filled(size.w, size.h, theme.palette.desktop);
    let layout = chrome::draw_window(
        &mut canvas,
        theme,
        Rect::new(60, 40, 980, 620),
        WindowStyle::new("Домашняя папка — Files"),
    );

    // Содержимое под меню: сетка папок.
    let mut grid = Grid::new(6);
    for label in [
        "bin", "boot", "dev", "mnt", "opt", "proc", "srv", "sys", "tmp", "home", "usr", "var",
    ] {
        grid.push(GridItem {
            label: label.to_string(),
            icon: Icon::Folder,
            is_dir: true,
        });
    }
    grid.selected = Some(4);
    grid.draw(
        &mut canvas,
        theme,
        Rect::new(layout.content.x + 14, layout.content.y + 14, 820, 320),
    );

    let create_menu = Menu::from_entries(vec![
        MenuEntry::action(101, "Папку…"),
        MenuEntry::action(102, "Текстовый файл…"),
        MenuEntry::action(103, "Пустой файл…"),
        MenuEntry::action(104, "Файл HTML…"),
        MenuEntry::Separator,
        MenuEntry::action(105, "Ярлык для адреса в Интернете (URL)…"),
        MenuEntry::action(106, "Ссылку на файл или папку…"),
        MenuEntry::action(107, "Ярлык для приложения…"),
    ]);
    let menu_model = Menu::from_entries(vec![
        MenuEntry::submenu_icon("Создать", Icon::Plus, create_menu),
        MenuEntry::action(2, "Открыть папку в приложении"),
        MenuEntry::action_shortcut(3, "Вставить содержимое буфера обмена…", "Ctrl+V"),
        MenuEntry::submenu(
            "Сортировка",
            Menu::from_entries(vec![
                MenuEntry::action(4, "По имени"),
                MenuEntry::action(41, "По размеру"),
            ]),
        ),
        MenuEntry::submenu(
            "Изменить режим просмотра",
            Menu::from_entries(vec![
                MenuEntry::action(5, "Значки"),
                MenuEntry::action(51, "Подробный список"),
            ]),
        ),
        MenuEntry::action_shortcut(6, "Открыть терминал в этой папке", "Alt+Shift+F4"),
        MenuEntry::action(7, "Упаковать"),
        MenuEntry::submenu(
            "Команды",
            Menu::from_entries(vec![MenuEntry::action(8, "Сравнить файлы…")]),
        ),
        MenuEntry::submenu(
            "Присвоить метки",
            Menu::from_entries(vec![MenuEntry::action(9, "Важное")]),
        ),
        MenuEntry::action_shortcut(10, "Свойства", "Alt+Return"),
    ]);
    let origin = Point::new(300, 210);
    let menu_layout = menu::draw(&mut canvas, theme, &menu_model, origin, Some(0));
    let submenu_origin = menu_layout.submenu_origin(0).unwrap();
    let submenu = menu_model.entries[0].submenu_of().unwrap();
    menu::draw(&mut canvas, theme, submenu, submenu_origin, Some(1));

    font::draw(
        &mut canvas,
        60,
        680,
        "Контекстное меню и подменю «Создать»",
        theme.palette.text_muted,
    );
    canvas
}

pub fn palette(theme: &Theme) -> Canvas {
    let size = Size::new(900, 420);
    let mut canvas = Canvas::filled(size.w, size.h, theme.palette.desktop);
    let layout = chrome::draw_window(
        &mut canvas,
        theme,
        Rect::new(20, 20, size.w - 40, size.h - 40),
        WindowStyle::new("Pagh UI — палитра темы"),
    );
    let palette = &theme.palette;
    let swatches = [
        ("desktop", palette.desktop),
        ("window", palette.window),
        ("titlebar", palette.titlebar),
        ("toolbar", palette.toolbar),
        ("sidebar", palette.sidebar),
        ("field", palette.field),
        ("selection", palette.selection),
        ("menu", palette.menu),
        ("menu_hover", palette.menu_hover),
        ("status", palette.status),
        ("accent", palette.accent),
        ("accent_hover", palette.accent_hover),
        ("text", palette.text),
        ("text_muted", palette.text_muted),
        ("text_disabled", palette.text_disabled),
        ("separator", palette.separator),
        ("folder_front", palette.folder_front),
        ("folder_back", palette.folder_back),
        ("folder_line", palette.folder_line),
        ("paper", palette.paper),
        ("paper_line", palette.paper_line),
        ("scrollbar", palette.scrollbar),
    ];
    let rects = layout::grid(
        layout.content.inset(18),
        6,
        Size::new(130, 76),
        Size::new(10, 10),
        swatches.len() as i32,
    );
    for (index, (name, color)) in swatches.iter().enumerate() {
        let rect = rects[index];
        canvas.fill_rounded(rect, 6, *color);
        canvas.stroke_rounded(rect, 6, 1, palette.menu_border);
        let text_color = if (color.r as u32 + color.g as u32 + color.b as u32) > 380 {
            palette.accent_text
        } else {
            palette.text
        };
        font::draw_aligned(
            &mut canvas,
            Rect::new(rect.x, rect.bottom() - 24, rect.w, 18),
            name,
            text_color,
            TextAlign::Center,
        );
    }
    canvas
}
