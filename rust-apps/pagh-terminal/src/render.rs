//! Software RGBA renderer: terminal grid, colors, cursor, selection, scrollbar
//! and optional window chrome for screenshots.
//!
//! The renderer has no GPU/backend dependency; the compositor receives the
//! resulting RGBA frame through the PDP/1 protocol (or, for headless tests,
//! it is written straight to PNG).

use crate::cell::{default_palette, Attrs, Cell, Color, Rgb, CELL_WIDE};
use crate::font::{self, Glyph, CELL_H, CELL_W};
use crate::screen::Screen;

/// Padding around the text grid inside a client surface.
pub const VIEWPORT_PADDING: usize = 6;
/// Height of the screenshot-only title bar.
pub const TITLE_H: usize = 30;
/// Height of the screenshot-only status bar.
pub const STATUS_H: usize = 22;
/// Window border thickness.
pub const BORDER: usize = 1;

/// A straight-alpha RGBA image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    /// RGBA, 4 bytes per pixel, row-major, alpha is 255 everywhere this crate
    /// draws (the protocol commits opaque XRGB frames).
    pub pixels: Vec<u8>,
}

impl Canvas {
    pub fn new(width: usize, height: usize, bg: Rgb) -> Canvas {
        let mut pixels = Vec::with_capacity(width * height * 4);
        for _ in 0..width * height {
            pixels.extend_from_slice(&[bg.0, bg.1, bg.2, 255]);
        }
        Canvas {
            width,
            height,
            pixels,
        }
    }

    pub fn from_rgba(width: usize, height: usize, pixels: Vec<u8>) -> Option<Canvas> {
        if pixels.len() != width.checked_mul(height)?.checked_mul(4)? {
            return None;
        }
        Some(Canvas {
            width,
            height,
            pixels,
        })
    }

    pub fn fill(&mut self, bg: Rgb) {
        for chunk in self.pixels.chunks_exact_mut(4) {
            chunk.copy_from_slice(&[bg.0, bg.1, bg.2, 255]);
        }
    }

    #[inline]
    fn put(&mut self, x: usize, y: usize, color: Rgb) {
        if x >= self.width || y >= self.height {
            return;
        }
        let i = (y * self.width + x) * 4;
        self.pixels[i] = color.0;
        self.pixels[i + 1] = color.1;
        self.pixels[i + 2] = color.2;
        self.pixels[i + 3] = 255;
    }

    /// Alpha-blend `color` over the existing pixel.
    pub fn blend_pixel(&mut self, x: usize, y: usize, color: Rgb, alpha: u8) {
        if x >= self.width || y >= self.height || alpha == 0 {
            return;
        }
        if alpha == 255 {
            self.put(x, y, color);
            return;
        }
        let i = (y * self.width + x) * 4;
        let a = u32::from(alpha);
        let inv = 255 - a;
        for (dst, src) in self.pixels[i..i + 3]
            .iter_mut()
            .zip([color.0, color.1, color.2])
        {
            *dst = ((u32::from(*dst) * inv + u32::from(src) * a + 127) / 255) as u8;
        }
        self.pixels[i + 3] = 255;
    }

    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: Rgb) {
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        for py in y..y1 {
            for px in x..x1 {
                self.put(px, py, color);
            }
        }
    }

    pub fn blend_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: Rgb, alpha: u8) {
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        for py in y..y1 {
            for px in x..x1 {
                self.blend_pixel(px, py, color, alpha);
            }
        }
    }

    pub fn hline(&mut self, x: usize, y: usize, w: usize, color: Rgb) {
        self.fill_rect(x, y, w, 1, color);
    }

    pub fn vline(&mut self, x: usize, y: usize, h: usize, color: Rgb) {
        self.fill_rect(x, y, 1, h, color);
    }

    pub fn outline_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: Rgb) {
        if w == 0 || h == 0 {
            return;
        }
        self.hline(x, y, w, color);
        self.hline(x, y + h - 1, w, color);
        self.vline(x, y, h, color);
        self.vline(x + w - 1, y, h, color);
    }

    pub fn blit(&mut self, src: &Canvas, x: usize, y: usize) {
        for sy in 0..src.height {
            for sx in 0..src.width {
                let si = (sy * src.width + sx) * 4;
                let sx_dst = x + sx;
                let sy_dst = y + sy;
                if sx_dst >= self.width || sy_dst >= self.height {
                    continue;
                }
                let di = (sy_dst * self.width + sx_dst) * 4;
                self.pixels[di..di + 4].copy_from_slice(&src.pixels[si..si + 4]);
            }
        }
    }

    pub fn pixel(&self, x: usize, y: usize) -> [u8; 4] {
        let i = (y * self.width + x) * 4;
        [
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        ]
    }

    pub fn save_png(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        crate::png::save(self, path)
    }
}

/// Pagh terminal theme.  Teal accent, dark blue-green surfaces, readable ANSI
/// palette — the same accent family as the desktop concept art.
#[derive(Clone, Debug)]
pub struct Theme {
    pub bg: Rgb,
    pub fg: Rgb,
    pub cursor: Rgb,
    pub cursor_text: Rgb,
    pub selection: Rgb,
    pub selection_fg: Rgb,
    pub palette: [Rgb; 256],
    pub window_bg: Rgb,
    pub border: Rgb,
    pub title_bg: Rgb,
    pub title_fg: Rgb,
    pub status_bg: Rgb,
    pub status_fg: Rgb,
    pub accent: Rgb,
    pub button: Rgb,
    pub close: Rgb,
}

impl Default for Theme {
    fn default() -> Theme {
        Theme {
            bg: Rgb(0x0e, 0x16, 0x1b),
            fg: Rgb(0xd8, 0xe6, 0xe3),
            cursor: Rgb(0x2f, 0xd4, 0xb0),
            cursor_text: Rgb(0x06, 0x12, 0x14),
            selection: Rgb(0x1f, 0x5f, 0x59),
            selection_fg: Rgb(0xe8, 0xff, 0xfb),
            palette: default_palette(),
            window_bg: Rgb(0x0b, 0x12, 0x17),
            border: Rgb(0x25, 0x3a, 0x40),
            title_bg: Rgb(0x13, 0x22, 0x28),
            title_fg: Rgb(0xc9, 0xdc, 0xd9),
            status_bg: Rgb(0x10, 0x1d, 0x22),
            status_fg: Rgb(0x91, 0xaa, 0xa8),
            accent: Rgb(0x2f, 0xd4, 0xb0),
            button: Rgb(0x8f, 0xa6, 0xa4),
            close: Rgb(0xe0, 0x5c, 0x5c),
        }
    }
}

/// Viewport rendering options.
#[derive(Clone, Debug)]
pub struct RenderOptions {
    pub padding: usize,
    pub show_cursor: bool,
    pub focused: bool,
    pub blink_on: bool,
    pub show_scrollbar: bool,
}

impl Default for RenderOptions {
    fn default() -> RenderOptions {
        RenderOptions {
            padding: VIEWPORT_PADDING,
            show_cursor: true,
            focused: true,
            blink_on: true,
            show_scrollbar: true,
        }
    }
}

/// Render the terminal grid (no title/status chrome).
pub fn render_screen(screen: &Screen, theme: &Theme, opts: &RenderOptions) -> Canvas {
    let width = opts.padding * 2 + screen.cols() * CELL_W;
    let height = opts.padding * 2 + screen.rows() * CELL_H;
    let mut canvas = Canvas::new(width, height, theme.bg);
    render_screen_into(&mut canvas, opts.padding, opts.padding, screen, theme, opts);
    canvas
}

/// Render the grid at an arbitrary origin inside an existing canvas.
pub fn render_screen_into(
    canvas: &mut Canvas,
    ox: usize,
    oy: usize,
    screen: &Screen,
    theme: &Theme,
    opts: &RenderOptions,
) {
    let cursor_on = opts.show_cursor
        && screen.cursor.visible
        && screen.cursor_in_view()
        && (opts.blink_on || !screen.cursor_blink);
    let cursor_block = cursor_on && screen.cursor.style == crate::screen::CursorStyle::Block;

    for y in 0..screen.rows() {
        let line = screen.view_line(y);
        let abs_line = screen.absolute_line(y);
        // Pass 1: backgrounds.  Wide glyphs are 16 px wide and must not be
        // painted over by the background of their continuation cell, so all
        // backgrounds go down before any glyph.
        for x in 0..screen.cols() {
            let cell = line.get(x).copied().unwrap_or_default();
            let selected = screen.is_selected(abs_line, x);
            let is_cursor = cursor_on && x == screen.cursor.x && y == screen.cursor.y;
            let style = resolve_style(
                &cell,
                theme,
                screen,
                selected,
                is_cursor && cursor_block,
                opts.blink_on,
            );
            canvas.fill_rect(ox + x * CELL_W, oy + y * CELL_H, CELL_W, CELL_H, style.bg);
        }
        // Pass 2: glyphs and decorations, one per cell; continuation cells are
        // covered by the wide glyph drawn from their leading cell.
        for x in 0..screen.cols() {
            let cell = line.get(x).copied().unwrap_or_default();
            if cell.is_wide_continuation() {
                continue;
            }
            let selected = screen.is_selected(abs_line, x);
            let is_cursor = cursor_on && x == screen.cursor.x && y == screen.cursor.y;
            let style = resolve_style(
                &cell,
                theme,
                screen,
                selected,
                is_cursor && cursor_block,
                opts.blink_on,
            );
            draw_cell(
                canvas,
                ox + x * CELL_W,
                oy + y * CELL_H,
                &cell,
                style.fg,
                style.bg,
                style.focused_cursor,
            );
        }
    }

    // Bar / underline cursor styles are drawn over the cell.
    if cursor_on && screen.cursor_in_view() && !cursor_block {
        let px = ox + screen.cursor.x * CELL_W;
        let py = oy + screen.cursor.y * CELL_H;
        let color = if opts.focused {
            theme.cursor
        } else {
            theme.button
        };
        match screen.cursor.style {
            crate::screen::CursorStyle::Underline => {
                canvas.fill_rect(px, py + CELL_H - 2, CELL_W, 2, color)
            }
            crate::screen::CursorStyle::Bar => canvas.fill_rect(px, py, 2, CELL_H, color),
            crate::screen::CursorStyle::Block => {}
        }
        if !opts.focused {
            canvas.outline_rect(px, py, CELL_W, CELL_H, color);
        }
    } else if cursor_on && !opts.focused {
        let px = ox + screen.cursor.x * CELL_W;
        let py = oy + screen.cursor.y * CELL_H;
        canvas.outline_rect(px, py, CELL_W, CELL_H, theme.button);
    }

    if opts.show_scrollbar {
        draw_scrollbar(canvas, ox, oy, screen, theme);
    }
}

/// Render the grid plus a Pagh-style title bar and status bar.  The compositor
/// normally draws decorations itself; this is for screenshots and standalone
/// preview, and it makes the headless output representative of the window.
pub fn render_window(screen: &Screen, theme: &Theme, title: &str, opts: &RenderOptions) -> Canvas {
    let content = render_screen(screen, theme, opts);
    let width = content.width + BORDER * 2;
    let height = content.height + BORDER * 2 + TITLE_H + STATUS_H;
    let mut canvas = Canvas::new(width, height, theme.window_bg);
    canvas.outline_rect(0, 0, width, height, theme.border);

    // Title bar.
    canvas.fill_rect(BORDER, BORDER, width - BORDER * 2, TITLE_H, theme.title_bg);
    canvas.fill_rect(
        BORDER,
        BORDER + TITLE_H - 2,
        width - BORDER * 2,
        2,
        if opts.focused {
            theme.accent
        } else {
            theme.border
        },
    );
    for (i, color) in [theme.accent, Rgb(0xe6, 0xc3, 0x5c), Rgb(0xe0, 0x5c, 0x5c)]
        .into_iter()
        .enumerate()
    {
        canvas.fill_rect(BORDER + 12 + i * 12, BORDER + 11, 7, 7, color);
    }
    let title = if title.is_empty() {
        "pagh-terminal".to_string()
    } else {
        title.to_string()
    };
    let title_y = BORDER + (TITLE_H - CELL_H) / 2;
    let max_title_px = width.saturating_sub(BORDER + 58 + 78);
    draw_text(
        &mut canvas,
        BORDER + 50,
        title_y,
        &title,
        theme.title_fg,
        max_title_px,
    );

    // Window buttons (minimize / maximize / close).
    let button_y = BORDER + TITLE_H / 2;
    let bx = width - BORDER - 78;
    let color = theme.button;
    canvas.hline(bx, button_y, 10, color);
    canvas.outline_rect(bx + 26, button_y - 4, 9, 9, color);
    let cx = bx + 52;
    for i in 0..9 {
        canvas.put(cx + i, button_y - 4 + i, theme.close);
        canvas.put(cx + 8 - i, button_y - 4 + i, theme.close);
    }

    // Content.
    canvas.blit(&content, BORDER, BORDER + TITLE_H);

    // Status bar.
    let status_y = height - BORDER - STATUS_H;
    canvas.fill_rect(
        BORDER,
        status_y,
        width - BORDER * 2,
        STATUS_H,
        theme.status_bg,
    );
    let scroll = if screen.view_offset() > 0 {
        format!(
            "  SCROLL {}/{}",
            screen.view_offset(),
            screen.scrollback_len()
        )
    } else {
        String::new()
    };
    let status = format!(
        "{}x{}  UTF-8  ANSI 256  SB {}{}  {}:{}",
        screen.cols(),
        screen.rows(),
        screen.scrollback_len(),
        scroll,
        screen.cursor.x,
        screen.cursor.y
    );
    let status_text_y = status_y + (STATUS_H - CELL_H) / 2;
    draw_text(
        &mut canvas,
        BORDER + 10,
        status_text_y,
        &status,
        theme.status_fg,
        width.saturating_sub(120),
    );
    let right = "PaghTerm 0.1";
    let right_px = text_width(right);
    if right_px + 20 < width {
        draw_text(
            &mut canvas,
            width - BORDER - 10 - right_px,
            status_text_y,
            right,
            theme.status_fg,
            right_px,
        );
    }
    canvas
}

struct Style {
    fg: Rgb,
    bg: Rgb,
    focused_cursor: bool,
}

fn resolve_style(
    cell: &Cell,
    theme: &Theme,
    screen: &Screen,
    selected: bool,
    cursor_block: bool,
    blink_on: bool,
) -> Style {
    let mut fg = cell.fg.resolve(&theme.palette, theme.fg);
    let mut bg = cell.bg.resolve(&theme.palette, theme.bg);

    if cell.attrs.contains(Attrs::REVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.attrs.contains(Attrs::BOLD) {
        if let Color::Indexed(i) = cell.fg {
            if i < 8 {
                fg = theme.palette[i as usize + 8];
            }
        }
        fg = fg.lerp(Rgb::WHITE, 0.12);
    }
    if cell.attrs.contains(Attrs::DIM) {
        fg = fg.scale(0.62);
    }
    if cell.attrs.contains(Attrs::HIDDEN) {
        fg = bg;
    }
    if cell.attrs.contains(Attrs::BLINK) && !blink_on {
        fg = bg;
    }
    if screen.reverse_video {
        std::mem::swap(&mut fg, &mut bg);
    }
    if selected {
        bg = theme.selection;
        fg = theme.selection_fg;
    }
    let focused_cursor = cursor_block;
    if cursor_block {
        bg = theme.cursor;
        fg = theme.cursor_text;
    }
    Style {
        fg,
        bg,
        focused_cursor,
    }
}

fn draw_cell(
    canvas: &mut Canvas,
    x: usize,
    y: usize,
    cell: &Cell,
    fg: Rgb,
    _bg: Rgb,
    cursor_block: bool,
) {
    let attrs = cell.attrs;
    let ch = cell.ch;
    if ch != ' ' && ch != '\0' {
        if matches!(ch, '╱' | '╲' | '╳') {
            draw_diagonal_box(canvas, x, y, ch, fg);
        } else if let Some(spec) = box_spec(ch) {
            draw_box(canvas, x, y, spec, fg);
        } else if let Some(rects) = block_rects(ch) {
            for (rx, ry, rw, rh, alpha) in rects {
                if alpha == 255 {
                    canvas.fill_rect(x + rx, y + ry, rw, rh, fg);
                } else {
                    canvas.blend_rect(x + rx, y + ry, rw, rh, fg, alpha);
                }
            }
        } else {
            let width_expected = if cell.flags & CELL_WIDE != 0 { 2 } else { 1 };
            match font::glyph(ch) {
                Some(glyph) => {
                    draw_coverage(
                        canvas,
                        x,
                        y,
                        &glyph,
                        fg,
                        attrs.contains(Attrs::BOLD),
                        attrs.contains(Attrs::ITALIC),
                    );
                }
                None if width_expected == 2 => {
                    draw_placeholder_wide(canvas, x, y, fg);
                }
                None => {
                    let glyph = font::replacement_glyph();
                    draw_coverage(canvas, x, y, &glyph, fg, false, false);
                }
            }
        }
    }
    if attrs.contains(Attrs::UNDERLINE) {
        canvas.fill_rect(x, y + CELL_H - 2, CELL_W, 1, fg);
    }
    if attrs.contains(Attrs::STRIKETHROUGH) {
        canvas.fill_rect(x, y + CELL_H / 2, CELL_W, 1, fg);
    }
    if attrs.contains(Attrs::OVERLINE) {
        canvas.fill_rect(x, y + 1, CELL_W, 1, fg);
    }
    if cursor_block {
        // The block cursor already painted the cell background; a faint inner
        // outline keeps the cursor visible on top of an inverse-video glyph.
        canvas.outline_rect(x, y, CELL_W, CELL_H, fg);
    }
}

fn draw_coverage(
    canvas: &mut Canvas,
    x: usize,
    y: usize,
    glyph: &Glyph,
    fg: Rgb,
    bold: bool,
    italic: bool,
) {
    for row in 0..glyph.height {
        let shift = if italic { (glyph.height - row) / 7 } else { 0 };
        for col in 0..glyph.width {
            let mut alpha = glyph.coverage_at(col, row);
            if alpha == 0 {
                continue;
            }
            if bold {
                alpha = (u16::from(alpha) + 58).min(255) as u8;
            }
            canvas.blend_pixel(x + col + shift, y + row, fg, alpha);
        }
    }
}

fn draw_placeholder_wide(canvas: &mut Canvas, x: usize, y: usize, fg: Rgb) {
    let w = CELL_W * 2;
    canvas.outline_rect(x + 1, y + 2, w - 2, CELL_H - 4, fg);
    canvas.hline(x + 4, y + CELL_H / 2, w - 8, fg);
}

fn draw_text(canvas: &mut Canvas, x: usize, y: usize, text: &str, fg: Rgb, max_px: usize) {
    let mut px = 0usize;
    for ch in text.chars() {
        let advance = if font::char_width(ch) == 2 {
            2 * CELL_W
        } else {
            CELL_W
        };
        if px + advance > max_px {
            break;
        }
        match font::glyph(ch) {
            Some(glyph) => draw_coverage(canvas, x + px, y, &glyph, fg, false, false),
            None => {
                if let Some(glyph) = font::glyph('?') {
                    draw_coverage(canvas, x + px, y, &glyph, fg, false, false);
                }
            }
        }
        px += advance;
    }
}

fn text_width(text: &str) -> usize {
    text.chars()
        .map(|c| {
            if font::char_width(c) == 2 {
                2 * CELL_W
            } else {
                CELL_W
            }
        })
        .sum()
}

fn draw_scrollbar(canvas: &mut Canvas, ox: usize, oy: usize, screen: &Screen, theme: &Theme) {
    let sb = screen.scrollback_len();
    if sb == 0 {
        return;
    }
    let track_h = screen.rows() * CELL_H - 8;
    let track_x = ox + screen.cols() * CELL_W + 2;
    let track_y = oy + 4;
    canvas.fill_rect(track_x, track_y, 2, track_h, theme.border);
    let total = (sb + screen.rows()) as f32;
    let visible = screen.rows() as f32 / total;
    let thumb_h = ((track_h as f32 * visible).round() as usize)
        .max(14)
        .min(track_h);
    let start = (sb.saturating_sub(screen.view_offset())) as f32 / total;
    let thumb_y = track_y + ((track_h - thumb_h) as f32 * start).round() as usize;
    canvas.fill_rect(track_x, thumb_y, 2, thumb_h, theme.accent);
}

// ── box drawing & block elements ───────────────────────────────────────────

const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;

#[derive(Clone, Copy)]
struct BoxSpec {
    bits: u8,
    style: BoxStyle,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BoxStyle {
    Light,
    Heavy,
    Double,
    Dashed,
}

fn box_spec(ch: char) -> Option<BoxSpec> {
    use BoxStyle::*;
    let (bits, style) = match ch {
        '─' => (LEFT | RIGHT, Light),
        '━' => (LEFT | RIGHT, Heavy),
        '│' => (UP | DOWN, Light),
        '┃' => (UP | DOWN, Heavy),
        '┄' | '┈' | '╌' => (LEFT | RIGHT, Dashed),
        '┅' | '┉' | '╍' => (LEFT | RIGHT, Dashed),
        '┆' | '┊' | '╎' => (UP | DOWN, Dashed),
        '┇' | '┋' | '╏' => (UP | DOWN, Dashed),
        '┌' | '╭' => (RIGHT | DOWN, Light),
        '┍' | '┎' | '┏' => (RIGHT | DOWN, Heavy),
        '┐' | '╮' => (LEFT | DOWN, Light),
        '┑' | '┒' | '┓' => (LEFT | DOWN, Heavy),
        '└' | '╰' => (RIGHT | UP, Light),
        '┕' | '┖' | '┗' => (RIGHT | UP, Heavy),
        '┘' | '╯' => (LEFT | UP, Light),
        '┙' | '┚' | '┛' => (LEFT | UP, Heavy),
        '├' => (UP | DOWN | RIGHT, Light),
        '┝' | '┞' | '┟' | '┠' | '┡' | '┢' | '┣' => (UP | DOWN | RIGHT, Heavy),
        '┤' => (UP | DOWN | LEFT, Light),
        '┥' | '┦' | '┧' | '┨' | '┩' | '┪' | '┫' => (UP | DOWN | LEFT, Heavy),
        '┬' => (LEFT | RIGHT | DOWN, Light),
        '┭' | '┮' | '┯' | '┰' | '┱' | '┲' | '┳' => (LEFT | RIGHT | DOWN, Heavy),
        '┴' => (LEFT | RIGHT | UP, Light),
        '┵' | '┶' | '┷' | '┸' | '┹' | '┺' | '┻' => (LEFT | RIGHT | UP, Heavy),
        '┼' => (UP | DOWN | LEFT | RIGHT, Light),
        '┽' | '┾' | '┿' | '╀' | '╁' | '╂' | '╃' | '╄' | '╅' | '╆' | '╇' | '╈' | '╉' | '╊' | '╋' => {
            (UP | DOWN | LEFT | RIGHT, Heavy)
        }
        '═' => (LEFT | RIGHT, Double),
        '║' => (UP | DOWN, Double),
        '╒' | '╓' => (RIGHT | DOWN, Double),
        '╔' => (RIGHT | DOWN, Double),
        '╕' | '╖' => (LEFT | DOWN, Double),
        '╗' => (LEFT | DOWN, Double),
        '╘' | '╙' => (RIGHT | UP, Double),
        '╚' => (RIGHT | UP, Double),
        '╛' | '╜' => (LEFT | UP, Double),
        '╝' => (LEFT | UP, Double),
        '╞' | '╟' => (UP | DOWN | RIGHT, Double),
        '╠' => (UP | DOWN | RIGHT, Double),
        '╡' | '╢' => (UP | DOWN | LEFT, Double),
        '╣' => (UP | DOWN | LEFT, Double),
        '╤' | '╥' => (LEFT | RIGHT | DOWN, Double),
        '╦' => (LEFT | RIGHT | DOWN, Double),
        '╧' | '╨' => (LEFT | RIGHT | UP, Double),
        '╩' => (LEFT | RIGHT | UP, Double),
        '╪' | '╫' => (UP | DOWN | LEFT | RIGHT, Double),
        '╬' => (UP | DOWN | LEFT | RIGHT, Double),
        '╴' => (LEFT, Light),
        '╵' => (UP, Light),
        '╶' => (RIGHT, Light),
        '╷' => (DOWN, Light),
        '╸' => (LEFT, Heavy),
        '╹' => (UP, Heavy),
        '╺' => (RIGHT, Heavy),
        '╻' => (DOWN, Heavy),
        '╼' => (LEFT | RIGHT, Light),
        '╽' => (UP | DOWN, Light),
        '╾' => (LEFT | RIGHT, Heavy),
        '╿' => (UP | DOWN, Heavy),
        _ => return None,
    };
    Some(BoxSpec { bits, style })
}

fn draw_diagonal_box(canvas: &mut Canvas, x: usize, y: usize, ch: char, fg: Rgb) {
    let w = CELL_W;
    let h = CELL_H;
    if ch == '╱' || ch == '╳' {
        for i in 0..h {
            let px = (w - 1).saturating_sub(i * w / h);
            canvas.put(x + px, y + i, fg);
        }
    }
    if ch == '╲' || ch == '╳' {
        for i in 0..h {
            let px = i * w / h;
            canvas.put(x + px.min(w - 1), y + i, fg);
        }
    }
}

fn draw_box(canvas: &mut Canvas, x: usize, y: usize, spec: BoxSpec, fg: Rgb) {
    let cx = CELL_W / 2;
    let cy = CELL_H / 2;
    let thick = spec.style == BoxStyle::Heavy;
    let double = spec.style == BoxStyle::Double;
    let dashed = spec.style == BoxStyle::Dashed;
    let horizontal = |canvas: &mut Canvas, y_offset: usize| {
        let line_y = y + y_offset;
        if spec.bits & LEFT != 0 {
            draw_segment(
                canvas,
                x,
                line_y,
                x + cx,
                line_y,
                thick,
                double,
                dashed,
                true,
                fg,
            );
        }
        if spec.bits & RIGHT != 0 {
            draw_segment(
                canvas,
                x + cx,
                line_y,
                x + CELL_W,
                line_y,
                thick,
                double,
                dashed,
                true,
                fg,
            );
        }
    };
    let vertical = |canvas: &mut Canvas, x_offset: usize| {
        let line_x = x + x_offset;
        if spec.bits & UP != 0 {
            draw_segment(
                canvas,
                line_x,
                y,
                line_x,
                y + cy,
                thick,
                double,
                dashed,
                false,
                fg,
            );
        }
        if spec.bits & DOWN != 0 {
            draw_segment(
                canvas,
                line_x,
                y + cy,
                line_x,
                y + CELL_H,
                thick,
                double,
                dashed,
                false,
                fg,
            );
        }
    };
    if double {
        horizontal(canvas, cy.saturating_sub(1));
        vertical(canvas, cx.saturating_sub(1));
        horizontal(canvas, cy + 1);
        vertical(canvas, cx + 1);
    } else {
        horizontal(canvas, cy);
        vertical(canvas, cx);
        if thick {
            horizontal(canvas, cy + 1);
            vertical(canvas, cx + 1);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_segment(
    canvas: &mut Canvas,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    thick: bool,
    double: bool,
    dashed: bool,
    horizontal: bool,
    fg: Rgb,
) {
    if horizontal {
        let len = x1.saturating_sub(x0);
        if dashed {
            let mut i = 0;
            while i < len {
                if (i / 2) % 2 == 0 {
                    canvas.fill_rect(x0 + i, y0, 2.min(len - i), 1, fg);
                }
                i += 2;
            }
        } else {
            canvas.fill_rect(x0, y0, len, if thick { 2 } else { 1 }, fg);
        }
        let _ = (y1, double);
    } else {
        let len = y1.saturating_sub(y0);
        if dashed {
            let mut i = 0;
            while i < len {
                if (i / 2) % 2 == 0 {
                    canvas.fill_rect(x0, y0 + i, 1, 2.min(len - i), fg);
                }
                i += 2;
            }
        } else {
            canvas.fill_rect(x0, y0, if thick { 2 } else { 1 }, len, fg);
        }
        let _ = (x1, double);
    }
}

/// Rectangles (x, y, w, h, alpha) for Unicode block elements.
fn block_rects(ch: char) -> Option<Vec<(usize, usize, usize, usize, u8)>> {
    let w = CELL_W;
    let h = CELL_H;
    Some(match ch {
        '\u{2588}' => vec![(0, 0, w, h, 255)],
        '\u{2580}' => vec![(0, 0, w, h / 2, 255)],
        '\u{2581}' => vec![(0, h - h / 8, w, h / 8, 255)],
        '\u{2582}' => vec![(0, h - h / 4, w, h / 4, 255)],
        '\u{2583}' => vec![(0, h - h * 3 / 8, w, h * 3 / 8, 255)],
        '\u{2584}' => vec![(0, h / 2, w, h - h / 2, 255)],
        '\u{2585}' => vec![(0, h - h * 5 / 8, w, h * 5 / 8, 255)],
        '\u{2586}' => vec![(0, h - h * 3 / 4, w, h * 3 / 4, 255)],
        '\u{2587}' => vec![(0, h - h * 7 / 8, w, h * 7 / 8, 255)],
        '\u{2589}' => vec![(0, 0, w * 7 / 8, h, 255)],
        '\u{258A}' => vec![(0, 0, w * 3 / 4, h, 255)],
        '\u{258B}' => vec![(0, 0, w * 5 / 8, h, 255)],
        '\u{258C}' => vec![(0, 0, w / 2, h, 255)],
        '\u{258D}' => vec![(0, 0, w * 3 / 8, h, 255)],
        '\u{258E}' => vec![(0, 0, w / 4, h, 255)],
        '\u{258F}' => vec![(0, 0, w / 8, h, 255)],
        '\u{2590}' => vec![(w / 2, 0, w - w / 2, h, 255)],
        '\u{2591}' => vec![(0, 0, w, h, 64)],
        '\u{2592}' => vec![(0, 0, w, h, 128)],
        '\u{2593}' => vec![(0, 0, w, h, 192)],
        '\u{2594}' => vec![(0, 0, w, h / 8, 255)],
        '\u{2595}' => vec![(w - w / 8, 0, w / 8, h, 255)],
        '\u{2596}' => vec![(0, h / 2, w / 2, h - h / 2, 255)],
        '\u{2597}' => vec![(w / 2, h / 2, w - w / 2, h - h / 2, 255)],
        '\u{2598}' => vec![(0, 0, w / 2, h / 2, 255)],
        '\u{2599}' => vec![
            (0, 0, w / 2, h / 2, 255),
            (0, h / 2, w / 2, h - h / 2, 255),
            (w / 2, h / 2, w - w / 2, h - h / 2, 255),
        ],
        '\u{259A}' => vec![
            (0, 0, w / 2, h / 2, 255),
            (w / 2, h / 2, w - w / 2, h - h / 2, 255),
        ],
        '\u{259B}' => vec![
            (0, 0, w / 2, h / 2, 255),
            (w / 2, 0, w - w / 2, h / 2, 255),
            (0, h / 2, w / 2, h - h / 2, 255),
        ],
        '\u{259C}' => vec![
            (0, 0, w / 2, h / 2, 255),
            (w / 2, 0, w - w / 2, h / 2, 255),
            (w / 2, h / 2, w - w / 2, h - h / 2, 255),
        ],
        '\u{259D}' => vec![(w / 2, 0, w - w / 2, h / 2, 255)],
        '\u{259E}' => vec![
            (w / 2, 0, w - w / 2, h / 2, 255),
            (0, h / 2, w / 2, h - h / 2, 255),
        ],
        '\u{259F}' => vec![
            (w / 2, 0, w - w / 2, h / 2, 255),
            (0, h / 2, w / 2, h - h / 2, 255),
            (w / 2, h / 2, w - w / 2, h - h / 2, 255),
        ],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Terminal;

    fn screen_with(text: &str) -> Terminal {
        let mut t = Terminal::new(10, 3);
        t.feed_str(text);
        t
    }

    #[test]
    fn full_block_cell_is_exactly_foreground() {
        let mut t = Terminal::new(10, 3);
        t.screen.pen.fg = Color::Rgb(255, 0, 0);
        t.feed_str("█");
        let theme = Theme::default();
        let canvas = render_screen(&t.screen, &theme, &RenderOptions::default());
        let px = canvas.pixel(VIEWPORT_PADDING + 2, VIEWPORT_PADDING + 2);
        assert_eq!(&px[..3], &[255, 0, 0]);
    }

    #[test]
    fn background_and_glyph_pixels_differ() {
        let t = screen_with("A");
        let theme = Theme::default();
        let canvas = render_screen(&t.screen, &theme, &RenderOptions::default());
        let bg = canvas.pixel(0, 0);
        let mut colored = 0;
        for y in 0..18 {
            for x in 0..8 {
                if canvas.pixel(VIEWPORT_PADDING + x, VIEWPORT_PADDING + y)[..3] != bg[..3] {
                    colored += 1;
                }
            }
        }
        assert!(colored > 10, "only {colored} glyph pixels");
    }

    #[test]
    fn cursor_paints_accent_color() {
        let mut t = Terminal::new(5, 2);
        t.feed_str("ab");
        let theme = Theme::default();
        let canvas = render_screen(&t.screen, &theme, &RenderOptions::default());
        let px = canvas.pixel(VIEWPORT_PADDING + 2 * CELL_W + 3, VIEWPORT_PADDING + 3);
        assert_eq!(&px[..3], &theme.cursor.to_array());
    }

    #[test]
    fn selection_overrides_background() {
        let mut t = screen_with("hello");
        t.screen
            .begin_selection_abs(0, 1, crate::screen::SelectionMode::Char);
        t.screen.extend_selection_abs(0, 3);
        let theme = Theme::default();
        let canvas = render_screen(&t.screen, &theme, &RenderOptions::default());
        let px = canvas.pixel(VIEWPORT_PADDING + CELL_W + 2, VIEWPORT_PADDING + 2);
        assert_eq!(&px[..3], &theme.selection.to_array());
    }

    #[test]
    fn box_drawing_is_seamless() {
        let mut t = Terminal::new(10, 3);
        t.feed_str("┌─┐");
        let theme = Theme::default();
        let canvas = render_screen(&t.screen, &theme, &RenderOptions::default());
        let y = VIEWPORT_PADDING + CELL_H / 2;
        for x in [CELL_W / 2 + 1, CELL_W + 2, CELL_W * 2 + 2] {
            assert_eq!(
                &canvas.pixel(VIEWPORT_PADDING + x, y)[..3],
                &theme.fg.to_array(),
                "gap at x={x}"
            );
        }
    }

    #[test]
    fn window_chrome_has_expected_size() {
        let t = screen_with("x");
        let canvas = render_window(
            &t.screen,
            &Theme::default(),
            "test",
            &RenderOptions::default(),
        );
        assert_eq!(
            canvas.width,
            VIEWPORT_PADDING * 2 + 10 * CELL_W + BORDER * 2
        );
        assert_eq!(
            canvas.height,
            VIEWPORT_PADDING * 2 + 3 * CELL_H + BORDER * 2 + TITLE_H + STATUS_H
        );
    }
}
