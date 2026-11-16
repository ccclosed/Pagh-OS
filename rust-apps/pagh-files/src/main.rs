//! Files: демо-снимки без экрана или окно в живом композиторе Pagh desktop.
//!
//! ```text
//! pagh-files [--out DIR] [--shot NAME|all]       # headless: PNG-скриншоты
//! pagh-files --connect /run/pagh-desktop.sock    # окно по PDP/1
//! ```

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use pagh_files::screenshots;
use pagh_files::{Command, FilesApp, HostVfs, APP_NAME};
use pagh_ui::canvas::Canvas;
use pagh_ui::client::{close_name, error_name, DesktopClient, Incoming, ShmFrame};
use pagh_ui::damage::{DamageTracker, DEFAULT_TILE};
use pagh_ui::geom::{Rect, Size};
use pagh_ui::protocol::close_reason;
use pagh_ui::Color;

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
            "--connect" | "--socket" => {
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

/// Окно Files в живом композиторе (`pagh-desktop`, PDP/1).
///
/// Клиент рисует только клиентскую область: заголовок окна, рамку, кнопки
/// `v`/`^`/`x` и панель рисует композитор. Кадр уходит после каждого события,
/// меняющего вид; `configure` меняет размер клиентской области (включая
/// maximize/restore), `close` от сервера завершает приложение.
fn run_compositor(
    socket: &str,
    path: Option<String>,
    size: Size,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = DesktopClient::connect_retry(socket, APP_NAME, 200)?;
    let mut app = FilesApp::new(Box::new(HostVfs::new()), size);
    if let Some(path) = path {
        app.navigate_to(&path);
    }
    let window = client.open_window(app.size(), &app.title())?;
    println!(
        "pagh-files: окно {window} {}x{} (сокет {socket}, экран {}x{})",
        app.size().w,
        app.size().h,
        client.screen().w,
        client.screen().h
    );
    let _ = std::io::stdout().flush();

    let mut window_size = app.size();
    // ShmSurface — приоритетный путь: рендер прямо в mmap memfd, по сокету
    // едут только damage-прямоугольники. На legacy-сервере (нет welcome bit 2)
    // `enable_shm` сразу возвращает `None`, и работает полный кадр v1.
    let mut shm = client.enable_shm(window, window_size)?;
    let mut fallback: Option<Canvas> = if shm.is_none() {
        Some(Canvas::filled(
            window_size.w,
            window_size.h,
            app.theme.palette.window,
        ))
    } else {
        None
    };
    let mut tracker =
        DamageTracker::new(Rect::new(0, 0, window_size.w, window_size.h), DEFAULT_TILE);
    let mut dirty = true;

    // Переключиться на полный кадр: отпустить shm-поколение (если было) и
    // выделить обычный RGBA-холст.
    fn use_fallback(
        client: &mut DesktopClient,
        shm: &mut Option<ShmFrame>,
        fallback: &mut Option<Canvas>,
        tracker: &mut DamageTracker,
        size: Size,
        background: Color,
    ) {
        if let Some(frame) = shm.take() {
            let _ = client.release_shm(&frame);
        }
        *fallback = Some(Canvas::filled(size.w, size.h, background));
        tracker.resize(Rect::new(0, 0, size.w, size.h));
    }

    loop {
        for incoming in client.poll(Duration::from_millis(16))? {
            match incoming {
                Incoming::Event {
                    window_id, event, ..
                } if window_id == window => {
                    for command in app.handle_event(&event) {
                        match command {
                            Command::Quit => {
                                let _ = client.close_window(window);
                                if let Some(frame) = shm.take() {
                                    let _ = client.release_shm(&frame);
                                }
                                return Ok(());
                            }
                            Command::LaunchTerminal { path } => {
                                // PDP/1 launch (type 13): композитор запускает
                                // терминал. Успех отдельно не подтверждается;
                                // ошибка придёт как Incoming::Error.
                                client.launch("terminal", &path)?;
                            }
                            Command::OpenPath { path } => {
                                println!("pagh-files: открыть {path}");
                                let _ = std::io::stdout().flush();
                            }
                        }
                    }
                    dirty = true;
                }
                Incoming::Configure {
                    window_id, size, ..
                } if window_id == window => {
                    app.resize(size);
                    window_size = size;
                    let resized = match shm.as_mut() {
                        Some(frame) => client.resize_shm(frame, size).unwrap_or(false),
                        None => false,
                    };
                    if resized {
                        tracker.resize(Rect::new(0, 0, size.w, size.h));
                    } else {
                        use_fallback(
                            &mut client,
                            &mut shm,
                            &mut fallback,
                            &mut tracker,
                            size,
                            app.theme.palette.window,
                        );
                    }
                    dirty = true;
                }
                Incoming::ShmRelease {
                    window_id,
                    generation,
                } => {
                    // Сервер вытеснил поверхность: до нового offer рисуем и
                    // шлём полный кадр. Новый offer сделает configure.
                    if window_id == window
                        && shm
                            .as_ref()
                            .is_some_and(|frame| frame.generation == generation)
                    {
                        use_fallback(
                            &mut client,
                            &mut shm,
                            &mut fallback,
                            &mut tracker,
                            window_size,
                            app.theme.palette.window,
                        );
                        dirty = true;
                    }
                }
                Incoming::ShmReady { .. } => {}
                Incoming::Close { reason, .. } => {
                    let _ = client.close_window(window);
                    if reason != close_reason::CLIENT {
                        println!(
                            "pagh-files: композитор закрыл окно ({})",
                            close_name(reason)
                        );
                        let _ = std::io::stdout().flush();
                    }
                    return Ok(());
                }
                Incoming::Error { code, message } => {
                    eprintln!(
                        "pagh-files: композитор: {} ({code}): {message}",
                        error_name(code)
                    );
                }
                Incoming::Focus { .. } | Incoming::Ping { .. } => {}
                Incoming::Configure { .. } | Incoming::Event { .. } => {}
            }
        }
        if !dirty {
            continue;
        }
        dirty = false;
        let needs_full_frame;
        if let Some(frame) = shm.as_mut() {
            app.draw(&mut frame.canvas);
            let damage = tracker.update(&frame.canvas);
            match client.commit_shm(frame, damage) {
                Ok(()) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                    // Поколение устарело (сервер прислал release, но мы его
                    // ещё не прочитали): переходим на полный кадр.
                    use_fallback(
                        &mut client,
                        &mut shm,
                        &mut fallback,
                        &mut tracker,
                        window_size,
                        app.theme.palette.window,
                    );
                    needs_full_frame = true;
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            needs_full_frame = true;
        }
        if needs_full_frame {
            let canvas = fallback.get_or_insert_with(|| {
                Canvas::filled(window_size.w, window_size.h, app.theme.palette.window)
            });
            app.draw(canvas);
            client.commit(window, canvas)?;
        }
    }
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
         \x20 --connect SOCKET    окно в композиторе Pagh desktop (PDP/1)\n\
         \x20 --socket SOCKET     то же, что --connect\n\
         \x20 --path DIR          начальный каталог для окна\n\
         \x20 --size WxH          размер клиентской области (по умолчанию 1060x640)\n\
         \n\
         Оформление окна (заголовок, v/^/x, панель) принадлежит композитору;\n\
         Files рисует только клиентскую область и шлёт кадры по PDP/1.",
        pagh_files::VERSION
    );
}
