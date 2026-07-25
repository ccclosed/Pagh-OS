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

use crate::drivers::framebuffer;
use crate::sync::spinlock::Spinlock;

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

// ─── STAGE-13.8: cursor-safe text output ─────────────────────────────────────
//
// The framebuffer console can be written from arbitrary threads (kernel logs,
// the first-boot provisioner, the Linux-compat stdout mirror). Only the shell
// follows the hide()/move_to() contract manually, so any other writer could
// draw glyphs (or scroll the whole screen) under the visible arrow, leaving
// its saved background stale and smearing artifacts on the next mouse move.
//
// `text_begin`/`text_end` wrap every console text write: the first (outermost)
// begin hides the arrow if it was visible, the matching last end redraws it at
// the same position. Depth-tracked so nested prints stay balanced, and a no-op
// while the shell already has the cursor hidden.

struct TextGuard {
    depth: usize,
    redraw: bool,
}

static TEXT_GUARD: Spinlock<TextGuard> = Spinlock::new(TextGuard {
    depth: 0,
    redraw: false,
});

/// Begin a console text write: on the outermost call, hide the arrow when it
/// is currently visible (remembering that it must come back).
pub fn text_begin() {
    let need_hide = {
        let mut g = TEXT_GUARD.lock();
        g.depth += 1;
        if g.depth == 1 {
            let visible = CURSOR.lock().visible;
            g.redraw = visible;
            visible
        } else {
            false
        }
    };
    if need_hide {
        hide();
    }
}

/// End a console text write: on the outermost call, redraw the arrow at its
/// last position when `text_begin` hid it.
pub fn text_end() {
    let redraw_at = {
        let mut g = TEXT_GUARD.lock();
        g.depth = g.depth.saturating_sub(1);
        if g.depth == 0 && g.redraw {
            g.redraw = false;
            let c = CURSOR.lock();
            Some((c.x, c.y))
        } else {
            None
        }
    };
    if let Some((x, y)) = redraw_at {
        move_to(x, y);
    }
}
