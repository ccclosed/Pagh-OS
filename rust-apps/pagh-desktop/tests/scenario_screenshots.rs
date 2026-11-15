//! Проверка headless-сценария и скриншотов: файлы валидны, отличаются друг от друга,
//! панель/окна реально нарисованы (не одноцветный кадр).

use pagh_desktop::scenario;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

fn png_size(data: &[u8]) -> (u32, u32) {
    assert!(data.starts_with(b"\x89PNG\r\n\x1a\n"), "PNG-сигнатура");
    assert_eq!(&data[12..16], b"IHDR");
    let width = u32::from_be_bytes(data[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(data[20..24].try_into().unwrap());
    (width, height)
}

fn digest(data: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}

#[test]
fn scenario_writes_distinct_valid_screenshots() {
    let dir = std::env::temp_dir().join(format!(
        "pagh-desktop-scenario-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let report = scenario::run(&dir).expect("сценарий проигрывается");
    assert_eq!(report.screenshots.len(), 9);
    assert!(report.notes.len() >= 8);

    let mut hashes = HashSet::new();
    for path in &report.screenshots {
        assert!(path.exists(), "{} отсутствует", path.display());
        let data = std::fs::read(path).expect("чтение PNG");
        let (width, height) = png_size(&data);
        assert_eq!((width, height), (1280, 720));
        assert!(
            data.len() > 10_000,
            "{} подозрительно мал: {} байт",
            path.display(),
            data.len()
        );
        hashes.insert(digest(&data));
    }
    assert!(
        hashes.len() >= 7,
        "скриншоты должны отличаться, уникальных {}",
        hashes.len()
    );

    let transcript = std::fs::read_to_string(dir.join("scenario.txt")).expect("transcript");
    assert!(transcript.contains("23:38"));
    assert!(transcript.contains("15.11.2026"));
    std::fs::remove_dir_all(dir).ok();
}
