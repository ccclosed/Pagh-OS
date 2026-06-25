// drivers/pci/mod.rs — Legacy PCI configuration-space access and bus enumeration
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// Implements Component 1 of the networking-and-storage design: it walks the PCI
// configuration space via the legacy I/O ports 0xCF8 (CONFIG_ADDRESS) and 0xCFC
// (CONFIG_DATA), discovers present devices, and exposes helpers used by
// `virtio-drivers` to attach (config read/write, bus-master enable). Virtio
// devices are identified by PCI vendor id 0x1AF4; callers filter on `vendor_id`.
//
// Enumeration performs only pure configuration-space reads (vendor/device id,
// class/subclass, header type). BAR size-probing is intentionally NOT performed:
// `virtio-drivers` discovers and maps device BARs through its own PCI transport,
// so decoding them here produced unused, side-effecting config-space writes.

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

/// A present PCI device (one function).
#[derive(Debug, Clone)]
pub struct PciDevice {
    pub address: PciAddress,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
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

/// Read the full descriptor for a present function at `addr`.
fn read_device(addr: PciAddress, id0: u32) -> PciDevice {
    let vendor_id = (id0 & 0xFFFF) as u16;
    let device_id = (id0 >> 16) as u16;

    // Offset 0x08: revision (7..0), prog-if (15..8), subclass (23..16), class (31..24).
    let class_reg = config_read_u32(addr, 0x08);
    let subclass = ((class_reg >> 16) & 0xFF) as u8;
    let class = ((class_reg >> 24) & 0xFF) as u8;

    PciDevice {
        address: addr,
        vendor_id,
        device_id,
        class,
        subclass,
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
