//! Per-`Compat_Process` file-descriptor table (R2.4, R2.6, R2.14).
//!
//! Maps small integer fds to open objects, with 0/1/2 pre-bound to the standard
//! streams (R2.2) and fresh descriptors allocated as the lowest free index `>= 3`
//! (R2.4). The pure index-allocation and close bookkeeping lives in the
//! dependency-free [`super::fd_alloc`] module so it is host-testable for Property
//! 7; this module layers the kernel-only [`OpenObject`] (which embeds
//! `Arc<dyn VfsNode>`) and the shared [`Errno`] mapping on top of it.
#![allow(dead_code)]

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::x86_64::linux::errno::Errno;
use crate::vfs::VfsNode;

use super::fd_alloc::FdSlots;

/// An object a file descriptor can refer to.
///
/// Standard streams resolve to the kernel console / stdin; everything else is an
/// ext2-backed file reached through the VFS, carrying its own read/write offset.
pub enum OpenObject {
    /// The kernel console (pre-bound to fds 1 and 2 for stdout/stderr).
    Console,
    /// Standard input (pre-bound to fd 0).
    Stdin,
    /// An open ext2-backed file and its current byte offset.
    File {
        /// The VFS node backing this descriptor.
        node: Arc<dyn VfsNode>,
        /// Current read/write offset within the file.
        offset: u64,
    },
    /// An open directory: the absolute path it was opened under, the snapshot of
    /// its child nodes taken at open time, and the `getdents64` cursor index into
    /// that snapshot (Feature: linux-binary-compat). Snapshotting the children at
    /// open avoids re-reading the VFS (a potentially blocking operation) while the
    /// `COMPAT_STATES` lock is held during `getdents64`.
    Dir {
        /// Absolute path the directory was opened under (used by `fchdir`).
        path: String,
        /// Child nodes captured at open time.
        children: Vec<Arc<dyn VfsNode>>,
        /// Index of the next child `getdents64` will emit.
        index: usize,
    },
}

impl OpenObject {
    /// Produce an independent duplicate of this descriptor for `dup`/`dup2`/`dup3`.
    ///
    /// Standard streams clone trivially; a file clones the `Arc` node handle and
    /// copies the current offset (Linux `dup` shares the open-file description, so
    /// the offset is the same at duplication time); a directory clones its captured
    /// child list and cursor so the duplicate continues iterating from the same
    /// position.
    pub fn dup_clone(&self) -> OpenObject {
        match self {
            OpenObject::Console => OpenObject::Console,
            OpenObject::Stdin => OpenObject::Stdin,
            OpenObject::File { node, offset } => OpenObject::File {
                node: Arc::clone(node),
                offset: *offset,
            },
            OpenObject::Dir {
                path,
                children,
                index,
            } => OpenObject::Dir {
                path: path.clone(),
                children: children.clone(),
                index: *index,
            },
        }
    }
}

/// A process's file-descriptor table.
///
/// Thin kernel-facing wrapper over the pure [`FdSlots`] bookkeeping: it fixes the
/// stored type to [`OpenObject`], pins the minimum allocatable descriptor at 3,
/// and maps the pure absent-fd error to [`Errno::EBADF`].
pub struct FdTable {
    slots: FdSlots<OpenObject>,
}

impl FdTable {
    /// Lowest descriptor a fresh `alloc` may return; 0/1/2 are reserved for the
    /// standard streams (R2.4).
    const FIRST_DYNAMIC_FD: usize = 3;

    /// Create a table with the standard streams pre-bound: fd 0 = stdin,
    /// fd 1 = console (stdout), fd 2 = console (stderr) (R2.2).
    pub fn with_standard_streams() -> Self {
        let mut initial: Vec<Option<OpenObject>> = Vec::with_capacity(Self::FIRST_DYNAMIC_FD);
        initial.push(Some(OpenObject::Stdin)); // fd 0
        initial.push(Some(OpenObject::Console)); // fd 1
        initial.push(Some(OpenObject::Console)); // fd 2
        Self {
            slots: FdSlots::from_slots(initial),
        }
    }

    /// Allocate the lowest free descriptor `>= 3`, store `obj` there, and return
    /// the descriptor, growing the table as needed (R2.4).
    pub fn alloc(&mut self, obj: OpenObject) -> u32 {
        self.slots.alloc(Self::FIRST_DYNAMIC_FD, obj)
    }

    /// Borrow the object referenced by `fd`, or `None` for an out-of-range/empty
    /// descriptor (caller maps `None` -> `EBADF`, R2.14).
    pub fn get(&self, fd: u32) -> Option<&OpenObject> {
        self.slots.get(fd)
    }

    /// Mutably borrow the object referenced by `fd`, or `None` for an
    /// out-of-range/empty descriptor (caller maps `None` -> `EBADF`, R2.14).
    pub fn get_mut(&mut self, fd: u32) -> Option<&mut OpenObject> {
        self.slots.get_mut(fd)
    }

    /// Close `fd`. Returns `Err(Errno::EBADF)` when the descriptor is absent or
    /// already closed, leaving the table unchanged; otherwise releases it and
    /// returns `Ok` (R2.6, R2.14).
    pub fn close(&mut self, fd: u32) -> Result<(), Errno> {
        self.slots.close(fd).map_err(|_| Errno::EBADF)
    }

    /// `dup` (32): duplicate `oldfd` into the lowest free descriptor `>= 3`,
    /// returning the new descriptor. `EBADF` if `oldfd` is not open.
    pub fn dup(&mut self, oldfd: u32) -> Result<u32, Errno> {
        self.dup_min(oldfd, Self::FIRST_DYNAMIC_FD as u32)
    }

    /// `fcntl(F_DUPFD)`: duplicate `oldfd` into the lowest free descriptor that is
    /// `>= min`, returning the new descriptor. `EBADF` if `oldfd` is not open.
    pub fn dup_min(&mut self, oldfd: u32, min: u32) -> Result<u32, Errno> {
        let dup = self.slots.get(oldfd).ok_or(Errno::EBADF)?.dup_clone();
        Ok(self.slots.alloc(min as usize, dup))
    }

    /// `dup2`/`dup3`: duplicate `oldfd` into the explicit descriptor `newfd`,
    /// closing whatever currently occupies `newfd` first. Returns `newfd`.
    /// `EBADF` if `oldfd` is not open.
    ///
    /// When `oldfd == newfd` the caller must enforce the `dup2`/`dup3` distinction
    /// (`dup2` returns `newfd` unchanged; `dup3` is `EINVAL`); this method assumes
    /// they differ.
    pub fn dup_to(&mut self, oldfd: u32, newfd: u32) -> Result<u32, Errno> {
        let dup = self.slots.get(oldfd).ok_or(Errno::EBADF)?.dup_clone();
        self.slots.set(newfd, dup);
        Ok(newfd)
    }
}
