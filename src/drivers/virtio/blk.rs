// drivers/virtio/blk.rs — virtio-blk driver: attach `VirtIOBlk`, expose a `BlockDevice`
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// Component 3 of the networking-and-storage design. This module attaches the
// QEMU `virtio-blk` disk discovered by `pci::enumerate()` using the
// `virtio-drivers` 0.11 crate's PCI transport, wraps the resulting `VirtIOBlk`
// behind a `Spinlock`, and registers it in the global `DeviceManager` as the
// kernel `BlockDevice` named "virtio-blk0" (R3.1).
//
// ## virtio-drivers 0.11 PCI transport init flow
//
// The crate's PCI transport is built from two pieces:
//
//   * a `PciRoot<C: ConfigurationAccess>` — the root complex, parameterized
//     over a `ConfigurationAccess` implementation that performs raw 32-bit
//     config-space reads/writes for a `(bus, device, function)` + register
//     offset. We supply [`PaghPciAccess`], a zero-sized adapter that forwards
//     to pagh's `pci::config_read_u32` / `pci::config_write_u32`.
//   * a `DeviceFunction { bus, device, function }` naming the target device.
//
// `ConfigurationAccess` (from `transport::pci::bus`) has exactly three methods:
//   - `fn read_word(&self, df: DeviceFunction, offset: u8) -> u32`
//   - `fn write_word(&mut self, df: DeviceFunction, offset: u8, data: u32)`
//   - `unsafe fn unsafe_clone(&self) -> Self`   (read-only-fields clone)
//
// The flow is: build `PaghPciAccess`, wrap it in `PciRoot::new(access)`, form
// the `DeviceFunction` from the `PciDevice` our enumerate() found, enable
// bus-mastering (`pci::enable_bus_master`, which also sets memory-space enable
// so the transport can reach the device's MMIO BARs), then
// `PciTransport::new::<PaghHal, _>(&mut root, device_function)`. The transport
// probes the device's virtio PCI capabilities, locating the common-config /
// notify / ISR / device-config structures inside the device's *already
// allocated* memory BARs (QEMU + OVMF program the BARs before our kernel runs),
// mapping each via `PaghHal::mmio_phys_to_virt`. Finally
// `VirtIOBlk::<PaghHal, _>::new(transport)` performs the virtio device
// handshake + feature negotiation and sets up the request virtqueue.

use alloc::sync::Arc;

use virtio_drivers::device::blk::{VirtIOBlk, SECTOR_SIZE};
use virtio_drivers::transport::pci::bus::{
    ConfigurationAccess, DeviceFunction, PciRoot,
};
use virtio_drivers::transport::pci::PciTransport;

use crate::drivers::pci::{self, PciAddress, PciDevice};
use crate::drivers::virtio::hal::PaghHal;
use crate::drivers::{self, BlockDevice};
use crate::sync::spinlock::Spinlock;
use crate::{info, warn};

/// PCI device IDs that identify a virtio block device.
///
/// `0x1001` is the transitional (legacy) virtio-blk ID; `0x1042` is the modern
/// virtio-1.0 ID (`0x1040 + DeviceType::Block`). QEMU's `virtio-blk-pci`
/// presents one of these depending on its (non-)transitional configuration.
const VIRTIO_BLK_DEVICE_ID_LEGACY: u16 = 0x1001;
const VIRTIO_BLK_DEVICE_ID_MODERN: u16 = 0x1042;

/// The 512-byte sector size virtio-blk reports capacity in and that
/// `read_blocks`/`write_blocks` operate on.
const SECTOR: usize = SECTOR_SIZE; // 512

/// A `ConfigurationAccess` adapter bridging the `virtio-drivers` PCI transport
/// to pagh's legacy-port PCI config-space helpers.
///
/// Zero-sized: all state lives in the global config-space ports, so cloning is
/// trivial and free of aliasing concerns (the trait only ever uses a clone to
/// read read-only fields during enumeration, which we do not drive here).
struct PaghPciAccess;

impl ConfigurationAccess for PaghPciAccess {
    fn read_word(&self, device_function: DeviceFunction, register_offset: u8) -> u32 {
        pci::config_read_u32(df_to_addr(device_function), register_offset)
    }

    fn write_word(&mut self, device_function: DeviceFunction, register_offset: u8, data: u32) {
        pci::config_write_u32(df_to_addr(device_function), register_offset, data);
    }

    unsafe fn unsafe_clone(&self) -> Self {
        PaghPciAccess
    }
}

/// Convert a `virtio-drivers` `DeviceFunction` into a pagh `PciAddress`.
fn df_to_addr(df: DeviceFunction) -> PciAddress {
    PciAddress {
        bus: df.bus,
        device: df.device,
        function: df.function,
    }
}

/// A kernel `BlockDevice` backed by a `virtio-blk` disk.
///
/// Access is serialized behind a `Spinlock` (R3.7): the underlying `VirtIOBlk`
/// request path needs `&mut self`, and the device drives a single request
/// virtqueue, so every read/write takes the lock for the duration of the
/// device round-trip.
pub struct VirtioBlkDevice {
    inner: Spinlock<VirtIOBlk<PaghHal, PciTransport>>,
    name: &'static str,
    /// Device capacity in 512-byte sectors, as reported by virtio-blk.
    capacity_blocks: u64,
}

impl VirtioBlkDevice {
    /// Validate a `(block, len)` request against the sector-size and capacity
    /// rules shared by reads and writes (R3.2/R3.3/R3.4/R3.5).
    ///
    /// Returns the sector count on success, or `Err(())` if the length is not a
    /// positive multiple of the sector size or the range exceeds the device.
    fn validate(&self, block: u64, len: usize) -> Result<usize, ()> {
        // R3.5: length must be a positive multiple of the 512-byte sector size.
        if len == 0 || len % SECTOR != 0 {
            return Err(());
        }
        let sectors = (len / SECTOR) as u64;
        // R3.4: the whole range must lie within the device capacity.
        // `block + sectors` cannot overflow for any realistic disk, but guard
        // anyway so a crafted request can never wrap past the bound.
        match block.checked_add(sectors) {
            Some(end) if end <= self.capacity_blocks => Ok(sectors as usize),
            _ => Err(()),
        }
    }
}

impl BlockDevice for VirtioBlkDevice {
    fn name(&self) -> &str {
        self.name
    }

    /// Read `buf.len() / 512` sectors starting at sector `block` into `buf`.
    ///
    /// Returns the byte count transferred (`buf.len()`) on success (R3.2), or
    /// `Err(())` on a bad length / out-of-range request (R3.4/R3.5) leaving
    /// kernel-visible state unchanged.
    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<usize, ()> {
        self.validate(block, buf.len())?;
        let mut dev = self.inner.lock();
        dev.read_blocks(block as usize, buf).map_err(|_| ())?;
        Ok(buf.len())
    }

    /// Write `buf.len() / 512` sectors from `buf` starting at sector `block`.
    ///
    /// Returns the byte count transferred (`buf.len()`) on success (R3.3), or
    /// `Err(())` on a bad length / out-of-range request (R3.4/R3.5).
    fn write_block(&self, block: u64, buf: &[u8]) -> Result<usize, ()> {
        self.validate(block, buf.len())?;
        let mut dev = self.inner.lock();
        dev.write_blocks(block as usize, buf).map_err(|_| ())?;
        Ok(buf.len())
    }
}

/// True if a discovered PCI device is a virtio block device.
fn is_virtio_blk(dev: &PciDevice) -> bool {
    dev.is_virtio()
        && (dev.device_id == VIRTIO_BLK_DEVICE_ID_LEGACY
            || dev.device_id == VIRTIO_BLK_DEVICE_ID_MODERN)
}

/// Discover the first virtio-blk device among `devices`, attach it, and
/// register it as the `"virtio-blk0"` `BlockDevice` (R3.1).
///
/// If no virtio-blk device is present this logs a warning and returns without
/// touching anything (R17.4: boot is always preserved). Any transport/handshake
/// failure is likewise logged and skipped rather than faulting the kernel.
pub fn init_blk(devices: &[PciDevice]) {
    let dev = match devices.iter().find(|d| is_virtio_blk(d)) {
        Some(d) => d,
        None => {
            warn!("virtio-blk: no virtio block device found; storage disabled");
            return;
        }
    };

    let addr = dev.address;
    info!(
        "virtio-blk: found device {:02x}:{:02x}.{} (id {:#06x})",
        addr.bus, addr.device, addr.function, dev.device_id
    );

    // Enable bus-mastering (and memory-space decoding) so the device can drive
    // virtqueue DMA and respond to MMIO BAR accesses from the transport.
    pci::enable_bus_master(addr);

    // Build the crate's PCI root over our config-space adapter and name the
    // target device function.
    let mut root = PciRoot::new(PaghPciAccess);
    let device_function = DeviceFunction {
        bus: addr.bus,
        device: addr.device,
        function: addr.function,
    };

    // Construct the virtio PCI transport (probes capabilities, maps BARs).
    let transport = match PciTransport::new::<PaghHal, _>(&mut root, device_function) {
        Ok(t) => t,
        Err(e) => {
            warn!("virtio-blk: PCI transport init failed: {:?}; storage disabled", e);
            return;
        }
    };

    // Perform the virtio device handshake + feature negotiation and set up the
    // request virtqueue.
    let blk = match VirtIOBlk::<PaghHal, PciTransport>::new(transport) {
        Ok(b) => b,
        Err(e) => {
            warn!("virtio-blk: device init failed: {:?}; storage disabled", e);
            return;
        }
    };

    let capacity_blocks = blk.capacity();
    let read_only = blk.readonly();
    info!(
        "virtio-blk: capacity {} sectors ({} KiB), read_only={}",
        capacity_blocks,
        capacity_blocks * SECTOR as u64 / 1024,
        read_only
    );

    let device = Arc::new(VirtioBlkDevice {
        inner: Spinlock::new(blk),
        name: "virtio-blk0",
        capacity_blocks,
    });

    // Probe read of sector 0 to exercise the full virtqueue DMA path once at
    // boot. This is non-destructive (read only) and confirms the device and our
    // HAL/transport are operating before anything depends on the disk.
    let mut probe = [0u8; SECTOR];
    match device.read_block(0, &mut probe) {
        Ok(n) => info!("virtio-blk: probe read of sector 0 ok ({} bytes)", n),
        Err(()) => warn!("virtio-blk: probe read of sector 0 failed"),
    }

    drivers::register_block(device);
    info!("virtio-blk: registered block device \"virtio-blk0\"");
}
