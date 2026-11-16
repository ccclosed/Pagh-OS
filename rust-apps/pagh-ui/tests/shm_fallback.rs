//! Fallback-путь клиента: если `welcome.flags` не объявляет ShmSurface или
//! окружение отключило shared memory, приложение продолжает слать полный кадр
//! по сокету (v1), и окно работает.
//!
//! Этот файл — отдельный integration-test binary, чтобы тестовый выключатель
//! `PAGH_UI_DISABLE_SHM` не влиял на `shm_e2e.rs` (env общий на процесс).

use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use pagh_ui::client::DesktopClient;
use pagh_ui::geom::Size;
use pagh_ui::protocol::{self, Message};
use pagh_ui::{Canvas, Color};

const W: i32 = 64;
const H: i32 = 48;
const FILL: Color = Color::rgb(1, 2, 3);

fn socket_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pagh-ui-shm-fallback-{tag}-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Сервер без ShmSurface (или с ним): принимает hello/create, затем один
/// full-frame commit и проверяет пиксели.
fn run_server(
    path: &std::path::Path,
    advertise_shm: bool,
) -> (mpsc::Receiver<Vec<u8>>, thread::JoinHandle<()>) {
    let listener = UnixListener::bind(path).unwrap();
    let (result_tx, result_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let hello = protocol::read_message(&mut stream).unwrap();
        assert!(matches!(hello, Message::Hello { .. }));
        let mut flags =
            protocol::welcome_flags::FORMAT_XRGB8888 | protocol::welcome_flags::FORMAT_RGB565;
        if advertise_shm {
            flags |= protocol::welcome_flags::SHM_SURFACE;
        }
        stream
            .write_all(
                &Message::Welcome {
                    version: protocol::VERSION,
                    client_id: 9,
                    width: 1280,
                    height: 800,
                    flags,
                }
                .encode(),
            )
            .unwrap();
        let create = protocol::read_message(&mut stream).unwrap();
        assert!(matches!(create, Message::Create { .. }));

        // Клиент обязан продолжить full-frame commit'ом (fallback).
        let message = protocol::read_message(&mut stream).unwrap();
        let Message::Commit {
            width,
            height,
            format,
            stride,
            pixels,
            ..
        } = message
        else {
            panic!("fallback must use full-frame commit, got {message:?}");
        };
        assert_eq!(
            (width, height, format, stride),
            (W as u32, H as u32, 1, W as u32 * 4)
        );
        assert_eq!(pixels.len(), (W * H * 4) as usize);
        let rgba = protocol::unpack_xrgb8888(&pixels, W as usize, H as usize).unwrap();
        result_tx.send(rgba).unwrap();
    });
    (result_rx, handle)
}

fn draw_and_commit(client: &mut DesktopClient, window: u32) {
    assert!(
        client
            .enable_shm(window, Size::new(W, H))
            .unwrap()
            .is_none(),
        "fallback expected, but the client enabled ShmSurface"
    );
    let mut canvas = Canvas::filled(W, H, Color::BLACK);
    canvas.fill(canvas.rect(), FILL);
    client.commit(window, &canvas).unwrap();
}

#[test]
fn fallback_without_welcome_flag() {
    let path = socket_path("no-flag");
    let (result_rx, handle) = run_server(&path, false);
    let mut client = DesktopClient::connect(&path, "fallback-no-flag").unwrap();
    assert!(!client.shm_supported());
    let window = client.open_window(Size::new(W, H), "Fallback").unwrap();
    draw_and_commit(&mut client, window);
    let rgba = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(&rgba[..4], &[FILL.r, FILL.g, FILL.b, 255]);
    handle.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn fallback_with_env_disable_even_if_server_advertises_shm() {
    std::env::set_var("PAGH_UI_DISABLE_SHM", "1");
    let path = socket_path("env-disable");
    let (result_rx, handle) = run_server(&path, true);
    let mut client = DesktopClient::connect(&path, "fallback-env").unwrap();
    assert!(
        !client.shm_supported(),
        "test hook must force the fallback path"
    );
    let window = client.open_window(Size::new(W, H), "Fallback").unwrap();
    draw_and_commit(&mut client, window);
    let rgba = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(&rgba[..4], &[FILL.r, FILL.g, FILL.b, 255]);
    handle.join().unwrap();
    std::env::remove_var("PAGH_UI_DISABLE_SHM");
    let _ = std::fs::remove_file(&path);
}
