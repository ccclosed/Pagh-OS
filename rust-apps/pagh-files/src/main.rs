//! Files: демо-снимки без экрана или окно в живом композиторе.
//!
//! ```text
//! pagh-files [--out DIR] [--shot NAME|all]     # headless: PNG-скриншоты
//! pagh-files --connect /run/pagh-desktop.sock [--path DIR] [--size WxH]
//! ```

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use pagh_files::screenshots;
use pagh_files::{Command, FilesApp, HostVfs, APP_NAME};
use pagh_ui::canvas::Canvas;
use pagh_ui::client::{DesktopClient, Incoming};
use pagh_ui::geom::Size;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pagh-files: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut out = PathBuf::from("screenshots");
    let mut connect: Option<String> = None;
    let mut path: Option<String> = None;
    let mut size = screenshots::WINDOW;
    let mut shot: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" | "-o" => {
                out = PathBuf::from(args.next().ok_or("--out требует каталог")?);
            }
            "--connect" => {
                connect = Some(args.next().ok_or("--connect требует путь к сокету")?);
            }
            "--path" | "-p" => {
                path = Some(args.next().ok_or("--path требует путь")?);
            }
            "--size" => {
                let value = args.next().ok_or("--size требует WxH")?;
                size = parse_size(&value)?;
            }
            "--shot" => {
                shot = Some(args.next().ok_or("--shot требует имя сцены")?);
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => return Err(format!("неизвестный аргумент: {other}").into()),
        }
    }

    if let Some(socket) = connect {
        return run_compositor(&socket, path, size);
    }
    run_headless(&out, shot.as_deref())
}

fn run_headless(out: &PathBuf, filter: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(out)?;
    let mut written = 0usize;
    for (name, canvas) in screenshots::all_scenes() {
        if let Some(filter) = filter {
            if filter != "all" && filter != name {
                continue;
            }
        }
        let file = screenshots::save_scene(out, name, &canvas)?;
        println!(
            "{name}: {} ({}x{}, hash {:#018x})",
            file.display(),
            canvas.width(),
            canvas.height(),
            pagh_ui::testkit::hash(&canvas)
        );
        written += 1;
    }
    if written == 0 {
        return Err("ни одна сцена не подошла под --shot".into());
    }
    Ok(())
}

fn run_compositor(
    socket: &str,
    path: Option<String>,
    size: Size,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = DesktopClient::connect(socket, APP_NAME)?;
    let mut app = FilesApp::new(Box::new(HostVfs::new()), size);
    if let Some(path) = path {
        app.navigate_to(&path);
    }
    let window = client.create_window(app.size(), &app.title())?;
    let mut canvas = Canvas::filled(app.size().w, app.size().h, app.theme.palette.desktop);
    let mut dirty = true;
    loop {
        for incoming in client.poll()? {
            match incoming {
                Incoming::Event { event, .. } => {
                    for command in app.handle_event(&event) {
                        if handle_command(command) {
                            return Ok(());
                        }
                    }
                    dirty = true;
                }
                Incoming::Configure { size, .. } => {
                    app.resize(size);
                    canvas = Canvas::filled(size.w, size.h, app.theme.palette.desktop);
                    dirty = true;
                }
                Incoming::Closed { .. } => return Ok(()),
                Incoming::Error { code, message } => {
                    eprintln!("композитор: ошибка {code}: {message}");
                }
            }
        }
        if dirty {
            app.draw(&mut canvas);
            client.commit(window, &canvas)?;
            dirty = false;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
}

/// Выполнить команду приложения; `true` — пора выходить.
fn handle_command(command: Command) -> bool {
    match command {
        Command::Quit => return true,
        Command::OpenTerminal { path } => {
            // Терминал — отдельное приложение; запуск по протоколу появится
            // вместе с лаунчером композитора.
            println!("открыть терминал в: {path}");
        }
        Command::OpenPath { path } => {
            println!("открыть: {path}");
        }
        Command::Minimize | Command::Maximize => {
            println!("окно: свернуть/развернуть");
        }
    }
    false
}

fn parse_size(value: &str) -> Result<Size, Box<dyn std::error::Error>> {
    let (width, height) = value
        .split_once(['x', 'X'])
        .ok_or("--size ожидает WxH, например 1060x640")?;
    let width: i32 = width.parse()?;
    let height: i32 = height.parse()?;
    if width <= 0 || height <= 0 {
        return Err("размер окна должен быть положительным".into());
    }
    Ok(Size::new(width, height))
}

fn print_help() {
    println!(
        "pagh-files {}\n\
         \n\
         Без --connect рисует демо-сцены в PNG (headless).\n\
         \n\
         Аргументы:\n\
         \x20 --out DIR           каталог для PNG (по умолчанию screenshots)\n\
         \x20 --shot NAME|all     какая сцена: files-main, files-context-menu,\n\
         \x20                     files-properties, files-list-view, files-search,\n\
         \x20                     files-empty (по умолчанию all)\n\
         \x20 --connect SOCKET    подключиться к композитору Pagh desktop\n\
         \x20 --path DIR          начальный каталог для окна\n\
         \x20 --size WxH          размер окна (по умолчанию 1060x640)",
        pagh_files::VERSION
    );
}
