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
use pagh_desktop::geometry::{Point, Rect};
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

    // Готовый глиф Plug Connected 20×20: альфа занимает 2..=17, то есть ±8
    // от центра. Wi-Fi-дуги старого образца поднимались на ~14 px выше центра
    // и расходились до ±15 — такие пиксели здесь были бы регрессией.
    let (widest, lit) = widest_lit(&frame);
    assert!(lit > 40, "иконка сети не нарисована, пикселей {lit}");
    assert!(
        widest <= 11,
        "иконка шире проводной формы: вылет {widest} px (Wi-Fi-дуги давали >= 15)"
    );
    let (min_y, max_y) = lit_rows(&frame, net, center);
    assert!(
        min_y >= center.y - 10 && max_y <= center.y + 10,
        "иконка выше проводной формы: y [{}, {}] от центра (дуги давали -14)",
        min_y - center.y,
        max_y - center.y
    );

    // Глиф тинтован цветом панели: у непрозрачного пикселя вне диагонали
    // RGB заменён тинтом, альфа сохранена — сравниваем с точным blend.
    let (source_x, source_y, alpha) = opaque_off_diagonal();
    let sample = Point::new(center.x - 10 + source_x, center.y - 10 + source_y);
    let expected = |frame: &Canvas, tint: pagh_desktop::color::Color| {
        let background = frame.get(500, sample.y);
        color::blend(
            background,
            color::rgba(
                color::red(tint),
                color::green(tint),
                color::blue(tint),
                alpha,
            ),
            255,
        )
    };
    assert_eq!(
        frame.get(sample.x, sample.y),
        expected(&frame, theme.panel_icon),
        "онлайн-иконка сети не тинтована panel_icon"
    );

    // Иконка сети строго монохромная (старый образец ставил акцентную точку).
    let mut accent = 0;
    for y in net.y..net.bottom() {
        for x in net.x..net.right() {
            if frame.get(x, y) == theme.accent {
                accent += 1;
            }
        }
    }
    assert_eq!(accent, 0, "в иконке сети не должно быть акцентных пикселей");

    // Отключённая сеть: тот же глиф в panel_icon_dim плюс диагональ.
    compositor.set_network_online(false);
    compositor.render_full();
    png::write_png(dir.join("panel-network-offline.png"), compositor.frame()).expect("PNG offline");
    let frame = compositor.frame().snapshot();
    assert_eq!(
        frame.get(sample.x, sample.y),
        expected(&frame, theme.panel_icon_dim),
        "офлайн-иконка сети не тинтована panel_icon_dim"
    );
    let struck = (net.y..net.bottom())
        .flat_map(|y| (net.x..net.right()).map(move |x| (x, y)))
        .filter(|(x, y)| frame.get(*x, *y) == theme.button_close)
        .count();
    assert!(struck > 0, "нет диагонали «нет сети»");
}

/// Готовые иконки приложений на панели: каждый блок ячейки закрашен, Paint —
/// акцентом, а не прежней многоцветной палитрой из примитивов.
#[test]
fn panel_app_icons_are_ready_made() {
    use pagh_desktop::panel::{icon_rect, PanelIcon};

    let mut compositor = Compositor::new(1280, 720);
    compositor.render_full();
    let frame = compositor.frame().snapshot();
    let theme = *compositor.theme();
    let screen = compositor.screen();
    for (index, icon) in PanelIcon::ALL.iter().enumerate() {
        let cell = icon_rect(screen, index);
        let lit = (cell.y..cell.bottom())
            .flat_map(|y| (cell.x..cell.right()).map(move |x| (x, y)))
            .filter(|(x, y)| frame.get(*x, *y) != theme.panel_bg)
            .count();
        assert!(
            lit > 80,
            "{icon:?}: готовая иконка не нарисована ({lit} пикселей)"
        );
    }
    let paint = icon_rect(screen, PanelIcon::Paint.index());
    let accent = (paint.y..paint.bottom())
        .flat_map(|y| (paint.x..paint.right()).map(move |x| (x, y)))
        .filter(|(x, y)| frame.get(*x, *y) == theme.accent)
        .count();
    assert!(accent > 0, "иконка Paint должна тинтоваться акцентом");
}

/// Вертикальные границы зажжённых пикселей блока сети.
fn lit_rows(frame: &Canvas, net: Rect, center: Point) -> (i32, i32) {
    let mut min_y = i32::MAX;
    let mut max_y = i32::MIN;
    for y in net.y..net.bottom() {
        let bg = frame.get(500, y);
        for x in net.x..net.right() {
            if frame.get(x, y) != bg {
                min_y = min_y.min(y - center.y);
                max_y = max_y.max(y - center.y);
            }
        }
    }
    (center.y + min_y, center.y + max_y)
}

/// Пиксель глифа сети с максимальной альфой вне диагонали «нет сети».
fn opaque_off_diagonal() -> (i32, i32, u8) {
    let raster = pagh_desktop::icons::raster(pagh_desktop::icons::Icon::NetworkWired, 20)
        .expect("ассет иконки сети");
    let mut best = (0i32, 0i32, 0u8);
    for y in 0..raster.height as i32 {
        for x in 0..raster.width as i32 {
            let alpha = raster.alpha(x as u32, y as u32);
            let diagonal = (x + y - raster.width as i32).abs();
            if alpha > best.2 && diagonal >= 4 {
                best = (x, y, alpha);
            }
        }
    }
    assert!(best.2 > 200, "не нашлось непрозрачного пикселя глифа");
    best
}
