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
const CHAR_W: usize = 8;
const CHAR_H: usize = 16;
const HEADER_H: usize = 22;
const FOOTER_H: usize = 38;
/// Sidebar width in character cells.
const TREE_COLS: usize = 24;
const BG: u32 = 0x191919;
const SURFACE: u32 = 0x252525;
const TEXT: u32 = 0xF2F2F2;
const MUTED: u32 = 0x9B9B9B;
const BLUE: u32 = 0x2783DE;
const GREEN: u32 = 0x46A171;
const RED: u32 = 0xE56458;
const SELECT: u32 = 0x24496D;

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
}

impl Editor {
    fn new(path: &str, text: &str, config: NanoConfig) -> Self {
        let mut lines: Vec<String> = text.split('\n').map(sanitize_line).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            path: path.to_string(),
            lines,
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
        self.dirty = false;
        self.undo.clear();
        self.redo.clear();
        self.search_hit = None;
        self.quit_armed = false;
        self.status = alloc::format!("opened {}", path);
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
        self.dirty = true;
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
            self.dirty = true;
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
            self.dirty = true;
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
    editor.dirty = false;
    editor.quit_armed = false;
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

fn render(editor: &Editor, caret_on: bool, tree: Option<&super::tree::FileTree>) {
    render_impl(editor, caret_on, tree, false)
}

/// Repaint **only** the caret cell.
///
/// The caret has to change twice per blink period while the rest of the screen
/// stays put. Redrawing everything for that produced frames caught mid-draw — the
/// screen looked like it was tearing — so the blink path touches two cells instead:
/// the old caret (restored by repainting its character) and the new one.
fn render_caret(editor: &Editor, caret_on: bool, tree: Option<&super::tree::FileTree>) {
    render_impl(editor, caret_on, tree, true)
}

fn render_impl(
    editor: &Editor,
    caret_on: bool,
    tree: Option<&super::tree::FileTree>,
    caret_only: bool,
) {
    let (w, h) = framebuffer::dimensions();
    if w == 0 || h == 0 {
        return;
    }
    // The sidebar takes a fixed share of the width, and the text area gets the
    // rest: the tree is context, the text is the work.
    let tree_cols = if tree.is_some() { TREE_COLS } else { 0 };
    let cols = (w / CHAR_W).saturating_sub(tree_cols);
    let rows = h.saturating_sub(HEADER_H + FOOTER_H) / CHAR_H;
    let gutter = if editor.config.line_numbers {
        7usize
    } else {
        0
    };
    let [bg, surface, text, muted, blue, green, red, select] = editor.config.palette();
    cursor::hide();
    let _ = framebuffer::with(|fb| {
        if caret_only {
            // Only the two cells the caret occupies can have changed.
            paint_caret_bar(fb, editor, caret_on, tree, w);
            return;
        }
        fb.fill_rect(0, 0, w, h, bg);
        fb.fill_rect(0, 0, w, HEADER_H, blue);
        // Unsaved work is stated in the title, in words rather than a lone
        // asterisk: "did my edit do anything" should be answerable at a glance,
        // and an `*` next to a long path is not.
        let title = if editor.dirty {
            format!(" nano+  {}   [modified - ^S saves]", editor.path)
        } else {
            format!(" nano+  {}", editor.path)
        };
        fb.draw_text_px(
            6,
            3,
            &clipped_ascii(&title, 0, cols.saturating_sub(1), false),
            if editor.dirty { 0xFFE066 } else { 0xFFFFFF },
            blue,
        );
        // ── Sidebar: the project tree ──
        if let Some(tree) = tree {
            let tw = tree_cols * CHAR_W;
            fb.fill_rect(
                0,
                HEADER_H,
                tw,
                h.saturating_sub(HEADER_H + FOOTER_H),
                surface,
            );
            let header = alloc::format!(" {} ", tree.root());
            fb.draw_text_px(
                2,
                HEADER_H + 2,
                &clipped_ascii(&header, 0, tree_cols.saturating_sub(1), false),
                muted,
                surface,
            );
            let list_y = HEADER_H + CHAR_H + 4;
            let list_rows = rows.saturating_sub(1);
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
                    &clipped_ascii(&text_line, 0, tree_cols.saturating_sub(1), false),
                    fg,
                    bg,
                );
            }
        }
        for vr in 0..rows {
            let r = editor.top + vr;
            let y = HEADER_H + vr * CHAR_H;
            if r >= editor.lines.len() {
                fb.draw_text_px(tree_cols * CHAR_W + 8, y, "~", blue, bg);
                continue;
            }
            if editor.config.line_numbers {
                let num = format!("{:>5} ", r + 1);
                fb.draw_text_px(tree_cols * CHAR_W, y, &num, muted, surface);
            }
            let visible = clipped_ascii(
                &editor.lines[r],
                editor.left,
                cols.saturating_sub(gutter),
                editor.config.show_whitespace,
            );
            if let Some((hr, hc, hlen)) = editor.search_hit {
                if hr == r && hc >= editor.left && hc < editor.left + visible.len() {
                    let before = clipped_ascii(
                        &editor.lines[r],
                        editor.left,
                        hc - editor.left,
                        editor.config.show_whitespace,
                    );
                    let hit =
                        clipped_ascii(&editor.lines[r], hc, hlen, editor.config.show_whitespace);
                    fb.draw_text_px(tree_cols * CHAR_W + gutter * CHAR_W, y, &visible, text, bg);
                    fb.draw_text_px(
                        tree_cols * CHAR_W + (gutter + before.len()) * CHAR_W,
                        y,
                        &hit,
                        0xFFFFFF,
                        select,
                    );
                } else {
                    fb.draw_text_px(tree_cols * CHAR_W + gutter * CHAR_W, y, &visible, text, bg);
                }
            } else {
                fb.draw_text_px(tree_cols * CHAR_W + gutter * CHAR_W, y, &visible, text, bg);
            }
        }
        let foot = h - FOOTER_H;
        paint_footer(fb, editor, foot, w, cols);
        // The caret: a blinking vertical bar in the left pixel column of the cell
        // the next character will occupy — the same shape and color as the shell's
        // caret, so the two read as one thing.
        if editor.prompt.is_none() && editor.row >= editor.top && editor.col >= editor.left {
            let cx = (tree_cols + gutter + editor.col - editor.left) * CHAR_W;
            let cy = HEADER_H + (editor.row - editor.top) * CHAR_H;
            if cx < w && caret_on {
                fb.fill_rect(cx, cy, 2, CHAR_H, super::caret::CARET_COLOR);
            }
        }
    });
    // The console is RAM-backed; nothing is visible until the frame is flushed.
    framebuffer::flush();
    let _ = caret_only;
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
    let mut node = node;
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
            t.view_rows = 20;
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
    let mut caret_cell = caret_cell_of(&editor, tree.as_ref());
    // Cursor position before the current key, so the repaint after it can touch only
    // the rows that key actually changed (usually one).
    let mut before = (editor.row, editor.col);
    render(&editor, last_phase, tree.as_ref());
    'app: loop {
        crate::arch::cpu::halt();
        while let Some(sc) = super::try_read_scancode() {
            let Some(event) = decoder.feed(sc) else {
                continue;
            };
            if editor.prompt.is_some() {
                editor.handle_prompt(event);
                render(&editor, blink(), tree.as_ref());
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
                    render(&editor, blink(), tree.as_ref());
                    continue;
                }
                KeyEvent::Ctrl('s') => {
                    if let Err(e) = save(&mut editor, &node) {
                        editor.status = format!("Error saving: {:?}", e)
                    }
                    render(&editor, blink(), tree.as_ref());
                    continue;
                }
                KeyEvent::Ctrl('b') => {
                    if tree.is_some() {
                        *tree = None;
                        pane = Pane::Text;
                    } else {
                        *tree = Some(super::tree::FileTree::new(&parent_dir(&editor.path)));
                        pane = Pane::Tree;
                    }
                    *tree_focus = pane == Pane::Tree;
                    render(&editor, blink(), tree.as_ref());
                    continue;
                }
                _ => {}
            }

            // ── Sidebar focus: the tree owns the arrow keys and Enter ──
            if pane == Pane::Tree {
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
                        if let Some(t) = tree.as_mut() {
                            if let Some(path) = t.activate() {
                                // Switching files with unsaved changes would silently
                                // drop them, so it is refused instead.
                                if editor.dirty {
                                    editor.status =
                                        "Unsaved changes — ^S to save, or ^B to go back".into();
                                } else if let Ok((n, text)) = open_or_create(&path) {
                                    editor.reload(&path, &text);
                                    node = n;
                                }
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
                    t.ensure_visible();
                }
                render(&editor, blink(), tree.as_ref());
                continue;
            }

            match event {
                KeyEvent::Char(c) => editor.insert(c),
                KeyEvent::Enter => editor.newline(),
                KeyEvent::Backspace => editor.backspace(),
                KeyEvent::Delete => editor.delete(),
                KeyEvent::Left => editor.left(),
                KeyEvent::Right => editor.right(),
                KeyEvent::Up => editor.up(),
                KeyEvent::Down => editor.down(),
                KeyEvent::Home => {
                    editor.col = 0;
                    editor.ensure_visible()
                }
                KeyEvent::End => {
                    editor.col = editor.lines[editor.row].len();
                    editor.ensure_visible()
                }
                KeyEvent::PageUp => {
                    for _ in 0..12 {
                        editor.up()
                    }
                }
                KeyEvent::PageDown => {
                    for _ in 0..12 {
                        editor.down()
                    }
                }
                KeyEvent::Tab => {
                    for _ in 0..editor.config.tab_size {
                        editor.insert(' ')
                    }
                }
                KeyEvent::Escape => {
                    if editor.dirty && !editor.quit_armed {
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
                KeyEvent::Ctrl('c') => editor.copy_line(),
                KeyEvent::Ctrl('k') => editor.cut_line(),
                KeyEvent::Ctrl('u') | KeyEvent::Ctrl('v') => editor.paste_line(),
                _ => {}
            }
            // A keystroke changed one line and the cursor column, so repaint that
            // line and the caret — not the whole frame. Refreshing everything here
            // is what made typing feel slow, and (because a frame takes longer than
            // the gap between keystrokes) left the screen showing half-drawn
            // frames, which is why an edit looked like it had not landed until
            // something else forced a redraw.
            // Repaint the frame on every keystroke. The console is RAM-backed now, so
            // this costs a `memcpy` of the changed rectangle rather than tens of
            // thousands of uncached pixel writes — cheap enough that the per-row
            // special case this replaced is not worth its failure modes (it missed
            // updates whenever the cursor moved without the row changing).
            render(&editor, blink(), tree.as_ref());
            before = (editor.row, editor.col);
        }
        // Blink: repaint **only the caret cell**, and only when the phase changes.
        //
        // `halt()` returns on every timer tick (1 kHz), so a full repaint per wake
        // rebuilt the whole screen a thousand times a second: frames were caught
        // half-drawn (which reads as tearing) and the cost landed between
        // keystrokes, which is what made the editor feel slow. A full frame is drawn
        // when something actually changes — a keystroke, a navigation — and the
        // blink touches two cells, twice per period.
        let phase = blink();
        if phase != last_phase {
            last_phase = phase;
            // Erase the bar where it was, then paint the new phase where it is:
            // the two can differ because the cursor moved between blinks.
            let prev = caret_cell;
            let now = caret_cell_of(&editor, tree.as_ref());
            caret_cell = now;
            let _ = framebuffer::with(|fb| {
                let bg = editor.config.palette()[0];
                if let Some((px, py)) = prev {
                    fb.fill_rect(px, py, 2, CHAR_H, bg);
                }
                if let Some((cx, cy)) = now {
                    if phase {
                        fb.fill_rect(cx, cy, 2, CHAR_H, super::caret::CARET_COLOR);
                    }
                }
            });
        }
    }
    let _ = tree_focus;
}

/// Draw the footer strip: the status line (or the active prompt) and the key hints.
///
/// Split out so a keystroke can refresh it without repainting the text body: the
/// `Ln, Col` readout and the modified flag live here, and they change on almost
/// every key.
fn paint_footer(
    fb: &mut crate::drivers::framebuffer::FramebufferWriter,
    editor: &Editor,
    foot: usize,
    w: usize,
    cols: usize,
) {
    let [_, _, _, _, _, green, red, _] = editor.config.palette();
    let surface = editor.config.palette()[1];
    let text = editor.config.palette()[2];
    fb.fill_rect(0, foot, w, FOOTER_H, surface);
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
        &clipped_ascii(&status, 0, cols.saturating_sub(1), false),
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
        foot + 20,
        &alloc::format!(
            "{}  ^B Files  ^F Find  ^R Replace  ^K Cut  ^U Paste  ^Z Undo",
            dirty_hint
        ),
        if editor.dirty { 0xFFE066 } else { text },
        surface,
    );
}

/// Repaint just the footer strip.
fn render_footer(editor: &Editor) {
    let (w, h) = framebuffer::dimensions();
    if w == 0 || h == 0 {
        return;
    }
    let tree_cols = 0usize;
    let cols = (w / CHAR_W).saturating_sub(tree_cols);
    let foot = h - FOOTER_H;
    let _ = framebuffer::with(|fb| {
        fb.fill_rect(0, foot, w, FOOTER_H, editor.config.palette()[1]);
        paint_footer(fb, editor, foot, w, cols);
    });
    framebuffer::flush();
}

/// Repaint one editor row (and the gutter beside it).
///
/// One keystroke changes one line — the row the cursor is on — and the cursor's
/// own column. Repainting the whole frame for that is what made editing feel slow:
/// a full frame is ~18 000 pixel writes, and `put_pixel` writes the framebuffer
/// through three separate volatile stores per pixel, so on this machine a frame is
/// tens of milliseconds. One row is ~130 writes.
fn paint_editor_row(
    fb: &mut crate::drivers::framebuffer::FramebufferWriter,
    editor: &Editor,
    r: usize,
    vr: usize,
    tree_cols: usize,
    gutter: usize,
    cols: usize,
) {
    let y = HEADER_H + vr * CHAR_H;
    let bg = editor.config.palette()[0];
    let surface = editor.config.palette()[1];
    let text = editor.config.palette()[2];
    let muted = editor.config.palette()[3];
    let blue = editor.config.palette()[4];
    let select = editor.config.palette()[7];
    let x0 = tree_cols * CHAR_W;
    if r >= editor.lines.len() {
        fb.draw_text_px(x0 + 8, y, "~", blue, bg);
        return;
    }
    if editor.config.line_numbers {
        let num = format!("{:>5} ", r + 1);
        fb.draw_text_px(x0, y, &num, muted, surface);
    }
    let visible = clipped_ascii(
        &editor.lines[r],
        editor.left,
        cols.saturating_sub(gutter),
        editor.config.show_whitespace,
    );
    let tx = x0 + gutter * CHAR_W;
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
}

/// Where the caret cell is, in pixels, or `None` when it is off-screen or a
/// prompt is showing. Used by the blink path so it can erase the previous bar.
fn caret_cell_of(editor: &Editor, tree: Option<&super::tree::FileTree>) -> Option<(usize, usize)> {
    if editor.prompt.is_some() || editor.row < editor.top || editor.col < editor.left {
        return None;
    }
    let tree_cols = if tree.is_some() { TREE_COLS } else { 0 };
    let gutter = if editor.config.line_numbers {
        7usize
    } else {
        0
    };
    Some((
        (tree_cols + gutter + editor.col - editor.left) * CHAR_W,
        HEADER_H + (editor.row - editor.top) * CHAR_H,
    ))
}

/// Repaint just the caret bar for the current cursor position.
///
/// Erasing costs nothing extra: the cell is painted with the caret's own cell
/// background first, which is what the full render would have drawn there anyway.
fn paint_caret_bar(
    fb: &mut crate::drivers::framebuffer::FramebufferWriter,
    editor: &Editor,
    caret_on: bool,
    tree: Option<&super::tree::FileTree>,
    w: usize,
) {
    let tree_cols = if tree.is_some() { TREE_COLS } else { 0 };
    let gutter = if editor.config.line_numbers {
        7usize
    } else {
        0
    };
    if editor.prompt.is_some() || editor.row < editor.top || editor.col < editor.left {
        return;
    }
    let bg = editor.config.palette()[0];
    let cx = (tree_cols + gutter + editor.col - editor.left) * CHAR_W;
    let cy = HEADER_H + (editor.row - editor.top) * CHAR_H;
    if cx >= w {
        return;
    }
    // Clear the two pixels the bar occupies, then draw it when lit. Clearing is
    // what erases a caret from the previous phase without repainting the screen.
    fb.fill_rect(cx, cy, 2, CHAR_H, bg);
    if caret_on {
        fb.fill_rect(cx, cy, 2, CHAR_H, super::caret::CARET_COLOR);
    }
}

/// The caret's blink phase, shared with the shell so both carets blink together.
fn blink() -> bool {
    super::caret::Caret::blink_on(crate::task::scheduler::ticks())
}

/// The directory containing `path`, used to root the tree beside the edited file.
fn parent_dir(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => String::from("/"),
        Some(i) => String::from(&trimmed[..i]),
    }
}
