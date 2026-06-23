// drivers/mod.rs — Device Manager and driver traits
// 64-bit x86_64 OS kernel in Rust (#![no_std])

pub mod serial;
pub mod ps2_kbd;
pub mod framebuffer;
pub mod pci;
pub mod virtio;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::string::String;
use crate::sync::spinlock::Spinlock;

/// A text console sink (serial, framebuffer, ...). Implementations must be
/// safe to share across threads/IRQ context (interior-mutability behind a lock).
pub trait Console: Send + Sync {
    fn write_str(&self, s: &str);
    fn clear(&self);
}

/// Trait for character-device drivers (keyboard, serial, etc.).
pub trait CharacterDevice: Send + Sync {
    fn name(&self) -> &str;
    fn read_char(&self) -> Option<u8>;
    fn write_char(&self, c: u8);
}

/// Trait for block-device drivers (disk, etc.).
pub trait BlockDevice: Send + Sync {
    fn name(&self) -> &str;
    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<usize, ()>;
    fn write_block(&self, block: u64, buf: &[u8]) -> Result<usize, ()>;
}

/// Global device registry.
pub struct DeviceManager {
    chars: BTreeMap<String, Arc<dyn CharacterDevice>>,
    blocks: BTreeMap<String, Arc<dyn BlockDevice>>,
}

static DEVICE_MANAGER: Spinlock<DeviceManager> = Spinlock::new(DeviceManager {
    chars: BTreeMap::new(),
    blocks: BTreeMap::new(),
});

impl DeviceManager {
    pub fn register_char(&mut self, dev: Arc<dyn CharacterDevice>) {
        let name = String::from(dev.name());
        crate::debug!("[DEVMGR] Register char device: {}", name);
        self.chars.insert(name, dev);
    }

    pub fn register_block(&mut self, dev: Arc<dyn BlockDevice>) {
        let name = String::from(dev.name());
        crate::debug!("[DEVMGR] Register block device: {}", name);
        self.blocks.insert(name, dev);
    }
}

/// Called during boot to initialize built-in devices.
pub fn init() {
    crate::debug!("Initializing device manager...");
    ps2_kbd::init();
    framebuffer::init();
    crate::info!("Device manager ready");
}

/// Register a character device in the global manager.
pub fn register_char(dev: Arc<dyn CharacterDevice>) {
    DEVICE_MANAGER.lock().register_char(dev);
}

/// Register a block device in the global manager.
pub fn register_block(dev: Arc<dyn BlockDevice>) {
    DEVICE_MANAGER.lock().register_block(dev);
}

/// Get a character device by name
pub fn get_char(name: &str) -> Option<Arc<dyn CharacterDevice>> {
    DEVICE_MANAGER.lock().chars.get(name).map(Arc::clone)
}

/// Get a block device by name (mirrors [`get_char`]).
pub fn get_block(name: &str) -> Option<Arc<dyn BlockDevice>> {
    DEVICE_MANAGER.lock().blocks.get(name).map(Arc::clone)
}
