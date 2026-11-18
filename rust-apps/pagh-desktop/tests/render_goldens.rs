//! Пиксельные goldens рендера: дайджест кадра для каждого состояния сцены.
//!
//! Это гейт корректности для оптимизаций frame time (P1/P2/P3): изменение
//! codegen'а или горячего пути НЕ имеет права изменить хотя бы один пиксель.
//! Дайджест — FNV-1a по непрозрачным пикселям холста (`0x00RRGGBB`), поэтому
//! он падает на любое изменение цвета/геометрии/сглаживания, а не только на
//! «картинка отличается размером».
//!
//! Дайджесты сняты на дереве после `fix/icon-render-quality` (чёткая отрисовка
//! PNG-иконок в нативном размере): сравнение с до-иконочными значениями падало
//! бы на каждом законном улучшении рендера.
//!
//! Сцена собирается тем же `bench::build_desktop`, на котором меряется frame
//! time: goldens и замеры описывают один и тот же кадр.

use pagh_desktop::bench;
use pagh_desktop::compositor::{Compositor, MouseButton};
use pagh_desktop::demo::{self, DemoState};
use pagh_desktop::geometry::Point;

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
        idle_digest, 0x792e_fce4_c73a_b156,
        "idle-full: пиксели изменились (получено 0x{idle_digest:016x})"
    );

    // 2. Курсор в движении над обоями и окнами (cursor).
    let mut cursor = fresh();
    cursor.pointer_motion(Point::new(700, 520));
    cursor.render_full();
    let cursor_digest = digest(&cursor);
    assert_eq!(
        cursor_digest, 0x71bd_b466_b1a6_6aa8,
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
        drag_digest, 0xe54d_5c26_477b_cc48,
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
        files_digest, 0x6d6e_0c18_4571_1044,
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
