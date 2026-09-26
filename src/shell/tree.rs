//! A file-tree sidebar for the editor.
//!
//! The editor had no way to see the filesystem while editing: you opened one path
//! and that was the whole context. This is the sidebar half of the "looks like an
//! IDE" change — a project tree beside the text, with directories that open and
//! close, keyboard navigation, and Enter to swap the edited file.
//!
//! The tree is stored **flattened**: `rows` holds exactly what is displayed, in
//! order, each row carrying its own indent depth. Drawing and moving the cursor are
//! then both a walk over one `Vec`, and collapsing a directory is a rebuild rather
//! than a recursive skip. The visible list is rebuilt from the filesystem, so a
//! file created by a command (or by `apt install`) shows up without a refresh.
//!
//! Sizing is bounded on purpose: directories are sorted, hidden dotfiles are
//! skipped (the shell's own dotfiles are not project content), and the depth is
//! capped so a symlink loop cannot grow the list without end.

use crate::vfs;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// One visible line of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Display name (no path).
    pub name: String,
    /// Absolute path, used to open the file.
    pub path: String,
    /// Indent level: 0 for an entry of the root, 1 for its children, …
    pub depth: usize,
    pub is_dir: bool,
    /// Whether the entry is expanded (always false for a file).
    pub expanded: bool,
}

/// The sidebar: a root path, the expanded set, and the flattened visible rows.
pub struct FileTree {
    root: String,
    rows: Vec<Row>,
    /// Index into `rows` of the highlighted entry.
    pub cursor: usize,
    /// Top offset for scrolling, maintained by [`Self::ensure_visible`].
    pub top: usize,
    /// How many rows fit on screen; set by the renderer each frame.
    pub view_rows: usize,
    expanded: Vec<String>,
}

/// Directories deeper than this are not descended into. Symlinks to a parent would
/// otherwise make the visible list unbounded.
const MAX_DEPTH: usize = 8;

/// Entries listed per directory. A `/usr/bin`-sized directory is not project
/// content, and an unbounded row list is a memory leak with a nice name.
const MAX_ENTRIES: usize = 512;

impl FileTree {
    /// Build a tree rooted at `root` (a directory path), with `root` expanded.
    pub fn new(root: &str) -> Self {
        let mut t = FileTree {
            root: root.to_string(),
            rows: Vec::new(),
            cursor: 0,
            top: 0,
            view_rows: 1,
            expanded: alloc::vec![root.to_string()],
        };
        t.rebuild();
        t
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The highlighted entry, if the tree is non-empty.
    pub fn selected(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }

    /// Rebuild the visible rows from the filesystem, keeping the cursor on the same
    /// path when that path still exists.
    pub fn rebuild(&mut self) {
        let keep = self.selected().map(|r| r.path.clone());
        let mut rows = Vec::new();
        self.walk(&self.root.clone(), 0, &mut rows);
        self.rows = rows;
        if let Some(path) = keep {
            if let Some(i) = self.rows.iter().position(|r| r.path == path) {
                self.cursor = i;
            }
        }
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
    }

    /// Append `dir`'s entries to `out`, then recurse into expanded subdirectories.
    ///
    /// Errors are swallowed: a directory that cannot be read simply contributes no
    /// rows. The editor is not the place to report a filesystem problem, and a
    /// sidebar that refuses to draw because one entry is unreadable is worse than
    /// one that is short a row.
    fn walk(&self, dir: &str, depth: usize, out: &mut Vec<Row>) {
        if depth > MAX_DEPTH || out.len() >= MAX_ENTRIES {
            return;
        }
        let Ok(node) = vfs::lookup_path(dir) else {
            return;
        };
        let Ok(children) = node.readdir() else {
            return;
        };
        let mut entries: Vec<(String, bool, String)> = Vec::new();
        for child in children.iter() {
            let name = child.name();
            if name.is_empty() || name == "." || name == ".." || name.starts_with('.') {
                continue;
            }
            let path = join(dir, &name);
            entries.push((name.to_string(), child.is_directory(), path));
        }
        // Directories first, then alphabetically: the order a project explorer shows.
        entries.sort_by(|a, b| match (a.1, b.1) {
            (true, false) => core::cmp::Ordering::Less,
            (false, true) => core::cmp::Ordering::Greater,
            _ => a.0.cmp(&b.0),
        });
        for (name, is_dir, path) in entries {
            if out.len() >= MAX_ENTRIES {
                return;
            }
            let expanded = is_dir && self.is_expanded(&path);
            out.push(Row {
                name,
                path: path.clone(),
                depth,
                is_dir,
                expanded,
            });
            if expanded {
                self.walk(&path, depth + 1, out);
            }
        }
    }

    fn is_expanded(&self, path: &str) -> bool {
        self.expanded.iter().any(|p| p == path)
    }

    /// Open the highlighted directory, or close it when it is already open.
    /// Returns the path to open when the highlight is a file.
    pub fn activate(&mut self) -> Option<String> {
        let row = self.selected()?.clone();
        if row.is_dir {
            if let Some(i) = self.expanded.iter().position(|p| *p == row.path) {
                self.expanded.remove(i);
            } else {
                self.expanded.push(row.path.clone());
            }
            self.rebuild();
            None
        } else {
            Some(row.path)
        }
    }

    /// Collapse the highlighted directory, or move to the parent directory.
    /// Returns `true` when the tree changed.
    pub fn collapse_or_parent(&mut self) -> bool {
        let Some(row) = self.selected().cloned() else {
            return false;
        };
        if row.is_dir && row.expanded {
            if let Some(i) = self.expanded.iter().position(|p| *p == row.path) {
                self.expanded.remove(i);
            }
            self.rebuild();
            return true;
        }
        // Jump to the enclosing directory's row.
        let depth = row.depth;
        if depth == 0 {
            return false;
        }
        if let Some(i) = self.rows[..self.cursor]
            .iter()
            .rposition(|r| r.is_dir && r.depth + 1 == depth)
        {
            self.cursor = i;
            return true;
        }
        false
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 < self.rows.len() {
            self.cursor += 1;
        }
    }

    /// Keep the cursor inside the visible window.
    pub fn ensure_visible(&mut self) {
        let view = self.view_rows.max(1);
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + view {
            self.top = self.cursor + 1 - view;
        }
    }
}

/// Join a directory path and a name, tolerating a trailing slash on the directory.
fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        alloc::format!("{}{}", dir, name)
    } else {
        alloc::format!("{}/{}", dir, name)
    }
}
