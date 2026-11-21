//! E2E двойного клика: настоящий бинарник `pagh-files` по реальному AF_UNIX
//! сокету и протоколу PDP/1, но композитор — скриптовый (этот тест): он знает
//! клиентские координаты окна и потому может нажать точно в ячейку сетки.
//!
//! Проверяется именно живой путь F3.1: PDP/1 передаёт только отдельные
//! `Button` (в протоколе нет двойного клика), `pagh-ui::client` синтезирует
//! `Event::DoubleClick`, `pagh-files` по нему входит в каталог, а затем
//! открывает файл уже внутри него — путь из stdout приложения доказывает, что
//! первый двойной клик действительно сменил каталог.
//!
//! Почему не настоящий `pagh-desktop`: его окна размещаются каскадом внутри
//! композитора, и тест не может спросить у живого композитора клиентский
//! прямоугольник окна. Скриптовый сервер ставит клиентскую область в (0,0),
//! поэтому координаты ячеек — ровно те, что считает `FilesApp` того же
//! размера. Сам композитор (PDP/1-сервер, размещение, доставка) покрыт
//! `pdp_e2e.rs`/`socket_e2e.rs`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pagh_files::{FilesApp, HostVfs};
use pagh_ui::geom::{Point, Size};
use pagh_ui::protocol::{self, buttons, close_reason, InputEvent, Message};

/// Размер клиентской области: тот же, что просит тест у бинарника.
const WINDOW: Size = Size::new(900, 560);

fn temp_dir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pagh-files-dblclick-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn wait_for<F: FnMut() -> bool>(timeout: Duration, mut predicate: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Точка внутри ячейки сетки для элемента `name` в каталоге `cwd`.
///
/// Считается тем же `FilesApp` (тот же размер окна, тот же `HostVfs`), что
/// крутится в бинарнике, поэтому координаты совпадают с живой раскладкой.
fn entry_point(cwd: &Path, name: &str) -> Point {
    let mut app = FilesApp::new(Box::new(HostVfs::new()), WINDOW);
    app.navigate_to(&cwd.to_string_lossy());
    let index = app
        .entries()
        .iter()
        .position(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("нет элемента {name:?} в {cwd:?}"));
    app.item_rect(index)
        .unwrap_or_else(|| panic!("ячейка {name:?} не разложена"))
        .center()
}

/// Клик левой кнопкой в клиентских координатах окна.
fn write_click(stream: &mut impl Write, pos: Point) {
    for pressed in [true, false] {
        let event = InputEvent::Button {
            x: pos.x,
            y: pos.y,
            button: buttons::LEFT,
            pressed,
            buttons: if pressed { buttons::MASK_LEFT } else { 0 },
        };
        stream
            .write_all(
                &Message::Input {
                    window_id: 1,
                    event,
                }
                .encode(),
            )
            .unwrap();
    }
}

fn wait_child_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
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
fn double_click_over_pdp_opens_directory_and_then_file() {
    // Рабочий каталог, который приложение получит через `--path`.
    let root = temp_dir("root");
    let work = root.join("work");
    std::fs::create_dir_all(work.join("Открыть")).unwrap();
    std::fs::write(work.join("Открыть").join("внутри.txt"), b"ok").unwrap();
    let nested = work.join("Открыть").join("внутри.txt");

    // Координаты: ячейка каталога в `work`, затем ячейка файла внутри него.
    let dir_point = entry_point(&work, "Открыть");
    let file_point = entry_point(&work.join("Открыть"), "внутри.txt");

    let socket = root.join("pagh-desktop.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_pagh-files"))
        .arg("--connect")
        .arg(&socket)
        .arg("--size")
        .arg(format!("{}x{}", WINDOW.w, WINDOW.h))
        .arg("--path")
        .arg(&work)
        .env("PAGH_UI_DISABLE_SHM", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("pagh-files стартует");

    // stdout читаем отдельным потоком: `OpenPath` печатается туда.
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = lines.clone();
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            sink.lock().unwrap().push(line);
        }
    });

    // Подключение бинарника к скриптовому композитору.
    let mut stream = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && stream.is_none() {
        match listener.accept() {
            Ok((connection, _)) => stream = Some(connection),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("accept: {error}"),
        }
    }
    let mut stream = stream.expect("pagh-files не подключился к сокету");

    // hello → welcome без ShmSurface (клиент идёт путём полного кадра).
    let hello = protocol::read_message(&mut stream).unwrap();
    assert!(matches!(hello, Message::Hello { .. }), "{hello:?}");
    stream
        .write_all(
            &Message::Welcome {
                version: protocol::VERSION,
                client_id: 3,
                width: 1280,
                height: 800,
                flags: protocol::welcome_flags::FORMAT_XRGB8888,
            }
            .encode(),
        )
        .unwrap();

    // create → configure с клиентской областью в (0,0): координаты ячеек,
    // посчитанные выше, и есть клиентские координаты ввода.
    let create = protocol::read_message(&mut stream).unwrap();
    let Message::Create {
        window_id,
        width,
        height,
        ..
    } = create
    else {
        panic!("ожидался create, получено {create:?}");
    };
    assert_eq!((width, height), (WINDOW.w as u32, WINDOW.h as u32));
    stream
        .write_all(
            &Message::Configure {
                window_id,
                x: 0,
                y: 0,
                width,
                height,
                flags: 0,
            }
            .encode(),
        )
        .unwrap();

    // Кадры клиента нужно вычитывать, иначе 2,4 МБ commit'ов забьют сокет и
    // приложение перестанет читать ввод. Читает отдельный поток; пишем только
    // мы, поэтому направления не пересекаются.
    let mut frames = stream.try_clone().unwrap();
    let drain = std::thread::spawn(move || while protocol::read_message(&mut frames).is_ok() {});

    // Два двойных клика в одном залпе: приложение обрабатывает события по
    // порядку, поэтому к моменту второго клика `cwd` уже внутри каталога.
    write_click(&mut stream, dir_point);
    write_click(&mut stream, dir_point);
    write_click(&mut stream, file_point);
    write_click(&mut stream, file_point);
    stream.flush().unwrap();

    let expected = format!("pagh-files: открыть {}", nested.display());
    let opened = wait_for(Duration::from_secs(20), || {
        lines.lock().unwrap().iter().any(|line| *line == expected)
    });
    let seen = lines.lock().unwrap().clone();
    assert!(
        opened,
        "файл внутри каталога не открылся двойным кликом; stdout: {seen:?}"
    );
    // Первый двойной клик обязан был именно войти в каталог: путь открытого
    // файла лежит внутри `Открыть`, а не в `work`.
    assert!(
        seen.iter().any(|line| line == &expected),
        "двойной клик открыл не тот файл: {seen:?}"
    );

    stream
        .write_all(
            &Message::Close {
                window_id,
                reason: close_reason::SERVER,
            }
            .encode(),
        )
        .unwrap();
    stream.flush().unwrap();
    let status = wait_child_exit(&mut child, Duration::from_secs(10));
    if status.is_none() {
        let _ = child.kill();
    }
    assert!(
        matches!(status, Some(ref status) if status.success()),
        "pagh-files не завершился штатно: {status:?}"
    );
    let _ = drain.join();
    let _ = reader.join();
    let _ = std::fs::remove_dir_all(&root);
}
