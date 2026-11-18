//! Headless-рендер панельной иконки сети: PNG и пиксельные проверки формы.
//!
//! Композитор рисует кадр в память, поэтому вид панели проверяется без
//! QEMU/дисплея. PNG кладутся в `.cache/netfly/` (каталог git-ignored):
//! `panel-network.png` — панель целиком, `panel-network-zoom.png` — блок сети
//! с 8-кратным увеличением, `panel-network-offline.png` — сеть выключена.
//!
//! Контракт, который здесь зафиксирован: иконка сети — проводной порт
//! (корпус с контактами и кабелем), а не Wi-Fi-дуги. Wi-Fi у машины нет:
//! линк приходит с e1000 (провод) или RNDIS по USB (тетеринг телефона).

use pagh_desktop::color;
use pagh_desktop::compositor::Compositor;
use pagh_desktop::geometry::Rect;
use pagh_desktop::panel;
use pagh_desktop::png;
use pagh_desktop::Canvas;

const ZOOM: i32 = 8;

/// Увеличенная копия области кадра (ближайший сосед) — для глазной проверки.
fn zoom(src: &Canvas, area: Rect, factor: i32) -> Canvas {
    let mut out = Canvas::new(
        (area.w * factor) as u32,
        (area.h * factor) as u32,
        color::rgb(0, 0, 0),
    );
    let stride = out.width() as usize;
    for dy in 0..area.h {
        for dx in 0..area.w {
            let pixel = src.get(area.x + dx, area.y + dy);
            for oy in 0..factor {
                for ox in 0..factor {
                    let index = (dy * factor + oy) as usize * stride + (dx * factor + ox) as usize;
                    out.pixels_mut()[index] = pixel;
                }
            }
        }
    }
    out
}

/// Самый дальний по X «зажжённый» пиксель в блоке сети.
///
/// Фон панели — вертикальный градиент, поэтому каждый пиксель сравнивается с
/// цветом своей строки вне иконки (x = 500 — пустая область панели).
fn widest_lit(frame: &Canvas) -> (i32, usize) {
    let screen = panel::panel_rect(Rect::new(0, 0, 1280, 720));
    let net = panel::network_rect(Rect::new(0, 0, 1280, 720));
    let center = net.center();
    let mut widest = 0;
    let mut lit = 0;
    for y in net.y..net.bottom() {
        let bg = frame.get(500, y);
        for x in net.x..net.right() {
            if frame.get(x, y) != bg {
                lit += 1;
                widest = widest.max((x - center.x).abs());
            }
        }
    }
    assert!(
        net.y >= screen.y && net.bottom() <= screen.bottom(),
        "блок сети должен лежать внутри панели"
    );
    (widest, lit)
}

#[test]
fn panel_network_icon_is_wired_and_renders() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.cache/netfly");
    std::fs::create_dir_all(&dir).expect("каталог скриншотов");
    let net = panel::network_rect(Rect::new(0, 0, 1280, 720));

    let mut compositor = Compositor::new(1280, 720);
    compositor.render_full();
    png::write_png(dir.join("panel-network.png"), compositor.frame()).expect("PNG панели");
    png::write_png(
        dir.join("panel-network-zoom.png"),
        &zoom(compositor.frame(), net, ZOOM),
    )
    .expect("PNG увеличения");
    // Правый край панели: новая иконка рядом с соседями и часами.
    let tail = Rect::new(net.x - 40, net.y - 4, 240, 44);
    png::write_png(
        dir.join("panel-right-zoom.png"),
        &zoom(compositor.frame(), tail, 4),
    )
    .expect("PNG правого края панели");

    let theme = *compositor.theme();
    let frame = compositor.frame().snapshot();
    let center = net.center();

    // Проводная иконка укладывается в 20×19 вокруг центра: Wi-Fi-дуги старого
    // образца давали вылет >= 15 px (радиус 16 от нижней точки блока).
    let (widest, lit) = widest_lit(&frame);
    assert!(lit > 60, "иконка сети не нарисована, пикселей {lit}");
    assert!(
        widest <= 11,
        "иконка шире проводной формы: вылет {widest} px (Wi-Fi-дуги давали >= 15)"
    );

    // Корпус порта: рамка по краям и вырезанная внутренность.
    assert_eq!(frame.get(center.x - 10, center.y), theme.panel_icon);
    assert_eq!(frame.get(center.x + 9, center.y), theme.panel_icon);
    assert_eq!(frame.get(center.x - 4, center.y - 3), theme.panel_bg);

    // Четыре контакта разъёма в один ряд.
    let mut contacts = 0;
    for dx in [-6, -2, 2, 6] {
        if frame.get(center.x + dx, center.y - 4) == theme.panel_icon {
            contacts += 1;
        }
    }
    assert_eq!(contacts, 4, "контакты порта не нарисованы: {contacts}/4");

    // Кабель уходит вниз от корпуса.
    assert_eq!(frame.get(center.x, center.y + 7), theme.panel_icon);

    // Старая иконка ставила акцентную точку; проводная — чисто монохромная.
    let mut accent = 0;
    for y in net.y..net.bottom() {
        for x in net.x..net.right() {
            if frame.get(x, y) == theme.accent {
                accent += 1;
            }
        }
    }
    assert_eq!(accent, 0, "в иконке сети не должно быть акцентных пикселей");

    // Отключённая сеть: приглушённый контур и диагональ.
    compositor.set_network_online(false);
    compositor.render_full();
    png::write_png(dir.join("panel-network-offline.png"), compositor.frame()).expect("PNG offline");
    let frame = compositor.frame().snapshot();
    assert_eq!(frame.get(center.x - 10, center.y), theme.panel_icon_dim);
    let struck = (net.y..net.bottom())
        .flat_map(|y| (net.x..net.right()).map(move |x| (x, y)))
        .filter(|(x, y)| frame.get(*x, *y) == theme.button_close)
        .count();
    assert!(struck > 0, "нет диагонали «нет сети»");
}
