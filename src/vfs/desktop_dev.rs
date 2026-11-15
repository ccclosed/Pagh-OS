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

/// `/dev/vtconsole0` — read-only shared VT surface (ABI v1).
///
/// Содержимое буфера живёт в [`crate::drivers::vtsurface`]; узел только
/// сообщает геометрию/кадры для `mmap` и отдаёт `read(2)`-снимок. Запись через
/// этот fd запрещена: экран публикует только ядро.
pub struct VtConsoleDevice;

impl VfsNode for VtConsoleDevice {
    fn name(&self) -> &str {
        "vtconsole0"
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn is_vtconsole(&self) -> bool {
        true
    }
    fn vtconsole_info(&self) -> Option<crate::drivers::vtsurface::VtConsoleInfo> {
        crate::drivers::vtsurface::info()
    }
    fn vtconsole_page(&self, index: u64) -> Option<u64> {
        crate::drivers::vtsurface::page(index)
    }
    fn mode(&self) -> u32 {
        // Экран консоли может содержать секреты: только владелец консоли
        // (root) может читать shared surface.
        0o600
    }
    fn size(&self) -> u64 {
        self.vtconsole_info()
            .map(|i| i.total_bytes as u64)
            .unwrap_or(0)
    }
    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let Ok(offset) = u32::try_from(offset) else {
            return Ok(0);
        };
        Ok(crate::drivers::vtsurface::read_at(offset, buf))
    }
    fn write(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(super::VfsError::NotSupported)
    }
}

/// `/dev/vtinput` — write-only приём VT-байтов от терминала.
pub struct VtInputDevice;

impl VfsNode for VtInputDevice {
    fn name(&self) -> &str {
        "vtinput"
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn is_vtinput(&self) -> bool {
        true
    }
    fn mode(&self) -> u32 {
        0o600
    }
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(super::VfsError::NotSupported)
    }
    fn write(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(crate::drivers::vtinput::write(buf))
    }
}

/// `/dev/vtconsole0` как `Arc<dyn VfsNode>`.
pub fn vtconsole0() -> Arc<dyn VfsNode> {
    Arc::new(VtConsoleDevice) as Arc<dyn VfsNode>
}

/// `/dev/vtinput` как `Arc<dyn VfsNode>`.
pub fn vtinput() -> Arc<dyn VfsNode> {
    Arc::new(VtInputDevice) as Arc<dyn VfsNode>
}
