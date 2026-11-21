//! Black-box tests for the `pagh-terminal` binary: headless screenshots,
//! script parsing, selection dump and the screenshot-set command.

use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pagh_terminal::protocol::{self, configure_flags, welcome_flags, Message};

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "pagh-term-cli-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_pagh-terminal")
}

#[test]
fn demo_screenshot_is_a_valid_png_with_window_chrome() {
    let dir = temp_dir("demo");
    let out = dir.join("welcome.png");
    let output = Command::new(binary())
        .args(["--headless", "--demo", "01-welcome", "--out"])
        .arg(&out)
        .args(["--print"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Pagh OS 2.24"), "{stdout:?}");
    assert!(out.is_file());
    let png = std::fs::read(&out).unwrap();
    let canvas = pagh_terminal::png::decode(&png).unwrap();
    // 90x28 grid + 2 px padding + 1 px border + a 22 px title strip.
    assert_eq!(canvas.width, 90 * 8 + 4 + 2);
    assert_eq!(canvas.height, 28 * 18 + 4 + 22 + 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn script_mode_no_window_and_resize() {
    let dir = temp_dir("script");
    let script = dir.join("script.bin");
    std::fs::write(
        &script,
        b"\x1b[1;31mred\x1b[0m\r\nplain \xd0\x9f\xd1\x80\xd0\xb8\xd0\xb2\xd0\xb5\xd1\x82\r\n\xe7\x95\x8c",
    )
    .unwrap();
    let out = dir.join("script.png");
    let output = Command::new(binary())
        .args(["--headless", "--script"])
        .arg(&script)
        .args(["--cols", "20", "--rows", "5", "--no-window", "--out"])
        .arg(&out)
        .args(["--print"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("red\nplain Привет\n界"), "{stdout:?}");
    let canvas = pagh_terminal::png::decode(&std::fs::read(&out).unwrap()).unwrap();
    // No window chrome: just the grid plus the client-side viewport padding.
    let pad = pagh_terminal::render::VIEWPORT_PADDING * 2;
    assert_eq!(canvas.width, 20 * 8 + pad);
    assert_eq!(canvas.height, 5 * 18 + pad);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn selection_and_scroll_dump() {
    let dir = temp_dir("selection");
    let script = dir.join("lines.txt");
    let mut text = String::new();
    for i in 0..30 {
        text.push_str(&format!("line {i:02} — Привет\r\n"));
    }
    std::fs::write(&script, text).unwrap();
    let output = Command::new(binary())
        .args(["--headless", "--script"])
        .arg(&script)
        .args(["--cols", "30", "--rows", "8", "--scroll", "4"])
        .args(["--select", "2,0:3,9", "--dump-selection", "--print"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("line "), "{stdout:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn screenshot_set_command_regenerates_all_fixtures() {
    let dir = temp_dir("shots");
    let output = Command::new(binary())
        .args(["--screenshots"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for name in pagh_terminal::demo::SCREENSHOTS {
        let path = dir.join(format!("{name}.png"));
        assert!(path.is_file(), "missing {name}");
        let png = std::fs::read(&path).unwrap();
        assert_eq!(
            png,
            pagh_terminal::demo::demo_png(name, &pagh_terminal::render::Theme::default()).unwrap(),
            "{name} differs from the library render"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_arguments_and_unknown_demo_exit_nonzero() {
    let bad = Command::new(binary())
        .args(["--definitely-not-a-flag"])
        .output()
        .unwrap();
    assert!(!bad.status.success());
    assert_eq!(bad.status.code(), Some(2));

    let unknown = Command::new(binary())
        .args(["--headless", "--demo", "nope", "--print"])
        .output()
        .unwrap();
    assert!(!unknown.status.success());
    assert_eq!(unknown.status.code(), Some(2));
}

#[test]
fn help_and_version_are_available() {
    let help = Command::new(binary()).arg("--help").output().unwrap();
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("--headless"));
    assert!(text.contains("--vt"));
    assert!(text.contains("--screenshots"));

    let version = Command::new(binary()).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("0.1.0"));
}

#[test]
fn live_binary_talks_pdp1_and_exits_on_server_close() {
    let dir = temp_dir("live");
    let socket = dir.join("desktop.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        match protocol::read_message(&mut stream).unwrap() {
            Message::Hello { version, app, .. } => {
                assert_eq!(version, 1);
                assert_eq!(app, "pagh-terminal");
            }
            other => panic!("expected hello, got {other:?}"),
        }
        stream
            .write_all(
                &Message::Welcome {
                    version: 1,
                    client_id: 11,
                    width: 1280,
                    height: 720,
                    flags: welcome_flags::FORMAT_XRGB8888 | welcome_flags::FORMAT_RGB565,
                }
                .encode(),
            )
            .unwrap();
        match protocol::read_message(&mut stream).unwrap() {
            Message::Create {
                window_id,
                width,
                height,
                format,
                ..
            } => {
                assert_eq!(window_id, 101);
                assert_eq!((width, height), (320, 200));
                assert_eq!(format, 1);
            }
            other => panic!("expected create, got {other:?}"),
        }
        for message in [
            Message::Focus {
                window_id: 101,
                focused: true,
            },
            Message::Configure {
                window_id: 101,
                x: 10,
                y: 20,
                width: 320,
                height: 200,
                flags: configure_flags::FOCUSED | configure_flags::RESIZABLE,
            },
        ] {
            stream.write_all(&message.encode()).unwrap();
        }
        // The client commits its initial frame, then reacts to focus/configure
        // and (after the key below) commits again.
        let first = protocol::read_message(&mut stream).unwrap();
        assert!(matches!(first, Message::Commit { .. }));
        stream
            .write_all(
                &Message::Input {
                    window_id: 101,
                    event: protocol::InputEvent::Key {
                        code: pagh_terminal::keys::evdev::KEY_H,
                        pressed: true,
                        modifiers: 0,
                    },
                }
                .encode(),
            )
            .unwrap();
        let second = protocol::read_message(&mut stream).unwrap();
        match second {
            Message::Commit {
                width,
                height,
                format,
                ..
            } => {
                assert_eq!((width, height), (320, 200));
                assert_eq!(format, 1);
            }
            other => panic!("expected second commit, got {other:?}"),
        }
        stream
            .write_all(
                &Message::Close {
                    window_id: 101,
                    reason: protocol::close_reason::SERVER,
                }
                .encode(),
            )
            .unwrap();
        // Give the client time to read close before the test removes the path.
        std::thread::sleep(Duration::from_millis(100));
    });

    let mut child = Command::new(binary())
        .args(["--socket"])
        .arg(&socket)
        .args(["--size", "320x200", "--title", "Test", "--demo-shell"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("live client did not exit after server close");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "live client failed: {status}");
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
