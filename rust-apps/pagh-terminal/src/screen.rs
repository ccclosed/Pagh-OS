//! Terminal screen buffer: cells, cursor, scroll region, scrollback, selection
//! and DEC modes.
//!
//! The parser in [`crate::parser`] owns the byte-level state machine and calls
//! the semantic operations here.  Keeping them separate makes both sides
//! testable: parser tests feed escape sequences, while screen tests exercise
//! editing operations directly.

use std::collections::VecDeque;

use crate::cell::{default_palette, Cell, Pen, Rgb, CELL_WIDE, CELL_WIDE_CONT};
use crate::font::char_width;

/// Upper bound accepted from protocol resize messages; keeps a malicious
/// surface from asking for a multi-terabyte grid.
pub const MAX_DIMENSION: usize = 1000;
/// Default scrollback limit in lines.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorStyle {
    #[default]
    Block,
    Underline,
    Bar,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseMode {
    #[default]
    Off,
    /// Report button presses only (DECSET 1000).
    Click,
    /// Report drags while a button is held (DECSET 1002).
    Drag,
    /// Report all motion (DECSET 1003).
    Motion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMode {
    /// Character-granular selection, including partial first/last lines.
    Char,
    /// Whole-line selection.
    Line,
}

/// A selection anchor and focus in *absolute* line coordinates.  Absolute line
/// 0 is the oldest retained scrollback line, so a selection stays attached to
/// its text when more output scrolls into the scrollback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub focus: (usize, usize),
    pub mode: SelectionMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub x: usize,
    pub y: usize,
    pub visible: bool,
    pub style: CursorStyle,
    /// A character was printed in the last column; the wrap happens before the
    /// *next* printable character (VT100 deferred-wrap semantics).
    pub wrap_pending: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Cursor {
            x: 0,
            y: 0,
            visible: true,
            style: CursorStyle::Block,
            wrap_pending: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SavedCursor {
    x: usize,
    y: usize,
    pen: Pen,
    origin_mode: bool,
    charsets: [u8; 2],
    active_charset: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SavedPrivateModes {
    auto_wrap: bool,
    origin_mode: bool,
    app_cursor: bool,
    cursor_visible: bool,
    bracketed_paste: bool,
    mouse_mode: MouseMode,
    mouse_sgr: bool,
    focus_events: bool,
    reverse_video: bool,
    newline_mode: bool,
    insert_mode: bool,
    cursor_blink: bool,
}

/// The terminal buffer and all mutable emulation state.
#[derive(Clone)]
pub struct Screen {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
    /// Primary grid saved while the alternate screen is active.
    alt_backup: Option<Vec<Cell>>,
    alt_saved_cursor: Option<SavedCursor>,
    scrollback: VecDeque<Vec<Cell>>,
    max_scrollback: usize,
    /// Lines scrolled up from the bottom of the view (0 = live bottom).
    view_offset: usize,
    pub cursor: Cursor,
    pub pen: Pen,
    saved_cursor: SavedCursor,
    saved_private_modes: Option<SavedPrivateModes>,
    pub scroll_top: usize,
    pub scroll_bot: usize,
    pub origin_mode: bool,
    pub auto_wrap: bool,
    pub insert_mode: bool,
    pub newline_mode: bool,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    pub focus_events: bool,
    pub mouse_mode: MouseMode,
    pub mouse_sgr: bool,
    pub cursor_blink: bool,
    pub reverse_video: bool,
    pub synchronized_output: bool,
    pub local_echo: bool,
    pub title: String,
    pub bell_count: u64,
    pub default_fg_rgb: Rgb,
    pub default_bg_rgb: Rgb,
    tabstops: Vec<bool>,
    charsets: [u8; 2],
    pub active_charset: u8,
    last_printed: Option<char>,
    selection: Option<Selection>,
}

impl std::fmt::Debug for Screen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Screen")
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("cursor", &self.cursor)
            .field("scrollback", &self.scrollback.len())
            .field("view_offset", &self.view_offset)
            .field("title", &self.title)
            .finish()
    }
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Screen {
        Screen::with_scrollback(cols, rows, DEFAULT_SCROLLBACK)
    }

    pub fn with_scrollback(cols: usize, rows: usize, max_scrollback: usize) -> Screen {
        let cols = cols.clamp(1, MAX_DIMENSION);
        let rows = rows.clamp(1, MAX_DIMENSION);
        let mut screen = Screen {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            alt_backup: None,
            alt_saved_cursor: None,
            scrollback: VecDeque::new(),
            max_scrollback,
            view_offset: 0,
            cursor: Cursor::default(),
            pen: Pen::default(),
            saved_cursor: SavedCursor {
                x: 0,
                y: 0,
                pen: Pen::default(),
                origin_mode: false,
                charsets: [b'B', b'B'],
                active_charset: 0,
            },
            saved_private_modes: None,
            scroll_top: 0,
            scroll_bot: rows - 1,
            origin_mode: false,
            auto_wrap: true,
            insert_mode: false,
            newline_mode: false,
            app_cursor: false,
            bracketed_paste: false,
            focus_events: false,
            mouse_mode: MouseMode::Off,
            mouse_sgr: false,
            cursor_blink: true,
            reverse_video: false,
            synchronized_output: false,
            local_echo: true,
            title: String::new(),
            bell_count: 0,
            default_fg_rgb: Rgb(0xd8, 0xe6, 0xe3),
            default_bg_rgb: Rgb(0x0e, 0x16, 0x1b),
            tabstops: (0..cols).map(|x| x % 8 == 0).collect(),
            charsets: [b'B', b'B'],
            active_charset: 0,
            last_printed: None,
            selection: None,
        };
        screen.reset_tabstops();
        screen
    }

    /// Build a screen from a kernel VT shared surface snapshot.  Used by
    /// [`crate::vtconsole::VtSnapshot::to_screen`]; invalid or missing wide
    /// continuation cells are normalized to blanks.
    pub fn from_cells(
        cols: usize,
        rows: usize,
        cells: Vec<Cell>,
        cursor: (usize, usize),
        cursor_visible: bool,
        cursor_style: CursorStyle,
    ) -> Screen {
        let mut screen = Screen::new(cols, rows);
        if cells.len() == cols * rows {
            screen.cells = cells;
        }
        for y in 0..rows {
            screen.sanitize_row(y);
        }
        screen.cursor.x = cursor.0.min(cols - 1);
        screen.cursor.y = cursor.1.min(rows - 1);
        screen.cursor.visible = cursor_visible;
        screen.cursor.style = cursor_style;
        screen.cursor.wrap_pending = false;
        screen
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn max_scrollback(&self) -> usize {
        self.max_scrollback
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    pub fn view_offset(&self) -> usize {
        self.view_offset
    }

    pub fn alt_active(&self) -> bool {
        self.alt_backup.is_some()
    }

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.cols + x
    }

    pub fn cell(&self, x: usize, y: usize) -> Cell {
        if x >= self.cols || y >= self.rows {
            return Cell::default();
        }
        self.cells[self.idx(x, y)]
    }

    pub fn row(&self, y: usize) -> &[Cell] {
        let start = y * self.cols;
        &self.cells[start..start + self.cols]
    }

    fn set_cell(&mut self, x: usize, y: usize, cell: Cell) {
        if x < self.cols && y < self.rows {
            let i = self.idx(x, y);
            self.cells[i] = cell;
        }
    }

    /// Blank cell in the current erase background (BCE).
    fn erased(&self) -> Cell {
        Cell::blank(self.pen.bg)
    }

    fn clear_cell(&mut self, x: usize, y: usize) {
        let cell = self.erased();
        if self.cell(x, y).is_wide() {
            self.set_cell(x, y, cell);
            if x + 1 < self.cols {
                self.set_cell(x + 1, y, cell);
            }
        } else if self.cell(x, y).is_wide_continuation() {
            self.set_cell(x, y, cell);
            if x > 0 {
                self.set_cell(x - 1, y, cell);
            }
        } else {
            self.set_cell(x, y, cell);
        }
    }

    fn clear_row_range(&mut self, y: usize, start: usize, end: usize) {
        let end = end.min(self.cols);
        for x in start.min(self.cols)..end {
            self.clear_cell(x, y);
        }
    }

    fn clear_row(&mut self, y: usize) {
        let blank = self.erased();
        let start = y * self.cols;
        self.cells[start..start + self.cols].fill(blank);
    }

    fn sanitize_row(&mut self, y: usize) {
        let mut x = 0;
        while x < self.cols {
            let flags = self.cell(x, y).flags;
            if flags & CELL_WIDE != 0 {
                let ok = x + 1 < self.cols && self.cell(x + 1, y).flags & CELL_WIDE_CONT != 0;
                if !ok {
                    self.clear_cell(x, y);
                    x += 1;
                } else {
                    x += 2;
                }
            } else if flags & CELL_WIDE_CONT != 0 {
                self.clear_cell(x, y);
                x += 1;
            } else {
                x += 1;
            }
        }
    }

    // ── cursor and margins ─────────────────────────────────────────────────

    fn clear_wrap(&mut self) {
        self.cursor.wrap_pending = false;
    }

    fn limit_top(&self) -> usize {
        if self.origin_mode {
            self.scroll_top
        } else {
            0
        }
    }

    fn limit_bottom(&self) -> usize {
        if self.origin_mode {
            self.scroll_bot
        } else {
            self.rows - 1
        }
    }

    pub fn cursor_home(&mut self) {
        self.cursor.x = 0;
        self.cursor.y = if self.origin_mode { self.scroll_top } else { 0 };
        self.clear_wrap();
    }

    /// Move the cursor to a zero-based row/column (CUP/HVP).  In origin mode
    /// the row is relative to the top margin.
    pub fn goto(&mut self, row: usize, col: usize) {
        let row = if self.origin_mode {
            self.scroll_top + row.min(self.scroll_bot.saturating_sub(self.scroll_top))
        } else {
            row.min(self.rows - 1)
        };
        self.cursor.y = row;
        self.cursor.x = col.min(self.cols - 1);
        self.clear_wrap();
    }

    pub fn set_column(&mut self, col: usize) {
        self.cursor.x = col.min(self.cols - 1);
        self.clear_wrap();
    }

    pub fn set_row(&mut self, row: usize) {
        let row = if self.origin_mode {
            self.scroll_top + row.min(self.scroll_bot.saturating_sub(self.scroll_top))
        } else {
            row.min(self.rows - 1)
        };
        self.cursor.y = row;
        self.clear_wrap();
    }

    pub fn move_up(&mut self, n: usize) {
        let top = self.limit_top();
        self.cursor.y = self.cursor.y.saturating_sub(n).max(top);
        self.clear_wrap();
    }

    pub fn move_down(&mut self, n: usize) {
        let bottom = self.limit_bottom();
        self.cursor.y = (self.cursor.y + n).min(bottom);
        self.clear_wrap();
    }

    pub fn move_left(&mut self, n: usize) {
        self.cursor.x = self.cursor.x.saturating_sub(n);
        self.clear_wrap();
    }

    pub fn move_right(&mut self, n: usize) {
        self.cursor.x = (self.cursor.x + n).min(self.cols - 1);
        self.clear_wrap();
    }

    pub fn next_line(&mut self, n: usize) {
        self.move_down(n);
        self.cursor.x = 0;
    }

    pub fn previous_line(&mut self, n: usize) {
        self.move_up(n);
        self.cursor.x = 0;
    }

    pub fn backspace(&mut self) {
        if self.cursor.x > 0 {
            self.cursor.x -= 1;
        }
        self.clear_wrap();
    }

    pub fn carriage_return(&mut self) {
        self.cursor.x = 0;
        self.clear_wrap();
    }

    /// Line feed without carriage return.  In newline mode (LNM) it also
    /// performs a carriage return, matching the VT100 LNM behavior.
    pub fn linefeed(&mut self) {
        self.clear_wrap();
        if self.cursor.y == self.scroll_bot {
            self.scroll_up(1);
        } else if self.cursor.y + 1 < self.rows {
            self.cursor.y += 1;
        }
        if self.newline_mode {
            self.cursor.x = 0;
        }
    }

    pub fn reverse_index(&mut self) {
        self.clear_wrap();
        if self.cursor.y == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor.y > 0 {
            self.cursor.y -= 1;
        }
    }

    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let bottom = bottom.min(self.rows - 1);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bot = bottom;
        } else {
            self.scroll_top = 0;
            self.scroll_bot = self.rows - 1;
        }
        self.cursor_home();
    }

    pub fn set_origin_mode(&mut self, enabled: bool) {
        self.origin_mode = enabled;
        self.cursor_home();
    }

    pub fn save_cursor(&mut self) {
        self.saved_cursor = SavedCursor {
            x: self.cursor.x,
            y: self.cursor.y,
            pen: self.pen,
            origin_mode: self.origin_mode,
            charsets: self.charsets,
            active_charset: self.active_charset,
        };
    }

    pub fn restore_cursor(&mut self) {
        let saved = self.saved_cursor;
        self.cursor.x = saved.x.min(self.cols - 1);
        self.cursor.y = saved.y.min(self.rows - 1);
        self.pen = saved.pen;
        self.origin_mode = saved.origin_mode;
        self.charsets = saved.charsets;
        self.active_charset = saved.active_charset;
        self.clear_wrap();
    }

    // ── tabs ───────────────────────────────────────────────────────────────

    pub fn reset_tabstops(&mut self) {
        self.tabstops = (0..self.cols).map(|x| x % 8 == 0).collect();
    }

    pub fn set_tabstop(&mut self) {
        if self.cursor.x < self.tabstops.len() {
            self.tabstops[self.cursor.x] = true;
        }
    }

    pub fn clear_tabstop(&mut self, mode: u32) {
        match mode {
            0 => {
                if self.cursor.x < self.tabstops.len() {
                    self.tabstops[self.cursor.x] = false;
                }
            }
            3 => self.tabstops.iter_mut().for_each(|t| *t = false),
            _ => {}
        }
    }

    pub fn tab_forward(&mut self, n: usize) {
        for _ in 0..n {
            let mut x = self.cursor.x + 1;
            while x < self.cols && !self.tabstops.get(x).copied().unwrap_or(false) {
                x += 1;
            }
            self.cursor.x = x.min(self.cols - 1);
        }
        self.clear_wrap();
    }

    pub fn tab_backward(&mut self, n: usize) {
        for _ in 0..n {
            let mut x = self.cursor.x;
            loop {
                if x == 0 {
                    break;
                }
                x -= 1;
                if self.tabstops.get(x).copied().unwrap_or(false) {
                    break;
                }
            }
            self.cursor.x = x;
        }
        self.clear_wrap();
    }

    // ── editing ────────────────────────────────────────────────────────────

    /// Erase display (`CSI Ps J`): 0 = cursor to end, 1 = start to cursor,
    /// 2 = all, 3 = scrollback only (xterm).
    pub fn erase_in_display(&mut self, mode: u32) {
        match mode {
            0 => {
                let cx = self.cursor.x;
                let cy = self.cursor.y;
                self.clear_row_range(cy, cx, self.cols);
                for y in cy + 1..self.rows {
                    self.clear_row(y);
                }
            }
            1 => {
                let cx = self.cursor.x;
                let cy = self.cursor.y;
                for y in 0..cy {
                    self.clear_row(y);
                }
                self.clear_row_range(cy, 0, cx + 1);
            }
            2 => {
                for y in 0..self.rows {
                    self.clear_row(y);
                }
            }
            3 => {
                self.scrollback.clear();
                self.view_offset = 0;
                self.selection = None;
            }
            _ => {}
        }
    }

    /// Erase line (`CSI Ps K`): 0 = cursor to end, 1 = start to cursor, 2 = all.
    pub fn erase_in_line(&mut self, mode: u32) {
        match mode {
            0 => {
                let (x, y) = (self.cursor.x, self.cursor.y);
                self.clear_row_range(y, x, self.cols)
            }
            1 => {
                let (x, y) = (self.cursor.x, self.cursor.y);
                self.clear_row_range(y, 0, x + 1)
            }
            2 => self.clear_row(self.cursor.y),
            _ => {}
        }
    }

    pub fn erase_chars(&mut self, n: usize) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        let end = (x + n).min(self.cols);
        self.clear_row_range(y, x, end);
    }

    pub fn insert_chars(&mut self, n: usize) {
        let count = self.cols - self.cursor.x;
        let n = n.min(count);
        if n == 0 {
            return;
        }
        let (x, y) = (self.cursor.x, self.cursor.y);
        let start = self.idx(x, y);
        if n < count {
            self.cells.copy_within(start..start + count - n, start + n);
        }
        let blank = self.erased();
        self.cells[start..start + n].fill(blank);
        self.sanitize_row(y);
    }

    pub fn delete_chars(&mut self, n: usize) {
        let count = self.cols - self.cursor.x;
        let n = n.min(count);
        if n == 0 {
            return;
        }
        let (x, y) = (self.cursor.x, self.cursor.y);
        let start = self.idx(x, y);
        if n < count {
            self.cells.copy_within(start + n..start + count, start);
        }
        let blank = self.erased();
        self.cells[start + count - n..start + count].fill(blank);
        self.sanitize_row(y);
    }

    pub fn insert_lines(&mut self, n: usize) {
        if self.cursor.y < self.scroll_top || self.cursor.y > self.scroll_bot {
            return;
        }
        let n = n.min(self.scroll_bot - self.cursor.y + 1);
        let top = self.idx(0, self.cursor.y);
        let bot = self.idx(0, self.scroll_bot + 1);
        let shift = n * self.cols;
        self.cells.copy_within(top..bot - shift, top + shift);
        let blank = self.erased();
        self.cells[top..top + shift].fill(blank);
    }

    pub fn delete_lines(&mut self, n: usize) {
        if self.cursor.y < self.scroll_top || self.cursor.y > self.scroll_bot {
            return;
        }
        let n = n.min(self.scroll_bot - self.cursor.y + 1);
        let top = self.idx(0, self.cursor.y);
        let bot = self.idx(0, self.scroll_bot + 1);
        let shift = n * self.cols;
        self.cells.copy_within(top + shift..bot, top);
        let blank = self.erased();
        self.cells[bot - shift..bot].fill(blank);
    }

    /// Scroll the region up, pushing full-screen primary lines into scrollback.
    pub fn scroll_up(&mut self, n: usize) {
        if self.scroll_top > self.scroll_bot || self.scroll_bot >= self.rows {
            return;
        }
        let height = self.scroll_bot - self.scroll_top + 1;
        let n = n.min(height);
        if n == 0 {
            return;
        }
        let push_scrollback =
            self.alt_backup.is_none() && self.scroll_top == 0 && self.scroll_bot == self.rows - 1;
        for _ in 0..n {
            if push_scrollback {
                let line = self.cells[..self.cols].to_vec();
                self.push_scrollback(line);
            }
            let top = self.idx(0, self.scroll_top);
            let bot = self.idx(0, self.scroll_bot + 1);
            let shift = self.cols;
            if bot >= top + shift {
                self.cells.copy_within(top + shift..bot, top);
            }
            let last = self.idx(0, self.scroll_bot);
            let blank = self.erased();
            self.cells[last..last + self.cols].fill(blank);
        }
    }

    pub fn scroll_down(&mut self, n: usize) {
        if self.scroll_top > self.scroll_bot || self.scroll_bot >= self.rows {
            return;
        }
        let height = self.scroll_bot - self.scroll_top + 1;
        let n = n.min(height);
        for _ in 0..n {
            let top = self.idx(0, self.scroll_top);
            let bot = self.idx(0, self.scroll_bot + 1);
            let shift = self.cols;
            self.cells.copy_within(top..bot - shift, top + shift);
            let blank = self.erased();
            self.cells[top..top + self.cols].fill(blank);
        }
    }

    fn push_scrollback(&mut self, line: Vec<Cell>) {
        if self.max_scrollback == 0 || self.alt_backup.is_some() {
            return;
        }
        if self.scrollback.len() == self.max_scrollback {
            self.scrollback.pop_front();
            if let Some(sel) = &mut self.selection {
                sel.anchor.0 = sel.anchor.0.saturating_sub(1);
                sel.focus.0 = sel.focus.0.saturating_sub(1);
                if sel.anchor.0 == 0 && sel.focus.0 == 0 && sel.anchor.1 == 0 && sel.focus.1 == 0 {
                    self.selection = None;
                }
            }
        }
        self.scrollback.push_back(line);
        if self.view_offset > 0 {
            self.view_offset = (self.view_offset + 1).min(self.scrollback.len());
        }
    }

    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
        self.view_offset = 0;
    }

    // ── writing ────────────────────────────────────────────────────────────

    /// Write one printable character, handling wide cells, insert mode and
    /// deferred autowrap.
    pub fn write_char(&mut self, ch: char) {
        let width = char_width(ch);
        if width == 0 {
            // Combining marks and format controls are accepted but not
            // composed (documented limitation; see TERMINAL.md).
            return;
        }
        if width > self.cols {
            return;
        }
        if self.cursor.wrap_pending && self.auto_wrap {
            self.cursor.x = 0;
            self.linefeed();
            self.cursor.wrap_pending = false;
        }
        if self.cursor.x + width > self.cols {
            if self.auto_wrap {
                self.cursor.x = 0;
                self.linefeed();
            } else {
                self.cursor.x = self.cols - 1;
                return;
            }
        }
        if self.cursor.x + width > self.cols {
            return;
        }
        if self.insert_mode {
            self.insert_chars(width);
        }
        let x = self.cursor.x;
        let y = self.cursor.y;
        let flags = if width == 2 { CELL_WIDE } else { 0 };
        self.set_cell(x, y, self.pen.cell(ch, flags));
        if width == 2 {
            self.set_cell(x + 1, y, self.pen.cell(' ', CELL_WIDE_CONT));
        }
        self.cursor.x += width;
        self.last_printed = Some(ch);
        if self.cursor.x >= self.cols {
            self.cursor.x = self.cols - 1;
            self.cursor.wrap_pending = self.auto_wrap;
        }
    }

    /// REP (`CSI Ps b`): repeat the last printed graphic character.
    pub fn repeat_last_char(&mut self, n: usize) {
        if let Some(ch) = self.last_printed {
            for _ in 0..n {
                self.write_char(ch);
            }
        }
    }

    /// DECALN (`ESC # 8`): fill the screen with `E`, reset margins.
    pub fn fill_alignment(&mut self) {
        let cell = Cell {
            ch: 'E',
            ..Cell::default()
        };
        self.cells.fill(cell);
        self.scroll_top = 0;
        self.scroll_bot = self.rows - 1;
        self.cursor_home();
    }

    pub fn set_pen(&mut self, pen: Pen) {
        self.pen = pen;
    }

    pub fn reset_pen(&mut self) {
        self.pen = Pen::default();
    }

    pub fn set_default_colors(&mut self, fg: Rgb, bg: Rgb) {
        self.default_fg_rgb = fg;
        self.default_bg_rgb = bg;
    }

    pub fn set_title(&mut self, title: String) {
        self.title = title;
    }

    pub fn set_charset(&mut self, slot: u8, designator: u8) {
        if (slot as usize) < 2 {
            self.charsets[slot as usize] = designator;
        }
    }

    pub fn shift_charset(&mut self, slot: u8) {
        if slot < 2 {
            self.active_charset = slot;
        }
    }

    /// Map a printable ASCII character through the active G0/G1 charset.
    pub fn map_charset(&self, ch: char) -> char {
        if !ch.is_ascii() {
            return ch;
        }
        let designator = self.charsets[self.active_charset as usize];
        match designator {
            b'0' => dec_special_graphics(ch).unwrap_or(ch),
            b'A' => {
                if ch == '#' {
                    '£'
                } else {
                    ch
                }
            }
            _ => ch,
        }
    }

    // ── modes ──────────────────────────────────────────────────────────────

    pub fn set_mode(&mut self, private: bool, mode: u32, enable: bool) -> bool {
        if private {
            match mode {
                1 => self.app_cursor = enable,
                5 => self.reverse_video = enable,
                6 => self.set_origin_mode(enable),
                7 => self.auto_wrap = enable,
                12 => self.cursor_blink = enable,
                25 => self.cursor.visible = enable,
                47 => {
                    if enable {
                        self.enter_alt_screen(false);
                    } else {
                        self.leave_alt_screen();
                    }
                }
                1047 => {
                    if enable {
                        self.enter_alt_screen(true);
                    } else {
                        self.leave_alt_screen();
                    }
                }
                1048 => {
                    if enable {
                        self.save_cursor();
                    } else {
                        self.restore_cursor();
                    }
                }
                1049 => {
                    if enable {
                        self.enter_alt_screen(true);
                    } else {
                        self.leave_alt_screen();
                    }
                }
                1000 => {
                    self.mouse_mode = if enable {
                        MouseMode::Click
                    } else {
                        MouseMode::Off
                    }
                }
                1002 => {
                    self.mouse_mode = if enable {
                        MouseMode::Drag
                    } else {
                        MouseMode::Off
                    }
                }
                1003 => {
                    self.mouse_mode = if enable {
                        MouseMode::Motion
                    } else {
                        MouseMode::Off
                    }
                }
                1005 => {} // UTF-8 mouse encoding is not implemented; SGR preferred
                1006 => self.mouse_sgr = enable,
                1004 => self.focus_events = enable,
                2004 => self.bracketed_paste = enable,
                2026 => self.synchronized_output = enable,
                _ => return false,
            }
        } else {
            match mode {
                4 => self.insert_mode = enable,
                20 => self.newline_mode = enable,
                12 => self.local_echo = enable,
                _ => return false,
            }
        }
        true
    }

    /// DECRQM state: 1 = set, 2 = reset, 0 = not recognized (None).
    pub fn mode_state(&self, private: bool, mode: u32) -> Option<u8> {
        let set = |v: bool| Some(if v { 1 } else { 2 });
        if private {
            match mode {
                1 => set(self.app_cursor),
                5 => set(self.reverse_video),
                6 => set(self.origin_mode),
                7 => set(self.auto_wrap),
                12 => set(self.cursor_blink),
                25 => set(self.cursor.visible),
                47 | 1047 | 1049 => set(self.alt_active()),
                1000 | 1002 | 1003 => set(self.mouse_mode != MouseMode::Off),
                1006 => set(self.mouse_sgr),
                1004 => set(self.focus_events),
                2004 => set(self.bracketed_paste),
                2026 => set(self.synchronized_output),
                _ => None,
            }
        } else {
            match mode {
                4 => set(self.insert_mode),
                20 => set(self.newline_mode),
                _ => None,
            }
        }
    }

    fn save_private_modes(&mut self) {
        self.saved_private_modes = Some(SavedPrivateModes {
            auto_wrap: self.auto_wrap,
            origin_mode: self.origin_mode,
            app_cursor: self.app_cursor,
            cursor_visible: self.cursor.visible,
            bracketed_paste: self.bracketed_paste,
            mouse_mode: self.mouse_mode,
            mouse_sgr: self.mouse_sgr,
            focus_events: self.focus_events,
            reverse_video: self.reverse_video,
            newline_mode: self.newline_mode,
            insert_mode: self.insert_mode,
            cursor_blink: self.cursor_blink,
        });
    }

    fn restore_private_modes(&mut self) {
        if let Some(m) = self.saved_private_modes {
            self.auto_wrap = m.auto_wrap;
            self.origin_mode = m.origin_mode;
            self.app_cursor = m.app_cursor;
            self.cursor.visible = m.cursor_visible;
            self.bracketed_paste = m.bracketed_paste;
            self.mouse_mode = m.mouse_mode;
            self.mouse_sgr = m.mouse_sgr;
            self.focus_events = m.focus_events;
            self.reverse_video = m.reverse_video;
            self.newline_mode = m.newline_mode;
            self.insert_mode = m.insert_mode;
            self.cursor_blink = m.cursor_blink;
        }
    }

    pub fn save_private_modes_cmd(&mut self) {
        self.save_private_modes();
    }

    pub fn restore_private_modes_cmd(&mut self) {
        self.restore_private_modes();
    }

    fn enter_alt_screen(&mut self, clear: bool) {
        if self.alt_backup.is_some() {
            if clear {
                let blank = Cell::default();
                self.cells.fill(blank);
            }
            return;
        }
        self.alt_saved_cursor = Some(self.saved_cursor_from_current());
        self.alt_backup = Some(std::mem::take(&mut self.cells));
        self.cells = vec![Cell::default(); self.cols * self.rows];
        self.cursor.x = 0;
        self.cursor.y = 0;
        self.cursor.wrap_pending = false;
        self.selection = None;
        self.view_offset = 0;
    }

    fn leave_alt_screen(&mut self) {
        if let Some(primary) = self.alt_backup.take() {
            self.cells = primary;
            if let Some(saved) = self.alt_saved_cursor.take() {
                self.cursor.x = saved.x.min(self.cols - 1);
                self.cursor.y = saved.y.min(self.rows - 1);
                self.pen = saved.pen;
                self.origin_mode = saved.origin_mode;
                self.charsets = saved.charsets;
                self.active_charset = saved.active_charset;
            }
            self.cursor.wrap_pending = false;
            self.selection = None;
            self.view_offset = 0;
        }
    }

    fn saved_cursor_from_current(&self) -> SavedCursor {
        SavedCursor {
            x: self.cursor.x,
            y: self.cursor.y,
            pen: self.pen,
            origin_mode: self.origin_mode,
            charsets: self.charsets,
            active_charset: self.active_charset,
        }
    }

    /// RIS (`ESC c`): reset emulation state, keep the window size.
    pub fn reset(&mut self) {
        self.cells.fill(Cell::default());
        self.scrollback.clear();
        self.view_offset = 0;
        self.cursor = Cursor::default();
        self.pen = Pen::default();
        self.saved_cursor = SavedCursor {
            x: 0,
            y: 0,
            pen: Pen::default(),
            origin_mode: false,
            charsets: [b'B', b'B'],
            active_charset: 0,
        };
        self.saved_private_modes = None;
        self.scroll_top = 0;
        self.scroll_bot = self.rows - 1;
        self.origin_mode = false;
        self.auto_wrap = true;
        self.insert_mode = false;
        self.newline_mode = false;
        self.app_cursor = false;
        self.bracketed_paste = false;
        self.focus_events = false;
        self.mouse_mode = MouseMode::Off;
        self.mouse_sgr = false;
        self.cursor_blink = true;
        self.reverse_video = false;
        self.synchronized_output = false;
        self.local_echo = true;
        self.title.clear();
        self.alt_backup = None;
        self.alt_saved_cursor = None;
        self.active_charset = 0;
        self.charsets = [b'B', b'B'];
        self.last_printed = None;
        self.selection = None;
        self.bell_count = 0;
        self.reset_tabstops();
    }

    // ── view and scrollback ────────────────────────────────────────────────

    pub fn set_view_offset(&mut self, offset: usize) {
        self.view_offset = offset.min(self.scrollback.len());
    }

    /// Positive `delta` scrolls towards older lines, negative towards the live
    /// bottom.
    pub fn scroll_view(&mut self, delta: isize) {
        let next = (self.view_offset as isize + delta).clamp(0, self.scrollback.len() as isize);
        self.view_offset = next as usize;
    }

    pub fn view_top_absolute(&self) -> usize {
        if self.alt_active() {
            return 0;
        }
        self.absolute_line_count()
            .saturating_sub(self.rows)
            .saturating_sub(self.view_offset)
    }

    /// Total lines addressable by absolute coordinates (scrollback + grid).
    pub fn absolute_line_count(&self) -> usize {
        if self.alt_active() {
            self.rows
        } else {
            self.scrollback.len() + self.rows
        }
    }

    pub fn absolute_line(&self, view_row: usize) -> usize {
        (self.view_top_absolute() + view_row).min(self.absolute_line_count() - 1)
    }

    pub fn line_at_absolute(&self, line: usize) -> Option<&[Cell]> {
        if self.alt_active() {
            return if line < self.rows {
                Some(self.row(line))
            } else {
                None
            };
        }
        if line < self.scrollback.len() {
            self.scrollback.get(line).map(|v| v.as_slice())
        } else {
            let row = line - self.scrollback.len();
            if row < self.rows {
                Some(self.row(row))
            } else {
                None
            }
        }
    }

    /// The line shown at viewport row `view_row` with the current scroll offset.
    pub fn view_line(&self, view_row: usize) -> &[Cell] {
        let abs = self.absolute_line(view_row);
        self.line_at_absolute(abs).unwrap_or(&[])
    }

    pub fn cursor_in_view(&self) -> bool {
        self.view_offset == 0
    }

    pub fn text(&self) -> String {
        let mut lines: Vec<String> = (0..self.rows).map(|y| self.line_text(y)).collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    pub fn full_text(&self) -> String {
        let mut lines: Vec<String> = self
            .scrollback
            .iter()
            .map(|line| cells_text(line))
            .collect();
        for y in 0..self.rows {
            lines.push(self.line_text(y));
        }
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    pub fn view_text(&self) -> String {
        let mut lines: Vec<String> = (0..self.rows)
            .map(|y| cells_text(self.view_line(y)))
            .collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Text of one grid row, trailing blanks trimmed.
    pub fn line_text(&self, y: usize) -> String {
        if y >= self.rows {
            return String::new();
        }
        cells_text(self.row(y))
    }

    // ── selection ──────────────────────────────────────────────────────────

    pub fn begin_selection_view(&mut self, view_row: usize, col: usize, mode: SelectionMode) {
        let line = self.absolute_line(view_row);
        self.begin_selection_abs(line, col, mode);
    }

    pub fn begin_selection_abs(&mut self, line: usize, col: usize, mode: SelectionMode) {
        let line = line.min(self.absolute_line_count().saturating_sub(1));
        let col = col.min(self.cols - 1);
        self.selection = Some(Selection {
            anchor: (line, col),
            focus: (line, col),
            mode,
        });
    }

    pub fn extend_selection_view(&mut self, view_row: usize, col: usize) {
        let line = self.absolute_line(view_row);
        self.extend_selection_abs(line, col);
    }

    pub fn extend_selection_abs(&mut self, line: usize, col: usize) {
        let line = line.min(self.absolute_line_count().saturating_sub(1));
        let col = col.min(self.cols - 1);
        if let Some(sel) = &mut self.selection {
            sel.focus = (line, col);
        }
    }

    pub fn select_word_view(&mut self, view_row: usize, col: usize) {
        let line = self.absolute_line(view_row);
        let Some(cells) = self.line_at_absolute(line) else {
            return;
        };
        let col = col.min(cells.len().saturating_sub(1));
        let is_word = |c: char| {
            c.is_alphanumeric() || c == '_' || (!c.is_whitespace() && !c.is_ascii_punctuation())
        };
        let mut start = col;
        while start > 0 && is_word(cells[start - 1].ch) {
            start -= 1;
        }
        let mut end = col;
        while end + 1 < cells.len() && is_word(cells[end + 1].ch) {
            end += 1;
        }
        self.selection = Some(Selection {
            anchor: (line, start),
            focus: (line, end),
            mode: SelectionMode::Char,
        });
    }

    pub fn select_line_view(&mut self, view_row: usize) {
        let line = self.absolute_line(view_row);
        self.selection = Some(Selection {
            anchor: (line, 0),
            focus: (line, self.cols - 1),
            mode: SelectionMode::Line,
        });
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn selection_bounds(&self) -> Option<((usize, usize), (usize, usize))> {
        let sel = self.selection?;
        if sel.anchor <= sel.focus {
            Some((sel.anchor, sel.focus))
        } else {
            Some((sel.focus, sel.anchor))
        }
    }

    /// Is the cell at absolute line/column covered by the selection?
    pub fn is_selected(&self, line: usize, col: usize) -> bool {
        let Some((start, end)) = self.selection_bounds() else {
            return false;
        };
        let sel = self.selection.unwrap();
        match sel.mode {
            SelectionMode::Line => line >= start.0 && line <= end.0,
            SelectionMode::Char => {
                if line < start.0 || line > end.0 {
                    false
                } else if start.0 == end.0 {
                    col >= start.1 && col <= end.1
                } else if line == start.0 {
                    col >= start.1
                } else if line == end.0 {
                    col <= end.1
                } else {
                    true
                }
            }
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        let (start, end) = self.selection_bounds()?;
        let sel = self.selection.unwrap();
        let mut out: Vec<String> = Vec::new();
        for line in start.0..=end.0 {
            let Some(cells) = self.line_at_absolute(line) else {
                continue;
            };
            let (from, to) = match sel.mode {
                SelectionMode::Line => (0, self.cols.saturating_sub(1)),
                SelectionMode::Char => {
                    let from = if line == start.0 { start.1 } else { 0 };
                    let to = if line == end.0 { end.1 } else { self.cols - 1 };
                    (from, to)
                }
            };
            let from = from.min(cells.len());
            let to = (to + 1).min(cells.len());
            if from < to {
                out.push(cells_text_range(cells, from, to));
            } else {
                out.push(String::new());
            }
        }
        Some(out.join("\n"))
    }

    // ── resize ─────────────────────────────────────────────────────────────

    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        let new_cols = new_cols.clamp(1, MAX_DIMENSION);
        let new_rows = new_rows.clamp(1, MAX_DIMENSION);
        if new_cols == self.cols && new_rows == self.rows {
            return;
        }

        if self.alt_active() {
            let rows = split_rows(&self.cells, self.cols, self.rows);
            let resized: Vec<Vec<Cell>> = rows
                .into_iter()
                .map(|row| resize_line(row, new_cols))
                .collect();
            self.cells = flatten_rows(&resized, new_rows, new_cols, Cell::default());
            if let Some(primary) = &mut self.alt_backup {
                let rows = split_rows(primary, self.cols, self.rows);
                let resized: Vec<Vec<Cell>> = rows
                    .into_iter()
                    .map(|row| resize_line(row, new_cols))
                    .collect();
                *primary = flatten_rows(&resized, new_rows, new_cols, Cell::default());
            }
            self.cols = new_cols;
            self.rows = new_rows;
            self.scroll_top = 0;
            self.scroll_bot = new_rows - 1;
            self.cursor.x = self.cursor.x.min(new_cols - 1);
            self.cursor.y = self.cursor.y.min(new_rows - 1);
            self.cursor.wrap_pending = false;
            self.selection = None;
            self.view_offset = 0;
            self.reset_tabstops();
            return;
        }

        // Resize scrollback and grid rows to the new width first.
        for line in self.scrollback.iter_mut() {
            *line = resize_line(std::mem::take(line), new_cols);
        }
        let mut rows: Vec<Vec<Cell>> = split_rows(&self.cells, self.cols, self.rows)
            .into_iter()
            .map(|row| resize_line(row, new_cols))
            .collect();

        let mut cursor_y = self.cursor.y;
        if new_rows < self.rows {
            let remove = self.rows - new_rows;
            for _ in 0..remove {
                let line = rows.remove(0);
                self.push_scrollback(line);
            }
            cursor_y = cursor_y.saturating_sub(remove);
        } else if new_rows > self.rows {
            let add = new_rows - self.rows;
            let available = self.scrollback.len().min(add);
            let mut prefix: Vec<Vec<Cell>> = Vec::with_capacity(available);
            for _ in 0..available {
                if let Some(line) = self.scrollback.pop_back() {
                    prefix.push(line);
                }
            }
            prefix.reverse();
            prefix.append(&mut rows);
            rows = prefix;
            cursor_y += available;
        }

        self.cells = flatten_rows(&rows, new_rows, new_cols, Cell::default());
        self.cols = new_cols;
        self.rows = new_rows;
        self.scroll_top = 0;
        self.scroll_bot = self.rows - 1;
        self.cursor.x = self.cursor.x.min(self.cols - 1);
        self.cursor.y = cursor_y.min(self.rows - 1);
        self.cursor.wrap_pending = false;
        self.view_offset = self.view_offset.min(self.scrollback.len());
        self.selection = None;
        self.reset_tabstops();
    }
}

fn split_rows(cells: &[Cell], cols: usize, rows: usize) -> Vec<Vec<Cell>> {
    if cols == 0 {
        return Vec::new();
    }
    cells
        .chunks(cols)
        .take(rows)
        .map(|chunk| chunk.to_vec())
        .collect()
}

fn resize_line(mut line: Vec<Cell>, new_cols: usize) -> Vec<Cell> {
    let old = line.len();
    if old == new_cols {
        return line;
    }
    if old > new_cols {
        line.truncate(new_cols);
    } else {
        line.resize(new_cols, Cell::default());
    }
    // A truncated wide pair must not leave a dangling continuation cell.
    let mut x = 0;
    while x < line.len() {
        if line[x].is_wide() {
            if x + 1 >= line.len() || !line[x + 1].is_wide_continuation() {
                line[x] = Cell::default();
            } else {
                x += 2;
                continue;
            }
        } else if line[x].is_wide_continuation() {
            line[x] = Cell::default();
        }
        x += 1;
    }
    line
}

fn flatten_rows(rows: &[Vec<Cell>], want_rows: usize, cols: usize, blank: Cell) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(want_rows * cols);
    for row in rows.iter().take(want_rows) {
        if row.len() == cols {
            cells.extend_from_slice(row);
        } else {
            cells.extend(row.iter().copied().take(cols));
            cells.resize(
                cells.len() + cols.saturating_sub(row.len().min(cols)),
                blank,
            );
        }
    }
    while cells.len() < want_rows * cols {
        cells.push(blank);
    }
    cells
}

fn cells_text(cells: &[Cell]) -> String {
    cells_text_range(cells, 0, cells.len())
}

fn cells_text_range(cells: &[Cell], start: usize, end: usize) -> String {
    let mut s = String::new();
    for cell in cells
        .iter()
        .take(end.min(cells.len()))
        .skip(start.min(cells.len()))
    {
        if cell.is_wide_continuation() {
            continue;
        }
        match cell.ch {
            '\0' => s.push(' '),
            ch => s.push(ch),
        }
    }
    s.trim_end().to_string()
}

/// DEC Special Graphics (line-drawing) mapping used by `ESC ( 0`.
fn dec_special_graphics(ch: char) -> Option<char> {
    Some(match ch {
        '`' => '◆',
        'a' => '▒',
        'b' => '␉',
        'c' => '␌',
        'd' => '␍',
        'e' => '␊',
        'f' => '°',
        'g' => '±',
        'h' => '␤',
        'i' => '␋',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        _ => return None,
    })
}

/// Convenience: the default palette used by the renderer.
pub fn palette_256() -> [Rgb; 256] {
    default_palette()
}

impl Default for Screen {
    fn default() -> Self {
        Screen::new(80, 24)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_text_and_wraps_at_the_last_column() {
        let mut s = Screen::new(5, 2);
        for ch in "abcde".chars() {
            s.write_char(ch);
        }
        assert_eq!(s.cursor.x, 4);
        assert!(s.cursor.wrap_pending);
        s.write_char('f');
        assert_eq!(s.line_text(0), "abcde");
        assert_eq!(s.line_text(1), "f");
        assert_eq!((s.cursor.x, s.cursor.y), (1, 1));
    }

    #[test]
    fn wide_character_occupies_two_cells() {
        let mut s = Screen::new(6, 2);
        s.write_char('界');
        assert!(s.cell(0, 0).is_wide());
        assert!(s.cell(1, 0).is_wide_continuation());
        assert_eq!(s.cursor.x, 2);
        assert_eq!(s.line_text(0), "界");
    }

    #[test]
    fn scrollback_keeps_evicted_lines() {
        let mut s = Screen::new(4, 2);
        s.write_char('a');
        s.linefeed();
        s.carriage_return();
        s.write_char('b');
        s.linefeed();
        s.carriage_return();
        s.write_char('c');
        assert_eq!(s.scrollback_len(), 1);
        assert_eq!(s.line_text(0), "b");
        assert_eq!(s.line_text(1), "c");
        s.set_view_offset(1);
        assert_eq!(s.view_text(), "a\nb");
    }

    #[test]
    fn selection_text_roundtrip() {
        let mut s = Screen::new(6, 3);
        for (y, text) in ["hello", "world", "!"].iter().enumerate() {
            for (x, ch) in text.chars().enumerate() {
                s.set_cell(
                    x,
                    y,
                    Cell {
                        ch,
                        ..Cell::default()
                    },
                );
            }
        }
        s.begin_selection_abs(0, 1, SelectionMode::Char);
        s.extend_selection_abs(1, 2);
        assert_eq!(s.selection_text().as_deref(), Some("ello\nwor"));
        s.begin_selection_abs(2, 0, SelectionMode::Line);
        assert_eq!(s.selection_text().as_deref(), Some("!"));
    }

    #[test]
    fn resize_keeps_content_and_scrollback() {
        let mut s = Screen::new(4, 3);
        for (y, text) in ["one", "two", "tri"].iter().enumerate() {
            for (x, ch) in text.chars().enumerate() {
                s.set_cell(
                    x,
                    y,
                    Cell {
                        ch,
                        ..Cell::default()
                    },
                );
            }
        }
        s.resize(6, 2);
        assert_eq!(s.scrollback_len(), 1);
        assert_eq!(s.line_text(0), "two");
        assert_eq!(s.line_text(1), "tri");
        s.resize(6, 4);
        assert_eq!(s.scrollback_len(), 0);
        assert_eq!(s.line_text(0), "one");
        assert_eq!(s.line_text(1), "two");
        assert_eq!(s.line_text(2), "tri");
    }

    #[test]
    fn alt_screen_restores_primary() {
        let mut s = Screen::new(6, 2);
        for (x, ch) in "main".chars().enumerate() {
            s.set_cell(
                x,
                0,
                Cell {
                    ch,
                    ..Cell::default()
                },
            );
        }
        s.cursor.x = 2;
        s.set_mode(true, 1049, true);
        assert!(s.alt_active());
        for (x, ch) in "alt".chars().enumerate() {
            s.set_cell(
                x,
                0,
                Cell {
                    ch,
                    ..Cell::default()
                },
            );
        }
        s.set_mode(true, 1049, false);
        assert!(!s.alt_active());
        assert_eq!(s.line_text(0), "main");
        assert_eq!(s.cursor.x, 2);
    }

    #[test]
    fn tabs_follow_stops() {
        let mut s = Screen::new(30, 2);
        s.tab_forward(1);
        assert_eq!(s.cursor.x, 8);
        s.tab_forward(1);
        assert_eq!(s.cursor.x, 16);
        s.set_column(24);
        s.clear_tabstop(0);
        s.set_column(0);
        s.tab_forward(2);
        assert_eq!(s.cursor.x, 16);
        s.set_column(8);
        s.tab_forward(1);
        assert_eq!(s.cursor.x, 16);
    }
}
