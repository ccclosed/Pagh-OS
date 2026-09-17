//! Pure, host-testable symbolic-link path resolution (issue #18).
//!
//! The kernel resolves a path component by component: `..` moves up the
//! directory stack, a final symlink is followed only when the syscall asks for
//! it, and intermediates are followed always. A crossed link splices its target
//! **in front of** the remaining components, and the walk restarts from the VFS
//! root for an absolute target but keeps the current directory for a relative
//! one. Crossing more than [`SYMLOOP_MAX`] links during one resolution is
//! `ELOOP` (Linux `SYMLOOP_MAX`), and the expanded path is capped so an
//! attacker-controlled image cannot grow the walk without bound.
//!
//! This module owns the whole algorithm and is `core` + `alloc` only: the
//! kernel's `vfs::lookup_path_walk` supplies a [`LinkTree`] adapter over
//! `Arc<dyn VfsNode>` handles, and `host-tests` drives the same source with an
//! in-memory tree and an independent oracle (so the semantics — not just the
//! compilation — are covered on the host, per R11.6).
//!
//! [`guest_path_keeps_root`] mirrors the guest-namespace policy that lives next
//! to the syscall layer (`arch::x86_64::linux::io`); the two copies are kept
//! honest by the host property `link_walk_agrees_with_io_guest_root`.
#![allow(dead_code)]

use alloc::string::String;
use alloc::vec::Vec;

/// Maximum number of symbolic links crossed while resolving one path.
///
/// Linux's `SYMLOOP_MAX`; the 41st crossing is `ELOOP`.
pub const SYMLOOP_MAX: u32 = 40;

/// Cap on the expanded path length of one resolution (`ENAMETOOLONG` beyond).
///
/// Every link target comes from disk and may be attacker-controlled, so the walk
/// must not be able to grow its pending list without bound: 40 targets of
/// `PATH_MAX` each would otherwise be materialized before the budget stops it.
pub const MAX_EXPANDED_BYTES: usize = 4096;

/// Guest-namespace path components that resolve against the VFS root instead of
/// being remapped under `/mnt` — the kernel's own trees (`/dev`, `/tmp`, the
/// synthetic `/proc`) plus the explicit `/mnt` view itself.
pub const ROOT_KEEPING_COMPONENTS: [&str; 5] = ["mnt", "dev", "proc", "sys", "tmp"];

/// Does an absolute guest path resolve against the VFS root (rather than being
/// remapped under `/mnt`)? Mirrors
/// `arch::x86_64::linux::io::guest_path_keeps_root`.
pub fn guest_path_keeps_root(abs: &str) -> bool {
    let first = abs.trim_start_matches('/').split('/').next().unwrap_or("");
    ROOT_KEEPING_COMPONENTS.contains(&first)
}

/// Why a path could not be resolved. The syscall layer maps these to `ENOENT`,
/// `ENOTDIR`, `ELOOP` and `ENAMETOOLONG`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum WalkError {
    /// Some component does not exist (including a dangling link that had to be
    /// followed).
    NotFound,
    /// An intermediate component is not a directory.
    NotDir,
    /// More than [`SYMLOOP_MAX`] links were crossed.
    TooManyLinks,
    /// The expanded path exceeded [`MAX_EXPANDED_BYTES`].
    TooLong,
}

/// A symlink target as the walker must interpret it.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct LinkTarget {
    /// The target in the walker's namespace. An absolute target is already
    /// mapped to the kernel namespace (see [`map_guest_target`]); a relative one
    /// is resolved against the directory holding the link.
    pub path: String,
    /// True for an absolute target: the walk restarts at the tree root.
    pub absolute: bool,
}

/// Map a link target (a *guest* path, stored verbatim on disk) into the walker's
/// namespace.
///
/// Absolute targets get the same mapping a syscall path gets — `/x` becomes
/// `/mnt/x` unless `x` is a kernel-owned tree — while relative targets are left
/// untouched (they resolve against the link's directory). `..` is deliberately
/// **not** collapsed here: the walker's directory stack must see it, so
/// `/a/link/..` with `link → /b/c` behaves like Linux and not like a lexical
/// `format!`-prefix.
pub fn map_guest_target(target: &str) -> LinkTarget {
    if !target.starts_with('/') {
        return LinkTarget {
            path: String::from(target),
            absolute: false,
        };
    }
    let path = if guest_path_keeps_root(target) {
        String::from(target)
    } else {
        let mut s = String::with_capacity(target.len() + 4);
        s.push_str("/mnt");
        s.push_str(target);
        s
    };
    LinkTarget {
        path,
        absolute: true,
    }
}

/// Split a path into components, dropping empty ones and `.` and keeping `..`
/// (the caller decides what `..` means for its directory stack).
pub fn components(path: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            other => out.push(other),
        }
    }
    out
}

/// The filesystem as the walker sees it: opaque node handles plus the three
/// questions resolution needs to ask.
pub trait LinkTree {
    /// The node a resolution starts from (the tree's root directory).
    fn root(&self) -> usize;
    /// Look `name` up in directory `id`; `None` when it is absent.
    fn lookup(&mut self, id: usize, name: &str) -> Option<usize>;
    /// Is `id` a directory?
    fn is_dir(&self, id: usize) -> bool;
    /// [`LinkTarget`] when `id` is a symbolic link, `None` otherwise. A link
    /// whose target cannot be read returns an empty `path` so the walk fails
    /// with [`WalkError::NotFound`] instead of treating it as a plain file.
    fn link_target(&self, id: usize) -> Option<LinkTarget>;
}

/// Resolve `path` against `tree`, returning the node id it names.
///
/// `path` is already in the walker's namespace (the kernel hands over the
/// guest-mapped absolute path). `follow_final` selects `stat`/`open` (true) or
/// `lstat`/`readlink`/`unlink` (false) semantics for the **last** component;
/// intermediate links are always followed.
pub fn resolve<T: LinkTree>(
    tree: &mut T,
    path: &str,
    follow_final: bool,
) -> Result<usize, WalkError> {
    let mut pending: Vec<String> = components(path).iter().map(|c| String::from(*c)).collect();
    pending.reverse(); // LIFO: the next component is at the end.
    let mut expanded = path.len();
    let mut links: u32 = 0;
    let mut stack: Vec<usize> = alloc::vec![tree.root()];

    loop {
        let Some(comp) = pending.pop() else {
            // Path fully consumed: the current directory is the result ("/"
            // resolves to the root).
            return Ok(*stack.last().expect("the stack always holds the root"));
        };
        if comp == ".." {
            // Never above the root: "/.." is "/".
            if stack.len() > 1 {
                stack.pop();
            }
            continue;
        }
        let dir = *stack.last().expect("the stack always holds the root");
        let child = tree.lookup(dir, &comp).ok_or(WalkError::NotFound)?;
        let is_last = pending.is_empty();

        if let Some(target) = tree.link_target(child) {
            if follow_final || !is_last {
                links += 1;
                if links > SYMLOOP_MAX {
                    return Err(WalkError::TooManyLinks);
                }
                if target.path.is_empty() {
                    // A link whose target could not be read: `ENOENT`, not a
                    // silent fallback to the link node.
                    return Err(WalkError::NotFound);
                }
                expanded += target.path.len();
                if expanded > MAX_EXPANDED_BYTES {
                    return Err(WalkError::TooLong);
                }
                if target.absolute {
                    // An absolute target restarts the walk at the root: the
                    // directory stack forgets where the link was. The *pending*
                    // components after the link are kept — `/a/link/..` must
                    // still apply its `..` to the target's directory.
                    stack.truncate(1);
                }
                // LIFO: the target's components are consumed before whatever
                // followed the link.
                for c in components(&target.path).iter().rev() {
                    pending.push(String::from(*c));
                }
                continue;
            }
        }

        if !is_last && !tree.is_dir(child) {
            return Err(WalkError::NotDir);
        }
        stack.push(child);
    }
}
