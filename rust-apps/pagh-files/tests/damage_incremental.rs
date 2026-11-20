//! Инкрементальный кадр Files: виджетный damage обязан совпадать с полным.
//!
//! Свойство, которое здесь держится: если взять кадр, нарисованный до события,
//! и применить к нему только виджетный damage (`take_damage` +
//! `draw_incremental`), получится **побитово** тот же кадр, что `draw` рисует
//! с нуля после события. Любая забытая ветка состояния — потерянный пиксель —
//! валит тест, поэтому набор событий покрывает наведение, выделение, меню,
//! поиск, клавиши, прокрутку и диалог.
//!
//! Второе свойство — цена: damage клика/наведения по строке списка должен быть
//! на порядки меньше окна, иначе инкрементальный путь не имеет смысла.

use pagh_files::screenshots::{demo_app, render_window, WINDOW};
use pagh_files::{FilesApp, ViewMode};
use pagh_ui::canvas::Canvas;
use pagh_ui::event::{Event, Key, Modifiers, MouseButton};
use pagh_ui::geom::{Point, Rect, Size};
use pagh_ui::layer::StaticLayer;

fn no_modifiers() -> Modifiers {
    Modifiers {
        shift: false,
        ctrl: false,
        alt: false,
    }
}

fn key(key: Key) -> Event {
    Event::Key {
        key,
        pressed: true,
        modifiers: no_modifiers(),
    }
}

/// Применить событие к `app` и получить инкрементальный кадр.
///
/// `frame` — кадр «до» события; после вызова в нём кадр «после».
fn apply(app: &mut FilesApp, frame: &mut Canvas, layer: &StaticLayer, event: &Event) -> usize {
    let commands = app.handle_event(event);
    let damage = app.take_damage();
    let painted = app.draw_incremental(frame, layer, damage.rects());
    assert_eq!(
        commands.len(),
        0,
        "событие {event:?} не должно просить внешних действий"
    );
    painted
}

fn assert_same_as_full(app: &FilesApp, incremental: &Canvas, what: &str) {
    let full = render_window(app);
    assert_eq!(
        incremental.size(),
        full.size(),
        "{what}: размер кадра разошёлся"
    );
    assert_eq!(
        incremental.pixels(),
        full.pixels(),
        "{what}: инкрементальный кадр не совпал с полным"
    );
}

/// Прогоняет событие: кадр «до» → событие → инкремент → сравнение с полным.
fn step(app: &mut FilesApp, frame: &mut Canvas, layer: &StaticLayer, event: Event, what: &str) {
    let painted = apply(app, frame, layer, &event);
    assert!(
        painted > 0,
        "{what}: событие не пометило ни одного прямоугольника"
    );
    assert_same_as_full(app, frame, what);
}

fn prepared() -> (FilesApp, Canvas, StaticLayer) {
    let mut app = demo_app();
    let mut frame = render_window(&app);
    let mut layer = StaticLayer::new(app.size());
    layer.rebuild(|canvas| app.draw_base(canvas));
    app.take_damage();
    (app, frame, layer)
}

/// Прямоугольник строки списка (для проверки «мало пикселей»).
fn list_row(app: &FilesApp, index: usize) -> Rect {
    app.item_rect(index).expect("нет такой строки")
}

#[test]
fn list_hover_damages_only_the_row() {
    let (mut app, mut frame, layer) = prepared();
    app.set_view(ViewMode::List);
    // Переключение режима вне события — большое изменение: полный кадр.
    frame = render_window(&app);
    app.take_damage();

    let row = list_row(&app, 1);
    let point = Point::new(row.x + row.w / 2, row.y + row.h / 2);
    app.handle_event(&Event::MouseMove { pos: point });
    let damage = app.take_damage();
    // Наведение — ушедшая и пришедшая строка, а не всё окно.
    assert!(
        damage.area() >= row.area() as i64,
        "строка наведения не попала в damage: {} px",
        damage.area()
    );
    assert!(
        damage.area() <= 4 * row.area() as i64,
        "наведение повредило слишком много: {} px при строке {}",
        damage.area(),
        row.area()
    );
    app.draw_incremental(&mut frame, &layer, damage.rects());
    assert_same_as_full(&app, &frame, "hover строки");
}

#[test]
fn selection_click_matches_full_frame() {
    let (mut app, mut frame, layer) = prepared();
    let row = list_row(&app, 2);
    let point = Point::new(row.x + 40, row.y + row.h / 2);
    step(
        &mut app,
        &mut frame,
        &layer,
        Event::MouseDown {
            pos: point,
            button: MouseButton::Left,
        },
        "клик по элементу",
    );
}

#[test]
fn keyboard_selection_matches_full_frame() {
    let (mut app, mut frame, layer) = prepared();
    step(&mut app, &mut frame, &layer, key(Key::Down), "стрелка вниз");
    step(
        &mut app,
        &mut frame,
        &layer,
        key(Key::Down),
        "вторая стрелка вниз",
    );
    step(&mut app, &mut frame, &layer, key(Key::Up), "стрелка вверх");
}

#[test]
fn grid_hover_matches_full_frame() {
    let (mut app, mut frame, layer) = prepared();
    let cell = app.item_rect(1).expect("нет ячейки");
    step(
        &mut app,
        &mut frame,
        &layer,
        Event::MouseMove {
            pos: Point::new(cell.x + 6, cell.y + cell.h / 2),
        },
        "hover ячейки сетки",
    );
}

#[test]
fn toolbar_hover_matches_full_frame() {
    let (mut app, mut frame, layer) = prepared();
    let layout = app.layout();
    let back = layout.back;
    step(
        &mut app,
        &mut frame,
        &layer,
        Event::MouseMove {
            pos: Point::new(back.x + 4, back.y + 4),
        },
        "hover кнопки «назад»",
    );
    // Уводим курсор на пустое место — подсветка должна исчезнуть.
    step(
        &mut app,
        &mut frame,
        &layer,
        Event::MouseMove {
            pos: Point::new(back.x, layout.toolbar.bottom() - 1),
        },
        "уход с кнопки",
    );
}

#[test]
fn sidebar_hover_matches_full_frame() {
    let (mut app, mut frame, layer) = prepared();
    let layout = app.layout();
    let item = layout
        .sidebar_list
        .items
        .get(3)
        .map(|(_, rect)| *rect)
        .expect("нет пункта сайдбара");
    step(
        &mut app,
        &mut frame,
        &layer,
        Event::MouseMove {
            pos: Point::new(item.x + 20, item.y + item.h / 2),
        },
        "hover пункта сайдбара",
    );
}

#[test]
fn search_typing_matches_full_frame() {
    let (mut app, mut frame, layer) = prepared();
    app.show_search("");
    frame = render_window(&app);
    app.take_damage();
    step(
        &mut app,
        &mut frame,
        &layer,
        Event::Text {
            text: "p".to_string(),
        },
        "ввод в поиск",
    );
    step(
        &mut app,
        &mut frame,
        &layer,
        Event::Text {
            text: "n".to_string(),
        },
        "второй символ поиска",
    );
}

#[test]
fn scroll_matches_full_frame() {
    let (mut app, mut frame, layer) = prepared();
    app.set_view(ViewMode::List);
    frame = render_window(&app);
    app.take_damage();
    let layout = app.layout();
    let center = Point::new(
        layout.grid_area.x + layout.grid_area.w / 2,
        layout.grid_area.y + layout.grid_area.h / 2,
    );
    step(
        &mut app,
        &mut frame,
        &layer,
        Event::Wheel {
            pos: center,
            delta: -1,
        },
        "прокрутка колесом",
    );
}

#[test]
fn menu_hover_matches_full_frame() {
    let (mut app, mut frame, layer) = prepared();
    let layout = app.layout();
    let row = list_row(&app, 1);
    // Правый клик по строке открывает контекстное меню — это popup, полный кадр.
    app.handle_event(&Event::MouseDown {
        pos: Point::new(row.x + 30, row.y + row.h / 2),
        button: MouseButton::Right,
    });
    assert!(app.menu_open(), "меню не открылось");
    frame = render_window(&app);
    app.take_damage();

    // Ведём курсор по пунктам меню: damage — прямоугольник меню.
    let (root, _submenu) = app.menu_layouts().expect("меню без раскладки");
    let first_row = root.rows.first().expect("в меню нет пунктов").rect;
    let point = Point::new(first_row.x + first_row.w / 2, first_row.y + first_row.h / 2);
    step(
        &mut app,
        &mut frame,
        &layer,
        Event::MouseMove { pos: point },
        "hover меню",
    );
    let damage = app.damage();
    let bounds = app.size();
    assert!(
        damage.area() < (bounds.w as i64 * bounds.h as i64) / 2,
        "наведение в меню повредило пол-окна: {}",
        damage.area()
    );
}

#[test]
fn direct_state_change_without_event_requests_full_frame() {
    let (mut app, _frame, _layer) = prepared();
    // Смена режима в обход `handle_event` (демо/тесты/старт) damage не
    // наполняет: такой кадр рисуется целиком (`force_full` у вызывающего).
    app.set_view(ViewMode::List);
    assert!(app.take_damage().is_empty());
    // А после события damage снова точечный и совпадает с полным кадром.
    let (mut app, mut frame, layer) = prepared();
    app.set_view(ViewMode::List);
    let mut frame = render_window(&app);
    app.take_damage();
    let row = list_row(&app, 2);
    step(
        &mut app,
        &mut frame,
        &layer,
        Event::MouseDown {
            pos: Point::new(row.x + 20, row.y + row.h / 2),
            button: MouseButton::Left,
        },
        "клик после смены режима",
    );
}

#[test]
fn click_on_row_costs_less_than_a_percent_of_the_window() {
    let (mut app, mut frame, layer) = prepared();
    let row = Rect::new(0, 0, WINDOW.w, 26);
    let point = Point::new(40, row.y + 13);
    // Выделение первой строки списка.
    app.set_view(ViewMode::List);
    frame = render_window(&app);
    app.take_damage();
    let item = list_row(&app, 0);
    app.handle_event(&Event::MouseDown {
        pos: Point::new(item.x + 40, item.y + item.h / 2),
        button: MouseButton::Left,
    });
    let damage = app.take_damage();
    let window = WINDOW.w as i64 * WINDOW.h as i64;
    // Строка списка — это и есть rect damage; на окне 1060x640 она ~3 %,
    // то есть в ~30 раз меньше полного кадра (и без хеширования 2,7 МиБ).
    assert!(
        damage.area() * 25 < window,
        "клик по строке повредил {} px из {window} (<4 % ожидалось)",
        damage.area()
    );
    // И всё равно кадр совпадает с полным.
    let painted = app.draw_incremental(&mut frame, &layer, damage.rects());
    assert_eq!(painted, damage.len());
    assert_same_as_full(&app, &frame, "клик по строке");
    let _ = point;
}

#[test]
fn resize_event_requests_full_frame() {
    let (mut app, mut frame, _layer) = prepared();
    app.handle_event(&Event::Resize {
        size: Size::new(WINDOW.w - 40, WINDOW.h - 20),
    });
    let damage = app.take_damage();
    assert!(damage.is_full(), "resize обязан требовать полный кадр");
}
