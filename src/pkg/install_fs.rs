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
//!     directories (R10.2).
//!   * Create/overwrite the target file and write exactly the entry content so the
//!     stored size equals the content length (R10.3, R10.7).
//!   * On a no-space failure from ext2, remove any partial file and return
//!     [`InstallError::NoSpace`] (R10.4).
//!   * Emit exactly one structured diagnostic per failure naming
//!     `component=Package_Installer`, the stage, and the file path (R12.4, R12.5).
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
///   * missing parent directories are created (R10.2) and resolved **through
///     symbolic links**, so a member under an existing `/lib64 -> usr/lib64`
///     lands inside the target instead of creating a parallel tree;
///   * an existing entry at a member's path is removed first — through the
///     link-aware `unlink`, so removing one name of a hard-linked file cannot take
///     the data another name still uses. A directory in the way is skipped with one
///     diagnostic (never recursively deleted).
///
/// On a no-space failure the partial file is removed and [`InstallError::NoSpace`]
/// is returned (R10.4); every failure emits one structured diagnostic (R12.4,
/// R12.5). Nothing is ever materialized as a copy.
pub fn install_data_tar(entries: &[TarEntry<'_>], root: &str) -> Result<usize, InstallError> {
    let plan = plan_install(entries);

    if plan.skipped_unsafe > 0 || plan.skipped_empty_target > 0 {
        crate::warn!(
            "Package_Installer: skipped {} path-unsafe and {} empty-target members",
            plan.skipped_unsafe,
            plan.skipped_empty_target
        );
    }

    let mut installed = 0usize;

    // Hard links needing a target that this archive does not create (or that was
    // created later in the plan): retried after the first pass.
    let mut deferred: Vec<&InstallOp<'_>> = Vec::new();

    for op in plan.ops.iter() {
        match op {
            InstallOp::File { path, content, .. } => {
                install_one(root, path, content)?;
                installed += 1;
            }
            InstallOp::Symlink { path, target } => {
                install_symlink(root, path, target)?;
                installed += 1;
            }
            InstallOp::Hardlink { path, target } => {
                match install_hardlink(root, path, target) {
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
            match install_hardlink(root, path, target) {
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
fn install_symlink(root: &str, rel: &str, target: &str) -> Result<(), InstallError> {
    let abs = join_abs(root, rel);
    let (dir, filename) = resolve_parent(root, rel, &abs)?;

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
fn install_hardlink(root: &str, rel: &str, target: &str) -> Result<(), HardlinkOutcome> {
    let abs = join_abs(root, rel);
    let target_abs = join_abs(root, target);

    // The target is resolved **without** following a final symlink: `link(2)`
    // hard-links the entry itself, and a hard link to a symlink is legal.
    let target_node = match vfs::lookup_path_walk(&target_abs, false) {
        Ok(n) => n,
        Err(_) => return Err(HardlinkOutcome::Missing),
    };
    if target_node.is_directory() {
        crate::warn!(
            "Package_Installer: hard link '{}' skipped (target '{}' is a directory)",
            abs,
            target_abs
        );
        return Ok(());
    }

    let (dir, filename) = match resolve_parent(root, rel, &abs) {
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

/// Resolve the parent directory of a normalized root-relative path, creating any
/// missing component.
///
/// Parents are resolved **through symlinks** (`lookup_path_walk(_, true)`): when a
/// package ships a file under a path that is already a link to a directory
/// (`/lib64 -> usr/lib64`), the file must land in the target, not in a fresh
/// parallel directory.
fn resolve_parent<'a>(
    root: &str,
    rel: &'a str,
    abs: &str,
) -> Result<(Arc<dyn VfsNode>, &'a str), InstallError> {
    let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    if comps.is_empty() {
        return Err(vfs_err("resolve", abs, VfsError::InvalidArgument));
    }
    let (dirs, last) = comps.split_at(comps.len() - 1);
    let filename = last[0];

    let mut dir =
        vfs::lookup_path_walk(root, true).map_err(|e| vfs_err("resolve_root", abs, walk_err(e)))?;
    let mut walked = String::from(root.trim_end_matches('/'));
    for comp in dirs {
        walked.push('/');
        walked.push_str(comp);
        dir = match vfs::lookup_path_walk(&walked, true) {
            Ok(child) => child,
            Err(_) => match dir.create_dir(comp) {
                Ok(child) => child,
                Err(VfsError::AlreadyExists) => vfs::lookup_path_walk(&walked, true)
                    .map_err(|e| vfs_err("mkdir", abs, walk_err(e)))?,
                Err(VfsError::IoError) => return Err(no_space("mkdir", abs)),
                Err(e) => return Err(vfs_err("mkdir", abs, e)),
            },
        };
    }
    Ok((dir, filename))
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
fn install_one(root: &str, rel: &str, content: &[u8]) -> Result<(), InstallError> {
    let abs = join_abs(root, rel);

    // R10.2: walk (and create) the parent chain, following symbolic links — a
    // member under `/lib64 -> usr/lib64` belongs inside the target.
    let (dir, filename) = resolve_parent(root, rel, &abs)?;

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
