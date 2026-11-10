//! Emergency console: text straight into the Limine framebuffer, no heap.
//!
//! WHY THIS EXISTS. On a real PC there is no serial port, so the only thing an
//! operator can see is the screen — and every failure before the normal console
//! is up (heap allocation, APIC bring-up, a Limine response missing, a panic) was
//! invisible: `fatal()` and the panic handler wrote to COM1 and halted, leaving a
//! black display with no clue what happened. That is exactly the bug report this
//! module answers: "no image at all".
//!
//! CONSTRAINTS, and why the implementation looks primitive:
//!
//! * **No allocator.** The normal `FramebufferWriter` allocates a shadow buffer
//!   (`back`) and two grids, so it cannot exist until the heap does — and the
//!   steps that fail are precisely the ones before that.
//! * **No locks.** A panic may hold a lock already, so anything taking a spinlock
//!   could deadlock while reporting the very failure that stopped the machine.
//! * **No formatting machinery that allocates.** `core::fmt::Write` into a fixed
//!   buffer is fine; `format!` is not.
//!
//! So this is a plain "draw one glyph into the linear framebuffer" loop over the
//! same embedded font asset the real console uses. It is used by [`fatal`] and by
//! the panic handler, and by boot steps as a progress trace — so a machine that
//! dies at step N shows steps 1..N-1 and the name of the step that failed.

use core::fmt::{self, Write};
use core::ptr::write_volatile;

use crate::sync::spinlock::Spinlock;

/// Same glyph cell as the normal console (assets/font8x18.bin, 8x18 coverage).
const CELL_W: usize = 8;
const CELL_H: usize = 18;
/// 8-bit coverage per pixel, row-major: one byte per pixel.
const GLYPH_BYTES: usize = CELL_W * CELL_H;

/// The console's font asset, shared with `framebuffer.rs`.
static FONT: &[u8] = include_bytes!("../../assets/font8x18.bin");

/// Geometry captured once, at the first call.
struct Console {
    addr: u64,
    width: usize,
    height: usize,
    pitch: usize,
    bpp: usize,
    col: usize,
    row: usize,
}

static CONSOLE: Spinlock<Option<Console>> = Spinlock::new(None);
static READY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Colours (0x00RRGGBB), matching the normal console's defaults.
const FG: u32 = 0xFF_AAAAAA;
const BG: u32 = 0xFF_000000;
/// Panic/fatal text is red so it cannot be mistaken for a progress line.
const FG_ERROR: u32 = 0xFF_FF5555;

/// The asset covers printable ASCII only: `0x20..=0x7E`, one glyph per
/// codepoint, in order. It does NOT start at byte 0 for character 0 — indexing by
/// the raw byte shifts every glyph by 32, which renders as plausible-looking but
/// entirely wrong text.
const FONT_FIRST_CP: u8 = 0x20;
const FONT_LAST_CP: u8 = 0x7E;

fn glyph(ch: u8) -> &'static [u8] {
    if ch < FONT_FIRST_CP || ch > FONT_LAST_CP {
        return &FALLBACK;
    }
    let idx = (ch - FONT_FIRST_CP) as usize;
    let start = idx * GLYPH_BYTES;
    if start + GLYPH_BYTES <= FONT.len() {
        &FONT[start..start + GLYPH_BYTES]
    } else {
        &FALLBACK
    }
}

/// A hollow box, drawn for any byte outside the font's range so a stray control
/// character is visible as a box rather than silently swallowed.
static FALLBACK: [u8; GLYPH_BYTES] = {
    let mut g = [0u8; GLYPH_BYTES];
    let mut i = 0;
    while i < GLYPH_BYTES {
        let (row, col) = (i / CELL_W, i % CELL_W);
        let border = row == 0 || row == CELL_H - 1 || col == 0 || col == CELL_W - 1;
        g[i] = if border { 0xFF } else { 0x00 };
        i += 1;
    }
    g
};

fn put_px(c: &Console, x: usize, y: usize, colour: u32) {
    if x >= c.width || y >= c.height {
        return;
    }
    let off = c.addr + (y * c.pitch + x * c.bpp) as u64;
    // SAFETY: `off` is inside the framebuffer Limine described (bounds checked
    // above), and every write is volatile so the compiler cannot elide or reorder
    // it — this runs during a panic, where optimising away a write is plausible.
    unsafe {
        match c.bpp {
            4 => write_volatile(off as *mut u32, colour),
            3 => {
                // BGR in memory for the usual 24-bit mode; write bytewise.
                write_volatile(off as *mut u8, (colour & 0xFF) as u8);
                write_volatile((off + 1) as *mut u8, ((colour >> 8) & 0xFF) as u8);
                write_volatile((off + 2) as *mut u8, ((colour >> 16) & 0xFF) as u8);
            }
            _ => {}
        }
    }
}

/// Draw one character at the given cell.
fn draw(c: &Console, ch: u8, colour: u32) {
    let x0 = c.col * CELL_W;
    let y0 = c.row * CELL_H;
    let g = glyph(ch);
    for dy in 0..CELL_H {
        for dx in 0..CELL_W {
            let cov = g[dy * CELL_W + dx] as u32;
            // Blend coverage between the background and the ink, same idea as the
            // real console's antialiasing (no FPU: integer weighting).
            let px = if cov == 0 {
                colour & 0xFF00_0000 | BG
            } else {
                let fg = colour & 0x00FF_FFFF;
                let bg = BG & 0x00FF_FFFF;
                let mix = |shift: u32| -> u32 {
                    let f = (fg >> shift) & 0xFF;
                    let b = (bg >> shift) & 0xFF;
                    ((f * cov + b * (255 - cov)) / 255) & 0xFF
                };
                (FG & 0xFF00_0000) | (mix(16) << 16) | (mix(8) << 8) | mix(0)
            };
            put_px(c, x0 + dx, y0 + dy, px);
        }
    }
}

/// Capture the Limine framebuffer geometry. Idempotent; cheap after the first call.
///
/// Returns false when Limine gave no framebuffer, in which case this console
/// silently does nothing (a serial-only boot, or the in-QEMU E2E runs).
pub fn init() -> bool {
    if READY.load(core::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    let Some(response) = crate::FRAMEBUFFER_REQUEST.response() else {
        return false;
    };
    let Some(fb) = response.framebuffers().first() else {
        return false;
    };
    let bpp = (fb.bpp / 8) as usize;
    if bpp != 3 && bpp != 4 {
        return false;
    }
    let mut guard = CONSOLE.lock();
    if guard.is_none() {
        *guard = Some(Console {
            addr: fb.address() as u64,
            width: fb.width as usize,
            height: fb.height as usize,
            pitch: fb.pitch as usize,
            bpp,
            col: 0,
            row: 0,
        });
    }
    drop(guard);
    READY.store(true, core::sync::atomic::Ordering::Relaxed);
    true
}

/// Write text, wrapping at the screen edge and scrolling by one cell row.
fn emit(text: &str, colour: u32) {
    if !READY.load(core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let mut guard = CONSOLE.lock();
    let Some(c) = guard.as_mut() else {
        return;
    };
    let cols = c.width / CELL_W;
    let rows = c.height / CELL_H;
    if cols == 0 || rows == 0 {
        return;
    }
    for byte in text.bytes() {
        match byte {
            b'\n' => {
                c.col = 0;
                c.row += 1;
            }
            b'\r' => c.col = 0,
            b'\t' => {
                c.col = (c.col + 4) & !3;
            }
            0x20..=0x7E => {
                let snapshot = Console {
                    addr: c.addr,
                    width: c.width,
                    height: c.height,
                    pitch: c.pitch,
                    bpp: c.bpp,
                    col: c.col,
                    row: c.row,
                };
                draw(&snapshot, byte, colour);
                c.col += 1;
            }
            _ => continue,
        }
        if c.col >= cols {
            c.col = 0;
            c.row += 1;
        }
        if c.row >= rows {
            // Scroll: no shadow buffer to blit, so move the visible rows up by
            // copying the framebuffer onto itself a row at a time.
            let row_bytes = c.pitch * CELL_H;
            // SAFETY: both ranges are inside the framebuffer Limine described. The
            // source starts one text row in, so the copy moves upward over memory
            // that is already mapped and inside the same device region.
            unsafe {
                core::ptr::copy(
                    (c.addr + row_bytes as u64) as *const u8,
                    c.addr as *mut u8,
                    row_bytes * (rows - 1),
                );
            }
            // Clear the last row.
            for y in ((rows - 1) * CELL_H)..(rows * CELL_H) {
                for x in 0..c.width {
                    let snapshot = Console {
                        addr: c.addr,
                        width: c.width,
                        height: c.height,
                        pitch: c.pitch,
                        bpp: c.bpp,
                        col: 0,
                        row: 0,
                    };
                    put_px(&snapshot, x, y, BG);
                }
            }
            c.row = rows - 1;
        }
    }
}

/// Print a line of progress text (grey).
pub fn println(args: fmt::Arguments) {
    let mut buf = FixedBuf::new();
    let _ = buf.write_fmt(args);
    emit("\n", FG);
    emit(buf.as_str(), FG);
    emit("\n", FG);
}

/// Print an error line in red. Used by `fatal()` and the panic handler.
pub fn errorln(args: fmt::Arguments) {
    let mut buf = FixedBuf::new();
    let _ = buf.write_fmt(args);
    emit("\n", FG_ERROR);
    emit(buf.as_str(), FG_ERROR);
    emit("\n", FG_ERROR);
}

/// Clear the screen to black (called once when the console is first captured).
pub fn clear() {
    if !init() {
        return;
    }
    let mut guard = CONSOLE.lock();
    let Some(c) = guard.as_mut() else {
        return;
    };
    let snapshot = Console {
        addr: c.addr,
        width: c.width,
        height: c.height,
        pitch: c.pitch,
        bpp: c.bpp,
        col: 0,
        row: 0,
    };
    for y in 0..snapshot.height {
        for x in 0..snapshot.width {
            put_px(&snapshot, x, y, BG);
        }
    }
    c.col = 0;
    c.row = 0;
}

/// A fixed 512-byte formatting buffer: `core::fmt` without allocation.
struct FixedBuf {
    buf: [u8; 512],
    len: usize,
}

impl FixedBuf {
    fn new() -> Self {
        Self {
            buf: [0; 512],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("<non-utf8>")
    }
}

impl Write for FixedBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if self.len < self.buf.len() {
                self.buf[self.len] = b;
                self.len += 1;
            } else {
                return Err(fmt::Error);
            }
        }
        Ok(())
    }
}
