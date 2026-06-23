// drivers/framebuffer.rs — Simple framebuffer text output using Limine
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use crate::sync::spinlock::Spinlock;
use core::fmt;

const CHAR_WIDTH: usize = 8;
const CHAR_HEIGHT: usize = 16;
const CHARS_PER_LINE: usize = 100;
const MAX_LINES: usize = 37;

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
        match ch {
            b'\n' => {
                self.col = 0;
                self.row += 1;
                if self.row >= MAX_LINES {
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
                    if self.row >= MAX_LINES {
                        self.scroll();
                    }
                }
                self.draw_char(ch, self.col, self.row);
                self.col += 1;
            }
            _ => {}
        }
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
        // Simple scroll: move everything up by one line
        unsafe {
            let fb = self.fb_addr as *mut u8;
            let line_bytes = CHAR_HEIGHT * self.pitch;
            let total_bytes = (self.height - CHAR_HEIGHT) * self.pitch;
            
            core::ptr::copy(
                fb.add(line_bytes),
                fb,
                total_bytes
            );
            
            // Clear last line
            let last_line_offset = (self.height - CHAR_HEIGHT) * self.pitch;
            for i in 0..(line_bytes) {
                fb.add(last_line_offset + i).write_volatile(0);
            }
        }
        self.row = MAX_LINES - 1;
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
}

impl crate::drivers::Console for FbWriter {
    fn write_str(&self, s: &str) {
        // Lock the inner writer and route the string through the existing
        // glyph-rendering path. No-op if the framebuffer never initialized.
        if let Some(ref mut writer) = *self.inner.lock() {
            writer.write_string(s);
        }
    }

    fn clear(&self) {
        FbWriter::clear(self);
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

#[macro_export]
macro_rules! fb_print {
    ($($arg:tt)*) => ($crate::drivers::framebuffer::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! fb_println {
    () => ($crate::fb_print!("\n"));
    ($($arg:tt)*) => ($crate::fb_print!("{}\n", format_args!($($arg)*)));
}
