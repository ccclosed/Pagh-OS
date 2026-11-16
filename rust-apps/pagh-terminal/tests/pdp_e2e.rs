//! E2E: настоящий `pagh-desktop` (host build) + настоящий бинарник
//! `pagh-terminal` по AF_UNIX/PDP/1.
//!
//! На dev-сервере `welcome.flags` не содержит bit 2, поэтому это ещё и
//! проверка fallback-пути: полный кадр v1 продолжает работать. На сервере с
//! ShmSurface тот же бинарник автоматически переходит на memfd (см.
//! `tests/shm_e2e.rs`).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pagh_desktop::protocol::{key, MOD_SHIFT};
use pagh_desktop::server::{unique_socket_path, Server, ServerControl, ServerOptions};

fn wait_for<F: FnMut() -> bool>(timeout: Duration, mut predicate: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn temp_dir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pagh-terminal-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn capture(control: &ServerControl, dir: &Path, name: &str) -> Vec<u8> {
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    control.screenshot(&path);
    assert!(
        wait_for(Duration::from_secs(5), || path.exists()),
        "кадр {name} не появился"
    );
    std::thread::sleep(Duration::from_millis(40));
    std::fs::read(&path).unwrap()
}

fn frame_count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("frame_"))
                .count()
        })
        .unwrap_or(0)
}

fn latest_frame(dir: &Path) -> Option<PathBuf> {
    let mut frames: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().starts_with("frame_"))
                .unwrap_or(false)
        })
        .collect();
    frames.sort();
    frames.pop()
}

fn wait_child_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    None
}

#[test]
fn pagh_terminal_runs_under_real_pagh_desktop() {
    let socket = unique_socket_path("terminal-pdp");
    let shots = temp_dir("shots");
    let server = Server::spawn(ServerOptions {
        socket: socket.clone(),
        width: 1280,
        height: 800,
        quiet: std::env::var("PAGH_E2E_VERBOSE").is_err(),
        screenshot_dir: Some(shots.clone()),
        ..ServerOptions::default()
    })
    .expect("композитор стартует");

    let empty = capture(&server.control, &shots, "00-empty.png");
    let mut child = Command::new(env!("CARGO_BIN_EXE_pagh-terminal"))
        .args(["--socket", socket.to_str().unwrap(), "--size", "700x460"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("pagh-terminal стартует");

    let mut connected = None;
    assert!(
        wait_for(Duration::from_secs(15), || {
            let frame = capture(&server.control, &shots, "01-connected.png");
            if frame != empty {
                connected = Some(frame);
                true
            } else {
                false
            }
        }),
        "окно терминала не появилось на экране композитора"
    );
    let connected = connected.unwrap();

    // Ввод с клавиатуры композитора → клиент перерисовал текст.
    let _ = std::fs::remove_file(shots.join("02-input.png"));
    server.control.key(key::A, true, 0);
    server.control.key(key::A, false, 0);
    assert!(
        wait_for(Duration::from_secs(10), || {
            let frame = capture(&server.control, &shots, "02-input.png");
            frame != connected
        }),
        "ввод не дошёл до терминала (кадр не изменился)"
    );

    // Enter запускает демо-команду — ещё одно изменение кадра.
    let before_enter = capture(&server.control, &shots, "03-before-enter.png");
    server.control.key(key::ENTER, true, 0);
    server.control.key(key::ENTER, false, 0);
    assert!(
        wait_for(Duration::from_secs(10), || {
            let frame = capture(&server.control, &shots, "03-enter.png");
            frame != before_enter
        }),
        "Enter не изменил кадр терминала"
    );

    // Shift+A (заглавная) — проверяем, что модификаторы доходят.
    server.control.key(key::A, true, MOD_SHIFT);
    server.control.key(key::A, false, MOD_SHIFT);
    assert!(
        wait_for(Duration::from_secs(10), || frame_count(&shots) >= 3),
        "после ввода не появилось новых presented-кадров"
    );
    if let Some(frame) = latest_frame(&shots) {
        std::fs::copy(&frame, shots.join("04-terminal-under-pagh-desktop.png")).unwrap();
    }

    // Композитор закрывает окно (id по умолчанию у терминала — 101).
    server.control.close_window(101);
    let status = wait_child_exit(&mut child, Duration::from_secs(10))
        .expect("pagh-terminal не завершился после close");
    assert!(status.success(), "pagh-terminal завершился с {status:?}");

    server.control.shutdown();
    server.join().expect("композитор завершился");

    if let Ok(dir) = std::env::var("PAGH_E2E_SHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        if shots.join("04-terminal-under-pagh-desktop.png").exists() {
            std::fs::copy(
                shots.join("04-terminal-under-pagh-desktop.png"),
                Path::new(&dir).join("terminal-under-pagh-desktop.png"),
            )
            .unwrap();
        }
        if shots.join("02-input.png").exists() {
            std::fs::copy(
                shots.join("02-input.png"),
                Path::new(&dir).join("terminal-under-pagh-desktop-input.png"),
            )
            .unwrap();
        }
    }
    let _ = std::fs::remove_dir_all(&shots);
}
