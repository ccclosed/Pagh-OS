// drivers/mod.rs — device traits + registry (ported from x86_64, trimmed to the
// arch-neutral parts the storage stack needs). The riscv hardware drivers
// (virtio-mmio blk/net, ns16550/SBI console) live in their own top-level modules
// and register here.
#![allow(dead_code)]

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;

use crate::sync::spinlock::Spinlock;

/// Block-device driver trait (disk). Identical contract to the x86 kernel so the
/// ported ext2/journal stack works unchanged.
pub trait BlockDevice: Send + Sync {
    fn name(&self) -> &str;
    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<usize, ()>;
    fn write_block(&self, block: u64, buf: &[u8]) -> Result<usize, ()>;
    /// Total addressable 512-byte sectors (`0` = unknown).
    fn sector_count(&self) -> u64 {
        0
    }
}

/// Character-device trait (e.g. a serial console for `/dev/serial`).
pub trait CharacterDevice: Send + Sync {
    fn name(&self) -> &str;
    fn read_char(&self) -> Option<u8>;
}

struct DeviceManager {
    blocks: BTreeMap<String, Arc<dyn BlockDevice>>,
}

static DEVICE_MANAGER: Spinlock<DeviceManager> = Spinlock::new(DeviceManager {
    blocks: BTreeMap::new(),
});

/// Register a block device in the global manager.
pub fn register_block(dev: Arc<dyn BlockDevice>) {
    let name = String::from(dev.name());
    crate::debug!("[DEVMGR] register block device: {}", name);
    DEVICE_MANAGER.lock().blocks.insert(name, dev);
}

/// Look up a registered block device by name.
pub fn get_block(name: &str) -> Option<Arc<dyn BlockDevice>> {
    DEVICE_MANAGER.lock().blocks.get(name).map(Arc::clone)
}
