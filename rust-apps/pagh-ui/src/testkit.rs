//! Хелперы для headless-демо и screenshot-тестов.
//!
//! Рендер чисто целочисленный, поэтому один и тот же сценарий даёт одинаковый
//! буфер на любой машине. Тесты сравнивают 64-битный хеш FNV-1a с записанным
//! эталоном; при расхождении PNG сцены сохраняется в каталог вывода, чтобы
//! можно было посмотреть, что изменилось.
//!
//! Каталог вывода: переменная `PAGH_SCREENSHOT_DIR`, иначе `screenshots/`
//! рядом с текущим каталогом.

use std::io;
use std::path::PathBuf;

use crate::canvas::Canvas;
use crate::color::Color;
use crate::geom::Size;
use crate::image::Image;

/// Каталог, куда пишутся PNG-скриншоты.
pub fn output_dir() -> PathBuf {
    match std::env::var_os("PAGH_SCREENSHOT_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from("screenshots"),
    }
}

/// Сохранить сцену в PNG, «сплющив» её на непрозрачный фон.
pub fn save_png(name: &str, canvas: &Canvas, background: Color) -> io::Result<PathBuf> {
    let path = output_dir().join(format!("{name}.png"));
    Image::flatten(canvas, background).save_png_creating_dirs(&path)?;
    Ok(path)
}

/// Сохранить PPM (удобно для отладки без PNG).
pub fn save_ppm(name: &str, canvas: &Canvas, background: Color) -> io::Result<PathBuf> {
    let path = output_dir().join(format!("{name}.ppm"));
    Image::flatten(canvas, background).save_ppm_creating_dirs(&path)?;
    Ok(path)
}

/// Хеш сцены (FNV-1a по пикселям).
pub fn hash(canvas: &Canvas) -> u64 {
    Image::from_canvas(canvas).fnv1a()
}

/// Проверить сцену против эталонного хеша.
///
/// При расхождении сцена сохраняется в каталог вывода, а тест падает с
/// подсказкой: посмотреть PNG и обновить эталон.
pub fn assert_golden(name: &str, canvas: &Canvas, expected: u64) {
    let actual = hash(canvas);
    if actual == expected {
        return;
    }
    let path = save_png(name, canvas, Color::BLACK)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("<не сохранено: {error}>"));
    panic!(
        "скриншот `{name}` не совпал с эталоном:\n  ожидался {expected:#018x}\n  получен  {actual:#018x}\n  PNG: {path}\n\
         если изменение ожидаемо, обновите хеш в тесте и приложите новый PNG к PR",
    );
}

/// Создать композицию «рабочий стол»: фон + окно, центрированное на экране.
pub fn desktop_scene(screen: Size, background: Color) -> Canvas {
    Canvas::filled(screen.w, screen.h, background)
}

/// Центрированное окно заданного размера на экране.
pub fn centered(size: Size, screen: Size) -> crate::geom::Rect {
    crate::geom::Rect::new(0, 0, size.w, size.h)
        .centered_in(crate::geom::Rect::new(0, 0, screen.w, screen.h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_depends_on_pixels() {
        let canvas = Canvas::filled(8, 8, Color::rgb(1, 2, 3));
        assert_eq!(hash(&canvas), hash(&canvas));
        let other = Canvas::filled(8, 8, Color::rgb(3, 2, 1));
        assert_ne!(hash(&canvas), hash(&other));
    }

    #[test]
    fn output_dir_can_be_overridden() {
        let key = "PAGH_SCREENSHOT_DIR";
        let previous = std::env::var_os(key);
        std::env::set_var(key, "/tmp/pagh-ui-shots");
        assert_eq!(output_dir(), PathBuf::from("/tmp/pagh-ui-shots"));
        match previous {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn saves_png_into_output_dir() {
        let dir = std::env::temp_dir().join(format!("pagh-ui-test-{}", std::process::id()));
        std::env::set_var("PAGH_SCREENSHOT_DIR", &dir);
        let canvas = Canvas::filled(16, 16, Color::hex(0x081216));
        let path = save_png("unit-test", &canvas, Color::BLACK).unwrap();
        assert!(path.exists());
        assert_eq!(path.extension().unwrap(), "png");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], &[0x89, b'P', b'N', b'G']);
        let _ = std::fs::remove_file(path);
        std::env::remove_var("PAGH_SCREENSHOT_DIR");
    }

    #[test]
    fn centered_window() {
        let rect = centered(Size::new(100, 50), Size::new(400, 300));
        assert_eq!(rect.x, 150);
        assert_eq!(rect.y, 125);
    }
}
