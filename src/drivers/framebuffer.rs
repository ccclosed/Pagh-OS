// drivers/framebuffer.rs — Simple framebuffer text output using Limine
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use crate::sync::spinlock::Spinlock;
use core::fmt;

/// Glyph cell width in pixels. **Public on purpose**: the VT emulator, the shell's
/// mouse-to-cell mapping and the `paint` toolbar all lay out in cells, so they must
/// take the cell from here instead of re-declaring it. A second copy of these two
/// numbers is a silent layout bug the moment the font changes (`tools/gen_font.py`
/// decides the cell; the asset and these constants move together).
pub const CHAR_WIDTH: usize = 8;
/// Glyph cell height in pixels (see [`CHAR_WIDTH`]).
pub const CHAR_HEIGHT: usize = 18;
const CHARS_PER_LINE: usize = 100;
const MAX_LINES: usize = 37;

/// Height in pixels of the reserved status-bar strip at the bottom of the
/// screen. The text console never scrolls into this region (see `scroll`).
///
/// Sized to hold one glyph row with a little padding: the strip's own labels go
/// through `draw_text_px`, so a bar shorter than a glyph would clip them in half.
/// `paint`'s taskbar uses this same constant, so the two bars stay aligned.
pub const STATUS_BAR_HEIGHT: usize = CHAR_HEIGHT;

/// The console typeface: Fira Code (the host's terminal font), rasterised offline
/// by `tools/gen_font.py` into one **8-bit coverage** glyph per printable ASCII
/// codepoint.
///
/// The kernel deliberately does not parse TrueType: the asset holds 95 glyphs, each
/// `CHAR_HEIGHT` rows of `CHAR_WIDTH` coverage bytes (0 = background, 255 = full
/// ink), covering 0x20 (space) through 0x7E ('~'), so the glyph for codepoint `ch`
/// lives at index `ch - 0x20`. Coverage — not a bitmap — is what lets the console
/// antialias its glyphs ([`blend_px`]); a 1-bit asset at an 8 px advance is the
/// "rough" text this replaced. Regenerate (never hand-edit) with::
///
///     python3 tools/gen_font.py \
///         --font /usr/share/fonts/fira-code/FiraCode-Regular.ttf \
///         --cell 8 --cell-h 18 --out assets/font8x18.bin
///
/// Fira Code 6.002 (`sha256 5992ab96…f6117`) was chosen because its advance is
/// 0.6154 em: at em 13 the advance is exactly 8.00 px, so the 8 px cell fits the
/// face with zero slack and no glyph is squeezed. The generator derives the pixel
/// size from the cell width rather than trusting a number here, and `--check`
/// re-rasterises the asset and fails unless the bytes match. A wider cell renders
/// the face at the size it was drawn for, but reduces the column count and the
/// text grid (`CHARS_PER_LINE`); that trade was tried and rejected. Regenerating
/// with another size or cell is supported, but these constants, the asset and the
/// grid must change in the same commit.
static FONT: &[u8] = include_bytes!("../../assets/font8x18.bin");

/// Console colours before any theme is applied. `settings` overrides both.
pub const DEFAULT_FG: u32 = 0xFF_FFFF;
pub const DEFAULT_BG: u32 = 0x00_0000;

const FONT_FIRST_CP: u8 = 0x20; // first codepoint present in the font
const FONT_LAST_CP: u8 = 0x7E; // last codepoint present in the font
const GLYPH_BYTES: usize = CHAR_HEIGHT * CHAR_WIDTH; // coverage bytes per glyph

// Fallback glyph (hollow rectangle) for codepoints outside the font range. Built
// from the cell rather than written out, so it keeps filling exactly one cell when
// the cell changes. The first and last rows carry the top and bottom edges, the
// rows between them the sides; every border pixel is full ink.
const FALLBACK_GLYPH: [u8; GLYPH_BYTES] = {
    let mut g = [0u8; GLYPH_BYTES];
    let mut i = 0;
    while i < GLYPH_BYTES {
        let (row, col) = (i / CHAR_WIDTH, i % CHAR_WIDTH); // pixel this byte holds
        let border = row == 0 || row == CHAR_HEIGHT - 1 || col == 0 || col == CHAR_WIDTH - 1;
        g[i] = if border { 0xFF } else { 0x00 };
        i += 1;
    }
    g
};

/// Returns the coverage bitmap for `ch`, sourced from the embedded font.
/// Codepoints outside the font's `0x20..=0x7E` range fall back to a plain box.
fn get_glyph(ch: u8) -> &'static [u8] {
    if ch >= FONT_FIRST_CP && ch <= FONT_LAST_CP {
        let idx = (ch - FONT_FIRST_CP) as usize;
        let start = idx * GLYPH_BYTES;
        &FONT[start..start + GLYPH_BYTES]
    } else {
        &FALLBACK_GLYPH
    }
}

/// Blend `b` into `a` by `t/256`, channel-wise. Used for derived shades (the
/// status-bar edge, the wallpaper ramp) so a theme swap needs no extra constants.
fn mix_rgb(a: u32, b: u32, t: u32) -> u32 {
    let ch = |shift: u32| {
        let (x, y) = ((a >> shift) & 0xFF, (b >> shift) & 0xFF);
        (x * (256 - t) + y * t) >> 8
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Coverage (0 = background, 255 = full ink) of pixel `(dx, dy)` of `glyph`.
///
/// The glyph is `CHAR_HEIGHT` rows of `CHAR_WIDTH` bytes, row-major: one byte per
/// pixel, in reading order. Treating a row as any other length is the mistake that
/// stays silent — it shifts every row of every glyph after the first.
fn glyph_coverage(glyph: &[u8], dx: usize, dy: usize) -> u8 {
    glyph[dy * CHAR_WIDTH + dx]
}

/// Blend `fg` over `base` with 8-bit coverage: 0 leaves `base`, 255 is exactly
/// `fg`, and everything between walks the ramp with rounding.
///
/// This is the whole reason the asset stores coverage instead of bits: the console
/// gets the antialiased edges the same face has in a terminal on the host. The
/// rounding matters beyond cosmetics — `tools/font_check.py` rebuilds the expected
/// screen from the asset with the same formula and fails on a single differing
/// pixel, so an off-by-one ramp means the console is not provably showing the font
/// that is committed.
fn blend_px(base: u32, fg: u32, cov: u8) -> u32 {
    match cov {
        0 => base,
        255 => fg,
        _ => {
            let c = cov as u32;
            let ch = |shift: u32| {
                let (a, b) = ((base >> shift) & 0xFF, (fg >> shift) & 0xFF);
                // Coverage is 0..=255, i.e. the blend weight is cov/255 (not /256):
                // the `+127` rounds, so coverage 128 lands halfway.
                (a * (255 - c) + b * c + 127) / 255
            };
            (ch(16) << 16) | (ch(8) << 8) | ch(0)
        }
    }
}

pub struct FramebufferWriter {
    fb_addr: u64,
    width: usize,
    height: usize,
    pitch: usize,
    bpp: usize,
    col: usize,
    row: usize,
    fg_color: u32,
    /// Background colour: the unlit pixels of every cell, the `clear` fill, and the
    /// fallback for a cell painted without an explicit background. The `settings`
    /// panel changes it for the theme; the text blitter only ever reads it.
    bg_color: u32,
    /// Status-bar strip colours, themed by `settings` (see `shell::theme`).
    status_bg: u32,
    status_fg: u32,
    /// Whether a printed character also paints its cell background.
    ///
    /// `true` (the default, and the only behaviour there was before wallpapers)
    /// keeps the classic destructive console. `false` leaves the pixels behind a
    /// glyph alone, which is what lets a wallpaper show *through* the text: with it
    /// on, every printed cell stamps `bg_color` over the gradient underneath and the
    /// wallpaper survives only outside the 100x37 text grid.
    opaque_bg: bool,
    /// A retained copy of what is currently on the text grid, `MAX_LINES` rows of
    /// `CHARS_PER_LINE` cells. The pixels are still the display; this is the model
    /// the shell needs to place things *at* a cell (the blinking caret) and to
    /// answer "what character is here" (a selection copied out of the screen).
    /// Kept in sync by `write_char`, `scroll` and `clear`.
    grid: alloc::vec::Vec<u8>,
    /// The foreground color each grid cell was last drawn in, so a cell repainted
    /// for a selection can be restored *to its own color* rather than to the
    /// console default. Without this, selecting the green prompt and releasing
    /// would leave it white: the pixels carry no record of how they were drawn.
    grid_fg: alloc::vec::Vec<u32>,
    /// Shadow copy of the screen in ordinary RAM.
    ///
    /// All drawing goes here first, because the framebuffer itself is MMIO: every
    /// `write_volatile` to it is uncached and crosses to the device, so a full
    /// 1280x800 frame cost ~54 000 such writes and took tens of milliseconds. Text
    /// output felt slow and frames were caught half-drawn for that reason alone.
    /// Writing to RAM costs a few nanoseconds per pixel, and [`Self::flush`] then
    /// moves only the rectangle that actually changed up to the device in one
    /// linear copy.
    back: alloc::vec::Vec<u8>,
    /// Bounding box of cells changed since the last flush, as `(x0, y0, x1, y1)`
    /// inclusive. `None` when nothing has been drawn.
    dirty: Option<(usize, usize, usize, usize)>,
    /// When set, `put_pixel` does not mark the frame dirty.
    ///
    /// The software cursor is an overlay: it moves on its own and is pushed to
    /// the device with [`Self::flush_rect`], so drawing it must not make the whole
    /// 1280x800 surface "changed". Without this flag every mouse move would force
    /// a full-surface copy on the next frame flush — an idle shell would spend the
    /// CPU of a busy one just to redraw a 12x19 arrow nobody asked to move.
    quiet: bool,
}

impl FramebufferWriter {
    pub fn new() -> Option<Self> {
        crate::debug!("Attempting to initialize framebuffer...");

        let fb_response = crate::FRAMEBUFFER_REQUEST.response();
        if fb_response.is_none() {
            crate::error!("[FB] No framebuffer response from Limine");
            return None;
        }

        let fb_response = fb_response.unwrap();
        let fbs = fb_response.framebuffers();
        crate::debug!("Framebuffers available: {}", fbs.len());

        let fb = fbs.get(0);
        if fb.is_none() {
            crate::error!("[FB] No framebuffer at index 0");
            return None;
        }

        let fb = fb.unwrap();

        // Access public fields and address() method
        let address = fb.address() as u64;
        let width = fb.width;
        let height = fb.height;
        let pitch = fb.pitch;
        let bpp = fb.bpp;

        crate::debug!(
            "addr=0x{:x} w={} h={} pitch={} bpp={}",
            address,
            width,
            height,
            pitch,
            bpp
        );

        Some(FramebufferWriter {
            fb_addr: address,
            width: width as usize,
            height: height as usize,
            pitch: pitch as usize,
            bpp: (bpp / 8) as usize,
            col: 0,
            row: 0,
            fg_color: DEFAULT_FG,
            bg_color: DEFAULT_BG,
            status_bg: 0x1B_3A5B, // slate blue
            status_fg: 0xE6_F0FF,
            opaque_bg: true,
            // Bounded by construction: the console never holds more than
            // `MAX_LINES` text rows, and `text_rows()` is clamped to it, so the
            // model cannot grow with output volume.
            grid: alloc::vec![b' '; MAX_LINES * CHARS_PER_LINE],
            grid_fg: alloc::vec![0xFFFFFF; MAX_LINES * CHARS_PER_LINE],
            // Zeroed, matching what the device shows after `clear`. Sized by PITCH,
            // not by width: every offset in this file is `y * pitch + x * bpp`, so a
            // buffer of `width * bpp` per row would be read at the wrong stride
            // whenever the device pads its rows (which it does), and the flush would
            // copy garbage.
            back: alloc::vec![0u8; (pitch as usize) * (height as usize)],
            dirty: None,
            quiet: false,
        })
    }

    pub fn clear(&mut self) {
        self.col = 0;
        self.row = 0;
        for cell in self.grid.iter_mut() {
            *cell = b' ';
        }
        for fg in self.grid_fg.iter_mut() {
            *fg = self.fg_color;
        }
        let (b, g, r) = (
            (self.bg_color & 0xFF) as u8,
            ((self.bg_color >> 8) & 0xFF) as u8,
            ((self.bg_color >> 16) & 0xFF) as u8,
        );
        for row in 0..self.height {
            let base = row * self.pitch;
            for px in 0..self.width {
                let off = base + px * self.bpp;
                self.back[off] = b;
                self.back[off + 1] = g;
                self.back[off + 2] = r;
            }
        }
        self.dirty = Some((
            0,
            0,
            self.width.saturating_sub(1),
            self.height.saturating_sub(1),
        ));
    }

    /// Record `ch` at grid cell `(col, row)`; out-of-range writes are dropped.
    fn set_cell(&mut self, col: usize, row: usize, ch: u8) {
        if col < CHARS_PER_LINE && row < MAX_LINES {
            let idx = row * CHARS_PER_LINE + col;
            self.grid[idx] = ch;
            self.grid_fg[idx] = self.fg_color;
        }
    }

    /// The character recorded at grid cell `(col, row)`, or a space outside the
    /// grid. Used to restore the glyph a caret was drawn over.
    fn cell_char(&self, col: usize, row: usize) -> u8 {
        if col < CHARS_PER_LINE && row < MAX_LINES {
            self.grid[row * CHARS_PER_LINE + col]
        } else {
            b' '
        }
    }

    /// The foreground color cell `(col, row)` was drawn in.
    fn cell_fg(&self, col: usize, row: usize) -> u32 {
        if col < CHARS_PER_LINE && row < MAX_LINES {
            self.grid_fg[row * CHARS_PER_LINE + col]
        } else {
            0xFFFFFF
        }
    }

    /// Copy `[from_col, to_col)` of `row` into `out` as a `String`.
    ///
    /// This is how text is copied off the screen: the model is the only record of
    /// what is displayed, and reading it back avoids re-deriving text from glyphs.
    /// Out-of-range coordinates are clipped rather than refused, so a caller
    /// copying a selection that runs off the end simply gets the part that exists.
    fn row_text(
        &self,
        row: usize,
        from_col: usize,
        to_col: usize,
        out: &mut alloc::string::String,
    ) {
        let from = from_col.min(CHARS_PER_LINE);
        let to = to_col.min(CHARS_PER_LINE);
        for col in from..to {
            let ch = self.grid[row.min(MAX_LINES - 1) * CHARS_PER_LINE + col];
            // The grid holds console bytes (printable ASCII, see `write_char`).
            out.push(ch as char);
        }
    }

    pub fn write_char(&mut self, ch: u8) {
        let max_rows = self.text_rows();
        match ch {
            b'\n' => {
                self.col = 0;
                self.row += 1;
                if self.row >= max_rows {
                    self.scroll();
                }
            }
            b'\r' => {
                self.col = 0;
            }
            0x08 => {
                // Backspace
                if self.col > 0 {
                    self.col -= 1;
                    self.draw_char(b' ', self.col, self.row);
                    self.set_cell(self.col, self.row, b' ');
                }
            }
            32..=126 => {
                if self.col >= CHARS_PER_LINE {
                    self.col = 0;
                    self.row += 1;
                    if self.row >= max_rows {
                        self.scroll();
                    }
                }
                self.draw_char(ch, self.col, self.row);
                self.set_cell(self.col, self.row, ch);
                self.col += 1;
            }
            _ => {}
        }
    }

    /// Number of text rows the console may use, leaving room for the bottom
    /// status bar. Derived from the actual screen height so low-resolution
    /// modes (e.g. 800×600, where 37 full rows would collide with the status
    /// bar) reserve a row for it rather than overdrawing or clipping the bar.
    fn text_rows(&self) -> usize {
        let avail = self.height.saturating_sub(STATUS_BAR_HEIGHT) / CHAR_HEIGHT;
        avail.min(MAX_LINES).max(1)
    }

    fn draw_char(&mut self, ch: u8, col: usize, row: usize) {
        let x = col * CHAR_WIDTH;
        let y = row * CHAR_HEIGHT;

        let glyph = get_glyph(ch);

        for dy in 0..CHAR_HEIGHT {
            if y + dy >= self.height {
                break;
            }
            for dx in 0..CHAR_WIDTH {
                if x + dx >= self.width {
                    break;
                }
                let cov = glyph_coverage(glyph, dx, dy);
                if cov == 0 && !self.opaque_bg {
                    // Transparent console (wallpaper active): an empty pixel must
                    // leave the picture behind the glyph alone.
                    continue;
                }
                // Opaque console: blend against the cell background. Transparent:
                // blend against whatever is already in the frame, so a glyph sits on
                // the wallpaper instead of stamping a rectangle over it.
                let base = if self.opaque_bg {
                    self.bg_color
                } else {
                    self.get_pixel(x + dx, y + dy)
                };
                self.put_pixel(x + dx, y + dy, blend_px(base, self.fg_color, cov));
            }
        }
    }

    fn put_pixel(&mut self, x: usize, y: usize, color: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = y * self.pitch + x * self.bpp;
        if self.bpp >= 3 {
            self.back[offset] = (color & 0xFF) as u8;
            self.back[offset + 1] = ((color >> 8) & 0xFF) as u8;
            self.back[offset + 2] = ((color >> 16) & 0xFF) as u8;
            if !self.quiet {
                self.mark_dirty(x, y);
            }
        }
    }

    /// Extend the pending-flush rectangle to include `(x, y)`.
    fn mark_dirty(&mut self, x: usize, y: usize) {
        self.dirty = Some(match self.dirty {
            None => (x, y, x, y),
            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
        });
    }

    /// Push everything drawn since the last flush to the framebuffer.
    ///
    /// Copies only the tracked bounding box rather than the whole surface: a cursor
    /// blink touches 2x18 pixels and a typed character one text row, and copying
    /// 1280x800 pixels for those is what made the editor feel slow under TCG.
    /// Returns whether anything was flushed.
    ///
    /// The box is trustworthy because **every** write to `back` goes through
    /// [`Self::put_pixel`] (which widens it) and the two bulk moves widen it
    /// explicitly: [`Self::scroll`] marks the whole text area, [`Self::clear`] the
    /// whole surface. Overlays that must *not* count as frame damage (the software
    /// cursor) draw with [`Self::quiet`] set and push themselves via
    /// [`Self::flush_rect`]. A bug that bypasses `put_pixel` would show as a stale
    /// rectangle, so keep new drawing primitives going through it.
    pub fn flush(&mut self) -> bool {
        let Some((x0, y0, x1, y1)) = self.dirty.take() else {
            return false;
        };
        if self.bpp < 3 || self.width == 0 || self.height == 0 {
            return false;
        }
        // `mark_dirty` only ever records in-bounds pixels, but the box is clipped
        // here anyway: a future caller that sets `dirty` by hand must not be able to
        // index outside the buffer.
        let x1 = x1.min(self.width - 1);
        let y1 = y1.min(self.height - 1);
        if x0 >= self.width || y0 >= self.height || x0 > x1 || y0 > y1 {
            return false;
        }
        self.flush_rect(x0, y0, x1 - x0 + 1, y1 - y0 + 1);
        true
    }

    /// Set whether pixel writes are "quiet" (see [`Self::quiet`]).
    ///
    /// Used by the software cursor around its save/restore/draw sequence: those
    /// pixels are pushed by [`Self::flush_rect`] and must not schedule a
    /// full-surface flush.
    pub fn set_quiet(&mut self, quiet: bool) {
        self.quiet = quiet;
    }

    /// Push just one rectangle of the shadow buffer to the device.
    ///
    /// A side channel for overlays that have their own flush: unlike [`Self::flush`]
    /// it does not consume the dirty box, so a caller that just wants these pixels
    /// on the device (the software cursor) does not force a frame flush later.
    /// Rectangles are clipped to the surface; an empty intersection is a no-op.
    pub fn flush_rect(&mut self, x: usize, y: usize, w: usize, h: usize) {
        if self.bpp < 3 || w == 0 || h == 0 || x >= self.width || y >= self.height {
            return;
        }
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        let row_bytes = (x1 - x) * self.bpp;
        let src_base = y * self.pitch + x * self.bpp;
        // SAFETY: `x1 <= width` and `y1 <= height`, so every offset stays inside
        // the `pitch * height` shadow buffer and inside the device surface of the
        // same geometry; the spans do not overlap (kernel RAM vs MMIO).
        unsafe {
            let fb = self.fb_addr as *mut u8;
            for row in 0..(y1 - y) {
                let offset = src_base + row * self.pitch;
                core::ptr::copy_nonoverlapping(
                    self.back.as_ptr().add(offset),
                    fb.add(offset),
                    row_bytes,
                );
            }
        }
    }

    fn scroll(&mut self) {
        // Scroll only the text region (the usable rows), leaving the reserved
        // status-bar strip at the bottom of the screen untouched.
        let rows = self.text_rows();
        {
            let line_bytes = CHAR_HEIGHT * self.pitch;
            let text_px = rows * CHAR_HEIGHT;
            let move_bytes = (text_px - CHAR_HEIGHT) * self.pitch;
            // Shift the shadow copy; the device catches up on the next `flush`, which
            // is why the whole text area is marked dirty rather than a rectangle.
            self.back
                .copy_within(line_bytes..line_bytes + move_bytes, 0);
            let last_line_offset = (text_px - CHAR_HEIGHT) * self.pitch;
            for b in &mut self.back[last_line_offset..last_line_offset + line_bytes] {
                *b = 0;
            }
            self.dirty = Some((
                0,
                0,
                self.width.saturating_sub(1),
                text_px.saturating_sub(1),
            ));
        }
        // The model must move with the pixels: a caret or a selection computed
        // from a stale grid would point at the wrong row for the rest of the
        // session. Shift up by one row over the same `rows` the pixels moved.
        for row in 0..rows.saturating_sub(1) {
            let src = (row + 1) * CHARS_PER_LINE;
            let dst = row * CHARS_PER_LINE;
            for col in 0..CHARS_PER_LINE {
                self.grid[dst + col] = self.grid[src + col];
                self.grid_fg[dst + col] = self.grid_fg[src + col];
            }
        }
        let last = (rows - 1) * CHARS_PER_LINE;
        for col in 0..CHARS_PER_LINE {
            self.grid[last + col] = b' ';
            self.grid_fg[last + col] = self.fg_color;
        }
        self.row = rows - 1;
    }

    pub fn write_string(&mut self, s: &str) {
        for byte in s.bytes() {
            self.write_char(byte);
        }
    }

    /// Sets the foreground color used for lit glyph pixels in `write_char`.
    /// The background/clear color is unaffected. Defaults to `DEFAULT_FG`.
    pub fn set_fg_color(&mut self, color: u32) {
        self.fg_color = color;
    }

    /// Sets the background colour: unlit glyph pixels, the `clear` fill, and the
    /// background of a cell painted without one. Defaults to `DEFAULT_BG`.
    pub fn set_bg_color(&mut self, color: u32) {
        self.bg_color = color;
    }

    /// See [`FramebufferWriter::opaque_bg`]. `false` makes the console paint glyphs
    /// only, so whatever is behind them — a wallpaper — stays visible.
    pub fn set_opaque_bg(&mut self, opaque: bool) {
        self.opaque_bg = opaque;
    }

    /// Sets the status-bar strip colours.
    pub fn set_status_colors(&mut self, bg: u32, fg: u32) {
        self.status_bg = bg;
        self.status_fg = fg;
    }

    /// Redraw every cell of the model into the shadow buffer.
    ///
    /// `clear` wipes the pixels, so anything that repaints the background (a
    /// wallpaper, a theme switch) has to put the text back. This walks the model
    /// rather than a screen scrape, so what reappears is exactly what was there.
    pub fn repaint_grid(&mut self) {
        let (cols, rows) = self.text_grid();
        for row in 0..rows {
            for col in 0..cols {
                let ch = self.cell_char(col, row);
                let fg = self.cell_fg(col, row);
                // Not `explicit_bg`: a transparent console must leave the wallpaper
                // behind an empty cell alone, otherwise every repaint stamps the
                // console background back over the picture.
                self.paint_cell_with_bg(col, row, ch, fg, self.bg_color, false);
            }
        }
    }

    // ─── Text-cursor support (the blinking caret) ───

    /// Current text cursor as `(col, row)`, in character cells.
    ///
    /// This is where the *next* character would land, which is what makes a
    /// mid-line caret computable at all: the shell knows how many characters it
    /// has printed after the caret, so it can walk back from here.
    pub fn cursor_cell(&self) -> (usize, usize) {
        (self.col, self.row)
    }

    /// Number of character columns on one text row, and the number of text rows
    /// the console may use (the status bar is excluded).
    pub fn text_grid(&self) -> (usize, usize) {
        (CHARS_PER_LINE, self.text_rows())
    }

    /// Repaint cell `(col, row)` with `ch`, **without moving the text cursor**.
    ///
    /// `write_char` cannot be used to touch a cell that is not the current one:
    /// it advances `col`/`row`, so using it to erase a caret would walk the
    /// console forward and eventually scroll. This paints in place, so a caller
    /// can erase a caret and draw a new one anywhere on the grid while the
    /// console's own position stays exactly where it was.
    ///
    /// `color` is the glyph color for this cell only; the writer's `fg_color` is
    /// left alone, so a caret's color cannot leak into subsequent output.
    pub fn paint_cell(&mut self, col: usize, row: usize, ch: u8, color: u32) {
        let bg = self.bg_color;
        self.paint_cell_with_bg(col, row, ch, color, bg, true);
    }

    /// As [`paint_cell`](Self::paint_cell), with an explicit cell background.
    ///
    /// The background is what makes a text selection readable: inverting the cell
    /// is visible where a foreground change would be lost against text of the same
    /// color.
    pub fn paint_cell_with_bg(
        &mut self,
        col: usize,
        row: usize,
        ch: u8,
        color: u32,
        bg: u32,
        explicit_bg: bool,
    ) {
        let (max_cols, max_rows) = self.text_grid();
        if col >= max_cols || row >= max_rows {
            return;
        }
        // Draw the glyph's own pixels in `color`, everything else in `bg`, so
        // repainting a cell fully replaces what was there.
        let x = col * CHAR_WIDTH;
        let y = row * CHAR_HEIGHT;
        let glyph = get_glyph(ch);
        let paint_bg = self.opaque_bg || explicit_bg;
        for dy in 0..CHAR_HEIGHT {
            if y + dy >= self.height {
                break;
            }
            for dx in 0..CHAR_WIDTH {
                if x + dx >= self.width {
                    break;
                }
                let cov = glyph_coverage(glyph, dx, dy);
                if cov == 0 && !paint_bg {
                    continue;
                }
                let base = if paint_bg {
                    bg
                } else {
                    self.get_pixel(x + dx, y + dy)
                };
                self.put_pixel(x + dx, y + dy, blend_px(base, color, cov));
            }
        }
    }

    /// Paint a vertical bar in the **left** pixel column of cell `(col, row)`,
    /// leaving the cell's own glyph pixels in their columns alone.
    ///
    /// A bar rather than a block on purpose: a block would hide the character it
    /// sits on, and the caret's job here is to show where text will go, not to
    /// replace it. `top_skip`/`height` bound the bar inside the 16-pixel cell so
    /// the caller can make it shorter than the letters.
    pub fn paint_cell_bar(&mut self, col: usize, row: usize, color: u32, height: usize) {
        let (max_cols, max_rows) = self.text_grid();
        if col >= max_cols || row >= max_rows {
            return;
        }
        let x = col * CHAR_WIDTH;
        let y = row * CHAR_HEIGHT;
        let h = height.min(CHAR_HEIGHT);
        for dy in 0..h {
            if y + dy >= self.height {
                break;
            }
            self.put_pixel(x, y + dy, color);
        }
    }

    // ─── Graphics primitives (used by `paint`, the cursor, the status bar) ───
    //
    // These give callers raw pixel-level access to the framebuffer behind the
    // same lock as text output. All are bounds-checked through `put_pixel`, so
    // out-of-range coordinates are silently clipped (never panic).

    /// Framebuffer dimensions in pixels, `(width, height)`.
    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Write a single pixel (bounds-checked, clipped).
    pub fn set_pixel(&mut self, x: usize, y: usize, color: u32) {
        self.put_pixel(x, y, color);
    }

    /// Read a single pixel back from the framebuffer. Returns `0` for
    /// out-of-range coordinates. Used for the software cursor's
    /// save/restore and for the flood-fill seed sample.
    pub fn get_pixel(&self, x: usize, y: usize) -> u32 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        let offset = y * self.pitch + x * self.bpp;
        // Read the shadow copy, not the device: the device is only current up to the
        // last `flush`, and a caller that saved pixels and restored them must see the
        // same bytes it drew.
        if self.bpp >= 3 {
            let b = self.back[offset] as u32;
            let g = self.back[offset + 1] as u32;
            let r = self.back[offset + 2] as u32;
            (r << 16) | (g << 8) | b
        } else {
            0
        }
    }
    /// Fill an axis-aligned rectangle with a solid color.
    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        let mut py = y;
        while py < y1 {
            let mut px = x;
            while px < x1 {
                self.put_pixel(px, py, color);
                px += 1;
            }
            py += 1;
        }
    }

    /// Draw the one-pixel outline of an axis-aligned rectangle.
    pub fn draw_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        if w == 0 || h == 0 {
            return;
        }
        for px in x..(x + w) {
            self.put_pixel(px, y, color);
            self.put_pixel(px, y + h - 1, color);
        }
        for py in y..(y + h) {
            self.put_pixel(x, py, color);
            self.put_pixel(x + w - 1, py, color);
        }
    }

    /// Bresenham line between two points (signed coordinates, clipped).
    pub fn draw_line(&mut self, x0: isize, y0: isize, x1: isize, y1: isize, color: u32) {
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let mut x = x0;
        let mut y = y0;
        loop {
            if x >= 0 && y >= 0 {
                self.put_pixel(x as usize, y as usize, color);
            }
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
    }

    /// A line with adjustable thickness (square brush of `thick` pixels),
    /// used by the `paint` pencil/line tools.
    pub fn draw_thick_line(
        &mut self,
        x0: isize,
        y0: isize,
        x1: isize,
        y1: isize,
        thick: isize,
        color: u32,
    ) {
        if thick <= 1 {
            self.draw_line(x0, y0, x1, y1, color);
            return;
        }
        let r = thick / 2;
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let mut x = x0;
        let mut y = y0;
        loop {
            self.fill_rect(
                (x - r).max(0) as usize,
                (y - r).max(0) as usize,
                thick as usize,
                thick as usize,
                color,
            );
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
    }

    /// Midpoint-circle outline of radius `r` centered at `(cx, cy)`.
    pub fn draw_circle(&mut self, cx: isize, cy: isize, r: isize, color: u32) {
        if r <= 0 {
            return;
        }
        let mut x = r;
        let mut y = 0isize;
        let mut err = 1 - r;
        let plot = |px: isize, py: isize, this: &mut Self| {
            if px >= 0 && py >= 0 {
                this.put_pixel(px as usize, py as usize, color);
            }
        };
        while x >= y {
            plot(cx + x, cy + y, self);
            plot(cx + y, cy + x, self);
            plot(cx - y, cy + x, self);
            plot(cx - x, cy + y, self);
            plot(cx - x, cy - y, self);
            plot(cx - y, cy - x, self);
            plot(cx + y, cy - x, self);
            plot(cx + x, cy - y, self);
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
    }

    /// Solid (filled) disc of radius `r` centered at `(cx, cy)`.
    pub fn fill_circle(&mut self, cx: isize, cy: isize, r: isize, color: u32) {
        if r <= 0 {
            return;
        }
        let r2 = r * r;
        let mut dy = -r;
        while dy <= r {
            let mut dx = -r;
            while dx <= r {
                if dx * dx + dy * dy <= r2 {
                    let px = cx + dx;
                    let py = cy + dy;
                    if px >= 0 && py >= 0 {
                        self.put_pixel(px as usize, py as usize, color);
                    }
                }
                dx += 1;
            }
            dy += 1;
        }
    }

    /// Copy a `w×h` block of pixels from `src` (row stride `src_stride`) to the
    /// framebuffer at `(dx, dy)`. Used by `paint` to restore canvas regions.
    pub fn blit(
        &mut self,
        dx: usize,
        dy: usize,
        w: usize,
        h: usize,
        src: &[u32],
        src_stride: usize,
    ) {
        for row in 0..h {
            let py = dy + row;
            if py >= self.height {
                break;
            }
            let base = row * src_stride;
            for col in 0..w {
                if base + col >= src.len() {
                    break;
                }
                self.put_pixel(dx + col, py, src[base + col]);
            }
        }
    }

    /// Draw one glyph at an arbitrary pixel position with explicit fg/bg
    /// (bg is painted for the unlit pixels). Used by the status bar and the
    /// `paint` toolbar, which need pixel-precise text outside the text grid.
    pub fn draw_glyph_px(&mut self, ch: u8, px: usize, py: usize, fg: u32, bg: u32) {
        let glyph = get_glyph(ch);
        for dy in 0..CHAR_HEIGHT {
            for dx in 0..CHAR_WIDTH {
                let cov = glyph_coverage(glyph, dx, dy);
                self.put_pixel(px + dx, py + dy, blend_px(bg, fg, cov));
            }
        }
    }

    /// Draw an ASCII string at a pixel position (left to right). Non-printable
    /// bytes render as the font's fallback box.
    pub fn draw_text_px(&mut self, px: usize, py: usize, s: &str, fg: u32, bg: u32) {
        let mut x = px;
        for b in s.bytes() {
            self.draw_glyph_px(b, x, py, fg, bg);
            x += CHAR_WIDTH;
        }
    }

    /// Pixel `y` of the top of the reserved status-bar strip at the bottom of
    /// the screen. Text output never scrolls into this region (`text_rows`
    /// reserves it).
    fn status_bar_top(&self) -> usize {
        self.height.saturating_sub(STATUS_BAR_HEIGHT)
    }

    /// Repaint the bottom status bar: a colored strip with left- and
    /// right-aligned text.
    pub fn draw_status_bar(&mut self, left: &str, right: &str) {
        let top = self.status_bar_top();
        if top + STATUS_BAR_HEIGHT > self.height {
            return;
        }
        let (bg, fg) = (self.status_bg, self.status_fg);
        self.fill_rect(0, top, self.width, STATUS_BAR_HEIGHT, bg);
        // separator highlight line on top of the bar, one shade lighter
        let edge = mix_rgb(bg, fg, 60);
        for px in 0..self.width {
            self.put_pixel(px, top, edge);
        }
        let ty = top + (STATUS_BAR_HEIGHT - CHAR_HEIGHT) / 2;
        self.draw_text_px(6, ty, left, fg, bg);
        let rw = right.len() * CHAR_WIDTH;
        if rw + 6 < self.width {
            self.draw_text_px(self.width - rw - 6, ty, right, fg, bg);
        }
    }
}

impl fmt::Write for FramebufferWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_string(s);
        Ok(())
    }
}

pub struct FbWriter {
    inner: Spinlock<Option<FramebufferWriter>>,
}

impl FbWriter {
    const fn new() -> Self {
        FbWriter {
            inner: Spinlock::new(None),
        }
    }

    pub fn init(&self) {
        if self.inner.lock().is_some() {
            return;
        }
        // Build the writer OUTSIDE the lock: `FramebufferWriter::new()` logs via
        // the facade, which fans out to this same console and re-locks
        // `self.inner`. Holding the lock across new()/logging would deadlock the
        // non-reentrant spinlock.
        let writer = FramebufferWriter::new();
        let succeeded = writer.is_some();
        {
            let mut guard = self.inner.lock();
            if guard.is_some() {
                return;
            }
            *guard = writer;
        }
        if succeeded {
            crate::info!("Framebuffer writer initialized successfully");
        } else {
            crate::warn!("Framebuffer writer failed to initialize");
        }
    }

    pub fn write_fmt(&self, args: fmt::Arguments) {
        use core::fmt::Write;
        if let Some(ref mut writer) = *self.inner.lock() {
            let _ = writer.write_fmt(args);
        }
    }

    /// Write formatted arguments and push the result to the device.
    ///
    /// This is the `fb_print!` path. Without the flush the text only lands in the
    /// shadow buffer and the screen stays black until something else happens to
    /// flush: that is exactly how the first-boot `[provision] ... [Y/n]` prompt
    /// came up on an empty screen and only appeared once the answer was typed.
    pub fn write_fmt_now(&self, args: fmt::Arguments) {
        self.write_fmt(args);
        self.flush();
    }

    pub fn clear(&self) {
        if let Some(ref mut writer) = *self.inner.lock() {
            writer.clear();
        }
    }

    /// Sets the framebuffer foreground (glyph) color, locking the inner writer.
    /// No-op if the framebuffer never initialized.
    pub fn set_fg_color(&self, color: u32) {
        if let Some(ref mut writer) = *self.inner.lock() {
            writer.set_fg_color(color);
        }
    }

    /// Sets the background colour (see [`FramebufferWriter::set_bg_color`]).
    pub fn set_bg_color(&self, color: u32) {
        if let Some(ref mut writer) = *self.inner.lock() {
            writer.set_bg_color(color);
        }
    }

    /// Makes the console paint glyphs only (see [`FramebufferWriter::opaque_bg`]).
    pub fn set_opaque_bg(&self, opaque: bool) {
        if let Some(ref mut writer) = *self.inner.lock() {
            writer.set_opaque_bg(opaque);
        }
    }

    /// Sets the status-bar strip colours.
    pub fn set_status_colors(&self, bg: u32, fg: u32) {
        if let Some(ref mut writer) = *self.inner.lock() {
            writer.set_status_colors(bg, fg);
        }
    }

    /// Redraw the retained grid into the shadow buffer (see
    /// [`FramebufferWriter::repaint_grid`]). The device catches up on `flush`.
    pub fn repaint_grid(&self) {
        if let Some(ref mut writer) = *self.inner.lock() {
            writer.repaint_grid();
        }
    }

    /// Run `f` with exclusive access to the raw [`FramebufferWriter`], returning
    /// its result (or `None` if the framebuffer never initialized).
    ///
    /// This is the batch graphics entry point used by `paint` and the software
    /// cursor: locking once per logical operation avoids re-locking per pixel.
    /// The lock disables interrupts while held, so the closure must not block
    /// or log (logging re-locks this same console and would deadlock).
    pub fn with<R>(&self, f: impl FnOnce(&mut FramebufferWriter) -> R) -> Option<R> {
        self.inner.lock().as_mut().map(f)
    }

    /// Framebuffer dimensions, or `(0, 0)` if uninitialized.
    pub fn dimensions(&self) -> (usize, usize) {
        self.inner
            .lock()
            .as_ref()
            .map(|w| w.dimensions())
            .unwrap_or((0, 0))
    }

    /// Repaint the bottom status bar via the locked writer.
    pub fn draw_status_bar(&self, left: &str, right: &str) {
        if let Some(ref mut writer) = *self.inner.lock() {
            writer.draw_status_bar(left, right);
        }
    }

    /// Copy one row of the text grid into `out`, as `[from_col, to_col)`.
    ///
    /// The console is the only record of what is on screen, so copying text off it
    /// goes through here rather than re-deriving characters from pixels.
    pub fn read_row(&self, row: usize, from_col: usize, to_col: usize) -> alloc::string::String {
        let mut out = alloc::string::String::new();
        if let Some(writer) = self.inner.lock().as_ref() {
            writer.row_text(row, from_col, to_col, &mut out);
        }
        out
    }

    /// Paint cell `(col, row)` as selected: `bg` fills the cell, the glyph is
    /// drawn in `fg` on top.
    ///
    /// A selection has to be visible *over* ordinary text, which a foreground-only
    /// change cannot do — so this is the one place that paints a cell background.
    /// It restores the glyph from the model, so clearing a selection is the same
    /// call with the console's own colors.
    pub fn paint_cell_selected(&self, col: usize, row: usize, fg: u32, bg: u32, restore: bool) {
        if let Some(ref mut writer) = *self.inner.lock() {
            let (max_cols, max_rows) = writer.text_grid();
            if col >= max_cols || row >= max_rows {
                return;
            }
            let ch = writer.cell_char(col, row);
            // Restoring uses the color the cell was drawn in, not a default: the
            // pixels do not remember, but the model does.
            let color = if restore {
                writer.cell_fg(col, row)
            } else {
                fg
            };
            // A selection overlay really does want the cell filled.
            writer.paint_cell_with_bg(col, row, ch, color, bg, true);
        }
    }

    /// Push the changed rectangle of the shadow buffer to the framebuffer.
    ///
    /// Drawing goes to RAM (see `FramebufferWriter::back`); nothing reaches the
    /// device until this is called. Callers that expect the screen to be current —
    /// the shell and the editor, at the end of every repaint — call it once per
    /// frame, which is what turns tens of thousands of uncached pixel writes into a
    /// few linear row copies.
    pub fn flush(&self) {
        if let Some(ref mut writer) = *self.inner.lock() {
            writer.flush();
        }
    }

    /// The console's current text cursor cell as `(col, row)`, or `None` when the
    /// framebuffer is uninitialized (serial-only boot).
    pub fn cursor_cell(&self) -> Option<(usize, usize)> {
        self.inner.lock().as_ref().map(|w| w.cursor_cell())
    }

    /// The text grid as `(cols, rows)`, or `(0, 0)` when uninitialized.
    pub fn text_grid(&self) -> (usize, usize) {
        self.inner
            .lock()
            .as_ref()
            .map(|w| w.text_grid())
            .unwrap_or((0, 0))
    }

    /// Draw a blinking caret as a vertical bar in cell `(col, row)`.
    ///
    /// `visible = false` erases a caret previously drawn at that cell by
    /// repainting the glyph underneath it, so the caller keeps the character it
    /// is sitting on. Both directions go through one method because they must be
    /// exact inverses: a caret erased at a different cell than it was drawn at
    /// leaves a stray bar on the screen.
    pub fn paint_caret(&self, col: usize, row: usize, visible: bool, color: u32, height: usize) {
        if let Some(ref mut writer) = *self.inner.lock() {
            if visible {
                writer.paint_cell_bar(col, row, color, height);
            } else {
                // Repaint the cell's glyph in the console's own foreground color
                // so the letter the caret was covering comes back.
                let fg = writer.fg_color;
                let ch = writer.cell_char(col, row);
                writer.paint_cell(col, row, ch, fg);
            }
        }
    }
}

impl crate::drivers::Console for FbWriter {
    fn write_str(&self, s: &str) {
        // Keep the software mouse cursor coherent -- hide it
        // around the glyph write so scrolling/overdraw never captures or
        // restores a stale background (see drivers::cursor::text_begin).
        crate::drivers::cursor::text_begin();
        // Lock the inner writer and route the string through the existing
        // glyph-rendering path. No-op if the framebuffer never initialized.
        if let Some(ref mut writer) = *self.inner.lock() {
            writer.write_string(s);
            // A line end is the natural frame boundary of a scrolling console
            // (the logging facade writes tag + message + "\n" as separate
            // chunks). Without this push the boot log lives only in RAM and the
            // device stays black until some other component flushes.
            if s.contains('\n') {
                writer.flush();
            }
        }
        crate::drivers::cursor::text_end();
    }
}

static FB_WRITER: FbWriter = FbWriter::new();

pub fn init() {
    FB_WRITER.init();
}

/// Accessor returning the static framebuffer [`Console`](crate::drivers::Console)
/// handle, for use by the logging facade and other consumers.
pub fn console() -> &'static FbWriter {
    &FB_WRITER
}

pub fn _print(args: fmt::Arguments) {
    // Cursor-safe text output (see drivers::cursor::text_begin).
    crate::drivers::cursor::text_begin();
    // Push on every explicit print: callers of `fb_print!` are reacting to
    // something (boot progress, a command's output, a prompt) and expect it on
    // screen when the call returns, not whenever the next frame flush happens.
    FB_WRITER.write_fmt_now(args);
    crate::drivers::cursor::text_end();
}

pub fn clear_screen() {
    // Hide the arrow first and capture its background *after* the wipe: clearing
    // the screen while the cursor is drawn leaves its saved pixels describing the
    // old frame, and the next `hide()` would blit that stale square back.
    crate::drivers::cursor::text_begin();
    FB_WRITER.clear();
    FB_WRITER.flush();
    crate::drivers::cursor::text_end();
}

/// Push the shadow buffer's changed rectangle to the framebuffer.
///
/// Every drawing call writes to RAM (`FramebufferWriter::back`); nothing reaches
/// the device until this runs. That is what turns a full frame from tens of
/// thousands of uncached MMIO stores into a few linear row copies, and it is why
/// callers must flush once at the end of a repaint.
pub fn flush() {
    FB_WRITER.flush();
}

/// Sets the framebuffer foreground (glyph) color via the global writer.
/// Mirrors [`clear_screen`]; serial output is unaffected (color is
/// framebuffer-only).
pub fn set_fg_color(color: u32) {
    FB_WRITER.set_fg_color(color);
}

/// Sets the console background colour (unlit glyph pixels and the `clear` fill).
pub fn set_bg_color(color: u32) {
    FB_WRITER.set_bg_color(color);
}

/// Makes the console paint glyphs only, leaving the pixels behind them alone.
///
/// Used by `shell::theme` when a wallpaper is active; the shell reinstates opaque
/// painting when the wallpaper is switched back off.
pub fn set_opaque_bg(opaque: bool) {
    FB_WRITER.set_opaque_bg(opaque);
}

/// Sets the status-bar strip colours via the global writer.
pub fn set_status_colors(bg: u32, fg: u32) {
    FB_WRITER.set_status_colors(bg, fg);
}

/// Redraw the retained text grid into the shadow buffer and push it out.
///
/// Call after anything that wipes the pixels behind the text (wallpaper, theme
/// switch, `clear`): the model is the only record of what was on screen.
pub fn repaint_grid() {
    FB_WRITER.repaint_grid();
    FB_WRITER.flush();
}

/// Framebuffer dimensions in pixels via the global writer, `(0, 0)` if the
/// framebuffer never initialized.
pub fn dimensions() -> (usize, usize) {
    FB_WRITER.dimensions()
}

/// Run `f` with exclusive raw access to the framebuffer. See
/// [`FbWriter::with`]. Returns `None` if the framebuffer never initialized.
pub fn with<R>(f: impl FnOnce(&mut FramebufferWriter) -> R) -> Option<R> {
    FB_WRITER.with(f)
}

/// Repaint the bottom status bar with left/right aligned text.
pub fn draw_status_bar(left: &str, right: &str) {
    FB_WRITER.draw_status_bar(left, right);
}

#[macro_export]
macro_rules! fb_print {
    ($($arg:tt)*) => ($crate::drivers::framebuffer::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! fb_println {
    () => ($crate::fb_print!("\n"));
    ($($arg:tt)*) => ($crate::fb_print!("{}\n", format_args!($($arg)*)));
}
