// vfs/mod.rs — Virtual File System (ported from x86_64, adapted for riscv).
// VfsNode trait, /dev/null, /dev/serial (riscv UART), mount_at, lookup_path.
// The x86 vfs::elf / elf_classify submodules are NOT ported here — the riscv
// ELF loader lives in `crate::elf`.
#![allow(dead_code)]

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::sync::spinlock::Spinlock;

pub type VfsResult<T> = Result<T, VfsError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsError {
    NotFound,
    NotSupported,
    InvalidArgument,
    IoError,
    AlreadyExists,
}

pub trait VfsNode: Send + Sync {
    fn name(&self) -> &str;
    fn is_directory(&self) -> bool;
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(VfsError::NotSupported)
    }
    fn write(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::NotSupported)
    }
    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        Err(VfsError::NotSupported)
    }
    fn lookup(&self, _name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        Err(VfsError::NotFound)
    }
    fn size(&self) -> u64 {
        0
    }
    fn create_dir(&self, _name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        Err(VfsError::NotSupported)
    }
    fn create_file(&self, _name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        Err(VfsError::NotSupported)
    }
    fn remove(&self, _name: &str) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }
    fn sync(&self) {}
}

// ─── /dev/null ──────────────────────────────────────────────────────────────
struct NullDevice;
impl VfsNode for NullDevice {
    fn name(&self) -> &str {
        "null"
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn read(&self, _o: u64, _b: &mut [u8]) -> VfsResult<usize> {
        Ok(0)
    }
    fn write(&self, _o: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(buf.len())
    }
}

// ─── /dev/serial (riscv ns16550/SBI console) ─────────────────────────────────
struct SerialDevice;
impl VfsNode for SerialDevice {
    fn name(&self) -> &str {
        "serial"
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn read(&self, _o: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match crate::uart::try_getb() {
            Some(b) => {
                buf[0] = b;
                Ok(1)
            }
            None => Ok(0),
        }
    }
    fn write(&self, _o: u64, buf: &[u8]) -> VfsResult<usize> {
        for &b in buf {
            crate::uart::putb(b);
        }
        Ok(buf.len())
    }
}

// ─── /dev directory ───────────────────────────────────────────────────────
struct DevDirectory {
    children: Vec<Arc<dyn VfsNode>>,
}
impl VfsNode for DevDirectory {
    fn name(&self) -> &str {
        "dev"
    }
    fn is_directory(&self) -> bool {
        true
    }
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

// ─── Mount point wrapper ──────────────────────────────────────────────────
struct MountNode {
    name: String,
    inner: Arc<dyn VfsNode>,
}
impl VfsNode for MountNode {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        self.inner.is_directory()
    }
    fn read(&self, o: u64, b: &mut [u8]) -> VfsResult<usize> {
        self.inner.read(o, b)
    }
    fn write(&self, o: u64, b: &[u8]) -> VfsResult<usize> {
        self.inner.write(o, b)
    }
    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        self.inner.readdir()
    }
    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        self.inner.lookup(name)
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn create_dir(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        self.inner.create_dir(name)
    }
    fn create_file(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        self.inner.create_file(name)
    }
    fn remove(&self, name: &str) -> VfsResult<()> {
        self.inner.remove(name)
    }
    fn sync(&self) {
        self.inner.sync()
    }
}

// ─── Root directory ───────────────────────────────────────────────────────
struct RootDirectory {
    children: Spinlock<Vec<Arc<dyn VfsNode>>>,
}
impl VfsNode for RootDirectory {
    fn name(&self) -> &str {
        "/"
    }
    fn is_directory(&self) -> bool {
        true
    }
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

static VFS_ROOT: Spinlock<Option<Arc<dyn VfsNode>>> = Spinlock::new(None);
static ROOT_DIR: Spinlock<Option<Arc<RootDirectory>>> = Spinlock::new(None);

pub fn init() {
    let null = Arc::new(NullDevice) as Arc<dyn VfsNode>;
    let serial = Arc::new(SerialDevice) as Arc<dyn VfsNode>;
    let dev = Arc::new(DevDirectory {
        children: vec![Arc::clone(&null), Arc::clone(&serial)],
    }) as Arc<dyn VfsNode>;
    let root = Arc::new(RootDirectory {
        children: Spinlock::new(vec![Arc::clone(&dev)]),
    });
    *ROOT_DIR.lock() = Some(Arc::clone(&root));
    *VFS_ROOT.lock() = Some(root as Arc<dyn VfsNode>);
    crate::debug!("VFS initialized (/dev/null, /dev/serial)");
}

/// Attach a `VfsNode` subtree at a single-component absolute path (e.g. `/mnt`).
pub fn mount_at(path: &str, node: Arc<dyn VfsNode>) -> VfsResult<()> {
    let name = match path.strip_prefix('/') {
        Some(rest) => rest.trim_end_matches('/'),
        None => return Err(VfsError::InvalidArgument),
    };
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

/// Resolve an absolute path from the VFS root to a node.
pub fn lookup_path(path: &str) -> VfsResult<Arc<dyn VfsNode>> {
    if !path.starts_with('/') {
        return Err(VfsError::InvalidArgument);
    }
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
