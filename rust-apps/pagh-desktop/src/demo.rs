//! Демонстрационный контент тестовых клиентов: Terminal, Files и Paint.
//!
//! Эти поверхности рисуются тем же программным холстом, что и композитор, поэтому
//! скриншоты воспроизводимы без GPU и без внешних ресурсов.

use crate::canvas::Canvas;
use crate::color::{self, Color};
use crate::geometry::{Point, Rect};
use crate::surface::{PixelFormat, Surface};

/// Состояние демо-клиента, влияющее на отрисовку.
#[derive(Clone, Debug)]
pub struct DemoState {
    pub mouse: Option<Point>,
    pub last_input: String,
    pub clicked: bool,
    pub focused: bool,
    pub scroll: i32,
}

impl Default for DemoState {
    fn default() -> Self {
        Self {
            mouse: None,
            last_input: "none".into(),
            clicked: false,
            focused: true,
            scroll: 0,
        }
    }
}

fn text_width(text: &str) -> i32 {
    text.chars().count() as i32 * 8
}

/// Терминальное окно: только сетка текста на тёмном фоне.
///
/// Визуальный референс — KDE Konsole (Breeze dark): почти чёрный контент
/// `#232627`, светлый моноширинный текст `#eff0f1`, блочный курсор, никакой
/// шапки, кнопок, градиентов, панелей ввода и статус-строки.  Заголовок,
/// кнопки окна и панель задач принадлежат композитору (см.
/// `rust-apps/pagh-desktop/PROTOCOL.md`), поэтому клиентская поверхность их
/// не дублирует.  Вкладок нет: терминал зеркалит один kernel-VT
/// (`/dev/vtconsole0`), то есть одну сессию.
pub fn terminal_surface(width: u32, height: u32, _state: &DemoState) -> Surface {
    const TERM_BG: Color = color::rgb(35, 38, 39);
    const TERM_TEXT: Color = color::rgb(239, 240, 241);
    const TERM_DIM: Color = color::rgb(127, 140, 141);

    let mut canvas = Canvas::new(width, height, TERM_BG);
    let h = height as i32;
    // Отступ — 4 px, как поля консольной сетки; всё остальное — рабочий текст.
    let x = 4;
    let line_h = 16;
    let lines: [(&str, Color); 9] = [
        ("pagh 2.24.0 | userspace compositor", TERM_DIM),
        ("", TERM_TEXT),
        ("pagh:/mnt$ uname -a", TERM_TEXT),
        ("Pagh-OS 2.24.0 x86_64", TERM_TEXT),
        ("pagh:/mnt$ pagh-desktop --headless", TERM_TEXT),
        ("PDP/1: hello -> welcome (1280x720)", TERM_DIM),
        ("pagh:/mnt$ ./demo-client --app terminal", TERM_TEXT),
        ("commit 640x400 xrgb8888 -> frame delivered", TERM_DIM),
        ("", TERM_TEXT),
    ];
    let mut y = 4;
    for (line, line_color) in lines {
        if y + line_h > h {
            break;
        }
        if !line.is_empty() {
            canvas.text(x, y, 1, line_color, line);
        }
        y += line_h;
    }
    // Приглашение и единственный блочный курсор — как в терминале, а не
    // «жирная кнопка»: подсветка 8x16 сразу после пробела.
    let prompt = "pagh:/mnt$ ";
    if y + line_h <= h {
        canvas.text(x, y, 1, TERM_TEXT, prompt);
        canvas.fill_rect(Rect::new(x + text_width(prompt), y, 8, line_h), TERM_TEXT);
    }
    Surface::from_canvas(&canvas, PixelFormat::Xrgb8888)
}

const FILES_BG: Color = color::rgb(25, 33, 38);
const FILES_TOOLBAR: Color = color::rgb(31, 42, 49);
const FILES_SIDEBAR: Color = color::rgb(21, 29, 34);
const FILES_TEXT: Color = color::rgb(220, 232, 237);
const FILES_DIM: Color = color::rgb(134, 152, 161);
const FILES_ACCENT: Color = color::rgb(50, 179, 163);

/// Окно файлового менеджера (Dolphin-подобное): sidebar, toolbar, grid, statusbar.
pub fn files_surface(width: u32, height: u32, state: &DemoState) -> Surface {
    let mut canvas = Canvas::new(width, height, FILES_BG);
    let w = width as i32;
    let h = height as i32;

    // Toolbar.
    canvas.fill_rect(Rect::new(0, 0, w, 38), FILES_TOOLBAR);
    // Стрелки назад/вперёд.
    canvas.line(Point::new(22, 13), Point::new(16, 19), FILES_TEXT, 2);
    canvas.line(Point::new(16, 19), Point::new(22, 25), FILES_TEXT, 2);
    canvas.line(Point::new(36, 13), Point::new(42, 19), FILES_DIM, 2);
    canvas.line(Point::new(42, 19), Point::new(36, 25), FILES_DIM, 2);
    // Путь.
    canvas.fill_rounded_rect(Rect::new(56, 8, 300, 22), 6, FILES_SIDEBAR, 255);
    canvas.text(66, 11, 1, FILES_TEXT, "/mnt");
    // Поиск.
    canvas.fill_rounded_rect(Rect::new(w - 170, 8, 158, 22), 6, FILES_SIDEBAR, 255);
    canvas.text(w - 158, 11, 1, FILES_DIM, "Search...");
    canvas.fill_rect(Rect::new(0, 38, w, 1), color::rgb(43, 58, 66));

    // Sidebar.
    canvas.fill_rect(Rect::new(0, 39, 130, h - 39 - 22), FILES_SIDEBAR);
    canvas.text(12, 52, 1, FILES_DIM, "PLACES");
    let entries = ["Home", "Network", "Devices", "Pagh-OS"];
    for (index, entry) in entries.iter().enumerate() {
        let y = 74 + index as i32 * 30;
        let selected = index == 0;
        if selected {
            canvas.fill_rounded_rect(Rect::new(6, y - 5, 118, 24), 5, color::rgb(35, 94, 88), 255);
        }
        // Иконка папки/диска.
        let icon_color = if selected { FILES_ACCENT } else { FILES_DIM };
        canvas.fill_rounded_rect(Rect::new(14, y, 14, 10), 2, icon_color, 255);
        canvas.fill_rounded_rect(Rect::new(14, y - 3, 7, 4), 1, icon_color, 255);
        canvas.text(
            36,
            y - 3,
            1,
            if selected { FILES_TEXT } else { FILES_DIM },
            entry,
        );
    }

    // Сетка файлов.
    canvas.text(146, 52, 1, FILES_DIM, "FOLDERS");
    let items = ["bin", "etc", "home", "mnt", "opt", "root", "tmp", "usr"];
    let cell_w = 100;
    for (index, name) in items.iter().enumerate() {
        let column = index as i32 % 4;
        let row = index as i32 / 4;
        let x = 146 + column * cell_w;
        let y = 72 + row * 84;
        let selected = *name == "mnt";
        let cell = Rect::new(x, y, 84, 72);
        if selected {
            canvas.fill_rounded_rect(cell, 8, color::rgb(35, 94, 88), 255);
        }
        // Папка.
        let folder = Rect::new(x + 24, y + 12, 36, 26);
        canvas.fill_rounded_rect(
            folder,
            4,
            if selected { FILES_ACCENT } else { FILES_DIM },
            255,
        );
        canvas.fill_rounded_rect(
            Rect::new(folder.x, folder.y - 7, 16, 9),
            2,
            if selected { FILES_ACCENT } else { FILES_DIM },
            255,
        );
        canvas.fill_rounded_rect(folder.inset(3), 2, FILES_BG, 255);
        canvas.text(
            x + 42 - text_width(name) / 2,
            y + 48,
            1,
            if selected { FILES_TEXT } else { FILES_DIM },
            name,
        );
        let _ = state;
    }

    // Statusbar.
    canvas.fill_rect(Rect::new(0, h - 22, w, 22), color::rgb(18, 25, 29));
    canvas.text(12, h - 19, 1, FILES_DIM, "8 items | ext2 | 2.1 GiB free");
    let mouse = state
        .mouse
        .map(|mouse| format!("mouse {},{}", mouse.x, mouse.y))
        .unwrap_or_else(|| "mouse -".to_string());
    canvas.text(w - 16 - text_width(&mouse), h - 19, 1, FILES_DIM, &mouse);

    Surface::from_canvas(&canvas, PixelFormat::Xrgb8888)
}

/// Окно Paint: холст с рисунком и палитра.
pub fn paint_surface(width: u32, height: u32, state: &DemoState) -> Surface {
    let mut canvas = Canvas::new(width, height, color::rgb(27, 34, 38));
    let w = width as i32;
    let h = height as i32;
    canvas.fill_gradient(
        Rect::new(0, 0, w, 40),
        color::rgb(38, 52, 58),
        color::rgb(23, 30, 34),
    );
    canvas.text(14, 12, 1, color::rgb(230, 240, 243), "Paint - Pagh");
    canvas.text(
        w - 16 - text_width("untitled.pnt"),
        12,
        1,
        FILES_DIM,
        "untitled.pnt",
    );

    let canvas_rect = Rect::new(12, 48, w - 24, h - 48 - 64);
    canvas.fill_rounded_rect(canvas_rect, 6, color::rgb(14, 20, 24), 255);
    canvas.stroke_rounded_rect(canvas_rect, 6, color::rgb(48, 64, 71), 255);
    // Рисунок: солнце, горы, вода.
    let sun = Point::new(
        canvas_rect.x + canvas_rect.w / 3,
        canvas_rect.y + canvas_rect.h / 3,
    );
    canvas.fill_circle(sun, 34, color::rgb(235, 190, 90), 220);
    canvas.fill_polygon(
        &[
            Point::new(canvas_rect.x + 20, canvas_rect.bottom() - 70),
            Point::new(canvas_rect.x + canvas_rect.w / 2, canvas_rect.y + 70),
            Point::new(canvas_rect.right() - 30, canvas_rect.bottom() - 70),
        ],
        color::rgb(46, 84, 92),
        255,
    );
    canvas.fill_polygon(
        &[
            Point::new(canvas_rect.x + 30, canvas_rect.bottom() - 20),
            Point::new(
                canvas_rect.x + canvas_rect.w / 3,
                canvas_rect.bottom() - 110,
            ),
            Point::new(canvas_rect.right() - 24, canvas_rect.bottom() - 20),
        ],
        color::rgb(30, 60, 70),
        255,
    );
    canvas.fill_rect(
        Rect::new(canvas_rect.x, canvas_rect.bottom() - 22, canvas_rect.w, 22),
        color::rgb(22, 57, 68),
    );
    // Палитра.
    let palette = [
        color::rgb(236, 240, 241),
        color::rgb(226, 92, 92),
        color::rgb(235, 190, 90),
        color::rgb(124, 202, 154),
        color::rgb(50, 179, 163),
        color::rgb(94, 156, 222),
        color::rgb(168, 132, 218),
        color::rgb(40, 44, 48),
    ];
    for (index, color) in palette.iter().enumerate() {
        let rect = Rect::new(14 + index as i32 * 32, h - 50, 24, 24);
        canvas.fill_rounded_rect(rect, 5, *color, 255);
        if index == 4 {
            canvas.stroke_rounded_rect(rect.inflate(2), 6, color::WHITE, 255);
        }
    }
    let status = format!(
        "brush 3 px | {}{}",
        state.last_input,
        if state.clicked { " · drawing" } else { "" }
    );
    canvas.text(14, h - 22, 1, FILES_DIM, &status);
    Surface::from_canvas(&canvas, PixelFormat::Xrgb8888)
}

/// Текущее UTC-время и дата в форматах панели (`HH:MM` и `DD.MM.YYYY`).
pub fn clock_now() -> (String, String) {
    format_unix(unix_now())
}

/// Секунды с UNIX epoch по стенным часам (один syscall `clock_gettime`).
///
/// В Pagh `CLOCK_REALTIME` читает CMOS RTC, а это медленная port-I/O операция:
/// вызывать её из кадра нельзя (измерено: десятки миллисекунд под TCG). Панель
/// берёт базовую точку один раз и дальше идёт по монотонным `Instant`.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `(HH:MM, DD.MM.YYYY)` из секунд UNIX epoch — чистая функция (host-тесты).
pub fn format_unix(seconds: u64) -> (String, String) {
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    let time = format!("{:02}:{:02}", (seconds / 3600) % 24, (seconds / 60) % 60);
    let date = format!("{day:02}.{month:02}.{year:04}");
    (time, date)
}

/// Гражданская дата из числа дней с 1970-01-01 (алгоритм Говарда Хиннанта).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_unix_is_the_panel_clock() {
        // 2026-11-15 23:38:00 UTC (the fixed demo clock) and the epoch.
        assert_eq!(
            format_unix(1_794_785_880),
            ("23:38".to_string(), "15.11.2026".to_string())
        );
        assert_eq!(
            format_unix(0),
            ("00:00".to_string(), "01.01.1970".to_string())
        );
        // A minute later only the time changes.
        assert_eq!(format_unix(1_794_785_940).0, "23:39");
    }

    #[test]
    fn terminal_surface_has_expected_size_and_pixels() {
        let surface = terminal_surface(640, 400, &DemoState::default());
        assert_eq!((surface.width, surface.height), (640, 400));
        assert_ne!(surface.get(10, 10), 0);
        assert_ne!(surface.get(320, 200), 0);
    }

    #[test]
    fn files_and_paint_draw() {
        let state = DemoState::default();
        let files = files_surface(560, 340, &state);
        let paint = paint_surface(600, 380, &state);
        assert_eq!((files.width, files.height), (560, 340));
        assert_eq!((paint.width, paint.height), (600, 380));
        assert_ne!(files.get(100, 100), 0);
        assert_ne!(paint.get(300, 200), 0);
    }

    #[test]
    fn clock_conversion_matches_known_date() {
        // 2026-11-15T23:38:00Z = 1_794_785_880 секунд (проверено календарём).
        let days = 1_794_785_880 / 86_400;
        let (year, month, day) = civil_from_days(days);
        assert_eq!((year, month, day), (2026, 11, 15));
    }
}
