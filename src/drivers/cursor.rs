// drivers/cursor.rs — Software mouse cursor (framebuffer overlay)
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// A classic arrow cursor drawn directly into the framebuffer. Because there is
// no hardware cursor plane, the pixels underneath the arrow are saved before
// drawing and restored before the next move, so the cursor leaves no trail.
//
// Ordering contract (important when something else draws under the cursor, e.g.
// `paint`'s shape preview): callers must `hide()` (restore background) before
// redrawing the area beneath the cursor, then `move_to()` afterwards to
// re-capture the fresh background and redraw the arrow on top.

use crate::sync::spinlock::Spinlock;
use crate::drivers::framebuffer;

const CURSOR_W: usize = 12;
const CURSOR_H: usize = 19;

const BORDER: u32 = 0x000000;
const FILL: u32 = 0xFFFFFF;

/// Arrow shape: `#` = black border, `o` = white fill, space = transparent.
static ARROW: [&str; CURSOR_H] = [
    "#",
    "##",
    "#o#",
    "#oo#",
    "#ooo#",
    "#oooo#",
    "#ooooo#",
    "#oooooo#",
    "#ooooooo#",
    "#oooooooo#",
    "#ooooo#####",
    "#oo#oo#",
    "#o# #oo#",
    "##  #oo#",
    "#    #oo#",
    "      #oo#",
    "      #oo#",
    "       #o#",
    "       ##",
];

struct CursorState {
    x: usize,
    y: usize,
    visible: bool,
    have_saved: bool,
    saved: [u32; CURSOR_W * CURSOR_H],
}

static CURSOR: Spinlock<CursorState> = Spinlock::new(CursorState {
    x: 0,
    y: 0,
    visible: false,
    have_saved: false,
    saved: [0; CURSOR_W * CURSOR_H],
});

/// Restore the background under the cursor (if drawn) and mark it hidden.
///
/// Call before drawing anything that lands under the current cursor position;
/// follow with [`move_to`] to bring it back.
pub fn hide() {
    let mut c = CURSOR.lock();
    if !c.have_saved {
        c.visible = false;
        return;
    }
    let (x, y, saved) = (c.x, c.y, c.saved);
    framebuffer::with(|fb| {
        fb.blit(x, y, CURSOR_W, CURSOR_H, &saved, CURSOR_W);
    });
    c.have_saved = false;
    c.visible = false;
}

/// Move the cursor to `(x, y)`: restore the old background, capture the new
/// background, and draw the arrow on top. Coordinates are clamped to the
/// framebuffer.
pub fn move_to(x: usize, y: usize) {
    let (fw, fh) = framebuffer::dimensions();
    if fw == 0 {
        return;
    }
    let nx = x.min(fw.saturating_sub(1));
    let ny = y.min(fh.saturating_sub(1));

    let mut c = CURSOR.lock();
    let (ox, oy, had) = (c.x, c.y, c.have_saved);
    let old_saved = c.saved;
    let mut new_saved = [0u32; CURSOR_W * CURSOR_H];

    framebuffer::with(|fb| {
        // 1) Restore the previous location.
        if had {
            fb.blit(ox, oy, CURSOR_W, CURSOR_H, &old_saved, CURSOR_W);
        }
        // 2) Capture the background at the new location.
        for row in 0..CURSOR_H {
            for col in 0..CURSOR_W {
                new_saved[row * CURSOR_W + col] = fb.get_pixel(nx + col, ny + row);
            }
        }
        // 3) Draw the arrow over the new location.
        draw_arrow(fb, nx, ny);
    });

    c.x = nx;
    c.y = ny;
    c.saved = new_saved;
    c.have_saved = true;
    c.visible = true;
}

fn draw_arrow(fb: &mut framebuffer::FramebufferWriter, x: usize, y: usize) {
    for (row, line) in ARROW.iter().enumerate() {
        for (col, ch) in line.bytes().enumerate() {
            match ch {
                b'#' => fb.set_pixel(x + col, y + row, BORDER),
                b'o' => fb.set_pixel(x + col, y + row, FILL),
                _ => {}
            }
        }
    }
}
