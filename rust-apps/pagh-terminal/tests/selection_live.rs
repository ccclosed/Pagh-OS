//! Живое репро бага «выделение fastfetch стирает картинку/текст».
//!
//! Настоящий композитор `pagh-desktop` (host build), настоящий бинарник
//! `pagh-terminal` по AF_UNIX/PDP/1, настоящий вывод `fastfetch`, снятый с
//! гостя, и настоящая мышь: композитор получает press/motion/release и шлёт их
//! клиенту, клиент сам строит selection и перерисовывает кадр.
//!
//! Проверка пиксельная и сравнивает кадр композитора *до* и *после* протяжки:
//! внутри выделенных клеток фон обязан уйти в подсветку, а каждый глиф —
//! остаться байт-в-байт прежним (старый `selection_fg` перекрашивал весь
//! foreground в один серый и стирал логотип).
//!
//! Скриншоты можно сохранить в каталог из `PAGH_TERMINAL_EVIDENCE` — evidence
//! для ревью; без переменной тест работает во временном каталоге.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pagh_desktop::panel::PANEL_HEIGHT;
use pagh_desktop::png::{decode_png, DecodedPng};
use pagh_desktop::server::{unique_socket_path, Server, ServerOptions};
use pagh_desktop::window::{BORDER, TITLEBAR_HEIGHT};
use pagh_terminal::demo::DemoShell;
use pagh_terminal::font::{CELL_H, CELL_W};
use pagh_terminal::render::{Theme, SELECTION_BG_MIX, VIEWPORT_PADDING};

const SCREEN: (u32, u32) = (1280, 800);
const CONTENT: (u32, u32) = (760, 520);

fn layout(width: u32, height: u32) -> (usize, usize) {
    let pad = VIEWPORT_PADDING * 2;
    let cols = ((width as usize).saturating_sub(pad) / CELL_W).max(2);
    let rows = ((height as usize).saturating_sub(pad) / CELL_H).max(2);
    (cols, rows)
}

fn wait_for<F: FnMut() -> bool>(timeout: Duration, mut predicate: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn temp_dir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pagh-terminal-live-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn evidence_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("PAGH_TERMINAL_EVIDENCE").map(PathBuf::from)?;
    std::fs::create_dir_all(&dir).unwrap();
    Some(dir)
}

fn capture(control: &pagh_desktop::server::ServerControl, dir: &Path, name: &str) -> Vec<u8> {
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    control.screenshot(&path);
    assert!(
        wait_for(Duration::from_secs(5), || path.exists()),
        "кадр {name} не появился"
    );
    std::thread::sleep(Duration::from_millis(60));
    std::fs::read(&path).unwrap()
}

/// Точка в координатах экрана композитора для центра клетки клиента.
fn cell_point(col: usize, row: usize) -> (i32, i32) {
    // Первое окно: cascade (96,64), затем clamp в пределах экрана. Кадр окна =
    // контент + рамка + заголовок; контент начинается на BORDER/TITLEBAR.
    let frame_w = CONTENT.0 as i32 + 2 * BORDER;
    let frame_h = CONTENT.1 as i32 + 2 * BORDER + TITLEBAR_HEIGHT;
    let max_x = (SCREEN.0 as i32 - frame_w - 24).max(16);
    let max_y = (SCREEN.1 as i32 - PANEL_HEIGHT - frame_h - 24).max(16);
    let left = 96.min(max_x) + BORDER;
    let top = 64.min(max_y) + BORDER + TITLEBAR_HEIGHT;
    (
        left + (VIEWPORT_PADDING + col * CELL_W + CELL_W / 2) as i32,
        top + (VIEWPORT_PADDING + row * CELL_H + CELL_H / 2) as i32,
    )
}

fn decode(bytes: &[u8]) -> DecodedPng {
    decode_png(bytes).expect("композитор отдал PNG, который декодируется")
}

fn pixel(img: &DecodedPng, x: usize, y: usize) -> [u8; 4] {
    let i = (y * img.width as usize + x) * 4;
    [
        img.rgba[i],
        img.rgba[i + 1],
        img.rgba[i + 2],
        img.rgba[i + 3],
    ]
}

#[test]
fn live_pointer_selection_keeps_fastfetch_glyphs_and_colours() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fastfetch-guest.script");
    let raw = std::fs::read(&fixture).expect("fastfetch guest fixture");

    // Та же сетка, что у клиента: — по ней вычисляем клетки логотипа, чтобы
    // протяжка шла именно по выводу fastfetch.
    let (cols, rows) = layout(CONTENT.0, CONTENT.1);
    let mut shell = DemoShell::new(cols, rows);
    shell.write(&raw);
    let mut logo_first = None;
    let mut logo_last = 0usize;
    let mut logo_right = 0usize;
    for row in 0..rows {
        let line = shell.terminal.screen.view_line(row);
        for col in 0..cols {
            if line.get(col).is_some_and(|cell| cell.ch == '#') {
                logo_first.get_or_insert(row);
                logo_last = logo_last.max(row);
                logo_right = logo_right.max(col);
            }
        }
    }
    let first = logo_first.expect("в выводе fastfetch нет '#'-логотипа");
    assert!(logo_right > 10, "логотип подозрительно узкий: {logo_right}");

    let shots = temp_dir("shots");
    let socket = unique_socket_path("terminal-selection-live");
    let server = Server::spawn(ServerOptions {
        socket: socket.clone(),
        width: SCREEN.0,
        height: SCREEN.1,
        quiet: std::env::var("PAGH_E2E_VERBOSE").is_err(),
        screenshot_dir: Some(shots.clone()),
        ..ServerOptions::default()
    })
    .expect("композитор стартует");

    let empty = capture(&server.control, &shots, "00-empty.png");
    let mut child: Child = Command::new(env!("CARGO_BIN_EXE_pagh-terminal"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "--size",
            &format!("{}x{}", CONTENT.0, CONTENT.1),
            "--feed",
            fixture.to_str().unwrap(),
            "--title",
            "fastfetch — selection",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("pagh-terminal стартует");

    let mut drawn = None;
    assert!(
        wait_for(Duration::from_secs(20), || {
            let frame = capture(&server.control, &shots, "01-terminal.png");
            if frame != empty {
                drawn = Some(frame);
                true
            } else {
                false
            }
        }),
        "окно терминала не появилось на экране композитора"
    );
    let before_bytes = drawn.unwrap();
    let before = decode(&before_bytes);
    let theme = Theme::default();

    // Протяжка мышью: от логотипа вправо-вниз по info-колонке. Нажатие в
    // контенте (не в заголовке) — композитор фокусирует окно и доставляет
    // pointer-события клиенту.
    let (sx, sy) = cell_point(1, first);
    let (ex, ey) = cell_point(logo_right.min(cols - 1), logo_last);
    server.control.mouse_move(sx, sy);
    std::thread::sleep(Duration::from_millis(120));
    server.control.mouse_button(1, true);
    std::thread::sleep(Duration::from_millis(80));
    let steps = 6;
    for step in 1..=steps {
        let x = sx + (ex - sx) * step / steps;
        let y = sy + (ey - sy) * step / steps;
        server.control.mouse_move(x, y);
        std::thread::sleep(Duration::from_millis(90));
    }
    server.control.mouse_button(1, false);
    // Увести указатель из выделения: композитор рисует стрелку поверх кадра,
    // и пиксель под ней не даёт сравнить фон/глиф. Motion без нажатой кнопки
    // selection не двигает (клиент проверяет held).
    server.control.mouse_move(30, 770);
    // Все Motion/Button команды обрабатываются по очереди; кадр, снятый в
    // середине протяжки, показал бы недорисованное выделение. Даём клиенту
    // доработать события и только потом снимаем итоговый кадр.
    std::thread::sleep(Duration::from_millis(700));

    // Подсветка: в кадре композитора те же цвета, что в клиентском канвасе.
    let highlight = theme.bg.lerp(theme.selection, SELECTION_BG_MIX);
    let x0 = (cell_point(1, first).0 as usize) - 4;
    let y0 = (cell_point(1, first).1 as usize) - CELL_H / 2;
    let x1 = (cell_point(logo_right.min(cols - 1), logo_last).0 as usize) + 4;
    let y1 = (cell_point(logo_right.min(cols - 1), logo_last).1 as usize) + CELL_H / 2;

    // Дождаться устойчивого кадра: два подряд одинаковых снимка *выделенной
    // области* (мигающий курсор вне её не должен считаться изменением).
    let region = |bytes: &[u8]| -> Vec<u8> {
        let img = decode(bytes);
        let mut out = Vec::with_capacity((x1 - x0) * (y1 - y0) * 4);
        for y in y0..y1.min(img.height as usize) {
            for x in x0..x1.min(img.width as usize) {
                out.extend_from_slice(&pixel(&img, x, y));
            }
        }
        out
    };
    let mut after_bytes = capture(&server.control, &shots, "02-selected-a.png");
    let mut after_region = region(&after_bytes);
    let mut stable = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(150));
        let next = capture(&server.control, &shots, "02-selected.png");
        let next_region = region(&next);
        if next_region == after_region {
            stable = true;
            after_bytes = next;
            break;
        }
        after_bytes = next;
        after_region = next_region;
    }
    assert!(stable, "кадр терминала не стабилизировался после выделения");
    assert_ne!(
        after_region,
        region(&before_bytes),
        "выделение не изменило выделенную область кадра"
    );
    let after = decode(&after_bytes);

    let mut background = 0usize;
    let mut glyph = 0usize;
    for y in y0..y1.min(before.height as usize) {
        for x in x0..x1.min(before.width as usize) {
            let b = pixel(&before, x, y);
            let a = pixel(&after, x, y);
            if b[..3] == theme.bg.to_array()[..] {
                background += 1;
                assert_eq!(
                    a[..3],
                    highlight.to_array()[..],
                    "фон выделенной клетки не стал подсветкой в ({x},{y})"
                );
            } else if a == b {
                // Ядро глифа (полное покрытие): цвет обязан остаться прежним.
                glyph += 1;
            }
            // Остальные не-фоновые пиксели — сглаженные края: они законно
            // смешиваются с новым фоном, поэтому их изменение не проверяем.
        }
    }
    assert!(
        background > 5_000,
        "выделение почти не попало в кадр: фоновых пикселей {background}"
    );
    assert!(glyph > 1_000, "в выделении не осталось глифов: {glyph}");

    server.control.close_window(101);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    server.control.shutdown();
    server.join().expect("композитор завершился");

    if let Some(dir) = evidence_dir() {
        std::fs::copy(shots.join("01-terminal.png"), dir.join("live-before.png")).unwrap();
        std::fs::copy(shots.join("02-selected.png"), dir.join("live-after.png")).unwrap();
    }

    let _ = std::fs::remove_dir_all(&shots);
}
