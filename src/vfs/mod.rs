// vfs/mod.rs — Virtual File System: trait VfsNode, /dev/null, /dev/serial
// 64-bit x86_64 OS kernel in Rust (#![no_std])

pub mod elf;

use alloc::sync::Arc;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::vec;

pub type VfsResult<T> = Result<T, VfsError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsError {
    NotFound,
    PermissionDenied,
    NotSupported,
    InvalidArgument,
    IoError,
    AlreadyExists,
}

pub trait VfsNode: Send + Sync {
    fn name(&self) -> &str;
    fn is_directory(&self) -> bool;
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> { Err(VfsError::NotSupported) }
    fn write(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> { Err(VfsError::NotSupported) }
    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> { Err(VfsError::NotSupported) }
    fn lookup(&self, _name: &str) -> VfsResult<Arc<dyn VfsNode>> { Err(VfsError::NotFound) }
    fn size(&self) -> u64 { 0 }

    // ── mutating directory operations (Task 5.2) ──
    //
    // Default to `NotSupported` so read-only nodes (e.g. `/dev`) need no change;
    // the ext2 directory node overrides these to route through the journaled
    // write path.

    /// Create a child directory named `name`, returning its node.
    fn create_dir(&self, _name: &str) -> VfsResult<Arc<dyn VfsNode>> { Err(VfsError::NotSupported) }
    /// Create an empty child file named `name`, returning its node.
    fn create_file(&self, _name: &str) -> VfsResult<Arc<dyn VfsNode>> { Err(VfsError::NotSupported) }
    /// Remove the child entry named `name` (file or empty directory).
    fn remove(&self, _name: &str) -> VfsResult<()> { Err(VfsError::NotSupported) }
    /// Flush any buffered state to stable storage. Default no-op.
    fn sync(&self) {}
}

// ─── /dev/null ────────────────────────────────────────────────────────────

struct NullDevice;

impl VfsNode for NullDevice {
    fn name(&self) -> &str { "null" }
    fn is_directory(&self) -> bool { false }

    fn read(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Ok(0)
    }

    fn write(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(buf.len())
    }
}

// ─── /dev/serial ──────────────────────────────────────────────────────────

struct SerialDevice;

impl VfsNode for SerialDevice {
    fn name(&self) -> &str { "serial" }
    fn is_directory(&self) -> bool { false }

    fn read(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.is_empty() { return Ok(0); }
        // SAFETY: Reading from COM1 LSR and DATA ports.
        unsafe {
            let mut lsr = x86_64::instructions::port::Port::<u8>::new(0x3FD);
            if lsr.read() & 1 != 0 {
                let mut data = x86_64::instructions::port::Port::<u8>::new(0x3F8);
                buf[0] = data.read();
                Ok(1)
            } else {
                Ok(0)
            }
        }
    }

    fn write(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        for &byte in buf {
            crate::drivers::serial::_kprint(core::format_args!("{}", byte as char));
        }
        Ok(buf.len())
    }
}

// ─── /dev directory ───────────────────────────────────────────────────────

struct DevDirectory {
    children: Vec<Arc<dyn VfsNode>>,
}

impl VfsNode for DevDirectory {
    fn name(&self) -> &str { "dev" }
    fn is_directory(&self) -> bool { true }

    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        Ok(self.children.clone())
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        for child in &self.children {
            if child.name() == name {
                return Ok(Arc::clone(child));
            }
        }
        Err(VfsError::NotFound)
    }
}

// ─── Mount point wrapper ────────────────────────────────────────────────────

/// Presents an arbitrary `VfsNode` subtree under a chosen mount name.
///
/// `mount_at("/mnt", ext2_root)` wraps the filesystem's root (whose own
/// `name()` is `"/"`) in a `MountNode` named `"mnt"` and splices it into the
/// VFS root's children, so `lookup_path("/mnt")` finds it and
/// `lookup_path("/mnt/<file>")` delegates straight into the mounted tree. Every
/// `VfsNode` method forwards to the inner node.
struct MountNode {
    name: String,
    inner: Arc<dyn VfsNode>,
}

impl VfsNode for MountNode {
    fn name(&self) -> &str { &self.name }
    fn is_directory(&self) -> bool { self.inner.is_directory() }
    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> { self.inner.read(offset, buf) }
    fn write(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> { self.inner.write(offset, buf) }
    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> { self.inner.readdir() }
    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> { self.inner.lookup(name) }
    fn size(&self) -> u64 { self.inner.size() }
    fn create_dir(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> { self.inner.create_dir(name) }
    fn create_file(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> { self.inner.create_file(name) }
    fn remove(&self, name: &str) -> VfsResult<()> { self.inner.remove(name) }
    fn sync(&self) { self.inner.sync() }
}

// ─── Root directory ─────────────────────────────────────────────────────────

struct RootDirectory {
    // Mutable so `mount_at` can splice in new top-level entries (e.g. `/mnt`)
    // after boot. Guarded by a spinlock; `lookup`/`readdir` take it briefly.
    children: Spinlock<Vec<Arc<dyn VfsNode>>>,
}

impl VfsNode for RootDirectory {
    fn name(&self) -> &str { "/" }
    fn is_directory(&self) -> bool { true }

    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        Ok(self.children.lock().clone())
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        for child in self.children.lock().iter() {
            if child.name() == name {
                return Ok(Arc::clone(child));
            }
        }
        Err(VfsError::NotFound)
    }
}

// ─── Initialization ───────────────────────────────────────────────────────

use crate::sync::spinlock::Spinlock;

static VFS_ROOT: Spinlock<Option<Arc<dyn VfsNode>>> = Spinlock::new(None);
/// Typed handle to the same root node held in `VFS_ROOT`, so `mount_at` can
/// splice into its mutable children without downcasting a `dyn VfsNode`.
static ROOT_DIR: Spinlock<Option<Arc<RootDirectory>>> = Spinlock::new(None);

pub fn init() {
    crate::debug!("Mounting /dev...");
    let null = Arc::new(NullDevice) as Arc<dyn VfsNode>;
    crate::debug!("/dev/null — ready");
    let serial = Arc::new(SerialDevice) as Arc<dyn VfsNode>;
    crate::debug!("/dev/serial — ready");
    let dev = Arc::new(DevDirectory {
        children: vec![Arc::clone(&null), Arc::clone(&serial)],
    }) as Arc<dyn VfsNode>;
    let root = Arc::new(RootDirectory {
        children: Spinlock::new(vec![Arc::clone(&dev)]),
    });
    *ROOT_DIR.lock() = Some(Arc::clone(&root));
    *VFS_ROOT.lock() = Some(root as Arc<dyn VfsNode>);
    crate::debug!("VFS Initialized");
}

/// Attach an arbitrary `VfsNode` subtree into the VFS tree at an absolute path.
///
/// v1 simplification: only **single-component** mount points are supported
/// (e.g. `"/mnt"`, not `"/a/b"`). The node is spliced in as a child of the root
/// under the path's single name, wrapped in a [`MountNode`] so it presents that
/// name (the filesystem's own root reports `"/"`). After this,
/// `lookup_path("/mnt")` resolves to the mount and `lookup_path("/mnt/<file>")`
/// delegates into the mounted tree (the mounted root's own `lookup`/`readdir`
/// handle deeper path components). Existing top-level entries such as `/dev`
/// are untouched. Re-mounting the same name replaces the previous entry.
pub fn mount_at(path: &str, node: Arc<dyn VfsNode>) -> VfsResult<()> {
    let name = match path.strip_prefix('/') {
        Some(rest) => rest.trim_end_matches('/'),
        None => return Err(VfsError::InvalidArgument),
    };
    // Single-level mount points only (v1): reject empty and nested paths.
    if name.is_empty() || name.contains('/') {
        return Err(VfsError::InvalidArgument);
    }

    let root = match ROOT_DIR.lock().clone() {
        Some(r) => r,
        None => return Err(VfsError::NotFound),
    };

    let mount = Arc::new(MountNode {
        name: String::from(name),
        inner: node,
    }) as Arc<dyn VfsNode>;

    let mut children = root.children.lock();
    children.retain(|c| c.name() != name);
    children.push(mount);
    Ok(())
}

// ─── Path resolution ──────────────────────────────────────────────────────

/// Returns a clone of the VFS root node, or `None` if the VFS is uninitialized.
///
/// Useful for the shell to list `/` directly.
pub fn root() -> Option<Arc<dyn VfsNode>> {
    VFS_ROOT.lock().clone()
}

/// Resolves an absolute path from the VFS root to a node.
///
/// - The path MUST start with `/` (otherwise `VfsError::InvalidArgument`).
/// - `/` resolves to the root node itself.
/// - Empty components are skipped, so `/dev/serial`, `/dev/serial/`, and
///   `//dev//serial` all resolve identically.
/// - Any component that cannot be found yields `VfsError::NotFound`.
pub fn lookup_path(path: &str) -> VfsResult<Arc<dyn VfsNode>> {
    if !path.starts_with('/') {
        return Err(VfsError::InvalidArgument);
    }

    // Take a clone of the root Arc, then release the VFS_ROOT lock before
    // walking the tree. The nodes are independently reference-counted and
    // internally synchronized/immutable, so we must not hold the VFS_ROOT
    // spinlock while calling `lookup` (avoids re-entrancy/deadlock).
    let mut node = match VFS_ROOT.lock().clone() {
        Some(root) => root,
        None => return Err(VfsError::NotFound),
    };

    for component in path.split('/') {
        if component.is_empty() {
            continue;
        }
        node = node.lookup(component)?;
    }

    Ok(node)
}
