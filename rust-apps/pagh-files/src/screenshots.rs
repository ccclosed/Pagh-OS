//! Демонстрационные сцены Files: окно на «обоях» рабочего стола.
//!
//! Сцены детерминированы (фейковая ФС, встроенный шрифт, целочисленный рендер)
//! и используются и демо-бинарником (`pagh-files --out screenshots`), и
//! screenshot-тестами. Событийные сцены проходят через настоящий
//! [`FilesApp::handle_event`], поэтому заодно проверяют меню и диалоги.

use std::path::{Path, PathBuf};

use pagh_ui::canvas::Canvas;
use pagh_ui::chrome::{self, WindowStyle};
use pagh_ui::color::Color;
use pagh_ui::event::{Event, Key, Modifiers, MouseButton};
use pagh_ui::geom::{Point, Rect, Size};
use pagh_ui::image::Image;

use crate::app::FilesApp;
use crate::fake::FakeVfs;

/// Размер окна Files (как в концепте).
pub const WINDOW: Size = Size::new(1060, 640);
/// Размер виртуального экрана для скриншотов.
pub const DESKTOP: Size = Size::new(1280, 800);

/// Приложение с деревом из концепта.
pub fn demo_app() -> FilesApp {
    FilesApp::new(Box::new(FakeVfs::concept()), WINDOW)
}

/// Отрисовать окно приложения в отдельную поверхность.
pub fn render_window(app: &FilesApp) -> Canvas {
    let mut canvas = Canvas::filled(app.size().w, app.size().h, app.theme.palette.desktop);
    app.draw(&mut canvas);
    canvas
}

/// Обои рабочего стола: тёмный градиент и бирюзовое свечение.
pub fn wallpaper(size: Size, theme: &pagh_ui::theme::Theme) -> Canvas {
    let mut canvas = Canvas::filled(size.w, size.h, theme.palette.desktop);
    canvas.vertical_gradient(
        canvas.rect(),
        theme.palette.desktop.lerp(Color::hex(0x14393d), 80),
        theme.palette.desktop,
    );
    // Мягкое свечение у нижней кромки: круги уходят за экран, поэтому
    // внутри кадра нет видимой границы фигуры.
    canvas.fill_circle(
        Point::new(size.w / 2, size.h + 210),
        460,
        theme.palette.accent.with_alpha(14),
    );
    canvas.fill_circle(
        Point::new(80, size.h + 160),
        300,
        theme.palette.accent.with_alpha(9),
    );
    canvas
}

/// Изобразить окно так, как его нарисует композитор: заголовок с `v`/`^`/`x`
/// плюс клиентская область от Files.
///
/// Это **только демо/скриншот**: в реальной связке оформление рисует
/// `pagh-desktop`, а клиент отдаёт поверхность без заголовка (см.
/// `files-under-pagh-desktop.png` — кадр настоящего композитора).
pub fn compose_window(app: &FilesApp) -> Canvas {
    let theme = &app.theme;
    let title_height = theme.metrics.title_height;
    let total = Size::new(app.size().w, app.size().h + title_height);
    let mut frame = Canvas::filled(total.w, total.h, theme.palette.window);
    let layout = chrome::draw_window(
        &mut frame,
        theme,
        Rect::new(0, 0, total.w, total.h),
        WindowStyle::new(&app.title()),
    );
    let client = render_window(app);
    frame.blit(&client, layout.content.origin());
    frame
}

/// Поставить окно приложения на обои рабочего стола.
pub fn compose(app: &FilesApp, desktop: Size) -> Canvas {
    let theme = &app.theme;
    let mut canvas = wallpaper(desktop, theme);
    let window = compose_window(app);
    let at = Point::new(
        (desktop.w - window.width()) / 2,
        (desktop.h - window.height()) / 2,
    );
    canvas.soft_shadow(
        Rect::new(at.x, at.y, window.width(), window.height()),
        theme.metrics.radius_window,
        26,
        theme.palette.shadow,
    );
    canvas.blit(&window, at);
    canvas
}

/// Домашняя папка в режиме значков.
pub fn main_scene() -> Canvas {
    let app = demo_app();
    compose(&app, DESKTOP)
}

/// Контекстное меню с открытым подменю «Создать».
pub fn context_menu_scene() -> Canvas {
    let mut app = demo_app();
    let index = app
        .entries()
        .iter()
        .position(|entry| entry.name == "boot")
        .unwrap_or(0);
    let Some(rect) = app.item_rect(index) else {
        return compose(&app, DESKTOP);
    };
    app.handle_event(&Event::MouseDown {
        pos: rect.center(),
        button: MouseButton::Right,
    });
    // Навести на «Создать», чтобы подменю раскрылось.
    if let Some((root, _)) = app.menu_layouts() {
        if let Some(row) = root.row(0) {
            app.handle_event(&Event::MouseMove { pos: row.center() });
        }
    }
    // Навести на «Пустой файл…» в подменю.
    if let Some((_, Some(submenu))) = app.menu_layouts() {
        if let Some(row) = submenu.row(2) {
            app.handle_event(&Event::MouseMove { pos: row.center() });
        }
    }
    compose(&app, DESKTOP)
}

/// Диалог «Свойства» для файла.
pub fn properties_scene() -> Canvas {
    let mut app = demo_app();
    let index = app
        .entries()
        .iter()
        .position(|entry| entry.name == "notes.md")
        .unwrap_or(0);
    app.select(Some(index));
    app.handle_event(&Event::Key {
        key: Key::Enter,
        pressed: true,
        modifiers: Modifiers {
            shift: false,
            ctrl: false,
            alt: true,
        },
    });
    compose(&app, DESKTOP)
}

/// Подробный список с выделенным файлом.
pub fn list_view_scene() -> Canvas {
    let mut app = demo_app();
    let home = app.home();
    app.navigate_to(&crate::vfs::join(&home, "Documents"));
    app.set_view(crate::ViewMode::List);
    let index = app
        .entries()
        .iter()
        .position(|entry| entry.name == "Заметки.md")
        .unwrap_or(0);
    app.select(Some(index));
    compose(&app, DESKTOP)
}

/// Поиск по папке включён.
pub fn search_scene() -> Canvas {
    let mut app = demo_app();
    app.show_search("doc");
    compose(&app, DESKTOP)
}

/// Пустая папка (Рабочий стол).
pub fn empty_folder_scene() -> Canvas {
    let mut app = demo_app();
    let home = app.home();
    app.navigate_to(&crate::vfs::join(&home, "Desktop"));
    compose(&app, DESKTOP)
}

/// Все сцены по порядку.
pub fn all_scenes() -> Vec<(&'static str, Canvas)> {
    vec![
        ("files-main", main_scene()),
        ("files-context-menu", context_menu_scene()),
        ("files-properties", properties_scene()),
        ("files-list-view", list_view_scene()),
        ("files-search", search_scene()),
        ("files-empty", empty_folder_scene()),
    ]
}

/// Записать сцену в PNG.
pub fn save_scene(dir: &Path, name: &str, canvas: &Canvas) -> std::io::Result<PathBuf> {
    let path = dir.join(format!("{name}.png"));
    Image::from_canvas(canvas).save_png_creating_dirs(&path)?;
    Ok(path)
}

/// Записать все сцены в каталог; вернуть `(имя, путь, хеш)`.
pub fn write_scenes(dir: &Path) -> std::io::Result<Vec<(String, PathBuf, u64)>> {
    let mut written = Vec::new();
    for (name, canvas) in all_scenes() {
        let path = save_scene(dir, name, &canvas)?;
        written.push((name.to_string(), path, pagh_ui::testkit::hash(&canvas)));
    }
    Ok(written)
}
