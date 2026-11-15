//! Символьные устройства desktop-варианта B: `/dev/fb0` и
//! `/dev/input/event0`.
//!
//! Узлы — тонкие адаптеры между VFS-трейтом и драйверами
//! ([`crate::drivers::framebuffer`], [`crate::drivers::evdev`]). Реальная
//! работа (ioctl/mmap/read/poll) живёт в syscall-слое: `VfsNode` не знает
//! пользовательских указателей и `errno`.

use alloc::sync::Arc;

use super::{VfsNode, VfsResult};
use crate::drivers::framebuffer::FbInfo;

/// `/dev/fb0` — Limine-фреймбуфер, который desktop забирает через open/mmap.
pub struct FramebufferDevice;

impl VfsNode for FramebufferDevice {
    fn name(&self) -> &str {
        "fb0"
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn is_fbdev(&self) -> bool {
        true
    }
    fn fb_info(&self) -> Option<FbInfo> {
        crate::drivers::framebuffer::fb_info()
    }
    fn size(&self) -> u64 {
        self.fb_info().map(|i| i.smem_len).unwrap_or(0)
    }
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        // Фреймбуфер читается через mmap: байтового read(2) у Linux fbdev нет
        // (он вернул бы EINVAL); syscall-слой отклоняет такое чтение.
        Err(super::VfsError::NotSupported)
    }
    fn write(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(super::VfsError::NotSupported)
    }
}

/// `/dev/input/event0` — evdev-подобный поток событий.
pub struct EventDevice;

impl VfsNode for EventDevice {
    fn name(&self) -> &str {
        "event0"
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn is_evdev(&self) -> bool {
        true
    }
    fn size(&self) -> u64 {
        // Размер не байтовый: читатель забирает по одному событию из очереди.
        0
    }
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(super::VfsError::NotSupported)
    }
    fn write(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(super::VfsError::NotSupported)
    }
}

/// Каталог-контейнер для устройств (например, `/dev/input`).
pub struct DeviceDirectory {
    name: &'static str,
    children: alloc::vec::Vec<Arc<dyn VfsNode>>,
}

impl DeviceDirectory {
    pub fn new(name: &'static str, children: alloc::vec::Vec<Arc<dyn VfsNode>>) -> Self {
        DeviceDirectory { name, children }
    }
}

impl VfsNode for DeviceDirectory {
    fn name(&self) -> &str {
        self.name
    }
    fn is_directory(&self) -> bool {
        true
    }
    fn readdir(&self) -> VfsResult<alloc::vec::Vec<Arc<dyn VfsNode>>> {
        Ok(self.children.clone())
    }
    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        self.children
            .iter()
            .find(|c| c.name() == name)
            .cloned()
            .ok_or(super::VfsError::NotFound)
    }
}

/// `/dev/fb0` как `Arc<dyn VfsNode>`.
pub fn fb0() -> Arc<dyn VfsNode> {
    Arc::new(FramebufferDevice) as Arc<dyn VfsNode>
}

/// `/dev/input` с единственным `event0`.
pub fn input_dir() -> Arc<dyn VfsNode> {
    let event0 = Arc::new(EventDevice) as Arc<dyn VfsNode>;
    Arc::new(DeviceDirectory::new("input", alloc::vec![event0])) as Arc<dyn VfsNode>
}
