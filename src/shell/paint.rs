//! `paint` — a full-screen framebuffer drawing application.
//!
//! Driven directly by the PS/2 mouse ([`crate::drivers::ps2_mouse`]) and the
//! keyboard, `paint` takes over the screen until the user quits (Esc or `q`).
//! It keeps an authoritative backing canvas in the heap (one `u32` per pixel)
//! so it can offer features that need to read the image back — shape preview
//! (rubber-banding), flood fill, undo, and save/load to disk.
//!
//! Layout (top → bottom):
//!   * a toolbar strip (palette swatches + current tool / color / brush size),
//!   * the canvas (the drawing surface), and
//!   * the kernel status bar at the very bottom (reused for live info).
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
    cw: usize,
    ch: usize,
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
    let cw = w;
    let ch = canvas_bottom - canvas_top;

    let mut app = PaintApp {
        cw,
        ch,
        canvas_top,
        canvas: vec![WHITE; cw * ch],
        undo: vec![WHITE; cw * ch],
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
    };

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
        // Paint the initial UI.
        framebuffer::with(|fb| {
            let (w, h) = fb.dimensions();
            fb.fill_rect(0, 0, w, h, 0x101014);
        });
        self.blit_all();
        self.draw_toolbar();
        let init = ps2_mouse::poll();
        self.draw_info(&init);
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
        }
    }

    // ─── Input handling ────────────────────────────────────────────────────

    /// Returns `true` to request quitting paint.
    fn handle_key(&mut self, c: char) -> bool {
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
            self.draw_info(&ms);
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

        // Toolbar clicks (top strip).
        if (my as usize) < TOOLBAR_H {
            if left_edge {
                if (my as usize) < PALETTE_ROW_H {
                    // Top row: pick a palette color.
                    let sw = mx as usize / SWATCH;
                    if sw < PALETTE.len() {
                        self.pal_index = sw;
                        self.color = PALETTE[sw];
                        self.draw_toolbar();
                    }
                } else {
                    // Second row: select a tool or adjust the brush size.
                    let px = mx as usize;
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
            let cx = mx.clamp(0, self.cw as i32 - 1);
            let cy = (my - self.canvas_top as i32).clamp(0, self.ch as i32 - 1);
            self.canvas_action(cx, cy, ms, left_edge, left_rel, right_edge, mid_edge);
        }

        self.prev_left = ms.left;
        self.prev_right = ms.right;
        self.prev_mid = ms.middle;

        // Repainting the status bar means a full-width fill + text rendering into
        // uncached framebuffer memory, which is comparatively slow. Doing it on
        // every mouse packet is what made the cursor feel like ~5 fps. Throttle
        // it to a handful of updates per second; the cursor still moves on every
        // event because cursor::move_to below is cheap.
        let now = crate::task::scheduler::ticks();
        if now.wrapping_sub(self.last_info_tick) >= 6 {
            self.draw_info(ms);
            self.last_info_tick = now;
        }
        cursor::move_to(ms.x, ms.y);
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
        let color = self.color;
        let b = self.brush as isize;
        let (sx, sy) = (self.drag_sx, self.drag_sy);

        // Draw the new preview directly onto the framebuffer (transient).
        framebuffer::with(|fb| match self.tool {
            Tool::Line => {
                fb.draw_thick_line(sx as isize, sy as isize + top, cx as isize, cy as isize + top, b, color);
            }
            Tool::Rect => {
                let (x0, y0, w, h) = norm_rect(sx, sy, cx, cy);
                fb.draw_rect(x0 as usize, (y0 + self.canvas_top as i32) as usize, w as usize, h as usize, color);
            }
            Tool::FilledRect => {
                let (x0, y0, w, h) = norm_rect(sx, sy, cx, cy);
                fb.fill_rect(x0 as usize, (y0 + self.canvas_top as i32) as usize, w as usize, h as usize, color);
            }
            Tool::Circle => {
                let r = radius(sx, sy, cx, cy);
                fb.draw_circle(sx as isize, sy as isize + top, r as isize, color);
            }
            Tool::Disc => {
                let r = radius(sx, sy, cx, cy);
                fb.fill_circle(sx as isize, sy as isize + top, r as isize, color);
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
        framebuffer::with(|fb| {
            fb.blit(x, y + top, w, h, &self.canvas[start..], self.cw);
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
        framebuffer::with(|fb| {
            let (w, _) = fb.dimensions();
            fb.fill_rect(0, 0, w, TOOLBAR_H, TOOLBAR_BG);
            // Palette swatches (top row).
            for (i, &col) in PALETTE.iter().enumerate() {
                let x = i * SWATCH;
                fb.fill_rect(x + 2, 4, SWATCH - 4, SWATCH - 4, col);
                if i == pal_index {
                    fb.draw_rect(x, 2, SWATCH, SWATCH, 0xFFFF00);
                    fb.draw_rect(x + 1, 3, SWATCH - 2, SWATCH - 2, 0x000000);
                }
            }
            // Current color chip + tool/size label (top row).
            let tx = PALETTE.len() * SWATCH + 10;
            if tx + 24 < w {
                fb.fill_rect(tx, 4, 22, 22, color);
                fb.draw_rect(tx, 4, 22, 22, 0xFFFFFF);
                fb.draw_text_px(tx + 30, 8, &label, 0xE6E6E6, TOOLBAR_BG);
            }
            // Tool buttons (second row).
            let ty = PALETTE_ROW_H + (TOOL_ROW_H - 16) / 2;
            for (t, bx, bw) in tool_buttons() {
                if bx + bw > w {
                    break;
                }
                let selected = t == cur_tool;
                let bg = if selected { 0x3A6EA5 } else { 0x3A3A44 };
                fb.fill_rect(bx, PALETTE_ROW_H + 2, bw, TOOL_ROW_H - 4, bg);
                if selected {
                    fb.draw_rect(bx, PALETTE_ROW_H + 2, bw, TOOL_ROW_H - 4, 0xFFFF00);
                }
                fb.draw_text_px(bx + 4, ty, t.name(), 0xE6E6E6, bg);
            }
            // Brush-size controls (second row, right of the tools).
            let bu = brush_ui();
            if bu.end_x <= w {
                let by = PALETTE_ROW_H + 2;
                let bh = TOOL_ROW_H - 4;
                let btn_bg = 0x3A3A44;
                // Minus button.
                fb.fill_rect(bu.minus_x, by, BRUSH_BTN_W, bh, btn_bg);
                fb.draw_text_px(bu.minus_x + (BRUSH_BTN_W - CHAR_W) / 2, ty, "-", 0xE6E6E6, btn_bg);
                // Size label.
                let s = format!("Sz:{}", brush);
                fb.draw_text_px(bu.label_x, ty, &s, 0xE6E6E6, TOOLBAR_BG);
                // Plus button.
                fb.fill_rect(bu.plus_x, by, BRUSH_BTN_W, bh, btn_bg);
                fb.draw_text_px(bu.plus_x + (BRUSH_BTN_W - CHAR_W) / 2, ty, "+", 0xE6E6E6, btn_bg);
            }
        });
    }

    fn draw_info(&self, ms: &ps2_mouse::MouseState) {
        let cx = ms.x as i32;
        let cy = ms.y as i32 - self.canvas_top as i32;
        let in_canvas = cy >= 0 && cy < self.ch as i32;
        let left = format!(
            "paint  tool:{}  color:#{:06X}  brush:{}",
            self.tool.name(),
            self.color & 0xFFFFFF,
            self.brush
        );
        let right = if in_canvas {
            format!("({:>4},{:>4})  q=quit", cx, cy)
        } else {
            String::from("toolbar  q=quit")
        };
        framebuffer::draw_status_bar(&left, &right);
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
        framebuffer::draw_status_bar(&msg, "q=quit");
        crate::kprintln!("{}", msg);
    }

    fn load(&mut self) {
        let node = match crate::vfs::lookup_path("/mnt/paint.img") {
            Ok(n) => n,
            Err(_) => {
                framebuffer::draw_status_bar("paint: /mnt/paint.img not found", "q=quit");
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
            framebuffer::draw_status_bar("paint: bad image header", "q=quit");
            return;
        }
        let fw = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
        let fh = u32::from_le_bytes([data[12], data[13], data[14], data[15]]) as usize;
        if fw != self.cw || fh != self.ch {
            framebuffer::draw_status_bar(
                "paint: image size mismatch", "q=quit",
            );
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
        framebuffer::draw_status_bar("paint: loaded /mnt/paint.img", "q=quit");
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
