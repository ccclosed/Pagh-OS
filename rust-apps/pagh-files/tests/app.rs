//! Интеграционные тесты Files: навигация, меню, создание/удаление, диалоги.
//!
//! События подаются так же, как их пришлёт композитор (или демо-сцена):
//! [`Event::MouseDown`]/[`Event::DoubleClick`]/[`Event::Key`], а результаты
//! проверяются через состояние приложения и команды наружу.

use pagh_files::screenshots::{demo_app, WINDOW};
use pagh_files::{Command, FilesApp, SortMode, ViewMode};
use pagh_ui::event::{Event, Key, Modifiers, MouseButton};
use pagh_ui::geom::{Point, Size};
use pagh_ui::menu::Menu;

fn entry_index(app: &FilesApp, name: &str) -> usize {
    app.entries()
        .iter()
        .position(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("нет элемента {name:?}"))
}

fn click(app: &mut FilesApp, pos: Point, button: MouseButton) -> Vec<Command> {
    app.handle_event(&Event::MouseDown { pos, button })
}

fn double_click(app: &mut FilesApp, pos: Point) -> Vec<Command> {
    app.handle_event(&Event::DoubleClick { pos })
}

fn key(app: &mut FilesApp, key: Key, modifiers: Modifiers) -> Vec<Command> {
    app.handle_event(&Event::Key {
        key,
        pressed: true,
        modifiers,
    })
}

fn ctrl_c(app: &mut FilesApp) -> Vec<Command> {
    key(
        app,
        Key::Char('c'),
        Modifiers {
            shift: false,
            ctrl: true,
            alt: false,
        },
    )
}

fn ctrl_v(app: &mut FilesApp) -> Vec<Command> {
    key(
        app,
        Key::Char('v'),
        Modifiers {
            shift: false,
            ctrl: true,
            alt: false,
        },
    )
}

fn alt_enter(app: &mut FilesApp) -> Vec<Command> {
    key(
        app,
        Key::Enter,
        Modifiers {
            shift: false,
            ctrl: false,
            alt: true,
        },
    )
}

fn alt_shift_f4(app: &mut FilesApp) -> Vec<Command> {
    key(
        app,
        Key::F4,
        Modifiers {
            shift: true,
            ctrl: false,
            alt: true,
        },
    )
}

/// Индекс пункта меню по подписи совпадает с индексом строки в раскладке
/// (`MenuLayout::row` принимает индекс пункта, включая разделители).
fn menu_item_index(menu: &Menu, label: &str) -> Option<usize> {
    menu.entries
        .iter()
        .position(|entry| entry.label() == Some(label))
}

fn row_index(menu: &Menu, label: &str) -> Option<usize> {
    menu_item_index(menu, label)
}

/// Открыть контекстное меню правым кликом по первому элементу.
fn open_context_menu(app: &mut FilesApp) {
    let rect = app.item_rect(0).expect("первый элемент");
    click(app, rect.center(), MouseButton::Right);
    assert!(app.menu_open(), "меню должно открыться");
}

/// Кликнуть пункт корневого меню по подписи.
fn click_root_item(app: &mut FilesApp, label: &str) {
    let model = app.menu_model().cloned().expect("меню открыто");
    let index = row_index(&model, label).unwrap_or_else(|| panic!("нет пункта {label:?}"));
    let (root, _) = app.menu_layouts().expect("раскладка меню");
    let pos = root.row(index).expect("строка меню").center();
    app.handle_event(&Event::MouseMove { pos });
    app.handle_event(&Event::MouseDown {
        pos,
        button: MouseButton::Left,
    });
}

/// Открыть подменю «Создать» и кликнуть в нём пункт.
fn click_create_item(app: &mut FilesApp, label: &str) {
    open_context_menu(app);
    click_root_item(app, "Создать");
    let submenu = app.submenu_model().cloned().expect("подменю «Создать»");
    let index = row_index(&submenu, label).unwrap_or_else(|| panic!("нет пункта {label:?}"));
    let (_, layout) = app.menu_layouts().expect("раскладка подменю");
    let pos = layout
        .expect("подменю открыто")
        .row(index)
        .expect("строка подменю")
        .center();
    app.handle_event(&Event::MouseMove { pos });
    app.handle_event(&Event::MouseDown {
        pos,
        button: MouseButton::Left,
    });
}

#[test]
fn starts_in_home_with_concept_status_line() {
    let app = demo_app();
    assert_eq!(app.cwd(), "/home/user");
    assert_eq!(app.title(), "Домашняя папка — Files");
    assert_eq!(app.location_label(), "Домашняя папка");
    assert_eq!(app.status_text(), "24 папки, 4 файла · 76,3 МБ");
    assert_eq!(app.entries().len(), 28);
    let dirs = app.entries().iter().filter(|entry| entry.is_dir()).count();
    assert_eq!(dirs, 24);
    assert_eq!(app.view(), ViewMode::Grid);
    assert_eq!(app.sort(), SortMode::Name);
    assert!(!app.menu_open());
    assert!(!app.dialog_open());
}

#[test]
fn navigate_into_folder_and_back_forward() {
    let mut app = demo_app();
    let documents = entry_index(&app, "Documents");
    let rect = app.item_rect(documents).unwrap();
    double_click(&mut app, rect.center());
    assert_eq!(app.cwd(), "/home/user/Documents");
    assert_eq!(app.title(), "Документы — Files");
    assert!(app.can_go_back());
    assert_eq!(app.status_text(), "1 папка, 3 файла · 1,2 МБ");

    let back = app.layout().back.center();
    click(&mut app, back, MouseButton::Left);
    assert_eq!(app.cwd(), "/home/user");
    assert!(app.can_go_forward());

    let forward = app.layout().forward.center();
    click(&mut app, forward, MouseButton::Left);
    assert_eq!(app.cwd(), "/home/user/Documents");
}

#[test]
fn sidebar_click_navigates_to_place() {
    let mut app = demo_app();
    let layout = app.layout();
    let place = app
        .places()
        .iter()
        .find(|place| place.label == "Изображения")
        .expect("точка входа");
    let rect = layout
        .sidebar_list
        .rect_of(place.id)
        .expect("пункт боковой панели");
    click(&mut app, rect.center(), MouseButton::Left);
    assert_eq!(app.cwd(), "/home/user/Pictures");
    assert_eq!(app.title(), "Изображения — Files");
}

#[test]
fn double_click_file_returns_open_command() {
    let mut app = demo_app();
    let index = entry_index(&app, "notes.md");
    let rect = app.item_rect(index).unwrap();
    let commands = double_click(&mut app, rect.center());
    assert_eq!(
        commands,
        vec![Command::OpenPath {
            path: "/home/user/notes.md".to_string()
        }]
    );
}

/// Двойной клик по папке открывает её (навигация), по файлу — `OpenPath`;
/// раньше `on_double_click` был мёртвым кодом, потому что живой путь ввода не
/// порождал `Event::DoubleClick` (синтез — `pagh-ui::client`, F3.1).
#[test]
fn double_click_directory_navigates_into_it() {
    let mut app = demo_app();
    let index = entry_index(&app, "Documents");
    let rect = app.item_rect(index).unwrap();
    let commands = double_click(&mut app, rect.center());
    assert!(
        commands.is_empty(),
        "навигация не выходит наружу: {commands:?}"
    );
    assert_eq!(app.cwd(), "/home/user/Documents");
    assert_eq!(app.location_label(), "Документы");
    assert!(app.error().is_none(), "{:?}", app.error());
}

#[test]
fn search_filters_entries_and_updates_status() {
    let mut app = demo_app();
    app.show_search("doc");
    assert!(app.search_visible());
    assert_eq!(app.search_query(), "doc");
    assert_eq!(app.entries().len(), 1);
    assert_eq!(app.entries()[0].name, "Documents");
    assert_eq!(app.status_text(), "1 папка, 0 файлов");
    // Escape убирает поиск и возвращает полный список.
    key(&mut app, Key::Escape, Modifiers::NONE);
    assert!(!app.search_visible());
    assert_eq!(app.entries().len(), 28);
}

#[test]
fn context_menu_matches_dolphin_and_has_no_disk_stats() {
    let mut app = demo_app();
    open_context_menu(&mut app);
    let labels: Vec<String> = app
        .menu_model()
        .unwrap()
        .entries
        .iter()
        .filter_map(|entry| entry.label().map(|label| label.to_string()))
        .collect();
    assert!(labels.contains(&"Создать".to_string()));
    assert!(labels.contains(&"Свойства".to_string()));
    assert!(labels
        .iter()
        .any(|label| label.contains("Открыть терминал")));
    assert!(
        !labels.iter().any(|label| label.contains("статистик")),
        "в меню не должно быть статистики использования дисков: {labels:?}"
    );

    // Наведение на «Создать» раскрывает подменю со всеми пунктами.
    click_root_item(&mut app, "Создать");
    let submenu = app.submenu_model().expect("подменю «Создать»");
    let submenu_labels: Vec<&str> = submenu
        .entries
        .iter()
        .filter_map(|entry| entry.label())
        .collect();
    for expected in [
        "Папку…",
        "Текстовый файл…",
        "Пустой файл…",
        "Файл HTML…",
        "Ярлык для адреса в Интернете (URL)…",
        "Ссылку на файл или папку…",
        "Ярлык для приложения…",
    ] {
        assert!(submenu_labels.contains(&expected), "нет пункта {expected}");
    }
}

#[test]
fn create_folder_and_files_from_menu() {
    let mut app = demo_app();
    click_create_item(&mut app, "Папку…");
    assert!(!app.menu_open(), "меню закрылось после действия");
    assert_eq!(app.message(), Some("Создано: Новая папка"));
    assert_eq!(app.status_text(), "25 папок, 4 файла · 76,3 МБ");
    // Второе создание получает уникальное имя.
    click_create_item(&mut app, "Папку…");
    assert_eq!(app.message(), Some("Создано: Новая папка 2"));

    click_create_item(&mut app, "Текстовый файл…");
    assert_eq!(app.message(), Some("Создано: Новый текстовый файл.txt"));
    click_create_item(&mut app, "Пустой файл…");
    assert_eq!(app.message(), Some("Создано: Новый файл"));
    click_create_item(&mut app, "Файл HTML…");
    assert_eq!(app.message(), Some("Создано: Новый файл.html"));
    click_create_item(&mut app, "Ярлык для адреса в Интернете (URL)…");
    assert_eq!(app.message(), Some("Создано: Новая ссылка.url"));

    // HTML-файл содержит шаблон, а не пустоту.
    let html = app
        .entries()
        .iter()
        .find(|entry| entry.name == "Новый файл.html")
        .expect("html создан");
    assert_eq!(html.kind, pagh_files::EntryKind::File);
    assert!(html.size > 20, "шаблон HTML записан");
}

#[test]
fn create_symlink_to_selected_entry() {
    let mut app = demo_app();
    let index = entry_index(&app, "Documents");
    let rect = app.item_rect(index).unwrap();
    click(&mut app, rect.center(), MouseButton::Left);
    click_create_item(&mut app, "Ссылку на файл или папку…");
    assert_eq!(app.message(), Some("Создано: Новая ссылка"));
    let link = app
        .entries()
        .iter()
        .find(|entry| entry.name == "Новая ссылка")
        .expect("ссылка создана");
    assert_eq!(link.kind, pagh_files::EntryKind::Symlink);
}

#[test]
fn disabled_menu_items_do_nothing() {
    let mut app = demo_app();
    open_context_menu(&mut app);
    click_root_item(&mut app, "Упаковать");
    assert!(app.menu_open(), "недоступный пункт не закрывает меню");
    assert_eq!(app.message(), None);
}

#[test]
fn properties_dialog_opens_for_selected_file() {
    let mut app = demo_app();
    let index = entry_index(&app, "notes.md");
    app.select(Some(index));
    let commands = alt_enter(&mut app);
    assert!(commands.is_empty());
    assert!(app.dialog_open());
    key(&mut app, Key::Escape, Modifiers::NONE);
    assert!(!app.dialog_open());
}

#[test]
fn properties_dialog_closes_by_click_outside() {
    let mut app = demo_app();
    app.select(Some(entry_index(&app, "notes.md")));
    alt_enter(&mut app);
    assert!(app.dialog_open());
    click(&mut app, Point::new(2, 2), MouseButton::Left);
    assert!(!app.dialog_open());
}

#[test]
fn delete_selected_file() {
    let mut app = demo_app();
    let index = entry_index(&app, "todo.txt");
    app.select(Some(index));
    key(&mut app, Key::Delete, Modifiers::NONE);
    assert_eq!(app.message(), Some("Удалено: todo.txt"));
    assert!(app.entries().iter().all(|entry| entry.name != "todo.txt"));
    assert_eq!(app.status_text(), "24 папки, 3 файла · 73,4 МБ");
}

#[test]
fn copy_and_paste_selected_file() {
    let mut app = demo_app();
    let index = entry_index(&app, "archive.tar");
    app.select(Some(index));
    ctrl_c(&mut app);
    assert_eq!(app.message(), Some("Скопировано: archive.tar"));
    ctrl_v(&mut app);
    assert_eq!(app.message(), Some("Вставлено: archive (копия).tar"));
    assert!(app
        .entries()
        .iter()
        .any(|entry| entry.name == "archive (копия).tar"));
}

#[test]
fn paste_without_clipboard_reports_message() {
    let mut app = demo_app();
    ctrl_v(&mut app);
    assert_eq!(app.message(), Some("Буфер обмена пуст"));
}

#[test]
fn toolbar_menu_switches_view_and_sort() {
    let mut app = demo_app();
    let kebab = app.layout().kebab.center();
    click(&mut app, kebab, MouseButton::Left);
    assert!(app.menu_open(), "меню «кебаба» открылось");
    click_root_item(&mut app, "Изменить режим просмотра");
    let submenu = app.submenu_model().cloned().expect("подменю режима");
    let index = row_index(&submenu, "Подробный список").expect("пункт списка");
    let (_, layout) = app.menu_layouts().unwrap();
    let pos = layout.unwrap().row(index).unwrap().center();
    app.handle_event(&Event::MouseMove { pos });
    app.handle_event(&Event::MouseDown {
        pos,
        button: MouseButton::Left,
    });
    assert_eq!(app.view(), ViewMode::List);

    // Сортировка по размеру через контекстное меню.
    open_context_menu(&mut app);
    click_root_item(&mut app, "Сортировка");
    let submenu = app.submenu_model().cloned().expect("подменю сортировки");
    let index = row_index(&submenu, "По размеру").expect("пункт размера");
    let (_, layout) = app.menu_layouts().unwrap();
    let pos = layout.unwrap().row(index).unwrap().center();
    app.handle_event(&Event::MouseMove { pos });
    app.handle_event(&Event::MouseDown {
        pos,
        button: MouseButton::Left,
    });
    assert_eq!(app.sort(), SortMode::Size);
    let files: Vec<&str> = app
        .entries()
        .iter()
        .filter(|entry| !entry.is_dir())
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(
        files,
        vec!["photo.png", "archive.tar", "notes.md", "todo.txt"],
        "файлы отсортированы по убыванию размера"
    );
}

#[test]
fn terminal_shortcut_returns_command() {
    let mut app = demo_app();
    let commands = alt_shift_f4(&mut app);
    assert_eq!(
        commands,
        vec![Command::LaunchTerminal {
            path: "/home/user".to_string()
        }]
    );
}

#[test]
fn close_request_from_compositor_quits() {
    let mut app = demo_app();
    // Кнопку x рисует композитор: он присылает close, приложение завершается.
    let commands = app.handle_event(&Event::CloseRequest);
    assert_eq!(commands, vec![Command::Quit]);
}

#[test]
fn keyboard_navigation_moves_selection() {
    let mut app = demo_app();
    key(&mut app, Key::Down, Modifiers::NONE);
    assert_eq!(app.selected_index(), Some(0));
    key(&mut app, Key::Right, Modifiers::NONE);
    assert_eq!(app.selected_index(), Some(1));
    key(&mut app, Key::Down, Modifiers::NONE);
    assert_eq!(app.selected_index(), Some(7));
    key(&mut app, Key::End, Modifiers::NONE);
    assert_eq!(app.selected_index(), Some(27));
    let selected = app.selected().unwrap().name.clone();
    let commands = key(&mut app, Key::Enter, Modifiers::NONE);
    assert_eq!(
        commands,
        vec![Command::OpenPath {
            path: format!("/home/user/{selected}")
        }]
    );
}

#[test]
fn empty_and_missing_folders() {
    let mut app = demo_app();
    let home = app.home();
    app.navigate_to(&pagh_files::vfs::join(&home, "Desktop"));
    assert!(app.entries().is_empty());
    assert_eq!(app.status_text(), "0 папок, 0 файлов");
    assert_eq!(app.error(), None);

    app.navigate_to("/нет-такой-папки");
    assert!(app.error().is_some());
    assert!(app.entries().is_empty());
}

#[test]
fn wheel_scrolls_small_window() {
    let mut app = FilesApp::new(
        Box::new(pagh_files::FakeVfs::concept()),
        Size::new(900, 520),
    );
    let layout = app.layout();
    let pos = layout.grid_area.center();
    app.handle_event(&Event::Wheel { pos, delta: -2 });
    assert!(app.scroll_offset() > 0, "колесо прокручивает сетку");
    app.handle_event(&Event::Wheel { pos, delta: 100 });
    assert_eq!(app.scroll_offset(), 0);
}

#[test]
fn resize_recomputes_layout() {
    let mut app = demo_app();
    assert_eq!(app.size(), WINDOW);
    app.handle_event(&Event::Resize {
        size: Size::new(900, 560),
    });
    assert_eq!(app.size(), Size::new(900, 560));
    let layout = app.layout();
    assert_eq!(layout.toolbar.w, 900);
    assert!(app.item_rect(0).is_some());
}

#[test]
fn path_field_accepts_absolute_path() {
    let mut app = demo_app();
    let path = app.layout().path_field.center();
    click(&mut app, path, MouseButton::Left);
    // Поле получает текущий путь; стираем его и вводим новый.
    let existing = app.cwd().chars().count();
    for _ in 0..existing {
        key(&mut app, Key::Backspace, Modifiers::NONE);
    }
    app.handle_event(&Event::Text {
        text: "/home/user/Pictures".to_string(),
    });
    key(&mut app, Key::Enter, Modifiers::NONE);
    assert_eq!(app.cwd(), "/home/user/Pictures");
}

#[test]
fn menus_never_leave_the_window() {
    let mut app = demo_app();
    // Правый клик в правом нижнем углу контента: меню должно поместиться.
    let layout = app.layout();
    let corner = Point::new(layout.grid_area.right() - 4, layout.grid_area.bottom() - 4);
    click(&mut app, corner, MouseButton::Right);
    let (root, _) = app.menu_layouts().unwrap();
    assert!(root.rect.right() <= app.size().w - 4);
    assert!(root.rect.bottom() <= app.size().h - 4);
    assert!(root.rect.x >= 4);
    assert!(root.rect.y >= 4);
}
