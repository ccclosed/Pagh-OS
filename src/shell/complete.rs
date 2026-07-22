//! Tab completion: command-name and path completion plus longest-common-prefix
//! logic (R3).
//!
//! Everything here is **pure logic over plain data** — no console, keyboard, or
//! VFS calls. Directory entries are supplied by the caller (the interactive
//! loop reads them from the VFS and hands them in as a `&[&str]`), so the
//! matching and longest-common-prefix computation can be exercised
//! deterministically by the in-kernel property tests (Property 24, task 6.2).
//!
//! These items are consumed by the interactive loop wired in task 10; until
//! then they are not referenced internally, hence the `#[allow(dead_code)]`
//! annotations that keep the `#![no_std]` build warning-free.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The char-wise longest common prefix of `candidates`.
///
/// The result is guaranteed to be (a) a prefix of *every* candidate and (b)
/// **maximal** — there is no longer string that is also a prefix of every
/// candidate. Comparison operates on Unicode scalar values (`char`s), never raw
/// bytes, so the returned `String` is always valid UTF-8 and never splits a
/// multi-byte character (R11.6).
///
/// Edge cases:
/// - an empty slice returns `""` (there is no common content),
/// - a single candidate returns that candidate unchanged.
#[allow(dead_code)]
pub fn longest_common_prefix(candidates: &[&str]) -> String {
    // No candidates: nothing is common.
    let (first, rest) = match candidates.split_first() {
        Some(parts) => parts,
        None => return String::new(),
    };

    // Walk the chars of the first candidate; the prefix can never be longer
    // than the shortest candidate, and the first candidate bounds it.
    let mut prefix = String::new();
    for (idx, ch) in first.char_indices() {
        // Every other candidate must have exactly this char at this char
        // position. We compare by char, advancing each candidate's iterator in
        // lockstep with `idx` via `char_indices` lookups.
        let all_match = rest.iter().all(|cand| {
            // The char of `cand` starting at the same char index as `ch`.
            cand[idx..].chars().next() == Some(ch)
        });

        if all_match {
            prefix.push(ch);
        } else {
            break;
        }
    }

    prefix
}

/// The outcome of a completion request (R3.3/R3.4/R3.5).
///
/// Contract for the caller (the interactive loop, task 10):
/// - [`Completion::None`] — no candidate matched; leave the line unchanged
///   (R3.5).
/// - [`Completion::Single`] — exactly one candidate matched; the wrapped string
///   is the **full completed token/segment**, so the application makes the
///   token equal that string (R3.3, Property 24).
/// - [`Completion::Multiple`] — several candidates matched; `lcp` is the
///   longest common prefix to extend the token to, and `candidates` is the list
///   to display. Every entry in `candidates` (and `lcp` itself) starts with the
///   originally typed prefix/segment (R3.4).
#[allow(dead_code)]
pub enum Completion {
    /// No match — caller leaves the line unchanged (R3.5).
    None,
    /// Unique match — the wrapped value is the full completed token (R3.3).
    Single(String),
    /// Several matches — extend to `lcp`, then display `candidates` (R3.4).
    Multiple {
        /// Longest common prefix of all matches (>= the typed prefix).
        lcp: String,
        /// The matching candidate strings, to be listed for the user.
        candidates: Vec<String>,
    },
}

/// Build a [`Completion`] from the set of matching strings.
///
/// Shared by [`complete_command`] and [`complete_path`]: 0 matches → `None`,
/// 1 → `Single`, `>= 2` → `Multiple { lcp, candidates }`.
fn from_matches(matches: Vec<&str>) -> Completion {
    match matches.len() {
        0 => Completion::None,
        1 => Completion::Single(matches[0].to_string()),
        _ => {
            let lcp = longest_common_prefix(&matches);
            let candidates = matches.iter().map(|s| s.to_string()).collect();
            Completion::Multiple { lcp, candidates }
        }
    }
}

/// Complete a command name against the registry (R3.1).
///
/// Collects every registered command name (via
/// [`crate::shell::registry::command_names`]) that starts with `prefix`, then:
/// 0 matches → [`Completion::None`] (R3.5), 1 → [`Completion::Single`] (R3.3),
/// `>= 2` → [`Completion::Multiple`] with the longest common prefix and the
/// candidate list (R3.4). Every returned candidate starts with `prefix`.
#[allow(dead_code)]
pub fn complete_command(prefix: &str) -> Completion {
    let matches: Vec<&str> = crate::shell::registry::command_names()
        .filter(|name| name.starts_with(prefix))
        .collect();
    from_matches(matches)
}

/// Complete a filesystem path segment against supplied directory entries (R3.2).
///
/// This is **pure**: `dir_entries` are the names in the relevant directory,
/// supplied by the caller as data (no VFS calls happen here). Matching is done
/// against the **last path segment** of `partial` — i.e. the text after the
/// final `/` — so the caller is responsible for splicing the completed segment
/// back into the full token. The returned [`Completion::Single`] /
/// [`Completion::Multiple`] therefore carry the matched **entry name(s)**, each
/// of which starts with that last segment (Property 24).
///
/// `cwd` is kept for signature compatibility with the design; the prefix logic
/// here depends only on `partial`'s last segment and the supplied entries.
#[allow(dead_code)]
pub fn complete_path(cwd: &str, partial: &str, dir_entries: &[&str]) -> Completion {
    let _ = cwd; // retained per design signature; matching uses `partial`.

    // The segment being completed is the text after the final '/'. When there
    // is no '/', the whole `partial` is the segment.
    let segment = match partial.rfind('/') {
        Some(idx) => &partial[idx + 1..],
        None => partial,
    };

    let matches: Vec<&str> = dir_entries
        .iter()
        .copied()
        .filter(|entry| entry.starts_with(segment))
        .collect();
    from_matches(matches)
}
