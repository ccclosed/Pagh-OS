//! `paint` — a framebuffer drawing application that runs in a window.
//!
//! Driven directly by the PS/2 mouse ([`crate::drivers::ps2_mouse`]) and the
//! keyboard, `paint` draws itself as a desktop window with a title bar
//! (minimize / maximize / close buttons) and a matching taskbar button, so it
//! can be hidden, stretched to full screen, or closed. It keeps an
//! authoritative backing canvas in the heap (one `u32` per pixel) so it can
//! offer features that need to read the image back — shape preview
//! (rubber-banding), flood fill, undo, and save/load to disk.
//!
//! Layout (top → bottom):
//!   * the window title bar (drag strip + window controls),
//!   * a toolbar strip (palette swatches + current tool / color / brush size),
//!   * the canvas (the drawing surface), and
//!   * the taskbar at the very bottom (OS label + "Paint" button + live info).
//!
//! Tools: Pencil, Eraser, Line, Rectangle, Filled Rectangle, Circle, Disc,
//! Bucket fill, and color Picker. The current color comes from a 16-entry
//! palette; the left mouse button paints with it and the right button paints
//! with white (quick erase).

use alloc::vec;
use alloc::vec::Vec;
use alloc::format;
use alloc::string::String;

use crate::drivers::{cursor, framebuffer, ps2_mouse};
use super::keys::{Decoder, KeyEvent};

/// Top row of the toolbar holds the palette + current-color chip + label.
const PALETTE_ROW_H: usize = 28;
/// Second row holds the clickable tool buttons.
const TOOL_ROW_H: usize = 24;
/// Total toolbar height (both rows stacked).
const TOOLBAR_H: usize = PALETTE_ROW_H + TOOL_ROW_H;
const SWATCH: usize = 22; // palette swatch width/height in px
const CHAR_W: usize = 8; // framebuffer glyph width in px

const WHITE: u32 = 0xFFFFFF;
const TOOLBAR_BG: u32 = 0x2C2C34;

/// Height of the window title bar (drag strip + window buttons).
const TITLEBAR_H: usize = 22;
/// Width/height padding of the window's drawn border.
const BORDER: usize = 2;
/// Desktop backdrop painted behind the window in windowed/minimized states.
const DESKTOP_BG: u32 = 0x0E5A6E;
/// Title-bar background when the window has focus.
const TITLE_BG: u32 = 0x1B3A5B;
/// Square size (px) of each title-bar button (minimize / maximize / close).
const WIN_BTN_W: usize = 18;

/// Window placement state. `paint` is a single, modal window; these describe
/// whether it occupies a floating rectangle, the whole screen, or is hidden to
/// the taskbar.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WinMode {
    /// Floating window centered on the desktop.
    Windowed,
    /// Window stretched to fill the screen (above the taskbar).
    Maximized,
}

/// A title-bar window control.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TitleBtn {
    Minimize,
    Maximize,
    Close,
}

/// Tools shown as buttons on the second toolbar row, left to right.
const TOOLS: [Tool; 9] = [
    Tool::Pencil,
    Tool::Eraser,
    Tool::Line,
    Tool::Rect,
    Tool::FilledRect,
    Tool::Circle,
    Tool::Disc,
    Tool::Fill,
    Tool::Picker,
];

/// Compute the `(tool, x, width)` layout of the tool-button row. Shared by the
/// renderer and the click hit-test so they can never disagree.
fn tool_buttons() -> impl Iterator<Item = (Tool, usize, usize)> {
    let mut x = 4usize;
    TOOLS.iter().map(move |&t| {
        let w = t.name().len() * CHAR_W + 8;
        let bx = x;
        x += w + 4;
        (t, bx, w)
    })
}

/// Width (px) of the brush `-` / `+` buttons.
const BRUSH_BTN_W: usize = 22;

/// Pixel layout of the brush-size controls (right of the tool buttons).
struct BrushUi {
    minus_x: usize,
    label_x: usize,
    plus_x: usize,
    end_x: usize,
}

/// X coordinate just past the last tool button.
fn tools_end_x() -> usize {
    let mut x = 4usize;
    for &t in TOOLS.iter() {
        x += (t.name().len() * CHAR_W + 8) + 4;
    }
    x
}

/// Layout for the brush `-`, size label, and `+` controls. Shared by the
/// renderer and the click hit-test.
fn brush_ui() -> BrushUi {
    let minus_x = tools_end_x() + 16;
    let label_x = minus_x + BRUSH_BTN_W + 6;
    let label_w = 6 * CHAR_W; // room for "Sz:NN"
    let plus_x = label_x + label_w;
    let end_x = plus_x + BRUSH_BTN_W;
    BrushUi { minus_x, label_x, plus_x, end_x }
}

/// 16-color palette (indices 0..15; number keys 1..0 pick the first ten).
const PALETTE: [u32; 16] = [
    0x000000, // black
    0xFFFFFF, // white
    0xE6261F, // red
    0xF7791A, // orange
    0xF7D038, // yellow
    0x6BA539, // green
    0x149414, // dark green
    0x1FB6E6, // cyan
    0x2B5DF2, // blue
    0x4B2BF2, // indigo
    0x9B2BF2, // violet
    0xF22BC8, // magenta
    0x8B5A2B, // brown
    0x808080, // gray
    0xC0C0C0, // light gray
    0x404040, // dark gray
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Pencil,
    Eraser,
    Line,
    Rect,
    FilledRect,
    Circle,
    Disc,
    Fill,
    Picker,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Tool::Pencil => "Pencil",
            Tool::Eraser => "Eraser",
            Tool::Line => "Line",
            Tool::Rect => "Rect",
            Tool::FilledRect => "FillRect",
            Tool::Circle => "Circle",
            Tool::Disc => "Disc",
            Tool::Fill => "Bucket",
            Tool::Picker => "Picker",
        }
    }

    /// True for the click-drag-release shape tools that show a live preview.
    fn is_shape(self) -> bool {
        matches!(self, Tool::Line | Tool::Rect | Tool::FilledRect | Tool::Circle | Tool::Disc)
    }
}

struct PaintApp {
    /// Full framebuffer dimensions (the desktop extent).
    screen_w: usize,
    screen_h: usize,

    // Window geometry (outer rectangle, including title bar + border).
    win_x: usize,
    win_y: usize,
    win_w: usize,
    win_h: usize,
    mode: WinMode,
    /// When true the window is hidden and only its taskbar button shows.
    minimized: bool,
    /// True while the title bar is being dragged with the left button held.
    win_drag: bool,
    /// Mouse offset from the window origin captured when the drag began.
    win_drag_dx: i32,
    win_drag_dy: i32,

    cw: usize,
    ch: usize,
    /// Framebuffer X/Y of canvas pixel (0, 0).
    canvas_left: usize,
    canvas_top: usize,
    canvas: Vec<u32>,
    undo: Vec<u32>,
    has_undo: bool,

    tool: Tool,
    color: u32,
    pal_index: usize,
    brush: i32,

    // Active drag (shape tools / freehand).
    drawing: bool,
    drag_sx: i32,
    drag_sy: i32,
    last_x: i32,
    last_y: i32,
    preview_bbox: Option<(usize, usize, usize, usize)>,

    prev_left: bool,
    prev_right: bool,
    prev_mid: bool,

    /// Last scheduler tick at which the status bar was repainted (throttle).
    last_info_tick: u64,

    /// Set by the title-bar close button; the main loop exits when true.
    quit_requested: bool,
}

/// Entry point invoked by the `paint` shell command.
pub fn run() {
    let (w, h) = framebuffer::dimensions();
    if w == 0 || h == 0 {
        crate::fb_println!("paint: no framebuffer");
        return;
    }
    if !ps2_mouse::is_present() {
        crate::kprintln!("paint: warning — no mouse detected; keyboard tools only");
    }

    let canvas_top = TOOLBAR_H;
    let canvas_bottom = h.saturating_sub(framebuffer::STATUS_BAR_HEIGHT);
    if canvas_bottom <= canvas_top + 8 {
        crate::fb_println!("paint: screen too small");
        return;
    }

    // Start as a floating window centered on the desktop. `apply_layout` derives
    // the canvas geometry from the window rectangle and allocates the backing
    // buffers, so seed the fields with placeholders first.
    let mut app = PaintApp {
        screen_w: w,
        screen_h: h,
        win_x: 0,
        win_y: 0,
        win_w: 0,
        win_h: 0,
        mode: WinMode::Windowed,
        minimized: false,
        win_drag: false,
        win_drag_dx: 0,
        win_drag_dy: 0,
        cw: 0,
        ch: 0,
        canvas_left: 0,
        canvas_top,
        canvas: Vec::new(),
        undo: Vec::new(),
        has_undo: false,
        tool: Tool::Pencil,
        color: PALETTE[0],
        pal_index: 0,
        brush: 3,
        drawing: false,
        drag_sx: 0,
        drag_sy: 0,
        last_x: 0,
        last_y: 0,
        preview_bbox: None,
        prev_left: false,
        prev_right: false,
        prev_mid: false,
        last_info_tick: 0,
        quit_requested: false,
    };

    // Compute the initial window rectangle + canvas, allocating fresh buffers.
    app.apply_layout(true);
    if app.cw == 0 || app.ch == 0 {
        crate::fb_println!("paint: screen too small");
        return;
    }

    app.run_loop();

    // Restore the shell screen: clear to black and reset the text cursor so the
    // next prompt renders from the top.
    cursor::hide();
    framebuffer::clear_screen();
}

/// Read one raw scancode from the keyboard device, if available.
fn read_scancode() -> Option<u8> {
    crate::drivers::get_char("keyboard").and_then(|kbd| kbd.read_char())
}

impl PaintApp {
    // ─── Main loop ───────────────────────────────────────────────────────

    fn run_loop(&mut self) {
        // Paint the initial desktop + window UI.
        let init = ps2_mouse::poll();
        self.redraw_all(&init);
        cursor::move_to(init.x, init.y);

        let mut dec = Decoder::new();
        let mut last_seq = u64::MAX;

        loop {
            crate::arch::cpu::halt();

            // Keyboard: Esc (raw 0x01) quits; everything else goes through the
            // shared decoder so tool/color shortcuts work.
            let mut quit = false;
            while let Some(sc) = read_scancode() {
                if sc == 0x01 {
                    quit = true;
                    break;
                }
                if let Some(KeyEvent::Char(c)) = dec.feed(sc) {
                    if self.handle_key(c) {
                        quit = true;
                        break;
                    }
                }
            }
            if quit {
                break;
            }

            // Mouse: only act when something changed.
            let ms = ps2_mouse::poll();
            if ms.seq != last_seq {
                last_seq = ms.seq;
                self.handle_mouse(&ms);
            }

            // The title-bar close button requests quit asynchronously.
            if self.quit_requested {
                break;
            }
        }
    }

    // ─── Input handling ────────────────────────────────────────────────────

    /// Returns `true` to request quitting paint.
    fn handle_key(&mut self, c: char) -> bool {
        // While minimized the window is hidden; only quit is honored.
        if self.minimized {
            return matches!(c, 'q' | 'Q');
        }
        let mut redraw_toolbar = true;
        match c {
            'q' | 'Q' => return true,
            'p' | 'P' => self.tool = Tool::Pencil,
            'e' | 'E' => self.tool = Tool::Eraser,
            'l' | 'L' => self.tool = Tool::Line,
            'r' | 'R' => self.tool = Tool::Rect,
            'f' | 'F' => self.tool = Tool::FilledRect,
            'c' | 'C' => self.tool = Tool::Circle,
            'd' | 'D' => self.tool = Tool::Disc,
            'b' | 'B' => self.tool = Tool::Fill,
            'i' | 'I' => self.tool = Tool::Picker,
            '[' => self.brush = (self.brush - 1).max(1),
            ']' => self.brush = (self.brush + 1).min(64),
            '-' | '_' => self.brush = (self.brush - 1).max(1),
            '=' | '+' => self.brush = (self.brush + 1).min(64),
            '1'..='9' => {
                let idx = c as usize - '1' as usize;
                self.set_color_index(idx);
            }
            '0' => self.set_color_index(9),
            'u' | 'U' => {
                self.do_undo();
                redraw_toolbar = false;
            }
            'x' | 'X' => self.clear_canvas(),
            'm' | 'M' => {
                self.toggle_maximize();
                redraw_toolbar = false;
            }
            's' | 'S' => {
                self.save();
                redraw_toolbar = false;
            }
            'g' | 'G' => {
                self.load();
                redraw_toolbar = false;
            }
            _ => redraw_toolbar = false,
        }
        if redraw_toolbar {
            cursor::hide();
            self.draw_toolbar();
            let ms = ps2_mouse::poll();
            self.draw_bottom_bar(&ms);
            cursor::move_to(ms.x, ms.y);
        }
        false
    }

    fn set_color_index(&mut self, idx: usize) {
        if idx < PALETTE.len() {
            self.pal_index = idx;
            self.color = PALETTE[idx];
        }
    }

    fn handle_mouse(&mut self, ms: &ps2_mouse::MouseState) {
        let mx = ms.x as i32;
        let my = ms.y as i32;
        let left_edge = ms.left && !self.prev_left;
        let left_rel = !ms.left && self.prev_left;
        let right_edge = ms.right && !self.prev_right;
        let mid_edge = ms.middle && !self.prev_mid;

        // Hide the cursor before touching the framebuffer beneath it.
        cursor::hide();

        // The taskbar button is always live: it minimizes/restores the window.
        if left_edge && self.hit_taskbar_button(mx, my) {
            self.set_minimized(!self.minimized, ms);
            self.commit_input_state(ms);
            return;
        }

        if self.minimized {
            // Nothing else is interactive while hidden.
            self.commit_input_state(ms);
            cursor::move_to(ms.x, ms.y);
            return;
        }

        // Continue or finish an in-progress window drag before anything else.
        if self.win_drag {
            if ms.left {
                self.drag_window_to(mx, my, ms);
            } else {
                self.win_drag = false;
            }
            self.commit_input_state(ms);
            cursor::move_to(ms.x, ms.y);
            return;
        }

        // Title-bar window controls (minimize / maximize / close) and drag.
        if left_edge {
            match self.hit_title_button(mx, my) {
                Some(TitleBtn::Minimize) => {
                    self.set_minimized(true, ms);
                    self.commit_input_state(ms);
                    return;
                }
                Some(TitleBtn::Maximize) => {
                    self.toggle_maximize();
                    self.commit_input_state(ms);
                    return;
                }
                Some(TitleBtn::Close) => {
                    self.quit_requested = true;
                    return;
                }
                None => {
                    // A press on the title bar (not a button) begins a drag.
                    // Maximized windows are pinned, so dragging is windowed-only.
                    if self.mode == WinMode::Windowed && self.in_titlebar(mx, my) {
                        self.win_drag = true;
                        self.win_drag_dx = mx - self.win_x as i32;
                        self.win_drag_dy = my - self.win_y as i32;
                        self.commit_input_state(ms);
                        return;
                    }
                }
            }
        }

        let tb_top = (self.win_y + TITLEBAR_H) as i32; // toolbar top (fb space)
        let tb_bottom = tb_top + TOOLBAR_H as i32;
        let win_left = self.win_x as i32;
        let win_right = (self.win_x + self.win_w) as i32;
        let in_window_x = mx >= win_left && mx < win_right;

        if in_window_x && my >= tb_top && my < tb_bottom {
            // Toolbar clicks (relative to the window origin).
            if left_edge {
                let rel_y = (my - tb_top) as usize;
                let px = (mx - win_left) as usize;
                if rel_y < PALETTE_ROW_H {
                    // Top row: pick a palette color.
                    let sw = px / SWATCH;
                    if sw < PALETTE.len() {
                        self.pal_index = sw;
                        self.color = PALETTE[sw];
                        self.draw_toolbar();
                    }
                } else {
                    // Second row: select a tool or adjust the brush size.
                    let mut hit = false;
                    for (t, bx, bw) in tool_buttons() {
                        if px >= bx && px < bx + bw {
                            self.tool = t;
                            hit = true;
                            break;
                        }
                    }
                    if !hit {
                        let bu = brush_ui();
                        if px >= bu.minus_x && px < bu.minus_x + BRUSH_BTN_W {
                            self.brush = (self.brush - 1).max(1);
                            hit = true;
                        } else if px >= bu.plus_x && px < bu.plus_x + BRUSH_BTN_W {
                            self.brush = (self.brush + 1).min(64);
                            hit = true;
                        }
                    }
                    if hit {
                        self.draw_toolbar();
                    }
                }
            }
        } else {
            let canvas_x0 = self.canvas_left as i32;
            let canvas_y0 = self.canvas_top as i32;
            // Only act on the canvas when the pointer is within it. (Clicks on
            // the title bar, border, or desktop fall through and are ignored.)
            let in_canvas = mx >= canvas_x0
                && mx < canvas_x0 + self.cw as i32
                && my >= canvas_y0
                && my < canvas_y0 + self.ch as i32;
            if in_canvas || self.drawing {
                let cx = (mx - canvas_x0).clamp(0, self.cw as i32 - 1);
                let cy = (my - canvas_y0).clamp(0, self.ch as i32 - 1);
                self.canvas_action(cx, cy, ms, left_edge, left_rel, right_edge, mid_edge);
            }
        }

        self.commit_input_state(ms);

        // Repainting the taskbar is comparatively slow (full-width fill + text
        // into uncached framebuffer memory), so throttle it to a few updates per
        // second. The cursor still moves every event since move_to is cheap.
        let now = crate::task::scheduler::ticks();
        if now.wrapping_sub(self.last_info_tick) >= 6 {
            self.draw_bottom_bar(ms);
            self.last_info_tick = now;
        }
        cursor::move_to(ms.x, ms.y);
    }

    /// Record the current button state for next-frame edge detection.
    #[inline]
    fn commit_input_state(&mut self, ms: &ps2_mouse::MouseState) {
        self.prev_left = ms.left;
        self.prev_right = ms.right;
        self.prev_mid = ms.middle;
    }

    #[allow(clippy::too_many_arguments)]
    fn canvas_action(
        &mut self,
        cx: i32,
        cy: i32,
        ms: &ps2_mouse::MouseState,
        left_edge: bool,
        left_rel: bool,
        right_edge: bool,
        _mid_edge: bool,
    ) {
        match self.tool {
            Tool::Picker => {
                if left_edge {
                    self.color = self.get_px(cx, cy);
                    self.pal_index = usize::MAX; // custom color
                    self.draw_toolbar();
                }
            }
            Tool::Fill => {
                if left_edge {
                    self.snapshot_undo();
                    self.flood_fill(cx, cy, self.color);
                    self.blit_all();
                }
            }
            Tool::Pencil | Tool::Eraser => {
                let paint_color = if self.tool == Tool::Eraser {
                    WHITE
                } else if ms.right {
                    WHITE
                } else {
                    self.color
                };
                if left_edge || right_edge {
                    self.snapshot_undo();
                    self.last_x = cx;
                    self.last_y = cy;
                    self.draw_dot(cx, cy, paint_color);
                } else if ms.left || ms.right {
                    self.commit_line(self.last_x, self.last_y, cx, cy, paint_color);
                    self.last_x = cx;
                    self.last_y = cy;
                }
            }
            _ if self.tool.is_shape() => {
                if left_edge {
                    self.snapshot_undo();
                    self.drawing = true;
                    self.drag_sx = cx;
                    self.drag_sy = cy;
                    self.preview_bbox = None;
                } else if ms.left && self.drawing {
                    self.update_preview(cx, cy);
                } else if left_rel && self.drawing {
                    self.clear_preview();
                    self.commit_shape(self.drag_sx, self.drag_sy, cx, cy);
                    self.drawing = false;
                }
            }
            _ => {}
        }
    }
}

impl PaintApp {
    // ─── Canvas primitives (operate on the backing buffer) ─────────────────

    #[inline]
    fn in_bounds(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && (x as usize) < self.cw && (y as usize) < self.ch
    }

    #[inline]
    fn get_px(&self, x: i32, y: i32) -> u32 {
        if self.in_bounds(x, y) {
            self.canvas[y as usize * self.cw + x as usize]
        } else {
            WHITE
        }
    }

    #[inline]
    fn c_set(&mut self, x: i32, y: i32, color: u32) {
        if self.in_bounds(x, y) {
            self.canvas[y as usize * self.cw + x as usize] = color;
        }
    }

    /// Fill a `b×b` square centered on `(cx, cy)` (the brush footprint).
    fn c_square(&mut self, cx: i32, cy: i32, b: i32, color: u32) {
        let r = b / 2;
        for dy in 0..b {
            for dx in 0..b {
                self.c_set(cx - r + dx, cy - r + dy, color);
            }
        }
    }

    /// Paint a single brush dot and blit it.
    fn draw_dot(&mut self, cx: i32, cy: i32, color: u32) {
        self.c_square(cx, cy, self.brush, color);
        let r = self.brush / 2 + 1;
        self.blit_canvas_rect_i(cx - r, cy - r, self.brush + 2, self.brush + 2);
    }

    /// Brush-thick line on the canvas, then blit the bounding box once.
    fn commit_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
        let b = self.brush;
        let (mut x, mut y) = (x0, y0);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            self.c_square(x, y, b, color);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
        let pad = b / 2 + 1;
        let bx = x0.min(x1) - pad;
        let by = y0.min(y1) - pad;
        let bw = (x0 - x1).abs() + 2 * pad;
        let bh = (y0 - y1).abs() + 2 * pad;
        self.blit_canvas_rect_i(bx, by, bw, bh);
    }

    /// Commit a shape tool's final geometry to the canvas (start → end).
    fn commit_shape(&mut self, sx: i32, sy: i32, ex: i32, ey: i32) {
        let color = self.color;
        let b = self.brush;
        match self.tool {
            Tool::Line => self.commit_line(sx, sy, ex, ey, color),
            Tool::Rect => {
                let (x0, y0, w, h) = norm_rect(sx, sy, ex, ey);
                self.c_fill_rect(x0, y0, w, b, color);
                self.c_fill_rect(x0, y0 + h - b, w, b, color);
                self.c_fill_rect(x0, y0, b, h, color);
                self.c_fill_rect(x0 + w - b, y0, b, h, color);
                self.blit_canvas_rect_i(x0 - 1, y0 - 1, w + 2, h + 2);
            }
            Tool::FilledRect => {
                let (x0, y0, w, h) = norm_rect(sx, sy, ex, ey);
                self.c_fill_rect(x0, y0, w, h, color);
                self.blit_canvas_rect_i(x0 - 1, y0 - 1, w + 2, h + 2);
            }
            Tool::Circle => {
                let r = radius(sx, sy, ex, ey);
                self.c_circle(sx, sy, r, b, color);
                self.blit_canvas_rect_i(sx - r - b, sy - r - b, 2 * (r + b), 2 * (r + b));
            }
            Tool::Disc => {
                let r = radius(sx, sy, ex, ey);
                self.c_disc(sx, sy, r, color);
                self.blit_canvas_rect_i(sx - r - 1, sy - r - 1, 2 * r + 2, 2 * r + 2);
            }
            _ => {}
        }
    }

    fn c_fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        for dy in 0..h {
            for dx in 0..w {
                self.c_set(x + dx, y + dy, color);
            }
        }
    }

    /// Brush-thick circle outline (center, radius).
    fn c_circle(&mut self, cx: i32, cy: i32, r: i32, b: i32, color: u32) {
        if r <= 0 {
            return;
        }
        let mut x = r;
        let mut y = 0;
        let mut err = 1 - r;
        while x >= y {
            for &(px, py) in &[
                (cx + x, cy + y), (cx + y, cy + x), (cx - y, cy + x), (cx - x, cy + y),
                (cx - x, cy - y), (cx - y, cy - x), (cx + y, cy - x), (cx + x, cy - y),
            ] {
                self.c_square(px, py, b, color);
            }
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
    }

    /// Filled disc (center, radius).
    fn c_disc(&mut self, cx: i32, cy: i32, r: i32, color: u32) {
        if r <= 0 {
            return;
        }
        let r2 = r * r;
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r2 {
                    self.c_set(cx + dx, cy + dy, color);
                }
            }
        }
    }

    // ─── Shape preview (rubber-banding, framebuffer-only) ──────────────────

    fn update_preview(&mut self, cx: i32, cy: i32) {
        // Restore the previously-previewed region from the canvas.
        self.clear_preview();

        let top = self.canvas_top as isize;
        let left = self.canvas_left as isize;
        let color = self.color;
        let b = self.brush as isize;
        let (sx, sy) = (self.drag_sx, self.drag_sy);

        // Draw the new preview directly onto the framebuffer (transient).
        framebuffer::with(|fb| match self.tool {
            Tool::Line => {
                fb.draw_thick_line(sx as isize + left, sy as isize + top, cx as isize + left, cy as isize + top, b, color);
            }
            Tool::Rect => {
                let (x0, y0, w, h) = norm_rect(sx, sy, cx, cy);
                fb.draw_rect((x0 + self.canvas_left as i32) as usize, (y0 + self.canvas_top as i32) as usize, w as usize, h as usize, color);
            }
            Tool::FilledRect => {
                let (x0, y0, w, h) = norm_rect(sx, sy, cx, cy);
                fb.fill_rect((x0 + self.canvas_left as i32) as usize, (y0 + self.canvas_top as i32) as usize, w as usize, h as usize, color);
            }
            Tool::Circle => {
                let r = radius(sx, sy, cx, cy);
                fb.draw_circle(sx as isize + left, sy as isize + top, r as isize, color);
            }
            Tool::Disc => {
                let r = radius(sx, sy, cx, cy);
                fb.fill_circle(sx as isize + left, sy as isize + top, r as isize, color);
            }
            _ => {}
        });

        // Record the canvas-space bounding box to restore next frame.
        let bbox = match self.tool {
            Tool::Circle | Tool::Disc => {
                let r = radius(sx, sy, cx, cy);
                (sx - r - 2, sy - r - 2, 2 * r + 4, 2 * r + 4)
            }
            _ => {
                let pad = self.brush + 2;
                let x0 = sx.min(cx) - pad;
                let y0 = sy.min(cy) - pad;
                (x0, y0, (sx - cx).abs() + 2 * pad, (sy - cy).abs() + 2 * pad)
            }
        };
        self.preview_bbox = Some(self.clamp_box(bbox.0, bbox.1, bbox.2, bbox.3));
    }

    fn clear_preview(&mut self) {
        if let Some((x, y, w, h)) = self.preview_bbox.take() {
            self.blit_canvas_rect(x, y, w, h);
        }
    }

    // ─── Flood fill (scanline) ─────────────────────────────────────────────

    fn flood_fill(&mut self, sx: i32, sy: i32, new: u32) {
        let target = self.get_px(sx, sy);
        if target == new {
            return;
        }
        let mut stack: Vec<(i32, i32)> = Vec::new();
        stack.push((sx, sy));
        while let Some((x, y)) = stack.pop() {
            if self.get_px(x, y) != target {
                continue;
            }
            // Extend the span left and right.
            let mut lx = x;
            while lx > 0 && self.get_px(lx - 1, y) == target {
                lx -= 1;
            }
            let mut rx = x;
            while rx < self.cw as i32 - 1 && self.get_px(rx + 1, y) == target {
                rx += 1;
            }
            for px in lx..=rx {
                self.c_set(px, y, new);
            }
            // Seed the rows above and below at each run of target pixels.
            for &ny in &[y - 1, y + 1] {
                if ny < 0 || ny >= self.ch as i32 {
                    continue;
                }
                let mut px = lx;
                while px <= rx {
                    if self.get_px(px, ny) == target {
                        stack.push((px, ny));
                        while px <= rx && self.get_px(px, ny) == target {
                            px += 1;
                        }
                    } else {
                        px += 1;
                    }
                }
            }
        }
    }

    // ─── Blitting (canvas → framebuffer) ───────────────────────────────────

    fn clamp_box(&self, x: i32, y: i32, w: i32, h: i32) -> (usize, usize, usize, usize) {
        let x0 = x.max(0).min(self.cw as i32) as usize;
        let y0 = y.max(0).min(self.ch as i32) as usize;
        let x1 = (x + w).max(0).min(self.cw as i32) as usize;
        let y1 = (y + h).max(0).min(self.ch as i32) as usize;
        (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
    }

    fn blit_canvas_rect_i(&self, x: i32, y: i32, w: i32, h: i32) {
        let (x0, y0, bw, bh) = self.clamp_box(x, y, w, h);
        self.blit_canvas_rect(x0, y0, bw, bh);
    }

    fn blit_canvas_rect(&self, x: usize, y: usize, w: usize, h: usize) {
        if w == 0 || h == 0 || x >= self.cw || y >= self.ch {
            return;
        }
        let w = w.min(self.cw - x);
        let h = h.min(self.ch - y);
        let start = y * self.cw + x;
        let top = self.canvas_top;
        let left = self.canvas_left;
        framebuffer::with(|fb| {
            fb.blit(x + left, y + top, w, h, &self.canvas[start..], self.cw);
        });
    }

    fn blit_all(&self) {
        self.blit_canvas_rect(0, 0, self.cw, self.ch);
    }

    // ─── Undo / clear ──────────────────────────────────────────────────────

    fn snapshot_undo(&mut self) {
        self.undo.copy_from_slice(&self.canvas);
        self.has_undo = true;
    }

    fn do_undo(&mut self) {
        if !self.has_undo {
            return;
        }
        // Swap so a second undo acts as redo.
        core::mem::swap(&mut self.canvas, &mut self.undo);
        cursor::hide();
        self.blit_all();
        let ms = ps2_mouse::poll();
        cursor::move_to(ms.x, ms.y);
    }

    fn clear_canvas(&mut self) {
        self.snapshot_undo();
        for px in self.canvas.iter_mut() {
            *px = WHITE;
        }
        cursor::hide();
        self.blit_all();
        let ms = ps2_mouse::poll();
        cursor::move_to(ms.x, ms.y);
    }
}

impl PaintApp {
    // ─── UI chrome ─────────────────────────────────────────────────────────

    fn draw_toolbar(&self) {
        let color = self.color;
        let pal_index = self.pal_index;
        let cur_tool = self.tool;
        let brush = self.brush;
        let label = format!("{}  size:{}", self.tool.name(), self.brush);
        let ox = self.win_x;
        let oy = self.win_y + TITLEBAR_H;
        let aw = self.win_w;
        framebuffer::with(|fb| {
            fb.fill_rect(ox, oy, aw, TOOLBAR_H, TOOLBAR_BG);
            // Palette swatches (top row).
            for (i, &col) in PALETTE.iter().enumerate() {
                let x = ox + i * SWATCH;
                if x + SWATCH > ox + aw {
                    break;
                }
                fb.fill_rect(x + 2, oy + 4, SWATCH - 4, SWATCH - 4, col);
                if i == pal_index {
                    fb.draw_rect(x, oy + 2, SWATCH, SWATCH, 0xFFFF00);
                    fb.draw_rect(x + 1, oy + 3, SWATCH - 2, SWATCH - 2, 0x000000);
                }
            }
            // Current color chip + tool/size label (top row).
            let tx = PALETTE.len() * SWATCH + 10;
            if tx + 24 < aw {
                fb.fill_rect(ox + tx, oy + 4, 22, 22, color);
                fb.draw_rect(ox + tx, oy + 4, 22, 22, 0xFFFFFF);
                fb.draw_text_px(ox + tx + 30, oy + 8, &label, 0xE6E6E6, TOOLBAR_BG);
            }
            // Tool buttons (second row).
            let ty = oy + PALETTE_ROW_H + (TOOL_ROW_H - 16) / 2;
            for (t, bx, bw) in tool_buttons() {
                if bx + bw > aw {
                    break;
                }
                let selected = t == cur_tool;
                let bg = if selected { 0x3A6EA5 } else { 0x3A3A44 };
                fb.fill_rect(ox + bx, oy + PALETTE_ROW_H + 2, bw, TOOL_ROW_H - 4, bg);
                if selected {
                    fb.draw_rect(ox + bx, oy + PALETTE_ROW_H + 2, bw, TOOL_ROW_H - 4, 0xFFFF00);
                }
                fb.draw_text_px(ox + bx + 4, ty, t.name(), 0xE6E6E6, bg);
            }
            // Brush-size controls (second row, right of the tools).
            let bu = brush_ui();
            if bu.end_x <= aw {
                let by = oy + PALETTE_ROW_H + 2;
                let bh = TOOL_ROW_H - 4;
                let btn_bg = 0x3A3A44;
                // Minus button.
                fb.fill_rect(ox + bu.minus_x, by, BRUSH_BTN_W, bh, btn_bg);
                fb.draw_text_px(ox + bu.minus_x + (BRUSH_BTN_W - CHAR_W) / 2, ty, "-", 0xE6E6E6, btn_bg);
                // Size label.
                let s = format!("Sz:{}", brush);
                fb.draw_text_px(ox + bu.label_x, ty, &s, 0xE6E6E6, TOOLBAR_BG);
                // Plus button.
                fb.fill_rect(ox + bu.plus_x, by, BRUSH_BTN_W, bh, btn_bg);
                fb.draw_text_px(ox + bu.plus_x + (BRUSH_BTN_W - CHAR_W) / 2, ty, "+", 0xE6E6E6, btn_bg);
            }
        });
    }

    // ─── Window layout ─────────────────────────────────────────────────────

    /// Outer window rectangle `(x, y, w, h)` for a given placement mode.
    fn window_rect_for(&self, mode: WinMode) -> (usize, usize, usize, usize) {
        let avail_h = self.screen_h.saturating_sub(framebuffer::STATUS_BAR_HEIGHT);
        match mode {
            WinMode::Maximized => (0, 0, self.screen_w, avail_h),
            WinMode::Windowed => {
                let w = (self.screen_w * 5 / 6).max(TITLEBAR_H + 60);
                let h = (avail_h * 5 / 6).max(TITLEBAR_H + TOOLBAR_H + 60);
                let x = (self.screen_w.saturating_sub(w)) / 2;
                let y = (avail_h.saturating_sub(h)) / 2;
                (x, y, w, h)
            }
        }
    }

    /// Recompute window geometry + canvas dimensions from `self.mode`. On the
    /// initial call the backing buffers are freshly allocated; afterwards the
    /// canvas is resized in place (preserving the overlapping pixels).
    fn apply_layout(&mut self, initial: bool) {
        let (x, y, w, h) = self.window_rect_for(self.mode);
        self.win_x = x;
        self.win_y = y;
        self.win_w = w;
        self.win_h = h;
        self.canvas_left = x + BORDER;
        self.canvas_top = y + TITLEBAR_H + TOOLBAR_H;
        let new_cw = w.saturating_sub(2 * BORDER);
        let new_ch = h.saturating_sub(TITLEBAR_H + TOOLBAR_H + BORDER);
        if initial {
            self.cw = new_cw;
            self.ch = new_ch;
            self.canvas = vec![WHITE; new_cw * new_ch];
            self.undo = vec![WHITE; new_cw * new_ch];
            self.has_undo = false;
        } else {
            self.resize_canvas(new_cw, new_ch);
        }
    }

    /// Resize the canvas to `new_w × new_h`, copying the overlapping top-left
    /// region so existing artwork survives a maximize/restore.
    fn resize_canvas(&mut self, new_w: usize, new_h: usize) {
        if new_w == self.cw && new_h == self.ch {
            return;
        }
        let mut nc = vec![WHITE; new_w * new_h];
        let copy_w = new_w.min(self.cw);
        let copy_h = new_h.min(self.ch);
        for row in 0..copy_h {
            let src = row * self.cw;
            let dst = row * new_w;
            nc[dst..dst + copy_w].copy_from_slice(&self.canvas[src..src + copy_w]);
        }
        self.canvas = nc;
        self.undo = vec![WHITE; new_w * new_h];
        self.has_undo = false;
        self.cw = new_w;
        self.ch = new_h;
        self.drawing = false;
        self.preview_bbox = None;
    }

    /// Toggle between the floating and full-screen window placements.
    fn toggle_maximize(&mut self) {
        self.mode = match self.mode {
            WinMode::Windowed => WinMode::Maximized,
            WinMode::Maximized => WinMode::Windowed,
        };
        self.apply_layout(false);
        cursor::hide();
        let ms = ps2_mouse::poll();
        self.redraw_all(&ms);
        cursor::move_to(ms.x, ms.y);
    }

    /// Hide (or restore) the window to/from the taskbar.
    fn set_minimized(&mut self, value: bool, ms: &ps2_mouse::MouseState) {
        if self.minimized == value {
            return;
        }
        self.minimized = value;
        self.drawing = false;
        self.preview_bbox = None;
        cursor::hide();
        self.redraw_all(ms);
        cursor::move_to(ms.x, ms.y);
    }

    // ─── Window chrome ─────────────────────────────────────────────────────

    /// Repaint the whole screen: desktop backdrop, the window (unless hidden),
    /// and the taskbar.
    fn redraw_all(&self, ms: &ps2_mouse::MouseState) {
        self.draw_desktop();
        if !self.minimized {
            self.draw_window_frame();
            self.draw_toolbar();
            self.blit_all();
        }
        self.draw_bottom_bar(ms);
    }

    /// Fill the desktop area (everything above the taskbar) with the backdrop.
    fn draw_desktop(&self) {
        let h = self.screen_h.saturating_sub(framebuffer::STATUS_BAR_HEIGHT);
        framebuffer::with(|fb| {
            let (w, _) = fb.dimensions();
            fb.fill_rect(0, 0, w, h, DESKTOP_BG);
        });
    }

    /// Title-bar button rectangles: `(min_x, max_x, close_x, y, w, h)`.
    fn title_button_rects(&self) -> (usize, usize, usize, usize, usize, usize) {
        let bw = WIN_BTN_W;
        let bh = TITLEBAR_H.saturating_sub(2);
        let by = self.win_y + 1;
        let clx = self.win_x + self.win_w - bw - 3;
        let mxx = clx - bw - 3;
        let mnx = mxx - bw - 3;
        (mnx, mxx, clx, by, bw, bh)
    }

    fn hit_title_button(&self, mx: i32, my: i32) -> Option<TitleBtn> {
        let (mnx, mxx, clx, by, bw, bh) = self.title_button_rects();
        if my < by as i32 || my >= (by + bh) as i32 {
            return None;
        }
        let hit = |bx: usize| mx >= bx as i32 && mx < (bx + bw) as i32;
        if hit(clx) {
            Some(TitleBtn::Close)
        } else if hit(mxx) {
            Some(TitleBtn::Maximize)
        } else if hit(mnx) {
            Some(TitleBtn::Minimize)
        } else {
            None
        }
    }

    /// True when `(mx, my)` lies on the window's title-bar strip.
    fn in_titlebar(&self, mx: i32, my: i32) -> bool {
        mx >= self.win_x as i32
            && mx < (self.win_x + self.win_w) as i32
            && my >= self.win_y as i32
            && my < (self.win_y + TITLEBAR_H) as i32
    }

    /// Move the window so its origin tracks the dragged pointer, clamped to the
    /// desktop, then repaint. No-op when the position is unchanged.
    fn drag_window_to(&mut self, mx: i32, my: i32, ms: &ps2_mouse::MouseState) {
        let avail_h = self.screen_h.saturating_sub(framebuffer::STATUS_BAR_HEIGHT);
        let max_x = self.screen_w.saturating_sub(self.win_w) as i32;
        let max_y = avail_h.saturating_sub(self.win_h) as i32;
        let new_x = (mx - self.win_drag_dx).clamp(0, max_x) as usize;
        let new_y = (my - self.win_drag_dy).clamp(0, max_y) as usize;
        if new_x == self.win_x && new_y == self.win_y {
            return;
        }
        self.win_x = new_x;
        self.win_y = new_y;
        self.canvas_left = new_x + BORDER;
        self.canvas_top = new_y + TITLEBAR_H + TOOLBAR_H;
        cursor::hide();
        self.redraw_all(ms);
        cursor::move_to(ms.x, ms.y);
    }

    /// Draw the title bar, window border, and the three window buttons.
    fn draw_window_frame(&self) {
        let (mnx, mxx, clx, by, bw, bh) = self.title_button_rects();
        let title = match self.mode {
            WinMode::Maximized => "Paint  -  maximized",
            WinMode::Windowed => "Paint",
        };
        let wx = self.win_x;
        let wy = self.win_y;
        let ww = self.win_w;
        let wh = self.win_h;
        framebuffer::with(|fb| {
            // Title bar + caption.
            fb.fill_rect(wx, wy, ww, TITLEBAR_H, TITLE_BG);
            fb.draw_text_px(wx + 8, wy + (TITLEBAR_H - 16) / 2, title, 0xFFFFFF, TITLE_BG);
            // Window outline (double line for a slight bevel).
            fb.draw_rect(wx, wy, ww, wh, 0x05151F);
            if ww > 2 && wh > 2 {
                fb.draw_rect(wx + 1, wy + 1, ww - 2, wh - 2, 0x6FA8C8);
            }
            // Minimize button (underline glyph).
            fb.fill_rect(mnx, by, bw, bh, 0x3A5A74);
            fb.fill_rect(mnx + 4, by + bh - 6, bw - 8, 2, 0xFFFFFF);
            // Maximize button (square glyph).
            fb.fill_rect(mxx, by, bw, bh, 0x3A5A74);
            fb.draw_rect(mxx + 4, by + 4, bw - 8, bh - 8, 0xFFFFFF);
            // Close button (red, "X" glyph).
            fb.fill_rect(clx, by, bw, bh, 0xC0392B);
            fb.draw_text_px(clx + (bw - CHAR_W) / 2, by + (bh.saturating_sub(16)) / 2 + 1, "X", 0xFFFFFF, 0xC0392B);
        });
    }

    // ─── Taskbar ─────────────────────────────────────────────────────────

    /// Rectangle of the "Paint" taskbar button `(x, y, w, h)`.
    fn taskbar_button_rect(&self) -> (usize, usize, usize, usize) {
        let bar_top = self.screen_h.saturating_sub(framebuffer::STATUS_BAR_HEIGHT);
        let bw = 7 * CHAR_W; // " Paint "
        let bx = 200usize.min(self.screen_w.saturating_sub(bw + 6));
        let by = bar_top + 2;
        let bh = framebuffer::STATUS_BAR_HEIGHT.saturating_sub(4);
        (bx, by, bw, bh)
    }

    fn hit_taskbar_button(&self, mx: i32, my: i32) -> bool {
        let (bx, by, bw, bh) = self.taskbar_button_rect();
        mx >= bx as i32 && mx < (bx + bw) as i32 && my >= by as i32 && my < (by + bh) as i32
    }

    /// Repaint the taskbar with the live paint state.
    fn draw_bottom_bar(&self, ms: &ps2_mouse::MouseState) {
        let cx = ms.x as i32 - self.canvas_left as i32;
        let cy = ms.y as i32 - self.canvas_top as i32;
        let in_canvas = !self.minimized
            && cx >= 0
            && cx < self.cw as i32
            && cy >= 0
            && cy < self.ch as i32;
        let info = if self.minimized {
            String::from("minimized - click Paint to restore   q=quit")
        } else if in_canvas {
            format!("tool:{}  ({},{})   q=quit", self.tool.name(), cx, cy)
        } else {
            format!(
                "tool:{}  #{:06X}  br:{}   q=quit",
                self.tool.name(),
                self.color & 0xFFFFFF,
                self.brush
            )
        };
        self.draw_bottom_bar_msg(&info);
    }

    /// Draw the taskbar background, the "Paint" button, and `msg` on the right.
    fn draw_bottom_bar_msg(&self, msg: &str) {
        const BG: u32 = 0x1B3A5B;
        const FG: u32 = 0xE6F0FF;
        let bar_h = framebuffer::STATUS_BAR_HEIGHT;
        let bar_top = self.screen_h.saturating_sub(bar_h);
        let (bx, by, bw, bh) = self.taskbar_button_rect();
        let minimized = self.minimized;
        framebuffer::with(|fb| {
            let (w, _) = fb.dimensions();
            fb.fill_rect(0, bar_top, w, bar_h, BG);
            fb.fill_rect(0, bar_top, w, 1, 0x3C6FA0); // separator highlight
            let ty = bar_top + (bar_h.saturating_sub(16)) / 2;
            fb.draw_text_px(6, ty, "pagh OS", FG, BG);
            // Taskbar button for the paint window (dim when minimized).
            let btn_bg = if minimized { 0x10324F } else { 0x2E5C86 };
            fb.fill_rect(bx, by, bw, bh, btn_bg);
            fb.draw_rect(bx, by, bw, bh, 0x6FA8C8);
            fb.draw_text_px(bx + CHAR_W, ty, "Paint", FG, btn_bg);
            // Right-aligned status message.
            let rw = msg.len() * CHAR_W;
            if rw + 8 < w {
                fb.draw_text_px(w - rw - 6, ty, msg, FG, BG);
            }
        });
    }

    // ─── Save / load (raw image to /mnt/paint.img) ─────────────────────────

    fn save(&self) {
        let mut bytes: Vec<u8> = Vec::with_capacity(16 + self.canvas.len() * 4);
        bytes.extend_from_slice(b"PAGHIMG1");
        bytes.extend_from_slice(&(self.cw as u32).to_le_bytes());
        bytes.extend_from_slice(&(self.ch as u32).to_le_bytes());
        for &px in &self.canvas {
            bytes.extend_from_slice(&px.to_le_bytes());
        }

        let msg = match self.open_or_create("/mnt", "paint.img") {
            Ok(node) => match node.write(0, &bytes) {
                Ok(n) => {
                    node.sync();
                    format!("paint: saved {} bytes to /mnt/paint.img", n)
                }
                Err(e) => format!("paint: save failed: {:?}", e),
            },
            Err(e) => format!("paint: save failed: {:?}", e),
        };
        self.draw_bottom_bar_msg(&msg);
        crate::kprintln!("{}", msg);
    }

    fn load(&mut self) {
        let node = match crate::vfs::lookup_path("/mnt/paint.img") {
            Ok(n) => n,
            Err(_) => {
                self.draw_bottom_bar_msg("paint: /mnt/paint.img not found");
                return;
            }
        };
        // Read the whole file.
        let mut data: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        let mut off: u64 = 0;
        loop {
            match node.read(off, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    data.extend_from_slice(&buf[..n]);
                    off += n as u64;
                }
                Err(_) => break,
            }
        }
        if data.len() < 16 || &data[..8] != b"PAGHIMG1" {
            self.draw_bottom_bar_msg("paint: bad image header");
            return;
        }
        let fw = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
        let fh = u32::from_le_bytes([data[12], data[13], data[14], data[15]]) as usize;
        if fw != self.cw || fh != self.ch {
            self.draw_bottom_bar_msg("paint: image size mismatch");
            return;
        }
        let want = self.cw * self.ch;
        let avail = (data.len() - 16) / 4;
        let count = want.min(avail);
        self.snapshot_undo();
        for i in 0..count {
            let b = 16 + i * 4;
            self.canvas[i] = u32::from_le_bytes([data[b], data[b + 1], data[b + 2], data[b + 3]]);
        }
        cursor::hide();
        self.blit_all();
        let ms = ps2_mouse::poll();
        cursor::move_to(ms.x, ms.y);
        self.draw_bottom_bar_msg("paint: loaded /mnt/paint.img");
    }

    fn open_or_create(
        &self,
        dir: &str,
        name: &str,
    ) -> crate::vfs::VfsResult<alloc::sync::Arc<dyn crate::vfs::VfsNode>> {
        let d = crate::vfs::lookup_path(dir)?;
        match d.create_file(name) {
            Ok(n) => Ok(n),
            Err(crate::vfs::VfsError::AlreadyExists) => d.lookup(name),
            Err(e) => Err(e),
        }
    }
}

// ─── Free geometry helpers ───────────────────────────────────────────────────

/// Normalize two corners into `(x, y, w, h)` with positive extents.
fn norm_rect(x0: i32, y0: i32, x1: i32, y1: i32) -> (i32, i32, i32, i32) {
    let x = x0.min(x1);
    let y = y0.min(y1);
    let w = (x0 - x1).abs() + 1;
    let h = (y0 - y1).abs() + 1;
    (x, y, w, h)
}

/// Euclidean radius between two points (rounded down).
fn radius(x0: i32, y0: i32, x1: i32, y1: i32) -> i32 {
    let dx = (x1 - x0) as i64;
    let dy = (y1 - y0) as i64;
    isqrt(dx * dx + dy * dy) as i32
}

/// Integer square root (Newton's method).
fn isqrt(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}
