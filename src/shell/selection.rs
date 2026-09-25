//! Mouse text selection over the console.
//!
//! The console keeps a cell model of what it has printed (`FramebufferWriter`'s
//! grid) and can repaint a single cell with a background, which between them make
//! selection possible without touching the pixel-level drawing code: a selection
//! is a range of cells, drawn by repainting those cells inverted.
//!
//! Two coordinate systems meet here and the whole module is about not confusing
//! them:
//!
//! * **Console cells** — `(col, row)` on the screen. What the mouse reports, after
//!   dividing by the glyph size.
//! * **Line indices** — positions in the editable line, where index 0 is the first
//!   character *after the prompt*. What the editor understands.
//!
//! The prompt is what relates them: the prompt occupies `prompt_cols` cells at the
//! start of the line, so cell `prompt_cols + k` on the line's first row is line
//! index `k`. Anything selected to the left of the prompt's end is not line text
//! and is clipped away, because a selection that included the prompt could not be
//! deleted from the buffer.
//!
//! Text for the clipboard is read back from the grid rather than from the buffer,
//! so a selection spanning output the user scrolled past still copies what they
//! can see.

use crate::drivers::framebuffer;
use crate::shell::caret::Cell;

/// Colors for a selected cell: dark text on a light background, the inverse of
/// the console's ordinary light-on-dark. Inverting is what makes the selection
/// visible over text of any color.
pub const SELECT_FG: u32 = 0x000000;
pub const SELECT_BG: u32 = 0xCCCCCC;

/// A range of cells on the console, from `a` to `b` inclusive, in reading order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CellRange {
    pub a: Cell,
    pub b: Cell,
}

impl CellRange {
    /// Order two points into reading order (top-to-bottom, then left-to-right), so
    /// a drag in any direction describes the same range.
    pub fn new(from: Cell, to: Cell) -> Self {
        let ordered = (from.row, from.col) <= (to.row, to.col);
        if ordered {
            CellRange { a: from, b: to }
        } else {
            CellRange { a: to, b: from }
        }
    }

    /// Iterate the cells of the range, row by row.
    pub fn cells(&self, cols: usize) -> alloc::vec::Vec<Cell> {
        let mut out = alloc::vec::Vec::new();
        if cols == 0 || (self.a.row, self.a.col) > (self.b.row, self.b.col) {
            return out;
        }
        for row in self.a.row..=self.b.row {
            let from = if row == self.a.row { self.a.col } else { 0 };
            let to = if row == self.b.row {
                self.b.col
            } else {
                cols - 1
            };
            for col in from..=to.min(cols - 1) {
                out.push(Cell { col, row });
            }
        }
        out
    }

    /// The selected text, read back from the console's model.
    ///
    /// Rows are joined with `\n`, and each row is trimmed of trailing blanks: the
    /// console pads every row to the full width, so copying a selection that ends
    /// mid-line would otherwise drag a screenful of spaces into the clipboard.
    /// Leading spaces are kept — they are real content (indentation).
    pub fn text(&self, cols: usize) -> alloc::string::String {
        use alloc::string::String;
        let mut out = String::new();
        if cols == 0 {
            return out;
        }
        for row in self.a.row..=self.b.row {
            let from = if row == self.a.row { self.a.col } else { 0 };
            let to = if row == self.b.row {
                self.b.col
            } else {
                cols - 1
            };
            let mut line = framebuffer::console().read_row(row, from, to + 1);
            while line.ends_with(' ') {
                line.pop();
            }
            out.push_str(&line);
            if row != self.b.row {
                out.push('\n');
            }
        }
        out
    }

    /// The same range expressed as indices into the editable line.
    ///
    /// `prompt_cols` is the prompt's width on the first row of the line, `cols` the
    /// console width. Returns `None` when the range does not overlap the line's text
    /// at all (a selection entirely inside the prompt, or on an output row), because
    /// there is then nothing to delete or replace.
    pub fn line_indices(
        &self,
        prompt_cols: usize,
        cols: usize,
        line_len: usize,
    ) -> Option<(usize, usize)> {
        if cols == 0 {
            return None;
        }
        let first = self.a.row * cols + self.a.col;
        let last = self.b.row * cols + self.b.col;
        // The line's text starts right after the prompt and is `line_len` long.
        let start_of_line = prompt_cols; // prompt is printed at the line's first cell
        let end_of_line = start_of_line + line_len;
        let from = first.max(start_of_line);
        let to = (last + 1).min(end_of_line);
        if from >= to {
            return None;
        }
        Some((from - start_of_line, to - start_of_line))
    }
}

/// An in-progress or finished mouse selection.
pub struct Selection {
    anchor: Option<Cell>,
    range: Option<CellRange>,
    /// Exact line indices this selection corresponds to, when they are known
    /// without inference. Keyboard selection knows them by construction (`Ctrl+A`
    /// selects the buffer), so it does not have to re-derive them from cells —
    /// which is the arithmetic that turned out to be unreliable.
    line_range: Option<(usize, usize)>,
    painted: bool,
}

impl Selection {
    pub const fn new() -> Self {
        Selection {
            anchor: None,
            range: None,
            line_range: None,
            painted: false,
        }
    }

    /// Set the range directly (no drag), for keyboard-driven selection, together
    /// with the line indices it covers.
    pub fn set_range(&mut self, range: CellRange, line_range: Option<(usize, usize)>) {
        self.anchor = None;
        self.range = Some(range);
        self.line_range = line_range;
    }

    /// The line indices this selection covers, when they are known exactly.
    pub fn known_line_range(&self) -> Option<(usize, usize)> {
        self.line_range
    }

    /// Begin a drag at `cell`.
    pub fn begin(&mut self, cell: Cell) {
        self.clear();
        self.anchor = Some(cell);
        self.range = None;
        self.line_range = None;
    }

    /// Extend the drag to `cell` and report whether anything is selected.
    pub fn extend(&mut self, cell: Cell) -> bool {
        let Some(anchor) = self.anchor else {
            return false;
        };
        self.range = Some(CellRange::new(anchor, cell));
        self.range.is_some()
    }

    /// Finish the drag. Returns the finished range, if the drag covered more than
    /// one cell — a plain click is not a selection and must not leave a highlight.
    pub fn finish(&mut self) -> Option<CellRange> {
        self.anchor = None;
        match self.range {
            Some(r) if r.a != r.b => Some(r),
            _ => {
                self.clear();
                None
            }
        }
    }

    pub fn range(&self) -> Option<CellRange> {
        self.range
    }

    /// Whether a drag is in progress.
    pub fn is_active(&self) -> bool {
        self.anchor.is_some()
    }

    /// Repaint the range as selected. Repeated calls are cheap: the range is
    /// remembered, and only a changed range is repainted.
    pub fn paint(&mut self, cols: usize) {
        let Some(range) = self.range else {
            return;
        };
        for cell in range.cells(cols) {
            framebuffer::console()
                .paint_cell_selected(cell.col, cell.row, SELECT_FG, SELECT_BG, false);
        }
        self.painted = true;
    }

    /// Repaint the range back to ordinary console colors.
    pub fn unpaint(&mut self) {
        let Some(range) = self.range else {
            return;
        };
        let cols = framebuffer::console().text_grid().0;
        for cell in range.cells(cols) {
            // `restore = true`: each cell goes back to the color it was drawn in,
            // so releasing a selection over the green prompt leaves it green.
            framebuffer::console().paint_cell_selected(cell.col, cell.row, 0, 0x000000, true);
        }
        self.painted = false;
    }

    /// Drop the selection, restoring the cells it covered.
    pub fn clear(&mut self) {
        if self.painted {
            self.unpaint();
        }
        self.anchor = None;
        self.range = None;
        self.line_range = None;
        self.painted = false;
    }
}

impl Default for Selection {
    fn default() -> Self {
        Self::new()
    }
}
