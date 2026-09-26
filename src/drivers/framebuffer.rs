// drivers/framebuffer.rs — Simple framebuffer text output using Limine
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use crate::sync::spinlock::Spinlock;
use core::fmt;

const CHAR_WIDTH: usize = 8;
const CHAR_HEIGHT: usize = 16;
const CHARS_PER_LINE: usize = 100;
const MAX_LINES: usize = 37;

/// Height in pixels of the reserved status-bar strip at the bottom of the
/// screen. The text console never scrolls into this region (see `scroll`).
pub const STATUS_BAR_HEIGHT: usize = 18;

// 8x16 glyph data loaded from the font asset instead of a hardcoded table.
// The file packs 95 glyphs of 16 bytes each (one byte per row, bit 7 = leftmost
// pixel), covering printable ASCII from 0x20 (space) through 0x7E ('~'),
// so the glyph for codepoint `ch` lives at index `ch - 0x20`.
static FONT: &[u8] = include_bytes!("../../assets/font8x16.bin");

const FONT_FIRST_CP: u8 = 0x20; // first codepoint present in the font
const FONT_LAST_CP: u8 = 0x7E; // last codepoint present in the font
const GLYPH_BYTES: usize = 16; // rows per glyph

// Fallback glyph (filled rectangle) for codepoints outside the font range,
// preserving the previous behavior of the hardcoded table's default arm.
const FALLBACK_GLYPH: [u8; GLYPH_BYTES] = [
    0x00, 0x7E, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x7E, 0x00, 0x00, 0x00, 0x00,
];

/// Returns the 16-byte (16-row) glyph for `ch`, sourced from the embedded font.
/// Codepoints outside the font's `0x20..=0x7D` range fall back to a filled box.
fn get_glyph(ch: u8) -> &'static [u8] {
    if ch >= FONT_FIRST_CP && ch <= FONT_LAST_CP {
        let idx = (ch - FONT_FIRST_CP) as usize;
        let start = idx * GLYPH_BYTES;
        &FONT[start..start + GLYPH_BYTES]
    } else {
        &FALLBACK_GLYPH
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
            fg_color: 0xFFFFFF,
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
        for b in self.back.iter_mut() {
            *b = 0;
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

            let glyph_byte = glyph[dy];

            for dx in 0..CHAR_WIDTH {
                if x + dx >= self.width {
                    break;
                }

                let pixel_on = (glyph_byte & (1 << (7 - dx))) != 0;
                let color = if pixel_on { self.fg_color } else { 0x000000 };

                self.put_pixel(x + dx, y + dy, color);
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
            self.mark_dirty(x, y);
        }
    }

    /// Extend the pending-flush rectangle to include `(x, y)`.
    fn mark_dirty(&mut self, x: usize, y: usize) {
        self.dirty = Some(match self.dirty {
            None => (x, y, x, y),
            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
        });
    }

    /// Copy the changed rectangle from the shadow buffer to the framebuffer.
    ///
    /// Only the rows that changed are touched, and rows are copied with one
    /// incremental pointer walk rather than a volatile store per component, so the
    /// device sees a small number of linear runs instead of tens of thousands of
    /// individual writes. Returns whether anything was flushed.
    pub fn flush(&mut self) -> bool {
        if self.dirty.take().is_none() {
            return false;
        }
        // Flush the whole surface, not the tracked rectangle.
        //
        // A bounding box was tried first and was wrong in practice: the screen kept
        // showing stale rows, because a rectangle computed from individual pixel
        // writes does not survive the bulk paths (`scroll` shifts the buffer, `clear`
        // wipes it) without every one of them remembering to widen it. Correctness
        // first: this is one linear copy into the device instead of tens of thousands
        // of uncached component writes, which is where the original slowness came
        // from. Damage tracking can come back once the simple version is trusted.
        let len = self.pitch * self.height;
        // SAFETY: the shadow buffer is `pitch * height` bytes (allocated that way) and
        // the framebuffer is the device surface of the same geometry, so both spans
        // are valid and do not overlap (one is kernel RAM, the other is MMIO).
        unsafe {
            core::ptr::copy_nonoverlapping(self.back.as_ptr(), self.fb_addr as *mut u8, len);
        }
        true
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
    /// The background/clear color is unaffected. Defaults to `0xFFFFFF`.
    pub fn set_fg_color(&mut self, color: u32) {
        self.fg_color = color;
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
        self.paint_cell_with_bg(col, row, ch, color, 0x000000);
    }

    /// As [`paint_cell`](Self::paint_cell), with an explicit cell background.
    ///
    /// The background is what makes a text selection readable: inverting the cell
    /// is visible where a foreground change would be lost against text of the same
    /// color.
    pub fn paint_cell_with_bg(&mut self, col: usize, row: usize, ch: u8, color: u32, bg: u32) {
        let (max_cols, max_rows) = self.text_grid();
        if col >= max_cols || row >= max_rows {
            return;
        }
        // Draw the glyph's own pixels in `color`, everything else in `bg`, so
        // repainting a cell fully replaces what was there.
        let x = col * CHAR_WIDTH;
        let y = row * CHAR_HEIGHT;
        let glyph = get_glyph(ch);
        for dy in 0..CHAR_HEIGHT {
            if y + dy >= self.height {
                break;
            }
            let glyph_byte = glyph[dy];
            for dx in 0..CHAR_WIDTH {
                if x + dx >= self.width {
                    break;
                }
                let pixel_on = (glyph_byte & (1 << (7 - dx))) != 0;
                self.put_pixel(x + dx, y + dy, if pixel_on { color } else { bg });
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
            let row = glyph[dy];
            for dx in 0..CHAR_WIDTH {
                let on = (row & (1 << (7 - dx))) != 0;
                self.put_pixel(px + dx, py + dy, if on { fg } else { bg });
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
        const BG: u32 = 0x1B3A5B; // slate blue
        const FG: u32 = 0xE6F0FF;
        self.fill_rect(0, top, self.width, STATUS_BAR_HEIGHT, BG);
        // separator highlight line on top of the bar
        for px in 0..self.width {
            self.put_pixel(px, top, 0x3C6FA0);
        }
        let ty = top + (STATUS_BAR_HEIGHT - CHAR_HEIGHT) / 2;
        self.draw_text_px(6, ty, left, FG, BG);
        let rw = right.len() * CHAR_WIDTH;
        if rw + 6 < self.width {
            self.draw_text_px(self.width - rw - 6, ty, right, FG, BG);
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
            writer.paint_cell_with_bg(col, row, ch, color, bg);
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
    FB_WRITER.write_fmt(args);
    crate::drivers::cursor::text_end();
}

pub fn clear_screen() {
    FB_WRITER.clear();
    FB_WRITER.flush();
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
