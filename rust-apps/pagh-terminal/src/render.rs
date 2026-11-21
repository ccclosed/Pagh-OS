//! Software RGBA renderer: terminal grid, colors, cursor, selection, scrollbar
//! and optional window chrome for screenshots.
//!
//! The renderer has no GPU/backend dependency; the compositor receives the
//! resulting RGBA frame through the PDP/1 protocol (or, for headless tests,
//! it is written straight to PNG).
//!
//! The visual language is a plain-terminal one: the grid fills the surface,
//! the palette is the stock Breeze-dark ANSI set, and the screenshot chrome is
//! a single hairline title strip.  Nothing here is interactive: in the live
//! PDP/1 path the compositor owns the window title bar and buttons
//! (`PROTOCOL.md`), so the client commits content only.

use crate::cell::{default_palette, Attrs, Cell, Color, Rgb, CELL_WIDE};
use crate::font::{self, Glyph, CELL_H, CELL_W};
use crate::screen::Screen;

/// Padding around the text grid inside a client surface.
pub const VIEWPORT_PADDING: usize = 2;
/// Share of [`Theme::selection`] mixed into a selected cell's background.
///
/// Selection is a *wash over the existing background*, not an opaque repaint:
/// background art (ANSI `48;*` cells, colour swatches) stays readable and the
/// foreground is never touched, while 60 % accent is still unmistakably a
/// highlight on the dark theme.  100 % would erase the background; a much lower
/// value makes the selection hard to see.
pub const SELECTION_BG_MIX: f32 = 0.6;
/// Height of the screenshot-only title strip.
pub const TITLE_H: usize = 22;
/// Window border thickness.
pub const BORDER: usize = 1;

/// A straight-alpha RGBA image or a client `ShmSurface` mapping.
///
/// By default pixels live in [`Canvas::pixels`] as RGBA (screenshots, PNG,
/// headless).  [`Canvas::from_mapped`] instead renders directly into the memfd
/// mapping in PDP/1 wire order (XRGB8888 little-endian `B,G,R,0`), so terminal
/// frames reach the compositor without a socket upload.
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    /// RGBA, 4 bytes per pixel, row-major.  Empty for a mapped surface.
    pub pixels: Vec<u8>,
    mapping: Option<pagh_ui::shm::MappedBuffer>,
    /// Bytes per row (`width * 4` for both RGBA and XRGB8888).
    stride: usize,
}

impl PartialEq for Canvas {
    fn eq(&self, other: &Canvas) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.pixels == other.pixels
            && self.surface_bytes() == other.surface_bytes()
    }
}

impl Eq for Canvas {}

impl std::fmt::Debug for Canvas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Canvas")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("mapped", &self.mapping.is_some())
            .finish_non_exhaustive()
    }
}

impl Clone for Canvas {
    fn clone(&self) -> Canvas {
        if let Some(mapping) = &self.mapping {
            let mut pixels = Vec::with_capacity(self.width * self.height * 4);
            for y in 0..self.height {
                for x in 0..self.width {
                    pixels.extend_from_slice(&mapped_rgba(mapping.as_slice(), self.stride, x, y));
                }
            }
            return Canvas {
                width: self.width,
                height: self.height,
                pixels,
                mapping: None,
                stride: self.width * 4,
            };
        }
        Canvas {
            width: self.width,
            height: self.height,
            pixels: self.pixels.clone(),
            mapping: None,
            stride: self.stride,
        }
    }
}

/// Fill `bytes` with a repeated 4-byte surface pixel.
///
/// The loop shape is not cosmetic: `chunks_exact_mut(4)` + `copy_from_slice`
/// is what LLVM unrolls into wide (vector) stores, while assigning an array
/// element by element (`*chunk = [..]` over `as_chunks_mut::<4>`) is not. On a
/// 900×600 surface (2.16 MiB) that is 0.055 ms against 0.32 ms; "wider" blocks
/// (`as_chunks_mut::<8>`/`<16>` + `copy_from_slice`) measure 0.25 ms and break
/// the unroll as well. No alignment is required, so the same code serves a
/// `Vec<u8>` and a `MAP_SHARED` mapping with an arbitrary `stride`, and it
/// stays inside the crate's `#![forbid(unsafe_code)]`.
fn fill_pixel_run(bytes: &mut [u8], pixel: [u8; 4]) {
    let mut chunks = bytes.chunks_exact_mut(4);
    for chunk in chunks.by_ref() {
        chunk.copy_from_slice(&pixel);
    }
    // No < 4-byte tail occurs in this renderer (every length is a multiple of
    // 4), but a fill must cover its slice completely or the frame gets a hole.
    for (i, byte) in chunks.into_remainder().iter_mut().enumerate() {
        *byte = pixel[i % 4];
    }
}

/// The 4 bytes of a pixel in surface storage order: RGBA for an own buffer,
/// `B,G,R,0` for a client `ShmSurface` (the PDP/1 wire order).
#[inline]
fn surface_pixel(mapped: bool, color: Rgb) -> [u8; 4] {
    if mapped {
        [color.2, color.1, color.0, 0]
    } else {
        [color.0, color.1, color.2, 255]
    }
}

#[inline]
fn mapped_rgba(bytes: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
    let offset = y * stride + x * 4;
    [bytes[offset + 2], bytes[offset + 1], bytes[offset], 255]
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
            mapping: None,
            stride: width * 4,
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
            mapping: None,
            stride: width * 4,
        })
    }

    /// Поверхность, пишущая прямо в `mmap` клиентской `ShmSurface` (XRGB8888).
    ///
    /// `buffer` должен покрывать `stride * height`; `pixels` остаётся пустым.
    pub fn from_mapped(
        buffer: pagh_ui::shm::MappedBuffer,
        width: usize,
        height: usize,
        stride: usize,
    ) -> Option<Canvas> {
        if stride < width.checked_mul(4)? || buffer.len() < stride.checked_mul(height)? {
            return None;
        }
        Some(Canvas {
            width,
            height,
            pixels: Vec::new(),
            mapping: Some(buffer),
            stride,
        })
    }

    pub fn is_mapped(&self) -> bool {
        self.mapping.is_some()
    }

    /// Байты видимой поверхности (RGBA или сырой XRGB mapping).
    pub fn surface_bytes(&self) -> &[u8] {
        match &self.mapping {
            Some(mapping) => mapping.as_slice(),
            None => &self.pixels,
        }
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn fill(&mut self, bg: Rgb) {
        if let Some(mapping) = &mut self.mapping {
            let pixel = [bg.2, bg.1, bg.0, 0];
            let bytes = mapping.as_mut_slice();
            for y in 0..self.height {
                let row = &mut bytes[y * self.stride..y * self.stride + self.width * 4];
                fill_pixel_run(row, pixel);
            }
            return;
        }
        fill_pixel_run(&mut self.pixels, [bg.0, bg.1, bg.2, 255]);
    }

    #[inline]
    fn put(&mut self, x: usize, y: usize, color: Rgb) {
        if x >= self.width || y >= self.height {
            return;
        }
        if let Some(mapping) = &mut self.mapping {
            let i = y * self.stride + x * 4;
            let pixel = surface_pixel(true, color);
            mapping.as_mut_slice()[i] = pixel[0];
            mapping.as_mut_slice()[i + 1] = pixel[1];
            mapping.as_mut_slice()[i + 2] = pixel[2];
            mapping.as_mut_slice()[i + 3] = pixel[3];
            return;
        }
        let pixel = surface_pixel(false, color);
        let i = (y * self.width + x) * 4;
        self.pixels[i] = pixel[0];
        self.pixels[i + 1] = pixel[1];
        self.pixels[i + 2] = pixel[2];
        self.pixels[i + 3] = pixel[3];
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
        let dst = self.pixel(x, y);
        let a = u32::from(alpha);
        let inv = 255 - a;
        let blend = |dst: u8, src: u8| -> u8 {
            ((u32::from(dst) * inv + u32::from(src) * a + 127) / 255) as u8
        };
        self.put(
            x,
            y,
            Rgb(
                blend(dst[0], color.0),
                blend(dst[1], color.1),
                blend(dst[2], color.2),
            ),
        );
    }

    /// Fill a rectangle without blending, clipped to the surface.
    ///
    /// Each row is one contiguous store (row-store): bounds and the
    /// `mapping`/`pixels` choice are resolved once per row instead of once per
    /// pixel. On the 112×33 grid this is the 8×18 cell background that used to
    /// go through [`Canvas::put`] — 532k calls per frame, 0.99 ms against
    /// 0.10 ms after the change.
    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: Rgb) {
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        if x >= x1 || y >= y1 {
            return;
        }
        let run = x1 - x;
        if let Some(mapping) = &mut self.mapping {
            let pixel = surface_pixel(true, color);
            let bytes = mapping.as_mut_slice();
            for py in y..y1 {
                let start = py * self.stride + x * 4;
                fill_pixel_run(&mut bytes[start..start + run * 4], pixel);
            }
            return;
        }
        let pixel = surface_pixel(false, color);
        let stride = self.width * 4;
        for py in y..y1 {
            let start = py * stride + x * 4;
            fill_pixel_run(&mut self.pixels[start..start + run * 4], pixel);
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
                let sx_dst = x + sx;
                let sy_dst = y + sy;
                if sx_dst >= self.width || sy_dst >= self.height {
                    continue;
                }
                self.put(
                    sx_dst,
                    sy_dst,
                    Rgb(
                        src.pixel(sx, sy)[0],
                        src.pixel(sx, sy)[1],
                        src.pixel(sx, sy)[2],
                    ),
                );
            }
        }
    }

    pub fn pixel(&self, x: usize, y: usize) -> [u8; 4] {
        if let Some(mapping) = &self.mapping {
            return mapped_rgba(mapping.as_slice(), self.stride, x, y);
        }
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

impl pagh_ui::damage::DamageSurface for Canvas {
    fn dims(&self) -> (i32, i32) {
        (self.width as i32, self.height as i32)
    }

    fn stride(&self) -> usize {
        self.stride
    }

    fn bytes_per_pixel(&self) -> usize {
        4
    }

    fn bytes(&self) -> &[u8] {
        self.surface_bytes()
    }
}

/// Pagh terminal theme: the stock Breeze-dark terminal palette.
///
/// The reference is the *look* of a KDE Konsole window (dark content, light
/// monospace text, hairline chrome) as seen in a screenshot — no Konsole code,
/// assets or color-scheme file is copied, so this crate stays MIT.  The palette
/// is the long-published Breeze dark ANSI set: muted surfaces, no neon accent.
#[derive(Clone, Debug)]
pub struct Theme {
    pub bg: Rgb,
    pub fg: Rgb,
    pub cursor: Rgb,
    pub cursor_text: Rgb,
    pub cursor_unfocused: Rgb,
    pub selection: Rgb,
    pub palette: [Rgb; 256],
    pub window_bg: Rgb,
    pub border: Rgb,
    pub title_bg: Rgb,
    pub title_fg: Rgb,
    pub scrollbar_track: Rgb,
    pub scrollbar: Rgb,
}

impl Default for Theme {
    fn default() -> Theme {
        Theme {
            bg: Rgb(0x23, 0x26, 0x27),
            fg: Rgb(0xef, 0xf0, 0xf1),
            cursor: Rgb(0xef, 0xf0, 0xf1),
            cursor_text: Rgb(0x23, 0x26, 0x27),
            cursor_unfocused: Rgb(0x7f, 0x8c, 0x8d),
            selection: Rgb(0x3d, 0xae, 0xe9),
            palette: default_palette(),
            window_bg: Rgb(0x23, 0x26, 0x27),
            border: Rgb(0x1b, 0x1e, 0x1f),
            title_bg: Rgb(0x31, 0x36, 0x3b),
            title_fg: Rgb(0xbd, 0xc3, 0xc7),
            scrollbar_track: Rgb(0x23, 0x26, 0x27),
            scrollbar: Rgb(0x7f, 0x8c, 0x8d),
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
    // Canvas::new painted exactly `theme.bg`, so the cleared path is safe and
    // skips the second background pass.
    render_screen_into_cleared(&mut canvas, opts.padding, opts.padding, screen, theme, opts);
    canvas
}

/// Render the grid at an arbitrary origin inside an existing canvas.
///
/// Every cell background is painted, so the canvas region under the grid may
/// hold anything. A frame pipeline that has just filled the canvas with
/// [`Theme::bg`] wants [`render_screen_into_cleared`] instead: it skips the
/// cells whose background is already `theme.bg` and produces the same bytes.
pub fn render_screen_into(
    canvas: &mut Canvas,
    ox: usize,
    oy: usize,
    screen: &Screen,
    theme: &Theme,
    opts: &RenderOptions,
) {
    render_grid(canvas, ox, oy, screen, theme, opts, false);
}

/// [`render_screen_into`] for a canvas whose background is already painted.
///
/// Requires every pixel of the grid rectangle to be exactly `theme.bg` — what
/// [`Canvas::fill(theme.bg)`] leaves behind, and what [`Canvas::new`] paints by
/// construction. Under that precondition a cell background equal to
/// `theme.bg` would rewrite bytes with the value already there, so the pass
/// skips it: output is pixel-identical to [`render_screen_into`], while the
/// second background fill (a 112×33 grid is 532k pixels written twice per
/// frame) disappears. Painting one contiguous `theme.bg` rectangle and then
/// only the *non-default* cells is strictly cheaper than filling every cell:
/// on a 900×600 surface the per-cell pass alone costs 0.10 ms against 0.055 ms
/// for the single canvas-wide fill, and the whole frame drops from 1.70 ms to
/// 0.58 ms (release, `lto`, host build).
pub fn render_screen_into_cleared(
    canvas: &mut Canvas,
    ox: usize,
    oy: usize,
    screen: &Screen,
    theme: &Theme,
    opts: &RenderOptions,
) {
    render_grid(canvas, ox, oy, screen, theme, opts, true);
}

fn render_grid(
    canvas: &mut Canvas,
    ox: usize,
    oy: usize,
    screen: &Screen,
    theme: &Theme,
    opts: &RenderOptions,
    bg_cleared: bool,
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
            // A default-background cell on a canvas that is already theme.bg
            // would rewrite identical bytes; see render_screen_into_cleared.
            if bg_cleared && style.bg == theme.bg {
                continue;
            }
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
            theme.cursor_unfocused
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
        canvas.outline_rect(px, py, CELL_W, CELL_H, theme.cursor_unfocused);
    }

    if opts.show_scrollbar {
        draw_scrollbar(canvas, ox, oy, screen, theme);
    }
}

/// Render the grid plus a hairline title strip.  The compositor normally draws
/// the window decorations itself (`PROTOCOL.md`), so this chrome exists only
/// for screenshots and standalone previews: one dark strip with the session
/// name, and nothing else — no buttons, no status readouts, no accent bars.
pub fn render_window(screen: &Screen, theme: &Theme, title: &str, opts: &RenderOptions) -> Canvas {
    let content = render_screen(screen, theme, opts);
    let width = content.width + BORDER * 2;
    let height = content.height + BORDER * 2 + TITLE_H;
    let mut canvas = Canvas::new(width, height, theme.window_bg);

    // Title strip, with a hairline separating it from the grid the way a
    // terminal's tab strip sits against the text area.
    canvas.fill_rect(BORDER, BORDER, width - BORDER * 2, TITLE_H, theme.title_bg);
    canvas.fill_rect(
        0,
        BORDER + TITLE_H - 1,
        width,
        1,
        if opts.focused {
            theme.border
        } else {
            theme.title_bg
        },
    );
    let title = if title.is_empty() {
        "vt0 — Pagh Terminal".to_string()
    } else {
        title.to_string()
    };
    let title_y = BORDER + (TITLE_H - CELL_H) / 2;
    draw_text(
        &mut canvas,
        BORDER + 8,
        title_y,
        &title,
        theme.title_fg,
        width.saturating_sub(2 * BORDER + 16),
    );

    // Content.
    canvas.blit(&content, BORDER, BORDER + TITLE_H);
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
    // HIDDEN and a dark blink phase paint the foreground in the background
    // colour.  Defer that decision until after the selection wash: it has to
    // track the *current* background, or selection would make hidden text show
    // up (fg keeps the pre-selection colour while bg moved).
    let blank =
        cell.attrs.contains(Attrs::HIDDEN) || (cell.attrs.contains(Attrs::BLINK) && !blink_on);
    if screen.reverse_video {
        std::mem::swap(&mut fg, &mut bg);
    }
    if selected {
        // Selection changes the background only.  Overwriting the foreground
        // with one dark colour (the old `selection_fg`) erased exactly what a
        // selection must preserve: a fastfetch logo is coloured `#` glyphs and
        // its palette strip is ANSI *backgrounds*, so a flat fill turned the
        // logo into dark blocks and the strip into one blue bar, while every
        // coloured key/value went grey.  A wash keeps the glyph colour
        // byte-identical and lets background art show through, and the mixed
        // contrast still makes the highlight obvious.
        bg = bg.lerp(theme.selection, SELECTION_BG_MIX);
    }
    if blank {
        fg = bg;
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

fn draw_scrollbar(canvas: &mut Canvas, ox: usize, oy: usize, screen: &Screen, theme: &Theme) {
    let sb = screen.scrollback_len();
    if sb == 0 {
        return;
    }
    // The scrollbar lives in the right padding: 2 px wide, flush with the
    // window edge, so it never eats a glyph cell.
    let track_h = screen.rows() * CELL_H;
    let track_x = ox + screen.cols() * CELL_W;
    let track_y = oy;
    canvas.fill_rect(track_x, track_y, 2, track_h, theme.scrollbar_track);
    let total = (sb + screen.rows()) as f32;
    let visible = screen.rows() as f32 / total;
    let thumb_h = ((track_h as f32 * visible).round() as usize)
        .max(14)
        .min(track_h);
    let start = (sb.saturating_sub(screen.view_offset())) as f32 / total;
    let thumb_y = track_y + ((track_h - thumb_h) as f32 * start).round() as usize;
    canvas.fill_rect(track_x, thumb_y, 2, thumb_h, theme.scrollbar);
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
        assert_eq!(
            &px[..3],
            &theme.bg.lerp(theme.selection, SELECTION_BG_MIX).to_array()
        );
    }

    /// Собрать цвета всех пикселей клетки `(row, col)`.
    fn cell_pixels(canvas: &Canvas, row: usize, col: usize) -> Vec<[u8; 3]> {
        let mut out = Vec::with_capacity(CELL_W * CELL_H);
        for y in 0..CELL_H {
            for x in 0..CELL_W {
                let px = canvas.pixel(
                    VIEWPORT_PADDING + col * CELL_W + x,
                    VIEWPORT_PADDING + row * CELL_H + y,
                );
                out.push([px[0], px[1], px[2]]);
            }
        }
        out
    }

    /// Репро бага: выделение fastfetch (цветной `#`-логотип + ANSI-текст)
    /// раньше перекрашивало foreground в один серый (`selection_fg`), поэтому
    /// логотип и цветные ключи пропадали. Выделение обязано менять только фон:
    /// каждый пиксель глифа сохраняет свой цвет, а фон уходит в подсветку.
    #[test]
    fn selection_preserves_glyph_colours() {
        let mut t = Terminal::new(12, 2);
        t.feed_str("\x1b[1m\x1b[33m##\x1b[0m\x1b[1m\x1b[37mPagh\x1b[0m");
        let theme = Theme::default();
        let opts = RenderOptions::default();
        let plain = render_screen(&t.screen, &theme, &opts);

        let mut selected = Terminal::new(12, 2);
        selected.feed_str("\x1b[1m\x1b[33m##\x1b[0m\x1b[1m\x1b[37mPagh\x1b[0m");
        selected
            .screen
            .begin_selection_abs(0, 0, crate::screen::SelectionMode::Char);
        selected.screen.extend_selection_abs(0, 5);
        let after = render_screen(&selected.screen, &theme, &opts);

        let highlight = theme.bg.lerp(theme.selection, SELECTION_BG_MIX);
        let yellow = theme.palette[11].lerp(Rgb::WHITE, 0.12);
        let white = theme.palette[15].lerp(Rgb::WHITE, 0.12);
        let mut glyph_pixels = 0usize;
        for col in 0..6 {
            let before = cell_pixels(&plain, 0, col);
            let now = cell_pixels(&after, 0, col);
            for (b, a) in before.iter().zip(now.iter()) {
                if *b == yellow.to_array() || *b == white.to_array() {
                    glyph_pixels += 1;
                    assert_eq!(b, a, "foreground pixel at cell {col} changed colour");
                } else if *b == theme.bg.to_array() {
                    assert_eq!(a, &highlight.to_array(), "selection background mismatch");
                }
            }
        }
        assert!(glyph_pixels > 40, "too few glyph pixels: {glyph_pixels}");
    }

    /// Full-block art (the `█`-style logo) is pure foreground in the cell: the
    /// old opaque `selection_fg` fill turned the whole cell into the theme
    /// background, i.e. the logo visibly disappeared.  Its pixels must stay
    /// byte-identical under selection.
    #[test]
    fn selection_does_not_erase_full_block_glyphs() {
        let mut t = Terminal::new(4, 1);
        t.screen.pen.fg = Color::Rgb(255, 0, 0);
        t.feed_str("█");
        t.screen
            .begin_selection_abs(0, 0, crate::screen::SelectionMode::Char);
        t.screen.extend_selection_abs(0, 0);
        let canvas = render_screen(&t.screen, &Theme::default(), &RenderOptions::default());
        for px in cell_pixels(&canvas, 0, 0) {
            assert_eq!(px, [255, 0, 0], "selected block glyph was repainted");
        }
    }

    /// ANSI-background art (`48;*`) is part of the picture too: the wash must
    /// keep it distinguishable instead of replacing it with a solid bar.
    #[test]
    fn selection_keeps_ansi_background_art() {
        let mut t = Terminal::new(4, 1);
        t.feed_str("\x1b[41m  \x1b[0m");
        let theme = Theme::default();
        let opts = RenderOptions::default();
        let plain = render_screen(&t.screen, &theme, &opts);
        t.screen
            .begin_selection_abs(0, 0, crate::screen::SelectionMode::Char);
        t.screen.extend_selection_abs(0, 1);
        let after = render_screen(&t.screen, &theme, &opts);
        let red = theme.palette[1];
        let washed = red.lerp(theme.selection, SELECTION_BG_MIX);
        for col in 0..2 {
            assert_eq!(cell_pixels(&plain, 0, col)[0], red.to_array());
            let px = cell_pixels(&after, 0, col)[0];
            assert_eq!(px, washed.to_array());
            assert_ne!(px, theme.selection.to_array(), "background art was erased");
            assert_ne!(px, red.to_array(), "selection is invisible");
        }
    }

    /// Hidden text must stay hidden: now that the foreground is preserved, the
    /// "paint fg in the background colour" decision has to follow the *washed*
    /// background, not the pre-selection one.
    #[test]
    fn selection_keeps_hidden_text_hidden() {
        let mut t = Terminal::new(8, 1);
        t.feed_str("\x1b[8msecret");
        let theme = Theme::default();
        t.screen
            .begin_selection_abs(0, 0, crate::screen::SelectionMode::Char);
        t.screen.extend_selection_abs(0, 5);
        let canvas = render_screen(&t.screen, &theme, &RenderOptions::default());
        let washed = theme.bg.lerp(theme.selection, SELECTION_BG_MIX);
        for col in 0..6 {
            for px in cell_pixels(&canvas, 0, col) {
                assert_eq!(px, washed.to_array(), "hidden glyph appeared on selection");
            }
        }
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
            VIEWPORT_PADDING * 2 + 3 * CELL_H + BORDER * 2 + TITLE_H
        );
    }
}
