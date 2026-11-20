//! # pagh-ui
//!
//! Клиентский UI-тулкит рабочего стола Pagh (вариант B: userspace-композитор).
//!
//! Библиотека без внешних зависимостей: собственный растр, встроенный шрифт,
//! PNG-энкодер и клиент протокола **PDP/1** (его серверная половина —
//! `rust-apps/pagh-desktop`). Всё, что нужно приложению, — это [`Canvas`],
//! [`Theme`], набор виджетов и [`client::DesktopClient`] для связи с
//! композитором. Клиент рисует только клиентскую область: заголовок окна,
//! кнопки `v`/`^`/`x`, панель, размещение и фокус принадлежат композитору.
//!
//! ## Из чего состоит
//!
//! - [`geom`], [`color`], [`canvas`] — целочисленная геометрия, RGBA-цвет и
//!   программный рендер (скругления, тени, градиенты, сглаживание по SDF).
//! - [`font`] — встроенный растровый шрифт DejaVu Sans 13 px с русским
//!   алфавитом (сгенерирован [`tools/gen_font.py`]).
//! - [`theme`] — палитра и метрики, снятые с концепта `docs/file-manager-concept.png`.
//! - [`layout`] — flex-полосы и сетка значков.
//! - [`widgets`] — подпись, кнопка, поле ввода, список с секциями, сетка.
//! - [`menu`] — контекстные меню и подменю с раскладкой и hit-тестами.
//! - [`layer`] — статический слой кадра (кэш фона/панели для damage-рендера).
//! - [`chrome`] — заголовок окна (`v`/`^`/`x`), строка состояния.
//! - [`icon`] — векторные иконки, включая синюю папку с листом бумаги.
//! - [`protocol`], [`client`] — клиентская часть протокола PDP/1 (включая
//!   расширение `launch`, type 13).
//! - [`image`], [`testkit`] — PNG/PPM и screenshot-тесты.
//! - [`demo`] — готовые демо-сцены (витрина, меню, палитра).
//!
//! ## Быстрый старт
//!
//! ```no_run
//! use pagh_ui::{canvas::Canvas, chrome, geom::Rect, theme::Theme};
//!
//! let theme = Theme::pagh_dark();
//! let mut canvas = Canvas::filled(640, 400, theme.palette.desktop);
//! let layout = chrome::draw_window(
//!     &mut canvas,
//!     &theme,
//!     Rect::new(20, 20, 600, 360),
//!     chrome::WindowStyle::new("Домашняя папка — Files"),
//! );
//! // ... нарисовать содержимое в layout.content ...
//! ```
//!
//! Демо-бинарник `pagh-ui-gallery` рисует витрину виджетов в PNG без экрана:
//!
//! ```text
//! cargo run --bin pagh-ui-gallery -- --out screenshots
//! ```

pub mod canvas;
pub mod chrome;
pub mod client;
pub mod color;
pub mod damage;
pub mod demo;
pub mod event;
pub mod font;
pub mod font_data;
pub mod geom;
pub mod icon;
pub mod image;
pub mod layer;
pub mod layout;
pub mod menu;
pub mod protocol;
pub mod shm;
pub mod testkit;
pub mod theme;
pub mod widgets;

pub use canvas::Canvas;
pub use chrome::WindowStyle;
pub use client::DesktopClient;
pub use color::Color;
pub use geom::{Insets, Point, Rect, Size};
pub use theme::Theme;

/// Версия тулкита.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Имя протокола, который реализует тулкит.
pub const PROTOCOL_NAME: &str = "PDP/1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_version_matches_manifest() {
        assert!(!VERSION.is_empty());
        assert!(VERSION.starts_with('0'));
    }

    #[test]
    fn public_api_is_usable_without_defaults() {
        let theme = Theme::pagh_dark();
        let mut canvas = Canvas::filled(320, 200, theme.palette.desktop);
        chrome::draw_window(
            &mut canvas,
            &theme,
            Rect::new(10, 10, 300, 180),
            WindowStyle::new("Тест"),
        );
        assert!(testkit::hash(&canvas) != 0);
    }
}
