//! `pagh-terminal` — headless screenshot tool and PDP/1 terminal surface.
//!
//! Modes:
//!
//! * `--socket PATH` (default windowed mode): attach to the **kernel shell
//!   session** — blit `/dev/vtfb1` pixels into the window and forward keys to
//!   `/dev/vtinput1`.  The kernel owns the shell: commands, echo, line editing
//!   and scrollback are its, the client only renders and forwards input.
//! * `--headless` (default without a socket): parse a script or built-in demo,
//!   print the text and/or write a PNG.
//! * `--vt PATH`: read snapshots from the kernel VT console shared text surface
//!   and send input to `/dev/vtinput` (ABI v1, see TERMINAL.md).  With
//!   `--socket` this is the legacy window paths: `--mirror-vt` mirrors the boot
//!   VT, otherwise the private userspace shell session starts.
//! * `--pipe-shell` / `--demo-shell`: explicit test/fallback window modes (the
//!   private userspace shell over pipes, and the built-in demo shell).
//!
//! The mmap/ioctl glue for the kernel-session devices lives in the `devfs`
//! submodule: the library crate forbids `unsafe`, so every unsafe call sits here.

// Explicit path: cargo auto-discovers every `*.rs` directly under `src/bin/` as
// a binary target, so the module lives in the `pagh-terminal/` subdirectory and
// is named here instead of being a second (empty) bin.
#[path = "pagh-terminal/devfs.rs"]
mod devfs;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use pagh_terminal::cell::{CELL_WIDE, CELL_WIDE_CONT};
use pagh_terminal::client::{close_name, error_name, Event, ShmFrame, SurfaceClient};
use pagh_terminal::demo::{self, DemoShell};
use pagh_terminal::font::{CELL_H, CELL_W};
use pagh_terminal::kernelsession::{
    is_missing_device, is_session_end, render_notice, Draw, FileInput, KernelFrame, KeyForwarder,
    VtFbError,
};
use pagh_terminal::keys::{self, KeyModes};
use pagh_terminal::protocol::{buttons, InputEvent};
use pagh_terminal::render::{
    render_screen, render_screen_into, render_screen_into_cleared, render_window, Canvas,
    RenderOptions, Theme, VIEWPORT_PADDING,
};
use pagh_terminal::screen::{CursorStyle, SelectionMode};
use pagh_terminal::session::{self, ShellSession};
use pagh_terminal::vtconsole::VtConsoleReader;
use pagh_terminal::Terminal;
use pagh_terminal::{Attrs, Cell, Screen};
use pagh_ui::damage::Damage;
use pagh_ui::geom::Rect;

const VERSION: &str = "0.1.0";

/// Kernel shell-session pixel surface (mode default).
const KERNEL_FB: &str = "/dev/vtfb1";
/// Kernel shell-session input device.
const KERNEL_INPUT: &str = "/dev/vtinput1";
/// Boot VT text surface (legacy `--vt`/`--mirror-vt`).
const MIRROR_VT: &str = "/dev/vtconsole0";
/// Boot VT input device (legacy mirror).
const MIRROR_INPUT: &str = "/dev/vtinput";
/// Сколько держать в окне честное сообщение «сессия завершена/ошибка» перед
/// закрытием окна.
const NOTICE_HOLD: Duration = Duration::from_millis(1500);

#[derive(Debug)]
struct Args {
    headless: bool,
    socket: Option<String>,
    vt: Option<String>,
    /// Input device override; per-mode default (`/dev/vtinput` for the boot-VT
    /// mirror, `/dev/vtinput1` for the kernel session) when absent.
    vt_input: Option<String>,
    /// Pixel surface of the kernel shell session (default `/dev/vtfb1`).
    vt_fb: String,
    /// Log every drawn kernel frame (generation evidence for live runs).
    vt_trace: bool,
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
    /// Run as the shell child of a terminal session (`--shell-child`).
    shell_child: bool,
    /// Optional program for the session's child (default: this binary in
    /// `--shell-child` mode). Used to drive a real shell once one exists.
    shell_cmd: Option<PathBuf>,
    /// Keep the old windowed behaviour: mirror the kernel boot VT instead of
    /// attaching to the kernel shell session.
    mirror_vt: bool,
    /// Explicit window mode: the private userspace shell over two pipes
    /// (fallback/test path of the kernel-session mode).
    pipe_shell: bool,
    /// Explicit window mode: the built-in demo shell (screenshots, `--feed`).
    demo_shell: bool,
    help: bool,
    version: bool,
}

impl Default for Args {
    fn default() -> Args {
        Args {
            headless: false,
            socket: None,
            vt: None,
            vt_input: None,
            vt_fb: KERNEL_FB.to_string(),
            vt_trace: false,
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
            // The compositor draws the window title bar, so this string is what
            // the user sees above the grid: the session name, not a product
            // name.  One kernel-VT means one session — no tab-style label.
            title: "vt0 — Pagh Terminal".to_string(),
            window_id: 101,
            feed: None,
            screenshots: None,
            shell_child: false,
            shell_cmd: None,
            mirror_vt: false,
            pipe_shell: false,
            demo_shell: false,
            help: false,
            version: false,
        }
    }
}

impl Args {
    /// Where the boot-VT mirror writes VT input bytes.
    fn mirror_input(&self) -> &str {
        self.vt_input.as_deref().unwrap_or(MIRROR_INPUT)
    }

    /// Where the kernel-session mode writes VT input bytes.
    fn kernel_input(&self) -> &str {
        self.vt_input.as_deref().unwrap_or(KERNEL_INPUT)
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
    if args.shell_child {
        // A terminal session's child: stdin/stdout are pipes owned by the window.
        std::process::exit(pagh_terminal::sh::run());
    }
    if let Some(vt) = &args.vt {
        return run_vt(&args, vt);
    }
    // `--mirror-vt` without an explicit `--vt` path means the boot VT console.
    if args.mirror_vt {
        return run_vt(&args, MIRROR_VT);
    }
    // Explicit non-default window modes (tests, screenshots, A/B comparison).
    if args.demo_shell || args.feed.is_some() {
        let socket = args
            .socket
            .as_deref()
            .ok_or("--demo-shell/--feed require --socket")?;
        return run_client(&args, socket);
    }
    if args.pipe_shell {
        let socket = args
            .socket
            .as_deref()
            .ok_or("--pipe-shell requires --socket")?;
        return run_shell_session(&args, socket);
    }
    if let Some(socket) = &args.socket {
        // Default windowed mode: attach to the kernel shell session.
        return run_kernel_session(&args, socket);
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

// ───────────────────── виджетный damage терминала ─────────────────────
//
// Терминал не хеширует кадр: он знает, какие клетки изменились, потому что
// сравнивает сетку с прошлым кадром. Damage — это отрезки клеток, и
// перерисовываются только они. Полный кадр остаётся для первого кадра,
// resize/expose, прокрутки (меняются все строки), выделения (оверлей не виден
// в клетках) и крупных правок.

/// Горизонтальный отрезок клеток одной строки — минимальная перерисовка.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellRun {
    y: usize,
    x0: usize,
    x1: usize,
}

/// Клетка строки представления; строки короче сетки дополняются пустыми.
fn line_cell(line: &[Cell], x: usize) -> Cell {
    line.get(x).copied().unwrap_or_default()
}

impl CellRun {
    /// Отрезок вокруг изменённых клеток `start..end` с запасом на сглаживание.
    ///
    /// Широкий глиф занимает две клетки (16 px) и рисуется из ведущей, а
    /// курсивный вынос доходит на 1–2 px вправо, поэтому отрезок расширяется
    /// на две клетки в обе стороны, а широкая пара всегда попадает целиком.
    /// Лишние клетки перерисовываются тем же содержимым, что и в полном кадре,
    /// поэтому кадр остаётся пиксель-в-пиксель.
    fn around(line: &[Cell], y: usize, start: usize, end: usize, cols: usize) -> CellRun {
        let mut x0 = start.min(cols.saturating_sub(1));
        let mut x1 = end.max(start + 1).min(cols);
        while x0 > 0 && line_cell(line, x0).flags & CELL_WIDE_CONT != 0 {
            x0 -= 1;
        }
        while x1 < cols && line_cell(line, x1 - 1).flags & CELL_WIDE != 0 {
            x1 += 1;
        }
        CellRun {
            y,
            x0: x0.saturating_sub(2),
            x1: (x1 + 2).min(cols),
        }
    }

    /// Прямоугольник этого отрезка в координатах клиентской поверхности.
    fn rect(self) -> Rect {
        Rect::new(
            VIEWPORT_PADDING as i32 + self.x0 as i32 * CELL_W as i32,
            VIEWPORT_PADDING as i32 + self.y as i32 * CELL_H as i32,
            (self.x1 - self.x0) as i32 * CELL_W as i32,
            CELL_H as i32,
        )
    }
}

/// Что перерисовать в следующем кадре.
#[derive(Debug)]
enum FrameDamage {
    /// Первый кадр, resize/expose, прокрутка, выделение, крупная правка.
    Full,
    Runs(Vec<CellRun>),
}

/// Когда изменилась четверть клеток, дешевле нарисовать весь кадр.
const FULL_REDRAW_SHARE: usize = 4;

/// Снимок отрисованного кадра: сетка и всё, что влияет на её вид.
struct FrameSnapshot {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
    cursor: (usize, usize),
    cursor_visible: bool,
    cursor_style: CursorStyle,
    cursor_blink: bool,
    view_offset: usize,
    scrollback: usize,
    selected: bool,
    focused: bool,
    blink_on: bool,
    show_scrollbar: bool,
}

impl FrameSnapshot {
    fn capture(screen: &Screen, opts: &RenderOptions) -> FrameSnapshot {
        let cols = screen.cols();
        let rows = screen.rows();
        let mut cells = Vec::with_capacity(cols * rows);
        for y in 0..rows {
            let line = screen.view_line(y);
            for x in 0..cols {
                cells.push(line_cell(line, x));
            }
        }
        FrameSnapshot {
            cols,
            rows,
            cells,
            cursor: (screen.cursor.x, screen.cursor.y),
            cursor_visible: screen.cursor.visible,
            cursor_style: screen.cursor.style,
            cursor_blink: screen.cursor_blink,
            view_offset: screen.view_offset(),
            scrollback: screen.scrollback_len(),
            selected: screen.selection().is_some(),
            focused: opts.focused,
            blink_on: opts.blink_on,
            show_scrollbar: opts.show_scrollbar,
        }
    }

    /// Сравнить с текущим экраном и вернуть повреждённые отрезки клеток.
    fn damage(&self, screen: &Screen, opts: &RenderOptions) -> FrameDamage {
        let cols = screen.cols();
        let rows = screen.rows();
        // Пока выделение есть, его оверлей живёт вне клеток: двигается он или
        // исчезает — перерисовываем кадр целиком (как и раньше).
        if self.cols != cols
            || self.rows != rows
            || self.view_offset != screen.view_offset()
            || self.scrollback != screen.scrollback_len()
            || self.selected
            || screen.selection().is_some()
            || self.show_scrollbar != opts.show_scrollbar
        {
            return FrameDamage::Full;
        }
        let mut runs: Vec<CellRun> = Vec::new();
        let mut changed = 0usize;
        for y in 0..rows {
            let line = screen.view_line(y);
            let mut x = 0;
            while x < cols {
                if line_cell(line, x) == self.cells[y * cols + x] {
                    x += 1;
                    continue;
                }
                let start = x;
                x += 1;
                while x < cols && line_cell(line, x) != self.cells[y * cols + x] {
                    x += 1;
                }
                changed += x - start;
                runs.push(CellRun::around(line, y, start, x, cols));
            }
        }
        // Курсор не входит в клетки: переезд, мигание и потеря фокуса
        // повреждают только его клетку.
        if self.cursor != (screen.cursor.x, screen.cursor.y)
            || self.cursor_visible != screen.cursor.visible
            || self.cursor_style != screen.cursor.style
            || self.cursor_blink != screen.cursor_blink
            || self.focused != opts.focused
            || self.blink_on != opts.blink_on
        {
            for (x, y) in [self.cursor, (screen.cursor.x, screen.cursor.y)] {
                if x < cols && y < rows {
                    let line = screen.view_line(y);
                    runs.push(CellRun::around(line, y, x, x + 1, cols));
                }
            }
        }
        // Смена фазы мигания гасит и зажигает клетки с атрибутом BLINK.
        if self.blink_on != opts.blink_on {
            let mut blinked = 0usize;
            for y in 0..rows {
                let line = screen.view_line(y);
                for x in 0..cols {
                    if line_cell(line, x).attrs.contains(Attrs::BLINK) {
                        blinked += 1;
                        if blinked > 128 {
                            return FrameDamage::Full;
                        }
                        runs.push(CellRun::around(line, y, x, x + 1, cols));
                    }
                }
            }
        }
        if changed * FULL_REDRAW_SHARE > cols * rows {
            return FrameDamage::Full;
        }
        FrameDamage::Runs(runs)
    }
}

/// Перерисовать отрезки поверх уже готового кадра.
///
/// Каждый отрезок рисуется как самостоятельный экран из одной строки: это
/// тот же рендер, что и в полном кадре, поэтому результат совпадает
/// пиксель-в-пиксель, а стоит O(клеток отрезка). Здесь нужен полный
/// [`render_screen_into`], а не `_cleared` из #164: область отрезка держит
/// пиксели прошлого кадра, а не заливку `theme.bg`, поэтому фон каждой клетки
/// обязан быть перерисован.
fn render_runs(
    canvas: &mut Canvas,
    screen: &Screen,
    theme: &Theme,
    opts: &RenderOptions,
    runs: &[CellRun],
) {
    let mut sub_opts = opts.clone();
    // Скроллбар живёт в правом поле и в damage клеток не входит.
    sub_opts.show_scrollbar = false;
    for run in runs {
        let width = run.x1.saturating_sub(run.x0);
        if width == 0 {
            continue;
        }
        let line = screen.view_line(run.y);
        let cells: Vec<Cell> = (run.x0..run.x1).map(|x| line_cell(line, x)).collect();
        let cursor_here =
            screen.cursor.y == run.y && screen.cursor.x >= run.x0 && screen.cursor.x < run.x1;
        let cursor = if cursor_here {
            (screen.cursor.x - run.x0, 0)
        } else {
            (0, 0)
        };
        let mut sub = Screen::from_cells(
            width,
            1,
            cells,
            cursor,
            cursor_here && screen.cursor.visible,
            screen.cursor.style,
        );
        sub.cursor_blink = screen.cursor_blink;
        // Реверс экрана (DECSCNM) — состояние экрана, а не клетки: суб-экран
        // обязан его унаследовать, иначе частичный кадр разойдётся с полным.
        sub.reverse_video = screen.reverse_video;
        render_screen_into(
            canvas,
            VIEWPORT_PADDING + run.x0 * CELL_W,
            VIEWPORT_PADDING + run.y * CELL_H,
            &sub,
            theme,
            &sub_opts,
        );
    }
}

/// Проводной damage из перерисованных отрезков.
fn runs_damage(runs: &[CellRun], width: usize, height: usize) -> Damage {
    let mut damage = Damage::new(Rect::new(0, 0, width as i32, height as i32));
    for run in runs {
        damage.add(run.rect());
    }
    damage
}

/// Нарисовать кадр и вернуть его damage (полный или только изменившиеся клетки).
fn paint_frame(
    canvas: &mut Canvas,
    screen: &Screen,
    theme: &Theme,
    opts: &RenderOptions,
    prev: &mut Option<FrameSnapshot>,
) -> Damage {
    let damage = match prev.as_ref().map(|snapshot| snapshot.damage(screen, opts)) {
        None | Some(FrameDamage::Full) => {
            // #164 (opt-level/row-store): фон заливается один раз, а
            // `render_screen_into_cleared` пропускает клетки с bg == theme.bg —
            // пиксель-в-пиксель то же, что полный `render_screen_into`, без
            // второй заливки каждой клетки.
            canvas.fill(theme.bg);
            render_screen_into_cleared(
                canvas,
                VIEWPORT_PADDING,
                VIEWPORT_PADDING,
                screen,
                theme,
                opts,
            );
            Damage::full(Rect::new(0, 0, canvas.width as i32, canvas.height as i32))
        }
        Some(FrameDamage::Runs(runs)) => {
            render_runs(canvas, screen, theme, opts, &runs);
            runs_damage(&runs, canvas.width, canvas.height)
        }
    };
    *prev = Some(FrameSnapshot::capture(screen, opts));
    damage
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
    // Снимок прошлого кадра: damage считается по разнице клеток, без хешей.
    let mut prev_frame: Option<FrameSnapshot> = None;
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
                let damage = paint_frame(
                    &mut frame.canvas,
                    &shell.terminal.screen,
                    &theme,
                    &render_opts,
                    &mut prev_frame,
                );
                match client.commit_shm(frame, &damage) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                        // Поколение устарело: следующий кадр — полный.
                        prev_frame = None;
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
                render_screen_into_cleared(
                    canvas,
                    VIEWPORT_PADDING,
                    VIEWPORT_PADDING,
                    &shell.terminal.screen,
                    &theme,
                    &render_opts,
                );
                client.commit(canvas).map_err(|e| format!("commit: {e}"))?;
                prev_frame = None;
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
                    let same_size = shm.as_ref().is_some_and(|frame| {
                        frame.canvas.width == width as usize
                            && frame.canvas.height == height as usize
                    });
                    if same_size {
                        dirty = true;
                        continue;
                    }
                    let resized = match shm.as_mut() {
                        Some(frame) => client.resize_shm(frame, width, height).unwrap_or(false),
                        None => false,
                    };
                    prev_frame = None;
                    // Новая поверхность (resize_shm) уже означает полный кадр;
                    // иначе уходим на обычный холст, как legacy-клиент.
                    if !resized {
                        if let Some(stale) = shm.take() {
                            let _ = client.release_shm(&stale);
                        }
                        fallback = Some(Canvas::new(width as usize, height as usize, theme.bg));
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
                        prev_frame = None;
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
    // A windowed terminal is a session, not a mirror: the boot VT carries the
    // kernel shell's log (and, on a desktop boot, is held foreground by
    // `lxrun`, so `/dev/vtinput` feeds the desktop rather than a prompt). The
    // window therefore gets its own shell child behind two pipes. Mirroring the
    // boot VT stays available for diagnostics via `--mirror-vt`; the headless
    // `--vt` snapshot path below is unchanged.
    if let Some(socket) = &args.socket {
        if !args.mirror_vt {
            match run_shell_session(args, socket) {
                Ok(()) => return Ok(()),
                Err(error) => eprintln!(
                    "pagh-terminal: shell session unavailable ({error}); falling back to the VT mirror"
                ),
            }
        }
    }

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

        // Нулевой дедлайн + неблокирующий сокет: забираем то, что уже пришло,
        // и возвращаемся к выводу shell'а. Иначе окно ждало бы только события
        // композитора и не заметило бы эхо/строку, напечатанную потомком.
        for event in client
            .poll_events(Duration::ZERO)
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
                            pagh_terminal::vtconsole::send_input(args.mirror_input(), &bytes)
                        {
                            if !input_error_reported {
                                eprintln!(
                                    "pagh-terminal: cannot write {}: {error}",
                                    args.mirror_input()
                                );
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

/// Отправить полный кадр, временно вернув сокету блокирующий режим.
///
/// UI-цикл держит сокет неблокирующим, чтобы не зависнуть в `read` (ядро
/// игнорирует `SO_RCVTIMEO`). Но `commit` — это `write` целого кадра; на
/// неблокирующем сокете большой кадр может быть принят частично и вернуть
/// `EAGAIN`. Блокирующая запись дожидается приёма всего кадра композитором.
fn commit_blocking(client: &mut SurfaceClient, canvas: &Canvas) -> Result<(), String> {
    client
        .set_nonblocking(false)
        .map_err(|e| format!("commit: {e}"))?;
    // Ядро отдаёт EAGAIN и на блокирующем сокете, когда буфер приёма
    // композитора полон (кадр больше него): ждём, пока он вычитает порцию,
    // вместо того чтобы ронять сессию на первом же большом кадре.
    let mut result = Ok(());
    for _ in 0..80 {
        match client.commit(canvas) {
            Ok(()) => {
                result = Ok(());
                break;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                result = Err(format!("commit: {error}"));
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                result = Err(format!("commit: {error}"));
                break;
            }
        }
    }
    client
        .set_nonblocking(true)
        .map_err(|e| format!("commit: {e}"))?;
    result
}

/// Оконная shell-сессия: отдельный процесс-shell за двумя pipe'ами.
///
/// Окно не читает `/dev/vtconsole0` и не пишет в `/dev/vtinput`: вывод потомка
/// идёт в собственный VT-парсер окна, клавиатура — в его stdin. Поэтому
/// boot-лог не появляется, а ввод не уходит в `lxrun`/desktop, который держит
/// консоль в форграунде.
fn run_shell_session(args: &Args, socket: &str) -> Result<(), String> {
    let child = match &args.shell_cmd {
        Some(program) => std::process::Command::new(program),
        None => session::self_shell_command(std::env::args().next().as_deref())
            .map_err(|e| format!("shell child: {e}"))?,
    };
    let mut shell = ShellSession::spawn(child).map_err(|e| format!("shell child: {e}"))?;

    let mut client = SurfaceClient::connect(socket).map_err(|e| format!("{socket}: {e}"))?;
    let welcome = client
        .handshake("pagh-terminal", Duration::from_secs(5))
        .map_err(|e| format!("handshake: {e}"))?;
    eprintln!(
        "pagh-terminal: session connected (client {}, screen {}x{})",
        welcome.client_id, welcome.screen_width, welcome.screen_height
    );
    client
        .create_window(args.window_id, args.size.0, args.size.1, &args.title)
        .map_err(|e| format!("create_window: {e}"))?;
    // Ядро игнорирует SO_RCVTIMEO, поэтому «чтение с таймаутом» здесь означало бы
    // вечную блокировку на простое и никогда не прочитанный вывод shell'а.
    let theme = Theme::default();
    let mut width = args.size.0.max(64);
    let mut height = args.size.1.max(32);
    let (cols, rows) = layout(width, height);
    let mut terminal = pagh_terminal::Terminal::with_scrollback(cols, rows, 2000);

    // ShmSurface: рендер прямо в общий mmap, по сокету — только damage. Это не
    // оптимизация «на будущее», а разница между отзывчивым эхом и кадром 2 МиБ
    // на каждую нажатую клавишу (в QEMU-TCG такой кадр идёт сотни миллисекунд).
    // Сервер объявляет capability битом 2 welcome; если что-то не сложилось —
    // молча откатываемся на полный кадр, как legacy-клиент.
    let mut shm = match client.enable_shm(width, height) {
        Ok(shm) => shm,
        Err(error) => {
            eprintln!("pagh-terminal: shm unavailable ({error}); full frames");
            None
        }
    };
    let mut fallback: Option<Canvas> = if shm.is_none() {
        Some(Canvas::new(width as usize, height as usize, theme.bg))
    } else {
        None
    };
    // Снимок прошлого кадра: damage — разница клеток, без хешей канваса.
    let mut prev_frame: Option<FrameSnapshot> = None;
    let render_opts = RenderOptions::default();
    // Ядро игнорирует SO_RCVTIMEO, поэтому «чтение с таймаутом» здесь означало бы
    // вечную блокировку на простое и никогда не прочитанный вывод shell'а.
    client
        .set_nonblocking(true)
        .map_err(|e| format!("nonblocking: {e}"))?;

    let mut dirty = true;
    let mut exited_reported = false;
    let mut input_error_reported = false;
    let mut last_commit = Instant::now();

    loop {
        // Вывод shell'а приходит асинхронно: забираем всё, что уже прочитал
        // reader-поток, и отдаём парсеру. Пустой канал — не повод крутить кадр.
        if let Some(bytes) = shell.try_read() {
            if !bytes.is_empty() {
                terminal.feed(&bytes);
                dirty = true;
            }
        }

        // Сообщение о выходе печатаем один раз и только когда из pipe'ов
        // дочитано всё, иначе последняя строка shell'а потерялась бы.
        if !exited_reported {
            if let Ok(Some(status)) = shell.try_wait() {
                if shell.output_closed() {
                    terminal.feed_str(&format!(
                        "\r\n[terminal session] shell exited ({status})\r\n"
                    ));
                    dirty = true;
                    exited_reported = true;
                }
            }
        }

        // Нулевой дедлайн + неблокирующий сокет: забираем то, что уже пришло,
        // и возвращаемся к выводу shell'а. Иначе окно ждало бы только события
        // композитора и не заметило бы эхо/строку, напечатанную потомком.
        for event in client
            .poll_events(Duration::ZERO)
            .map_err(|e| format!("poll: {e}"))?
        {
            match event {
                Event::Configure(configure) => {
                    width = configure.width.max(64);
                    height = configure.height.max(32);
                    let (cols, rows) = layout(width, height);
                    terminal.resize(cols, rows);
                    let same_size = shm.as_ref().is_some_and(|frame| {
                        frame.canvas.width == width as usize
                            && frame.canvas.height == height as usize
                    });
                    if same_size {
                        dirty = true;
                        continue;
                    }
                    let resized = match shm.as_mut() {
                        Some(frame) => client.resize_shm(frame, width, height).unwrap_or(false),
                        None => false,
                    };
                    prev_frame = None;
                    // Новая поверхность (resize_shm) уже означает полный кадр;
                    // иначе уходим на обычный холст, как legacy-клиент.
                    if !resized {
                        if let Some(stale) = shm.take() {
                            let _ = client.release_shm(&stale);
                        }
                        fallback = Some(Canvas::new(width as usize, height as usize, theme.bg));
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
                        prev_frame = None;
                        dirty = true;
                    }
                }
                Event::ShmReady { .. } => {}
                Event::Input(InputEvent::Key {
                    code,
                    pressed,
                    modifiers,
                }) => {
                    let modes = KeyModes::from_screen(&terminal.screen);
                    let bytes = keys::encode_key(code, pressed, modifiers, &modes);
                    if bytes.is_empty() {
                        continue;
                    }
                    if let Err(error) = shell.write_input(&bytes) {
                        if !input_error_reported {
                            eprintln!("pagh-terminal: shell input: {error}");
                            input_error_reported = true;
                        }
                    }
                }
                Event::Input(InputEvent::Axis { vertical, .. }) => {
                    if vertical != 0 {
                        terminal
                            .screen
                            .scroll_view(if vertical > 0 { -3 } else { 3 });
                        dirty = true;
                    }
                }
                Event::Focus { focused, .. } => {
                    terminal.screen.cursor.visible = focused;
                    dirty = true;
                }
                Event::Close { reason, .. } => {
                    eprintln!(
                        "pagh-terminal: server closed the window ({})",
                        close_name(reason)
                    );
                    shell.shutdown();
                    return Ok(());
                }
                Event::Error { code, message } => {
                    eprintln!(
                        "pagh-terminal: server error {} ({}): {}",
                        error_name(code),
                        code,
                        message
                    );
                }
                _ => {}
            }
        }
        // Кадр — полный (2 МиБ на 900×600), поэтому отправлять его на каждое
        // эхо-нажатие нельзя: окно перестало бы успевать читать ввод, пока
        // композитор принимает предыдущий кадр. 40 мс — 25 кадров/с, при этом
        // события выше уже разобраны, так что Enter не ждёт за кадром.
        let now = Instant::now();
        if dirty && now.duration_since(last_commit) >= Duration::from_millis(40) {
            if let Some(frame) = shm.as_mut() {
                let damage = paint_frame(
                    &mut frame.canvas,
                    &terminal.screen,
                    &theme,
                    &render_opts,
                    &mut prev_frame,
                );
                match client.commit_shm(frame, &damage) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                        // Поколение устарело (композитор пересоздал поверхность):
                        // дальше — полный кадр, как legacy-клиент.
                        if let Some(stale) = shm.take() {
                            let _ = client.release_shm(&stale);
                        }
                        fallback = Some(Canvas::new(width as usize, height as usize, theme.bg));
                        prev_frame = None;
                    }
                    Err(error) => return Err(format!("commit_shm: {error}")),
                }
            } else if let Some(canvas) = fallback.as_mut() {
                canvas.fill(theme.bg);
                render_screen_into_cleared(
                    canvas,
                    VIEWPORT_PADDING,
                    VIEWPORT_PADDING,
                    &terminal.screen,
                    &theme,
                    &render_opts,
                );
                commit_blocking(&mut client, canvas)?;
                prev_frame = None;
            }
            dirty = false;
            last_commit = Instant::now();
        }
        // Короткий сон: ввод и вывод shell'а замечаются в пределах кадра, а
        // простаивающее окно не крутит CPU.
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Оконный режим по умолчанию: сессия ядра (`/dev/vtfb1` + `/dev/vtinput1`).
///
/// Ядро владеет shell'ем целиком — командами, эхом, редактированием строки,
/// history и scrollback'ом; клиент блитит пиксели его поверхности и передаёт
/// клавиатуру VT-байтами.  Ни одна команда здесь не имитируется: если
/// устройство не отвечает, окно говорит об этом прямо.
///
/// Поведение при отказе:
///
/// * `/dev/vtfb1` (или `/dev/vtinput1`) отсутствует — старое ядро: честная
///   строка в stderr и откат на прежнюю pipe-сессию с userspace-shell'ом;
/// * устройство есть, но ioctl/mmap/read/write не работают — честное сообщение
///   **в окне** и выход с кодом 2, без отката в демо;
/// * `exit` в сессии (`ENODEV`): сообщение «сессия завершена», окно закрывается.
///   Реконнект — повторный запуск окна: первый `open` спавнит новую сессию.
fn run_kernel_session(args: &Args, socket: &str) -> Result<(), String> {
    let fb_path = args.vt_fb.as_str();
    let input_path = args.kernel_input();

    let source = match devfs::open_frame_source(fb_path) {
        Ok(source) => source,
        Err(devfs::OpenError::Missing) => {
            eprintln!(
                "pagh-terminal: {fb_path} is not present (kernel without the shell-session \
                 surface); falling back to the private userspace shell session"
            );
            return run_shell_session(args, socket);
        }
        Err(devfs::OpenError::Failed(reason)) => return fail_with_notice(args, socket, &reason),
    };
    let key_sink = match FileInput::open(input_path) {
        Ok(sink) => sink,
        Err(error) if is_missing_device(&error) => {
            eprintln!(
                "pagh-terminal: {input_path} is not present (kernel without the shell-session \
                 input); falling back to the private userspace shell session"
            );
            return run_shell_session(args, socket);
        }
        Err(error) => return fail_with_notice(args, socket, &format!("{input_path}: {error}")),
    };
    let mut frame = match KernelFrame::open(source) {
        Ok(frame) => frame,
        Err(error) => return fail_with_notice(args, socket, &error.message()),
    };
    let info = frame.info();
    eprintln!(
        "pagh-terminal: kernel session {}x{} (stride {}, generation {}) on {fb_path}",
        info.width, info.height, info.stride, info.generation
    );

    let mut client = SurfaceClient::connect(socket).map_err(|e| format!("{socket}: {e}"))?;
    let welcome = client
        .handshake("pagh-terminal", Duration::from_secs(5))
        .map_err(|e| format!("handshake: {e}"))?;
    eprintln!(
        "pagh-terminal: kernel session connected (client {}, screen {}x{})",
        welcome.client_id, welcome.screen_width, welcome.screen_height
    );
    client
        .create_window(args.window_id, args.size.0, args.size.1, &args.title)
        .map_err(|e| format!("create_window: {e}"))?;
    // Ядро игнорирует SO_RCVTIMEO (см. TERMINAL.md), поэтому окно обязано
    // опрашивать сокет неблокирующе и само возвращаться к кадру поверхности.
    client
        .set_nonblocking(true)
        .map_err(|e| format!("nonblocking: {e}"))?;

    let theme = Theme::default();
    let mut width = args.size.0.max(64);
    let mut height = args.size.1.max(32);
    // ShmSurface: блит пишется прямо в общий mmap, по сокету идут только
    // damage-прямоугольники. Для пиксельной поверхности сессии это не
    // оптимизация, а разница между «окно оживает» и «3 МиБ на кадр в QEMU-TCG».
    let mut shm = match client.enable_shm(width, height) {
        Ok(shm) => shm,
        Err(error) => {
            eprintln!("pagh-terminal: shm unavailable ({error}); full frames");
            None
        }
    };
    let mut fallback: Option<Canvas> = if shm.is_none() {
        Some(Canvas::new(width as usize, height as usize, theme.bg))
    } else {
        None
    };
    let mut tracker = pagh_ui::damage::DamageTracker::new(
        pagh_ui::geom::Rect::new(0, 0, width as i32, height as i32),
        pagh_ui::damage::DEFAULT_TILE,
    );
    let mut forwarder = KeyForwarder::new(key_sink);
    let mut dirty = true;
    let mut last_commit = Instant::now();
    let mut input_error_reported = false;
    // `--vt-trace` (или переменная окружения) печатает каждый отрисованный
    // кадр: в гостевой консоли это serial-доказательство того, что generation
    // растёт с выводом сессии, а не «кадр застыл».
    let trace = args.vt_trace || std::env::var_os("PAGH_TERMINAL_KERNEL_TRACE").is_some();

    loop {
        // 1. Кадр сессии: перерисовываем окно только когда ядро опубликовало
        //    новый generation — иначе простой не жжёт CPU на блит.
        {
            let canvas = canvas_mut(&mut shm, &mut fallback);
            match frame.draw_if_changed(canvas, &theme) {
                Ok(Draw::Drawn { generation }) => {
                    if trace {
                        eprintln!("pagh-terminal: kernel frame generation {generation} drawn");
                    }
                    dirty = true;
                }
                Ok(Draw::Unchanged) => {}
                Err(VtFbError::Ended) => {
                    return end_kernel_session(
                        &mut client,
                        &mut shm,
                        &theme,
                        width,
                        height,
                        ENDED_NOTICE,
                    );
                }
                Err(error) => {
                    let message = error.message();
                    let lines = [
                        "[kernel session] the device failed; nothing is simulated instead",
                        "",
                        message.as_str(),
                        "",
                        "Close this window and relaunch pagh-terminal to retry.",
                    ];
                    let _ =
                        end_kernel_session(&mut client, &mut shm, &theme, width, height, &lines);
                    return Err(message);
                }
            }
        }

        // 2. Отправка кадра — не чаще 60 Гц: полный кадр на каждое изменение
        //    generation не оставляет времени на чтение ввода.
        if dirty && last_commit.elapsed() >= Duration::from_millis(16) {
            // `retry_full`: кадр не ушёл, нужен полный блит в новый холст.
            let mut retry_full = false;
            if let Some(shm_frame) = shm.as_mut() {
                let damage = tracker.update(&shm_frame.canvas);
                match client.commit_shm(shm_frame, damage) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                        // Поколение поверхности устарело (композитор пересоздал
                        // memfd): дальше — полный кадр, как legacy-клиент.
                        if let Some(stale) = shm.take() {
                            let _ = client.release_shm(&stale);
                        }
                        fallback = Some(Canvas::new(width as usize, height as usize, theme.bg));
                        tracker.resize(pagh_ui::geom::Rect::new(0, 0, width as i32, height as i32));
                        // Новый холст — нужен полный блит, даже если generation
                        // поверхности не менялась.
                        retry_full = true;
                    }
                    Err(error) => return Err(format!("commit_shm: {error}")),
                }
            } else if let Some(canvas) = fallback.as_mut() {
                commit_blocking(&mut client, canvas)?;
            }
            if retry_full {
                frame.invalidate();
            }
            dirty = retry_full;
            last_commit = Instant::now();
        }

        // 3. События окна. Клавиши → VT-байты в `/dev/vtinput1`; колесо и
        //    протяжка не транслируются: scrollback и selection живут в ядре.
        //    Нулевой дедлайн: ядро игнорирует SO_RCVTIMEO, поэтому «чтение с
        //    таймаутом» на блокирующем сокете означало бы вечное ожидание, а
        //    окно обязано возвращаться к кадру поверхности.
        for event in client
            .poll_events(Duration::ZERO)
            .map_err(|e| format!("poll: {e}"))?
        {
            match event {
                Event::Configure(configure) => {
                    width = configure.width.max(64);
                    height = configure.height.max(32);
                    let same_size = shm.as_ref().is_some_and(|frame| {
                        frame.canvas.width == width as usize
                            && frame.canvas.height == height as usize
                    });
                    if same_size {
                        frame.invalidate();
                        dirty = true;
                        continue;
                    }
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
                    // Масштаб под новый content rect меняется — полный блит.
                    frame.invalidate();
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
                        frame.invalidate();
                        dirty = true;
                    }
                }
                Event::ShmReady { .. } => {}
                Event::Input(InputEvent::Key {
                    code,
                    pressed,
                    modifiers,
                }) => match forwarder.handle_key(code, pressed, modifiers) {
                    Ok(_) => {}
                    Err(error) if is_session_end(&error) => {
                        // Сессия закончилась между кадром и нажатием: следующий
                        // опрос поверхности вернёт ENODEV и закроет окно честно.
                        if !input_error_reported {
                            eprintln!(
                                "pagh-terminal: session ended while forwarding a key ({error})"
                            );
                            input_error_reported = true;
                        }
                    }
                    Err(error) => {
                        if !input_error_reported {
                            eprintln!("pagh-terminal: cannot write {input_path}: {error}");
                            input_error_reported = true;
                        }
                    }
                },
                Event::Focus { .. } => {
                    // Курсор рисует ядро в пикселях: подсветка фокуса здесь не наша.
                }
                Event::Input(InputEvent::Axis { .. }) | Event::Input(InputEvent::Button { .. }) => {
                    // Скроллбек и selection принадлежат сессии ядра; у окна нет
                    // доступа к её сетке, поэтому событие честно игнорируется.
                }
                Event::Input(InputEvent::Motion { .. }) => {}
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
                        error_name(code),
                        code,
                        message
                    );
                }
                Event::Ping(_) | Event::Pong(_) => {}
            }
        }

        // Простой: не крутим CPU, но и не спим дольше одного кадра. Сессия
        // ядра флашит generation на каждую операцию shell'а, поэтому 6 мс —
        // это ≤ ~160 проверок ioctl/с, дешевле любой перерисовки.
        std::thread::sleep(Duration::from_millis(6));
    }
}

/// Что показать в окне, когда `exit` в сессии ядра завершил её (`ENODEV`).
const ENDED_NOTICE: &[&str] = &[
    "[kernel session] the shell session has ended.",
    "",
    "The kernel starts a fresh session on the next open, so relaunch",
    "pagh-terminal to reconnect. Nothing is simulated in the meantime.",
];

/// Получить текущий холст окна (общий mmap или локальный fallback).
fn canvas_mut<'a>(
    shm: &'a mut Option<ShmFrame>,
    fallback: &'a mut Option<Canvas>,
) -> &'a mut Canvas {
    match shm.as_mut() {
        Some(frame) => &mut frame.canvas,
        None => fallback
            .as_mut()
            .expect("fallback canvas exists whenever shm is absent"),
    }
}

/// Финальный кадр с честным сообщением: отдать его окну и закрыть клиент.
///
/// ShmSurface отпускается, кадр уходит полным `commit` — сообщение должно
/// дойти даже если memfd уже невалиден.
fn end_kernel_session(
    client: &mut SurfaceClient,
    shm: &mut Option<ShmFrame>,
    theme: &Theme,
    width: u32,
    height: u32,
    lines: &[&str],
) -> Result<(), String> {
    show_notice(client, shm, theme, width, height, lines)?;
    let _ = client.close();
    eprintln!("pagh-terminal: kernel session ended (the shell exited); window closed");
    Ok(())
}

/// Нарисовать сообщение клиента в окне и подержать его `NOTICE_HOLD`.
fn show_notice(
    client: &mut SurfaceClient,
    shm: &mut Option<ShmFrame>,
    theme: &Theme,
    width: u32,
    height: u32,
    lines: &[&str],
) -> Result<(), String> {
    if let Some(stale) = shm.take() {
        let _ = client.release_shm(&stale);
    }
    let mut canvas = Canvas::new(width.max(64) as usize, height.max(32) as usize, theme.bg);
    render_notice(&mut canvas, theme, lines);
    commit_blocking(client, &canvas)?;
    std::thread::sleep(NOTICE_HOLD);
    Ok(())
}

/// Устройство есть, но работать с ним нельзя: честное сообщение в окне.
///
/// Окно создаётся только чтобы сказать правду (запуск из шелла без дисплея
/// просто получит ошибку в stderr). Никакого отката в демо-шелл: пользователь
/// должен видеть, что сессии ядра нет.
fn fail_with_notice(args: &Args, socket: &str, reason: &str) -> Result<(), String> {
    eprintln!("pagh-terminal: kernel session unavailable: {reason}");
    let mut client = SurfaceClient::connect(socket).map_err(|e| format!("{socket}: {e}"))?;
    client
        .handshake("pagh-terminal", Duration::from_secs(5))
        .map_err(|e| format!("handshake: {e}"))?;
    client
        .create_window(args.window_id, args.size.0, args.size.1, &args.title)
        .map_err(|e| format!("create_window: {e}"))?;
    let theme = Theme::default();
    let lines = [
        "[kernel session] the kernel shell-session devices are not usable",
        "",
        reason,
        "",
        "The window renders the kernel's shell session over",
        "/dev/vtfb1 + /dev/vtinput1; it does not simulate commands.",
    ];
    let mut no_shm = None;
    show_notice(
        &mut client,
        &mut no_shm,
        &theme,
        args.size.0,
        args.size.1,
        &lines,
    )?;
    let _ = client.close();
    Err(reason.to_string())
}

fn render_frame(screen: &pagh_terminal::Screen, theme: &Theme, width: u32, height: u32) -> Canvas {
    let width = width.max(1) as usize;
    let height = height.max(1) as usize;
    let mut canvas = Canvas::new(width, height, theme.bg);
    render_screen_into_cleared(
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
            "--vt-input" => out.vt_input = Some(next_value(&mut args, "--vt-input")?),
            "--vt-fb" => out.vt_fb = next_value(&mut args, "--vt-fb")?,
            "--vt-trace" => out.vt_trace = true,
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
            "--shell-child" => out.shell_child = true,
            "--shell-cmd" => {
                out.shell_cmd = Some(PathBuf::from(next_value(&mut args, "--shell-cmd")?))
            }
            "--mirror-vt" => out.mirror_vt = true,
            "--pipe-shell" => out.pipe_shell = true,
            "--demo-shell" => out.demo_shell = true,
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
  pagh-terminal --socket PATH [--size WxH] [--title TITLE]
                [--vt-fb /dev/vtfb1] [--vt-input /dev/vtinput1]
  pagh-terminal --vt /dev/vtconsole0 [--socket PATH] [--vt-input /dev/vtinput]
                [--mirror-vt]
  pagh-terminal --screenshots DIR

WINDOW (default with --socket): KERNEL SHELL SESSION
  The window attaches to the kernel's own shell session: it blits the pixels of
  /dev/vtfb1 into the window and forwards window keys as VT bytes to
  /dev/vtinput1.  Commands (df, ps, ppkg, lxrun, disk, nano, history, line
  editing, scrollback) are the kernel's — the client renders and forwards input
  only, and simulates nothing.
  --vt-fb PATH      session pixel surface (default /dev/vtfb1)
  --vt-input PATH   session input device (default /dev/vtinput1)
  --vt-trace        log every drawn frame (its kernel generation) to stderr;
                   the guest console forwards that to the serial log
  If the devices are missing (kernel without the session surface) the window
  says so on stderr and falls back to the private userspace shell session.  If
  the devices exist but fail, the window shows the honest error and exits; it
  never pretends to be a shell.

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
  --window-id N    client-chosen window id (default 101)
  --feed FILE      preload terminal output into the demo shell
  --demo-shell     explicit demo-shell window (screenshots, A/B comparison)
  --pipe-shell     explicit userspace shell over two pipes (fallback/test mode;
                   minimal built-in commands, not the kernel shell)
  --vt PATH        kernel VT console shared surface (ABI v1): with --socket the
                   window mirrors the boot VT; without --socket it prints one
                   snapshot (diagnostics)
  --vt-input PATH  input device override for --vt/--mirror-vt (default
                   /dev/vtinput) and for the kernel session (default
                   /dev/vtinput1)
  --mirror-vt      mirror the boot VT in the window (diagnostics; the boot VT is
                   held by lxrun).  Without --vt the path defaults to
                   /dev/vtconsole0
  --shell-child    internal: run as the session's shell process (pipes)
  --shell-cmd PATH child program for the session (default: this binary with
                   --shell-child; use a real shell when the image has one)

NOT IN THIS STAGE (honest gaps): no PTY/termios, no TIOCGWINSZ/SIGWINCH (the
kernel owns the shell size), no terminal job control, and the pixel surface
carries no DEC private modes — arrow keys are always sent in normal mode.  One
kernel session per window; scrollback and selection stay in the kernel.

SCREENSHOTS:
  --screenshots DIR  regenerate the committed demo PNG fixtures

Environment: PAGH_DESKTOP_SOCKET overrides the default socket path
({}).",
        pagh_terminal::client::DEFAULT_SOCKET
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(cols: usize, rows: usize) -> (usize, usize) {
        (
            VIEWPORT_PADDING * 2 + cols * CELL_W,
            VIEWPORT_PADDING * 2 + rows * CELL_H,
        )
    }

    fn screen_canvas(cols: usize, rows: usize, theme: &Theme) -> Canvas {
        let (width, height) = grid(cols, rows);
        Canvas::new(width, height, theme.bg)
    }

    /// Полный кадр в отдельной поверхности (эталон для сравнения).
    fn full_frame(
        screen: &Screen,
        theme: &Theme,
        opts: &RenderOptions,
        cols: usize,
        rows: usize,
    ) -> Canvas {
        let mut canvas = screen_canvas(cols, rows, theme);
        let mut prev = None;
        let damage = paint_frame(&mut canvas, screen, theme, opts, &mut prev);
        assert!(damage.is_full(), "первый кадр обязан быть полным");
        canvas
    }

    fn terminal_with(cols: usize, rows: usize, text: &[u8]) -> Terminal {
        let mut terminal = Terminal::new(cols, rows);
        terminal.feed(text);
        terminal
    }

    #[test]
    fn first_frame_is_full_and_idle_frame_is_empty() {
        let theme = Theme::default();
        let opts = RenderOptions::default();
        let terminal = terminal_with(40, 8, b"hello");
        let mut canvas = screen_canvas(40, 8, &theme);
        let mut prev = None;
        assert!(paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev).is_full());
        // Ничего не менялось — повреждений нет, кадр не перерисовывается.
        let damage = paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev);
        assert!(
            damage.is_empty(),
            "простой кадр не должен ничего повреждать"
        );
    }

    #[test]
    fn typing_damages_one_cell_and_matches_full_frame() {
        let theme = Theme::default();
        let opts = RenderOptions::default();
        let mut terminal = terminal_with(80, 24, b"$ ls");
        let (width, height) = grid(80, 24);
        let mut canvas = screen_canvas(80, 24, &theme);
        let mut prev = None;
        paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev);

        terminal.feed(b" -l");
        let damage = paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev);
        assert!(!damage.is_full(), "ввод символа не повод для полного кадра");
        // Клетка + запас на сглаживание: единицы клеток, а не экран.
        let window = width as i64 * height as i64;
        assert!(
            damage.area() * 100 < window,
            "ввод повредил {} из {window} px",
            damage.area()
        );
        let reference = full_frame(&terminal.screen, &theme, &opts, 80, 24);
        assert!(
            canvas == reference,
            "частичный кадр разошёлся с полным после ввода"
        );
    }

    #[test]
    fn cursor_blink_damages_only_the_cursor_cell() {
        let theme = Theme::default();
        let opts = RenderOptions::default();
        let terminal = terminal_with(80, 24, b"$ echo hello");
        let (_, _) = grid(80, 24);
        let mut canvas = screen_canvas(80, 24, &theme);
        let mut prev = None;
        paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev);

        // Мигание — это opts.blink_on, а не содержимое клеток.
        let mut blinked = opts.clone();
        blinked.blink_on = false;
        let damage = paint_frame(&mut canvas, &terminal.screen, &theme, &blinked, &mut prev);
        assert!(
            !damage.is_full(),
            "мигание курсора не полноэкранное событие"
        );
        assert!(
            // Курсор + две клетки запаса с каждой стороны = 5 клеток.
            damage.area() <= 6 * (CELL_W * CELL_H) as i64,
            "мигание повредило {} px (клетка {} px)",
            damage.area(),
            CELL_W * CELL_H
        );
        let reference = full_frame(&terminal.screen, &theme, &blinked, 80, 24);
        assert!(
            canvas == reference,
            "частичный кадр разошёлся с полным при мигании"
        );
    }

    #[test]
    fn scrolling_and_selection_ask_for_a_full_frame() {
        let theme = Theme::default();
        let opts = RenderOptions::default();
        // Прокрутка возможна только при непустой истории.
        let text: Vec<u8> = (0..24)
            .flat_map(|i| format!("line {i}\r\n").into_bytes())
            .collect();
        let mut terminal = terminal_with(40, 8, &text);
        assert!(
            terminal.screen.scrollback_len() > 0,
            "нет истории для прокрутки"
        );
        let mut canvas = screen_canvas(40, 8, &theme);
        let mut prev = None;
        paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev);

        terminal.screen.scroll_view(1);
        let damage = paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev);
        assert!(
            damage.is_full(),
            "прокрутка меняет все строки — полный кадр"
        );

        terminal.screen.scroll_view(-1);
        paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev);
        terminal
            .screen
            .begin_selection_view(0, 0, SelectionMode::Char);
        let damage = paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev);
        assert!(damage.is_full(), "выделение — оверлей вне клеток");
    }

    #[test]
    fn reverse_video_survives_a_partial_frame() {
        let theme = Theme::default();
        let opts = RenderOptions::default();
        let mut terminal = terminal_with(40, 8, b"one\r\ntwo");
        terminal.screen.reverse_video = true;
        let mut canvas = screen_canvas(40, 8, &theme);
        let mut prev = None;
        paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev);
        terminal.feed(b" three");
        let damage = paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev);
        assert!(!damage.is_full());
        let reference = full_frame(&terminal.screen, &theme, &opts, 40, 8);
        assert!(
            canvas == reference,
            "реверс экрана потерялся в частичном кадре"
        );
    }

    #[test]
    fn wide_glyph_run_includes_both_cells() {
        let theme = Theme::default();
        let opts = RenderOptions::default();
        let mut terminal = terminal_with(20, 4, b"ab");
        let mut canvas = screen_canvas(20, 4, &theme);
        let mut prev = None;
        paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev);
        // Широкий символ (CJK) занимает две клетки.
        terminal.feed("世".as_bytes());
        let damage = paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev);
        assert!(!damage.is_full());
        let reference = full_frame(&terminal.screen, &theme, &opts, 20, 4);
        assert!(
            canvas == reference,
            "половина широкого глифа потерялась в частичном кадре"
        );
    }

    /// Честные счётчики для PR: клетка против окна.
    #[test]
    fn event_cost_counters() {
        let theme = Theme::default();
        let opts = RenderOptions::default();
        let mut terminal = terminal_with(80, 24, b"$ ls");
        let (width, height) = grid(80, 24);
        let mut canvas = screen_canvas(80, 24, &theme);
        let mut prev = None;
        paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev);

        terminal.feed(b"x");
        let typing = paint_frame(&mut canvas, &terminal.screen, &theme, &opts, &mut prev);
        let mut blinked = opts.clone();
        blinked.blink_on = false;
        let blink = paint_frame(&mut canvas, &terminal.screen, &theme, &blinked, &mut prev);
        eprintln!(
            "terminal damage: window {}x{} = {} px; typing {} px; blink {} px",
            width,
            height,
            width * height,
            typing.area(),
            blink.area()
        );
        assert!(typing.area() * 100 < (width * height) as i64);
        assert!(blink.area() * 100 < (width * height) as i64);
    }
}
