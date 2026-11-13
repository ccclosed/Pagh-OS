//! Effectful ext2 package installer (design component 10, `Package_Installer`).
//!
//! This is the *kernel-only* half of the installer. The pure selection/normalization
//! logic lives in [`super::install`] (`install.rs`), which is `#[path]`-included by
//! the `host-tests` crate and therefore MUST stay free of kernel/VFS dependencies
//! (R11.6). [`install_data_tar`] is the effectful shell: it reuses
//! [`normalize_entry_path`](super::install::normalize_entry_path) and writes the
//! selected regular files onto the real ext2 filesystem through the [`VfsNode`]
//! trait. It is split into this sibling module — exactly like `net::http` (pure) vs
//! `net::http_fetch` (effectful) — so `install.rs` remains host-includable.
//!
//! Responsibilities (R10.1–R10.4, R10.6, R10.7, R10.8, R12.4, R12.5):
//!   * Skip non-regular entries (R10.6) and `..`-escaping / empty paths (R10.8).
//!   * Resolve each safe path relative to `root`, creating any missing parent
//!     directories (R10.2) — but **never outside `root`**, and never through a
//!     symlink the same package creates (F-x86n-1, see below).
//!   * Create/overwrite the target file and write exactly the entry content so the
//!     stored size equals the content length (R10.3, R10.7).
//!   * On a no-space failure from ext2, remove any partial file and return
//!     [`InstallError::NoSpace`] (R10.4).
//!   * Emit exactly one structured diagnostic per failure naming
//!     `component=Package_Installer`, the stage, and the file path (R12.4, R12.5).
//!
//! ## Install-root confinement (F-x86n-1)
//!
//! The archive is untrusted even when it arrived over an authenticated channel: a
//! `.deb` member may be a symlink, and older code resolved later members' parent
//! directories through **any** symlink in the tree (`lookup_path_walk(_, true)`).
//! Because the walker interprets link targets in the *guest* namespace, an
//! absolute target like `/etc` maps to `/mnt/etc` — so `./y -> /etc` followed by
//! `./y/autorun` wrote `/mnt/etc/autorun`, which the shell executes as root at
//! boot (also `./x -> /` + `./x/pwned_root` → `/mnt/pwned_root`). Neither path
//! contains `..`, so the archive-path normalization could not catch it.
//!
//! The fix is two independent layers:
//!
//!   * the pure [`plan_install`] collects every symlink the archive creates and
//!     refuses any member (or hard-link target) whose ancestor component is one of
//!     them; [`install_data_tar`] turns that into a whole-package refusal
//!     ([`InstallError::UnsafePath`]) **before writing anything**;
//!   * the effectful resolver below walks parents component by component starting
//!     at the install-root node and never leaves it: a component must be a real
//!     directory or a missing name (created when asked), a symbolic link is only
//!     followed when its target is **relative** and the spliced components stay
//!     under the root, and a link this same install created is never followed at
//!     all (lstat-style resolutution). Absolute targets are refused outright —
//!     remapping them into the guest namespace is exactly what made them
//!     install-root-relative.
//!
//! Hard-link targets are resolved by the same confined walker (final component
//! unfollowed, as `link(2)` requires), so a hostile archive cannot hard-link a
//! node from outside the root either.
//!
//! ## No-space detection through the VFS boundary
//!
//! ext2 reports an exhausted block/inode bitmap as `FsError::OutOfSpace`, which the
//! ext2 `VfsNode` adapter (`fs/ext2/mod.rs::fs_to_vfs`) collapses to
//! [`VfsError::IoError`]. The VFS trait surface exposes no dedicated out-of-space
//! variant, so a write/create that fails with [`VfsError::IoError`] is the only
//! observable no-space signal and is mapped to [`InstallError::NoSpace`] here (R10.4).
//! Every other [`VfsError`] is reported verbatim via [`InstallError::Vfs`].
#![allow(dead_code)]

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::vfs::{self, VfsError, VfsNode};

use super::install::{plan_install, InstallOp};
use super::tar::TarEntry;

/// Failure modes of the effectful ext2 install (design component 10).
#[derive(Debug)]
pub enum InstallError {
    /// The ext2 filesystem ran out of space while creating/writing `path`. Any
    /// partial file at that path has already been removed (R10.4).
    NoSpace { path: String },
    /// Any other VFS/ext2 error surfaced while installing (the kernel's real
    /// [`VfsError`]).
    Vfs(VfsError),
    /// The package tries to materialize `path` through a symbolic link instead of
    /// where its member path says (F-x86n-1). The whole package is refused before
    /// anything is written; `reason` names the refused construct.
    UnsafePath { path: String, reason: &'static str },
}

impl core::fmt::Display for InstallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InstallError::NoSpace { path } => {
                write!(f, "no space while writing '{}'", path)
            }
            InstallError::Vfs(e) => write!(f, "filesystem error {:?}", e),
            InstallError::UnsafePath { path, reason } => {
                write!(f, "refused unsafe install path '{}': {}", path, reason)
            }
        }
    }
}

/// Install every member of a decompressed `data.tar` onto ext2 under `root`,
/// returning the number of entries created (R10.1–R10.4, R10.6–R10.8, issue #18).
///
/// The selection and ordering rules live in the pure [`plan_install`]; this is the
/// effectful shell that executes the plan:
///
///   * regular files are written verbatim (replacing an existing entry so the
///     stored size matches the content length — R10.3, R10.7);
///   * **symlinks are created as symlinks**, with the archive's target stored
///     verbatim (`VfsNode::create_symlink`); a dangling target is normal;
///   * **hard links are created as hard links** (`VfsNode::link`), i.e. a second
///     name for the target inode — never a copy. The target may come from this
///     archive or from an earlier package, so the hard-link operations are retried
///     until no further progress, and whatever remains is reported and skipped;
///   * missing parent directories are created (R10.2) and resolved strictly inside
///     the install root: an existing `/lib64 -> usr/lib64` (relative, in-root) is
///     still followed so the member lands inside the target instead of creating a
///     parallel tree, but a link this package creates is never traversed and an
///     absolute target is refused (F-x86n-1, see the module docs);
///   * an existing entry at a member's path is removed first — through the
///     link-aware `unlink`, so removing one name of a hard-linked file cannot take
///     the data another name still uses. A directory in the way is skipped with one
///     diagnostic (never recursively deleted).
///
/// A package whose member paths descend through symlinks the archive itself
/// creates is refused **atomically** with [`InstallError::UnsafePath`] before any
/// write, so a hostile `.deb` cannot leave a partial tree behind (F-x86n-1).
///
/// On a no-space failure the partial file is removed and [`InstallError::NoSpace`]
/// is returned (R10.4); every failure emits one structured diagnostic (R12.4,
/// R12.5). Nothing is ever materialized as a copy.
pub fn install_data_tar(entries: &[TarEntry<'_>], root: &str) -> Result<usize, InstallError> {
    let plan = plan_install(entries);

    // F-x86n-1, layer 1 (pure plan): a member below a symlink this archive creates
    // would be written wherever that link points. Refuse the whole package before
    // creating a single inode — a partially installed hostile package is a
    // confusing state, and the archive has already proven it cannot be trusted.
    if let Some(m) = plan.unsafe_members.first() {
        let abs = join_abs(root, &m.path);
        let reason = if m.is_hardlink_target {
            "hard-link target descends through an archive symlink"
        } else {
            "member path descends through an archive symlink"
        };
        return Err(unsafe_path_with_link(&abs, reason, &m.link));
    }

    if plan.skipped_unsafe > 0 || plan.skipped_empty_target > 0 {
        crate::warn!(
            "Package_Installer: skipped {} path-unsafe and {} empty-target members",
            plan.skipped_unsafe,
            plan.skipped_empty_target
        );
    }

    let mut installed = 0usize;
    // Symlink paths this same install creates. Layer 2 (the effectful resolver)
    // uses this to refuse any traversal through them even if the plan-level check
    // were ever bypassed or regressed.
    let mut own_links: BTreeSet<String> = BTreeSet::new();

    // Hard links needing a target that this archive does not create (or that was
    // created later in the plan): retried after the first pass.
    let mut deferred: Vec<&InstallOp<'_>> = Vec::new();

    for op in plan.ops.iter() {
        match op {
            InstallOp::File {
                path,
                content,
                mode,
            } => {
                install_one(root, path, content, *mode, &own_links)?;
                installed += 1;
            }
            InstallOp::Symlink { path, target } => {
                install_symlink(root, path, target, &own_links)?;
                own_links.insert(path.clone());
                installed += 1;
            }
            InstallOp::Hardlink { path, target } => {
                match install_hardlink(root, path, target, &own_links) {
                    Ok(()) => installed += 1,
                    // The target is not on the tree yet: a later member (or an
                    // earlier package) may still provide it.
                    Err(HardlinkOutcome::Missing) => deferred.push(op),
                    Err(HardlinkOutcome::Failed(e)) => return Err(e),
                }
            }
        }
    }

    // Bounded fixpoint over the deferred hard links: each pass must create at
    // least one, otherwise the rest can never resolve.
    loop {
        if deferred.is_empty() {
            break;
        }
        let mut progressed = false;
        let mut remaining: Vec<&InstallOp<'_>> = Vec::new();
        for op in deferred {
            let InstallOp::Hardlink { path, target } = op else {
                continue;
            };
            match install_hardlink(root, path, target, &own_links) {
                Ok(()) => {
                    installed += 1;
                    progressed = true;
                }
                Err(HardlinkOutcome::Missing) => remaining.push(op),
                Err(HardlinkOutcome::Failed(e)) => return Err(e),
            }
        }
        deferred = remaining;
        if !progressed {
            break;
        }
    }

    // One diagnostic per hard link that never found its target. It is skipped —
    // turning it into a copy is exactly what issue #18 removed.
    for op in deferred {
        if let InstallOp::Hardlink { path, target } = op {
            crate::warn!(
                "Package_Installer: hard link '{}' -> '{}' skipped (target missing after all passes)",
                path,
                target
            );
        }
    }

    Ok(installed)
}

/// Outcome of one hard-link attempt.
enum HardlinkOutcome {
    /// The target does not exist (yet) — the caller may retry.
    Missing,
    /// A real failure (no space, I/O, a directory in the way, ...).
    Failed(InstallError),
}

/// Create `rel` as a symbolic link to `target` (stored verbatim).
fn install_symlink(
    root: &str,
    rel: &str,
    target: &str,
    own_links: &BTreeSet<String>,
) -> Result<(), InstallError> {
    let abs = join_abs(root, rel);
    let (dir, filename) = resolve_parent(root, rel, &abs, own_links)?;

    // Replace any existing entry (link-aware removal), but never a directory.
    match dir.lookup(filename) {
        Ok(existing) => {
            if existing.is_directory() {
                crate::warn!(
                    "Package_Installer: link '{}' skipped (a directory is in the way)",
                    abs
                );
                return Ok(());
            }
            dir.remove(filename)
                .map_err(|e| vfs_err("replace", &abs, e))?;
        }
        Err(VfsError::NotFound) => {}
        Err(e) => return Err(vfs_err("stat", &abs, e)),
    }

    match dir.create_symlink(filename, target.as_bytes()) {
        Ok(_) => {
            crate::debug!("Package_Installer: link {} -> {}", abs, target);
            Ok(())
        }
        Err(VfsError::IoError) => Err(no_space("symlink", &abs)),
        Err(e) => Err(vfs_err("symlink", &abs, e)),
    }
}

/// Create `rel` as a hard link to the existing entry at `target` (a
/// root-relative path from the archive).
fn install_hardlink(
    root: &str,
    rel: &str,
    target: &str,
    own_links: &BTreeSet<String>,
) -> Result<(), HardlinkOutcome> {
    let abs = join_abs(root, rel);
    let target_abs = join_abs(root, target);

    // The target is resolved **without** following a final symlink: `link(2)`
    // hard-links the entry itself, and a hard link to a symlink is legal. It goes
    // through the same confined walker as the parents (F-x86n-1), so a hostile
    // archive cannot hard-link a node from outside the install root.
    let target_node = match resolve_confined(root, target, &target_abs, false, false, own_links) {
        Ok(n) => n,
        // Not on the tree yet: a later member (or an earlier package) may still
        // provide it.
        Err(ResolveFailure::NotFound) => return Err(HardlinkOutcome::Missing),
        Err(failure) => {
            return Err(HardlinkOutcome::Failed(resolve_failure(
                "resolve_target",
                &target_abs,
                failure,
            )))
        }
    };
    if target_node.is_directory() {
        crate::warn!(
            "Package_Installer: hard link '{}' skipped (target '{}' is a directory)",
            abs,
            target_abs
        );
        return Ok(());
    }

    let (dir, filename) = match resolve_parent(root, rel, &abs, own_links) {
        Ok(v) => v,
        Err(e) => return Err(HardlinkOutcome::Failed(e)),
    };
    match dir.lookup(filename) {
        Ok(existing) => {
            if existing.is_directory() {
                crate::warn!(
                    "Package_Installer: hard link '{}' skipped (a directory is in the way)",
                    abs
                );
                return Ok(());
            }
            if let Err(e) = dir.remove(filename) {
                return Err(HardlinkOutcome::Failed(vfs_err("replace", &abs, e)));
            }
        }
        Err(VfsError::NotFound) => {}
        Err(e) => return Err(HardlinkOutcome::Failed(vfs_err("stat", &abs, e))),
    }

    match dir.link(filename, &target_node) {
        Ok(_) => Ok(()),
        Err(VfsError::IoError) => Err(HardlinkOutcome::Failed(no_space("link", &abs))),
        Err(e) => Err(HardlinkOutcome::Failed(vfs_err("link", &abs, e))),
    }
}

/// Why one step of a confined resolution failed.
///
/// Kept separate from [`InstallError`] so a hard-link attempt that simply has no
/// target yet (`NotFound`) can be retried without emitting a failure diagnostic;
/// the call site adds the stage and path when it really is an error.
enum ResolveFailure {
    /// A component does not exist and the caller did not ask to create it.
    NotFound,
    /// Following the path would leave the install root (F-x86n-1).
    Unsafe(&'static str),
    /// A real VFS/ext2 error.
    Vfs(VfsError),
    /// No space while creating a missing parent directory.
    NoSpace,
}

/// Linux `SYMLOOP_MAX`: the 41st symlink crossed in one resolution is refused.
const INSTALL_SYMLOOP_MAX: u32 = 40;
/// `PATH_MAX`-sized cap on the expanded path of one resolution.
const INSTALL_MAX_EXPANDED_BYTES: usize = 4096;

/// Resolve `rel` strictly inside the install root, following only relative
/// symbolic links that stay under it (F-x86n-1).
///
/// The walk starts at the install-root node and never consults the global VFS root
/// again: the root is the boundary, `..` may not pop it, and every traversed
/// component is a real directory. A symbolic link is followed only when its stored
/// target is **relative**; the spliced components are then resolved under the root
/// like any other component (so `..` inside a target cannot escape either).
/// Absolute targets are refused outright, because the guest namespace maps them to
/// install-root-relative paths — that mapping is exactly what the attack relied on.
/// A link this same install created (`own_links`) is never followed at all.
///
/// `follow_final` selects `true` for a parent directory (a member is placed
/// inside it) and `false` for a hard-link target (`link(2)` does not follow the
/// final component). `create_missing` creates a missing component as a directory
/// (R10.2) instead of returning [`ResolveFailure::NotFound`].
fn resolve_confined(
    root: &str,
    rel: &str,
    abs: &str,
    follow_final: bool,
    create_missing: bool,
    own_links: &BTreeSet<String>,
) -> Result<Arc<dyn VfsNode>, ResolveFailure> {
    let trimmed = root.trim_end_matches('/');
    let root_path = if trimmed.is_empty() { "/" } else { trimmed };
    let root_node =
        vfs::lookup_path_walk(root_path, true).map_err(|e| ResolveFailure::Vfs(walk_err(e)))?;
    if !root_node.is_directory() {
        return Err(ResolveFailure::Vfs(VfsError::InvalidArgument));
    }

    // Components still to resolve (LIFO) and the real directories walked so far.
    let mut pending: Vec<String> = rel
        .split('/')
        .filter(|c| !c.is_empty())
        .map(String::from)
        .collect();
    pending.reverse();
    let mut stack: Vec<Arc<dyn VfsNode>> = alloc::vec![root_node];
    let mut dirs: Vec<String> = Vec::new();
    let mut links: u32 = 0;
    let mut expanded = abs.len();

    while let Some(comp) = pending.pop() {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            // `..` applies to the directories actually walked; the install root has
            // no parent to escape to.
            if dirs.pop().is_none() {
                return Err(ResolveFailure::Unsafe("path escapes the install root"));
            }
            stack.pop();
            continue;
        }
        let parent = match stack.last() {
            Some(parent) => parent,
            None => return Err(ResolveFailure::Vfs(VfsError::NotFound)),
        };
        let node = match parent.lookup(&comp) {
            Ok(node) => node,
            Err(VfsError::NotFound) if create_missing => match parent.create_dir(&comp) {
                Ok(node) => node,
                // Never seen in a single-threaded install, but ext2 may report an
                // existing entry: fall back to a plain lookup instead of guessing.
                Err(VfsError::AlreadyExists) => match parent.lookup(&comp) {
                    Ok(node) => node,
                    Err(e) => return Err(ResolveFailure::Vfs(e)),
                },
                Err(VfsError::IoError) => return Err(ResolveFailure::NoSpace),
                Err(e) => return Err(ResolveFailure::Vfs(e)),
            },
            Err(VfsError::NotFound) => return Err(ResolveFailure::NotFound),
            Err(e) => return Err(ResolveFailure::Vfs(e)),
        };

        if node.is_symlink() {
            if pending.is_empty() && !follow_final {
                // lstat-style: a hard link to a symlink links the link itself.
                return Ok(node);
            }
            // The root-relative path of the link itself, to compare against the
            // links this install created.
            let mut link_rel = String::new();
            for d in dirs.iter() {
                link_rel.push_str(d);
                link_rel.push('/');
            }
            link_rel.push_str(&comp);
            if own_links.contains(&link_rel) {
                return Err(ResolveFailure::Unsafe(
                    "descends through a symlink created by this archive",
                ));
            }
            let target = match node.read_link() {
                Some(t) if !t.is_empty() => t,
                _ => return Err(ResolveFailure::Unsafe("unreadable or empty symlink target")),
            };
            if target.starts_with('/') {
                return Err(ResolveFailure::Unsafe(
                    "absolute symlink target (only relative in-root links are followed)",
                ));
            }
            links = links.saturating_add(1);
            if links > INSTALL_SYMLOOP_MAX {
                return Err(ResolveFailure::Vfs(VfsError::InvalidArgument));
            }
            expanded = expanded.saturating_add(target.len());
            if expanded > INSTALL_MAX_EXPANDED_BYTES {
                return Err(ResolveFailure::Vfs(VfsError::InvalidArgument));
            }
            // Splice the relative target in front of the remaining components; it
            // resolves against the link's directory, i.e. the current stack.
            for part in target.split('/').rev() {
                pending.push(String::from(part));
            }
            continue;
        }
        if !node.is_directory() {
            if pending.is_empty() && !follow_final {
                return Ok(node);
            }
            return Err(ResolveFailure::Vfs(VfsError::InvalidArgument));
        }
        dirs.push(comp);
        stack.push(node);
    }

    stack.pop().ok_or(ResolveFailure::Vfs(VfsError::NotFound))
}

/// Resolve the parent directory of a normalized root-relative path, creating any
/// missing component — strictly inside the install root (F-x86n-1).
///
/// A pre-existing relative link that stays inside the root is still followed, so a
/// member under `/lib64 -> usr/lib64` lands inside the target instead of creating a
/// parallel tree; a link the same package creates is not followed, and an absolute
/// target is refused (see [`resolve_confined`]).
fn resolve_parent<'a>(
    root: &str,
    rel: &'a str,
    abs: &str,
    own_links: &BTreeSet<String>,
) -> Result<(Arc<dyn VfsNode>, &'a str), InstallError> {
    let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    if comps.is_empty() {
        return Err(vfs_err("resolve", abs, VfsError::InvalidArgument));
    }
    let (dirs, last) = comps.split_at(comps.len() - 1);
    let filename = last[0];

    // Join the parent components into the root-relative path the walker resolves.
    // A top-level member has none: the walker returns the install-root node.
    let mut dir_rel = String::new();
    for (i, comp) in dirs.iter().enumerate() {
        if i > 0 {
            dir_rel.push('/');
        }
        dir_rel.push_str(comp);
    }
    let dir = resolve_confined(root, &dir_rel, abs, true, true, own_links)
        .map_err(|failure| resolve_failure("resolve", abs, failure))?;
    if !dir.is_directory() {
        return Err(vfs_err("resolve", abs, VfsError::InvalidArgument));
    }
    Ok((dir, filename))
}

/// Map a confined-resolution failure onto the public [`InstallError`] (with one
/// structured diagnostic naming the stage and path — R12.4, R12.5).
fn resolve_failure(stage: &str, path: &str, failure: ResolveFailure) -> InstallError {
    match failure {
        ResolveFailure::NotFound => vfs_err(stage, path, VfsError::NotFound),
        ResolveFailure::Unsafe(reason) => unsafe_path(path, reason),
        ResolveFailure::Vfs(e) => vfs_err(stage, path, e),
        ResolveFailure::NoSpace => no_space("mkdir", path),
    }
}

/// Map a walk failure onto a VFS error for diagnostics.
fn walk_err(e: crate::vfs::link_walk::WalkError) -> VfsError {
    use crate::vfs::link_walk::WalkError;
    match e {
        WalkError::NotFound => VfsError::NotFound,
        WalkError::NotDir | WalkError::TooManyLinks | WalkError::TooLong => {
            VfsError::InvalidArgument
        }
    }
}

/// Install a single normalized, root-relative regular file.
fn install_one(
    root: &str,
    rel: &str,
    content: &[u8],
    mode: u32,
    own_links: &BTreeSet<String>,
) -> Result<(), InstallError> {
    let abs = join_abs(root, rel);

    // R10.2: walk (and create) the parent chain inside the install root. An
    // existing in-root relative link such as `/lib64 -> usr/lib64` is still
    // followed; a link this package creates is not (F-x86n-1).
    let (dir, filename) = resolve_parent(root, rel, &abs, own_links)?;

    // R10.7: replace an existing regular file so the stored size matches the new
    // content length. ext2 `write_file` only ever GROWS `i_size`, so overwriting in
    // place would leave a stale tail when the new content is shorter; removing and
    // recreating guarantees `size == content.len()` (R10.3).
    match dir.lookup(filename) {
        Ok(existing) => {
            if existing.is_directory() {
                crate::warn!(
                    "Package_Installer: file '{}' skipped (a directory is in the way)",
                    abs
                );
                return Ok(());
            }
            if let Err(e) = dir.remove(filename) {
                return Err(vfs_err("replace", &abs, e));
            }
        }
        Err(VfsError::NotFound) => {}
        Err(e) => return Err(vfs_err("create", &abs, e)),
    }

    // Create the fresh (empty, size 0) file.
    let file_node: Arc<dyn VfsNode> = match dir.create_file(filename) {
        Ok(n) => n,
        Err(VfsError::IoError) => return Err(no_space("create", &abs)),
        Err(e) => return Err(vfs_err("create", &abs, e)),
    };

    // R10.3: write exactly the entry content. An empty file is already size 0, so
    // there is nothing to write (ext2 `write_file` treats an empty buffer as a no-op).
    if !content.is_empty() {
        match file_node.write(0, content) {
            Ok(written) if written == content.len() => {}
            // A short write without an explicit error still means the full content
            // did not land (most plausibly an exhausted bitmap) → treat as no-space
            // and remove the partial file (R10.4).
            Ok(_) => {
                let _ = dir.remove(filename);
                return Err(no_space("write", &abs));
            }
            // ext2 OutOfSpace → IoError (see module docs): R10.4 cleanup + NoSpace.
            Err(VfsError::IoError) => {
                let _ = dir.remove(filename);
                return Err(no_space("write", &abs));
            }
            Err(e) => {
                let _ = dir.remove(filename);
                return Err(vfs_err("write", &abs, e));
            }
        }
    }

    // Preserve the archive's permission bits on the ext2 inode. `create_file`
    // creates 0644, which would make every installed program non-executable —
    // the security model's execute check requires at least one x bit, even for
    // root. A filesystem without mode support (not ext2) keeps the default.
    let perms = mode & 0o7777;
    if perms != 0 {
        if let Err(e) = file_node.set_mode(perms) {
            crate::warn!(
                "Package_Installer: could not apply mode {:04o} to '{}': {:?}",
                perms,
                abs,
                e
            );
        }
    }

    Ok(())
}

/// Join the installation root and a normalized relative path into an absolute path
/// for diagnostics and the [`InstallError::NoSpace`] payload.
fn join_abs(root: &str, rel: &str) -> String {
    let mut s = String::from(root.trim_end_matches('/'));
    s.push('/');
    s.push_str(rel);
    s
}

/// Build a [`InstallError::NoSpace`] and emit the single structured diagnostic for it
/// (R10.4, R12.4, R12.5).
fn no_space(stage: &str, path: &str) -> InstallError {
    crate::error!(
        "Package_Installer: stage={} path={} cause=NoSpace",
        stage,
        path
    );
    InstallError::NoSpace {
        path: String::from(path),
    }
}

/// Build a [`InstallError::Vfs`] and emit the single structured diagnostic for it
/// (R12.4, R12.5).
fn vfs_err(stage: &str, path: &str, e: VfsError) -> InstallError {
    crate::error!(
        "Package_Installer: stage={} path={} cause=Vfs({:?})",
        stage,
        path,
        e
    );
    InstallError::Vfs(e)
}

/// Build an [`InstallError::UnsafePath`] and emit the single structured diagnostic
/// for it (F-x86n-1, R12.4, R12.5).
fn unsafe_path(path: &str, reason: &'static str) -> InstallError {
    crate::error!(
        "Package_Installer: stage=confine path={} cause=UnsafePath reason={}",
        path,
        reason
    );
    InstallError::UnsafePath {
        path: String::from(path),
        reason,
    }
}

/// [`unsafe_path`] for a refusal decided by the pure planner, naming the archive
/// symlink the member descends through.
fn unsafe_path_with_link(path: &str, reason: &'static str, link: &str) -> InstallError {
    crate::error!(
        "Package_Installer: stage=plan path={} cause=UnsafePath link={} reason={}",
        path,
        link,
        reason
    );
    InstallError::UnsafePath {
        path: String::from(path),
        reason,
    }
}
