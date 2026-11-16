//! `pagh-terminal` — headless screenshot tool and PDP/1 terminal surface.
//!
//! Modes:
//!
//! * `--headless` (default without a socket): parse a script or built-in demo,
//!   print the text and/or write a PNG.
//! * `--socket PATH`: connect to the desktop compositor, create a window,
//!   upload frames and process input events.
//! * `--vt PATH`: read snapshots from the proposed kernel VT console shared
//!   surface and send input to `/dev/vtinput` (ABI v1, see TERMINAL.md).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use pagh_terminal::client::{close_name, error_name, Event, SurfaceClient};
use pagh_terminal::demo::{self, DemoShell};
use pagh_terminal::keys::{self, KeyModes};
use pagh_terminal::protocol::{buttons, InputEvent};
use pagh_terminal::render::{
    render_screen, render_screen_into, render_window, Canvas, RenderOptions, Theme,
    VIEWPORT_PADDING,
};
use pagh_terminal::screen::SelectionMode;
use pagh_terminal::vtconsole::VtConsoleReader;
use pagh_terminal::Terminal;

const VERSION: &str = "0.1.0";

#[derive(Debug)]
struct Args {
    headless: bool,
    socket: Option<String>,
    vt: Option<String>,
    vt_input: String,
    size: (u32, u32),
    cols: Option<usize>,
    rows: Option<usize>,
    demo: Option<String>,
    script: Option<PathBuf>,
    out: Option<PathBuf>,
    print: bool,
    print_full: bool,
    dump_selection: bool,
    scroll: Option<usize>,
    select: Option<(usize, usize, usize, usize)>,
    window: bool,
    focused: bool,
    title: String,
    window_id: u32,
    feed: Option<PathBuf>,
    screenshots: Option<PathBuf>,
    help: bool,
    version: bool,
}

impl Default for Args {
    fn default() -> Args {
        Args {
            headless: false,
            socket: None,
            vt: None,
            vt_input: "/dev/vtinput".to_string(),
            size: (900, 600),
            cols: None,
            rows: None,
            demo: None,
            script: None,
            out: None,
            print: false,
            print_full: false,
            dump_selection: false,
            scroll: None,
            select: None,
            window: true,
            focused: true,
            title: "PaghTerm".to_string(),
            window_id: 101,
            feed: None,
            screenshots: None,
            help: false,
            version: false,
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pagh-terminal: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args(std::env::args().skip(1))?;
    if args.help {
        print_help();
        return Ok(());
    }
    if args.version {
        println!("pagh-terminal {VERSION}");
        return Ok(());
    }
    if let Some(dir) = &args.screenshots {
        let paths = demo::generate_screenshots(dir).map_err(|e| e.to_string())?;
        for path in paths {
            println!("{}", path.display());
        }
        return Ok(());
    }
    if let Some(vt) = &args.vt {
        return run_vt(&args, vt);
    }
    if let Some(socket) = &args.socket {
        return run_client(&args, socket);
    }
    run_headless(&args)
}

fn run_headless(args: &Args) -> Result<(), String> {
    let theme = Theme::default();
    let mut terminal = if let Some(path) = &args.script {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let cols = args.cols.unwrap_or(80);
        let rows = args.rows.unwrap_or(24);
        let mut terminal = Terminal::new(cols, rows);
        terminal.feed(&bytes);
        terminal
    } else {
        let name = args.demo.as_deref().unwrap_or("01-welcome");
        let mut terminal =
            demo::build_demo(name).ok_or_else(|| format!("unknown demo {name:?}"))?;
        if let (Some(cols), Some(rows)) = (args.cols, args.rows) {
            terminal.resize(cols, rows);
        }
        terminal
    };
    terminal.screen.set_default_colors(theme.fg, theme.bg);

    if let Some(scroll) = args.scroll {
        terminal.screen.set_view_offset(scroll);
    }
    if let Some((r1, c1, r2, c2)) = args.select {
        terminal
            .screen
            .begin_selection_view(r1, c1, SelectionMode::Char);
        terminal.screen.extend_selection_view(r2, c2);
    }

    if args.print || args.print_full {
        let text = if args.print_full {
            terminal.screen.full_text()
        } else {
            terminal.screen.text()
        };
        println!("{text}");
    }
    if args.dump_selection {
        match terminal.screen.selection_text() {
            Some(text) => println!("{text}"),
            None => eprintln!("pagh-terminal: no selection"),
        }
    }

    if let Some(out) = &args.out {
        let opts = RenderOptions {
            focused: args.focused,
            ..RenderOptions::default()
        };
        let canvas = if args.window {
            let title = if terminal.screen.title.is_empty() {
                args.title.as_str()
            } else {
                terminal.screen.title.as_str()
            };
            render_window(&terminal.screen, &theme, title, &opts)
        } else {
            render_screen(&terminal.screen, &theme, &opts)
        };
        canvas
            .save_png(out)
            .map_err(|e| format!("{}: {e}", out.display()))?;
        eprintln!(
            "pagh-terminal: wrote {} ({}x{})",
            out.display(),
            canvas.width,
            canvas.height
        );
    } else if !args.print && !args.print_full && !args.dump_selection {
        return Err("nothing to do: pass --out, --print or --dump-selection".to_string());
    }
    Ok(())
}

fn run_client(args: &Args, socket: &str) -> Result<(), String> {
    let mut client = SurfaceClient::connect(socket).map_err(|e| format!("{socket}: {e}"))?;
    let welcome = client
        .handshake("pagh-terminal", Duration::from_secs(5))
        .map_err(|e| format!("handshake: {e}"))?;
    eprintln!(
        "pagh-terminal: connected (client {}, screen {}x{}, flags {:#x})",
        welcome.client_id, welcome.screen_width, welcome.screen_height, welcome.flags
    );
    client
        .create_window(args.window_id, args.size.0, args.size.1, &args.title)
        .map_err(|e| format!("create_window: {e}"))?;

    let mut width = args.size.0.max(64);
    let mut height = args.size.1.max(32);
    let (cols, rows) = layout(width, height);
    let mut shell = DemoShell::new(cols, rows);
    if let Some(path) = &args.feed {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        shell.write(&bytes);
    }
    let theme = Theme::default();
    let mut render_opts = RenderOptions::default();
    // ShmSurface: рендер прямо в mmap, по сокету — только damage. На legacy-
    // сервере `enable_shm` вернёт None, и ниже работает полный кадр v1.
    let mut shm = client
        .enable_shm(width, height)
        .map_err(|e| format!("shm offer: {e}"))?;
    let mut fallback: Option<Canvas> = if shm.is_none() {
        Some(Canvas::new(width as usize, height as usize, theme.bg))
    } else {
        None
    };
    let mut tracker = pagh_ui::damage::DamageTracker::new(
        pagh_ui::geom::Rect::new(0, 0, width as i32, height as i32),
        pagh_ui::damage::DEFAULT_TILE,
    );
    let mut dirty = true;
    let mut dragging = false;
    let mut blink_on = true;
    let mut next_blink = Instant::now() + Duration::from_millis(500);

    loop {
        // Cursor blink: damage только клетки курсора (трекер сравнит тайлы).
        if shell.terminal.screen.cursor_blink && Instant::now() >= next_blink {
            blink_on = !blink_on;
            next_blink = Instant::now() + Duration::from_millis(500);
            dirty = true;
        }
        if dirty {
            render_opts.blink_on = blink_on;
            let mut needs_full_frame = false;
            if let Some(frame) = shm.as_mut() {
                frame.canvas.fill(theme.bg);
                render_screen_into(
                    &mut frame.canvas,
                    VIEWPORT_PADDING,
                    VIEWPORT_PADDING,
                    &shell.terminal.screen,
                    &theme,
                    &render_opts,
                );
                let damage = tracker.update(&frame.canvas);
                match client.commit_shm(frame, damage) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                        needs_full_frame = true;
                    }
                    Err(error) => return Err(format!("commit_shm: {error}")),
                }
            } else {
                needs_full_frame = true;
            }
            if needs_full_frame {
                if let Some(stale) = shm.take() {
                    let _ = client.release_shm(&stale);
                }
                let canvas = fallback
                    .get_or_insert_with(|| Canvas::new(width as usize, height as usize, theme.bg));
                canvas.fill(theme.bg);
                render_screen_into(
                    canvas,
                    VIEWPORT_PADDING,
                    VIEWPORT_PADDING,
                    &shell.terminal.screen,
                    &theme,
                    &render_opts,
                );
                client.commit(canvas).map_err(|e| format!("commit: {e}"))?;
                tracker.resize(pagh_ui::geom::Rect::new(0, 0, width as i32, height as i32));
            }
            dirty = false;
        }

        // Не спим дольше, чем до следующего мигания курсора.
        let mut timeout = Duration::from_millis(16);
        if shell.terminal.screen.cursor_blink {
            let until_blink = next_blink.saturating_duration_since(Instant::now());
            timeout = timeout.min(until_blink.max(Duration::from_millis(1)));
        }
        for event in client
            .poll_events(timeout)
            .map_err(|e| format!("poll: {e}"))?
        {
            match event {
                Event::Configure(configure) => {
                    width = configure.width.max(64);
                    height = configure.height.max(32);
                    let (cols, rows) = layout(width, height);
                    shell.terminal.resize(cols, rows);
                    let resized = match shm.as_mut() {
                        Some(frame) => client.resize_shm(frame, width, height).unwrap_or(false),
                        None => false,
                    };
                    if resized {
                        tracker.resize(pagh_ui::geom::Rect::new(0, 0, width as i32, height as i32));
                    } else {
                        if let Some(stale) = shm.take() {
                            let _ = client.release_shm(&stale);
                        }
                        fallback = Some(Canvas::new(width as usize, height as usize, theme.bg));
                        tracker.resize(pagh_ui::geom::Rect::new(0, 0, width as i32, height as i32));
                    }
                    dirty = true;
                }
                Event::ShmRelease {
                    window_id,
                    generation,
                } => {
                    if window_id == args.window_id
                        && shm
                            .as_ref()
                            .is_some_and(|frame| frame.generation == generation)
                    {
                        if let Some(stale) = shm.take() {
                            let _ = client.release_shm(&stale);
                        }
                        fallback = Some(Canvas::new(width as usize, height as usize, theme.bg));
                        tracker.resize(pagh_ui::geom::Rect::new(0, 0, width as i32, height as i32));
                        dirty = true;
                    }
                }
                Event::ShmReady { .. } => {}
                Event::Input(InputEvent::Key {
                    code,
                    pressed,
                    modifiers,
                }) => {
                    if shell.handle_key(code, pressed, modifiers) {
                        dirty = true;
                    }
                    if shell.quit {
                        if let Some(stale) = shm.take() {
                            let _ = client.release_shm(&stale);
                        }
                        let _ = client.close();
                        return Ok(());
                    }
                }
                Event::Input(InputEvent::Axis {
                    vertical,
                    horizontal,
                    ..
                }) => {
                    if vertical != 0 {
                        shell
                            .terminal
                            .screen
                            .scroll_view(if vertical > 0 { -3 } else { 3 });
                        dirty = true;
                    } else if horizontal != 0 {
                        shell
                            .terminal
                            .screen
                            .scroll_view(if horizontal > 0 { -1 } else { 1 });
                        dirty = true;
                    }
                }
                Event::Input(InputEvent::Button {
                    x,
                    y,
                    button,
                    pressed,
                    ..
                }) => {
                    if button == buttons::LEFT {
                        if pressed {
                            if let Some((cell_x, cell_y)) = cell_at(x, y, width, height) {
                                shell.terminal.screen.begin_selection_view(
                                    cell_y,
                                    cell_x,
                                    SelectionMode::Char,
                                );
                                dragging = true;
                                dirty = true;
                            }
                        } else if dragging {
                            dragging = false;
                            dirty = true;
                        }
                    }
                }
                Event::Input(InputEvent::Motion {
                    x,
                    y,
                    buttons: held,
                    ..
                }) => {
                    if dragging && held != 0 {
                        if let Some((cell_x, cell_y)) = cell_at(x, y, width, height) {
                            shell.terminal.screen.extend_selection_view(cell_y, cell_x);
                            dirty = true;
                        }
                    }
                }
                Event::Focus { focused, .. } => {
                    shell.terminal.screen.cursor.visible = focused;
                    render_opts.focused = focused;
                    dirty = true;
                }
                Event::Close { reason, .. } => {
                    eprintln!(
                        "pagh-terminal: server closed the window ({})",
                        close_name(reason)
                    );
                    return Ok(());
                }
                Event::Error { code, message } => {
                    eprintln!(
                        "pagh-terminal: server error {} ({}): {}",
                        code,
                        error_name(code),
                        message
                    );
                }
                Event::Ping(_) | Event::Pong(_) => {}
            }
        }
    }
}

fn run_vt(args: &Args, path: &str) -> Result<(), String> {
    let mut reader = VtConsoleReader::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut client = match &args.socket {
        Some(socket) => {
            let mut client =
                SurfaceClient::connect(socket).map_err(|e| format!("{socket}: {e}"))?;
            let welcome = client
                .handshake("pagh-terminal", Duration::from_secs(5))
                .map_err(|e| format!("handshake: {e}"))?;
            eprintln!(
                "pagh-terminal: connected (client {}, screen {}x{}, flags {:#x})",
                welcome.client_id, welcome.screen_width, welcome.screen_height, welcome.flags
            );
            client
                .create_window(args.window_id, args.size.0, args.size.1, &args.title)
                .map_err(|e| format!("create_window: {e}"))?;
            Some(client)
        }
        None => None,
    };
    let theme = Theme::default();
    let mut width = args.size.0;
    let mut height = args.size.1;
    let mut screen: Option<pagh_terminal::Screen> = None;
    let mut view_offset = 0usize;
    let mut input_error_reported = false;
    let mut dirty = true;

    loop {
        match reader.poll() {
            Ok(Some(snapshot)) => {
                let mut new_screen = snapshot.to_screen();
                new_screen.set_view_offset(view_offset);
                screen = Some(new_screen);
                dirty = true;
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("pagh-terminal: {error}");
                return Err(error.to_string());
            }
        }

        if let (Some(screen), true) = (screen.as_ref(), dirty) {
            if let Some(client) = &mut client {
                let canvas = render_frame(screen, &theme, width, height);
                client.commit(&canvas).map_err(|e| format!("commit: {e}"))?;
            }
            dirty = false;
        }

        let Some(client) = &mut client else {
            if let Some(screen) = &screen {
                println!("{}", screen.text());
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(16));
            continue;
        };

        for event in client
            .poll_events(Duration::from_millis(16))
            .map_err(|e| format!("poll: {e}"))?
        {
            match event {
                Event::Configure(configure) => {
                    width = configure.width.max(64);
                    height = configure.height.max(32);
                    dirty = true;
                }
                Event::Input(InputEvent::Key {
                    code,
                    pressed,
                    modifiers,
                }) => {
                    let modes = screen
                        .as_ref()
                        .map(KeyModes::from_screen)
                        .unwrap_or_default();
                    let bytes = keys::encode_key(code, pressed, modifiers, &modes);
                    if !bytes.is_empty() {
                        if let Err(error) =
                            pagh_terminal::vtconsole::send_input(&args.vt_input, &bytes)
                        {
                            if !input_error_reported {
                                eprintln!("pagh-terminal: cannot write {}: {error}", args.vt_input);
                                input_error_reported = true;
                            }
                        }
                    }
                }
                Event::Input(InputEvent::Axis { vertical, .. }) => {
                    if let Some(screen) = &mut screen {
                        screen.scroll_view(if vertical > 0 { -3 } else { 3 });
                        view_offset = screen.view_offset();
                        dirty = true;
                    }
                }
                Event::Focus { focused, .. } => {
                    if let Some(screen) = &mut screen {
                        screen.cursor.visible = focused;
                        dirty = true;
                    }
                }
                Event::Close { reason, .. } => {
                    eprintln!(
                        "pagh-terminal: server closed the window ({})",
                        close_name(reason)
                    );
                    return Ok(());
                }
                Event::Error { code, message } => {
                    eprintln!(
                        "pagh-terminal: server error {} ({}): {}",
                        code,
                        error_name(code),
                        message
                    );
                }
                _ => {}
            }
        }
    }
}

fn render_frame(screen: &pagh_terminal::Screen, theme: &Theme, width: u32, height: u32) -> Canvas {
    let width = width.max(1) as usize;
    let height = height.max(1) as usize;
    let mut canvas = Canvas::new(width, height, theme.bg);
    render_screen_into(
        &mut canvas,
        VIEWPORT_PADDING,
        VIEWPORT_PADDING,
        screen,
        theme,
        &RenderOptions::default(),
    );
    canvas
}

fn layout(width: u32, height: u32) -> (usize, usize) {
    let pad = VIEWPORT_PADDING * 2;
    let cols = ((width as usize).saturating_sub(pad) / pagh_terminal::font::CELL_W).max(2);
    let rows = ((height as usize).saturating_sub(pad) / pagh_terminal::font::CELL_H).max(2);
    (cols, rows)
}

fn cell_at(x: i32, y: i32, width: u32, height: u32) -> Option<(usize, usize)> {
    if x < 0 || y < 0 {
        return None;
    }
    let x = x as usize;
    let y = y as usize;
    if x >= width as usize || y >= height as usize {
        return None;
    }
    if x < VIEWPORT_PADDING || y < VIEWPORT_PADDING {
        return None;
    }
    Some((
        (x - VIEWPORT_PADDING) / pagh_terminal::font::CELL_W,
        (y - VIEWPORT_PADDING) / pagh_terminal::font::CELL_H,
    ))
}

fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<Args, String> {
    let mut out = Args::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--headless" => out.headless = true,
            "--socket" => out.socket = Some(next_value(&mut args, "--socket")?),
            "--vt" => out.vt = Some(next_value(&mut args, "--vt")?),
            "--vt-input" => out.vt_input = next_value(&mut args, "--vt-input")?,
            "--size" => out.size = parse_size(&next_value(&mut args, "--size")?)?,
            "--cols" => out.cols = Some(parse_usize(&next_value(&mut args, "--cols")?)?),
            "--rows" => out.rows = Some(parse_usize(&next_value(&mut args, "--rows")?)?),
            "--demo" => out.demo = Some(next_value(&mut args, "--demo")?),
            "--script" => out.script = Some(PathBuf::from(next_value(&mut args, "--script")?)),
            "--out" => out.out = Some(PathBuf::from(next_value(&mut args, "--out")?)),
            "--print" => out.print = true,
            "--print-full" => out.print_full = true,
            "--dump-selection" => out.dump_selection = true,
            "--scroll" => out.scroll = Some(parse_usize(&next_value(&mut args, "--scroll")?)?),
            "--select" => out.select = Some(parse_select(&next_value(&mut args, "--select")?)?),
            "--window" => out.window = true,
            "--no-window" => out.window = false,
            "--unfocused" => out.focused = false,
            "--title" => out.title = next_value(&mut args, "--title")?,
            "--window-id" => out.window_id = parse_u32(&next_value(&mut args, "--window-id")?)?,
            "--feed" => out.feed = Some(PathBuf::from(next_value(&mut args, "--feed")?)),
            "--screenshots" => {
                out.screenshots = Some(PathBuf::from(next_value(&mut args, "--screenshots")?))
            }
            "--help" | "-h" => out.help = true,
            "--version" | "-V" => out.version = true,
            other => return Err(format!("unknown argument {other:?}; try --help")),
        }
    }
    Ok(out)
}

fn next_value<I: Iterator<Item = String>>(args: &mut I, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_usize(text: &str) -> Result<usize, String> {
    text.parse().map_err(|_| format!("invalid number {text:?}"))
}

fn parse_u32(text: &str) -> Result<u32, String> {
    text.parse().map_err(|_| format!("invalid number {text:?}"))
}

fn parse_size(text: &str) -> Result<(u32, u32), String> {
    let (w, h) = text
        .split_once('x')
        .or_else(|| text.split_once('X'))
        .ok_or_else(|| format!("size must be WIDTHxHEIGHT, got {text:?}"))?;
    Ok((
        w.parse()
            .map_err(|_| format!("invalid width in {text:?}"))?,
        h.parse()
            .map_err(|_| format!("invalid height in {text:?}"))?,
    ))
}

fn parse_select(text: &str) -> Result<(usize, usize, usize, usize), String> {
    let (start, end) = text
        .split_once(':')
        .ok_or_else(|| format!("selection must be R1,C1:R2,C2, got {text:?}"))?;
    let parse_pair = |pair: &str| -> Result<(usize, usize), String> {
        let (a, b) = pair
            .split_once(',')
            .ok_or_else(|| format!("expected ROW,COL in {pair:?}"))?;
        Ok((
            a.parse().map_err(|_| "invalid row".to_string())?,
            b.parse().map_err(|_| "invalid column".to_string())?,
        ))
    };
    let (r1, c1) = parse_pair(start)?;
    let (r2, c2) = parse_pair(end)?;
    Ok((r1, c1, r2, c2))
}

fn print_help() {
    println!(
        "pagh-terminal {VERSION} — VT100/ANSI terminal for Pagh desktop B

USAGE:
  pagh-terminal --headless [--demo NAME|--script FILE] [--cols N --rows N]
                [--out FILE.png] [--print|--print-full] [--window|--no-window]
                [--scroll N] [--select R1,C1:R2,C2] [--dump-selection]
  pagh-terminal --socket PATH [--size WxH] [--title TITLE] [--feed FILE]
  pagh-terminal --vt /dev/vtconsole0 [--socket PATH] [--vt-input /dev/vtinput]
  pagh-terminal --screenshots DIR

HEADLESS:
  --demo NAME      01-welcome, 02-colors, 03-scrollback, 04-unicode
  --script FILE    raw terminal output (escape sequences allowed)
  --out FILE       write PNG (window chrome by default; --no-window for viewport)
  --print          print the visible text
  --print-full     print scrollback + visible text
  --dump-selection print selected text

LIVE:
  --socket PATH    AF_UNIX control socket of pagh-desktop
  --size WxH       initial window content size (default 900x600)
  --feed FILE      preload terminal output into the demo shell
  --window-id N    client-chosen window id (default 101)
  --vt PATH        read the kernel VT shared text surface (ABI v1)
  --vt-input PATH  where to write input bytes (default /dev/vtinput)

SCREENSHOTS:
  --screenshots DIR  regenerate the committed demo PNG fixtures

Environment: PAGH_DESKTOP_SOCKET overrides the default socket path
({}).",
        pagh_terminal::client::DEFAULT_SOCKET
    );
}
