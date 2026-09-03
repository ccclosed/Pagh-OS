//! Minimal in-memory filesystem backing `/tmp`.
//!
//! Nothing else in the tree implements `create_dir` (the ext2 root is
//! read-mostly and directory creation there means real on-disk allocation),
//! but Linux userspace assumes a writable `/tmp`: nvim's `vim_mktempdir`
//! (mkdir /tmp/nvim.XXXXXX), mkstemp, unix-socket paths. A tiny ramfs gives
//! all of that POSIX surface without touching the ext2 driver.
//!
//! Design: directories hold `BTreeMap<name, Arc<dyn VfsNode>>` under a
//! spinlock; files hold `Spinlock<Vec<u8>>`. Every node gets a unique
//! `fs_ino` from a global counter so `(st_dev, st_ino)` identity stays sound
//! (glibc dedups by that pair).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::{VfsError, VfsNode, VfsResult};
use crate::sync::spinlock::Spinlock;

/// Ram-node inode numbers start high above the ext2 range so the pairs never
/// collide with real on-disk inodes.
static NEXT_INO: AtomicU64 = AtomicU64::new(0x0054_0000);

fn next_ino() -> u64 {
    NEXT_INO.fetch_add(1, Ordering::Relaxed)
}

// ─── directory ───

pub struct RamDir {
    name: String,
    ino: u64,
    children: Spinlock<BTreeMap<String, Arc<dyn VfsNode>>>,
}

impl RamDir {
    pub fn new(name: &str) -> Self {
        RamDir {
            name: String::from(name),
            ino: next_ino(),
            children: Spinlock::new(BTreeMap::new()),
        }
    }
}

impl VfsNode for RamDir {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        true
    }
    fn fs_ino(&self) -> u64 {
        self.ino
    }

    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        Ok(self.children.lock().values().cloned().collect())
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        self.children
            .lock()
            .get(name)
            .cloned()
            .ok_or(VfsError::NotFound)
    }

    fn create_dir(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        if name.is_empty() || name.contains('/') {
            return Err(VfsError::InvalidArgument);
        }
        let mut ch = self.children.lock();
        if ch.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let node = Arc::new(RamDir::new(name)) as Arc<dyn VfsNode>;
        ch.insert(String::from(name), Arc::clone(&node));
        Ok(node)
    }

    fn create_file(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        if name.is_empty() || name.contains('/') {
            return Err(VfsError::InvalidArgument);
        }
        let mut ch = self.children.lock();
        if ch.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let node = Arc::new(RamFile::new(name)) as Arc<dyn VfsNode>;
        ch.insert(String::from(name), Arc::clone(&node));
        Ok(node)
    }

    fn remove(&self, name: &str) -> VfsResult<()> {
        let mut ch = self.children.lock();
        match ch.get(name) {
            None => Err(VfsError::NotFound),
            Some(node) => {
                if node.is_directory() {
                    if let Ok(list) = node.readdir() {
                        if !list.is_empty() {
                            // Linux would say ENOTEMPTY; NotSupported keeps the
                            // caller-visible errno mapping unchanged.
                            return Err(VfsError::NotSupported);
                        }
                    }
                }
                ch.remove(name);
                Ok(())
            }
        }
    }
}

// ─── regular file ───

pub struct RamFile {
    name: String,
    ino: u64,
    data: Spinlock<Vec<u8>>,
}

impl RamFile {
    pub fn new(name: &str) -> Self {
        RamFile {
            name: String::from(name),
            ino: next_ino(),
            data: Spinlock::new(Vec::new()),
        }
    }
}

impl VfsNode for RamFile {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn fs_ino(&self) -> u64 {
        self.ino
    }
    fn size(&self) -> u64 {
        self.data.lock().len() as u64
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let data = self.data.lock();
        let off = offset as usize;
        if off >= data.len() {
            return Ok(0);
        }
        let n = core::cmp::min(buf.len(), data.len() - off);
        buf[..n].copy_from_slice(&data[off..off + n]);
        Ok(n)
    }

    fn write(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        let mut data = self.data.lock();
        let off = offset as usize;
        let end = off
            .checked_add(buf.len())
            .ok_or(VfsError::InvalidArgument)?;
        let old_len = data.len();
        if old_len < end {
            // Sparse-write gap (off > len) is zero-filled by resize, matching
            // POSIX semantics for writes past EOF. try_reserve first so a huge
            // user-driven offset cannot exhaust the fixed kernel heap
            // (allocation failure is a kernel abort, not an error return).
            data.try_reserve(end - old_len)
                .map_err(|_| VfsError::IoError)?;
            data.resize(end, 0);
        }
        data[off..end].copy_from_slice(buf);
        Ok(buf.len())
    }

    fn truncate(&self, size: u64) -> VfsResult<()> {
        let mut data = self.data.lock();
        let old_len = data.len();
        if size as usize > old_len {
            data.try_reserve(size as usize - old_len)
                .map_err(|_| VfsError::IoError)?;
        }
        data.resize(size as usize, 0);
        Ok(())
    }
}
