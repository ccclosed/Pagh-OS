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
//
// Flushing contract: the framebuffer console draws into a RAM shadow buffer, so
// the arrow is only on the screen if its pixels are pushed to the device. The
// cursor owns that push: writes are "quiet" (they do not mark the frame dirty)
// and `hide`/`move_to` flush just the 12x19 rectangle they touched. This keeps
// the frame flush for actual frame changes — before that split, every mouse move
// made the shell copy the whole surface, and drawing the arrow *after* the frame
// flush without a rect flush meant the arrow never reached the device at all
// (the next `hide` restored the background before the next frame flush).

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
        fb.set_quiet(true);
        fb.blit(x, y, CURSOR_W, CURSOR_H, &saved, CURSOR_W);
        // The device still shows the arrow until these restored pixels are pushed.
        fb.flush_rect(x, y, CURSOR_W, CURSOR_H);
        fb.set_quiet(false);
    });
    c.have_saved = false;
    c.visible = false;
}

/// Draw the arrow at `(x, y)` into the shadow buffer as ordinary frame content.
///
/// Unlike [`move_to`] this does not `flush_rect` the arrow: the pixels are marked
/// as frame damage, so the caller's single `framebuffer::flush` carries the whole
/// update — old arrow erased by [`hide_deferred`], the new frame, and the new arrow
/// — to the device in one step. That is what makes the pointer continuous while it
/// moves: there is no moment in which the device shows a frame without an arrow.
pub fn place_deferred(x: usize, y: usize) {
    let (fw, fh) = framebuffer::dimensions();
    if fw == 0 {
        return;
    }
    let nx = x.min(fw.saturating_sub(1));
    let ny = y.min(fh.saturating_sub(1));
    let mut c = CURSOR.lock();
    let mut new_saved = [0u32; CURSOR_W * CURSOR_H];
    framebuffer::with(|fb| {
        for row in 0..CURSOR_H {
            for col in 0..CURSOR_W {
                new_saved[row * CURSOR_W + col] = fb.get_pixel(nx + col, ny + row);
            }
        }
        draw_arrow(fb, nx, ny);
    });
    c.x = nx;
    c.y = ny;
    c.saved = new_saved;
    c.have_saved = true;
    c.visible = true;
}

/// [`place_deferred`] at the cursor's last known coordinates.
pub fn place_here_deferred() {
    let (x, y) = {
        let c = CURSOR.lock();
        (c.x, c.y)
    };
    place_deferred(x, y);
}

/// Redraw the arrow at its last known position.
///
/// The counterpart of [`hide`]: full-screen drawers (`nano`, the settings panel)
/// take the arrow down for a frame and must put it back on top of the finished
/// frame. `hide` deliberately keeps the coordinates, so this re-captures whatever
/// the new frame has under the pointer instead of restoring a pre-repaint
/// background.
pub fn show() {
    let (x, y) = {
        let c = CURSOR.lock();
        (c.x, c.y)
    };
    move_to(x, y);
}

/// Hide the arrow *without* pushing the restored pixels right away.
///
/// For a frame at a time: the restored background is marked as frame damage
/// instead of being `flush_rect`-ed immediately, so the caller's next
/// `framebuffer::flush` pushes it together with the new frame. That removes the
/// window in which the device showed neither the old frame nor the arrow — the
/// "cursor flickers while it moves" complaint — while keeping the saved pixels
/// consistent. Call [`redraw`] after the frame flush.
pub fn hide_deferred() {
    let mut c = CURSOR.lock();
    if !c.have_saved {
        c.visible = false;
        return;
    }
    let (x, y, saved) = (c.x, c.y, c.saved);
    // Not quiet: this is ordinary frame content now, so the frame flush carries it.
    framebuffer::with(|fb| fb.blit(x, y, CURSOR_W, CURSOR_H, &saved, CURSOR_W));
    c.have_saved = false;
    c.visible = false;
}

/// Re-capture the background and draw the arrow at its last known position.
///
/// The counterpart of [`hide_deferred`]: any saved pixels describe the frame from
/// before the repaint, so they are discarded rather than restored. This is also
/// what full-screen drawers (`nano`'s file switch, the settings panel) use after
/// their flush so the pointer never vanishes for the duration of the repaint.
pub fn redraw() {
    {
        let mut c = CURSOR.lock();
        c.have_saved = false;
    }
    show();
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
    // Idempotent: the shell calls this on every wake-up (once per timer tick),
    // and redrawing an arrow that has not moved would push the same rectangle to
    // the device for nothing.
    if c.have_saved && c.x == nx && c.y == ny {
        c.visible = true;
        return;
    }
    let (ox, oy, had) = (c.x, c.y, c.have_saved);
    let old_saved = c.saved;
    let mut new_saved = [0u32; CURSOR_W * CURSOR_H];

    framebuffer::with(|fb| {
        fb.set_quiet(true);
        // 1) Restore the previous location.
        if had {
            fb.blit(ox, oy, CURSOR_W, CURSOR_H, &old_saved, CURSOR_W);
            fb.flush_rect(ox, oy, CURSOR_W, CURSOR_H);
        }
        // 2) Capture the background at the new location.
        for row in 0..CURSOR_H {
            for col in 0..CURSOR_W {
                new_saved[row * CURSOR_W + col] = fb.get_pixel(nx + col, ny + row);
            }
        }
        // 3) Draw the arrow over the new location and push that rectangle.
        draw_arrow(fb, nx, ny);
        fb.flush_rect(nx, ny, CURSOR_W, CURSOR_H);
        fb.set_quiet(false);
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

// ─── cursor-safe text output ─────────────────────────────────────
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
