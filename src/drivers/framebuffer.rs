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
const FALLBACK_GLYPH: [u8; GLYPH_BYTES] =
    [0x00, 0x7E, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x7E, 0x00, 0x00, 0x00, 0x00];

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
        
        crate::debug!("addr=0x{:x} w={} h={} pitch={} bpp={}", 
            address, width, height, pitch, bpp);
        
        Some(FramebufferWriter {
            fb_addr: address,
            width: width as usize,
            height: height as usize,
            pitch: pitch as usize,
            bpp: (bpp / 8) as usize,
            col: 0,
            row: 0,
            fg_color: 0xFFFFFF,
        })
    }

    pub fn clear(&mut self) {
        self.col = 0;
        self.row = 0;
        unsafe {
            let fb = self.fb_addr as *mut u8;
            for i in 0..(self.height * self.pitch) {
                fb.add(i).write_volatile(0);
            }
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
            0x08 => { // Backspace
                if self.col > 0 {
                    self.col -= 1;
                    self.draw_char(b' ', self.col, self.row);
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
        unsafe {
            let fb = self.fb_addr as *mut u8;
            if self.bpp >= 3 {
                fb.add(offset).write_volatile((color & 0xFF) as u8);
                fb.add(offset + 1).write_volatile(((color >> 8) & 0xFF) as u8);
                fb.add(offset + 2).write_volatile(((color >> 16) & 0xFF) as u8);
            }
        }
    }

    fn scroll(&mut self) {
        // Scroll only the text region (the usable rows), leaving the reserved
        // status-bar strip at the bottom of the screen untouched.
        let rows = self.text_rows();
        unsafe {
            let fb = self.fb_addr as *mut u8;
            let line_bytes = CHAR_HEIGHT * self.pitch;
            let text_px = rows * CHAR_HEIGHT;
            let move_bytes = (text_px - CHAR_HEIGHT) * self.pitch;

            core::ptr::copy(
                fb.add(line_bytes),
                fb,
                move_bytes
            );

            // Clear the now-vacated last text line.
            let last_line_offset = (text_px - CHAR_HEIGHT) * self.pitch;
            for i in 0..(line_bytes) {
                fb.add(last_line_offset + i).write_volatile(0);
            }
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
        // SAFETY: offset is within the mapped framebuffer (bounds checked).
        unsafe {
            let fb = self.fb_addr as *const u8;
            if self.bpp >= 3 {
                let b = fb.add(offset).read_volatile() as u32;
                let g = fb.add(offset + 1).read_volatile() as u32;
                let r = fb.add(offset + 2).read_volatile() as u32;
                (r << 16) | (g << 8) | b
            } else {
                0
            }
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
    pub fn draw_thick_line(&mut self, x0: isize, y0: isize, x1: isize, y1: isize, thick: isize, color: u32) {
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
            self.fill_rect((x - r).max(0) as usize, (y - r).max(0) as usize, thick as usize, thick as usize, color);
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
    pub fn blit(&mut self, dx: usize, dy: usize, w: usize, h: usize, src: &[u32], src_stride: usize) {
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
        self.inner.lock().as_ref().map(|w| w.dimensions()).unwrap_or((0, 0))
    }

    /// Repaint the bottom status bar via the locked writer.
    pub fn draw_status_bar(&self, left: &str, right: &str) {
        if let Some(ref mut writer) = *self.inner.lock() {
            writer.draw_status_bar(left, right);
        }
    }
}

impl crate::drivers::Console for FbWriter {
    fn write_str(&self, s: &str) {
        // Lock the inner writer and route the string through the existing
        // glyph-rendering path. No-op if the framebuffer never initialized.
        if let Some(ref mut writer) = *self.inner.lock() {
            writer.write_string(s);
        }
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
    FB_WRITER.write_fmt(args);
}

pub fn clear_screen() {
    FB_WRITER.clear();
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
