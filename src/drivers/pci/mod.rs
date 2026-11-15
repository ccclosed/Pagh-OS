// drivers/pci/mod.rs — доступ к legacy PCI configuration space и перечисление шины
// 64-битное ядро ОС x86_64 на Rust (#![no_std])
//
// Реализует Component 1 дизайна networking-and-storage: обходит configuration
// space через legacy I/O-порты 0xCF8 (CONFIG_ADDRESS) и 0xCFC (CONFIG_DATA),
// находит присутствующие устройства и выставляет хелперы, используемые
// `virtio-drivers` для attach (чтение/запись конфигурации, включение bus-master).
// Virtio-устройства опознаются по PCI vendor id 0x1AF4; вызывающие фильтруют по
// `vendor_id`.
//
// Перечисление выполняет только чистые чтения configuration space (vendor/device
// id, class/subclass, header type). BAR size-probing намеренно НЕ выполняется:
// `virtio-drivers` находит и мапит BAR устройства через свой PCI transport, так
// что декодирование здесь давало неиспользуемые записи config space с побочными
// эффектами.

use alloc::vec::Vec;
use x86_64::instructions::port::Port;

/// Legacy PCI-порт CONFIG_ADDRESS.
const CONFIG_ADDRESS: u16 = 0xCF8;
/// Legacy PCI-порт CONFIG_DATA.
const CONFIG_DATA: u16 = 0xCFC;

/// PCI vendor id всех virtio-устройств QEMU.
pub const VIRTIO_VENDOR_ID: u16 = 0x1AF4;

/// PCI vendor id Intel (NIC e1000).
pub const VENDOR_INTEL: u16 = 0x8086;

/// Координата bus/device/function в PCI configuration space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciAddress {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

/// Присутствующее PCI-устройство (одна функция).
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
                let addr = PciAddress {
                    bus: bus as u8,
                    device,
                    function,
                };
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
