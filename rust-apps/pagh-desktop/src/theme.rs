//! Оформление в духе KDE Breeze Dark с бирюзовым акцентом Pagh.
//!
//! Все элементы (панель, заголовки, меню, wallpaper) берут цвета отсюда, чтобы
//! композитор и демо-сцены выглядели одинаково и легко перекрашивались.

use crate::color::{rgb, Color};

/// Набор цветов интерфейса.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub wallpaper_top: Color,
    pub wallpaper_bottom: Color,
    pub wallpaper_glow: Color,
    pub wallpaper_glow_strong: Color,

    pub panel_bg: Color,
    pub panel_bg_alt: Color,
    pub panel_border: Color,
    pub panel_icon: Color,
    pub panel_icon_dim: Color,
    pub panel_hover: Color,
    pub panel_text: Color,
    pub panel_text_dim: Color,

    pub accent: Color,
    pub accent_dim: Color,

    pub window_bg: Color,
    pub window_border_active: Color,
    pub window_border_inactive: Color,
    pub titlebar_active: Color,
    pub titlebar_inactive: Color,
    pub titlebar_text: Color,
    pub titlebar_text_dim: Color,
    pub button_hover: Color,
    pub button_close: Color,

    pub menu_bg: Color,
    pub menu_border: Color,
    pub menu_text: Color,
    pub menu_text_dim: Color,
    pub menu_hover: Color,
    pub separator: Color,

    pub chip_bg: Color,
    pub chip_text: Color,

    pub shadow: Color,
    pub cursor_fill: Color,
    pub cursor_outline: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::breeze_dark()
    }
}

impl Theme {
    pub const fn breeze_dark() -> Self {
        Self {
            wallpaper_top: rgb(23, 39, 49),
            wallpaper_bottom: rgb(8, 14, 19),
            wallpaper_glow: rgb(28, 118, 112),
            wallpaper_glow_strong: rgb(37, 150, 138),

            panel_bg: rgb(13, 21, 26),
            panel_bg_alt: rgb(19, 30, 36),
            panel_border: rgb(34, 51, 59),
            panel_icon: rgb(214, 226, 231),
            panel_icon_dim: rgb(118, 136, 145),
            panel_hover: rgb(38, 62, 68),
            panel_text: rgb(231, 240, 243),
            panel_text_dim: rgb(145, 164, 173),

            accent: rgb(50, 179, 163),
            accent_dim: rgb(31, 110, 103),

            window_bg: rgb(30, 39, 45),
            window_border_active: rgb(62, 88, 98),
            window_border_inactive: rgb(37, 48, 55),
            titlebar_active: rgb(43, 59, 68),
            titlebar_inactive: rgb(31, 42, 49),
            titlebar_text: rgb(236, 244, 247),
            titlebar_text_dim: rgb(148, 165, 173),
            button_hover: rgb(66, 92, 101),
            button_close: rgb(209, 84, 84),

            menu_bg: rgb(26, 36, 42),
            menu_border: rgb(55, 76, 85),
            menu_text: rgb(232, 240, 243),
            menu_text_dim: rgb(126, 143, 151),
            menu_hover: rgb(42, 116, 108),
            separator: rgb(48, 64, 71),

            chip_bg: rgb(32, 48, 55),
            chip_text: rgb(203, 219, 224),

            shadow: rgb(0, 0, 0),
            cursor_fill: rgb(250, 250, 250),
            cursor_outline: rgb(10, 12, 14),
        }
    }

    /// Второй вариант обоев — переключение из контекстного меню.
    pub const fn wallpaper_alt(self) -> (Color, Color, Color, Color) {
        (
            rgb(17, 28, 44),
            rgb(4, 8, 15),
            rgb(62, 76, 158),
            rgb(91, 112, 205),
        )
    }
}
