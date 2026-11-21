//! Тема оформления: палитра и метрики рабочего стола Pagh.
//!
//! Палитра снята с концепта `docs/file-manager-concept.png` (тёмная бирюзовая
//! схема): почти чёрный фон `#051518`, панели `#081216`, заголовок окна
//! `#102c30`, выделение `#173e3f`, акцент `#2f9e94`, синяя папка `#3788c8`.

use crate::color::Color;

/// Цвета темы.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Фон рабочего стола (обои).
    pub desktop: Color,
    /// Фон окна.
    pub window: Color,
    /// Рамка окна.
    pub window_border: Color,
    /// Заголовок активного окна.
    pub titlebar: Color,
    /// Заголовок неактивного окна.
    pub titlebar_inactive: Color,
    /// Текст заголовка.
    pub title_text: Color,
    /// Панель инструментов.
    pub toolbar: Color,
    /// Боковая панель.
    pub sidebar: Color,
    /// Разделитель/рамка боковой панели.
    pub sidebar_border: Color,
    /// Заголовок секции боковой панели.
    pub section_text: Color,
    /// Обычный текст боковой панели.
    pub sidebar_text: Color,
    /// Выделенный текст боковой панели.
    pub sidebar_text_selected: Color,
    /// Фон выделенного пункта боковой панели.
    pub sidebar_selected: Color,
    /// Фон поля ввода.
    pub field: Color,
    /// Рамка поля ввода.
    pub field_border: Color,
    /// Рамка поля ввода в фокусе.
    pub field_border_focused: Color,
    /// Плейсхолдер поля ввода.
    pub placeholder: Color,
    /// Основной текст.
    pub text: Color,
    /// Приглушённый текст.
    pub text_muted: Color,
    /// Текст недоступного элемента.
    pub text_disabled: Color,
    /// Акцентный цвет (бирюза).
    pub accent: Color,
    /// Акцент при наведении.
    pub accent_hover: Color,
    /// Текст на акцентном фоне.
    pub accent_text: Color,
    /// Фон выделения.
    pub selection: Color,
    /// Рамка выделения.
    pub selection_border: Color,
    /// Фон строки состояния.
    pub status: Color,
    /// Текст строки состояния.
    pub status_text: Color,
    /// Фон меню и всплывающих панелей.
    pub menu: Color,
    /// Рамка меню.
    pub menu_border: Color,
    /// Подсветка пункта меню.
    pub menu_hover: Color,
    /// Текст меню.
    pub menu_text: Color,
    /// Текст недоступного пункта меню.
    pub menu_text_disabled: Color,
    /// Горячие клавиши в меню.
    pub menu_shortcut: Color,
    /// Разделитель.
    pub separator: Color,
    /// Тень окон и меню.
    pub shadow: Color,
    /// Задняя часть иконки папки.
    pub folder_back: Color,
    /// Передняя часть иконки папки.
    pub folder_front: Color,
    /// Блик на папке.
    pub folder_line: Color,
    /// Лист бумаги в папке.
    pub paper: Color,
    /// Линии на бумаге.
    pub paper_line: Color,
    /// Ползунок прокрутки.
    pub scrollbar: Color,
    /// Ползунок прокрутки при наведении.
    pub scrollbar_hover: Color,
}

impl Default for Palette {
    fn default() -> Palette {
        Theme::pagh_dark().palette
    }
}

/// Метрики раскладки (в пикселях).
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    /// Высота заголовка окна.
    pub title_height: i32,
    /// Высота панели инструментов.
    pub toolbar_height: i32,
    /// Высота строки состояния.
    pub status_height: i32,
    /// Ширина боковой панели. Files подписывает устройства путём монтирования
    /// («Диск данных (/mnt)»), поэтому ширина обязана вмещать такую подпись:
    /// меньше ~204 px — и `List` обрезает путь многоточием.
    pub sidebar_width: i32,
    /// Высота строки списка.
    pub row_height: i32,
    /// Высота пункта меню.
    pub menu_row_height: i32,
    /// Внутренний отступ меню.
    pub menu_padding: i32,
    /// Ширина меню по умолчанию.
    pub menu_min_width: i32,
    /// Размер иконок в списках.
    pub icon_size: i32,
    /// Радиус скругления окна.
    pub radius_window: i32,
    /// Радиус скругления карточек и меню.
    pub radius_card: i32,
    /// Радиус скругления небольших элементов.
    pub radius_control: i32,
    /// Высота нижней панели рабочего стола (для композитора).
    pub panel_height: i32,
    /// Отступ контента от краёв.
    pub content_padding: i32,
}

/// Тема: палитра и метрики.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub palette: Palette,
    pub metrics: Metrics,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::pagh_dark()
    }
}

impl Theme {
    /// Основная тёмная тема Pagh (по концепту Files).
    pub const fn pagh_dark() -> Theme {
        Theme {
            palette: Palette {
                desktop: Color::hex(0x051518),
                window: Color::hex(0x081216),
                window_border: Color::hex(0x123033),
                titlebar: Color::hex(0x102c30),
                titlebar_inactive: Color::hex(0x0b1f23),
                title_text: Color::hex(0xd4ebe8),
                toolbar: Color::hex(0x071215),
                sidebar: Color::hex(0x091619),
                sidebar_border: Color::hex(0x0f2326),
                section_text: Color::hex(0x5a7a79),
                sidebar_text: Color::hex(0x9db5b3),
                sidebar_text_selected: Color::hex(0xe3f2ee),
                sidebar_selected: Color::hex(0x173c3e),
                field: Color::hex(0x0b1b1e),
                field_border: Color::hex(0x1c3d40),
                field_border_focused: Color::hex(0x2f9e94),
                placeholder: Color::hex(0x5f7c7b),
                text: Color::hex(0xcfdedd),
                text_muted: Color::hex(0x8fa9a7),
                text_disabled: Color::hex(0x55706f),
                accent: Color::hex(0x2f9e94),
                accent_hover: Color::hex(0x37b0a5),
                accent_text: Color::hex(0x04201e),
                selection: Color::hex(0x173e3f),
                selection_border: Color::hex(0x2f9e94),
                status: Color::hex(0x0b1a1d),
                status_text: Color::hex(0x7e9b9a),
                menu: Color::hex(0x0c1c1f),
                menu_border: Color::hex(0x17383a),
                menu_hover: Color::hex(0x173e3f),
                menu_text: Color::hex(0xcfdedd),
                menu_text_disabled: Color::hex(0x55706f),
                menu_shortcut: Color::hex(0x6f8a88),
                separator: Color::hex(0x122a2d),
                shadow: Color::rgba(0, 0, 0, 150),
                folder_back: Color::hex(0x2f78b4),
                folder_front: Color::hex(0x3788c8),
                folder_line: Color::hex(0x78b6df),
                paper: Color::hex(0xf2f8fa),
                paper_line: Color::hex(0x9fc4e0),
                scrollbar: Color::hex(0x1f4c4d),
                scrollbar_hover: Color::hex(0x2a6d6c),
            },
            metrics: Metrics {
                title_height: 30,
                toolbar_height: 40,
                status_height: 28,
                sidebar_width: 212,
                row_height: 26,
                menu_row_height: 28,
                menu_padding: 6,
                menu_min_width: 240,
                icon_size: 14,
                radius_window: 8,
                radius_card: 6,
                radius_control: 4,
                panel_height: 44,
                content_padding: 12,
            },
        }
    }

    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_theme_matches_concept_palette() {
        let theme = Theme::pagh_dark();
        assert_eq!(theme.palette.window, Color::hex(0x081216));
        assert_eq!(theme.palette.titlebar, Color::hex(0x102c30));
        assert_eq!(theme.palette.accent, Color::hex(0x2f9e94));
        assert_eq!(theme.palette.folder_front, Color::hex(0x3788c8));
        assert_eq!(theme.metrics.title_height, 30);
        assert_eq!(theme.metrics.sidebar_width, 212);
    }

    #[test]
    fn default_is_pagh_dark() {
        let a = Theme::default();
        let b = Theme::pagh_dark();
        assert_eq!(a.palette.window, b.palette.window);
    }
}
