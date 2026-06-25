//! Pure dependency resolver for an `apt install <name>` feature (the planning
//! side, layered over [`super::apt_index::PackageIndex`]).
//!
//! Given a parsed repository [`PackageIndex`], a target package name, and the set
//! of already-installed package names, [`resolve_install`] returns the list of
//! package names to install in **dependency-first (topological) order** — every
//! dependency appears before the package that needs it, and the original
//! `target` appears last among its own subtree. Callers install in the returned
//! order.
//!
//! ## Cross-crate module path
//!
//! The index types are imported via `super::apt_index`, which resolves in BOTH
//! crates: in the kernel this module is `crate::pkg::apt_resolve`, so `super` is
//! `crate::pkg` (which declares `pub mod apt_index;`); in `host-tests` it is
//! included at the crate root, so `super` is the crate root (which also declares
//! `pub mod apt_index;`). One source, two crates, no shim — the same convention
//! `install.rs` uses for `super::tar`.
//!
//! ## Documented simplifications
//!
//! This resolver is intentionally a pragmatic, real-world-usable approximation,
//! not a full SAT-based apt solver:
//!
//!   * **Version constraints are ignored.** A `Depends: libc6 (>= 2.34)` is
//!     treated as a plain dependency on `libc6`; the only (newest/sole) record in
//!     the index for that name is used. The parser already stripped `(...)`.
//!   * **Pre-Depends are merged into Depends** at parse time (see
//!     [`super::apt_index::PkgRecord::depends`]); the resolver does not enforce
//!     the stricter pre-dependency *unpack* ordering beyond ordinary topological
//!     order.
//!   * **Missing dependencies are treated as already-satisfied essentials.** If a
//!     dependency group has no alternative that is installed *or* present in the
//!     index, the group is silently skipped rather than failing the install. In a
//!     real repo such a name is almost always an Essential/base package assumed
//!     present (e.g. `libc6`, `dpkg`); failing the whole transaction on it would
//!     make ordinary installs unusable.
//!   * **Alternatives pick the first satisfiable option.** For `a | b` the first
//!     alternative that is already installed, or failing that the first that
//!     exists in the index (real or via `Provides`), is chosen; if one is already
//!     installed the group needs nothing further.
//!   * **Virtual packages** are resolved through the index `Provides` map; the
//!     *providing real package* is what gets scheduled for install.

#![allow(dead_code)]

use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::apt_index::PackageIndex;

/// Errors from [`resolve_install`].
#[derive(Debug)]
pub enum AptError {
    /// The requested target is neither a real package nor a provided virtual
    /// name in the index. Carries the offending target name.
    NotFound(String),
}

/// Resolve the install plan for `target` against `index`, given the set of
/// `already_installed` package names.
///
/// Returns the package names to install in dependency-first (topological) order:
/// for any package in the result, all of its (resolvable, not-already-installed)
/// dependencies appear earlier in the list, and `target` (if it needs installing)
/// appears after its own subtree. Already-installed names are excluded, and the
/// result is de-duplicated.
///
/// Returns [`AptError::NotFound`] only if `target` itself cannot be satisfied by
/// the index (neither a real package nor a provided virtual name) *and* it is not
/// already installed. See the module docs for the full list of simplifications;
/// in particular, missing *transitive* dependencies are skipped, not errors.
pub fn resolve_install(
    index: &PackageIndex,
    target: &str,
    already_installed: &BTreeSet<String>,
) -> Result<Vec<String>, AptError> {
    // If the target is already installed, nothing to do.
    if already_installed.contains(target) {
        return Ok(Vec::new());
    }

    // The target must be satisfiable by the index (real or virtual).
    if index.get_provider(target).is_none() {
        return Err(AptError::NotFound(target.to_string()));
    }

    let mut order: Vec<String> = Vec::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut on_stack: BTreeSet<String> = BTreeSet::new();

    visit(
        index,
        target,
        already_installed,
        &mut visited,
        &mut on_stack,
        &mut order,
    );

    Ok(order)
}

/// Post-order DFS that appends each real package to `order` after its
/// dependencies. `name` may be a real or virtual name; it is resolved to its
/// providing real record before recursing/emitting.
fn visit(
    index: &PackageIndex,
    name: &str,
    already_installed: &BTreeSet<String>,
    visited: &mut BTreeSet<String>,
    on_stack: &mut BTreeSet<String>,
    order: &mut Vec<String>,
) {
    // Resolve to the concrete providing record. If nothing provides `name`, it is
    // a missing/essential dependency — silently skip (documented simplification).
    let record = match index.get_provider(name) {
        Some(r) => r,
        None => return,
    };
    let real_name = record.package().to_string();

    // Skip anything already installed.
    if already_installed.contains(&real_name) {
        return;
    }
    // Already scheduled, or currently being expanded (cycle): stop recursing.
    if visited.contains(&real_name) || on_stack.contains(&real_name) {
        return;
    }

    on_stack.insert(real_name.clone());

    for group in record.depends() {
        // Collect the group's OR-alternatives as owned names so the borrow of the
        // transient `PkgRef` does not outlive the recursive `visit` calls below.
        let alts: Vec<String> = group.alts().map(|a| a.to_string()).collect();
        // Pick the alternative to follow for this AND-group.
        if let Some(chosen) = choose_alternative(index, &alts, already_installed) {
            // If the chosen alternative is already installed, the group is
            // satisfied and needs no further work.
            if !already_installed.contains(&chosen) {
                visit(
                    index,
                    &chosen,
                    already_installed,
                    visited,
                    on_stack,
                    order,
                );
            }
        }
        // If no alternative is installed or in the index, skip the whole group
        // (documented simplification: assumed essential/base package).
    }

    on_stack.remove(&real_name);
    if visited.insert(real_name.clone()) {
        order.push(real_name);
    }
}

/// Choose which alternative of an OR-group to follow:
///   1. the first alternative that is already installed (group is satisfied), or
///   2. the first alternative satisfiable by the index (real or via Provides).
/// Returns `None` if no alternative is installed or present in the index.
fn choose_alternative(
    index: &PackageIndex,
    alts: &[String],
    already_installed: &BTreeSet<String>,
) -> Option<String> {
    // Prefer an already-installed alternative.
    for alt in alts {
        if already_installed.contains(alt) {
            return Some(alt.clone());
        }
    }
    // Otherwise the first one present in the index.
    for alt in alts {
        if index.contains(alt) {
            return Some(alt.clone());
        }
    }
    None
}
