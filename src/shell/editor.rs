//! `LineEditor`: a buffer + cursor model with pure insert/delete/move
//! operations.
//!
//! The cursor is tracked in **character** units (not bytes) so the editor
//! stays correct around multi-byte UTF-8 (R11.6). Every operation preserves
//! two invariants:
//!   * `cursor <= buf.chars().count()`
//!   * `buf.len() <= MAX_CMD_LEN` (byte length is bounded, R11.1)
//! and never splits a multi-byte UTF-8 character — all indexing maps a
//! character index to a byte index via `char_indices`.

use alloc::string::String;

/// Maximum command line length in bytes. Matches the shell constant in
/// `shell/mod.rs` so the editor and the legacy read loop agree on the cap.
const MAX_CMD_LEN: usize = 256;

/// An editable line of text with a cursor positioned between characters.
#[allow(dead_code)]
pub struct LineEditor {
    buf: String,
    /// Cursor position in character units, in `0..=buf.chars().count()`.
    cursor: usize,
}

#[allow(dead_code)]
impl LineEditor {
    /// Create an empty editor with the cursor at the start.
    pub fn new() -> Self {
        LineEditor {
            buf: String::new(),
            cursor: 0,
        }
    }

    /// Create an editor seeded with `line`, cursor positioned at the end.
    pub fn from_line(line: &str) -> Self {
        let mut buf = String::new();
        let cursor = Self::push_bounded(&mut buf, line);
        LineEditor { buf, cursor }
    }

    /// Map a character index into the corresponding byte index in `buf`.
    /// `char_idx == char count` maps to `buf.len()` (one past the end).
    fn byte_index(&self, char_idx: usize) -> usize {
        match self.buf.char_indices().nth(char_idx) {
            Some((byte_idx, _)) => byte_idx,
            None => self.buf.len(),
        }
    }

    /// Append as much of `line` as fits within `MAX_CMD_LEN` bytes onto `buf`,
    /// never splitting a character, and return the number of characters added.
    /// Used by constructors/`set_line` so seeded lines respect the byte cap.
    fn push_bounded(buf: &mut String, line: &str) -> usize {
        let mut count = 0;
        for ch in line.chars() {
            if buf.len() + ch.len_utf8() > MAX_CMD_LEN {
                break;
            }
            buf.push(ch);
            count += 1;
        }
        count
    }

    /// Insert `ch` at the cursor, shifting the suffix right, and advance the
    /// cursor by one (R1.3). The insert is dropped if it would push the buffer
    /// past `MAX_CMD_LEN` bytes (R11.1).
    pub fn insert(&mut self, ch: char) {
        if self.buf.len() + ch.len_utf8() > MAX_CMD_LEN {
            return;
        }
        let at = self.byte_index(self.cursor);
        self.buf.insert(at, ch);
        self.cursor += 1;
    }

    /// Backspace: remove the character before the cursor and move left
    /// (R1.4). Returns `true` if the buffer changed.
    pub fn delete_back(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let at = self.byte_index(self.cursor - 1);
        self.buf.remove(at);
        self.cursor -= 1;
        true
    }

    /// Delete: remove the character at the cursor, leaving the cursor in place
    /// (R1.5). Returns `true` if the buffer changed.
    pub fn delete_fwd(&mut self) -> bool {
        if self.cursor >= self.char_count() {
            return false;
        }
        let at = self.byte_index(self.cursor);
        self.buf.remove(at);
        true
    }

    /// Move the cursor one character left, clamping at the start (R1.1).
    /// Returns `true` if the cursor moved.
    pub fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    /// Move the cursor one character right, clamping at the end (R1.1).
    /// Returns `true` if the cursor moved.
    pub fn move_right(&mut self) -> bool {
        if self.cursor >= self.char_count() {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// Move the cursor to the start of the line (R1.2).
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the end of the line (R1.2).
    pub fn move_end(&mut self) {
        self.cursor = self.char_count();
    }

    /// Replace the buffer with `line` and place the cursor at the end. Used for
    /// history recall and completion. The new line is bounded to `MAX_CMD_LEN`
    /// bytes, never splitting a character.
    pub fn set_line(&mut self, line: &str) {
        self.buf.clear();
        self.cursor = Self::push_bounded(&mut self.buf, line);
    }

    /// The current buffer contents.
    pub fn buffer(&self) -> &str {
        &self.buf
    }

    /// The cursor position in character units.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Number of characters in the buffer (cursor upper bound).
    fn char_count(&self) -> usize {
        self.buf.chars().count()
    }
}
