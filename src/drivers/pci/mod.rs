// drivers/pci/mod.rs — Legacy PCI configuration-space access and bus enumeration
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// Implements Component 1 of the networking-and-storage design: it walks the PCI
// configuration space via the legacy I/O ports 0xCF8 (CONFIG_ADDRESS) and 0xCFC
// (CONFIG_DATA), discovers present devices, decodes their BARs, and exposes
// helpers used by `virtio-drivers` to attach (config read/write, bus-master
// enable). Virtio devices are identified by PCI vendor id 0x1AF4; callers filter
// on `vendor_id`.

use alloc::vec::Vec;
use x86_64::instructions::port::Port;

/// Legacy PCI CONFIG_ADDRESS I/O port.
const CONFIG_ADDRESS: u16 = 0xCF8;
/// Legacy PCI CONFIG_DATA I/O port.
const CONFIG_DATA: u16 = 0xCFC;

/// PCI vendor id used by all QEMU virtio devices.
pub const VIRTIO_VENDOR_ID: u16 = 0x1AF4;

/// A bus/device/function coordinate in PCI configuration space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciAddress {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

/// A discovered PCI base address register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bar {
    /// Unused / unimplemented BAR slot (also used for the high half consumed by
    /// a preceding 64-bit memory BAR).
    None,
    /// Memory-mapped BAR.
    Memory { base: u64, size: u64, prefetchable: bool },
    /// Port-I/O BAR.
    Io { base: u32, size: u32 },
}

/// A present PCI device (one function).
#[derive(Debug, Clone)]
pub struct PciDevice {
    pub address: PciAddress,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub bars: [Bar; 6],
    pub interrupt_line: u8,
}

impl PciDevice {
    /// True when this device is a QEMU virtio device available for attachment.
    pub fn is_virtio(&self) -> bool {
        self.vendor_id == VIRTIO_VENDOR_ID
    }
}

/// Compose the 32-bit CONFIG_ADDRESS value for a (bus, device, function, offset)
/// tuple. The enable bit (0x8000_0000) is always set, and the offset is aligned
/// down to a dword boundary as the hardware requires.
fn config_address(addr: PciAddress, offset: u8) -> u32 {
    0x8000_0000
        | ((addr.bus as u32) << 16)
        | ((addr.device as u32) << 11)
        | ((addr.function as u32) << 8)
        | ((offset as u32) & 0xFC)
}

/// Read the 32-bit configuration-space dword at `offset` for `addr`.
pub fn config_read_u32(addr: PciAddress, offset: u8) -> u32 {
    let address = config_address(addr, offset);
    // SAFETY: 0xCF8/0xCFC are the architecturally fixed legacy PCI configuration
    // ports. Writing the enable-bit-tagged address to CONFIG_ADDRESS and then
    // reading CONFIG_DATA performs a standard configuration-space dword read and
    // has no side effects beyond selecting/reading the addressed register.
    unsafe {
        let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
        let mut data_port: Port<u32> = Port::new(CONFIG_DATA);
        addr_port.write(address);
        data_port.read()
    }
}

/// Write the 32-bit configuration-space dword `value` at `offset` for `addr`.
pub fn config_write_u32(addr: PciAddress, offset: u8, value: u32) {
    let address = config_address(addr, offset);
    // SAFETY: 0xCF8/0xCFC are the architecturally fixed legacy PCI configuration
    // ports. We select the register via CONFIG_ADDRESS and write the dword to
    // CONFIG_DATA. Callers are responsible for writing meaningful values (e.g.
    // BAR size-probing restores the original contents afterward).
    unsafe {
        let mut addr_port: Port<u32> = Port::new(CONFIG_ADDRESS);
        let mut data_port: Port<u32> = Port::new(CONFIG_DATA);
        addr_port.write(address);
        data_port.write(value);
    }
}

/// Enable a device for bus-mastering DMA. Sets bit 1 (memory space enable) and
/// bit 2 (bus master enable) of the command register (offset 0x04) so the device
/// can drive virtqueue DMA and respond to MMIO BAR accesses.
pub fn enable_bus_master(addr: PciAddress) {
    let cmd = config_read_u32(addr, 0x04);
    // Command register is the low 16 bits; status is the high 16 bits and is
    // mostly write-1-to-clear, so preserve it untouched by writing back the full
    // dword with only the command bits modified.
    let new = cmd | (1 << 1) | (1 << 2);
    config_write_u32(addr, 0x04, new);
}

/// Probe a single BAR slot. Returns the decoded `Bar` and whether it consumed
/// the following BAR slot as the high 32 bits of a 64-bit memory BAR.
fn read_bar(addr: PciAddress, index: u8) -> (Bar, bool) {
    let offset = 0x10 + index * 4;
    let original = config_read_u32(addr, offset);

    // Size probe: write all-ones, read back the mask, then restore the original
    // value. The device clears the bits it does not decode; size is the value of
    // the lowest set bit of the writable mask.
    config_write_u32(addr, offset, 0xFFFF_FFFF);
    let probe = config_read_u32(addr, offset);
    config_write_u32(addr, offset, original);

    if original & 0x1 != 0 {
        // I/O space BAR.
        let base = original & 0xFFFF_FFFC;
        let mask = probe & 0xFFFF_FFFC;
        let size = if mask == 0 { 0 } else { (!mask).wrapping_add(1) };
        if base == 0 && size == 0 {
            (Bar::None, false)
        } else {
            (Bar::Io { base, size }, false)
        }
    } else {
        // Memory space BAR.
        let prefetchable = original & 0x8 != 0;
        let bar_type = (original >> 1) & 0x3;
        let base_low = (original & 0xFFFF_FFF0) as u64;
        let mask_low = (probe & 0xFFFF_FFF0) as u64;

        if bar_type == 0x2 {
            // 64-bit memory BAR: the next slot holds the high 32 bits.
            let high_offset = offset + 4;
            let high_original = config_read_u32(addr, high_offset);
            config_write_u32(addr, high_offset, 0xFFFF_FFFF);
            let high_probe = config_read_u32(addr, high_offset);
            config_write_u32(addr, high_offset, high_original);

            let base = (high_original as u64) << 32 | base_low;
            let full_mask = ((high_probe as u64) << 32) | mask_low;
            let size = if full_mask == 0 { 0 } else { (!full_mask).wrapping_add(1) };
            if base == 0 && size == 0 {
                (Bar::None, true)
            } else {
                (Bar::Memory { base, size, prefetchable }, true)
            }
        } else {
            // 32-bit (or legacy below-1MiB) memory BAR.
            let base = base_low;
            let size = if mask_low == 0 {
                0
            } else {
                (!(mask_low | 0xFFFF_FFFF_0000_0000)).wrapping_add(1)
            };
            if base == 0 && size == 0 {
                (Bar::None, false)
            } else {
                (Bar::Memory { base, size, prefetchable }, false)
            }
        }
    }
}

/// Read and decode all six BARs of a device function.
fn read_bars(addr: PciAddress) -> [Bar; 6] {
    let mut bars = [Bar::None; 6];
    let mut i = 0u8;
    while i < 6 {
        let (bar, consumed_next) = read_bar(addr, i);
        bars[i as usize] = bar;
        if consumed_next {
            // The following slot is the high half of this 64-bit BAR.
            if (i + 1) < 6 {
                bars[(i + 1) as usize] = Bar::None;
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    bars
}

/// Read the full descriptor for a present function at `addr`.
fn read_device(addr: PciAddress, id0: u32) -> PciDevice {
    let vendor_id = (id0 & 0xFFFF) as u16;
    let device_id = (id0 >> 16) as u16;

    // Offset 0x08: revision (7..0), prog-if (15..8), subclass (23..16), class (31..24).
    let class_reg = config_read_u32(addr, 0x08);
    let subclass = ((class_reg >> 16) & 0xFF) as u8;
    let class = ((class_reg >> 24) & 0xFF) as u8;

    // Offset 0x3C: interrupt line (7..0), interrupt pin (15..8).
    let int_reg = config_read_u32(addr, 0x3C);
    let interrupt_line = (int_reg & 0xFF) as u8;

    PciDevice {
        address: addr,
        vendor_id,
        device_id,
        class,
        subclass,
        bars: read_bars(addr),
        interrupt_line,
    }
}

/// Returns the header-type byte (offset 0x0C, bits 16..23) for a function.
fn header_type(addr: PciAddress) -> u8 {
    let reg = config_read_u32(addr, 0x0C);
    ((reg >> 16) & 0xFF) as u8
}

/// Enumerate the entire PCI configuration space, returning every present
/// function. Absent functions (vendor id 0xFFFF) are skipped, and non-
/// multifunction devices only have function 0 probed.
pub fn enumerate() -> Vec<PciDevice> {
    let mut out = Vec::new();
    for bus in 0u16..=255 {
        for device in 0u8..32 {
            for function in 0u8..8 {
                let addr = PciAddress { bus: bus as u8, device, function };
                let id0 = config_read_u32(addr, 0x00);
                let vendor = (id0 & 0xFFFF) as u16;
                if vendor == 0xFFFF {
                    // No device/function present here.
                    if function == 0 {
                        // Function 0 absent => whole device absent.
                        break;
                    }
                    continue;
                }

                out.push(read_device(addr, id0));

                if function == 0 {
                    // If function 0 is not multifunction, skip functions 1..8.
                    let ht = header_type(addr);
                    if ht & 0x80 == 0 {
                        break;
                    }
                }
            }
        }
    }
    out
}
