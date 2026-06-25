//! `History`: a bounded ring buffer of past commands with up/down recall,
//! in-progress line save/restore, and consecutive-dedup.
//!
//! Pure logic — no console or VFS I/O — so it can be property-tested in
//! isolation (design Property 23). The interactive loop drives it: Up calls
//! [`History::recall_prev`], Down calls [`History::recall_next`], and Enter
//! calls [`History::push`] (for non-empty lines) then [`History::reset_nav`].

use alloc::collections::VecDeque;
use alloc::string::String;

/// Maximum number of retained history entries. Combined with the line editor's
/// `MAX_CMD_LEN` (256) this bounds history memory to `CAP * MAX_CMD_LEN`
/// (R11.1).
const CAP: usize = 64;

/// A bounded ring buffer of past command lines with up/down recall and
/// in-progress line save/restore.
///
/// Storage layout: `entries` holds the retained lines with the **oldest at the
/// front** and the **newest at the back** (`push_back`/`pop_front`).
///
/// Navigation index convention: `nav` is `None` while the user is editing the
/// live (in-progress) line. While navigating, `nav` is `Some(i)` where `i`
/// counts **from the newest entry**: `i == 0` is the newest entry,
/// `i == entries.len() - 1` is the oldest. The mapping to the underlying
/// `VecDeque` index is `entries.len() - 1 - i`.
#[allow(dead_code)]
pub struct History {
    /// Retained lines, oldest at front, newest at back. Length is `<= CAP`.
    entries: VecDeque<String>,
    /// `None` = editing the live line; `Some(i)` = navigating, `i` counted
    /// from the newest entry (0 = newest).
    nav: Option<usize>,
    /// The in-progress line stashed on the first `recall_prev`, so moving
    /// past the newest entry with `recall_next` can restore exactly what the
    /// user was typing.
    saved: String,
}

#[allow(dead_code)]
impl History {
    /// Create an empty history with no navigation in progress.
    pub fn new() -> Self {
        History {
            entries: VecDeque::new(),
            nav: None,
            saved: String::new(),
        }
    }

    /// Record a command line (R2.1, R2.4, R2.5).
    ///
    /// - Empty lines are ignored.
    /// - A line equal to the most recent entry is skipped (consecutive
    ///   de-duplication).
    /// - Otherwise the line is appended as the newest entry; if the buffer
    ///   would exceed `CAP`, the oldest entry (front) is dropped.
    ///
    /// Recording always ends any in-progress navigation: `nav` is reset to
    /// `None` and the stashed line is cleared.
    pub fn push(&mut self, line: &str) {
        if line.is_empty() {
            // Still leave navigation in a clean state on Enter.
            self.nav = None;
            self.saved.clear();
            return;
        }

        // Consecutive de-dup: skip if identical to the most recent entry.
        if self.entries.back().map(|s| s.as_str()) != Some(line) {
            self.entries.push_back(String::from(line));
            if self.entries.len() > CAP {
                self.entries.pop_front();
            }
        }

        self.nav = None;
        self.saved.clear();
    }

    /// Move toward OLDER entries (Up key, R2.2).
    ///
    /// On the first call after editing (`nav == None`), the caller's current
    /// in-progress line is stashed into `saved` so it can be restored later,
    /// and the newest entry is returned. Each subsequent call steps one entry
    /// older. Returns `None` (leaving `nav` unchanged) when already at the
    /// oldest entry or when there are no entries.
    pub fn recall_prev(&mut self, current: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }

        let next_idx = match self.nav {
            None => {
                // First recall: stash the live line and select the newest.
                self.saved.clear();
                self.saved.push_str(current);
                0
            }
            Some(i) => {
                // Already at the oldest entry: nothing older to show.
                if i + 1 >= self.entries.len() {
                    return None;
                }
                i + 1
            }
        };

        self.nav = Some(next_idx);
        // Map newest-relative index to the VecDeque index (newest at back).
        let vec_idx = self.entries.len() - 1 - next_idx;
        self.entries.get(vec_idx).map(|s| s.as_str())
    }

    /// Move toward NEWER entries (Down key, R2.3).
    ///
    /// Returns the next-newer entry while one exists. When stepping past the
    /// newest entry, navigation ends (`nav` set to `None`) and `None` is
    /// returned to signal the caller to restore the stashed line via
    /// [`History::saved_line`]. Returns `None` immediately if not currently
    /// navigating.
    pub fn recall_next(&mut self) -> Option<&str> {
        match self.nav {
            None => None,
            Some(0) => {
                // Past the newest entry: back to the live line.
                self.nav = None;
                None
            }
            Some(i) => {
                let next_idx = i - 1;
                self.nav = Some(next_idx);
                let vec_idx = self.entries.len() - 1 - next_idx;
                self.entries.get(vec_idx).map(|s| s.as_str())
            }
        }
    }

    /// Reset navigation state (called on Enter). Clears any in-progress
    /// navigation and the stashed line.
    pub fn reset_nav(&mut self) {
        self.nav = None;
        self.saved.clear();
    }

    /// The in-progress line stashed on the first `recall_prev`. Used by the
    /// caller to restore the live line when `recall_next` steps past the
    /// newest entry.
    pub fn saved_line(&self) -> &str {
        &self.saved
    }
}
