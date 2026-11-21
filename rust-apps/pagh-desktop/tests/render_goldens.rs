//! Пиксельные goldens рендера: дайджест кадра для каждого состояния сцены.
//!
//! Это гейт корректности для оптимизаций frame time (P1/P2/P3): изменение
//! codegen'а или горячего пути НЕ имеет права изменить хотя бы один пиксель.
//! Дайджест — FNV-1a по непрозрачным пикселям холста (`0x00RRGGBB`), поэтому
//! он падает на любое изменение цвета/геометрии/сглаживания, а не только на
//! «картинка отличается размером».
//!
//! Сцена собирается тем же `bench::build_desktop`, на котором меряется frame
//! time: goldens и замеры описывают один и тот же кадр.

use pagh_desktop::bench;
use pagh_desktop::compositor::{Compositor, MouseButton};
use pagh_desktop::demo::{self, DemoState};
use pagh_desktop::geometry::Point;
use pagh_desktop::surface::PixelFormat;

/// FNV-1a 64 по словам пикселей (порядок — построчный).
fn digest(compositor: &Compositor) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for pixel in compositor.frame().pixels() {
        for byte in pixel.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    hash
}

fn fresh() -> Compositor {
    bench::build_desktop(1280, 800)
}

#[test]
fn rendered_pixels_match_golden_digests() {
    // 1. Статичный рабочий стол, полный кадр (idle-full).
    let mut idle = fresh();
    idle.render_full();
    let idle_digest = digest(&idle);
    assert_eq!(
        idle_digest, 0xcff4_39a9_e1a3_8432,
        "idle-full: пиксели изменились (получено 0x{idle_digest:016x})"
    );

    // 2. Курсор в движении над обоями и окнами (cursor).
    let mut cursor = fresh();
    cursor.pointer_motion(Point::new(700, 520));
    cursor.render_full();
    let cursor_digest = digest(&cursor);
    assert_eq!(
        cursor_digest, 0x3f6a_bbf9_4f02_f2c8,
        "cursor: пиксели изменились (получено 0x{cursor_digest:016x})"
    );

    // 3. Окно тянется за заголовок (drag): сдвинутая геометрия + фокус.
    let mut drag = fresh();
    drag.pointer_motion(Point::new(200, 84));
    drag.pointer_button(MouseButton::Left, true);
    drag.pointer_motion(Point::new(260, 140));
    drag.render_full();
    let drag_digest = digest(&drag);
    assert_eq!(
        drag_digest, 0x78a3_a4ab_fb3d_b8e8,
        "drag: пиксели изменились (получено 0x{drag_digest:016x})"
    );

    // 4. Прокрученное содержимое Files (files-scroll): новый клиентский кадр.
    let mut files = fresh();
    files.pointer_motion(Point::new(900, 430));
    let state = DemoState {
        focused: false,
        scroll: 120,
        ..DemoState::default()
    };
    files
        .commit_surface(2, 2, demo::files_surface(560, 340, &state))
        .expect("кадр Files");
    files.render_full();
    let files_digest = digest(&files);
    assert_eq!(
        files_digest, 0x4db8_1491_2ed4_31ac,
        "files-scroll: пиксели изменились (получено 0x{files_digest:016x})"
    );

    // Состояния обязаны отличаться: иначе goldens вырожденные и ничего не ловят.
    let digests = [idle_digest, cursor_digest, drag_digest, files_digest];
    for (index, left) in digests.iter().enumerate() {
        for right in &digests[index + 1..] {
            assert_ne!(left, right, "goldens состояний совпали");
        }
    }
}

#[test]
fn digest_is_deterministic_across_two_frames() {
    // Один и тот же кадр, отрисованный дважды, обязан дать один дайджест:
    // иначе goldens нестабильны и гейт бесполезен.
    let mut compositor = fresh();
    compositor.render_full();
    let first = digest(&compositor);
    compositor.render_full();
    assert_eq!(first, digest(&compositor));
}

/// Дополнительные состояния рендера вокруг границ тени: наведение на кнопку
/// заголовка, окно у края экрана (внеэкранная коробка тени), перекрытие
/// окон (тень одного ложится на содержимое другого), минимальный размер окна,
/// смена фокуса и кадр, перерисованный только по damage. Дайджесты сняты с
/// неоптимизированного рендера до переноса тени на кольцо: совпадение
/// доказывает, что оптимизация не изменила ни одного пикселя и в этих
/// состояниях, а не только в четырёх основных сценариях.
#[test]
fn rendered_pixels_match_golden_digests_on_borders() {
    let mut hover = fresh();
    let frame = hover.window(1).unwrap().frame;
    let close = pagh_desktop::window::button_rect_for(frame, 0).center();
    hover.pointer_motion(close);
    hover.render_full();
    let hover_digest = digest(&hover);
    assert_eq!(
        hover_digest, 0x4866_9870_62e6_150e,
        "hover-close: пиксели изменились (получено 0x{hover_digest:016x})"
    );

    let mut edge = fresh();
    edge.move_window(1, Point::new(0, 0));
    edge.render_full();
    let edge_digest = digest(&edge);
    assert_eq!(
        edge_digest, 0xbb6f_48b1_0f86_758d,
        "edge (коробка тени за экраном): пиксели изменились (получено 0x{edge_digest:016x})"
    );

    let mut overlap = fresh();
    overlap.move_window(2, Point::new(600, 400));
    overlap.render_full();
    let overlap_digest = digest(&overlap);
    assert_eq!(
        overlap_digest, 0xd180_6ca0_f48f_8168,
        "overlap (тень на чужом окне): пиксели изменились (получено 0x{overlap_digest:016x})"
    );

    let mut tiny = fresh();
    tiny.add_window(3, 3, "Paint", 64, 48, PixelFormat::Xrgb8888)
        .unwrap();
    tiny.move_window(3, Point::new(1180, 80));
    tiny.render_full();
    let tiny_digest = digest(&tiny);
    assert_eq!(
        tiny_digest, 0xa53a_3134_4044_ecae,
        "tiny (минимальное окно): пиксели изменились (получено 0x{tiny_digest:016x})"
    );

    let mut focused = fresh();
    focused.focus_and_raise(2);
    focused.render_full();
    let focused_digest = digest(&focused);
    assert_eq!(
        focused_digest, 0x7c6c_67f6_2df6_8b0a,
        "focus-files: пиксели изменились (получено 0x{focused_digest:016x})"
    );

    let mut damaged = fresh();
    damaged.render_full();
    damaged.pointer_motion(Point::new(704, 300));
    damaged.render();
    let damaged_digest = digest(&damaged);
    assert_eq!(
        damaged_digest, 0x96c2_9e0e_d6a9_0992,
        "damaged (кадр по damage через кольцо): пиксели изменились (получено 0x{damaged_digest:016x})"
    );

    let digests = [
        hover_digest,
        edge_digest,
        overlap_digest,
        tiny_digest,
        focused_digest,
        damaged_digest,
    ];
    for (index, left) in digests.iter().enumerate() {
        for right in &digests[index + 1..] {
            assert_ne!(left, right, "goldens дополнительных состояний совпали");
        }
    }
}
