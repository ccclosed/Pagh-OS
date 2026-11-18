//! S4(c): демо-клиент завершается чисто, когда сервер уходит (EOF на сокете).
//!
//! Сервер останавливается по `exit_after_ms` — это тот же путь, что и
//! `pagh-desktop --exit-after-ms`: `cleanup()` закрывает клиентские сокеты,
//! клиент видит `recv == 0`. Ожидаемое поведение — выход с кодом 0, без паники
//! и без спина.

use pagh_desktop::server::{unique_socket_path, Server, ServerOptions};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn demo_client_exits_cleanly_when_server_exits() {
    let socket = unique_socket_path("demo-eof");
    let options = ServerOptions {
        socket: socket.clone(),
        width: 320,
        height: 200,
        quiet: true,
        exit_after_ms: Some(600),
        ..ServerOptions::default()
    };
    let server = Server::spawn(options).expect("сервер стартует");

    let mut child = Command::new(env!("CARGO_BIN_EXE_pagh-desktop-demo"))
        .arg("--connect")
        .arg(&socket)
        .arg("--app")
        .arg("terminal")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("демо-клиент стартует");

    // Клиент должен не зависнуть, а выйти после ухода сервера.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("демо-клиент не завершился за 15 с после выхода сервера");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let output = child.wait_with_output().expect("вывод демо-клиента");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "EOF на сокете должен давать exit 0, получено {}; stdout={stdout:?} stderr={stderr:?}",
        output.status
    );
    assert!(
        !stderr.contains("panicked"),
        "клиент не должен паниковать: {stderr:?}"
    );

    server
        .join()
        .expect("сервер завершился сам по exit_after_ms");
}
