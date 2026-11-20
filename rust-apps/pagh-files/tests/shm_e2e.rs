//! E2E: реальный бинарник `pagh-files` против тестового PDP/1-сервера,
//! который объявляет `ShmSurface` и принимает memfd через `SCM_RIGHTS`.
//!
//! Проверяется клиентская половина §4 C1–C3 `docs/desktop-render-spec.md`:
//! окно Files рендерится прямо в mmap, по сокету идут только damage-метаданные,
//! `configure` пересоздаёт memfd/поколение, старое поколение release'ится,
//! а трассировка PDP/1 не содержит пиксельного payload.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pagh_ui::protocol::{self, close_reason, FrameFormat, Header, Message};
use pagh_ui::shm::{recv_fd_payload, MappedBuffer};

const W: u32 = 640;
const H: u32 = 400;
const W2: u32 = 720;
const H2: u32 = 460;

fn socket_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pagh-files-shm-{tag}-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn read_plain(stream: &mut UnixStream) -> (Header, Message) {
    let mut header = [0u8; protocol::HEADER_LEN];
    stream
        .read_exact(&mut header)
        .unwrap_or_else(|error| panic!("header: {error}"));
    let header = Header::parse(&header).unwrap();
    let mut payload = vec![0u8; header.length];
    stream
        .read_exact(&mut payload)
        .unwrap_or_else(|error| panic!("payload: {error}"));
    let message = Message::decode_with_flags(header.kind, header.flags, &payload)
        .unwrap_or_else(|error| panic!("decode: {error}"));
    (header, message)
}

/// Прочитать `SHM_OFFER` вместе с fd. Клиент ждёт `SHM_READY`, поэтому после
/// offer в потоке нет других сообщений — recvmsg получает ровно его.
fn read_offer(stream: &UnixStream) -> (Header, Message, OwnedFd) {
    let mut buffer = [0u8; 256];
    let (read, mut fds) = recv_fd_payload(stream, &mut buffer)
        .unwrap_or_else(|error| panic!("recvmsg(shm_offer): {error}"));
    assert!(read >= protocol::HEADER_LEN, "short offer ({read})");
    let header = Header::parse(&buffer[..read]).expect("offer header");
    assert_eq!(header.kind, protocol::types::SHM_OFFER);
    assert_eq!(header.flags, 0, "SHM_OFFER flags must be zero");
    // Допускаем, что recvmsg вернул offer + начало следующего сообщения:
    // fd относится к первым байтам, а decode ниже читает ровно payload offer.
    let message = Message::decode_with_flags(
        header.kind,
        header.flags,
        &buffer[protocol::HEADER_LEN..protocol::HEADER_LEN + header.length],
    )
    .expect("offer decode");
    assert_eq!(fds.len(), 1, "exactly one SCM_RIGHTS fd");
    (header, message, fds.remove(0))
}

fn wait_child_exit(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    None
}

#[test]
fn files_renders_into_memfd_and_sends_only_damage() {
    let path = socket_path("files");
    let listener = UnixListener::bind(&path).unwrap();
    let trace_path = socket_path("trace");
    let trace_arg = trace_path.clone();

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();

        // hello → welcome с объявленным ShmSurface.
        let (_, hello) = read_plain(&mut stream);
        assert!(matches!(hello, Message::Hello { .. }));
        stream
            .write_all(
                &Message::Welcome {
                    version: protocol::VERSION,
                    client_id: 7,
                    width: 1280,
                    height: 800,
                    flags: protocol::welcome_flags::FORMAT_XRGB8888
                        | protocol::welcome_flags::SHM_SURFACE,
                }
                .encode(),
            )
            .unwrap();

        let (_, create) = read_plain(&mut stream);
        let Message::Create {
            window_id,
            width,
            height,
            ..
        } = create
        else {
            panic!("expected create, got {create:?}");
        };
        assert_eq!((width, height), (W, H));

        // Первый offer: memfd ровно под stride*height, generation 1.
        let (_, offer, fd) = read_offer(&stream);
        let Message::ShmOffer {
            window_id: offer_window,
            width,
            height,
            format,
            stride,
            flags,
        } = offer
        else {
            panic!("expected shm_offer");
        };
        assert_eq!((offer_window, width, height), (window_id, W, H));
        assert_eq!(format, FrameFormat::Xrgb8888.to_wire());
        assert_eq!(stride, W * 4);
        assert_eq!(flags, 0);
        let mapping = MappedBuffer::map_shared_readonly(fd.as_raw_fd(), (stride * height) as usize)
            .expect("server maps client memfd read-only");
        stream
            .write_all(
                &Message::ShmReady {
                    window_id,
                    generation: 1,
                }
                .encode(),
            )
            .unwrap();

        // Первый кадр — полный damage, но без пикселей в сокете.
        let (header, commit) = read_plain(&mut stream);
        assert_eq!(header.flags, protocol::header_flags::SHM);
        let Message::CommitShm {
            window_id: commit_window,
            generation,
            rects,
        } = commit
        else {
            panic!("expected shm commit, got {commit:?}");
        };
        assert_eq!((commit_window, generation), (window_id, 1));
        assert_eq!(rects, vec![(0, 0, W, H)]);
        assert!(
            header.length <= 12 + 16 * 4,
            "pixels leaked into the control socket: {} bytes",
            header.length
        );
        // Files действительно нарисовал окно в mmap: в damage-области есть
        // непрозрачные (не нулевые) пиксели.
        let pixels = mapping.as_slice();
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|px| px[..3] != [0, 0, 0]),
            "shm surface stayed blank"
        );

        // configure: клиент обязан пересоздать memfd и прислать новое поколение.
        stream
            .write_all(
                &Message::Configure {
                    window_id,
                    x: 0,
                    y: 0,
                    width: W2,
                    height: H2,
                    flags: protocol::configure_flags::RESIZABLE,
                }
                .encode(),
            )
            .unwrap();
        let (_, offer2, fd2) = read_offer(&stream);
        let Message::ShmOffer {
            width: w2,
            height: h2,
            stride: stride2,
            ..
        } = offer2
        else {
            panic!("expected second shm_offer");
        };
        assert_eq!((w2, h2), (W2, H2));
        let mapping2 =
            MappedBuffer::map_shared_readonly(fd2.as_raw_fd(), (stride2 * h2) as usize).unwrap();
        stream
            .write_all(
                &Message::ShmReady {
                    window_id,
                    generation: 2,
                }
                .encode(),
            )
            .unwrap();

        // После SHM_READY клиент шлёт release(gen1), затем кадр gen2.
        let mut saw_release = false;
        let mut saw_commit2 = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !(saw_release && saw_commit2) {
            let (header, message) = read_plain(&mut stream);
            match message {
                Message::ShmRelease {
                    window_id: id,
                    generation,
                } => {
                    assert_eq!((id, generation), (window_id, 1));
                    saw_release = true;
                }
                Message::CommitShm {
                    window_id: id,
                    generation,
                    rects,
                } => {
                    assert_eq!((id, generation), (window_id, 2));
                    assert_eq!(rects, vec![(0, 0, W2, H2)]);
                    assert_eq!(header.flags, protocol::header_flags::SHM);
                    assert!(header.length <= 12 + 16 * 4);
                    assert!(
                        mapping2
                            .as_slice()
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .any(|px| px[..3] != [0, 0, 0]),
                        "resized shm surface stayed blank"
                    );
                    saw_commit2 = true;
                }
                other => panic!("unexpected message {other:?}"),
            }
        }
        assert!(saw_release, "old generation was not released");
        assert!(saw_commit2, "no commit for the new generation");

        // Композитор закрывает окно — клиент завершается.
        stream
            .write_all(
                &Message::Close {
                    window_id,
                    reason: close_reason::SERVER,
                }
                .encode(),
            )
            .unwrap();
        let _ = stream.flush();
    });

    let child = Command::new(env!("CARGO_BIN_EXE_pagh-files"))
        .args([
            "--connect",
            path.to_str().unwrap(),
            "--size",
            &format!("{W}x{H}"),
            "--path",
            "/tmp",
        ])
        .env("PAGH_UI_PDP_TRACE", &trace_arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("pagh-files стартует");

    let mut child = child;
    let status = wait_child_exit(&mut child, Duration::from_secs(15))
        .expect("pagh-files не завершился после close");
    assert!(status.success(), "pagh-files завершился с {status:?}");
    server.join().unwrap();

    let trace = std::fs::read_to_string(&trace_path).unwrap();
    let _ = std::fs::remove_file(&trace_path);
    let _ = std::fs::remove_file(&path);
    assert!(
        trace
            .lines()
            .filter(|line| line.starts_with("> shm_offer"))
            .count()
            == 2,
        "trace: {trace}"
    );
    assert!(trace.lines().any(|line| line.starts_with("< shm_ready")));
    assert!(trace.lines().any(|line| line.starts_with("> commit_shm")));
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
}
