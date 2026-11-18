//! # pagh-files
//!
//! Файловый менеджер **Files** рабочего стола Pagh (вариант B: userspace-
//! композитор, приложение — статический musl-бинарник).
//!
//! Окно рисуется целиком внутри приложения (заголовок `v`/`^`/`x`, панель
//! инструментов, боковая панель, сетка/список, строка состояния), поэтому
//! композитору остаётся только композитинг готового кадра.
//!
//! ## Состав
//!
//! - [`vfs`] — трейт [`Vfs`] (read_dir/stat/mkdir/create/write/unlink/rename/
//!   copy/symlink) и работа с путями;
//! - [`fake`] — [`FakeVfs`] с деревом из концепта (24 папки, 4 файла, 76,3 МБ);
//! - [`host_vfs`] — [`HostVfs`] поверх `std::fs` (работает и на Pagh, где
//!   Linux-вызовы обслуживает слой совместимости ядра);
//! - [`format`] — русские склонения, размеры, даты;
//! - [`menu_model`] — контекстное меню Dolphin-стиля с подменю «Создать»;
//! - [`app`] — состояние и отрисовка окна, обработка событий, [`Command`];
//! - [`screenshots`] — детерминированные демо-сцены для PNG и golden-тестов;
//! - [`icons`] — готовые иконки Fluent (MIT) для записей и сайдбара, с
//!   собственным PNG-декодером ([`png`] + [`deflate`]) без внешних зависимостей.
//!
//! ## Запуск
//!
//! ```text
//! cargo test                                   # VFS, меню, приложение, скриншоты
//! cargo run -- --out screenshots                # отрисовать демо-PNG
//! cargo run -- --connect /run/pagh-desktop.sock # окно в живом композиторе
//! ```
//!
//! ```text
//! cargo build --release --target x86_64-unknown-linux-musl
//! ```

pub mod app;
pub mod deflate;
pub mod fake;
pub mod format;
pub mod host_vfs;
pub mod icons;
pub mod menu_model;
pub mod png;
pub mod screenshots;
pub mod vfs;

pub use app::{Command, FilesApp, Place};
pub use fake::FakeVfs;
pub use host_vfs::HostVfs;
pub use menu_model::{SortMode, ViewMode};
pub use vfs::{DirEntry, EntryKind, FsError, Stat, Vfs};

/// Версия приложения.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Имя приложения в протоколе композитора.
pub const APP_NAME: &str = "pagh-files";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_metadata() {
        assert_eq!(APP_NAME, "pagh-files");
        assert!(VERSION.starts_with('0'));
    }
}
