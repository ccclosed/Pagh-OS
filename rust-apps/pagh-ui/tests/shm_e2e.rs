//! E2E клиента `ShmSurface` против тестового PDP/1-сервера, который реально
//! принимает memfd через `SCM_RIGHTS`.
//!
//! Проверяется главное требование §4 C1/C3 `docs/desktop-render-spec.md`:
//! клиент рендерит прямо в mmap, отправляет только damage-метаданные (без
//! пикселей в сокете), сервер видит каждый пиксель через разделяемую память;
//! resize пересоздаёт memfd/поколение, старый release'ится; трассировка PDP/1
//! не содержит пиксельного payload.

use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use pagh_ui::client::DesktopClient;
use pagh_ui::damage::Damage;
use pagh_ui::geom::{Rect, Size};
use pagh_ui::protocol::{self, Header, Message};
use pagh_ui::shm::MappedBuffer;
use pagh_ui::Color;

const W: i32 = 160;
const H: i32 = 90;
const W2: i32 = 220;
const H2: i32 = 120;

const BG: Color = Color::rgb(0x10, 0x20, 0x30);
const BG2: Color = Color::rgb(0x30, 0x20, 0x10);
const DOT: Color = Color::rgb(0xee, 0x11, 0x22);

fn socket_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pagh-ui-shm-e2e-{tag}-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Прочитать одно сообщение, которое может нести SCM_RIGHTS.
fn read_with_fds(stream: &mut std::os::unix::net::UnixStream) -> (Header, Message, Vec<OwnedFd>) {
    let mut buffer = [0u8; 4096];
    let (read, fds) = pagh_ui::shm::recv_fd_payload(stream, &mut buffer)
        .unwrap_or_else(|error| panic!("recvmsg: {error}"));
    assert!(read >= 12, "short PDP frame ({read} bytes)");
    let header = Header::parse(&buffer[..read]).unwrap();
    let message = Message::decode_with_flags(header.kind, header.flags, &buffer[12..read])
        .unwrap_or_else(|error| panic!("decode: {error}"));
    (header, message, fds)
}

/// Скопировать damage-прямоугольники из read-only mapping в RGBA-кадр сервера.
fn blit_damage(
    mapping: &MappedBuffer,
    stride: usize,
    format: pagh_ui::protocol::FrameFormat,
    rects: &[(i32, i32, u32, u32)],
    framebuffer: &mut [u8],
    width: i32,
) {
    let bpp = format.bytes_per_pixel();
    for &(x, y, w, h) in rects {
        for row in y..y + h as i32 {
            for col in x..x + w as i32 {
                let src = row as usize * stride + col as usize * bpp;
                let px = pagh_ui::shm::load_pixel(format, &mapping.as_slice()[src..]);
                let dst = (row as usize * width as usize + col as usize) * 4;
                framebuffer[dst..dst + 4].copy_from_slice(&px);
            }
        }
    }
}

fn rgba_at(framebuffer: &[u8], width: i32, x: i32, y: i32) -> [u8; 4] {
    let offset = (y as usize * width as usize + x as usize) * 4;
    framebuffer[offset..offset + 4].try_into().unwrap()
}

#[test]
fn shm_surface_e2e_without_pixels_on_the_wire() {
    let path = socket_path("shm");
    let listener = UnixListener::bind(&path).unwrap();
    let (result_tx, result_rx) = mpsc::channel::<Vec<u8>>();

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        // hello/welcome: сервер объявляет ShmSurface.
        let hello = protocol::read_message(&mut stream).unwrap();
        assert!(matches!(hello, Message::Hello { .. }));
        stream
            .write_all(
                &Message::Welcome {
                    version: protocol::VERSION,
                    client_id: 42,
                    width: 1280,
                    height: 800,
                    flags: protocol::welcome_flags::FORMAT_XRGB8888
                        | protocol::welcome_flags::FORMAT_RGB565
                        | protocol::welcome_flags::SHM_SURFACE,
                }
                .encode(),
            )
            .unwrap();
        let create = protocol::read_message(&mut stream).unwrap();
        assert!(matches!(create, Message::Create { .. }));

        // Ожидаем SHM_OFFER (fd едет отдельным sendmsg).
        let (header, offer, fds) = read_with_fds(&mut stream);
        assert_eq!(header.kind, protocol::types::SHM_OFFER);
        assert_eq!(header.flags, 0, "SHM_OFFER flags must be 0");
        assert_eq!(fds.len(), 1, "exactly one SCM_RIGHTS fd");
        let Message::ShmOffer {
            window_id,
            width,
            height,
            format,
            stride,
            flags,
        } = offer
        else {
            panic!("expected shm_offer, got {offer:?}");
        };
        assert_eq!((width, height, format, flags), (W as u32, H as u32, 1, 0));
        assert_eq!(stride, W as u32 * 4);
        let mapping =
            MappedBuffer::map_shared_readonly(fds[0].as_raw_fd(), (stride * height) as usize)
                .expect("server maps client memfd read-only");

        // READY generation 1.
        stream
            .write_all(
                &Message::ShmReady {
                    window_id,
                    generation: 1,
                }
                .encode(),
            )
            .unwrap();

        // Первый кадр: полный damage, клиент залил фон.
        let mut framebuffer = vec![0u8; (W * H * 4) as usize];
        let (header, message, fds) = read_with_fds(&mut stream);
        assert!(fds.is_empty(), "shm commit must not carry an fd");
        assert_eq!(header.flags, protocol::header_flags::SHM);
        let Message::CommitShm {
            window_id: id,
            generation,
            rects,
        } = message
        else {
            panic!("expected shm commit, got {message:?}");
        };
        assert_eq!((id, generation), (window_id, 1));
        assert_eq!(rects, vec![(0i32, 0i32, W as u32, H as u32)]);
        assert!(
            header.length < 64,
            "shm commit payload must be metadata only, got {} bytes",
            header.length
        );
        blit_damage(
            &mapping,
            stride as usize,
            pagh_ui::protocol::FrameFormat::Xrgb8888,
            &rects,
            &mut framebuffer,
            W,
        );
        assert_eq!(rgba_at(&framebuffer, W, 10, 10), [BG.r, BG.g, BG.b, 255]);

        // Второй кадр: одна точка — commit несёт 1 прямоугольник.
        let (header, message, _) = read_with_fds(&mut stream);
        assert_eq!(header.flags, protocol::header_flags::SHM);
        let Message::CommitShm {
            generation, rects, ..
        } = message
        else {
            panic!("expected shm commit, got {message:?}");
        };
        assert_eq!(generation, 1);
        assert_eq!(rects, vec![(5i32, 5i32, 1u32, 1u32)]);
        assert!(header.length <= 12 + 16, "pixels leaked into the socket");
        blit_damage(
            &mapping,
            stride as usize,
            pagh_ui::protocol::FrameFormat::Xrgb8888,
            &rects,
            &mut framebuffer,
            W,
        );
        assert_eq!(rgba_at(&framebuffer, W, 5, 5), [DOT.r, DOT.g, DOT.b, 255]);
        assert_eq!(
            rgba_at(&framebuffer, W, 6, 5),
            [BG.r, BG.g, BG.b, 255],
            "damage must not overwrite neighbours"
        );

        // configure resize → новый memfd/поколение.
        stream
            .write_all(
                &Message::Configure {
                    window_id,
                    x: 0,
                    y: 0,
                    width: W2 as u32,
                    height: H2 as u32,
                    flags: protocol::configure_flags::RESIZABLE,
                }
                .encode(),
            )
            .unwrap();
        let (header, offer, fds) = read_with_fds(&mut stream);
        assert_eq!(header.kind, protocol::types::SHM_OFFER);
        assert_eq!(fds.len(), 1);
        let Message::ShmOffer {
            width: w2,
            height: h2,
            stride: stride2,
            ..
        } = offer
        else {
            panic!("expected second shm_offer");
        };
        assert_eq!((w2, h2), (W2 as u32, H2 as u32));
        let mapping2 =
            MappedBuffer::map_shared_readonly(fds[0].as_raw_fd(), (stride2 * h2) as usize).unwrap();
        stream
            .write_all(
                &Message::ShmReady {
                    window_id,
                    generation: 2,
                }
                .encode(),
            )
            .unwrap();

        // SHM_RELEASE(gen=1) и новый кадр gen=2 (порядок не гарантируем — ждём оба).
        let mut released_old = false;
        let mut framebuffer2 = vec![0u8; (W2 * H2 * 4) as usize];
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && (!released_old || framebuffer2.iter().all(|b| *b == 0)) {
            let (header, message, _) = read_with_fds(&mut stream);
            match message {
                Message::ShmRelease { generation: 1, .. } => released_old = true,
                Message::CommitShm {
                    generation, rects, ..
                } => {
                    assert_eq!(generation, 2, "commit must use new generation");
                    assert_eq!(rects, vec![(0i32, 0i32, W2 as u32, H2 as u32)]);
                    blit_damage(
                        &mapping2,
                        stride2 as usize,
                        pagh_ui::protocol::FrameFormat::Xrgb8888,
                        &rects,
                        &mut framebuffer2,
                        W2,
                    );
                }
                other => panic!("unexpected message {other:?} flags={:#x}", header.flags),
            }
        }
        assert!(released_old, "old generation was not released");
        assert_eq!(
            rgba_at(&framebuffer2, W2, 30, 20),
            [BG2.r, BG2.g, BG2.b, 255]
        );

        result_tx.send(framebuffer2).unwrap();
    });

    // ---- клиент ----
    let trace_path = std::env::temp_dir().join(format!(
        "pagh-ui-shm-trace-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::env::set_var("PAGH_UI_PDP_TRACE", &trace_path);
    let mut client = DesktopClient::connect(&path, "shm-e2e").unwrap();
    assert!(client.shm_supported(), "welcome advertises ShmSurface");
    let window = client.open_window(Size::new(W, H), "Shm E2E").unwrap();
    let mut frame = client
        .enable_shm(window, Size::new(W, H))
        .unwrap()
        .expect("server must accept SHM_OFFER");
    assert_eq!(frame.generation, 1);
    assert_eq!(
        frame.canvas.frame_format(),
        Some(protocol::FrameFormat::Xrgb8888)
    );

    frame.canvas.fill(frame.canvas.rect(), BG);
    let damage = Damage::full(frame.rect());
    client.commit_shm(&frame, &damage).unwrap();

    frame.canvas.set_pixel(pagh_ui::Point::new(5, 5), DOT);
    let mut dot = Damage::new(frame.rect());
    dot.add(Rect::new(5, 5, 1, 1));
    client.commit_shm(&frame, &dot).unwrap();

    // Ждём configure и пересоздаём поверхность.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut resized = false;
    while Instant::now() < deadline && !resized {
        for event in client.poll(Duration::from_millis(50)).unwrap() {
            if let pagh_ui::client::Incoming::Configure { size, .. } = event {
                assert_eq!(size, Size::new(W2, H2));
                resized = client.resize_shm(&mut frame, Size::new(W2, H2)).unwrap();
            }
        }
    }
    assert!(
        resized,
        "resize_shm must succeed against a shm-capable server"
    );
    assert_eq!(frame.generation, 2);
    frame.canvas.fill(frame.canvas.rect(), BG2);
    client
        .commit_shm(&frame, &Damage::full(frame.rect()))
        .unwrap();

    // Дать серверу время обработать и получить результат.
    let framebuffer2 = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(framebuffer2.len(), (W2 * H2 * 4) as usize);
    server.join().unwrap();

    // Трассировка: только метаданные, без пиксельного payload.
    let trace = std::fs::read_to_string(&trace_path).unwrap();
    let _ = std::fs::remove_file(&trace_path);
    let shm_commits = trace
        .lines()
        .filter(|line| line.starts_with("> commit_shm"))
        .count();
    assert!(shm_commits >= 3, "trace: {trace}");
    assert!(
        !trace.contains("pixels="),
        "shm trace must not contain pixel payloads: {trace}"
    );
    for line in trace
        .lines()
        .filter(|line| line.starts_with("> commit_shm"))
    {
        let len: usize = line
            .split("len=")
            .nth(1)
            .and_then(|tail| tail.split_whitespace().next())
            .unwrap()
            .parse()
            .unwrap();
        assert!(len <= 12 + 16 * 64, "commit metadata too large: {line}");
    }
    let offers = trace
        .lines()
        .filter(|line| line.starts_with("> shm_offer"))
        .count();
    assert_eq!(offers, 2, "initial offer + resize offer: {trace}");
    assert!(trace.lines().any(|line| line.contains("shm_ready")));
    let _ = std::fs::remove_file(&path);
}
