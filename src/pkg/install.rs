//! Pure install-path normalization and a host-testable install model
//! (design component 10, `Package_Installer`).
//!
//! This module factors the *selection semantics* of the package installer into pure,
//! `core` + `alloc` logic (R11.6) so the same source is exercised on the host by
//! property test P26 and compiled identically by the `#![no_std]` kernel. The
//! effectful ext2 install (`install_data_tar`) is added by a later task (14.3) and
//! reuses [`normalize_entry_path`] from here.
//!
//! [`normalize_entry_path`] turns an archived tar path into either a safe,
//! root-relative path or a `SkipUnsafe` verdict for entries that would escape the
//! installation root via `..` (R10.1, R10.8). [`install_model`] is a *pure* model of
//! the effectful install: it folds a slice of [`TarEntry`] records into the
//! `path -> content` map that a faithful install would produce, capturing R10.5/R10.6/
//! R10.7/R10.8 (regular-files-only, skip-unsafe, last-writer-wins).
//!
//! ## Cross-crate module path
//!
//! [`TarEntry`]/[`TarType`] are imported via `super::tar`, which resolves in BOTH
//! crates: in the kernel this module is `crate::pkg::install`, so `super` is
//! `crate::pkg` (which declares `pub mod tar;`); in `host-tests` it is included at the
//! crate root as `crate::install`, so `super` is the crate root (which also declares
//! `pub mod tar;`). One source, two crates, no shim.
#![allow(dead_code)]

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use super::tar::{TarEntry, TarType};

/// The outcome of normalizing an archived tar path against the installation root.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum NormPath {
    /// A safe, root-relative path (components joined with `/`).
    Keep(String),
    /// The entry must be skipped: its path escapes the root via `..`, or it
    /// normalizes to the empty path (R10.8).
    SkipUnsafe,
}

/// Normalize an archived tar path to a safe, root-relative path (R10.1, R10.8).
///
/// The archived path is first stripped of any leading `./` and `/` components,
/// repeatedly, so the result is interpreted relative to the installation root. The
/// remainder is split on `/`; empty and `.` components are dropped. Each `..`
/// component pops the most recent kept component; if a `..` would pop above the root
/// (the component stack is empty) the path escapes and the entry is rejected with
/// [`NormPath::SkipUnsafe`]. An empty result (e.g. `"."` or `"/"`) is likewise
/// [`NormPath::SkipUnsafe`]. Otherwise the kept components are rejoined with `/` and
/// returned as [`NormPath::Keep`]. Pure and panic-free.
pub fn normalize_entry_path(archived: &str) -> NormPath {
    // Strip leading "./" and "/" repeatedly so the path is root-relative.
    let mut s = archived;
    loop {
        if let Some(rest) = s.strip_prefix("./") {
            s = rest;
        } else if let Some(rest) = s.strip_prefix('/') {
            s = rest;
        } else {
            break;
        }
    }

    // Resolve components, rejecting any `..` that would escape above the root.
    let mut stack: Vec<&str> = Vec::new();
    for comp in s.split('/') {
        match comp {
            "" | "." => continue,
            ".." => {
                if stack.pop().is_none() {
                    // Escapes above the installation root.
                    return NormPath::SkipUnsafe;
                }
            }
            other => stack.push(other),
        }
    }

    if stack.is_empty() {
        return NormPath::SkipUnsafe;
    }

    // Rejoin the surviving components with '/'.
    let mut out = String::new();
    for (i, comp) in stack.iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(comp);
    }
    NormPath::Keep(out)
}

/// Pure model of the effectful package install (R10.5/R10.6/R10.7/R10.8).
///
/// Folds the tar entries into the `path -> content` map that a faithful install onto
/// a fresh filesystem root would produce, capturing the installer's *selection*
/// semantics without touching any real filesystem (the effectful ext2 install lives
/// in task 14.3):
///
///   * Only [`TarType::Regular`] entries are installed; directories and other entry
///     kinds are skipped (R10.6).
///   * Each path is normalized via [`normalize_entry_path`]; [`NormPath::SkipUnsafe`]
///     entries (escaping `..` or empty) are skipped (R10.8).
///   * Content is preserved byte-for-byte and keyed by the normalized path (R10.5).
///   * When several entries normalize to the same path, the LAST entry wins (R10.7),
///     because entries are processed in order and each insert overwrites the prior.
///
/// The resulting [`BTreeMap`] is the model filesystem property test P26 asserts over.
/// Pure and panic-free.
pub fn install_model<'a>(entries: &[TarEntry<'a>]) -> BTreeMap<String, Vec<u8>> {
    let mut model: BTreeMap<String, Vec<u8>> = BTreeMap::new();

    for entry in entries {
        // Only regular files are installed (R10.6).
        if entry.kind != TarType::Regular {
            continue;
        }
        // Skip entries whose normalized path escapes the root or is empty (R10.8).
        match normalize_entry_path(entry.path) {
            NormPath::Keep(path) => {
                // Last writer wins (R10.7); content preserved verbatim (R10.5).
                model.insert(path, entry.content.to_vec());
            }
            NormPath::SkipUnsafe => continue,
        }
    }

    model
}
