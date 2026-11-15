//! Совместимость клиентского кодека `pagh-ui` с серверным кодеком
//! `pagh-desktop`: сообщения, закодированные одной стороной, должна понимать
//! другая. Это защищает от дрейфа двух реализаций одного провода PDP/1.

use pagh_desktop::protocol as server;
use pagh_ui::protocol::{self, FrameFormat, InputEvent, Message, MessageReader};

fn client_to_server(message: Message) {
    let encoded = message.encode();
    let mut decoder = server::Decoder::new();
    decoder.push(&encoded).expect("сервер принимает кадр");
    let decoded = decoder
        .next_message()
        .expect("разбор кадра")
        .expect("полный кадр");
    assert_eq!(
        decoded.name(),
        message.name(),
        "тип сообщения: {:?}",
        message
    );
    assert_same(&message, &decoded);
}

fn server_to_client(message: server::Message) {
    let encoded = message.encode();
    let mut reader = MessageReader::new();
    reader.push(&encoded);
    let decoded = reader.next().expect("разбор кадра").expect("полный кадр");
    assert_eq!(decoded.name(), message.name(), "тип сообщения");
    assert_same(&decoded, &message);
}

/// Сравнить клиентское и серверное представление одного сообщения.
fn assert_same(ours: &Message, theirs: &server::Message) {
    match (ours, theirs) {
        (
            Message::Hello { version, pid, app },
            server::Message::Hello {
                version: v,
                pid: p,
                app: a,
            },
        ) => {
            assert_eq!((version, pid, app), (v, p, a));
        }
        (
            Message::Welcome {
                version,
                client_id,
                width,
                height,
                flags,
            },
            server::Message::Welcome {
                version: v,
                client_id: id,
                width: w,
                height: h,
                flags: f,
            },
        ) => assert_eq!((version, client_id, width, height, flags), (v, id, w, h, f)),
        (
            Message::Create {
                window_id,
                width,
                height,
                format,
                title,
            },
            server::Message::Create {
                window_id: id,
                width: w,
                height: h,
                format: f,
                title: t,
            },
        ) => assert_eq!((window_id, width, height, format, title), (id, w, h, f, t)),
        (
            Message::Configure {
                window_id,
                x,
                y,
                width,
                height,
                flags,
            },
            server::Message::Configure {
                window_id: id,
                x: sx,
                y: sy,
                width: sw,
                height: sh,
                flags: sf,
            },
        ) => assert_eq!(
            (window_id, x, y, width, height, flags),
            (id, sx, sy, sw, sh, sf)
        ),
        (
            Message::Commit {
                window_id,
                width,
                height,
                format,
                stride,
                pixels,
            },
            server::Message::Commit {
                window_id: id,
                width: w,
                height: h,
                format: f,
                stride: s,
                pixels: p,
            },
        ) => assert_eq!(
            (window_id, width, height, format, stride, pixels),
            (id, w, h, f, s, p)
        ),
        (
            Message::Input { window_id, event },
            server::Message::Input {
                window_id: id,
                event: e,
            },
        ) => {
            assert_eq!(window_id, id);
            assert_eq!(format!("{event:?}"), format!("{e:?}"), "input event");
        }
        (
            Message::Focus { window_id, focused },
            server::Message::Focus {
                window_id: id,
                focused: f,
            },
        ) => assert_eq!((window_id, focused), (id, f)),
        (
            Message::Close { window_id, reason },
            server::Message::Close {
                window_id: id,
                reason: r,
            },
        ) => assert_eq!((window_id, reason), (id, r)),
        (
            Message::Damage { window_id, rects },
            server::Message::Damage {
                window_id: id,
                rects: r,
            },
        ) => {
            assert_eq!((window_id, rects), (id, r));
        }
        (
            Message::Error { code, message },
            server::Message::Error {
                code: c,
                message: m,
            },
        ) => assert_eq!((code, message), (c, m)),
        (Message::Ping { nonce }, server::Message::Ping { nonce: n }) => assert_eq!(nonce, n),
        (Message::Pong { nonce }, server::Message::Pong { nonce: n }) => assert_eq!(nonce, n),
        (Message::Launch { app, cwd }, server::Message::Launch { app: a, cwd: c }) => {
            assert_eq!((app, cwd), (a, c))
        }
        (ours, theirs) => panic!("разные типы: {ours:?} vs {theirs:?}"),
    }
}

#[test]
fn client_messages_are_understood_by_the_compositor() {
    let input_events = [
        InputEvent::Motion {
            x: 10,
            y: 20,
            dx: -1,
            dy: 2,
            buttons: 0b1,
        },
        InputEvent::Button {
            x: 3,
            y: 4,
            button: protocol::buttons::RIGHT,
            pressed: true,
            buttons: protocol::buttons::MASK_RIGHT,
        },
        InputEvent::Key {
            code: protocol::key::F4,
            pressed: true,
            modifiers: protocol::mods::ALT | protocol::mods::SHIFT,
        },
        InputEvent::Axis {
            x: 5,
            y: 6,
            horizontal: 0,
            vertical: -1,
            buttons: 0,
        },
    ];
    client_to_server(Message::Hello {
        version: protocol::VERSION,
        pid: 4242,
        app: "pagh-files".into(),
    });
    client_to_server(Message::Create {
        window_id: 1,
        width: 1060,
        height: 640,
        format: FrameFormat::Xrgb8888.to_wire(),
        title: "Домашняя папка — Files".into(),
    });
    for event in input_events {
        client_to_server(Message::Input {
            window_id: 1,
            event,
        });
    }
    client_to_server(Message::Commit {
        window_id: 1,
        width: 2,
        height: 1,
        format: FrameFormat::Rgb565.to_wire(),
        stride: 4,
        pixels: vec![0x00, 0xf8, 0x1f, 0x00],
    });
    client_to_server(Message::Damage {
        window_id: 1,
        rects: vec![(0, 0, 10, 10), (4, 5, 2, 3)],
    });
    client_to_server(Message::Close {
        window_id: 1,
        reason: protocol::close_reason::CLIENT,
    });
    client_to_server(Message::Ping { nonce: 99 });
    client_to_server(Message::Pong { nonce: 100 });
    client_to_server(Message::Launch {
        app: "terminal".into(),
        cwd: "/home/user/Документы".into(),
    });
}

#[test]
fn compositor_messages_are_understood_by_the_client() {
    let input_events = [
        server::InputEvent::Motion {
            x: 7,
            y: 8,
            dx: 1,
            dy: 1,
            buttons: 0,
        },
        server::InputEvent::Button {
            x: 1,
            y: 2,
            button: server::BUTTON_LEFT,
            pressed: false,
            buttons: 0,
        },
        server::InputEvent::Key {
            code: server::key::A,
            pressed: true,
            modifiers: server::MOD_CTRL,
        },
        server::InputEvent::Axis {
            x: 9,
            y: 10,
            horizontal: 1,
            vertical: -1,
            buttons: 0,
        },
    ];
    server_to_client(server::Message::Welcome {
        version: server::VERSION,
        client_id: 77,
        width: 1280,
        height: 800,
        flags: server::WELCOME_FORMAT_XRGB8888 | server::WELCOME_FORMAT_RGB565,
    });
    for event in input_events {
        server_to_client(server::Message::Input {
            window_id: 3,
            event,
        });
    }
    server_to_client(server::Message::Configure {
        window_id: 3,
        x: 96,
        y: 16,
        width: 1000,
        height: 600,
        flags: server::CONFIGURE_FOCUSED,
    });
    server_to_client(server::Message::Focus {
        window_id: 3,
        focused: true,
    });
    server_to_client(server::Message::Close {
        window_id: 3,
        reason: server::CLOSE_SERVER,
    });
    server_to_client(server::Message::Error {
        code: server::ERR_UNSUPPORTED,
        message: "приложение не настроено".into(),
    });
    server_to_client(server::Message::Ping { nonce: 5 });
    server_to_client(server::Message::Pong { nonce: 6 });
    server_to_client(server::Message::Launch {
        app: "files".into(),
        cwd: String::new(),
    });
}
