//! CWD state and pure path normalization/resolution (`.`/`..` folding,
//! clamping at root).
//!
//! `normalize` and `resolve` are pure functions over `&str`: they perform no
//! console or VFS I/O, so they are deterministic and trivially testable. The
//! shell-global current working directory lives behind a [`Spinlock`] and is
//! always stored as a normalized absolute path.

use alloc::string::String;
use alloc::vec::Vec;

use crate::sync::spinlock::Spinlock;

/// Shell-global current working directory.
///
/// Stored as a normalized absolute path. Initialized lazily: an empty string
/// (the const-constructible default) is treated as the root `/`, so the
/// observable initial CWD is `/` (R4.1) without needing runtime init.
static CWD: Spinlock<String> = Spinlock::new(String::new());

/// Fold an absolute path into canonical form.
///
/// Applies `.` (current) and `..` (parent) components, collapses runs of `/`,
/// and clamps `..` at the root (excess `..` cannot escape `/`). The result
/// always:
/// - begins with `/`,
/// - contains no `.`, `..`, or empty (`//`) components,
/// - has no trailing `/` except the root, which is exactly `"/"`.
///
/// An empty input is treated as `/`. The function is idempotent:
/// `normalize(normalize(p)) == normalize(p)`.
#[allow(dead_code)]
pub fn normalize(abs: &str) -> String {
    // Stack of surviving components (no leading '/', no '.'/'..', non-empty).
    let mut stack: Vec<&str> = Vec::new();

    for comp in abs.split('/') {
        match comp {
            // Empty components arise from leading '/', trailing '/', or runs of
            // '//'; "." is the current directory. Both are dropped.
            "" | "." => {}
            // Parent: pop one component, or stay at root when already empty
            // (clamp — `..` never escapes `/`).
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }

    if stack.is_empty() {
        return String::from("/");
    }

    let mut out = String::new();
    for comp in stack {
        out.push('/');
        out.push_str(comp);
    }
    out
}

/// Resolve a possibly-relative user path against `base`, then normalize.
///
/// If `input` begins with `/` it is absolute and `base` is ignored. Otherwise
/// the result is `base` joined with `input` (separated by `/`). `base` is
/// assumed to already be an absolute path (default `/`). The return value
/// satisfies the same canonical contract as [`normalize`].
#[allow(dead_code)]
pub fn resolve(base: &str, input: &str) -> String {
    if input.starts_with('/') {
        normalize(input)
    } else {
        // Join base + "/" + input; normalize folds any duplicate separators
        // (e.g. when base is "/" or already ends in '/').
        let mut joined = String::from(base);
        joined.push('/');
        joined.push_str(input);
        normalize(&joined)
    }
}

/// Snapshot of the shell-global current working directory.
///
/// Returns an owned clone so callers do not hold the lock while using it. The
/// value is always a normalized absolute path; an uninitialized store reads
/// back as `/`.
#[allow(dead_code)]
pub fn cwd() -> String {
    let guard = CWD.lock();
    if guard.is_empty() {
        String::from("/")
    } else {
        guard.clone()
    }
}

/// Store a new current working directory, normalizing it first so the global
/// invariant (a canonical absolute path) is preserved.
#[allow(dead_code)]
pub fn set_cwd(abs: &str) {
    let normalized = normalize(abs);
    let mut guard = CWD.lock();
    *guard = normalized;
}
