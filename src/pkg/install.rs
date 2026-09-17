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

use super::tar::{effective_path, TarEntry, TarType};

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

/// One planned install operation, in execution order (issue #18).
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum InstallOp<'a> {
    /// Create (or replace) a regular file with this content and mode.
    File {
        path: String,
        content: &'a [u8],
        mode: u32,
    },
    /// Create a symbolic link. `target` is the archive string **verbatim** — the
    /// extractor never normalizes or resolves it, so a link keeps working the way
    /// its author wrote it.
    Symlink { path: String, target: String },
    /// Create a hard link. `target` is a root-relative path that must exist when
    /// the operation runs (it may come from this archive or from an earlier
    /// package).
    Hardlink { path: String, target: String },
}

/// What an archive's members mean for the installer, computed purely so the
/// ordering rules can be property-tested on the host (issue #18, `EXT2-LINKS.md`
/// §5).
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct InstallPlan<'a> {
    /// Operations in execution order: regular files and symlinks in archive
    /// order, then hard links in dependency order.
    pub ops: Vec<InstallOp<'a>>,
    /// Hard links whose target is **not** provided by this archive: the target has
    /// to exist in the filesystem already (an earlier package, or a member the
    /// parser skipped). They are still emitted (last) for the effectful installer
    /// to try; anything left is reported and skipped, never copied.
    pub deferred: Vec<String>,
    /// Hard links that reference each other in a cycle inside this archive and so
    /// can never be created; skipped with one diagnostic each.
    pub unresolved: Vec<String>,
    /// Members dropped because their path escapes the install root (R10.8).
    pub skipped_unsafe: usize,
    /// Link members with an empty target: creating them would be meaningless.
    pub skipped_empty_target: usize,
    /// Members of a kind this installer does not create (device, fifo, ...).
    pub skipped_other: usize,
}

/// Turn tar members into the ordered operations a faithful install performs.
///
/// The rules (`EXT2-LINKS.md` §5.6):
///
///   * only `Regular`, `Symlink` and `Hardlink` members produce operations;
///     directories are created implicitly as parents and other kinds are skipped;
///   * a **symlink** is planned where it appears — a dangling target is legal and
///     common (`alternatives`-style aliases), so nothing is deferred for it;
///   * a **hard link** needs its target to exist first, so hard links are planned
///     *after* every file and symlink and ordered among themselves by a fixpoint
///     over the paths this archive creates (chains `a -> b -> c` work);
///   * hard links that reference each other in a cycle, or that no member
///     provides, are collected in [`InstallPlan::unresolved`] /
///     [`InstallPlan::deferred`] instead of being silently turned into copies;
///   * paths escaping the root (`..`), and link members with an empty target, are
///     counted and dropped.
///
/// Pure and allocation-bounded.
pub fn plan_install<'a>(entries: &[TarEntry<'a>]) -> InstallPlan<'a> {
    let mut plan = InstallPlan::default();
    // Paths this archive creates, used to order hard links.
    let mut created: Vec<String> = Vec::new();
    // Hard links still to be ordered: (member path, normalized target).
    let mut pending: Vec<(String, String)> = Vec::new();

    for entry in entries {
        // Long paths arrive through the GNU `'L'` header or the ustar `prefix`
        // field; `effective_path` joins the latter.
        let raw = effective_path(entry);
        let path = match normalize_entry_path(&raw) {
            NormPath::Keep(p) => p,
            NormPath::SkipUnsafe => {
                plan.skipped_unsafe += 1;
                continue;
            }
        };
        match entry.kind {
            TarType::Regular => {
                plan.ops.push(InstallOp::File {
                    path: path.clone(),
                    content: entry.content,
                    mode: entry.mode,
                });
                created.push(path);
            }
            TarType::Symlink => {
                if entry.link_target.is_empty() {
                    plan.skipped_empty_target += 1;
                    continue;
                }
                // Verbatim: no normalization, no existence check, no deferral.
                plan.ops.push(InstallOp::Symlink {
                    path: path.clone(),
                    target: String::from(entry.link_target),
                });
                // A symlink is a directory entry too, so a hard link may reference
                // it (Linux `link(2)` does not follow the final component).
                created.push(path);
            }
            TarType::Hardlink => {
                if entry.link_target.is_empty() {
                    plan.skipped_empty_target += 1;
                    continue;
                }
                // Tar stores the target as the *archive* path of the first
                // occurrence, so it is normalized like any other member path.
                match normalize_entry_path(entry.link_target) {
                    NormPath::Keep(t) => pending.push((path, t)),
                    NormPath::SkipUnsafe => {
                        plan.skipped_unsafe += 1;
                        continue;
                    }
                }
            }
            TarType::Directory | TarType::Other => {
                plan.skipped_other += 1;
            }
        }
    }

    // Fixpoint over the pending hard links: emit one whose target already exists,
    // then treat its own path as created (so chains resolve). No progress in a
    // round means the rest form cycles.
    loop {
        let mut progressed = false;
        let mut remaining: Vec<(String, String)> = Vec::new();
        for (path, target) in pending {
            if created.iter().any(|c| *c == target) {
                created.push(path.clone());
                plan.ops.push(InstallOp::Hardlink { path, target });
                progressed = true;
            } else {
                remaining.push((path, target));
            }
        }
        pending = remaining;
        if !progressed {
            break;
        }
    }

    // Everything left references a path this archive does not create. Two cases:
    //
    //   * the target is another *pending hard link's* path — a cycle inside the
    //     archive (a -> b -> a): it can never be satisfied and is reported;
    //   * otherwise the target must already exist in the filesystem (a member of
    //     an earlier package), which only the effectful installer can decide, so
    //     the operation is emitted last and recorded as deferred.
    for (path, target) in pending.iter() {
        let in_cycle = pending
            .iter()
            .any(|(other, _)| other != path && *other == *target);
        if in_cycle {
            plan.unresolved.push(path.clone());
        } else {
            plan.deferred.push(path.clone());
        }
    }
    for (path, target) in pending {
        plan.ops.push(InstallOp::Hardlink { path, target });
    }
    plan
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
