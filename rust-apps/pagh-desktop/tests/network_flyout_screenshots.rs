//! Headless-рендер сетевого флайаута: три честных состояния и проверки кадра.
//!
//! Композитор рисует 1280×720 в память, поэтому вид флайаута проверяется без
//! QEMU/дисплея. PNG-файлы кладутся в `.cache/netfly/` (каталог git-ignored):
//! `flyout-online.png` — линк есть и данные реальные (`eth0 · 10.0.2.15/24` +
//! `↓N Б/с ↑N Б/с`), `flyout-offline.png` — `IFF_RUNNING` снят,
//! `flyout-nodata.png` — сэмпла ещё нет (`None`), и флайаут не выдумывает чисел.

use pagh_desktop::compositor::{Compositor, MouseButton};
use pagh_desktop::geometry::{Point, Rect};
use pagh_desktop::netinfo::{self, NetInfo};
use pagh_desktop::panel;
use pagh_desktop::png;

/// Сэмпл, который в QEMU отдаёт ядро: один интерфейс `eth0`, DHCP 10.0.2.15/24.
fn sample(running: bool) -> NetInfo {
    NetInfo {
        iface: netinfo::IFACE.to_string(),
        ip: Some("10.0.2.15".parse().expect("IPv4")),
        prefix: Some(24),
        mac: Some([0x52, 0x54, 0, 12, 34, 56]),
        up: true,
        running,
        rx_bytes: 4096,
        tx_bytes: 2048,
        rx_packets: 20,
        tx_packets: 10,
        rx_rate: 1536,
        tx_rate: 340,
        at: std::time::Instant::now(),
    }
}

fn open_network_flyout(compositor: &mut Compositor) -> Rect {
    let center = panel::network_rect(compositor.screen()).center();
    compositor.pointer_motion(center);
    compositor.pointer_button(MouseButton::Left, true);
    compositor.pointer_button(MouseButton::Left, false);
    let flyout = compositor
        .flyout()
        .expect("клик по иконке сети открывает флайаут");
    assert_eq!(flyout.kind, pagh_desktop::compositor::FlyoutKind::Network);
    flyout.rect
}

/// Пиксели цвета `color` внутри `area` (границы включительно).
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

/// Число различающихся пикселей между текущим кадром и снимком.
fn diff_with(compositor: &Compositor, snapshot: &pagh_desktop::canvas::Canvas) -> usize {
    compositor
        .frame()
        .pixels()
        .iter()
        .zip(snapshot.pixels().iter())
        .filter(|(left, right)| left != right)
        .count()
}

#[test]
fn network_flyout_online_offline_nodata_render() {
    let mut compositor = Compositor::new(1280, 720);
    compositor.set_network_info(Some(sample(true)));
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

    // Карточка подключения — подложка `chip_bg` (348×76 за вычетом иконки/текста).
    let card = Rect::new(rect.x + 12, rect.y + 64, rect.w - 24, 76);
    let card_pixels = count_color(&compositor, card, compositor.theme().chip_bg);
    assert!(
        card_pixels > 10_000,
        "карточка подключения не нарисована: {card_pixels}"
    );

    // Первая строка карточки — имя/адрес (`eth0 · 10.0.2.15/24`), вторая —
    // реальные скорости. Обе рисуются цветами `menu_text`/`menu_text_dim`
    // (иконка занимает левые 48 px, поэтому текстовые области — правее).
    let line_one = Rect::new(card.x + 48, card.y + 22, card.w - 48, 16);
    let line_two = Rect::new(card.x + 48, card.y + 46, card.w - 48, 16);
    let text_one = count_color(&compositor, line_one, compositor.theme().menu_text);
    let text_two = count_color(&compositor, line_two, compositor.theme().menu_text_dim);
    assert!(
        text_one > 60,
        "имя/адрес интерфейса не нарисованы: {text_one}"
    );
    assert!(text_two > 60, "строка скоростей не нарисована: {text_two}");

    // Иконка карточки — готовый глиф Plug Connected 24 px, тинтованный
    // panel_icon. Непрозрачный пиксель ищется по маске, а не хардкодится:
    // фазовая подгонка ассетов сдвигает глиф на полпикселя, и координата
    // «(19,11)» перестаёт быть непрозрачной. Точка берётся вне диагонали
    // «нет сети», иначе её перекрывает штрих offline-состояния.
    let icon_center = Point::new(card.x + 24, card.center().y);
    let icon_mask = pagh_desktop::icons::raster(pagh_desktop::icons::Icon::NetworkWired, 24)
        .expect("ассет иконки карточки");
    let mut opaque = None;
    for y in 0..icon_mask.height as i32 {
        for x in 0..icon_mask.width as i32 {
            let off_diagonal = (x + y - icon_mask.width as i32).abs() >= 4;
            if off_diagonal && icon_mask.alpha(x as u32, y as u32) == 255 {
                opaque = Some((x, y));
            }
        }
    }
    let (opaque_x, opaque_y) = opaque.expect("в глифе нет непрозрачного пикселя вне диагонали");
    let icon_pixel = Point::new(icon_center.x - 12 + opaque_x, icon_center.y - 12 + opaque_y);
    assert_eq!(
        compositor.frame().get(icon_pixel.x, icon_pixel.y),
        compositor.theme().panel_icon,
        "онлайн-иконка карточки не тинтована panel_icon"
    );

    // «Подключено» — акцентная строка статуса; акцент обязан присутствовать.
    let accent_pixels = count_color(&compositor, rect, compositor.theme().accent);
    assert!(
        accent_pixels > 20,
        "статус «Подключено» не нарисован акцентом: {accent_pixels}"
    );

    // Нет данных: `None` — «Нет данных» и карточка без чисел/скоростей.
    compositor.set_network_info(None);
    compositor.render_full();
    png::write_png(dir.join("flyout-nodata.png"), compositor.frame()).expect("PNG nodata");
    let nodata = compositor.frame().snapshot();
    let accent_nodata = count_color(&compositor, rect, compositor.theme().accent);
    assert_eq!(accent_nodata, 0, "без данных акцент не нужен");
    let nodata_text = count_color(&compositor, rect, compositor.theme().menu_text_dim);
    assert!(
        nodata_text > 100,
        "«Нет данных» не нарисовано: {nodata_text}"
    );
    assert!(
        diff_with(&compositor, &online) > 40,
        "состояния online/нет данных визуально не различаются"
    );

    // Линк снят (`IFF_RUNNING` = 0): тот же глиф в dim и подпись «линк не поднят».
    compositor.set_network_info(Some(sample(false)));
    compositor.render_full();
    png::write_png(dir.join("flyout-offline.png"), compositor.frame()).expect("PNG offline");
    assert_eq!(
        compositor.frame().get(icon_pixel.x, icon_pixel.y),
        compositor.theme().panel_icon_dim,
        "офлайн-иконка карточки не тинтована panel_icon_dim"
    );
    let offline_accent = count_color(&compositor, rect, compositor.theme().accent);
    assert_eq!(offline_accent, 0, "в отключённом состоянии акцент не нужен");
    assert!(
        diff_with(&compositor, &nodata) > 40,
        "состояния офлайн/нет данных визуально не различаются"
    );
    assert!(
        diff_with(&compositor, &online) > 40,
        "состояния online/offline визуально не различаются"
    );

    // Кадр не одноцветный: тёмная подложка, текст и иконка — разные цвета.
    let mut colors = std::collections::HashSet::new();
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            colors.insert(compositor.frame().get(x, y));
        }
    }
    assert!(
        colors.len() >= 4,
        "флайаут почти одноцветный: {}",
        colors.len()
    );
}
