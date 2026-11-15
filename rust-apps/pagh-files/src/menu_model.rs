//! Модель меню Files: идентификаторы действий и сборка контекстного меню.
//!
//! Меню повторяет Dolphin из концепта `docs/file-manager-concept.png`, но без
//! пункта «Просмотр статистики использования дисков» — в этой версии Files
//! такой функции нет, и мы не показываем нерабочие пункты, кроме явно
//! недоступных (упаковка, метки, команды) с приглушённым видом.

use pagh_ui::icon::Icon;
use pagh_ui::menu::{Menu, MenuEntry};

/// Идентификаторы действий меню.
pub mod action {
    // «Создать» → подменю.
    pub const CREATE_FOLDER: u32 = 101;
    pub const CREATE_TEXT_FILE: u32 = 102;
    pub const CREATE_EMPTY_FILE: u32 = 103;
    pub const CREATE_HTML_FILE: u32 = 104;
    pub const CREATE_URL_LINK: u32 = 105;
    pub const CREATE_SYMLINK: u32 = 106;
    pub const CREATE_APP_LINK: u32 = 107;

    pub const OPEN_IN_APP: u32 = 200;
    pub const PASTE: u32 = 201;

    pub const SORT_NAME: u32 = 210;
    pub const SORT_SIZE: u32 = 211;
    pub const SORT_MODIFIED: u32 = 212;

    pub const VIEW_GRID: u32 = 220;
    pub const VIEW_LIST: u32 = 221;

    pub const OPEN_TERMINAL: u32 = 230;
    pub const COMPRESS: u32 = 240;
    pub const COMPARE: u32 = 250;
    pub const TAG_IMPORTANT: u32 = 260;
    pub const PROPERTIES: u32 = 270;
    pub const REFRESH: u32 = 300;
}

/// Порядок сортировки списка.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SortMode {
    #[default]
    Name,
    Size,
    Modified,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            SortMode::Name => "По имени",
            SortMode::Size => "По размеру",
            SortMode::Modified => "По дате изменения",
        }
    }
}

/// Режим просмотра папки.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ViewMode {
    /// Сетка значков с центрированными подписями.
    #[default]
    Grid,
    /// Подробный список с колонками.
    List,
}

impl ViewMode {
    pub fn label(self) -> &'static str {
        match self {
            ViewMode::Grid => "Значки",
            ViewMode::List => "Подробный список",
        }
    }
}

/// Состояние, влияющее на состав меню.
#[derive(Clone, Copy, Debug)]
pub struct MenuOptions {
    /// В буфере обмена есть элемент (пункт «Вставить» активен).
    pub clipboard: bool,
    pub view: ViewMode,
    pub sort: SortMode,
}

impl Default for MenuOptions {
    fn default() -> Self {
        MenuOptions {
            clipboard: false,
            view: ViewMode::Grid,
            sort: SortMode::Name,
        }
    }
}

/// Подменю «Создать».
pub fn create_submenu() -> Menu {
    Menu::from_entries(vec![
        MenuEntry::action(action::CREATE_FOLDER, "Папку…"),
        MenuEntry::action(action::CREATE_TEXT_FILE, "Текстовый файл…"),
        MenuEntry::action(action::CREATE_EMPTY_FILE, "Пустой файл…"),
        MenuEntry::action(action::CREATE_HTML_FILE, "Файл HTML…"),
        MenuEntry::Separator,
        MenuEntry::action(
            action::CREATE_URL_LINK,
            "Ярлык для адреса в Интернете (URL)…",
        ),
        MenuEntry::action(action::CREATE_SYMLINK, "Ссылку на файл или папку…"),
        MenuEntry::action(action::CREATE_APP_LINK, "Ярлык для приложения…"),
    ])
}

/// Подменю «Сортировка» с галочкой у текущего режима.
pub fn sort_submenu(current: SortMode) -> Menu {
    let check = |mode: SortMode, id: u32| -> MenuEntry {
        let entry = MenuEntry::action(id, mode.label());
        if mode == current {
            match entry {
                MenuEntry::Action {
                    id,
                    label,
                    shortcut,
                    enabled,
                    ..
                } => MenuEntry::Action {
                    id,
                    label,
                    icon: Some(Icon::Check),
                    shortcut,
                    enabled,
                },
                other => other,
            }
        } else {
            entry
        }
    };
    Menu::from_entries(vec![
        check(SortMode::Name, action::SORT_NAME),
        check(SortMode::Size, action::SORT_SIZE),
        check(SortMode::Modified, action::SORT_MODIFIED),
    ])
}

/// Подменю «Изменить режим просмотра».
pub fn view_submenu(current: ViewMode) -> Menu {
    let check = |mode: ViewMode, id: u32| -> MenuEntry {
        let entry = MenuEntry::action(id, mode.label());
        if mode == current {
            match entry {
                MenuEntry::Action {
                    id,
                    label,
                    shortcut,
                    enabled,
                    ..
                } => MenuEntry::Action {
                    id,
                    label,
                    icon: Some(Icon::Check),
                    shortcut,
                    enabled,
                },
                other => other,
            }
        } else {
            entry
        }
    };
    Menu::from_entries(vec![
        check(ViewMode::Grid, action::VIEW_GRID),
        check(ViewMode::List, action::VIEW_LIST),
    ])
}

/// Контекстное меню папки (правый клик в содержимом).
pub fn context_menu(options: MenuOptions) -> Menu {
    Menu::from_entries(vec![
        MenuEntry::submenu_icon("Создать", Icon::Plus, create_submenu()),
        MenuEntry::action(action::OPEN_IN_APP, "Открыть папку в приложении"),
        paste_entry(options.clipboard),
        MenuEntry::submenu("Сортировка", sort_submenu(options.sort)),
        MenuEntry::submenu("Изменить режим просмотра", view_submenu(options.view)),
        MenuEntry::action_shortcut(
            action::OPEN_TERMINAL,
            "Открыть терминал в этой папке",
            "Alt+Shift+F4",
        ),
        MenuEntry::action(action::COMPRESS, "Упаковать").disabled(),
        MenuEntry::submenu(
            "Команды",
            Menu::from_entries(vec![MenuEntry::action(action::COMPARE, "Сравнить файлы…")]),
        )
        .disabled(),
        MenuEntry::submenu(
            "Присвоить метки",
            Menu::from_entries(vec![MenuEntry::action(action::TAG_IMPORTANT, "Важное")]),
        )
        .disabled(),
        MenuEntry::action_shortcut(action::PROPERTIES, "Свойства", "Alt+Return"),
    ])
}

/// Меню «кебаба» на панели инструментов: те же действия без контекста клика.
pub fn toolbar_menu(options: MenuOptions) -> Menu {
    Menu::from_entries(vec![
        MenuEntry::submenu_icon("Создать", Icon::Plus, create_submenu()),
        paste_entry(options.clipboard),
        MenuEntry::Separator,
        MenuEntry::submenu("Сортировка", sort_submenu(options.sort)),
        MenuEntry::submenu("Изменить режим просмотра", view_submenu(options.view)),
        MenuEntry::action_shortcut(
            action::OPEN_TERMINAL,
            "Открыть терминал в этой папке",
            "Alt+Shift+F4",
        ),
        MenuEntry::action_shortcut(action::REFRESH, "Обновить", "F5"),
        MenuEntry::action_shortcut(action::PROPERTIES, "Свойства", "Alt+Return"),
    ])
}

fn paste_entry(clipboard: bool) -> MenuEntry {
    let entry = MenuEntry::action_shortcut(
        action::PASTE,
        "Вставить содержимое буфера обмена…",
        "Ctrl+V",
    );
    if clipboard {
        entry
    } else {
        entry.disabled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(menu: &Menu) -> Vec<String> {
        menu.entries
            .iter()
            .filter_map(|entry| entry.label().map(|label| label.to_string()))
            .collect()
    }

    fn find_entry(menu: &Menu, id: u32) -> Option<&MenuEntry> {
        menu.find(id)
    }

    #[test]
    fn create_submenu_has_all_dolphin_items() {
        let submenu = create_submenu();
        let labels = labels(&submenu);
        assert!(labels.contains(&"Папку…".to_string()));
        assert!(labels.contains(&"Текстовый файл…".to_string()));
        assert!(labels.contains(&"Пустой файл…".to_string()));
        assert!(labels.contains(&"Файл HTML…".to_string()));
        assert!(labels
            .iter()
            .any(|label| label.starts_with("Ярлык для адреса в Интернете")));
        assert!(labels
            .iter()
            .any(|label| label.starts_with("Ссылку на файл")));
        assert!(labels
            .iter()
            .any(|label| label.starts_with("Ярлык для приложения")));
        // Разделитель отделяет ярлыки от файлов.
        assert!(submenu
            .entries
            .iter()
            .any(|entry| matches!(entry, MenuEntry::Separator)));
        assert_eq!(submenu.entries.len(), 8);
    }

    #[test]
    fn context_menu_has_required_actions_and_no_disk_stats() {
        let menu = context_menu(MenuOptions::default());
        assert!(find_entry(&menu, action::PROPERTIES).is_some());
        assert!(find_entry(&menu, action::OPEN_TERMINAL).is_some());
        assert!(menu.entries.iter().any(|entry| entry
            .submenu_of()
            .map(|sub| sub.find(action::CREATE_FOLDER).is_some())
            .unwrap_or(false)));

        let all_labels: Vec<String> = menu
            .entries
            .iter()
            .map(|entry| entry.label().unwrap_or("").to_lowercase())
            .collect();
        assert!(
            !all_labels.iter().any(|label| label.contains("статистик")),
            "меню не должно содержать «Просмотр статистики использования дисков»: {all_labels:?}"
        );
        // Пункт «Свойства» — последний, как в концепте.
        assert_eq!(menu.entries.len(), 10);
        assert_eq!(
            menu.entries.last().and_then(|entry| entry.id()),
            Some(action::PROPERTIES)
        );
    }

    #[test]
    fn paste_disabled_without_clipboard() {
        let menu = context_menu(MenuOptions::default());
        let paste = find_entry(&menu, action::PASTE).expect("вставить");
        assert!(!paste.is_enabled());
        let menu = context_menu(MenuOptions {
            clipboard: true,
            ..MenuOptions::default()
        });
        assert!(find_entry(&menu, action::PASTE).unwrap().is_enabled());
    }

    #[test]
    fn unsupported_actions_are_disabled() {
        let menu = context_menu(MenuOptions::default());
        assert!(!find_entry(&menu, action::COMPRESS).unwrap().is_enabled());
        let commands = menu
            .entries
            .iter()
            .find(|entry| entry.label() == Some("Команды"))
            .expect("команды");
        assert!(!commands.is_enabled());
        let tags = menu
            .entries
            .iter()
            .find(|entry| entry.label() == Some("Присвоить метки"))
            .expect("метки");
        assert!(!tags.is_enabled());
    }

    #[test]
    fn shortcuts_match_dolphin() {
        let menu = context_menu(MenuOptions {
            clipboard: true,
            ..MenuOptions::default()
        });
        let shortcut = |id: u32| -> Option<String> {
            match find_entry(&menu, id) {
                Some(MenuEntry::Action { shortcut, .. }) => shortcut.clone(),
                _ => None,
            }
        };
        assert_eq!(shortcut(action::PASTE).as_deref(), Some("Ctrl+V"));
        assert_eq!(
            shortcut(action::OPEN_TERMINAL).as_deref(),
            Some("Alt+Shift+F4")
        );
        assert_eq!(shortcut(action::PROPERTIES).as_deref(), Some("Alt+Return"));
    }

    #[test]
    fn sort_and_view_submenus_mark_current_mode() {
        let sort = sort_submenu(SortMode::Size);
        let checked = sort
            .entries
            .iter()
            .find(|entry| {
                matches!(
                    entry,
                    MenuEntry::Action {
                        icon: Some(Icon::Check),
                        ..
                    }
                )
            })
            .expect("галочка");
        assert_eq!(checked.label(), Some(SortMode::Size.label()));

        let view = view_submenu(ViewMode::List);
        let checked = view
            .entries
            .iter()
            .find(|entry| {
                matches!(
                    entry,
                    MenuEntry::Action {
                        icon: Some(Icon::Check),
                        ..
                    }
                )
            })
            .expect("галочка");
        assert_eq!(checked.label(), Some(ViewMode::List.label()));
    }

    #[test]
    fn toolbar_menu_has_refresh_and_create() {
        let menu = toolbar_menu(MenuOptions::default());
        assert!(find_entry(&menu, action::REFRESH).is_some());
        assert!(menu
            .entries
            .iter()
            .any(|entry| entry.label() == Some("Создать")));
    }
}
