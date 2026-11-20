//! E2E: реальный `pagh-desktop` (host build) + реальный бинарник `pagh-files`
//! по настоящему AF_UNIX-сокету и протоколу PDP/1.
//!
//! Проверяется весь путь: hello/welcome, create окна, commit кадров, ввод
//! (клавиатура из композитора → клиент), `launch (type 13)` по «Открыть
//! терминал» (Alt+Shift+F4) и закрытие окна композитором (`close(server)`).
//! Скриншот снимает сам композитор — это кадр рабочего стола, а не рендер
//! клиента. Если задан `PAGH_E2E_SHOTS=DIR`, ключевые кадры копируются туда.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pagh_desktop::protocol::{key, MOD_ALT, MOD_SHIFT};
use pagh_desktop::server::{
    unique_socket_path, LaunchTarget, Server, ServerControl, ServerOptions,
};

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
        "pagh-files-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Снять кадр композитора и дождаться, пока файл появится.
fn capture(control: &ServerControl, dir: &Path, name: &str) -> Vec<u8> {
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    control.screenshot(&path);
    assert!(
        wait_for(Duration::from_secs(5), || path.exists()),
        "кадр {name} не появился"
    );
    // Дождаться, пока writer закроет файл (PNG целиком).
    std::thread::sleep(Duration::from_millis(40));
    std::fs::read(&path).unwrap()
}

/// Сколько PNG-кадров композитор уже записал в каталог скриншотов.
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

/// Самый свежий кадр композитора (`frame_NNNNN.png`).
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
fn pagh_files_runs_under_real_pagh_desktop() {
    let socket = unique_socket_path("files-pdp");
    let shots = temp_dir("shots");
    let marker = shots.join("terminal-launch.txt");

    // Цель запуска для LAUNCH: настоящий pagh-terminal (если задан
    // PAGH_TERMINAL_BIN) или скрипт-заглушка, который фиксирует argv.
    let stub = shots.join("stub-terminal.sh");
    std::fs::write(
        &stub,
        format!("#!/bin/sh\nprintf '%s' \"$*\" > '{}'\n", marker.display()),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&stub).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&stub, permissions).unwrap();
    }
    let real_terminal = std::env::var("PAGH_TERMINAL_BIN")
        .ok()
        .filter(|path| Path::new(path).exists());
    let launch_target = match &real_terminal {
        Some(binary) => LaunchTarget::new(
            "terminal",
            vec![
                binary.clone(),
                "--socket".to_string(),
                "{socket}".to_string(),
            ],
        ),
        None => LaunchTarget::new(
            "terminal",
            vec![
                stub.to_string_lossy().into_owned(),
                "--socket".to_string(),
                "{socket}".to_string(),
            ],
        ),
    };

    let server = Server::spawn(ServerOptions {
        socket: socket.clone(),
        width: 1280,
        height: 800,
        quiet: std::env::var("PAGH_E2E_VERBOSE").is_err(),
        screenshot_dir: Some(shots.clone()),
        launch_targets: vec![launch_target],
        ..ServerOptions::default()
    })
    .expect("композитор стартует");

    // 1. Кадр пустого рабочего стола — база для сравнения.
    let empty = capture(&server.control, &shots, "00-empty.png");

    // 2. Запускаем реальный бинарник Files, он подключается по PDP/1.
    let binary = env!("CARGO_BIN_EXE_pagh-files");
    let trace = shots.join("files.trace");
    let mut child = Command::new(binary)
        .arg("--connect")
        .arg(&socket)
        .arg("--size")
        .arg("1000x600")
        .env("PAGH_UI_PDP_TRACE", &trace)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("pagh-files стартует");

    // 3. Окно появилось в кадре композитора (commit дошёл до сервера).
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
        "окно Files не появилось на экране композитора"
    );
    let connected = connected.unwrap();

    // 4. Ввод из композитора: стрелка вниз выделяет первый элемент, клиент
    //    перерисовывается и присылает новый кадр.
    let _ = std::fs::remove_file(shots.join("02-input.png"));
    server.control.key(key::DOWN, true, 0);
    server.control.key(key::DOWN, false, 0);
    let mut after_input = None;
    assert!(
        wait_for(Duration::from_secs(10), || {
            let frame = capture(&server.control, &shots, "02-input.png");
            if frame != connected {
                after_input = Some(frame);
                true
            } else {
                false
            }
        }),
        "клавиатурный ввод не дошёл до Files (кадр не изменился)"
    );

    // 5. «Открыть терминал» (Alt+Shift+F4) → PDP/1 launch (type 13).
    server.control.key(key::F4, true, MOD_ALT | MOD_SHIFT);
    server.control.key(key::F4, false, MOD_ALT | MOD_SHIFT);
    if real_terminal.is_some() {
        // Настоящий терминал подключится вторым клиентом и пришлёт кадр:
        // ждём роста числа presented-кадров композитора (create + commit).
        let before = frame_count(&shots);
        assert!(
            wait_for(Duration::from_secs(20), || frame_count(&shots)
                >= before + 2),
            "pagh-terminal не прислал кадр после launch (кадров было {before})"
        );
        std::thread::sleep(Duration::from_millis(300));
        if let Some(frame) = latest_frame(&shots) {
            std::fs::copy(&frame, shots.join("03-launched.png")).unwrap();
        }
    } else {
        assert!(
            wait_for(Duration::from_secs(10), || marker.exists()),
            "композитор не запустил приложение по launch"
        );
        let argv = std::fs::read_to_string(&marker).unwrap();
        assert!(
            argv.contains(&socket.to_string_lossy().into_owned()),
            "в argv запуска нет сокета: {argv:?}"
        );
    }

    // 6. Финальный кадр рабочего стола для отчёта/PR: это PNG самого
    //    композитора (последний presented-кадр), а не рендер клиента.
    let final_frame = match latest_frame(&shots) {
        Some(frame) => {
            let bytes = std::fs::read(&frame).unwrap();
            std::fs::copy(&frame, shots.join("04-files-under-pagh-desktop.png")).unwrap();
            bytes
        }
        None => capture(&server.control, &shots, "04-files-under-pagh-desktop.png"),
    };
    assert!(final_frame.starts_with(b"\x89PNG"), "кадр не PNG");
    assert!(
        final_frame.len() > 20_000,
        "кадр подозрительно маленький: {} байт",
        final_frame.len()
    );
    if let Ok(dir) = std::env::var("PAGH_E2E_SHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(
            shots.join("04-files-under-pagh-desktop.png"),
            Path::new(&dir).join("files-under-pagh-desktop.png"),
        )
        .unwrap();
        std::fs::copy(
            shots.join("02-input.png"),
            Path::new(&dir).join("files-under-pagh-desktop-input.png"),
        )
        .unwrap();
        if shots.join("03-launched.png").exists() {
            std::fs::copy(
                shots.join("03-launched.png"),
                Path::new(&dir).join("files-under-pagh-desktop-launched.png"),
            )
            .unwrap();
        }
    }

    // 7. Композитор закрывает окно (кнопка x или выход сервера) — клиент
    //    получает close(reason=SERVER) и завершается сам.
    server.control.close_window(1);
    let status = wait_child_exit(&mut child, Duration::from_secs(10))
        .expect("pagh-files не завершился после close");
    assert!(status.success(), "pagh-files завершился с {status:?}");

    // Сервер в dev умеет ShmSurface: клиент обязан идти shm-путём, без
    // пиксельного payload в управляющем сокете.
    let trace = std::fs::read_to_string(&trace).expect("PDP trace");
    assert!(
        trace.lines().any(|line| line.starts_with("> shm_offer")),
        "Files не сделал shm_offer:\n{trace}"
    );
    assert!(trace.lines().any(|line| line.starts_with("< shm_ready")));
    assert!(trace.lines().any(|line| line.starts_with("> commit_shm")));
    assert!(
        !trace.contains("pixels="),
        "shm-путь не должен слать пиксели:\n{trace}"
    );

    server.control.shutdown();
    server.join().expect("композитор завершился");
    let _ = std::fs::remove_dir_all(&shots);
}

#[test]
fn pagh_files_falls_back_when_shm_disabled() {
    let socket = unique_socket_path("files-fallback");
    let shots = temp_dir("fallback-shots");
    let trace = shots.join("files-fallback.trace");
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_pagh-files"))
        .arg("--connect")
        .arg(&socket)
        .arg("--size")
        .arg("800x500")
        .env("PAGH_UI_DISABLE_SHM", "1")
        .env("PAGH_UI_PDP_TRACE", &trace)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("pagh-files стартует");
    assert!(
        wait_for(Duration::from_secs(15), || {
            capture(&server.control, &shots, "01-fallback.png") != empty
        }),
        "окно Files не появилось на экране композитора (fallback)"
    );
    server.control.close_window(1);
    let status = wait_child_exit(&mut child, Duration::from_secs(10))
        .expect("pagh-files не завершился после close");
    assert!(status.success(), "pagh-files завершился с {status:?}");
    server.control.shutdown();
    server.join().expect("композитор завершился");

    let trace = std::fs::read_to_string(&trace).expect("PDP trace");
    assert!(
        !trace.contains("shm_offer"),
        "PAGH_UI_DISABLE_SHM не должен делать shm_offer:\n{trace}"
    );
    assert!(
        trace
            .lines()
            .any(|line| line.starts_with("> commit") && line.contains("pixels=")),
        "fallback обязан слать полный кадр:\n{trace}"
    );
    let _ = std::fs::remove_dir_all(&shots);
}
