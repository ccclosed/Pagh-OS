//! Витрина виджетов `pagh-ui` в PNG без экрана.
//!
//! ```text
//! cargo run --bin pagh-ui-gallery -- --out screenshots
//! ```
//!
//! Сцены живут в [`pagh_ui::demo`]: их же используют screenshot-тесты.

use std::path::PathBuf;

use pagh_ui::testkit;
use pagh_ui::theme::Theme;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut out = PathBuf::from("screenshots");
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--out" | "-o" => {
                index += 1;
                if index < args.len() {
                    out = PathBuf::from(&args[index]);
                }
            }
            other => {
                eprintln!("неизвестный аргумент: {other}");
                std::process::exit(2);
            }
        }
        index += 1;
    }
    std::env::set_var("PAGH_SCREENSHOT_DIR", &out);

    let theme = Theme::pagh_dark();
    let scenes = [
        ("ui-gallery", pagh_ui::demo::gallery(&theme)),
        ("ui-menu", pagh_ui::demo::menu(&theme)),
        ("ui-palette", pagh_ui::demo::palette(&theme)),
    ];
    for (name, canvas) in &scenes {
        let path = testkit::save_png(name, canvas, theme.palette.desktop)?;
        println!(
            "{name}: {} (hash {:#018x})",
            path.display(),
            testkit::hash(canvas)
        );
    }
    Ok(())
}
