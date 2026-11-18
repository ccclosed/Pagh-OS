//! Headless-рендер сетевого флайаута: PNG «до/после» и проверки кадра.
//!
//! Композитор рисует 1280×720 в память, поэтому вид флайаута проверяется без
//! QEMU/дисплея. PNG-файлы кладутся в `.cache/netfly/` (каталог git-ignored):
//! `flyout-online.png` — сеть есть, `flyout-offline.png` — сети нет.

use pagh_desktop::compositor::{Compositor, MouseButton};
use pagh_desktop::geometry::Rect;
use pagh_desktop::panel;
use pagh_desktop::png;

fn open_network_flyout(compositor: &mut Compositor) -> Rect {
    let center = panel::network_rect(compositor.screen()).center();
    compositor.pointer_motion(center);
    compositor.pointer_button(MouseButton::Left, true);
    compositor.pointer_button(MouseButton::Left, false);
    let flyout = compositor.flyout().expect("клик по иконке сети открывает флайаут");
    assert_eq!(flyout.kind, pagh_desktop::compositor::FlyoutKind::Network);
    flyout.rect
}

/// Пиксели цвета `color` внутри `area` (центры пикселей, границы включительно).
fn count_color(compositor: &Compositor, area: Rect, color: pagh_desktop::color::Color) -> usize {
    let mut count = 0;
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if compositor.frame().get(x, y) == color {
                count += 1;
            }
        }
    }
    count
}

/// Число пикселей, где кадры различаются.
fn diff(a: &pagh_desktop::canvas::Canvas, b: &pagh_desktop::canvas::Canvas) -> usize {
    a.pixels()
        .iter()
        .zip(b.pixels().iter())
        .filter(|(left, right)| left != right)
        .count()
}

#[test]
fn network_flyout_online_offline_render() {
    let mut compositor = Compositor::new(1280, 720);
    let rect = open_network_flyout(&mut compositor);

    let panel_top = panel::panel_rect(compositor.screen()).y;
    assert!(
        rect.bottom() <= panel_top,
        "флайаут должен прилегать к панели: низ {}, панель {}",
        rect.bottom(),
        panel_top
    );

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.cache/netfly");
    std::fs::create_dir_all(&dir).expect("каталог скриншотов");

    compositor.render_full();
    png::write_png(dir.join("flyout-online.png"), compositor.frame()).expect("PNG online");
    let online = compositor.frame().snapshot();

    // «Подключено» — акцентная строка статуса; фейковых элементов быть не должно,
    // но акцент обязан присутствовать, иначе статус не отрисован.
    let accent_pixels = count_color(&compositor, rect, compositor.theme().accent);
    assert!(
        accent_pixels > 20,
        "статус «Подключено» не нарисован акцентом: {accent_pixels}"
    );

    compositor.set_network_online(false);
    compositor.render_full();
    png::write_png(dir.join("flyout-offline.png"), compositor.frame()).expect("PNG offline");
    let offline = compositor.frame().snapshot();

    let offline_accent = count_color(&compositor, rect, compositor.theme().accent);
    assert_eq!(offline_accent, 0, "в отключённом состоянии акцент не нужен");
    assert!(
        diff(&online, &offline) > 40,
        "состояния online/offline визуально не различаются"
    );

    // Кадр не одноцветный: тёмная подложка, текст и иконка — разные цвета.
    let mut colors = std::collections::HashSet::new();
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            colors.insert(compositor.frame().get(x, y));
        }
    }
    assert!(colors.len() >= 4, "флайаут почти одноцветный: {}", colors.len());
}