//! `nano+` — a small full-screen text editor for the framebuffer shell.
//!
//! Compared with classic nano it adds line numbers, a persistent status bar,
//! bounded undo/redo, incremental search, go-to-line, horizontal scrolling and
//! a dirty-file quit guard. Input remains ASCII because the PS/2 keyboard map is
//! ASCII; existing UTF-8 bytes are displayed lossily rather than split unsafely.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::keys::{Decoder, KeyEvent};
use super::nano_config::NanoConfig;
use crate::drivers::{cursor, framebuffer};
use crate::vfs::{VfsError, VfsNode};

const MAX_FILE: usize = 64 * 1024;
const MAX_LINE: usize = 4096;
const UNDO_LIMIT: usize = 32;
// Cell geometry comes from the console and nowhere else. These were `8`/`16`
// while the console rendered 8x16 glyphs; after the switch to the Fira Code
// 8x18 cell the editor kept laying every row out at 16 pixels and drew each row
// two pixels into the next one (and its caret was two pixels short).
const CHAR_W: usize = framebuffer::CHAR_WIDTH;
const CHAR_H: usize = framebuffer::CHAR_HEIGHT;
/// Title strip: one glyph row plus a small margin.
const HEADER_H: usize = CHAR_H + 4;
/// Footer: two text lines (status, key hints) plus a small margin.
const FOOTER_H: usize = 2 * CHAR_H + 2;
/// Sidebar width in character cells.
const TREE_COLS: usize = 24;

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptMode {
    Search,
    Goto,
    ReplaceFind,
    ReplaceWith,
}

/// A position in the document: a line and a byte column in it.
///
/// Columns are "just past the last character" at `line.len()`, so a selection can
/// end at the end of a line without pointing at a character.
#[derive(Clone, Copy, PartialEq, Eq)]
struct SelPoint {
    row: usize,
    col: usize,
}

/// A text selection in document coordinates.
///
/// `anchor` is where a drag started (or one end of a keyboard selection) and stays
/// put; `head` is the moving end. Both are kept in the order the user created them
/// and normalised by [`Self::ordered`] wherever reading order matters, so a drag in
/// any direction selects the same text.
#[derive(Clone, Copy)]
struct Selection {
    anchor: Option<SelPoint>,
    head: Option<SelPoint>,
}

impl Selection {
    const fn new() -> Self {
        Selection {
            anchor: None,
            head: None,
        }
    }

    /// The two ends in reading order, or `None` when no selection exists.
    fn ordered(&self) -> Option<(SelPoint, SelPoint)> {
        let (a, h) = (self.anchor?, self.head?);
        if (a.row, a.col) <= (h.row, h.col) {
            Some((a, h))
        } else {
            Some((h, a))
        }
    }

    fn is_empty(&self) -> bool {
        match self.ordered() {
            Some((a, b)) => a == b,
            None => true,
        }
    }

    fn clear(&mut self) {
        self.anchor = None;
        self.head = None;
    }
}

/// Where a mouse click landed.
enum Hit {
    /// A document position in the text pane.
    Text(SelPoint),
    /// A row index into the sidebar's visible list.
    Tree(usize),
}

pub struct Editor {
    path: String,
    lines: Vec<String>,
    row: usize,
    col: usize,
    top: usize,
    left: usize,
    dirty: bool,
    status: String,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    prompt: Option<(PromptMode, String)>,
    search_hit: Option<(usize, usize, usize)>,
    quit_armed: bool,
    config: NanoConfig,
    clipboard: Vec<String>,
    replace_needle: Option<String>,
    /// Directory the sidebar should open at, when the editor starts with one.
    tree_root: Option<String>,
    /// The buffer as of the last save/open, so "is this modified?" is answered by the
    /// text itself instead of by "did a keystroke happen".
    ///
    /// A counter of edits is what marked the file dirty after typing a character and
    /// deleting it again: the buffer was identical, the flag was not. Comparing the
    /// lines is the only answer that cannot lie about it.
    clean: Vec<String>,
    /// Active text selection (mouse drag or `^A`).
    sel: Selection,
    /// Clipboard for a copied/cut *selection*, as opposed to the line-oriented
    /// clipboard `^C`/`^K` use when nothing is selected.
    sel_clip: String,
}

impl Editor {
    fn new(path: &str, text: &str, config: NanoConfig) -> Self {
        let mut lines: Vec<String> = text.split('\n').map(sanitize_line).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        let clean = lines.clone();
        Self {
            path: path.to_string(),
            lines,
            clean,
            row: 0,
            col: 0,
            top: 0,
            left: 0,
            dirty: false,
            status: "Ready".to_string(),
            undo: Vec::new(),
            redo: Vec::new(),
            prompt: None,
            search_hit: None,
            quit_armed: false,
            config,
            clipboard: Vec::new(),
            tree_root: None,
            replace_needle: None,
            sel: Selection::new(),
            sel_clip: String::new(),
        }
    }

    /// Point this editor at a different file, keeping the configuration.
    ///
    /// Undo history is dropped deliberately: it belongs to the previous file, and
    /// keeping it would let an undo in one file rewrite another.
    fn reload(&mut self, path: &str, text: &str) {
        self.path = path.to_string();
        self.lines = {
            let mut l: Vec<String> = text.split('\n').map(sanitize_line).collect();
            if l.is_empty() {
                l.push(String::new());
            }
            l
        };
        self.row = 0;
        self.col = 0;
        self.top = 0;
        self.left = 0;
        self.mark_clean();
        self.undo.clear();
        self.redo.clear();
        self.search_hit = None;
        self.quit_armed = false;
        self.sel.clear();
        self.sel_clip.clear();
        self.status = alloc::format!("opened {}", path);
    }

    /// Recompute `dirty` from the buffer — the only honest definition of "modified".
    fn refresh_dirty(&mut self) {
        self.dirty = self.lines != self.clean;
        if !self.dirty {
            // Back to the saved text: there is nothing to lose, so the two-step quit
            // guard has nothing to guard either.
            self.quit_armed = false;
        }
    }

    /// Remember the current buffer as the saved state (open, reload, save).
    fn mark_clean(&mut self) {
        self.clean = self.lines.clone();
        self.dirty = false;
        self.quit_armed = false;
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            row: self.row,
            col: self.col,
        }
    }
    fn checkpoint(&mut self) {
        if self.undo.len() == UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.undo.push(self.snapshot());
        self.redo.clear();
        // `dirty` is decided by the text, not by the keystroke: the caller mutates and
        // then calls `refresh_dirty`.
        self.quit_armed = false;
    }
    fn restore(&mut self, s: Snapshot) {
        self.lines = s.lines;
        self.row = s.row.min(self.lines.len().saturating_sub(1));
        self.col = s.col.min(self.lines[self.row].len());
        self.ensure_visible();
    }
    fn undo(&mut self) {
        if let Some(s) = self.undo.pop() {
            let cur = self.snapshot();
            self.redo.push(cur);
            self.restore(s);
            self.refresh_dirty();
            self.status = "Undo".into();
        } else {
            self.status = "Nothing to undo".into();
        }
    }
    fn redo(&mut self) {
        if let Some(s) = self.redo.pop() {
            let cur = self.snapshot();
            self.undo.push(cur);
            self.restore(s);
            self.refresh_dirty();
            self.status = "Redo".into();
        } else {
            self.status = "Nothing to redo".into();
        }
    }
    fn insert(&mut self, ch: char) {
        if self.lines[self.row].len() >= MAX_LINE {
            self.status = "Line length limit reached".into();
            return;
        }
        self.checkpoint();
        self.lines[self.row].insert(self.col, ch);
        self.col += ch.len_utf8();
        if self.config.wrap_column > 0 && self.col >= self.config.wrap_column {
            self.newline_without_checkpoint();
        }
        self.ensure_visible();
        self.refresh_dirty();
    }
    fn newline_without_checkpoint(&mut self) {
        let indent = if self.config.auto_indent {
            self.lines[self.row]
                .bytes()
                .take_while(|b| *b == b' ')
                .count()
        } else {
            0
        };
        let tail = self.lines[self.row].split_off(self.col);
        self.row += 1;
        let mut next = String::new();
        for _ in 0..indent {
            next.push(' ')
        }
        next.push_str(&tail);
        self.lines.insert(self.row, next);
        self.col = indent;
        self.ensure_visible();
    }
    fn newline(&mut self) {
        self.checkpoint();
        self.newline_without_checkpoint();
        self.refresh_dirty();
    }
    fn backspace(&mut self) {
        if self.col > 0 {
            self.checkpoint();
            self.col -= 1;
            self.lines[self.row].remove(self.col);
        } else if self.row > 0 {
            self.checkpoint();
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            if self.col + current.len() <= MAX_LINE {
                self.lines[self.row].push_str(&current);
            } else {
                self.lines.insert(self.row + 1, current);
                self.row += 1;
                self.col = 0;
            }
        }
        self.ensure_visible();
        self.refresh_dirty();
    }
    fn delete(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.checkpoint();
            self.lines[self.row].remove(self.col);
        } else if self.row + 1 < self.lines.len()
            && self.lines[self.row].len() + self.lines[self.row + 1].len() <= MAX_LINE
        {
            self.checkpoint();
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
        self.refresh_dirty();
    }
    fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len()
        }
        self.ensure_visible();
    }
    fn right(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.col += 1
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0
        }
        self.ensure_visible();
    }
    fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].len())
        }
        self.ensure_visible();
    }
    fn down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].len())
        }
        self.ensure_visible();
    }
    fn copy_line(&mut self) {
        self.clipboard = alloc::vec![self.lines[self.row].clone()];
        self.status = "Line copied".into();
    }
    fn cut_line(&mut self) {
        self.checkpoint();
        self.clipboard = alloc::vec![self.lines.remove(self.row)];
        if self.lines.is_empty() {
            self.lines.push(String::new())
        }
        self.row = self.row.min(self.lines.len() - 1);
        self.col = self.col.min(self.lines[self.row].len());
        self.status = "Line cut".into();
        self.ensure_visible();
        self.refresh_dirty();
    }
    fn paste_line(&mut self) {
        if self.clipboard.is_empty() {
            self.status = "Clipboard empty".into();
            return;
        }
        self.checkpoint();
        let at = self.row + 1;
        for (i, line) in self.clipboard.clone().into_iter().enumerate() {
            self.lines.insert(at + i, line)
        }
        self.row = at;
        self.col = 0;
        self.status = "Pasted".into();
        self.ensure_visible();
        self.refresh_dirty();
    }
    fn ensure_visible(&mut self) {
        let (w, h) = framebuffer::dimensions();
        let rows = h.saturating_sub(HEADER_H + FOOTER_H) / CHAR_H;
        let cols = w / CHAR_W;
        let gutter = if self.config.line_numbers { 7 } else { 0 };
        if self.row < self.top {
            self.top = self.row
        } else if rows > 0 && self.row >= self.top + rows {
            self.top = self.row + 1 - rows
        }
        if self.col < self.left {
            self.left = self.col
        } else if cols > gutter && self.col >= self.left + cols - gutter {
            self.left = self.col + 1 - (cols - gutter)
        }
    }
    fn begin_prompt(&mut self, mode: PromptMode) {
        self.prompt = Some((mode, String::new()));
        self.status = match mode {
            PromptMode::Search => "Search",
            PromptMode::Goto => "Go to line",
            PromptMode::ReplaceFind => "Replace: find",
            PromptMode::ReplaceWith => "Replace with",
        }
        .into();
    }
    fn commit_prompt(&mut self) {
        let Some((mode, value)) = self.prompt.take() else {
            return;
        };
        match mode {
            PromptMode::Search => self.find_next(&value),
            PromptMode::Goto => match value.parse::<usize>() {
                Ok(n) if n > 0 => {
                    self.row = (n - 1).min(self.lines.len() - 1);
                    self.col = self.col.min(self.lines[self.row].len());
                    self.status = format!("Line {}", self.row + 1);
                    self.ensure_visible();
                }
                _ => self.status = "Invalid line number".into(),
            },
            PromptMode::ReplaceFind => {
                if value.is_empty() {
                    self.status = "Empty search".into()
                } else {
                    self.replace_needle = Some(value);
                    self.begin_prompt(PromptMode::ReplaceWith)
                }
            }
            PromptMode::ReplaceWith => {
                let Some(needle) = self.replace_needle.take() else {
                    return;
                };
                let count: usize = self.lines.iter().map(|l| l.matches(&needle).count()).sum();
                if count > 0 {
                    self.checkpoint();
                    for line in &mut self.lines {
                        *line = line.replace(&needle, &value)
                    }
                    self.refresh_dirty();
                }
                self.status = format!("Replaced {} occurrence(s)", count);
            }
        }
    }
    fn find_next(&mut self, needle: &str) {
        if needle.is_empty() {
            self.status = "Empty search".into();
            return;
        }
        let start_row = self.row;
        let start_col = (self.col + 1).min(self.lines[self.row].len());
        for pass in 0..2 {
            let from = if pass == 0 { start_row } else { 0 };
            let to = if pass == 0 {
                self.lines.len()
            } else {
                start_row + 1
            };
            for r in from..to {
                let off = if pass == 0 && r == start_row {
                    start_col
                } else {
                    0
                };
                if let Some(i) = self.lines[r][off..].find(needle) {
                    let c = off + i;
                    self.row = r;
                    self.col = c;
                    self.search_hit = Some((r, c, needle.len()));
                    self.status = format!("Found '{}'", needle);
                    self.ensure_visible();
                    return;
                }
            }
        }
        self.status = format!("'{}' not found", needle);
    }
    fn handle_prompt(&mut self, event: KeyEvent) {
        match event {
            KeyEvent::Char(c) => {
                if let Some((_, s)) = &mut self.prompt {
                    if s.len() < 64 {
                        s.push(c)
                    }
                }
            }
            KeyEvent::Backspace => {
                if let Some((_, s)) = &mut self.prompt {
                    s.pop();
                }
            }
            KeyEvent::Enter => self.commit_prompt(),
            KeyEvent::Escape | KeyEvent::Ctrl('q') => {
                self.prompt = None;
                self.status = "Cancelled".into();
            }
            _ => {}
        }
    }
    fn text(&self) -> String {
        self.lines.join("\n")
    }

    // ── selection ────────────────────────────────────────────────────────

    /// The selected range as `(start, end)` in reading order, or `None` for an
    /// empty/absent selection. `start` is included, `end` is the position just
    /// past the last selected character in the same row, or a character position
    /// on the last row for a multi-row selection.
    fn sel_bounds(&self) -> Option<(SelPoint, SelPoint)> {
        match self.sel.ordered() {
            Some((a, b)) if a != b => Some((a, b)),
            _ => None,
        }
    }

    fn has_selection(&self) -> bool {
        self.sel_bounds().is_some()
    }

    fn clear_selection(&mut self) {
        self.sel.clear();
    }

    fn begin_select(&mut self, p: SelPoint) {
        self.sel.anchor = Some(p);
        self.sel.head = Some(p);
    }

    fn extend_select(&mut self, p: SelPoint) {
        self.sel.head = Some(p);
    }

    /// Move the caret to `p`, clamped into the document and scrolled into view.
    /// Returns whether the viewport scrolled (the caller then repaints the frame).
    fn set_cursor(&mut self, p: SelPoint) -> bool {
        let (top, left) = (self.top, self.left);
        self.row = p.row.min(self.lines.len().saturating_sub(1));
        self.col = p.col.min(self.lines[self.row].len());
        self.ensure_visible();
        self.top != top || self.left != left
    }

    /// Select the whole buffer and put the caret at its end.
    fn select_all(&mut self) {
        let last = self.lines.len() - 1;
        let end = self.lines[last].len();
        let col = end.saturating_sub(1);
        self.sel.anchor = Some(SelPoint { row: 0, col: 0 });
        self.sel.head = Some(SelPoint { row: last, col });
        self.row = last;
        self.col = col;
        self.ensure_visible();
        self.status = alloc::format!("Selected {} line(s)", self.lines.len());
    }

    /// The selected text, or `None` when nothing is selected.
    ///
    /// Both ends are *character* positions (see [`Selection`]): a row-to-row drag
    /// includes the character under the pointer, which is what the highlight shows.
    fn sel_text(&self) -> Option<String> {
        let (a, b) = self.sel_bounds()?;
        let mut out = String::new();
        if a.row == b.row {
            let line = &self.lines[a.row];
            let ac = a.col.min(line.len());
            let bc = (b.col + 1).min(line.len());
            out.push_str(&line[ac..bc]);
        } else {
            let first = &self.lines[a.row];
            out.push_str(&first[a.col.min(first.len())..]);
            for r in (a.row + 1)..b.row {
                out.push('\n');
                out.push_str(&self.lines[r]);
            }
            out.push('\n');
            let last = &self.lines[b.row];
            let bc = (b.col + 1).min(last.len());
            out.push_str(&last[..bc]);
        }
        Some(out)
    }

    /// Delete the selected text, dropping the selection and leaving the caret at
    /// its start. Returns whether there was a selection to delete.
    fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.sel_bounds() else {
            self.sel.clear();
            return false;
        };
        self.checkpoint();
        // Both ends are inclusive character positions: same row deletes
        // `[a.col, b.col]`, a multi-row range deletes the tail of the first row,
        // the whole middle rows and the head of the last row, joining the two
        // remains into one line.
        if a.row == b.row {
            let line = &mut self.lines[a.row];
            let ac = a.col.min(line.len());
            let bc = (b.col + 1).min(line.len());
            line.replace_range(ac..bc, "");
            self.row = a.row;
            self.col = ac;
        } else {
            let ac = a.col.min(self.lines[a.row].len());
            let bc = (b.col + 1).min(self.lines[b.row].len());
            let tail: String = self.lines[b.row][bc..].into();
            self.lines[a.row].truncate(ac);
            self.lines[a.row].push_str(&tail);
            self.lines.drain(a.row + 1..=b.row);
            self.row = a.row;
            self.col = ac;
        }
        self.sel.clear();
        self.ensure_visible();
        self.refresh_dirty();
        true
    }

    /// Insert `text` at the caret (newlines split lines). Used by the pastes;
    /// a selection is deleted first by the caller.
    fn insert_text(&mut self, text: &str) {
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            for ch in part.chars() {
                self.insert(ch);
            }
        }
    }
}

fn sanitize_line(s: &str) -> String {
    s.chars()
        .take(MAX_LINE)
        .map(|c| {
            if c == '\t' {
                ' '
            } else if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

fn open_or_create(path: &str) -> Result<(Arc<dyn VfsNode>, String), VfsError> {
    match crate::vfs::lookup_path(path) {
        Ok(node) if !node.is_directory() => {
            let size = (node.size() as usize).min(MAX_FILE);
            let mut bytes = alloc::vec![0u8;size];
            let n = node.read(0, &mut bytes)?;
            bytes.truncate(n);
            Ok((node, String::from_utf8_lossy(&bytes).into_owned()))
        }
        Ok(_) => Err(VfsError::InvalidArgument),
        Err(_) => {
            let trimmed = path.trim_end_matches('/');
            let i = trimmed.rfind('/').ok_or(VfsError::InvalidArgument)?;
            let leaf = &trimmed[i + 1..];
            if leaf.is_empty() {
                return Err(VfsError::InvalidArgument);
            }
            let parent = if i == 0 { "/" } else { &trimmed[..i] };
            let dir = crate::vfs::lookup_path(parent)?;
            let node = dir.create_file(leaf)?;
            Ok((node, String::new()))
        }
    }
}

fn write_path(path: &str, data: &[u8]) -> Result<(), VfsError> {
    let t = path.trim_end_matches('/');
    let i = t.rfind('/').ok_or(VfsError::InvalidArgument)?;
    let parent = if i == 0 { "/" } else { &t[..i] };
    let leaf = &t[i + 1..];
    let dir = crate::vfs::lookup_path(parent)?;
    let file = match dir.lookup(leaf) {
        Ok(n) => n,
        Err(VfsError::NotFound) => dir.create_file(leaf)?,
        Err(e) => return Err(e),
    };
    file.truncate(0)?;
    if !data.is_empty() && file.write(0, data)? != data.len() {
        return Err(VfsError::IoError);
    }
    file.sync();
    Ok(())
}
fn save(editor: &mut Editor, node: &Arc<dyn VfsNode>) -> Result<(), VfsError> {
    if editor.config.backup && node.size() > 0 {
        let size = (node.size() as usize).min(MAX_FILE);
        let mut old = alloc::vec![0u8;size];
        let n = node.read(0, &mut old)?;
        old.truncate(n);
        write_path(&format!("{}.bak", editor.path), &old)?;
    }
    let text = if editor.config.trim_trailing {
        editor
            .lines
            .iter()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        editor.text()
    };
    if text.len() > MAX_FILE {
        return Err(VfsError::InvalidArgument);
    }
    node.truncate(0)?;
    if !text.is_empty() {
        let n = node.write(0, text.as_bytes())?;
        if n != text.len() {
            return Err(VfsError::IoError);
        }
    }
    node.sync();
    editor.mark_clean();
    editor.status = format!("Saved {} bytes", text.len());
    Ok(())
}

fn clipped_ascii(s: &str, start: usize, width: usize, spaces: bool) -> String {
    s.bytes()
        .skip(start)
        .take(width)
        .map(|b| {
            if b == b' ' && spaces {
                '.'
            } else if (32..=126).contains(&b) {
                b as char
            } else {
                '?'
            }
        })
        .collect()
}

// ─── rendering ────────────────────────────────────────────────────────────
//
// Three rules keep the drawing paths honest:
//
// * **Damage, not frames.** A keystroke changes one line and the caret.
//   Repainting the whole surface for it was measured at tens of milliseconds
//   under TCG (a full frame is ~10 000 glyphs = ~1.4 M pixel writes) and is what
//   made typing feel laggy. Each entry point below repaints the smallest region
//   that can have changed; the console's dirty-rect flush then copies only that
//   region to the device.
// * **The arrow is re-shown after every frame.** Full-screen drawers hide the
//   software mouse cursor before painting and must put it back on top of the
//   finished frame — a path that hides it and never shows it leaves the pointer
//   invisible for as long as the editor is open.
// * **RAM, then exactly one flush.** Pixels only reach the screen through
//   `framebuffer::flush`; a path that returns without it leaves its work in the
//   shadow buffer (that is how the caret vanished: the blink wrote the bar but
//   never pushed it).

/// Pixel geometry of the editor frame, derived once per draw.
struct Layout {
    w: usize,
    h: usize,
    /// Sidebar width in cells (0 when the sidebar is hidden).
    tree_cols: usize,
    /// Text columns beside the sidebar.
    cols: usize,
    /// Visible editor rows.
    rows: usize,
    /// Line-number gutter width in cells (0 when line numbers are off).
    gutter: usize,
    /// Top pixel of the footer strip.
    foot: usize,
}

/// Visible editor rows for a screen `h` pixels tall.
fn body_rows(h: usize) -> usize {
    h.saturating_sub(HEADER_H + FOOTER_H) / CHAR_H
}

fn layout(editor: &Editor, tree: Option<&super::tree::FileTree>) -> Option<Layout> {
    let (w, h) = framebuffer::dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let tree_cols = if tree.is_some() { TREE_COLS } else { 0 };
    Some(Layout {
        w,
        h,
        tree_cols,
        cols: (w / CHAR_W).saturating_sub(tree_cols),
        rows: body_rows(h),
        gutter: if editor.config.line_numbers { 7 } else { 0 },
        foot: h.saturating_sub(FOOTER_H),
    })
}

/// Title strip at the top: path and modified marker.
fn paint_header(fb: &mut framebuffer::FramebufferWriter, editor: &Editor, l: &Layout) {
    let blue = editor.config.palette()[4];
    fb.fill_rect(0, 0, l.w, HEADER_H, blue);
    // Unsaved work is stated in the title, in words rather than a lone asterisk:
    // "did my edit do anything" should be answerable at a glance, and an `*` next
    // to a long path is not.
    let title = if editor.dirty {
        format!(" nano+  {}   [modified - ^S saves]", editor.path)
    } else {
        format!(" nano+  {}", editor.path)
    };
    fb.draw_text_px(
        6,
        3,
        &clipped_ascii(&title, 0, l.cols.saturating_sub(1), false),
        if editor.dirty { 0xFFE066 } else { 0xFFFFFF },
        blue,
    );
}

/// The sidebar: the project tree rooted at the edited file's directory.
fn paint_tree(
    fb: &mut framebuffer::FramebufferWriter,
    editor: &Editor,
    tree: &super::tree::FileTree,
    l: &Layout,
) {
    let [_, surface, text, muted, _, _, _, _] = editor.config.palette();
    let tw = l.tree_cols * CHAR_W;
    fb.fill_rect(
        0,
        HEADER_H,
        tw,
        l.h.saturating_sub(HEADER_H + FOOTER_H),
        surface,
    );
    let header = alloc::format!(" {} ", tree.root());
    fb.draw_text_px(
        2,
        HEADER_H + 2,
        &clipped_ascii(&header, 0, l.tree_cols.saturating_sub(1), false),
        muted,
        surface,
    );
    let list_y = HEADER_H + CHAR_H + 4;
    let list_rows = l.rows.saturating_sub(1);
    for vr in 0..list_rows {
        let i = tree.top + vr;
        let Some(row) = tree.rows().get(i) else {
            break;
        };
        let y = list_y + vr * CHAR_H;
        // Indent per depth, and a marker that says open/closed at a glance.
        let indent = row.depth * 2;
        let mark = if row.is_dir {
            if row.expanded {
                "v "
            } else {
                "> "
            }
        } else {
            "  "
        };
        let text_line = alloc::format!("{}{}{}", " ".repeat(indent), mark, row.name);
        let (fg, bg) = if i == tree.cursor {
            (0x000000, 0xCCCCCC)
        } else if row.is_dir {
            (text, surface)
        } else {
            (muted, surface)
        };
        fb.draw_text_px(
            2,
            y,
            &clipped_ascii(&text_line, 0, l.tree_cols.saturating_sub(1), false),
            fg,
            bg,
        );
    }
}

/// Repaint one editor row (background, gutter, text, search hit).
///
/// Fills its own background: incremental callers have no full-frame fill behind
/// them, so the tail of a line that just got shorter must be wiped here.
fn paint_editor_row(
    fb: &mut framebuffer::FramebufferWriter,
    editor: &Editor,
    l: &Layout,
    r: usize,
    vr: usize,
) {
    let [bg, surface, text, muted, blue, _, _, select] = editor.config.palette();
    let y = HEADER_H + vr * CHAR_H;
    let x0 = l.tree_cols * CHAR_W;
    fb.fill_rect(x0, y, l.w.saturating_sub(x0), CHAR_H, bg);
    if r >= editor.lines.len() {
        fb.draw_text_px(x0 + 8, y, "~", blue, bg);
        return;
    }
    if editor.config.line_numbers {
        fb.fill_rect(x0, y, l.gutter * CHAR_W, CHAR_H, surface);
        let num = format!("{:>5} ", r + 1);
        fb.draw_text_px(x0, y, &num, muted, surface);
    }
    let visible = clipped_ascii(
        &editor.lines[r],
        editor.left,
        l.cols.saturating_sub(l.gutter),
        editor.config.show_whitespace,
    );
    let tx = x0 + l.gutter * CHAR_W;
    match editor.search_hit {
        Some((hr, hc, hlen))
            if hr == r && hc >= editor.left && hc < editor.left + visible.len() =>
        {
            let before = clipped_ascii(
                &editor.lines[r],
                editor.left,
                hc - editor.left,
                editor.config.show_whitespace,
            );
            let hit = clipped_ascii(&editor.lines[r], hc, hlen, editor.config.show_whitespace);
            fb.draw_text_px(tx, y, &visible, text, bg);
            fb.draw_text_px(tx + before.len() * CHAR_W, y, &hit, 0xFFFFFF, select);
        }
        _ => fb.draw_text_px(tx, y, &visible, text, bg),
    }
    // Selection highlight goes on last, over the text and over a search hit: what
    // the user selected is what they will act on. The console has no blending, so
    // this repaints the covered cells inverted (black on light) instead of tinting.
    if let Some((c0, c1)) = selected_cols(editor, r) {
        let vis_last = editor.left + visible.len().saturating_sub(1);
        if !visible.is_empty() && c0 <= vis_last && c1 >= editor.left {
            let from = c0.max(editor.left);
            let to = c1.min(vis_last);
            if from <= to {
                let seg = clipped_ascii(
                    &editor.lines[r],
                    from,
                    to - from + 1,
                    editor.config.show_whitespace,
                );
                fb.draw_text_px(
                    tx + (from - editor.left) * CHAR_W,
                    y,
                    &seg,
                    0x000000,
                    select,
                );
            }
        }
    }
}

/// The document columns of line `r` covered by the selection, inclusive, or `None`
/// when this row has no selected characters.
///
/// Selection ends are character positions (a drag includes the character under the
/// pointer); the last character of a multi-row selection's last row is `b.col`, and
/// rows in between are fully selected. An empty line has nothing to highlight.
fn selected_cols(editor: &Editor, r: usize) -> Option<(usize, usize)> {
    let (a, b) = editor.sel_bounds()?;
    if r < a.row || r > b.row {
        return None;
    }
    let line_len = editor.lines.get(r).map(|l| l.len()).unwrap_or(0);
    if line_len == 0 {
        return None;
    }
    let from = if r == a.row { a.col } else { 0 };
    let to = if r == b.row { b.col } else { line_len - 1 };
    let to = to.min(line_len - 1);
    if from > to {
        return None;
    }
    Some((from, to))
}

/// Map a pixel position to a document position when it is over the text pane.
///
/// The screen is cells of [`CHAR_W`]×[`CHAR_H`] pixels starting below the header;
/// the sidebar and the line-number gutter are subtracted before the column is
/// translated by the horizontal scroll offset. A click past the end of a line puts
/// the caret at its end, never on a non-existent character.
fn mouse_text_point(
    editor: &Editor,
    tree: Option<&super::tree::FileTree>,
    mx: usize,
    my: usize,
) -> Option<SelPoint> {
    let l = layout(editor, tree)?;
    if my < HEADER_H || my >= l.foot {
        return None;
    }
    let vr = (my - HEADER_H) / CHAR_H;
    if vr >= l.rows {
        return None;
    }
    let x0 = l.tree_cols * CHAR_W;
    if mx < x0 {
        return None;
    }
    let cell = (mx - x0) / CHAR_W;
    let col_in_text = cell.saturating_sub(l.gutter);
    let row = (editor.top + vr).min(editor.lines.len().saturating_sub(1));
    let col = (editor.left + col_in_text).min(editor.lines[row].len());
    Some(SelPoint { row, col })
}

/// What a click at `(mx, my)` hits: a document position, a sidebar row, or nothing.
fn mouse_target(
    editor: &Editor,
    tree: Option<&super::tree::FileTree>,
    mx: usize,
    my: usize,
) -> Option<Hit> {
    let l = layout(editor, tree)?;
    if my < HEADER_H || my >= l.foot {
        return None;
    }
    if let Some(t) = tree {
        if mx < l.tree_cols * CHAR_W {
            // Same rows `paint_tree` lists: the root line, then one row per entry.
            let list_y = HEADER_H + CHAR_H + 4;
            if my < list_y {
                return None;
            }
            let vr = (my - list_y) / CHAR_H;
            let i = t.top + vr;
            if i < t.rows().len() {
                return Some(Hit::Tree(i));
            }
            return None;
        }
    }
    mouse_text_point(editor, tree, mx, my).map(Hit::Text)
}

/// Draw the footer strip: the status line (or the active prompt) and key hints.
fn paint_footer(fb: &mut framebuffer::FramebufferWriter, editor: &Editor, l: &Layout) {
    let [_, surface, text, _, _, green, red, _] = editor.config.palette();
    let foot = l.foot;
    fb.fill_rect(0, foot, l.w, FOOTER_H, surface);
    let status = if let Some((mode, value)) = &editor.prompt {
        format!(
            "{}: {}_",
            if *mode == PromptMode::Search {
                "Search"
            } else {
                "Line"
            },
            value
        )
    } else {
        format!(
            "{}   Ln {}, Col {}",
            editor.status,
            editor.row + 1,
            editor.col + 1
        )
    };
    fb.draw_text_px(
        6,
        foot + 2,
        &clipped_ascii(&status, 0, l.cols.saturating_sub(1), false),
        if editor.status.starts_with("Error") {
            red
        } else {
            green
        },
        surface,
    );
    let dirty_hint = if editor.dirty {
        "^S Save (modified)  ^Q Quit"
    } else {
        "^S Save  ^Q Quit"
    };
    fb.draw_text_px(
        6,
        foot + CHAR_H + 2,
        &alloc::format!(
            "{}  ^A Sel  ^C Copy  ^X Cut  ^B Files  ^F Find  ^K Line  ^U Paste",
            dirty_hint
        ),
        if editor.dirty { 0xFFE066 } else { text },
        surface,
    );
}

/// The caret's pixel position, or `None` when it is off-screen or a prompt owns
/// the footer.
fn caret_position(editor: &Editor, l: &Layout) -> Option<(usize, usize)> {
    if editor.prompt.is_some() || editor.row < editor.top || editor.col < editor.left {
        return None;
    }
    let vr = editor.row - editor.top;
    if vr >= l.rows {
        return None;
    }
    let cx = (l.tree_cols + l.gutter + editor.col - editor.left) * CHAR_W;
    if cx + 1 >= l.w {
        return None;
    }
    Some((cx, HEADER_H + vr * CHAR_H))
}

/// Draw the caret bar on top of a row that has just been repainted.
///
/// Only the "on" phase needs drawing: the row under it was repainted immediately
/// before, which is what restores the character the bar covers.
fn paint_caret_bar(
    fb: &mut framebuffer::FramebufferWriter,
    editor: &Editor,
    caret_on: bool,
    l: &Layout,
) {
    let Some((cx, cy)) = caret_position(editor, l) else {
        return;
    };
    if caret_on {
        fb.fill_rect(cx, cy, 2, CHAR_H, super::caret::CARET_COLOR);
    }
}

/// Repaint the whole frame: header, sidebar, every editor row, footer, caret.
fn render_full(editor: &Editor, caret_on: bool, tree: Option<&super::tree::FileTree>) {
    let Some(l) = layout(editor, tree) else {
        return;
    };
    // Deferred: the restored background rides along with the frame flush below, so
    // the arrow stays on the device while the frame is being painted.
    cursor::hide_deferred();
    let _ = framebuffer::with(|fb| {
        let bg = editor.config.palette()[0];
        fb.fill_rect(0, 0, l.w, l.h, bg);
        paint_header(fb, editor, &l);
        if let Some(t) = tree {
            paint_tree(fb, editor, t, &l);
        }
        for vr in 0..l.rows {
            paint_editor_row(fb, editor, &l, editor.top + vr, vr);
        }
        paint_footer(fb, editor, &l);
        paint_caret_bar(fb, editor, caret_on, &l);
    });
    cursor::place_here_deferred();
    framebuffer::flush();
}

/// Repaint body rows `first..=last` (visible row indices), the footer and the
/// caret. The header and sidebar are left alone.
fn render_body(
    editor: &Editor,
    caret_on: bool,
    tree: Option<&super::tree::FileTree>,
    first: usize,
    last: usize,
) {
    let Some(l) = layout(editor, tree) else {
        return;
    };
    if l.rows == 0 {
        return;
    }
    let last = last.min(l.rows - 1);
    let first = first.min(last);
    // Deferred: the restored background rides along with the frame flush below, so
    // the arrow stays on the device while the frame is being painted.
    cursor::hide_deferred();
    let _ = framebuffer::with(|fb| {
        for vr in first..=last {
            paint_editor_row(fb, editor, &l, editor.top + vr, vr);
        }
        paint_footer(fb, editor, &l);
        paint_caret_bar(fb, editor, caret_on, &l);
    });
    cursor::place_here_deferred();
    framebuffer::flush();
}

/// Repaint the sidebar and footer (tree focus), plus the caret row so a blinking
/// bar cannot be left stranded while the text pane is not being edited.
fn render_tree(editor: &Editor, caret_on: bool, tree: Option<&super::tree::FileTree>) {
    let Some(l) = layout(editor, tree) else {
        return;
    };
    // Deferred: the restored background rides along with the frame flush below, so
    // the arrow stays on the device while the frame is being painted.
    cursor::hide_deferred();
    let _ = framebuffer::with(|fb| {
        if let Some(t) = tree {
            paint_tree(fb, editor, t, &l);
        }
        if editor.prompt.is_none() {
            if let Some(vr) = caret_position(editor, &l).map(|(_, y)| (y - HEADER_H) / CHAR_H) {
                if vr < l.rows {
                    paint_editor_row(fb, editor, &l, editor.top + vr, vr);
                }
            }
        }
        paint_footer(fb, editor, &l);
        paint_caret_bar(fb, editor, caret_on, &l);
    });
    cursor::place_here_deferred();
    framebuffer::flush();
}

/// Repaint only the footer (a live prompt is being typed into). No caret is drawn
/// while a prompt owns the footer, so nothing else can have changed.
fn render_footer_only(editor: &Editor, tree: Option<&super::tree::FileTree>) {
    let Some(l) = layout(editor, tree) else {
        return;
    };
    cursor::hide_deferred();
    let _ = framebuffer::with(|fb| paint_footer(fb, editor, &l));
    cursor::place_here_deferred();
    framebuffer::flush();
}

/// Repaint the caret's row and the caret itself — the blink path.
///
/// Erasing the bar means putting the character underneath back, so the row is
/// repainted rather than blanked with the background colour: a 2-pixel background
/// strip would erase the left column of a mid-line glyph on every blink.
fn render_blink(editor: &Editor, caret_on: bool, tree: Option<&super::tree::FileTree>) {
    let Some(l) = layout(editor, tree) else {
        return;
    };
    let Some(vr) = caret_position(editor, &l).map(|(_, y)| (y - HEADER_H) / CHAR_H) else {
        return;
    };
    // Deferred: the restored background rides along with the frame flush below, so
    // the arrow stays on the device while the frame is being painted.
    cursor::hide_deferred();
    let _ = framebuffer::with(|fb| {
        paint_editor_row(fb, editor, &l, editor.top + vr, vr);
        paint_caret_bar(fb, editor, caret_on, &l);
    });
    cursor::place_here_deferred();
    framebuffer::flush();
}

pub fn run(path_arg: &str) {
    let path = super::path::resolve(&super::path::cwd(), path_arg);
    let (node, text) = match open_or_create(&path) {
        Ok(v) => v,
        Err(e) => {
            super::render::error_line(&format!("nano: {}: {:?}", path, e));
            return;
        }
    };
    let node = node;
    let mut editor = Editor::new(&path, &text, NanoConfig::load());
    editor.tree_root = Some(parent_dir(&path));
    // The sidebar is visible from the start: seeing the project is the point of an
    // IDE-like editor, and `^B` hides it for a full-width view of the text.
    let mut tree: Option<super::tree::FileTree> = None;
    let mut tree_focus = false;
    crate::kprintln!("nano+: editing {}", path);
    event_loop(editor, node, &mut tree, &mut tree_focus);
    framebuffer::clear_screen();
    cursor::hide();
    crate::kprintln!("nano+: closed {}", path);
}

/// Which pane the keyboard drives. The sidebar is only reachable while it is open.
#[derive(PartialEq, Clone, Copy)]
enum Pane {
    Text,
    Tree,
}

/// The editor's event loop, with the sidebar threaded through it.
///
/// Split out of [`run`] so the tree and the editor are separate borrows: the loop
/// needs `&mut` on both, and holding them as two locals of one frame is what keeps
/// that legal without interior mutability.
fn event_loop(
    mut editor: Editor,
    mut node: alloc::sync::Arc<dyn crate::vfs::VfsNode>,
    tree: &mut Option<super::tree::FileTree>,
    tree_focus: &mut bool,
) {
    let mut decoder = Decoder::new();
    // The keyboard starts in the TEXT, always — including when the sidebar is open.
    // Opening a file is an intent to edit it; routing the first keystroke into a
    // file browser instead made the editor look broken (you could not type, and
    // `^Q` did nothing because the sidebar owned the keys). `Tab` moves into the
    // tree when it is wanted.
    let mut pane = Pane::Text;
    // Open the sidebar at the edited file's directory.
    if let Some(root) = editor.tree_root.clone() {
        *tree = Some(super::tree::FileTree::new(&root));
        if let Some(t) = tree.as_mut() {
            let (_, h) = framebuffer::dimensions();
            t.view_rows = body_rows(h).saturating_sub(1);
            // Put the highlight on the file being edited, not on the first entry:
            // the tree should show where you are.
            let current = editor.path.clone();
            if let Some(i) = t.rows().iter().position(|r| r.path == current) {
                t.cursor = i;
            }
            t.ensure_visible();
        }
    }
    let mut last_phase = blink();
    // Previous left-button state, for edge detection. The PS/2 driver reports the
    // button as a level, so "pressed" is a rising edge, not `left == true`.
    let mut prev_left = false;
    render_full(&editor, last_phase, tree.as_ref());
    'app: loop {
        crate::arch::cpu::halt();
        while let Some(sc) = super::try_read_scancode() {
            let Some(event) = decoder.feed(sc) else {
                continue;
            };
            if editor.prompt.is_some() {
                editor.handle_prompt(event);
                // A committed/cancelled prompt moves the cursor and may highlight a
                // search hit elsewhere on screen; typing into it only changes the
                // footer line.
                if matches!(
                    event,
                    KeyEvent::Enter | KeyEvent::Escape | KeyEvent::Ctrl('q')
                ) {
                    last_phase = blink();
                    render_full(&editor, last_phase, tree.as_ref());
                } else {
                    last_phase = blink();
                    render_footer_only(&editor, tree.as_ref());
                }
                continue;
            }

            // ── Keys that belong to the editor no matter where the focus is ──
            // `^Q`, `^S` and `^B` are how you leave, save and get back; a focus
            // state that swallows them is a trap, which is exactly what happened
            // when the sidebar opened focused.
            match event {
                KeyEvent::Ctrl('q') => {
                    if editor.dirty && !editor.quit_armed {
                        editor.quit_armed = true;
                        editor.status = "Unsaved changes — press ^Q again to quit".into();
                    } else {
                        break 'app;
                    }
                    last_phase = blink();
                    render_full(&editor, last_phase, tree.as_ref());
                    continue;
                }
                KeyEvent::Ctrl('s') => {
                    if let Err(e) = save(&mut editor, &node) {
                        editor.status = format!("Error saving: {:?}", e)
                    }
                    last_phase = blink();
                    render_full(&editor, last_phase, tree.as_ref());
                    continue;
                }
                KeyEvent::Ctrl('b') => {
                    if tree.is_some() {
                        *tree = None;
                        pane = Pane::Text;
                    } else {
                        *tree = Some(super::tree::FileTree::new(&parent_dir(&editor.path)));
                        pane = Pane::Tree;
                        if let Some(t) = tree.as_mut() {
                            let (_, h) = framebuffer::dimensions();
                            t.view_rows = body_rows(h).saturating_sub(1);
                        }
                    }
                    *tree_focus = pane == Pane::Tree;
                    last_phase = blink();
                    render_full(&editor, last_phase, tree.as_ref());
                    continue;
                }
                _ => {}
            }

            // ── Sidebar focus: the tree owns the arrow keys and Enter ──
            if pane == Pane::Tree {
                let mut switched = false;
                // Set when a file switch was refused because the buffer is dirty: the
                // sidebar must then snap its highlight back to the file that is
                // actually open, or it claims you are editing a file you are not.
                let mut refused = false;
                match event {
                    KeyEvent::Up => {
                        if let Some(t) = tree.as_mut() {
                            t.move_up()
                        }
                    }
                    KeyEvent::Down => {
                        if let Some(t) = tree.as_mut() {
                            t.move_down()
                        }
                    }
                    KeyEvent::Left => {
                        if let Some(t) = tree.as_mut() {
                            t.collapse_or_parent();
                        }
                    }
                    KeyEvent::Right | KeyEvent::Enter => {
                        if let Some(path) = tree.as_mut().and_then(|t| t.activate()) {
                            // Switching files with unsaved changes would silently
                            // drop them, so it is refused instead.
                            if editor.dirty {
                                editor.status =
                                    "Unsaved changes — ^S to save, ^Q to discard".into();
                                refused = true;
                            } else if let Ok((n, text)) = open_or_create(&path) {
                                editor.reload(&path, &text);
                                node = n;
                                switched = true;
                                // Opening a file is an intent to edit it: the keyboard
                                // goes back to the text, so the very next keystroke is
                                // typing instead of tree navigation. (Opening a
                                // directory keeps the focus here for arrow-key walking.)
                                pane = Pane::Text;
                                *tree_focus = false;
                            }
                        }
                    }
                    // `Tab` returns to the text; `Esc` too. The editor-level
                    // `^Q`/`^S`/`^B` above are already handled.
                    KeyEvent::Tab | KeyEvent::Escape => {
                        pane = Pane::Text;
                        *tree_focus = false;
                    }
                    _ => {}
                }
                if let Some(t) = tree.as_mut() {
                    let (_, h) = framebuffer::dimensions();
                    t.view_rows = body_rows(h).saturating_sub(1);
                    if refused {
                        // The switch was refused: the highlight had already moved to
                        // the clicked row, so put it back on the open file.
                        focus_tree_on(&editor, t);
                    }
                    t.ensure_visible();
                }
                last_phase = blink();
                if switched {
                    render_full(&editor, last_phase, tree.as_ref());
                } else {
                    render_tree(&editor, last_phase, tree.as_ref());
                }
                continue;
            }

            // ── Text focus ──
            let before = (
                editor.row,
                editor.col,
                editor.top,
                editor.left,
                editor.lines.len(),
            );
            match event {
                // Editing keys act on the selection when there is one: typing
                // replaces it, Backspace/Delete remove it, and `^A` + Delete clears
                // the whole line (or the whole file) instead of deleting one
                // character. Ignoring the visible selection here was the bug the
                // first user hit within a minute of opening the editor.
                KeyEvent::Char(c) => {
                    editor.delete_selection();
                    editor.insert(c)
                }
                KeyEvent::Enter => {
                    editor.delete_selection();
                    editor.newline()
                }
                KeyEvent::Backspace => {
                    if !editor.delete_selection() {
                        editor.backspace()
                    }
                }
                KeyEvent::Delete => {
                    if !editor.delete_selection() {
                        editor.delete()
                    }
                }
                // Plain caret movement cancels the selection, like every editor.
                KeyEvent::Left => {
                    editor.clear_selection();
                    editor.left()
                }
                KeyEvent::Right => {
                    editor.clear_selection();
                    editor.right()
                }
                KeyEvent::Up => {
                    editor.clear_selection();
                    editor.up()
                }
                KeyEvent::Down => {
                    editor.clear_selection();
                    editor.down()
                }
                KeyEvent::Home => {
                    editor.clear_selection();
                    editor.col = 0;
                    editor.ensure_visible()
                }
                KeyEvent::End => {
                    editor.clear_selection();
                    editor.col = editor.lines[editor.row].len();
                    editor.ensure_visible()
                }
                KeyEvent::PageUp => {
                    editor.clear_selection();
                    for _ in 0..12 {
                        editor.up()
                    }
                }
                KeyEvent::PageDown => {
                    editor.clear_selection();
                    for _ in 0..12 {
                        editor.down()
                    }
                }
                KeyEvent::Tab => {
                    editor.delete_selection();
                    for _ in 0..editor.config.tab_size {
                        editor.insert(' ')
                    }
                }
                KeyEvent::Escape => {
                    if editor.has_selection() {
                        // Esc first drops a selection; quitting is the next key.
                        editor.clear_selection();
                    } else if editor.dirty && !editor.quit_armed {
                        editor.quit_armed = true;
                        editor.status = "Unsaved changes — press ^Q again to quit".into()
                    } else {
                        break 'app;
                    }
                }
                // Enter the sidebar without the mouse.
                KeyEvent::Ctrl('t') => {
                    if tree.is_some() {
                        pane = Pane::Tree;
                        *tree_focus = true;
                    }
                }
                KeyEvent::Ctrl('f') => editor.begin_prompt(PromptMode::Search),
                KeyEvent::Ctrl('r') => editor.begin_prompt(PromptMode::ReplaceFind),
                KeyEvent::Ctrl('g') => editor.begin_prompt(PromptMode::Goto),
                KeyEvent::Ctrl('z') => editor.undo(),
                KeyEvent::Ctrl('y') => editor.redo(),
                // `^A` selects everything; `^C`/`^K`/`^X` copy and cut the
                // selection when one exists and fall back to their line behaviour
                // when none does. That keeps the classic bindings and gives the
                // mouse selection somewhere to go.
                KeyEvent::Ctrl('a') => editor.select_all(),
                KeyEvent::Ctrl('c') => {
                    if let Some(text) = editor.sel_text() {
                        let n = text.chars().count();
                        editor.sel_clip = text;
                        editor.status = format!("Copied {} char(s)", n);
                    } else {
                        editor.copy_line();
                    }
                }
                KeyEvent::Ctrl('x') | KeyEvent::Ctrl('k') => {
                    if let Some(text) = editor.sel_text() {
                        let n = text.chars().count();
                        editor.sel_clip = text;
                        editor.delete_selection();
                        editor.status = format!("Cut {} char(s)", n);
                    } else {
                        editor.cut_line();
                    }
                }
                KeyEvent::Ctrl('u') | KeyEvent::Ctrl('v') => {
                    if editor.sel_clip.is_empty() {
                        editor.paste_line();
                    } else {
                        let text = editor.sel_clip.clone();
                        editor.delete_selection();
                        editor.insert_text(&text);
                        editor.status = "Pasted".into();
                    }
                }
                _ => {}
            }
            last_phase = blink();
            if editor.prompt.is_some() {
                // ^F/^R/^G opened a prompt: repaint the frame so the caret bar on
                // its old row is erased and the footer shows the prompt.
                render_full(&editor, last_phase, tree.as_ref());
            } else if editor.top != before.2 || editor.left != before.3 {
                // Scrolling (or a very long line) moved the whole viewport.
                render_full(&editor, last_phase, tree.as_ref());
            } else if editor.lines.len() != before.4 {
                // Rows below the edit shifted: repaint from the changed row down.
                let first = before.0.min(editor.row).saturating_sub(editor.top);
                render_body(
                    &editor,
                    last_phase,
                    tree.as_ref(),
                    first,
                    body_rows(framebuffer::dimensions().1).saturating_sub(1),
                );
            } else {
                // Only the rows the caret moved between (usually one) changed.
                let a = before.0.saturating_sub(editor.top);
                let b = editor.row.saturating_sub(editor.top);
                render_body(&editor, last_phase, tree.as_ref(), a.min(b), a.max(b));
            }
        }
        // ── Mouse ────────────────────────────────────────────────────────
        // Clicking used to do nothing at all in the editor: the arrow was drawn,
        // the buttons were never read. A click places the caret, a drag selects
        // text (and the caret follows the drag), and a click on a sidebar row
        // focuses/activates it.
        let mouse = crate::drivers::ps2_mouse::poll();
        let pressed = mouse.left && !prev_left;
        let released = !mouse.left && prev_left;
        prev_left = mouse.left;
        let mut mouse_changed = false;
        let mut mouse_full = false;
        if pressed {
            match mouse_target(&editor, tree.as_ref(), mouse.x, mouse.y) {
                Some(Hit::Text(p)) => {
                    let old_row = editor.row;
                    editor.clear_selection();
                    mouse_full = editor.set_cursor(p);
                    editor.begin_select(p);
                    if !mouse_full {
                        let a = old_row.saturating_sub(editor.top);
                        let b = editor.row.saturating_sub(editor.top);
                        last_phase = blink();
                        render_body(&editor, last_phase, tree.as_ref(), a.min(b), a.max(b));
                    }
                    mouse_changed = true;
                }
                Some(Hit::Tree(i)) => {
                    pane = Pane::Tree;
                    *tree_focus = true;
                    if let Some(t) = tree.as_mut() {
                        t.cursor = i;
                    }
                    // A click on a file row opens it; on a directory it just
                    // focuses the row (Enter/Right still expands).
                    let target = tree.as_mut().and_then(|t| t.activate());
                    if let Some(path) = target {
                        if editor.dirty {
                            editor.status = "Unsaved changes — ^S to save, ^Q to discard".into();
                            if let Some(t) = tree.as_mut() {
                                focus_tree_on(&editor, t);
                            }
                        } else if let Ok((n, text)) = open_or_create(&path) {
                            editor.reload(&path, &text);
                            node = n;
                            // Clicking a file row opens it *and* hands the keyboard
                            // back to the text: leaving the sidebar focused is what made
                            // the editor look dead after a few file switches — every
                            // key was going to the tree.
                            pane = Pane::Text;
                            *tree_focus = false;
                        }
                    }
                    mouse_full = true;
                    mouse_changed = true;
                }
                None => {}
            }
        } else if mouse.left && editor.sel.anchor.is_some() {
            // Drag: extend the selection to the cell under the pointer and move the
            // caret with it, so typing after a drag lands at the pointer.
            if let Some(p) = mouse_text_point(&editor, tree.as_ref(), mouse.x, mouse.y) {
                let prev_head = editor.sel.head.map(|q| q.row);
                let old_row = editor.row;
                editor.extend_select(p);
                mouse_full = editor.set_cursor(p);
                if !mouse_full {
                    let mut lo = p.row;
                    let mut hi = p.row;
                    for r in [old_row, p.row]
                        .into_iter()
                        .chain(prev_head)
                        .chain(editor.sel.anchor.map(|q| q.row))
                    {
                        lo = lo.min(r);
                        hi = hi.max(r);
                    }
                    let lo_vr = lo.saturating_sub(editor.top);
                    let hi_vr = hi
                        .saturating_sub(editor.top)
                        .min(body_rows(framebuffer::dimensions().1).saturating_sub(1));
                    if lo_vr <= hi_vr {
                        last_phase = blink();
                        render_body(&editor, last_phase, tree.as_ref(), lo_vr, hi_vr);
                    }
                }
                mouse_changed = true;
            }
        } else if released && editor.sel.is_empty() {
            editor.clear_selection();
        }
        if mouse_changed && mouse_full {
            last_phase = blink();
            render_full(&editor, last_phase, tree.as_ref());
        }
        // The arrow: `move_to` is idempotent (it returns early when the pointer
        // has not moved), so this is a comparison per timer tick, not a redraw.
        cursor::move_to(mouse.x, mouse.y);
        // Blink: repaint the caret's row and the bar, only when the phase changed.
        // 1 kHz wakes are too fast for that to be anything but two screen changes
        // per second.
        let phase = blink();
        if phase != last_phase {
            last_phase = phase;
            render_blink(&editor, phase, tree.as_ref());
        }
    }
    let _ = tree_focus;
}

/// The caret's blink phase, shared with the shell so both carets blink together.
fn blink() -> bool {
    super::caret::Caret::blink_on(crate::task::scheduler::ticks())
}

/// Put the sidebar highlight back on the file that is actually open.
///
/// Used when a switch is refused (unsaved changes): the tree had already moved its
/// highlight to the clicked row, and leaving it there claims the editor is showing a
/// file it is not. Returns whether the highlight moved.
fn focus_tree_on(editor: &Editor, tree: &mut super::tree::FileTree) -> bool {
    let current = editor.path.clone();
    match tree.rows().iter().position(|r| r.path == current) {
        Some(i) if i != tree.cursor => {
            tree.cursor = i;
            tree.ensure_visible();
            true
        }
        _ => false,
    }
}

/// The directory containing `path`, used to root the tree beside the edited file.
fn parent_dir(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => String::from("/"),
        Some(i) => String::from(&trimmed[..i]),
    }
}
