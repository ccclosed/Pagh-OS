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
    testkit::assert_golden("files-main", &canvas, 0xe72e_11e9_4799_f4c8);
}

#[test]
fn context_menu_scene_matches_golden() {
    let canvas = screenshots::context_menu_scene();
    assert_eq!(canvas.size(), Size::new(1280, 800));
    testkit::assert_golden("files-context-menu", &canvas, 0x046b_e907_b24c_406d);
}

#[test]
fn properties_scene_matches_golden() {
    let canvas = screenshots::properties_scene();
    testkit::assert_golden("files-properties", &canvas, 0x3406_f6da_bdf8_761b);
}

#[test]
fn list_view_scene_matches_golden() {
    let canvas = screenshots::list_view_scene();
    testkit::assert_golden("files-list-view", &canvas, 0xe13d_db3d_489c_de70);
}

#[test]
fn search_scene_matches_golden() {
    let canvas = screenshots::search_scene();
    testkit::assert_golden("files-search", &canvas, 0x2895_0429_d616_48bd);
}

#[test]
fn empty_folder_scene_matches_golden() {
    let canvas = screenshots::empty_folder_scene();
    testkit::assert_golden("files-empty", &canvas, 0x32bf_8cce_00d4_5bf8);
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
