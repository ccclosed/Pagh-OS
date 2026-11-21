//! Контракт окна kernel-session (`/dev/vtfb1` + `/dev/vtinput1`) без ядра.
//!
//! Здесь проверяется ровно то, что обещает режим: `generation`-гейт, блит
//! (1:1 / downscale / letterbox), честные ошибки устройств и завершения
//! сессии, VT-кодирование клавиш и `read()`-fallback.  Mock-устройство
//! подменяет поверхность ядра через публичные трейты
//! [`pagh_terminal::kernelsession::FrameSource`] и
//! [`pagh_terminal::kernelsession::InputSink`] — на хосте `/dev/vtfb1` нет,
//! поэтому без абстракции этот режим нельзя было бы проверить вообще.
//!
//! Отдельно проверяется поведение самого бинаря: `--help` называет режим, а
//! отсутствие устройств даёт честную строку в stderr и откат на pipe-сессию.

use std::io::{self, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use pagh_terminal::kernelsession::{
    buttons_to_mask, encode_key_bytes, encode_mouse_event, encode_snapshot, fit_placement,
    is_missing_device, is_session_end, render_notice, solid_pixels, wheel_from_axis,
    window_to_surface, Draw, FileInput, FitPlacement, FrameSource, InputSink, KernelFrame,
    KeyForwarder, MouseEvent, MouseForwarder, ReadFrameSource, VtFbError, VtFbInfo, MOUSE_BTN_LEFT,
    MOUSE_BTN_MIDDLE, MOUSE_BTN_RIGHT, MOUSE_EVENT_LEN,
};
use pagh_terminal::protocol::{self, configure_flags, welcome_flags, Message};
use pagh_terminal::render::{Canvas, Theme};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_pagh-terminal")
}

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "pagh-term-kernelsession-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn info(width: u32, height: u32, generation: u64) -> VtFbInfo {
    VtFbInfo {
        width,
        height,
        stride: width * 4,
        bpp: 32,
        generation,
    }
}

/// Mock-поверхность сессии ядра: пиксели + generation в памяти хоста.
struct MockFb {
    info: VtFbInfo,
    pixels: Vec<u8>,
    polls: usize,
    /// Что вернуть из `poll()` вместо кадра: `ended`, `io` или `short`.
    failure: Option<&'static str>,
    /// Сколько `poll()`-ов отдать кадр до отказа (0 — отказ сразу).
    fail_after: usize,
}

impl MockFb {
    fn new(info: VtFbInfo, rgb: (u8, u8, u8)) -> MockFb {
        MockFb {
            pixels: solid_pixels(&info, rgb),
            info,
            polls: 0,
            failure: None,
            fail_after: 0,
        }
    }

    /// Опубликовать новый кадр: generation растёт, пиксели меняются.
    fn publish(&mut self, rgb: (u8, u8, u8)) {
        self.info.generation += 1;
        self.pixels = solid_pixels(&self.info, rgb);
    }
}

impl FrameSource for MockFb {
    fn poll(&mut self) -> Result<VtFbInfo, VtFbError> {
        self.polls += 1;
        if self.polls <= self.fail_after {
            return Ok(self.info);
        }
        match self.failure {
            Some("ended") => Err(VtFbError::Ended),
            Some("io") => Err(VtFbError::Io(io::Error::other("device exploded"))),
            _ => Ok(self.info),
        }
    }

    fn with_pixels(&mut self, f: &mut dyn FnMut(&[u8])) -> Result<(), VtFbError> {
        if self.failure == Some("short") {
            let short = self.pixels.len() / 2;
            f(&self.pixels[..short]);
            return Ok(());
        }
        f(&self.pixels);
        Ok(())
    }
}

/// Куда «устройство ввода» складывает байты в тестах.
struct MockInput {
    written: Vec<u8>,
}

impl InputSink for MockInput {
    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.written.extend_from_slice(bytes);
        Ok(())
    }
}

#[test]
fn first_frame_is_blitted_and_an_idle_poll_draws_nothing() {
    let fb = MockFb::new(info(4, 2, 1), (0x10, 0x20, 0x30));
    let mut frame = KernelFrame::open(fb).expect("open");
    let theme = Theme::default();
    let mut canvas = Canvas::new(8, 6, theme.bg);

    assert_eq!(
        frame.draw_if_changed(&mut canvas, &theme).unwrap(),
        Draw::Drawn { generation: 1 }
    );
    // 1:1 по центру: поверхность 4×2 внутри окна 8×6 → x=2, y=2.
    assert_eq!(canvas.pixel(2, 2), [0x10, 0x20, 0x30, 255]);
    assert_eq!(canvas.pixel(5, 3), [0x10, 0x20, 0x30, 255]);
    // Поля — фон темы, а не чужой пиксель.
    assert_eq!(
        canvas.pixel(0, 0),
        [theme.bg.0, theme.bg.1, theme.bg.2, 255]
    );

    let polls_before = frame.info().generation;
    assert_eq!(
        frame.draw_if_changed(&mut canvas, &theme).unwrap(),
        Draw::Unchanged,
        "generation не менялась — кадр перерисовываться не должен"
    );
    assert_eq!(frame.info().generation, polls_before);
}

#[test]
fn invalidate_forces_a_full_repaint_for_the_new_canvas() {
    let fb = MockFb::new(info(3, 3, 5), (1, 2, 3));
    let mut frame = KernelFrame::open(fb).expect("open");
    let theme = Theme::default();
    let mut canvas = Canvas::new(3, 3, theme.bg);
    assert_eq!(
        frame.draw_if_changed(&mut canvas, &theme).unwrap(),
        Draw::Drawn { generation: 5 }
    );
    assert_eq!(
        frame.draw_if_changed(&mut canvas, &theme).unwrap(),
        Draw::Unchanged
    );
    // Ресайз окна/новый memfd: generation та же, но холст пуст — нужен полный
    // блит, иначе окно осталось бы фоном темы.
    frame.invalidate();
    let mut fresh = Canvas::new(3, 3, theme.bg);
    assert_eq!(
        frame.draw_if_changed(&mut fresh, &theme).unwrap(),
        Draw::Drawn { generation: 5 }
    );
    assert_eq!(fresh.pixel(0, 0), [1, 2, 3, 255]);
}

#[test]
fn a_republished_frame_reaches_the_window() {
    let theme = Theme::default();
    let mut canvas = Canvas::new(2, 2, theme.bg);
    {
        let mut frame = KernelFrame::open(MockFb::new(info(2, 2, 10), (0, 0, 0))).expect("open");
        frame.draw_if_changed(&mut canvas, &theme).unwrap();
        assert_eq!(canvas.pixel(0, 0), [0, 0, 0, 255]);
    }

    // Второй кадр: ядро перекрасило сессию и подняло generation.
    let mut fb = MockFb::new(info(2, 2, 10), (0, 0, 0));
    fb.publish((0xAA, 0xBB, 0xCC));
    assert_eq!(fb.info.generation, 11);
    let mut frame = KernelFrame::open(fb).expect("reopen");
    let mut canvas = Canvas::new(2, 2, theme.bg);
    assert_eq!(
        frame.draw_if_changed(&mut canvas, &theme).unwrap(),
        Draw::Drawn { generation: 11 }
    );
    assert_eq!(canvas.pixel(1, 1), [0xAA, 0xBB, 0xCC, 255]);
}

#[test]
fn a_small_window_shows_the_whole_frame_scaled_down() {
    let fb = MockFb::new(info(16, 8, 1), (0x33, 0x44, 0x55));
    let mut frame = KernelFrame::open(fb).expect("open");
    let theme = Theme::default();
    let mut canvas = Canvas::new(8, 4, theme.bg);
    assert_eq!(
        frame.draw_if_changed(&mut canvas, &theme).unwrap(),
        Draw::Drawn { generation: 1 }
    );
    // Пропорции сохранены (16:8 → 8:4), отступов нет: кадр занимает окно.
    let place = fit_placement(16, 8, 8, 4).unwrap();
    assert_eq!(
        place,
        FitPlacement {
            x: 0,
            y: 0,
            width: 8,
            height: 4
        }
    );
    for (x, y) in [(0, 0), (7, 0), (0, 3), (7, 3)] {
        assert_eq!(
            canvas.pixel(x, y),
            [0x33, 0x44, 0x55, 255],
            "downscale обязан покрыть всё окно"
        );
    }
}

#[test]
fn session_end_is_reported_honestly_and_not_masked() {
    let mut fb = MockFb::new(info(2, 2, 1), (0, 0, 0));
    fb.failure = Some("ended");
    // Первый poll (open) отдаёт кадр: сессия завершается уже внутри цикла.
    fb.fail_after = 1;
    let mut frame = KernelFrame::open(fb).expect("open passes before the failure");
    let theme = Theme::default();
    let mut canvas = Canvas::new(4, 4, theme.bg);
    match frame.draw_if_changed(&mut canvas, &theme) {
        Err(error) if error.is_ended() => {
            assert!(
                error.message().contains("relaunch"),
                "сообщение обязано говорить, как переподключиться: {}",
                error.message()
            );
        }
        other => panic!("expected Ended, got {other:?}"),
    }
}

#[test]
fn device_errors_are_device_errors() {
    let mut fb = MockFb::new(info(2, 2, 1), (0, 0, 0));
    fb.failure = Some("io");
    fb.fail_after = 1;
    let mut frame = KernelFrame::open(fb).expect("open");
    let theme = Theme::default();
    let mut canvas = Canvas::new(4, 4, theme.bg);
    match frame.draw_if_changed(&mut canvas, &theme) {
        Err(VtFbError::Io(error)) => assert!(error.to_string().contains("device exploded")),
        other => panic!("expected Io, got {other:?}"),
    }
}

#[test]
fn a_short_frame_is_an_error_not_a_partial_picture() {
    let mut fb = MockFb::new(info(4, 4, 1), (0, 0, 0));
    fb.failure = Some("short");
    fb.fail_after = 1;
    let mut frame = KernelFrame::open(fb).expect("open");
    let theme = Theme::default();
    let mut canvas = Canvas::new(4, 4, theme.bg);
    match frame.draw_if_changed(&mut canvas, &theme) {
        Err(VtFbError::ShortRead { need, have }) => {
            assert_eq!(need, 4 * 4 * 4);
            assert_eq!(have, need / 2);
        }
        other => panic!("expected ShortRead, got {other:?}"),
    }
}

#[test]
fn read_fallback_tracks_generation_on_a_plain_file() {
    let dir = temp_dir("readfb");
    let path = dir.join("vtfb1");
    let first = info(3, 2, 1);
    std::fs::write(
        &path,
        encode_snapshot(first, &solid_pixels(&first, (0x01, 0x02, 0x03))),
    )
    .unwrap();

    let source = ReadFrameSource::open(&path).expect("open snapshot");
    let mut frame = KernelFrame::open(source).expect("kernel frame");
    let theme = Theme::default();
    let mut canvas = Canvas::new(3, 2, theme.bg);
    assert_eq!(
        frame.draw_if_changed(&mut canvas, &theme).unwrap(),
        Draw::Drawn { generation: 1 }
    );
    assert_eq!(canvas.pixel(2, 1), [0x01, 0x02, 0x03, 255]);

    // Ядро переписало поверхность: заголовок и пиксели одним снимком.
    let second = info(3, 2, 2);
    std::fs::write(
        &path,
        encode_snapshot(second, &solid_pixels(&second, (0xF0, 0xE0, 0xD0))),
    )
    .unwrap();
    assert_eq!(
        frame.draw_if_changed(&mut canvas, &theme).unwrap(),
        Draw::Drawn { generation: 2 }
    );
    assert_eq!(canvas.pixel(0, 0), [0xF0, 0xE0, 0xD0, 255]);
    assert_eq!(
        frame.draw_if_changed(&mut canvas, &theme).unwrap(),
        Draw::Unchanged
    );

    // Обрезанный снимок — честная ошибка, а не наполовину нарисованный кадр.
    // Generation выше прежней, иначе клиент даже не станет читать пиксели.
    let truncated = info(3, 2, 3);
    std::fs::write(
        &path,
        &encode_snapshot(truncated, &solid_pixels(&truncated, (0, 0, 0)))[..30],
    )
    .unwrap();
    match frame.draw_if_changed(&mut canvas, &theme) {
        Err(VtFbError::ShortRead { .. }) => {}
        other => panic!("expected ShortRead, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn keys_become_vt_bytes_for_the_kernel_input() {
    let mut forwarder = KeyForwarder::new(MockInput {
        written: Vec::new(),
    });
    use pagh_terminal::keys::evdev;
    // PDP/1 modifier bits (same wire values as `protocol::mods`).
    const MOD_CTRL: u32 = 1 << 1;

    // Нажатие printable-клавиши.
    assert_eq!(
        forwarder
            .handle_key(evdev::KEY_H, true, 0)
            .expect("write h"),
        1
    );
    // Release ничего не пишет: события отпускания kernel-shell'у не нужны.
    assert_eq!(
        forwarder
            .handle_key(evdev::KEY_H, false, 0)
            .expect("release"),
        0
    );
    // Enter — CR, как в существующем режиме `/dev/vtinput`.
    assert_eq!(
        forwarder
            .handle_key(evdev::KEY_ENTER, true, 0)
            .expect("enter"),
        1
    );
    // Ctrl+C — байт 0x03 (сигнал доставляет само ядро, клиент не имитирует).
    assert_eq!(
        forwarder
            .handle_key(evdev::KEY_C, true, MOD_CTRL)
            .expect("ctrl-c"),
        1
    );
    assert_eq!(forwarder.sink().written, b"h\r\x03");
    // И та же кодировка доступна без трейта — ею пользуется окно.
    assert_eq!(encode_key_bytes(evdev::KEY_H, true, 0), b"h");
}

#[test]
fn file_input_writes_the_bytes_it_is_given() {
    let dir = temp_dir("input");
    let path = dir.join("vtinput1");
    std::fs::write(&path, b"").unwrap();
    let mut sink = FileInput::open(&path).expect("open input file");
    sink.send(b"echo hi\r").expect("send");
    sink.send(b"x").expect("send again");
    assert_eq!(std::fs::read(&path).unwrap(), b"echo hi\rx");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_notice_is_rendered_into_the_window() {
    let theme = Theme::default();
    let mut canvas = Canvas::new(320, 120, theme.bg);
    let blank = canvas.pixel(100, 60);
    render_notice(
        &mut canvas,
        &theme,
        &[
            "[kernel session] the shell session has ended.",
            "",
            "Relaunch to reconnect.",
        ],
    );
    let changed = (0..canvas.width)
        .flat_map(|x| (0..canvas.height).map(move |y| (x, y)))
        .filter(|&(x, y)| canvas.pixel(x, y) != blank)
        .count();
    assert!(
        changed > 50,
        "сообщение обязано быть видимым (изменено {changed} px)"
    );
}

#[test]
fn errno_classification_matches_the_device_contract() {
    assert!(is_session_end(&io::Error::from_raw_os_error(19)));
    assert!(is_missing_device(&io::Error::from_raw_os_error(2)));
    assert!(!is_missing_device(&io::Error::from_raw_os_error(13)));
}

#[test]
fn help_names_the_kernel_session_mode_and_its_devices() {
    let help = Command::new(binary()).arg("--help").output().unwrap();
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    for needle in [
        "KERNEL SHELL SESSION",
        "/dev/vtfb1",
        "/dev/vtinput1",
        "--vt-fb",
        "--vt-trace",
        "--log-file",
        "--pipe-shell",
        "--demo-shell",
        "no PTY/termios",
    ] {
        assert!(text.contains(needle), "help must mention {needle:?}");
    }
}

/// FPDP/1-заглушка: hello → welcome (без ShmSurface) → create → configure/focus
/// → один commit. Дальше тест сам решает, послать close.
fn spawn_fake_desktop(
    socket: &Path,
) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Receiver<()>) {
    let listener = UnixListener::bind(socket).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        match protocol::read_message(&mut stream).unwrap() {
            Message::Hello { app, .. } => assert_eq!(app, "pagh-terminal"),
            other => panic!("expected hello, got {other:?}"),
        }
        stream
            .write_all(
                &Message::Welcome {
                    version: 1,
                    client_id: 7,
                    width: 1280,
                    height: 720,
                    flags: welcome_flags::FORMAT_XRGB8888 | welcome_flags::FORMAT_RGB565,
                }
                .encode(),
            )
            .unwrap();
        match protocol::read_message(&mut stream).unwrap() {
            Message::Create { window_id, .. } => assert_eq!(window_id, 101),
            other => panic!("expected create, got {other:?}"),
        }
        for message in [
            Message::Focus {
                window_id: 101,
                focused: true,
            },
            Message::Configure {
                window_id: 101,
                x: 0,
                y: 0,
                width: 320,
                height: 200,
                flags: configure_flags::FOCUSED | configure_flags::RESIZABLE,
            },
        ] {
            stream.write_all(&message.encode()).unwrap();
        }
        // Первый кадр: у fallback-сессии это prompt userspace-shell'а.
        match protocol::read_message(&mut stream).unwrap() {
            Message::Commit { .. } | Message::ShmOffer { .. } => {}
            other => panic!("expected a frame, got {other:?}"),
        }
        let _ = tx.send(());
        // Композитор закрывает окно (как кнопкой): клиент обязан завершиться
        // сам, иначе `Command::output()` в тесте повис бы навсегда.
        stream
            .write_all(
                &Message::Close {
                    window_id: 101,
                    reason: protocol::close_reason::SERVER,
                }
                .encode(),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
    });
    (handle, rx)
}

#[test]
fn missing_kernel_devices_fall_back_to_the_pipe_shell_session() {
    let dir = temp_dir("fallback");
    let socket = dir.join("desktop.sock");
    let (server, ready) = spawn_fake_desktop(&socket);
    let missing_fb = dir.join("no-such-vtfb1");
    let missing_input = dir.join("no-such-vtinput1");

    let out = Command::new(binary())
        .args(["--socket"])
        .arg(&socket)
        .args(["--size", "320x200"])
        .arg("--vt-fb")
        .arg(&missing_fb)
        .arg("--vt-input")
        .arg(&missing_input)
        .stdout(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("falling back to the private userspace shell session"),
        "отсутствие устройства обязано быть названо честно: {stderr}"
    );
    assert!(
        !stderr.contains("kernel session unavailable"),
        "отсутствие устройства — не ошибка устройства: {stderr}"
    );
    assert!(ready.recv_timeout(Duration::from_secs(10)).is_ok());
    server.join().expect("fake compositor");
    assert!(
        out.status.success(),
        "fallback-клиент обязан штатно закрыться по close: {:?}",
        out.status
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Диагностика режима должна уметь уходить в файл: desktop запускает клиента
/// `--launch-target` в фоновой группе консоли, где TOSTOP останавливает первую
/// же запись в stderr (SIGTTOU). `--log-file` — штатный путь для такого запуска.
#[test]
fn log_file_receives_the_diagnostics_instead_of_stderr() {
    let dir = temp_dir("logfile");
    let socket = dir.join("desktop.sock");
    let (server, ready) = spawn_fake_desktop(&socket);
    let log = dir.join("client.log");
    let missing_fb = dir.join("no-such-vtfb1");
    let missing_input = dir.join("no-such-vtinput1");

    let out = Command::new(binary())
        .args(["--socket"])
        .arg(&socket)
        .args(["--size", "320x200"])
        .arg("--vt-fb")
        .arg(&missing_fb)
        .arg("--vt-input")
        .arg(&missing_input)
        .arg("--log-file")
        .arg(&log)
        .stdout(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let text = std::fs::read_to_string(&log).expect("log file");
    assert!(
        text.contains("is not present") && text.contains("falling back"),
        "the kernel-session note must go to the log file: {text:?}"
    );
    assert!(
        text.starts_with("pagh-terminal: "),
        "log lines keep the client prefix: {text:?}"
    );
    assert!(
        !stderr.contains("is not present"),
        "with --log-file that line must not reach stderr: {stderr:?}"
    );
    assert!(ready.recv_timeout(Duration::from_secs(10)).is_ok());
    server.join().expect("fake compositor");
    assert!(
        out.status.success(),
        "fallback client must exit cleanly: {:?}",
        out.status
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Фатальная ошибка тоже обязана уходить в `--log-file`: у приложения,
/// запущенного композитором, запись в stderr останавливает его SIGTTOU, и
/// иначе сбой старта («неизвестный аргумент») не увидел бы никто.
/// Второй запуск не должен затирать строки первого: `O_APPEND` в этом
/// Linux-слое не реализован, поэтому клиент ищет конец файла сам.
#[test]
fn the_log_file_keeps_earlier_runs() {
    let dir = temp_dir("append");
    let log = dir.join("client.log");
    for _ in 0..2 {
        let out = Command::new(binary())
            .args(["--no-such-flag"])
            .arg("--log-file")
            .arg(&log)
            .stdout(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
    }
    let text = std::fs::read_to_string(&log).expect("log file");
    assert_eq!(
        text.matches("unknown argument").count(),
        2,
        "both runs must be preserved (append, not overwrite): {text:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fatal_errors_go_to_the_log_file_when_one_is_given() {
    let dir = temp_dir("fatal");
    let log = dir.join("client.log");
    let out = Command::new(binary())
        .args(["--no-such-flag"])
        .arg("--log-file")
        .arg(&log)
        .stdout(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "unknown argument must be fatal");
    let text = std::fs::read_to_string(&log).expect("fatal error must land in the log file");
    assert!(
        text.contains("unknown argument") && text.starts_with("pagh-terminal: "),
        "unexpected log content: {text:?}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("unknown argument"),
        "the fatal line must not also hit stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_regular_file_surface_uses_read_fallback_and_reports_a_missing_compositor() {
    let dir = temp_dir("readfallback");
    let surface = dir.join("vtfb1");
    let first = info(8, 6, 1);
    std::fs::write(
        &surface,
        encode_snapshot(first, &solid_pixels(&first, (0x20, 0x30, 0x40))),
    )
    .unwrap();
    let input = dir.join("vtinput1");
    std::fs::write(&input, b"").unwrap();

    let out = Command::new(binary())
        .args(["--socket"])
        .arg(dir.join("no-desktop.sock"))
        .args(["--size", "320x200"])
        .arg("--vt-fb")
        .arg(&surface)
        .arg("--vt-input")
        .arg(&input)
        .stdout(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "ошибка обязана быть видимой");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("read() snapshots"),
        "ioctl недоступен — обязан быть назван read()-fallback: {stderr}"
    );
    assert!(
        stderr.contains("kernel session 8x6"),
        "геометрия поверхности обязана быть в логе: {stderr}"
    );
    assert!(
        stderr.contains("no-desktop.sock"),
        "причина отказа композитора обязана быть названа: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── указатель сессии: /dev/vtmouse{N} ─────────────────────────────────────────

#[test]
fn mouse_event_is_sixteen_little_endian_bytes() {
    let event = MouseEvent {
        x: 1234,
        y: -5,
        buttons: MOUSE_BTN_LEFT | MOUSE_BTN_RIGHT,
        wheel: -3,
    };
    let bytes = encode_mouse_event(&event);
    assert_eq!(bytes.len(), MOUSE_EVENT_LEN);
    assert_eq!(i32::from_le_bytes(bytes[0..4].try_into().unwrap()), 1234);
    assert_eq!(i32::from_le_bytes(bytes[4..8].try_into().unwrap()), -5);
    assert_eq!(
        u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
        MOUSE_BTN_LEFT | MOUSE_BTN_RIGHT
    );
    assert_eq!(i32::from_le_bytes(bytes[12..16].try_into().unwrap()), -3);
}

#[test]
fn pdp_button_mask_maps_to_the_kernels_button_bits() {
    // PDP/1: LEFT = bit0, MIDDLE = bit1, RIGHT = bit2. Ядро: LEFT=1, RIGHT=2,
    // MIDDLE=4. Ошибка здесь — «нажал левую, выделилась правая».
    assert_eq!(
        buttons_to_mask(protocol::buttons::MASK_LEFT),
        MOUSE_BTN_LEFT
    );
    assert_eq!(
        buttons_to_mask(protocol::buttons::MASK_RIGHT),
        MOUSE_BTN_RIGHT
    );
    assert_eq!(
        buttons_to_mask(protocol::buttons::MASK_MIDDLE),
        MOUSE_BTN_MIDDLE
    );
    assert_eq!(
        buttons_to_mask(protocol::buttons::MASK_LEFT | protocol::buttons::MASK_RIGHT),
        MOUSE_BTN_LEFT | MOUSE_BTN_RIGHT
    );
    assert_eq!(buttons_to_mask(0), 0);
}

#[test]
fn wheel_axis_is_forwarded_unchanged() {
    // `Axis.vertical` is the kernel's own REL_WHEEL value (its PS/2 decoder
    // produced it); /dev/vtmouse wants exactly that. Inverting here made
    // wheel-up scroll towards newer text.
    assert_eq!(wheel_from_axis(1), 1);
    assert_eq!(wheel_from_axis(-1), -1);
    assert_eq!(wheel_from_axis(0), 0);
    assert_eq!(wheel_from_axis(3), 3);
}

#[test]
fn window_coordinates_map_back_to_surface_pixels() {
    let surface = info(1280, 800, 1);
    // 1:1 по центру: поверхность 1280×800 в окне 1400×900 → x=60, y=50.
    let place = fit_placement(1280, 800, 1400, 900).expect("fit");
    assert_eq!(
        (place.x, place.y, place.width, place.height),
        (60, 50, 1280, 800)
    );
    assert_eq!(window_to_surface(60, 50, &surface, &place), (0, 0));
    assert_eq!(
        window_to_surface(60 + 1279, 50 + 799, &surface, &place),
        (1279, 799)
    );
    // Точка в поле (левее кадра) прижимается к краю, а не уезжает в минус.
    assert_eq!(window_to_surface(0, 0, &surface, &place), (0, 0));

    // Downscale: окно 640×400 показывает поверхность 1280×800 вдвое меньше.
    let small = fit_placement(1280, 800, 640, 400).expect("fit");
    assert_eq!(
        (small.x, small.y, small.width, small.height),
        (0, 0, 640, 400)
    );
    assert_eq!(window_to_surface(0, 0, &surface, &small), (0, 0));
    assert_eq!(window_to_surface(639, 399, &surface, &small), (1278, 798));
    assert_eq!(window_to_surface(320, 200, &surface, &small), (640, 400));
}

#[test]
fn mouse_forwarder_sends_position_buttons_and_wheel() {
    let surface = info(100, 50, 1);
    let place = fit_placement(100, 50, 100, 50).expect("fit");
    let mut forwarder = MouseForwarder::new(
        MockInput {
            written: Vec::new(),
        },
        &surface,
        place,
    );
    forwarder.motion(10, 20, 0).unwrap();
    forwarder
        .button(10, 20, protocol::buttons::MASK_LEFT)
        .unwrap();
    forwarder
        .axis(10, 20, protocol::buttons::MASK_LEFT, 2)
        .unwrap();
    let written = &forwarder.sink().written;
    assert_eq!(written.len(), 3 * MOUSE_EVENT_LEN);
    let first = &written[..MOUSE_EVENT_LEN];
    assert_eq!(i32::from_le_bytes(first[0..4].try_into().unwrap()), 10);
    assert_eq!(i32::from_le_bytes(first[4..8].try_into().unwrap()), 20);
    assert_eq!(u32::from_le_bytes(first[8..12].try_into().unwrap()), 0);
    assert_eq!(i32::from_le_bytes(first[12..16].try_into().unwrap()), 0);
    let second = &written[MOUSE_EVENT_LEN..2 * MOUSE_EVENT_LEN];
    assert_eq!(
        u32::from_le_bytes(second[8..12].try_into().unwrap()),
        MOUSE_BTN_LEFT
    );
    let third = &written[2 * MOUSE_EVENT_LEN..];
    assert_eq!(i32::from_le_bytes(third[12..16].try_into().unwrap()), 2);
}

#[test]
fn mouse_forwarder_reports_a_broken_session_sink() {
    struct Failing;
    impl InputSink for Failing {
        fn send(&mut self, _bytes: &[u8]) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(19))
        }
    }
    let surface = info(10, 10, 1);
    let place = fit_placement(10, 10, 10, 10).unwrap();
    let mut forwarder = MouseForwarder::new(Failing, &surface, place);
    let error = forwarder.motion(1, 1, 0).unwrap_err();
    assert!(is_session_end(&error));
}

#[test]
fn help_names_the_pointer_device_and_session_scan() {
    let out = Command::new(binary()).arg("--help").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("--vt-mouse"),
        "help must name the pointer device: {text}"
    );
    assert!(
        text.contains("first free /dev/vtfbN"),
        "help must explain session auto-attach: {text}"
    );
}

/// Композитор для kernel-session: окно обязано получить id, производный от
/// pid (фиксированный 101 мог остаться занятым окном прошлого запуска).
fn spawn_fake_desktop_kernel_id(socket: &Path) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        match protocol::read_message(&mut stream).unwrap() {
            Message::Hello { .. } => {}
            other => panic!("expected hello, got {other:?}"),
        }
        stream
            .write_all(
                &Message::Welcome {
                    version: 1,
                    client_id: 9,
                    width: 1280,
                    height: 720,
                    flags: welcome_flags::FORMAT_XRGB8888,
                }
                .encode(),
            )
            .unwrap();
        let window_id = match protocol::read_message(&mut stream).unwrap() {
            Message::Create { window_id, .. } => {
                assert_ne!(window_id, 101, "kernel mode must not use the fixed demo id");
                assert!(
                    (1000..=91_000).contains(&window_id),
                    "id derives from pid: {window_id}"
                );
                window_id
            }
            other => panic!("expected create, got {other:?}"),
        };
        for message in [
            Message::Focus {
                window_id,
                focused: true,
            },
            Message::Configure {
                window_id,
                x: 0,
                y: 0,
                width: 320,
                height: 200,
                flags: configure_flags::FOCUSED,
            },
        ] {
            stream.write_all(&message.encode()).unwrap();
        }
        // Дать клиенту отправить хотя бы один кадр, затем закрыть окно.
        match protocol::read_message(&mut stream).unwrap() {
            Message::Commit { .. } | Message::ShmOffer { .. } => {}
            other => panic!("expected a frame, got {other:?}"),
        }
        stream
            .write_all(
                &Message::Close {
                    window_id,
                    reason: protocol::close_reason::SERVER,
                }
                .encode(),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
    })
}

#[test]
fn kernel_mode_uses_a_pid_derived_window_id() {
    let dir = temp_dir("dupid");
    let socket = dir.join("desktop.sock");
    let surface = dir.join("vtfb1");
    let info = info(8, 6, 1);
    std::fs::write(
        &surface,
        encode_snapshot(info, &solid_pixels(&info, (0x20, 0x30, 0x40))),
    )
    .unwrap();
    let input = dir.join("vtinput1");
    std::fs::write(&input, b"").unwrap();
    let server = spawn_fake_desktop_kernel_id(&socket);
    let out = Command::new(binary())
        .args(["--socket"])
        .arg(&socket)
        .args(["--size", "320x200"])
        .arg("--vt-fb")
        .arg(&surface)
        .arg("--vt-input")
        .arg(&input)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    server.join().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the client must exit cleanly on Close: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
