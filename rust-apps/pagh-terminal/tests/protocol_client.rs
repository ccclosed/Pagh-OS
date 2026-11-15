//! End-to-end tests for the PDP/1 client against a fake Unix socket server:
//! handshake, create, commit, configure/focus, input, ping/pong and close.

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use pagh_terminal::cell::Rgb;
use pagh_terminal::client::{Event, SurfaceClient};
use pagh_terminal::protocol::{
    self, buttons, close_reason, configure_flags, mods, types, welcome_flags, FrameFormat,
    InputEvent, Message, MessageReader, VERSION,
};
use pagh_terminal::render::Canvas;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn socket_path() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("pagh-term-pdp-{}-{n}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

fn send(stream: &mut UnixStream, message: &Message) {
    stream.write_all(&message.encode()).unwrap();
}

fn recv(stream: &mut UnixStream) -> Message {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    protocol::read_message(stream).expect("PDP/1 message")
}

#[test]
fn client_handshake_create_commit_input_and_ping() {
    let path = socket_path();
    let listener = UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");

        let hello = recv(&mut stream);
        match hello {
            Message::Hello { version, pid, app } => {
                assert_eq!(version, VERSION);
                assert_eq!(pid, std::process::id());
                assert_eq!(app, "pagh-terminal-test");
            }
            other => panic!("expected hello, got {other:?}"),
        }
        send(
            &mut stream,
            &Message::Welcome {
                version: VERSION,
                client_id: 7,
                width: 1280,
                height: 720,
                flags: welcome_flags::FORMAT_XRGB8888 | welcome_flags::FORMAT_RGB565,
            },
        );

        match recv(&mut stream) {
            Message::Create {
                window_id,
                width,
                height,
                format,
                title,
            } => {
                assert_eq!(window_id, 101);
                assert_eq!((width, height), (320, 200));
                assert_eq!(format, FrameFormat::Xrgb8888.to_wire());
                assert_eq!(title, "Test Window");
            }
            other => panic!("expected create, got {other:?}"),
        }

        send(
            &mut stream,
            &Message::Focus {
                window_id: 101,
                focused: true,
            },
        );
        send(
            &mut stream,
            &Message::Configure {
                window_id: 101,
                x: 10,
                y: 20,
                width: 320,
                height: 200,
                flags: configure_flags::FOCUSED | configure_flags::RESIZABLE,
            },
        );

        match recv(&mut stream) {
            Message::Commit {
                window_id,
                width,
                height,
                format,
                stride,
                pixels,
            } => {
                assert_eq!(window_id, 101);
                assert_eq!((width, height), (320, 200));
                assert_eq!(format, FrameFormat::Xrgb8888.to_wire());
                assert_eq!(stride, 320 * 4);
                assert_eq!(pixels.len(), (320 * 200 * 4) as usize);
                // First pixel Rgb(10,20,30) -> B,G,R,X.
                assert_eq!(&pixels[..4], &[30, 20, 10, 0]);
            }
            other => panic!("expected commit, got {other:?}"),
        }

        // A batch of input events, an unknown future type must never be sent by
        // the server (PDP/1 rejects it), so exercise all four event kinds.
        for event in [
            InputEvent::Motion {
                x: 5,
                y: 6,
                dx: 1,
                dy: 0,
                buttons: 0,
            },
            InputEvent::Button {
                x: 10,
                y: 20,
                button: buttons::LEFT,
                pressed: true,
                buttons: buttons::MASK_LEFT,
            },
            InputEvent::Key {
                code: pagh_terminal::keys::evdev::KEY_A,
                pressed: true,
                modifiers: mods::CTRL,
            },
            InputEvent::Axis {
                x: 1,
                y: 2,
                horizontal: 0,
                vertical: -1,
                buttons: 0,
            },
        ] {
            send(
                &mut stream,
                &Message::Input {
                    window_id: 101,
                    event,
                },
            );
        }
        send(&mut stream, &Message::Ping { nonce: 42 });

        match recv(&mut stream) {
            Message::Pong { nonce } => assert_eq!(nonce, 42),
            other => panic!("expected pong, got {other:?}"),
        }
        match recv(&mut stream) {
            Message::Close { window_id, reason } => {
                assert_eq!(window_id, 101);
                assert_eq!(reason, close_reason::CLIENT);
            }
            other => panic!("expected close, got {other:?}"),
        }
    });

    let mut client = SurfaceClient::connect(&path).unwrap();
    let welcome = client
        .handshake("pagh-terminal-test", Duration::from_secs(2))
        .unwrap();
    assert_eq!(welcome.client_id, 7);
    assert_eq!((welcome.screen_width, welcome.screen_height), (1280, 720));
    assert_eq!(client.format(), FrameFormat::Xrgb8888);

    client.create_window(101, 320, 200, "Test Window").unwrap();
    let mut canvas = Canvas::new(320, 200, Rgb(10, 20, 30));
    canvas.fill_rect(40, 40, 20, 20, Rgb(200, 100, 50));
    client.commit(&canvas).unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        events.extend(client.poll_events(Duration::from_millis(50)).unwrap());
        if events.iter().any(|e| matches!(e, Event::Ping(42))) {
            break;
        }
    }
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Focus {
            window_id: 101,
            focused: true
        }
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Configure(c) if c.width == 320 && c.height == 200 && c.focused()
    )));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::Input(InputEvent::Motion { x: 5, y: 6, .. }))));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Input(InputEvent::Button {
            button: buttons::LEFT,
            pressed: true,
            ..
        })
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Input(InputEvent::Key {
            code: 30,
            pressed: true,
            modifiers,
        }) if *modifiers == mods::CTRL
    )));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::Input(InputEvent::Axis { vertical: -1, .. }))));
    assert!(events.iter().any(|e| matches!(e, Event::Ping(42))));
    client.close().unwrap();
    server.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn rgb565_negotiation_uses_two_byte_pixels() {
    let path = socket_path();
    let listener = UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _hello = recv(&mut stream);
        send(
            &mut stream,
            &Message::Welcome {
                version: VERSION,
                client_id: 3,
                width: 800,
                height: 600,
                flags: welcome_flags::FORMAT_RGB565,
            },
        );
        match recv(&mut stream) {
            Message::Create { format, .. } => {
                assert_eq!(format, FrameFormat::RGB565_WIRE);
            }
            other => panic!("expected create, got {other:?}"),
        }
        match recv(&mut stream) {
            Message::Commit {
                format,
                stride,
                pixels,
                ..
            } => {
                assert_eq!(format, FrameFormat::RGB565_WIRE);
                assert_eq!(stride, 8 * 2);
                assert_eq!(pixels.len(), 8 * 2 * 4);
            }
            other => panic!("expected commit, got {other:?}"),
        }
        send(
            &mut stream,
            &Message::Close {
                window_id: 9,
                reason: close_reason::SERVER,
            },
        );
    });

    let mut client = SurfaceClient::connect(&path).unwrap();
    client
        .handshake("pagh-terminal-test", Duration::from_secs(2))
        .unwrap();
    assert_eq!(client.format(), FrameFormat::Rgb565);
    client.create_window(9, 8, 4, "x").unwrap();
    let canvas = Canvas::new(8, 4, Rgb(255, 0, 0));
    client.commit(&canvas).unwrap();
    let events = client.poll_events(Duration::from_secs(1)).unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        Event::Close {
            window_id: 9,
            reason: close_reason::SERVER
        }
    )));
    server.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn connecting_to_a_missing_socket_is_an_error() {
    let mut path = std::env::temp_dir();
    path.push(format!("pagh-term-missing-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    assert!(SurfaceClient::connect(&path).is_err());
}

#[test]
fn message_reader_handles_fragmented_and_coalesced_frames() {
    let mut wire = Message::Ping { nonce: 11 }.encode();
    wire.extend_from_slice(&Message::Pong { nonce: 22 }.encode());
    let mut reader = MessageReader::new();
    for byte in &wire {
        reader.push(&[*byte]);
    }
    let first = reader.next().unwrap().unwrap();
    assert_eq!(first, Message::Ping { nonce: 11 });
    let second = reader.next().unwrap().unwrap();
    assert_eq!(second, Message::Pong { nonce: 22 });
    assert!(reader.next().unwrap().is_none());
}

#[test]
fn malformed_frame_is_an_io_error_in_poll() {
    let path = socket_path();
    let listener = UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // A complete 12-byte header with a bad magic: the decoder must reject it
        // before waiting for a payload.
        stream
            .write_all(b"NOTPDS1\x01\x00\x00\x00\x00\x00")
            .unwrap();
    });
    let mut client = SurfaceClient::connect(&path).unwrap();
    let error = client.poll_events(Duration::from_millis(200)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    server.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn protocol_types_are_stable() {
    assert_eq!(types::HELLO, 1);
    assert_eq!(types::PONG, 12);
    assert_eq!(FrameFormat::Xrgb8888.to_wire(), 1);
    assert_eq!(FrameFormat::Rgb565.to_wire(), 2);
    let header = Message::Ping { nonce: 0x0102_0304 }.encode();
    assert_eq!(&header[..4], b"PDS1");
    assert_eq!(&header[4..6], &11u16.to_le_bytes());
    assert_eq!(&header[6..8], &[0, 0]);
    assert_eq!(&header[8..12], &4u32.to_le_bytes());
    assert_eq!(&header[12..16], &[4, 3, 2, 1]);
}
