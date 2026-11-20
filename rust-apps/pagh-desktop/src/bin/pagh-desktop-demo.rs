//! Демо-клиент PDP/1 и генератор скриншотов.
//!
//! ```text
//! # клиент к запущенному серверу
//! pagh-desktop-demo --connect /tmp/pagh-desktop.sock --app terminal
//! # скриншоты без сервера/сокета
//! pagh-desktop-demo --scenario --out screenshots
//! ```

use pagh_desktop::client::{is_disconnect, Client};
use pagh_desktop::demo::{self, DemoState};
use pagh_desktop::protocol::{InputEvent, Message};
use pagh_desktop::surface::{PixelFormat, Surface};
use std::env;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DemoApp {
    Terminal,
    Files,
    Paint,
}

impl DemoApp {
    fn from_arg(value: &str) -> Option<Self> {
        match value {
            "terminal" => Some(Self::Terminal),
            "files" => Some(Self::Files),
            "paint" => Some(Self::Paint),
            _ => None,
        }
    }

    fn dimensions(self) -> (u32, u32, &'static str) {
        match self {
            Self::Terminal => (640, 400, "Terminal - pagh"),
            Self::Files => (560, 340, "Files - /mnt"),
            Self::Paint => (600, 380, "Paint - Pagh"),
        }
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("pagh-desktop-demo: {err}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let mut scenario = false;
    let mut out = PathBuf::from("screenshots");
    let mut connect: Option<PathBuf> = None;
    let mut app = DemoApp::Terminal;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--scenario" => scenario = true,
            "--out" => {
                out = PathBuf::from(args.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--out требует путь")
                })?)
            }
            "--connect" => {
                connect = Some(PathBuf::from(args.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--connect требует путь")
                })?))
            }
            "--app" => {
                let value = args.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--app требует terminal|files|paint",
                    )
                })?;
                app = DemoApp::from_arg(&value).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("неизвестное приложение {value:?}"),
                    )
                })?;
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("неизвестный аргумент {other:?}"),
                ))
            }
        }
    }

    if scenario {
        let report = pagh_desktop::scenario::run(&out)?;
        println!(
            "сценарий готов: {} скриншотов в {}",
            report.screenshots.len(),
            report.out_dir.display()
        );
        for path in &report.screenshots {
            println!("  {}", path.display());
        }
        return Ok(());
    }

    let Some(socket) = connect else {
        print_help();
        return Ok(());
    };
    run_client(&socket, app)
}

fn run_client(socket: &PathBuf, app: DemoApp) -> io::Result<()> {
    // Сервер штатно завершился (`--exit-after-ms`, финал прогона): клиент
    // получает EOF/EPIPE. Это конец сеанса, а не ошибка — выходим с кодом 0.
    match client_session(socket, app) {
        Ok(()) => Ok(()),
        Err(err) if is_disconnect(&err) => {
            println!("pagh-desktop-demo: сервер закрыл соединение, завершаемся");
            Ok(())
        }
        Err(err) => Err(err),
    }
}

fn client_session(socket: &PathBuf, app: DemoApp) -> io::Result<()> {
    let mut client = Client::connect_retry(socket, 200)?;
    let welcome = client.hello("pagh-desktop-demo")?;
    let window_id = ((welcome.client_id as u32).wrapping_mul(100).wrapping_add(1)).max(1);
    let (width, height, title) = app.dimensions();
    let mut state = DemoState {
        focused: true,
        ..DemoState::default()
    };
    let mut surface = make_surface(app, width, height, &state);
    client.create_window(window_id, width, height, PixelFormat::Xrgb8888, title)?;
    client.commit(window_id, &surface)?;
    println!(
        "pagh-desktop-demo: window {window_id} {width}x{height} подключён к {}",
        socket.display()
    );

    let mut size = (width, height);
    let mut last_commit = Instant::now();
    loop {
        match client.next_message(Duration::from_millis(100))? {
            Some(Message::Configure { width, height, .. }) => {
                if (width, height) != size {
                    size = (width, height);
                    surface = make_surface(app, width, height, &state);
                    client.commit(window_id, &surface)?;
                    println!("pagh-desktop-demo: configure {width}x{height}");
                }
            }
            Some(Message::Input {
                window_id: id,
                event,
            }) => {
                if id != window_id {
                    continue;
                }
                apply_input(&mut state, event);
                let motion = matches!(event, InputEvent::Motion { .. } | InputEvent::Axis { .. });
                if !motion || last_commit.elapsed() >= Duration::from_millis(16) {
                    surface = make_surface(app, size.0, size.1, &state);
                    client.commit(window_id, &surface)?;
                    last_commit = Instant::now();
                }
            }
            Some(Message::Focus {
                window_id: id,
                focused,
            }) => {
                if id == window_id {
                    state.focused = focused;
                    surface = make_surface(app, size.0, size.1, &state);
                    client.commit(window_id, &surface)?;
                }
            }
            Some(Message::Close { window_id: id, .. }) if id == window_id => {
                println!("pagh-desktop-demo: сервер закрыл окно {id}");
                return Ok(());
            }
            Some(Message::Error { code, message }) => {
                eprintln!("pagh-desktop-demo: ошибка сервера {code}: {message}");
                return Ok(());
            }
            Some(_) => {}
            None => {}
        }
    }
}

fn apply_input(state: &mut DemoState, event: InputEvent) {
    match event {
        InputEvent::Motion {
            x,
            y,
            dx,
            dy,
            buttons,
        } => {
            state.mouse = Some(pagh_desktop::Point::new(x, y));
            state.last_input = format!("move {x},{y} d={dx},{dy} b={buttons}");
        }
        InputEvent::Button {
            x,
            y,
            button,
            pressed,
            ..
        } => {
            state.mouse = Some(pagh_desktop::Point::new(x, y));
            state.clicked = pressed && button == 1;
            state.last_input = format!("button {button} {}", if pressed { "down" } else { "up" });
        }
        InputEvent::Key {
            code,
            pressed,
            modifiers,
        } => {
            state.last_input = format!(
                "key {code} {} mods={modifiers}",
                if pressed { "down" } else { "up" }
            );
        }
        InputEvent::Axis { x, y, vertical, .. } => {
            state.mouse = Some(pagh_desktop::Point::new(x, y));
            state.scroll = state.scroll.saturating_add(vertical);
            state.last_input = format!("wheel {vertical} at {x},{y}");
        }
    }
}

fn make_surface(app: DemoApp, width: u32, height: u32, state: &DemoState) -> Surface {
    match app {
        DemoApp::Terminal => demo::terminal_surface(width, height, state),
        DemoApp::Files => demo::files_surface(width, height, state),
        DemoApp::Paint => demo::paint_surface(width, height, state),
    }
}

fn print_help() {
    println!(
        "pagh-desktop-demo — тестовый клиент и генератор скриншотов\n\
         \n\
         Использование:\n\
           pagh-desktop-demo --connect SOCKET --app terminal|files|paint\n\
           pagh-desktop-demo --scenario [--out DIR]\n\
         \n\
         В режиме клиента окно рисует Terminal/Files/Paint, обрабатывает\n\
         configure/input/focus/close и коммитит кадры по протоколу PDP/1."
    );
}
