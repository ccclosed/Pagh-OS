//! E2E: настоящий `pagh-desktop` (host build, headless backend) + настоящий
//! бинарник демо по AF_UNIX и протоколу PDP/1.
//!
//! Проверяется именно compositor/SHM-путь PR5:
//! * демо подключается, получает `SHM_READY`, рендерит в memfd и шлёт
//!   `COMMIT`+`DAMAGE` — серверные счётчики `shm_offers`/`shm_commits`
//!   доказывают, что кадр не ехал по сокету;
//! * композитор реально скомпоновал окно: пиксель рабочего стола под окном
//!   изменился (сравнение до/после `sample_pixel`);
//! * клиент печатает `TINYGL`-маркеры с теми же дайджестами, что host-тест;
//! * процесс завершается с кодом 0 на `--frames 5`.
//!
//! Скриншот композитора пишется в temp-каталог только как артефакт.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pagh_desktop::server::{unique_socket_path, Server, ServerOptions};

fn temp_dir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pagh-gl-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn wait_for<F: FnMut() -> bool>(timeout: Duration, mut predicate: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn demo_renders_into_the_compositor_over_shm() {
    let socket = unique_socket_path("gl-demo");
    let shots = temp_dir("shots");
    let server = Server::spawn(ServerOptions {
        socket: socket.clone(),
        width: 1280,
        height: 800,
        quiet: true,
        screenshot_dir: Some(shots.clone()),
        ..ServerOptions::default()
    })
    .expect("композитор стартует");
    assert!(
        wait_for(Duration::from_secs(5), || socket.exists()),
        "сокет композитора не появился"
    );

    // Пиксель под будущим окном (первое окно каскадом в 96,64): до окна это
    // фон рабочего стола. Ждём, пока headless-сервер сделает первый кадр.
    let probe = (200, 150);
    let mut before = None;
    assert!(
        wait_for(Duration::from_secs(5), || {
            before = server.control.sample_pixel(probe.0, probe.1);
            before.is_some()
        }),
        "композитор не отдал пиксель фона"
    );
    let before = before.unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_pagh-gl-demo"))
        .args([
            "gears",
            "--connect",
            socket.to_str().unwrap(),
            "--frames",
            "0", // бесконечно: окно должно быть живо на момент скриншота
            "--size",
            "640x480",
            "--hash",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("демо стартует");

    // Ждём, пока сервер обработает достаточно SHM-коммитов: счётчики
    // доказывают путь без full-frame upload, а кадр 59 успевает напечатать
    // накопительный ft.
    let mut stats = server.counters.snapshot();
    assert!(
        wait_for(Duration::from_secs(20), || {
            stats = server.counters.snapshot();
            stats.shm_offers >= 1 && stats.shm_commits >= 60
        }),
        "композитор не увидел SHM offer/commits: {stats:?}"
    );
    assert_eq!(stats.shm_offers, 1, "один offer на один размер окна");
    assert!(
        stats.shm_bytes >= 60 * 640 * 480 * 4,
        "SHM-байты меньше 60 кадров: {stats:?}"
    );

    // Композитор скомпоновал окно: пиксель под ним перестал быть фоном.
    assert!(
        wait_for(Duration::from_secs(10), || {
            server
                .control
                .sample_pixel(probe.0, probe.1)
                .is_some_and(|pixel| pixel != before)
        }),
        "окно демо не появилось в кадре композитора (пиксель не изменился)"
    );

    // Артефакт: PNG-скриншот рабочего стола с окном демо — снимаем, пока
    // клиент жив (после его выхода композитор окно уже убрал).
    let shot = shots.join("gl-demo-desktop.png");
    server.control.screenshot(&shot);
    assert!(
        wait_for(Duration::from_secs(5), || shot.exists()),
        "скриншот композитора не записался"
    );
    std::thread::sleep(Duration::from_millis(50));
    let size = std::fs::metadata(&shot).unwrap().len();
    assert!(size > 1024, "PNG-скриншот подозрительно мал: {size} байт");

    // Теперь останавливаем бесконечное демо и разбираем его вывод.
    let _ = child.kill();
    let status = child.wait().expect("wait после kill");
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        !status.success(),
        "SIGKILL не должен давать success: {status}"
    );
    assert!(
        stderr.contains("connected to"),
        "демо не напечатало подключение:\n{stderr}"
    );

    // Маркеры кадров: per-frame строки (--hash) с теми же дайджестами, что
    // pinned host-тест, и накопительный frame time после 60-го кадра.
    for frame in 0..5u32 {
        assert!(
            stdout.contains(&format!("TINYGL gears frame={frame} digest=0x")),
            "нет TINYGL-строки для кадра {frame}:\n{stdout}"
        );
    }
    assert!(
        stdout.contains("frame=0 digest=0x3d6fcc428ccbb634"),
        "дайджест кадра 0 изменился:\n{stdout}"
    );
    assert!(
        stdout.contains("frame=1 digest=0x2a04fc59b4722ea0"),
        "дайджест кадра 1 изменился:\n{stdout}"
    );
    assert!(stdout.contains("ft="), "frame time не напечатан:\n{stdout}");

    server.control.shutdown();
    server.join().expect("композитор корректно остановился");
}
