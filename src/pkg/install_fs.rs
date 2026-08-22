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

use super::install::{normalize_entry_path, NormPath};
use super::tar::{TarEntry, TarType};

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

/// Install every regular-file entry of a decompressed `data.tar` onto ext2 under
/// `root`, returning the number of files written (R10.1–R10.4, R10.6–R10.8).
///
/// For each entry: non-regular entries are skipped (R10.6); the archived path is
/// normalized and `..`-escaping/empty paths are skipped (R10.8); missing parent
/// directories are created (R10.2); the file is created (replacing any existing
/// regular file so the stored size equals the content length — R10.3, R10.7) and the
/// entry content is written verbatim. On a no-space failure the partial file is
/// removed and [`InstallError::NoSpace`] is returned (R10.4); every failure emits one
/// structured diagnostic (R12.4, R12.5).
pub fn install_data_tar(entries: &[TarEntry<'_>], root: &str) -> Result<usize, InstallError> {
    let mut installed = 0usize;

    for entry in entries {
        // R10.6: only regular files are installed; skip directories and others.
        if entry.kind != TarType::Regular {
            continue;
        }

        // R10.1/R10.8: normalize against the root; skip unsafe/empty paths.
        let rel = match normalize_entry_path(entry.path) {
            NormPath::Keep(p) => p,
            NormPath::SkipUnsafe => continue,
        };

        install_one(root, &rel, entry.content)?;
        installed += 1;
    }

    // Ext2 write support has no symlinks, and skipping
    // them lost critical files (libc6 ships /lib64/ld-linux-x86-64.so.2 and
    // most lib*.so.N names only as symlinks; python3-minimal ships
    // /usr/bin/python3 as a symlink). Materialize each link as a plain copy of
    // its target file, after all regular files of the package are on disk.
    installed += materialize_symlinks(entries, root)?;

    Ok(installed)
}

/// Materialize symlink/hardlink entries as regular-file copies of their
/// targets. Targets may live in this package (just installed) or in an earlier
/// one (already on disk). Multi-level link chains are handled by re-trying
/// unresolved links after each pass; links whose target never appears (or is a
/// directory) are skipped with one warning each.
fn materialize_symlinks(entries: &[TarEntry<'_>], root: &str) -> Result<usize, InstallError> {
    // (link rel path, candidate target rel paths)
    let mut pending: Vec<(String, Vec<String>)> = Vec::new();

    for entry in entries {
        if entry.kind != TarType::Symlink || entry.link_target.is_empty() {
            continue;
        }
        let rel = match normalize_entry_path(entry.path) {
            NormPath::Keep(p) => p,
            NormPath::SkipUnsafe => continue,
        };
        let mut candidates: Vec<String> = Vec::new();
        if let Some(stripped) = entry.link_target.strip_prefix('/') {
            // Absolute symlink target: interpret relative to the install root.
            if let NormPath::Keep(t) = normalize_entry_path(stripped) {
                candidates.push(t);
            }
        } else {
            // Relative symlink target: resolve against the link's parent dir.
            let mut comps: Vec<&str> = rel.split('/').collect();
            comps.pop();
            let mut ok = true;
            for c in entry.link_target.split('/') {
                match c {
                    "" | "." => {}
                    ".." => {
                        if comps.pop().is_none() {
                            ok = false;
                            break;
                        }
                    }
                    c => comps.push(c),
                }
            }
            if ok && !comps.is_empty() {
                candidates.push(comps.join("/"));
            }
            // Hardlink targets are archive paths (root-relative, no leading
            // slash): also try the raw target against the root.
            if let NormPath::Keep(t) = normalize_entry_path(entry.link_target) {
                if !candidates.contains(&t) {
                    candidates.push(t);
                }
            }
        }
        if candidates.is_empty() {
            crate::warn!(
                "Package_Installer: link '{}' -> '{}' skipped (unresolvable target)",
                entry.path,
                entry.link_target
            );
            continue;
        }
        pending.push((rel, candidates));
    }

    let mut count = 0usize;
    for _pass in 0..4 {
        if pending.is_empty() {
            break;
        }
        let mut progressed = false;
        let mut remaining: Vec<(String, Vec<String>)> = Vec::new();
        for (rel, candidates) in pending {
            let mut content: Option<Vec<u8>> = None;
            for cand in &candidates {
                if let Some(data) = read_regular_file(root, cand) {
                    content = Some(data);
                    break;
                }
            }
            match content {
                Some(data) => {
                    install_one(root, &rel, &data)?;
                    count += 1;
                    progressed = true;
                }
                None => remaining.push((rel, candidates)),
            }
        }
        pending = remaining;
        if !progressed {
            break;
        }
    }
    for (rel, candidates) in &pending {
        crate::warn!(
            "Package_Installer: link '{}' -> '{}' skipped (target missing or not a regular file)",
            rel,
            candidates[0]
        );
    }
    Ok(count)
}

/// Read a regular file below `root` fully into memory; `None` when the path
/// does not resolve, is a directory, or any read fails.
fn read_regular_file(root: &str, rel: &str) -> Option<Vec<u8>> {
    let abs = join_abs(root, rel);
    let node = vfs::lookup_path(&abs).ok()?;
    if node.is_directory() {
        return None;
    }
    let size = node.size() as usize;
    let mut data = alloc::vec![0u8; size];
    let mut off = 0usize;
    while off < size {
        match node.read(off as u64, &mut data[off..]) {
            Ok(0) => break,
            Ok(n) => off += n,
            Err(_) => return None,
        }
    }
    data.truncate(off);
    Some(data)
}

/// Install a single normalized, root-relative regular file.
fn install_one(root: &str, rel: &str, content: &[u8]) -> Result<(), InstallError> {
    let abs = join_abs(root, rel);

    // Resolve the installation root node.
    let root_node = vfs::lookup_path(root).map_err(|e| vfs_err("resolve_root", &abs, e))?;

    // Split the (non-empty, slash-joined) relative path into parent dirs + filename.
    let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    // `normalize_entry_path` never yields an empty Keep, so `comps` is non-empty.
    let (dirs, last) = comps.split_at(comps.len() - 1);
    let filename = last[0];

    // R10.2: walk the parent chain, creating any missing directory.
    let mut dir = root_node;
    for comp in dirs {
        dir = match dir.lookup(comp) {
            Ok(child) => child,
            Err(VfsError::NotFound) => match dir.create_dir(comp) {
                Ok(child) => child,
                // Lost a race (or pre-existing): re-resolve the now-present dir.
                Err(VfsError::AlreadyExists) => {
                    dir.lookup(comp).map_err(|e| vfs_err("mkdir", &abs, e))?
                }
                // ext2 OutOfSpace surfaces as IoError (see module docs) → NoSpace.
                Err(VfsError::IoError) => return Err(no_space("mkdir", &abs)),
                Err(e) => return Err(vfs_err("mkdir", &abs, e)),
            },
            Err(e) => return Err(vfs_err("mkdir", &abs, e)),
        };
    }

    // R10.7: replace an existing regular file so the stored size matches the new
    // content length. ext2 `write_file` only ever GROWS `i_size`, so overwriting in
    // place would leave a stale tail when the new content is shorter; removing and
    // recreating guarantees `size == content.len()` (R10.3).
    match dir.lookup(filename) {
        Ok(_existing) => {
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
