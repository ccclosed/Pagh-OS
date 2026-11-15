//! Сквозные тесты AF_UNIX-сервера: рукопожатие, окна, кадры, ввод, закрытие,
//! устойчивость к мусору в сокете.

use pagh_desktop::client::Client;
use pagh_desktop::protocol::{key, Message, ERR_BAD_MESSAGE};
use pagh_desktop::server::{unique_socket_path, Server, ServerHandle, ServerOptions};
use pagh_desktop::surface::{PixelFormat, Surface};
use std::io::Write;
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
