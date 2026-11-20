//! Сквозные тесты AF_UNIX-сервера: рукопожатие, окна, кадры, ввод, закрытие,
//! устойчивость к мусору в сокете.

use pagh_desktop::client::Client;
use pagh_desktop::protocol::{
    key, Message, ERR_BAD_MESSAGE, ERR_UNSUPPORTED, MOD_ALT, MOD_CTRL, WELCOME_SHM_SURFACE,
};
use pagh_desktop::server::{unique_socket_path, LaunchTarget, Server, ServerHandle, ServerOptions};
use pagh_desktop::surface::{PixelFormat, Surface};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn spawn_server(tag: &str) -> (ServerHandle, PathBuf) {
    let socket = unique_socket_path(tag);
    let options = ServerOptions {
        socket: socket.clone(),
        width: 800,
        height: 600,
        quiet: true,
        ..ServerOptions::default()
    };
    let server = Server::spawn(options).expect("сервер стартует");
    (server, socket)
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

fn expect_configure(client: &mut Client, window_id: u32) -> (i32, i32, u32, u32) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match client
            .next_message(Duration::from_millis(200))
            .expect("чтение сообщения")
        {
            Some(Message::Configure {
                window_id: id,
                x,
                y,
                width,
                height,
                ..
            }) if id == window_id => return (x, y, width, height),
            Some(_) => continue,
            None => continue,
        }
    }
    panic!("configure для окна {window_id} не пришёл");
}

fn expect_input(client: &mut Client) -> pagh_desktop::protocol::InputEvent {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match client
            .next_message(Duration::from_millis(200))
            .expect("чтение сообщения")
        {
            Some(Message::Input { event, .. }) => return event,
            Some(_) => continue,
            None => continue,
        }
    }
    panic!("input не пришёл");
}

#[test]
fn socket_end_to_end_flow() {
    let (server, socket) = spawn_server("e2e");
    let mut client = Client::connect_retry(&socket, 200).expect("клиент подключается");
    let welcome = client.hello("test-client").expect("hello");
    assert_eq!((welcome.screen.w, welcome.screen.h), (800, 600));
    assert_ne!(welcome.client_id, 0);

    client
        .create_window(11, 320, 200, PixelFormat::Xrgb8888, "Test window")
        .expect("create");
    let (x, y, width, height) = expect_configure(&mut client, 11);
    assert_eq!((width, height), (320, 200));

    let mut surface = Surface::filled(320, 200, PixelFormat::Xrgb8888, 0x0033_6677);
    surface.set(10, 10, 0x00FF_0000);
    client.commit(11, &surface).expect("commit");

    // Клик в содержимое окна: сервер должен переслать button + motion.
    let control = &server.control;
    control.mouse_move(x + 10, y + 10);
    control.mouse_button(1, true);
    let event = expect_input(&mut client);
    assert!(
        matches!(
            event,
            pagh_desktop::protocol::InputEvent::Button {
                pressed: true,
                button: 1,
                ..
            } | pagh_desktop::protocol::InputEvent::Motion { .. }
        ),
        "ожидался pointer motion/button, получено {event:?}"
    );
    control.mouse_button(1, false);

    // Клавиатура уходит в сфокусированное окно.
    control.key(key::A, true, 0);
    let mut got_key = false;
    for _ in 0..4 {
        let event = expect_input(&mut client);
        if let pagh_desktop::protocol::InputEvent::Key {
            code,
            pressed: true,
            ..
        } = event
        {
            if code == key::A {
                got_key = true;
                break;
            }
        }
    }
    assert!(got_key, "клавиатурное событие не доставлено");

    // Скриншот через сервер: файл должен появиться и быть PNG.
    let shot = socket.with_extension("png");
    control.screenshot(&shot);
    assert!(
        wait_for(Duration::from_secs(3), || shot.exists()),
        "скриншот не создан"
    );
    let data = std::fs::read(&shot).expect("чтение скриншота");
    assert!(data.starts_with(b"\x89PNG"), "не PNG");
    std::fs::remove_file(&shot).ok();

    // Клиент закрывает окно; сервер продолжает работать.
    client.close_window(11).expect("close");
    std::thread::sleep(Duration::from_millis(50));
    assert!(server.control.is_running());

    server.control.shutdown();
    server.join().expect("сервер завершился");
    assert!(!socket.exists(), "сокет должен быть удалён");
}

#[test]
fn malformed_client_gets_error_and_server_survives() {
    let (server, socket) = spawn_server("bad");
    let mut raw = UnixStream::connect(&socket).expect("сырое подключение");
    raw.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    // 12 байт заголовка с плохой сигнатурой.
    let header = [b'X', b'X', b'X', b'X', 1, 0, 0, 0, 0, 0, 0, 0];
    raw.write_all(&header).expect("запись мусора");
    let response = pagh_desktop::protocol::read_message(&mut raw).expect("сервер отвечает ошибкой");
    match response {
        Message::Error { code, .. } => assert_eq!(code, ERR_BAD_MESSAGE),
        other => panic!("ожидалась error, получено {other:?}"),
    }

    // Сервер жив: настоящий клиент проходит hello.
    let mut client = Client::connect_retry(&socket, 100).expect("клиент после мусора");
    let welcome = client.hello("survivor").expect("hello после мусора");
    assert_eq!(welcome.screen.w, 800);

    server.control.shutdown();
    server.join().unwrap();
}

#[test]
fn two_clients_focus_and_close_routing() {
    let (server, socket) = spawn_server("multi");
    let mut first = Client::connect_retry(&socket, 200).expect("клиент 1");
    let welcome_a = first.hello("client-a").expect("hello a");
    first
        .create_window(101, 300, 200, PixelFormat::Xrgb8888, "A")
        .expect("create a");
    let _ = expect_configure(&mut first, 101);

    let mut second = Client::connect_retry(&socket, 200).expect("клиент 2");
    let welcome_b = second.hello("client-b").expect("hello b");
    assert_ne!(welcome_a.client_id, welcome_b.client_id);
    second
        .create_window(102, 300, 200, PixelFormat::Xrgb8888, "B")
        .expect("create b");
    let _ = expect_configure(&mut second, 102);

    // Первый клиент должен получить focus=false при появлении второго окна.
    let mut lost_focus = false;
    for _ in 0..6 {
        match first
            .next_message(Duration::from_millis(300))
            .expect("чтение a")
        {
            Some(Message::Focus {
                window_id: 101,
                focused: false,
            }) => {
                lost_focus = true;
                break;
            }
            Some(_) => continue,
            None => continue,
        }
    }
    assert!(lost_focus, "первое окно должно потерять фокус");

    // Закрытие окна B возвращает фокус окну A.
    second.close_window(102).expect("close b");
    let mut regained = false;
    for _ in 0..8 {
        match first
            .next_message(Duration::from_millis(300))
            .expect("чтение a")
        {
            Some(Message::Focus {
                window_id: 101,
                focused: true,
            }) => {
                regained = true;
                break;
            }
            Some(_) => continue,
            None => continue,
        }
    }
    assert!(regained, "фокус должен вернуться первому окну");

    server.control.shutdown();
    server.join().unwrap();
}

#[test]
fn stale_socket_is_replaced_on_bind() {
    let socket = unique_socket_path("stale");
    // Оставляем файл после «упавшего» сервера.
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("старый сокет");
    drop(listener);
    assert!(Path::new(&socket).exists());
    let options = ServerOptions {
        socket: socket.clone(),
        width: 320,
        height: 240,
        quiet: true,
        max_frames: Some(1),
        ..ServerOptions::default()
    };
    let server = Server::spawn(options).expect("bind поверх устаревшего сокета");
    assert!(wait_for(Duration::from_secs(3), || !server
        .control
        .is_running()));
    server.join().expect("сервер с max_frames завершился");
    assert!(!socket.exists());
}

/// Проверка расширения `launch` (type 13): сервер запускает команду из
/// `launch_targets`, подставляет путь сокета и отвечает ошибкой на неизвестное
/// приложение.
#[test]
fn launch_message_spawns_configured_app() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!(
        "pagh-launch-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("launched.txt");
    let script = dir.join("stub.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf '%s' \"$*\" > '{}'\n", marker.display()),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let socket = unique_socket_path("launch");
    let options = ServerOptions {
        socket: socket.clone(),
        width: 640,
        height: 480,
        quiet: true,
        launch_targets: vec![LaunchTarget::new(
            "terminal",
            vec![
                script.to_string_lossy().into_owned(),
                "--socket".to_string(),
                "{socket}".to_string(),
            ],
        )],
        ..ServerOptions::default()
    };
    let server = Server::spawn(options).expect("сервер стартует");
    let mut client = Client::connect_retry(&socket, 200).expect("клиент подключается");
    client.hello("launch-test").expect("hello");

    client
        .send(&Message::Launch {
            app: "terminal".into(),
            cwd: "/home/user".into(),
        })
        .expect("launch");
    assert!(
        wait_for(Duration::from_secs(5), || marker.exists()),
        "команда запуска не выполнилась"
    );
    let argv = std::fs::read_to_string(&marker).unwrap();
    assert!(argv.contains("--socket"), "argv без --socket: {argv:?}");
    assert!(
        argv.contains(&socket.to_string_lossy().into_owned()),
        "в argv нет пути сокета: {argv:?}"
    );

    // Неизвестное приложение: spawn_demo выключен → ошибка UNSUPPORTED.
    client
        .send(&Message::Launch {
            app: "paint".into(),
            cwd: String::new(),
        })
        .expect("launch unknown");
    let mut error = None;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(Message::Error { code, message }) = client
            .next_message(Duration::from_millis(200))
            .expect("чтение")
        {
            error = Some((code, message));
            break;
        }
    }
    let (code, message) = error.expect("ошибка на неизвестное приложение");
    assert_eq!(code, ERR_UNSUPPORTED, "message = {message:?}");
    assert!(message.contains("paint"), "message = {message:?}");

    server.control.shutdown();
    server.join().expect("сервер завершился");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Уникальный каталог для стабов и маркеров теста.
fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pagh-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Исполняемый `/bin/sh`-стаб: дописывает свою argv строкой в `marker`.
fn stub_app(dir: &Path, name: &str, marker: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join(name);
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n",
            marker.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();
    script
}

/// Панель/меню/хоткей (`Server::launch`, Ctrl+Alt+T) обязаны открывать цель из
/// `launch_targets`, а не демо-клиент: иначе MVP-образ по клику поднимает
/// `pagh-desktop-demo`, хотя цель настроена на реальный бинарник.
#[test]
fn hotkey_launches_configured_target_not_demo() {
    let dir = scratch_dir("hotkey-target");
    let target_marker = dir.join("target.txt");
    let demo_marker = dir.join("demo.txt");
    let target = stub_app(&dir, "target.sh", &target_marker);
    let demo = stub_app(&dir, "demo.sh", &demo_marker);
    let socket = unique_socket_path("hotkey-target");
    let options = ServerOptions {
        socket: socket.clone(),
        width: 640,
        height: 480,
        quiet: true,
        // Демо включён явно: если `launch` пойдёт старым путём, маркер появится.
        spawn_demo: true,
        demo_binary: Some(demo),
        launch_targets: vec![LaunchTarget::new(
            "terminal",
            vec![
                target.to_string_lossy().into_owned(),
                "--socket".to_string(),
                "{socket}".to_string(),
            ],
        )],
        ..ServerOptions::default()
    };
    let server = Server::spawn(options).expect("сервер стартует");
    server.control.key(key::T, true, MOD_CTRL | MOD_ALT);

    assert!(
        wait_for(Duration::from_secs(5), || target_marker.exists()),
        "хоткей не запустил цель из launch_targets"
    );
    let argv = std::fs::read_to_string(&target_marker).unwrap();
    assert!(
        argv.contains(&socket.to_string_lossy().into_owned()),
        "в argv цели нет пути сокета: {argv:?}"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !demo_marker.exists(),
        "хоткей запустил демо-клиент вместо настроенной цели"
    );

    server.control.shutdown();
    server.join().expect("сервер завершился");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Без цели `launch` сохраняет прежний фолбэк: демо-клиент с `--app <id>`.
#[test]
fn hotkey_without_target_falls_back_to_demo() {
    let dir = scratch_dir("hotkey-demo");
    let demo_marker = dir.join("demo.txt");
    let demo = stub_app(&dir, "demo.sh", &demo_marker);
    let socket = unique_socket_path("hotkey-demo");
    let options = ServerOptions {
        socket: socket.clone(),
        width: 640,
        height: 480,
        quiet: true,
        spawn_demo: true,
        demo_binary: Some(demo),
        ..ServerOptions::default()
    };
    let server = Server::spawn(options).expect("сервер стартует");
    server.control.key(key::T, true, MOD_CTRL | MOD_ALT);

    assert!(
        wait_for(Duration::from_secs(5), || demo_marker.exists()),
        "фолбэк на демо-клиент не сработал"
    );
    let argv = std::fs::read_to_string(&demo_marker).unwrap();
    assert!(
        argv.contains("--app terminal"),
        "демо-клиент запущен без --app terminal: {argv:?}"
    );

    server.control.shutdown();
    server.join().expect("сервер завершился");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Ни цели, ни демо: `launch` только пишет диагностику — сервер жив и отвечает.
#[test]
fn hotkey_without_target_or_demo_keeps_server_alive() {
    let socket = unique_socket_path("hotkey-none");
    let options = ServerOptions {
        socket: socket.clone(),
        width: 640,
        height: 480,
        quiet: true,
        ..ServerOptions::default()
    };
    let server = Server::spawn(options).expect("сервер стартует");
    let mut client = Client::connect_retry(&socket, 200).expect("клиент подключается");
    client.hello("hotkey-none").expect("hello");

    server.control.key(key::T, true, MOD_CTRL | MOD_ALT);
    client.ping(7).expect("ping");
    let mut got = None;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(Message::Pong { nonce }) = client
            .next_message(Duration::from_millis(100))
            .expect("чтение")
        {
            got = Some(nonce);
            break;
        }
    }
    assert_eq!(got, Some(7), "сервер не ответил pong после launch без цели");
    assert!(
        server.control.is_running(),
        "сервер завершился после launch"
    );

    server.control.shutdown();
    server.join().expect("сервер завершился");
}

/// `--autostart APP` открывает цель сразу после старта и ровно один раз.
#[test]
fn autostart_opens_target_once_at_startup() {
    let dir = scratch_dir("autostart");
    let target_marker = dir.join("target.txt");
    let demo_marker = dir.join("demo.txt");
    let target = stub_app(&dir, "target.sh", &target_marker);
    let demo = stub_app(&dir, "demo.sh", &demo_marker);
    let socket = unique_socket_path("autostart");
    let options = ServerOptions {
        socket: socket.clone(),
        width: 640,
        height: 480,
        quiet: true,
        spawn_demo: true,
        demo_binary: Some(demo),
        launch_targets: vec![LaunchTarget::new(
            "terminal",
            vec![
                target.to_string_lossy().into_owned(),
                "--socket".to_string(),
                "{socket}".to_string(),
            ],
        )],
        autostart: Some("terminal".to_string()),
        ..ServerOptions::default()
    };
    let server = Server::spawn(options).expect("сервер стартует");

    assert!(
        wait_for(Duration::from_secs(5), || target_marker.exists()),
        "autostart не запустил цель"
    );
    // Главный цикл успевает пройти много итераций: без флага «уже запущено»
    // маркер пополнялся бы каждый кадр.
    std::thread::sleep(Duration::from_millis(400));
    let launches = std::fs::read_to_string(&target_marker)
        .unwrap()
        .lines()
        .count();
    assert_eq!(launches, 1, "autostart сработал больше одного раза");
    let argv = std::fs::read_to_string(&target_marker).unwrap();
    assert!(
        argv.contains(&socket.to_string_lossy().into_owned()),
        "в argv autostart-цели нет пути сокета: {argv:?}"
    );
    assert!(!demo_marker.exists(), "autostart ушёл в демо-клиент");

    server.control.shutdown();
    server.join().expect("сервер завершился");
    let _ = std::fs::remove_dir_all(&dir);
}

fn xrgb_data(width: u32, height: u32, color: u32) -> Vec<u8> {
    let mut data = vec![0u8; width as usize * height as usize * 4];
    for px in data.chunks_exact_mut(4) {
        px.copy_from_slice(&(color & 0x00FF_FFFF).to_le_bytes());
    }
    data
}

fn set_xrgb(data: &mut [u8], width: u32, x: u32, y: u32, color: u32) {
    let off = (y as usize * width as usize + x as usize) * 4;
    data[off..off + 4].copy_from_slice(&(color & 0x00FF_FFFF).to_le_bytes());
}

fn write_memfd(fd: &std::os::unix::io::OwnedFd, data: &[u8]) {
    let mut file = std::fs::File::from(fd.try_clone().expect("clone memfd"));
    file.seek(SeekFrom::Start(0)).expect("seek memfd");
    file.write_all(data).expect("write memfd");
}

fn wait_ready(client: &mut Client, window_id: u32) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(Message::ShmReady {
            window_id: id,
            generation,
        }) = client
            .next_message(Duration::from_millis(200))
            .expect("ready")
        {
            if id == window_id {
                return generation;
            }
        }
    }
    panic!("shm_ready для окна {window_id} не пришёл");
}

#[test]
fn idle_has_zero_presents_and_cursor_damage_is_small() {
    let (server, socket) = spawn_server("idle");
    let mut client = Client::connect_retry(&socket, 200).expect("клиент");
    client.hello("idle").expect("hello");
    client
        .create_window(21, 320, 200, PixelFormat::Xrgb8888, "Idle")
        .expect("create");
    let (x, y, _, _) = expect_configure(&mut client, 21);
    client
        .commit(
            21,
            &Surface::filled(320, 200, PixelFormat::Xrgb8888, 0x0022_3344),
        )
        .expect("commit");
    assert!(
        wait_for(Duration::from_secs(3), || server
            .counters
            .snapshot()
            .presents
            >= 1),
        "первый present не случился"
    );

    // Дать серверу дообработать focus/configure damage и обнулить счётчики.
    std::thread::sleep(Duration::from_millis(150));
    server.counters.reset();
    std::thread::sleep(Duration::from_millis(150));
    server.counters.reset();
    std::thread::sleep(Duration::from_secs(2));
    let idle = server.counters.snapshot();
    assert_eq!(idle.presents, 0, "idle не должен презентовать: {idle:?}");
    assert_eq!(idle.bytes_presented, 0, "idle не должен писать байты");
    assert_eq!(idle.damage_rects, 0);

    // Курсор: маленький damage, не полный экран.
    server.control.mouse_move(x + 40, y + 40);
    assert!(
        wait_for(Duration::from_secs(3), || server
            .counters
            .snapshot()
            .presents
            >= 1),
        "present после движения курсора"
    );
    let cursor = server.counters.snapshot();
    eprintln!("e2e idle: {idle:?}; e2e cursor: {cursor:?}");
    assert!(
        cursor.bytes_presented > 0 && cursor.bytes_presented < 20_000,
        "курсор должен дать маленький damage: {cursor:?}"
    );

    server.control.shutdown();
    server.join().expect("сервер завершился");
}

#[test]
fn drag_damage_is_bounded_by_window_not_screen() {
    let (server, socket) = spawn_server("drag");
    let mut client = Client::connect_retry(&socket, 200).expect("клиент");
    client.hello("drag").expect("hello");
    client
        .create_window(31, 320, 200, PixelFormat::Xrgb8888, "Drag")
        .expect("create");
    let (x, y, _, _) = expect_configure(&mut client, 31);
    client
        .commit(
            31,
            &Surface::filled(320, 200, PixelFormat::Xrgb8888, 0x0044_5566),
        )
        .expect("commit");
    assert!(wait_for(Duration::from_secs(3), || server
        .counters
        .snapshot()
        .presents
        >= 1));
    std::thread::sleep(Duration::from_millis(150));
    server.counters.reset();

    let title_y = y - 20;
    server.control.mouse_move(x + 50, title_y);
    server.control.mouse_button(1, true);
    server.control.mouse_move(x + 80, title_y + 30);
    assert!(
        wait_for(Duration::from_secs(3), || server
            .counters
            .snapshot()
            .presents
            >= 1),
        "present во время drag"
    );
    server.control.mouse_button(1, false);
    std::thread::sleep(Duration::from_millis(100));
    let stats = server.counters.snapshot();
    eprintln!("e2e drag: {stats:?}");
    let screen_bytes = 800u64 * 600 * 4;
    assert!(
        stats.bytes_presented > 0 && stats.bytes_presented < screen_bytes / 2,
        "drag не должен презентовать полэкрана: {stats:?}"
    );

    server.control.shutdown();
    server.join().expect("сервер завершился");
}

#[test]
fn shm_surface_commit_reoffer_and_stale_generation() {
    let (server, socket) = spawn_server("shm");
    let mut client = Client::connect_retry(&socket, 200).expect("клиент");
    let welcome = client.hello("shm").expect("hello");
    assert_ne!(
        welcome.flags & WELCOME_SHM_SURFACE,
        0,
        "welcome должен объявить ShmSurface"
    );
    client
        .create_window(41, 320, 200, PixelFormat::Xrgb8888, "SHM")
        .expect("create");
    let (x, y, width, height) = expect_configure(&mut client, 41);
    assert_eq!((width, height), (320, 200));

    let stride = width * 4;
    let size = stride as usize * height as usize;
    let fd = pagh_desktop::shm::create_memfd("pagh-e2e-shm", size).expect("memfd");
    let mut data = xrgb_data(width, height, 0x0011_2233);
    set_xrgb(&mut data, width, 10, 10, 0x00FF_0000);
    write_memfd(&fd, &data);
    client
        .shm_offer(
            41,
            width,
            height,
            PixelFormat::Xrgb8888,
            stride,
            fd.as_raw_fd(),
        )
        .expect("shm_offer");
    let generation = wait_ready(&mut client, 41);
    client
        .shm_commit(41, generation, &[(0, 0, width, height)])
        .expect("shm commit");

    assert!(
        wait_for(Duration::from_secs(3), || server
            .control
            .sample_pixel(x + 10, y + 10)
            == Some(0x00FF_0000)),
        "pattern из memfd не попал в back buffer"
    );
    assert_eq!(
        server.control.sample_pixel(x + 20, y + 20),
        Some(0x0011_2233)
    );
    let stats = server.counters.snapshot();
    assert_eq!(stats.shm_offers, 1);
    assert!(stats.shm_commits >= 1);
    assert!(stats.shm_bytes >= size as u64);
    eprintln!("e2e shm initial: {stats:?}");

    // Reoffer: новый memfd и generation; старый commit должен быть отброшен.
    let fd2 = pagh_desktop::shm::create_memfd("pagh-e2e-shm-2", size).expect("memfd 2");
    let mut data2 = xrgb_data(width, height, 0x0000_FF00);
    set_xrgb(&mut data2, width, 40, 40, 0x0000_00FF);
    write_memfd(&fd2, &data2);
    client
        .shm_offer(
            41,
            width,
            height,
            PixelFormat::Xrgb8888,
            stride,
            fd2.as_raw_fd(),
        )
        .expect("shm_offer 2");
    let generation2 = wait_ready(&mut client, 41);
    assert_ne!(generation2, generation, "generation должен смениться");
    // RELEASE старого generation приходит после READY.
    let mut released = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !released {
        if let Some(Message::ShmRelease {
            window_id,
            generation: old,
        }) = client
            .next_message(Duration::from_millis(200))
            .expect("release")
        {
            released = window_id == 41 && old == generation;
        }
    }
    assert!(released, "сервер не освободил старый generation");

    // Старый generation: молча отбрасывается, новый цвет не появляется.
    client
        .shm_commit(41, generation, &[(0, 0, width, height)])
        .expect("stale commit");
    std::thread::sleep(Duration::from_millis(150));
    assert_ne!(
        server.control.sample_pixel(x + 40, y + 40),
        Some(0x0000_00FF),
        "stale generation не должен применяться"
    );

    client
        .shm_commit(41, generation2, &[(0, 0, width, height)])
        .expect("shm commit 2");
    assert!(
        wait_for(Duration::from_secs(3), || server
            .control
            .sample_pixel(x + 40, y + 40)
            == Some(0x0000_00FF)),
        "новый generation не применился"
    );

    // Damage-only: меняем один пиксель и коммитим только его прямоугольник.
    let mut data3 = data2.clone();
    set_xrgb(&mut data3, width, 50, 50, 0x00FF_FF00);
    write_memfd(&fd2, &data3);
    client
        .shm_commit(41, generation2, &[(50, 50, 1, 1)])
        .expect("partial commit");
    assert!(
        wait_for(Duration::from_secs(3), || server
            .control
            .sample_pixel(x + 50, y + 50)
            == Some(0x00FF_FF00)),
        "частичный damage не применился"
    );
    assert_eq!(
        server.control.sample_pixel(x + 20, y + 20),
        Some(0x0000_FF00),
        "остальная поверхность не должна меняться"
    );

    server.control.shutdown();
    server.join().expect("сервер завершился");
}
