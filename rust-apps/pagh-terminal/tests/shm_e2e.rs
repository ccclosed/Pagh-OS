//! E2E: `pagh-terminal` против тестового PDP/1-сервера с `ShmSurface`.
//!
//! Проверяет §4 C1–C2 `docs/desktop-render-spec.md`: терминал рендерит прямо
//! в memfd, по сокету идут только damage-прямоугольники, ввод и мигание
//! курсора дают частичный damage (не полный кадр), `configure` пересоздаёт
//! поверхность/поколение, старое поколение release'ится.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pagh_terminal::keys::evdev::KEY_A;
use pagh_ui::protocol::{self, close_reason, FrameFormat, Header, Message};
use pagh_ui::shm::{recv_fd_payload, MappedBuffer};

type DamageRect = (i32, i32, u16, u16);

const W: u32 = 640;
const H: u32 = 400;
const W2: u32 = 720;
const H2: u32 = 460;

fn socket_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pagh-terminal-shm-{tag}-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn read_plain(stream: &mut UnixStream) -> (Header, Message) {
    let mut header = [0u8; protocol::HEADER_LEN];
    stream.read_exact(&mut header).expect("header");
    let header = Header::parse(&header).expect("parse header");
    let mut payload = vec![0u8; header.length];
    stream.read_exact(&mut payload).expect("payload");
    let message = Message::decode_with_flags(header.kind, header.flags, &payload)
        .unwrap_or_else(|error| panic!("decode: {error}"));
    (header, message)
}

fn read_offer(stream: &UnixStream) -> (Message, OwnedFd) {
    let mut buffer = [0u8; 256];
    let (read, mut fds) = recv_fd_payload(stream, &mut buffer).expect("recvmsg(shm_offer)");
    assert!(read >= protocol::HEADER_LEN, "short offer");
    let header = Header::parse(&buffer[..read]).expect("offer header");
    assert_eq!(header.kind, protocol::types::SHM_OFFER);
    assert_eq!(header.flags, 0);
    let offer = Message::decode_with_flags(
        header.kind,
        header.flags,
        &buffer[protocol::HEADER_LEN..protocol::HEADER_LEN + header.length],
    )
    .expect("offer decode");
    assert_eq!(fds.len(), 1, "exactly one SCM_RIGHTS fd");
    (offer, fds.remove(0))
}

fn read_commit(stream: &mut UnixStream) -> (Header, u32, Vec<DamageRect>) {
    let (header, message) = read_plain(stream);
    assert_eq!(header.flags, protocol::header_flags::SHM);
    let Message::CommitShm {
        generation, rects, ..
    } = message
    else {
        panic!("expected shm commit, got {message:?}");
    };
    assert!(
        header.length <= 12 + 12 * protocol::MAX_DAMAGE_RECTS,
        "pixels leaked into the socket: {} bytes",
        header.length
    );
    (header, generation, rects)
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
fn terminal_shm_damage_blink_and_resize() {
    let path = socket_path("terminal");
    let listener = UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();

        let (_, hello) = read_plain(&mut stream);
        assert!(matches!(hello, Message::Hello { .. }));
        stream
            .write_all(
                &Message::Welcome {
                    version: protocol::VERSION,
                    client_id: 11,
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
            panic!("expected create");
        };
        assert_eq!((width, height), (W, H));

        let (offer, fd) = read_offer(&stream);
        let Message::ShmOffer {
            width,
            height,
            format,
            stride,
            ..
        } = offer
        else {
            panic!("expected shm_offer");
        };
        assert_eq!((width, height), (W, H));
        assert_eq!(format, FrameFormat::Xrgb8888.to_wire());
        let mapping =
            MappedBuffer::map_shared_readonly(fd.as_raw_fd(), (stride * height) as usize).unwrap();
        stream
            .write_all(
                &Message::ShmReady {
                    window_id,
                    generation: 1,
                }
                .encode(),
            )
            .unwrap();

        // Первый кадр — полный damage.
        let (_, generation, rects) = read_commit(&mut stream);
        assert_eq!(generation, 1);
        assert_eq!(rects, vec![(0, 0, W as u16, H as u16)]);
        assert!(
            mapping
                .as_slice()
                .as_chunks::<4>()
                .0
                .iter()
                .any(|px| px[..3] != [0, 0, 0]),
            "terminal did not render into memfd"
        );

        // Ввод символа: damage должен быть маленьким (текст/курсор), не кадром.
        stream
            .write_all(
                &Message::Input {
                    window_id,
                    event: protocol::InputEvent::Key {
                        code: KEY_A,
                        pressed: true,
                        modifiers: 0,
                    },
                }
                .encode(),
            )
            .unwrap();
        let (_, generation, rects) = read_commit(&mut stream);
        assert_eq!(generation, 1);
        let area: i64 = rects
            .iter()
            .map(|(_, _, w, h)| i64::from(*w) * i64::from(*h))
            .sum();
        assert!(!rects.is_empty(), "typing produced no damage");
        assert!(
            area < i64::from(W) * i64::from(H) / 8,
            "typing produced near-full damage: {rects:?} area={area}"
        );

        // Мигание курсора: приходит кадр с damage порядка одной клетки.
        let mut blink_rects = None;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && blink_rects.is_none() {
            stream
                .set_read_timeout(Some(Duration::from_millis(800)))
                .unwrap();
            let mut header = [0u8; protocol::HEADER_LEN];
            match stream.read_exact(&mut header) {
                Ok(()) => {
                    let header = Header::parse(&header).unwrap();
                    let mut payload = vec![0u8; header.length];
                    stream.read_exact(&mut payload).unwrap();
                    let message =
                        Message::decode_with_flags(header.kind, header.flags, &payload).unwrap();
                    if let Message::CommitShm {
                        generation: 1,
                        rects,
                        ..
                    } = message
                    {
                        blink_rects = Some(rects);
                    }
                }
                Err(_) => break,
            }
        }
        let blink = blink_rects.expect("cursor blink did not produce a shm commit");
        let area: i64 = blink
            .iter()
            .map(|(_, _, w, h)| i64::from(*w) * i64::from(*h))
            .sum();
        assert!(
            area <= 2 * 16 * 32,
            "cursor blink damage is too large: {blink:?} area={area}"
        );

        // configure: новый memfd/gen2, затем release(gen1) и полный кадр gen2.
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
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
        let (offer2, fd2) = read_offer(&stream);
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

        let mut saw_release = false;
        let mut saw_commit2 = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !(saw_release && saw_commit2) {
            let (_, message) = read_plain(&mut stream);
            match message {
                Message::ShmRelease { generation, .. } => {
                    assert_eq!(generation, 1);
                    saw_release = true;
                }
                Message::CommitShm {
                    generation, rects, ..
                } => {
                    assert_eq!(generation, 2);
                    assert_eq!(rects, vec![(0, 0, W2 as u16, H2 as u16)]);
                    assert!(
                        mapping2
                            .as_slice()
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .any(|px| px[..3] != [0, 0, 0]),
                        "resized terminal surface stayed blank"
                    );
                    saw_commit2 = true;
                }
                other => panic!("unexpected message {other:?}"),
            }
        }
        assert!(saw_release, "old generation was not released");
        assert!(saw_commit2, "no commit for generation 2");

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

    let mut child = Command::new(env!("CARGO_BIN_EXE_pagh-terminal"))
        .args([
            "--socket",
            path.to_str().unwrap(),
            "--size",
            &format!("{W}x{H}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("pagh-terminal стартует");
    let status = wait_child_exit(&mut child, Duration::from_secs(20))
        .expect("pagh-terminal не завершился после close");
    assert!(status.success(), "pagh-terminal завершился с {status:?}");
    server.join().unwrap();
    let _ = std::fs::remove_file(&path);
}
