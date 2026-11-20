//! Screenshot-тесты: сцены из [`pagh_ui::demo`] должны совпадать с эталонами.
//!
//! Рендер чисто целочисленный, шрифт встроен, поэтому хеш кадра стабилен.
//! Если изменение внешнего вида намеренное, перерисуйте демо
//! (`cargo run --bin pagh-ui-gallery -- --out screenshots`), посмотрите PNG
//! и обновите ожидаемый хеш здесь.

use pagh_ui::demo;
use pagh_ui::geom::Size;
use pagh_ui::testkit;
use pagh_ui::theme::Theme;

#[test]
fn gallery_scene_matches_golden() {
    let theme = Theme::pagh_dark();
    let canvas = demo::gallery(&theme);
    assert_eq!(canvas.size(), Size::new(980, 600));
    testkit::assert_golden("ui-gallery", &canvas, 0xab6d_61ed_6651_4d7d);
}

#[test]
fn menu_scene_matches_golden() {
    let theme = Theme::pagh_dark();
    let canvas = demo::menu(&theme);
    assert_eq!(canvas.size(), Size::new(1100, 700));
    testkit::assert_golden("ui-menu", &canvas, 0xfe98_a263_1c60_4c1c);
}

#[test]
fn palette_scene_matches_golden() {
    let theme = Theme::pagh_dark();
    let canvas = demo::palette(&theme);
    assert_eq!(canvas.size(), Size::new(900, 420));
    testkit::assert_golden("ui-palette", &canvas, 0x78b0_0f2e_b17d_fbf8);
}

#[test]
fn scenes_are_deterministic_across_runs() {
    let theme = Theme::pagh_dark();
    let first = testkit::hash(&demo::gallery(&theme));
    let second = testkit::hash(&demo::gallery(&theme));
    assert_eq!(first, second);
}

#[test]
fn every_scene_has_visible_content() {
    let theme = Theme::pagh_dark();
    for canvas in [
        demo::gallery(&theme),
        demo::menu(&theme),
        demo::palette(&theme),
    ] {
        // Сцена не пустая: заметная доля пикселей отличается от фона.
        let background = theme.palette.desktop;
        let mut non_background = 0usize;
        for y in 0..canvas.height() {
            for x in 0..canvas.width() {
                if canvas.pixel(pagh_ui::Point::new(x, y)) != background {
                    non_background += 1;
                }
            }
        }
        let total = (canvas.width() * canvas.height()) as usize;
        assert!(
            non_background * 2 > total,
            "сцена почти пустая: {non_background}/{total}"
        );
    }
}
