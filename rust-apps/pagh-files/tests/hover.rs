//! Регрессии наведения: готовая иконка Fluent остаётся на месте.
//!
//! Векторный глиф, который виджет `pagh_ui` рисует в ячейке/строке, заменяется
//! растром. Площадку под растром приложение закрывает фоном ячейки — раньше он
//! сэмплировался одним пикселем слева от иконки, поэтому прокрученная ячейка
//! (иконка под панелью инструментов) получала прямоугольник чужого цвета.
//! Тесты ниже держат два свойства: наведение не стирает иконку и закрывает её
//! ровно фоном самой ячейки.

use pagh_files::icons::{self, FluentIcon};
use pagh_files::screenshots::{demo_app, render_window};
use pagh_files::{FakeVfs, FilesApp};
use pagh_ui::canvas::Canvas;
use pagh_ui::color::Color;
use pagh_ui::event::Event;
use pagh_ui::geom::{Point, Rect, Size};
use pagh_ui::theme::Theme;

const GRID_ICON_W: i32 = 40;
const GRID_ICON_H: i32 = 32;
const GRID_ICON_TOP: i32 = 8;

/// Прямоугольник иконки в ячейке сетки (как в `FilesApp::draw_content`).
fn grid_icon(cell: Rect) -> Rect {
    Rect::new(
        cell.x + (cell.w - GRID_ICON_W) / 2,
        cell.y + GRID_ICON_TOP,
        GRID_ICON_W,
        GRID_ICON_H,
    )
}

/// Площадка, которую приложение закрывает фоном перед отрисовкой растра.
fn erased_area(icon: Rect) -> Rect {
    Rect::new(icon.x - 2, icon.y - 2, icon.w + 4, icon.h + 4)
}

/// Куда попадёт растр: он центрируется в `icon` и может быть на размер больше.
fn raster_rect(raster: &icons::Raster, icon: Rect) -> Rect {
    Rect::new(
        icon.x + (icon.w - raster.width as i32) / 2,
        icon.y + (icon.h - raster.height as i32) / 2,
        raster.width as i32,
        raster.height as i32,
    )
}

/// Полная точка-в-точку модель площадки иконки: растр, наложенный тинтом на
/// однородный фон ячейки, плюс сам фон там, где растр прозрачен.
///
/// Это и есть контракт отрисовки: любое расхождение — либо оставшийся
/// векторный глиф, либо «дырка»/прямоугольник чужого цвета под иконкой.
fn expected_icon_area(
    glyph: FluentIcon,
    size: u32,
    icon: Rect,
    tint: Color,
    backdrop: Color,
) -> Vec<(Point, Color)> {
    let raster = icons::raster(glyph, size).expect("растр иконки декодируется");
    let dst = raster_rect(&raster, icon);
    let area = erased_area(icon);
    let mut expected = Vec::with_capacity((area.w * area.h) as usize);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let point = Point::new(x, y);
            let alpha = if dst.contains(point) {
                raster.alpha((x - dst.x) as u32, (y - dst.y) as u32)
            } else {
                0
            };
            expected.push((point, tint.with_alpha(alpha).blend_over(backdrop)));
        }
    }
    expected
}

/// Сетка: наведение только подсвечивает ячейку — иконка цела, «дырки» нет.
#[test]
fn hover_keeps_grid_icon_and_only_adds_tint() {
    let mut app = demo_app();
    let theme = Theme::pagh_dark();
    let palette = theme.palette;
    let before = render_window(&app);

    let cell = app.item_rect(0).expect("ячейка 0");
    let icon = grid_icon(cell);
    // Фон ячейки: точка внутри ячейки, но вне иконки и вне площадки стирания.
    let backdrop_point = Point::new(cell.x + 4, icon.center().y);
    let backdrop_before = before.pixel(backdrop_point);

    // До наведения площадка уже соответствует модели (растр вместо вектора).
    for (point, expected) in expected_icon_area(
        FluentIcon::Folder,
        32,
        icon,
        palette.accent,
        backdrop_before,
    ) {
        assert_eq!(
            before.pixel(point),
            expected,
            "исходный кадр не совпал с моделью в {point:?}"
        );
    }

    let commands = app.handle_event(&Event::MouseMove { pos: cell.center() });
    assert!(commands.is_empty(), "наведение не должно порождать команд");
    let after = render_window(&app);

    let backdrop_after = after.pixel(backdrop_point);
    assert_ne!(backdrop_before, backdrop_after, "подсветка не появилась");

    // 1. Иконка цела: площадка — ровно растр на новом фоне ячейки.
    for (point, expected) in
        expected_icon_area(FluentIcon::Folder, 32, icon, palette.accent, backdrop_after)
    {
        assert_eq!(
            after.pixel(point),
            expected,
            "наведение испортило иконку в {point:?}"
        );
    }

    // 2. Изменения не выходят за ячейку: наведение не задевает соседей.
    let bbox = diff_bbox(&before, &after).expect("кадр не изменился");
    assert_eq!(
        bbox.intersect(cell),
        Some(bbox),
        "наведение задело пиксели вне ячейки: bbox {bbox:?}, ячейка {cell:?}"
    );
}

/// Сайдбар: наведение на пункт не стирает его иконку.
#[test]
fn hover_keeps_sidebar_icon() {
    let mut app = demo_app();
    let theme = Theme::pagh_dark();
    let palette = theme.palette;
    let before = render_window(&app);

    // «Документы» (id 3) не выделены (выделена «Домашняя папка»), поэтому
    // показывают подсветку наведения.
    let item = app.layout().sidebar_list.rect_of(3).expect("пункт 3");
    let icon = Rect::new(
        item.x + 8,
        item.y + (item.h - theme.metrics.icon_size) / 2,
        theme.metrics.icon_size,
        theme.metrics.icon_size,
    );
    let backdrop_point = Point::new(item.x + 4, item.center().y);
    let backdrop_before = before.pixel(backdrop_point);

    app.handle_event(&Event::MouseMove {
        pos: Point::new(item.center().x, item.center().y),
    });
    let after = render_window(&app);

    let backdrop_after = after.pixel(backdrop_point);
    assert_ne!(
        backdrop_before, backdrop_after,
        "подсветка пункта не появилась"
    );
    for (point, expected) in expected_icon_area(
        FluentIcon::FolderOpen,
        16,
        icon,
        palette.sidebar_text,
        backdrop_after,
    ) {
        assert_eq!(
            after.pixel(point),
            expected,
            "наведение испортило иконку сайдбара в {point:?}"
        );
    }

    // Выделенный пункт («Домашняя папка») не тронут.
    let home = app.layout().sidebar_list.rect_of(1).expect("пункт 1");
    for y in home.y..home.bottom() {
        for x in home.x..home.right() {
            let point = Point::new(x, y);
            assert_eq!(
                before.pixel(point),
                after.pixel(point),
                "наведение задело выделенный пункт в {point:?}"
            );
        }
    }
}

/// Прокрученная сетка: ячейка уходит под панель инструментов. Площадка под
/// иконкой должна закрываться фоном ячейки, а не тем пикселем, что оказался
/// слева (раньше это был цвет панели инструментов).
#[test]
fn scrolled_row_icon_backdrop_comes_from_the_cell() {
    // Подобрано так, что после прокрутки колесом ячейка #10 частично видна
    // сверху, а точка старого сэмпла (erase.x - 4, center.y) лежит уже в
    // панели инструментов (y < content.y).
    let mut app = FilesApp::new(Box::new(FakeVfs::concept()), Size::new(900, 445));
    let area = app.layout().grid_area;
    for _ in 0..12 {
        let scroll = app.scroll_offset();
        app.handle_event(&Event::Wheel {
            pos: area.center(),
            delta: -1,
        });
        if app.scroll_offset() == scroll {
            break;
        }
    }
    assert!(app.scroll_offset() > 0, "сетка не прокрутилась");

    let cell = app.item_rect(10).expect("ячейка 10");
    let icon = grid_icon(cell);
    let erased = erased_area(icon);
    let visible = erased.intersect(area).expect("видимая часть площадки");
    assert!(visible.h > 0, "ячейка не видна");
    let layout = app.layout();
    let old_sample = Point::new(erased.x - 4, erased.center().y);
    assert!(
        old_sample.y < layout.content.y,
        "состояние подобрано неверно: старый сэмпл внутри контента ({old_sample:?})"
    );

    // Наводим на видимую часть ячейки.
    app.handle_event(&Event::MouseMove {
        pos: Point::new(cell.center().x, visible.center().y),
    });
    let canvas = render_window(&app);
    let palette = Theme::pagh_dark().palette;
    // Фон ячейки — точка внутри видимой части, вне иконки и её растра.
    let backdrop = canvas.pixel(Point::new(cell.x + 10, visible.center().y));

    for (point, expected) in
        expected_icon_area(FluentIcon::Folder, 32, icon, palette.accent, backdrop)
    {
        if !visible.contains(point) {
            continue;
        }
        assert_eq!(
            canvas.pixel(point),
            expected,
            "площадка под иконкой закрыта чужим цветом в {point:?}"
        );
    }
}

/// Наибольший прямоугольник, покрывающий все различающиеся пиксели.
fn diff_bbox(before: &Canvas, after: &Canvas) -> Option<Rect> {
    let mut bbox: Option<Rect> = None;
    for y in 0..before.height() {
        for x in 0..before.width() {
            let point = Point::new(x, y);
            if before.pixel(point) == after.pixel(point) {
                continue;
            }
            bbox = Some(match bbox {
                None => Rect::new(x, y, 1, 1),
                Some(rect) => rect.union(Rect::new(x, y, 1, 1)),
            });
        }
    }
    bbox
}
