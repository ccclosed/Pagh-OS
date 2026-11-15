//! Screenshot-тесты Files: шесть демо-сцен против эталонных хешей.
//!
//! Сцены детерминированы (фейковая ФС, встроенный шрифт, целочисленный
//! рендер). При намеренном изменении внешнего вида перерисуйте PNG:
//! `cargo run -- --out screenshots`, посмотрите результат и обновите хеши.

use pagh_files::screenshots;
use pagh_ui::geom::Size;
use pagh_ui::testkit;

#[test]
fn main_scene_matches_golden() {
    let canvas = screenshots::main_scene();
    assert_eq!(canvas.size(), screenshots::DESKTOP);
    testkit::assert_golden("files-main", &canvas, 0x8506_b68f_c1a1_8cf3);
}

#[test]
fn context_menu_scene_matches_golden() {
    let canvas = screenshots::context_menu_scene();
    assert_eq!(canvas.size(), Size::new(1280, 800));
    testkit::assert_golden("files-context-menu", &canvas, 0xa65c_fc02_cb3e_103a);
}

#[test]
fn properties_scene_matches_golden() {
    let canvas = screenshots::properties_scene();
    testkit::assert_golden("files-properties", &canvas, 0xbe5c_b8fb_c32e_1123);
}

#[test]
fn list_view_scene_matches_golden() {
    let canvas = screenshots::list_view_scene();
    testkit::assert_golden("files-list-view", &canvas, 0x7e05_2ecc_5447_ac3f);
}

#[test]
fn search_scene_matches_golden() {
    let canvas = screenshots::search_scene();
    testkit::assert_golden("files-search", &canvas, 0x8677_65d5_35f5_fe62);
}

#[test]
fn empty_folder_scene_matches_golden() {
    let canvas = screenshots::empty_folder_scene();
    testkit::assert_golden("files-empty", &canvas, 0xd350_57e8_1dc1_67e3);
}

#[test]
fn scenes_are_deterministic() {
    let first = testkit::hash(&screenshots::main_scene());
    let second = testkit::hash(&screenshots::main_scene());
    assert_eq!(first, second);
}

#[test]
fn every_scene_covers_screen_with_window() {
    let theme = pagh_ui::theme::Theme::pagh_dark();
    for (name, canvas) in screenshots::all_scenes() {
        assert_eq!(
            canvas.size(),
            screenshots::DESKTOP,
            "сцена {name}: размер экрана"
        );
        // Окно занимает заметную часть экрана (не пустой фон).
        let mut window_pixels = 0usize;
        for y in 100..700 {
            for x in 200..1080 {
                if canvas.pixel(pagh_ui::Point::new(x, y)) != theme.palette.desktop {
                    window_pixels += 1;
                }
            }
        }
        assert!(
            window_pixels > (880 * 600) / 2,
            "сцена {name}: окно слишком маленькое ({window_pixels})"
        );
    }
}
