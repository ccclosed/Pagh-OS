//! The blinking input caret.
//!
//! The framebuffer console used to be write-only: text went down and the position
//! of the *logical* cursor inside the line buffer was invisible, so a mid-line edit
//! looked like it had been applied at the end of the line. This module makes the
//! caret a real, visible object drawn at the cell the next typed character will
//! occupy.
//!
//! Three facts make that possible, and all three are load-bearing:
//!
//! * The console keeps a **cell model** of what it has printed, so a cell can be
//!   repainted without reprinting the line (`FramebufferWriter::paint_cell`).
//! * Painting a cell does **not advance the console's cursor**, so erasing the
//!   caret cannot walk the console forward or scroll it.
//! * The console reports **where it is** after the buffer has been printed, so the
//!   caret's cell is derived by walking *backwards* from there by the number of
//!   characters that follow the logical cursor. This is what makes a caret in the
//!   middle of the line exactly right instead of approximate.
//!
//! The caret is a vertical bar in the left pixel column of its cell, not a block:
//! a block hides the character it sits on, and the caret is meant to show where
//! text will go, not to replace what is already there.

use crate::drivers::framebuffer;
use crate::shell::editor::LineEditor;

/// Caret color: cyan (`0x00FFFF`), deliberately distinct from the green prompt
/// (R8.3) and the white default text, so it never reads as part of either.
pub const CARET_COLOR: u32 = 0x00FFFF;

/// The caret is on for the first half of each period and off for the second, so
/// the blink is symmetric and its rate does not depend on how often the shell
/// happens to come back around the loop.
const BLINK_PERIOD_MS: u64 = 600;

/// Caret bar height in pixels, out of the 16-pixel cell. Shorter than the glyphs
/// so the bar reads as an insertion point rather than as a drawn border.
const CARET_BAR_H: usize = 12;

/// A grid cell `(col, row)` on the text console.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub col: usize,
    pub row: usize,
}

/// Where the caret currently is, and whether it is drawn.
///
/// The shell owns one of these for the whole session: it has to erase the caret
/// **at the cell it was drawn at**, and a caret erased anywhere else leaves a
/// stray bar behind, so the cell cannot be recomputed from scratch at erase time.
pub struct Caret {
    cell: Option<Cell>,
    drawn: bool,
}

impl Caret {
    pub const fn new() -> Self {
        Caret {
            cell: None,
            drawn: false,
        }
    }

    /// Cell for the caret of `editor`, given the console's cursor cell **after**
    /// the buffer has been printed, and the console's column count.
    ///
    /// The console has just printed `prompt + buffer`, so its cursor sits one cell
    /// past the buffer's last character; the characters that follow the logical
    /// cursor are `len - cursor`, and the caret belongs that many cells back. This
    /// is exact rather than approximate — including mid-line, and including after
    /// the line has wrapped or the console has scrolled, because it is a difference
    /// between two positions rather than an absolute row.
    ///
    /// Returns `None` for a grid that cannot hold the caret (no framebuffer, a zero
    /// column count, or a back-walk that runs off the top of the grid), so callers
    /// simply skip drawing.
    pub fn cell_for(
        console_cell: (usize, usize),
        editor: &LineEditor,
        cols: usize,
    ) -> Option<Cell> {
        if cols == 0 {
            return None;
        }
        let trailing = editor
            .buffer()
            .chars()
            .count()
            .saturating_sub(editor.cursor());
        let (col, row) = console_cell;
        let end = row * cols + col;
        let start = end.checked_sub(trailing)?;
        Some(Cell {
            col: start % cols,
            row: start / cols,
        })
    }

    /// Move the caret to `cell`, drawing or erasing as needed, and return whether
    /// it is currently visible.
    ///
    /// Erase-then-draw, never draw-then-erase: the cell being vacated is restored
    /// from the console's own model, so the character under the old caret is not
    /// left blanked.
    pub fn place(&mut self, cell: Cell, on: bool) -> bool {
        if self.cell != Some(cell) {
            self.erase();
            self.cell = Some(cell);
        }
        self.set_visible(on);
        self.drawn
    }

    /// Blink phase for the current tick: on for the first half of every period.
    ///
    /// `ticks` is the scheduler's millisecond tick counter (`TICK_HZ` = 1000), so
    /// the period is expressed directly in milliseconds and does not depend on the
    /// console's refresh rate.
    pub fn blink_on(ticks: u64) -> bool {
        (ticks % BLINK_PERIOD_MS) < BLINK_PERIOD_MS / 2
    }

    /// Draw or erase the caret at its current cell. Idempotent: repeating the same
    /// state is a no-op, so this is safe to call on every loop iteration.
    pub fn set_visible(&mut self, on: bool) {
        if self.drawn == on {
            return;
        }
        let Some(cell) = self.cell else {
            return;
        };
        framebuffer::console().paint_caret(cell.col, cell.row, on, CARET_COLOR, CARET_BAR_H);
        self.drawn = on;
    }

    /// Erase the caret, if any, and forget where it was.
    ///
    /// Called before anything reprints the line (`redraw_line`, a new prompt, a
    /// completion listing): the caret is an overlay on top of a cell, so text
    /// printed over it would inherit a stray bar.
    pub fn erase(&mut self) {
        if let Some(cell) = self.cell {
            if self.drawn {
                framebuffer::console().paint_caret(
                    cell.col,
                    cell.row,
                    false,
                    CARET_COLOR,
                    CARET_BAR_H,
                );
            }
        }
        self.drawn = false;
    }

    /// Forget the caret without touching pixels, for when the screen was cleared
    /// or scrolled by someone else and no cell can be trusted any more.
    pub fn reset(&mut self) {
        self.cell = None;
        self.drawn = false;
    }
}

impl Default for Caret {
    fn default() -> Self {
        Self::new()
    }
}
