//! Intel 82540EM (e1000) NIC driver — QEMU `-device e1000`.
//!
//! Simpler and more portable than virtio-net: works on real hardware,
//! VirtualBox, VMware, and QEMU. Uses MMIO register access + legacy TX/RX
//! descriptor rings.
//!
//! ## Usage
//!
//! ```text
//! let mac = e1000::attach(pci_devices)?;   // find + init NIC
//! e1000::send(frame_bytes);                // transmit one Ethernet frame
//! let frame = e1000::receive();            // pop one received frame (or None)
//! ```

use crate::sync::spinlock::Spinlock;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr;

// ─── PCI identification ──────────────────────────────────────────────────────

pub const VENDOR_ID: u16 = 0x8086;
pub const DEVICE_ID: u16 = 0x100E; // 82540EM (QEMU default)

// ─── MMIO register offsets ───────────────────────────────────────────────────

const REG_CTRL: usize = 0x0000;
const REG_STATUS: usize = 0x0008;
const REG_EECD: usize = 0x0010; // EEPROM ctrl/data
const REG_EERD: usize = 0x0014; // EEPROM read
const REG_ICR: usize = 0x00C0; // interrupt cause (write 1 to clear)
const REG_RCTL: usize = 0x0100; // receive control
const REG_TCTL: usize = 0x0400; // transmit control
const REG_TDBAL: usize = 0x3800; // TX desc base addr low
const REG_TDBAH: usize = 0x3804; // TX desc base addr high
const REG_TDH: usize = 0x3810; // TX desc head
const REG_TDT: usize = 0x3818; // TX desc tail
const REG_RDBAL: usize = 0x2800; // RX desc base addr low
const REG_RDBAH: usize = 0x2804; // RX desc base addr high
const REG_RDH: usize = 0x2810; // RX desc head
const REG_RDT: usize = 0x2818; // RX desc tail
const REG_RXDLEN: usize = 0x2808; // RX desc ring length
const REG_TXDLEN: usize = 0x3808; // TX desc ring length

// CTRL bits
const CTRL_SLU: u32 = 1 << 6; // set link up
const CTRL_LRST: u32 = 1 << 3; // link reset

// RCTL bits
const RCTL_EN: u32 = 1 << 1;
const RCTL_SBP: u32 = 1 << 2;
const RCTL_UPE: u32 = 1 << 3; // unicast promiscuous
const RCTL_MPE: u32 = 1 << 4; // multicast promiscuous
const RCTL_LPE: u32 = 1 << 5; // long packet
const RCTL_BAM: u32 = 1 << 15; // broadcast accept
const RCTL_BSIZE_2048: u32 = 0 << 16;
const RCTL_BSEX: u32 = 1 << 26;

// TCTL bits
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3; // pad short packets

// TX descriptor command bits
const TXD_CMD_EOP: u8 = 1 << 0;
const TXD_CMD_IFCS: u8 = 1 << 1;
const TXD_CMD_RS: u8 = 1 << 3;

// RX/TX descriptor status
const RXD_STATUS_DD: u8 = 1 << 0; // descriptor done
const TXD_STATUS_DD: u8 = 1 << 0;

const DESC_COUNT: usize = 64;
const FRAME_MAX: usize = 1522; // Ethernet MTU + headers + VLAN tag

// ─── Descriptors ─────────────────────────────────────────────────────────────

#[repr(C)]
struct TxDesc {
    buffer_addr: u64,
    length: u16,
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
    special: u16,
}
const _: () = assert!(core::mem::size_of::<TxDesc>() == 16);

#[repr(C)]
struct RxDesc {
    buffer_addr: u64,
    length: u16,
    checksum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

// ─── Device state ────────────────────────────────────────────────────────────

pub struct E1000 {
    mmio_base: usize, // HHDM virtual address of BAR 0
    mac: [u8; 6],
    tx_descs: &'static mut [TxDesc],
    tx_bufs: Vec<u8>, // contiguous backing store for tx descriptors
    tx_tail: usize,
    rx_descs: &'static mut [RxDesc],
    rx_bufs: Vec<u8>,
    rx_head: usize, // next descriptor to check
}

unsafe impl Send for E1000 {}
unsafe impl Sync for E1000 {}

impl E1000 {
    // ── MMIO access ──

    unsafe fn reg_read(&self, offset: usize) -> u32 {
        ptr::read_volatile((self.mmio_base + offset) as *const u32)
    }

    unsafe fn reg_write(&self, offset: usize, value: u32) {
        ptr::write_volatile((self.mmio_base + offset) as *mut u32, value);
    }

    /// Read MAC from EEPROM (words 0..=2).
    fn read_mac_from_eeprom(&self) -> [u8; 6] {
        let mut mac = [0u8; 6];
        for i in 0..3u32 {
            // Trigger EEPROM read: write address | start bit, poll for done.
            unsafe {
                self.reg_write(REG_EERD, (i << 8) | 1);
                let mut spin = 0;
                while spin < 100_000 {
                    if self.reg_read(REG_EERD) & (1 << 4) != 0 {
                        break;
                    }
                    spin += 1;
                }
                let word = (self.reg_read(REG_EERD) >> 16) as u16;
                mac[(i * 2) as usize] = (word & 0xFF) as u8;
                mac[(i * 2 + 1) as usize] = (word >> 8) as u8;
            }
        }
        mac
    }

    /// Initialise TX/RX rings and enable send/receive.
    fn init_rings(&mut self) {
        // Allocate descriptor arrays from kernel heap. The e1000 needs
        // PHYSICAL addresses, but our heap is backed by individually mapped
        // frames through the HHDM window — phys_to_virt gives us the mapping.
        // For simplicity we allocate one contiguous region per ring.
        let tx_size = core::mem::size_of::<TxDesc>() * DESC_COUNT;
        let rx_size = core::mem::size_of::<RxDesc>() * DESC_COUNT;

        // Zero-initialise descriptors.
        let tx_layout: Vec<TxDesc> = {
            let mut v = Vec::new();
            for _ in 0..DESC_COUNT {
                v.push(TxDesc { buffer_addr: 0, length: 0, cso: 0, cmd: 0, status: 0, css: 0, special: 0 });
            }
            v
        };
        let rx_layout: Vec<RxDesc> = {
            let mut v = Vec::new();
            for _ in 0..DESC_COUNT {
                v.push(RxDesc { buffer_addr: 0, length: 0, checksum: 0, status: 0, errors: 0, special: 0 });
            }
            v
        };

        // We need PHYSICAL addresses for the descriptor arrays. Since our
        // allocator returns virtual addresses through the heap, we need to
        // translate. For now, allocate backing buffers and get their physical
        // addresses via the VMM.
        //
        // SAFETY: these are freshly allocated kernel-heap objects.
        let tx_boxed = alloc::boxed::Box::new(tx_layout);
        let rx_boxed = alloc::boxed::Box::new(rx_layout);
        let tx_ptr = alloc::boxed::Box::into_raw(tx_boxed);
        let rx_ptr = alloc::boxed::Box::into_raw(rx_boxed);

        self.tx_descs = unsafe { core::slice::from_raw_parts_mut(tx_ptr, DESC_COUNT) };
        self.rx_descs = unsafe { core::slice::from_raw_parts_mut(rx_ptr, DESC_COUNT) };

        // Get physical addresses via VMM walk.
        let tx_phys = crate::memory::vmm::virt_to_phys(tx_ptr as u64)
            .expect("e1000: tx desc virt_to_phys failed") & !0xFFF;
        let rx_phys = crate::memory::vmm::virt_to_phys(rx_ptr as u64)
            .expect("e1000: rx desc virt_to_phys failed") & !0xFFF;

        // Allocate RX data buffers and fill descriptors.
        self.rx_bufs = Vec::new();
        for i in 0..DESC_COUNT {
            let buf = vec![0u8; FRAME_MAX];
            // Place each buffer on its own page-aligned allocation? Not needed
            // if the NIC can handle non-page-aligned buffers (it can, as long
            // as they're physically contiguous). Our heap allocations may span
            // pages though — for correctness, allocate per-buffer frames.
            //
            // For now: leak one page per RX buffer (reclaimed at reboot).
            let frame = crate::memory::pmm::alloc_frame()
                .expect("e1000: no frame for RX buffer");
            let kvirt = crate::memory::vmm::phys_to_virt(frame);
            // Zero it.
            unsafe {
                ptr::write_bytes(kvirt as *mut u8, 0, FRAME_MAX);
            }
            self.rx_descs[i].buffer_addr = frame & !0xFFF;
            // Store the HHDM pointer for later reading.
            self.rx_bufs.extend_from_slice(&[]);
            let _ = kvirt;
        }

        // Program descriptor ring bases.
        unsafe {
            self.reg_write(REG_TDBAL, (tx_phys & 0xFFFF_FFFF) as u32);
            self.reg_write(REG_TDBAH, (tx_phys >> 32) as u32);
            self.reg_write(REG_TXDLEN, (DESC_COUNT * 16) as u32);
            self.reg_write(REG_TDH, 0);
            self.reg_write(REG_TDT, 0);

            self.reg_write(REG_RDBAL, (rx_phys & 0xFFFF_FFFF) as u32);
            self.reg_write(REG_RDBAH, (rx_phys >> 32) as u32);
            self.reg_write(REG_RXDLEN, (DESC_COUNT * 16) as u32);
            self.reg_write(REG_RDH, 0);
            // Tail = count tells the NIC all RX buffers are available.
            self.reg_write(REG_RDT, DESC_COUNT as u32);
        }

        self.tx_tail = 0;
        self.rx_head = 0;
    }

    /// Enable RX and TX engines.
    fn enable(&self) {
        unsafe {
            // Enable receive: unicast+multicast promisc, broadcast accept, 2048-byte bufs
            self.reg_write(
                REG_RCTL,
                RCTL_EN | RCTL_UPE | RCTL_MPE | RCTL_BAM | RCTL_BSIZE_2048,
            );
            // Enable transmit: pad short packets
            self.reg_write(REG_TCTL, TCTL_EN | TCTL_PSP);
        }
    }

    /// Send one Ethernet frame. Returns bytes sent, or 0 if the ring is full.
    pub fn send_frame(&mut self, data: &[u8]) -> usize {
        let tail = self.tx_tail;
        // Check if the descriptor at tail is free (DD cleared).
        if self.tx_descs[tail].status & TXD_STATUS_DD != 0 || self.tx_descs[tail].buffer_addr == 0 {
            // Free to use
        } else {
            return 0; // ring full
        }

        let len = core::cmp::min(data.len(), FRAME_MAX);

        // We need a physically contiguous buffer. Allocate a page from PMM,
        // copy data into it via HHDM, and point the descriptor there.
        let frame = match crate::memory::pmm::alloc_frame() {
            Some(f) => f & !0xFFF,
            None => return 0,
        };
        let kvirt = crate::memory::vmm::phys_to_virt(frame);
        // SAFETY: freshly allocated frame, zeroed by caller's responsibility.
        unsafe {
            ptr::copy_nonoverlapping(data.as_ptr(), kvirt as *mut u8, len);
        }

        // Fill descriptor.
        self.tx_descs[tail].buffer_addr = frame;
        self.tx_descs[tail].length = len as u16;
        self.tx_descs[tail].cmd = TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS;
        self.tx_descs[tail].status = 0;

        // Kick the NIC: advance TDT.
        self.tx_tail = (tail + 1) % DESC_COUNT;
        unsafe {
            self.reg_write(REG_TDT, self.tx_tail as u32);
        }

        len
    }

    /// Try to receive one frame. Returns `Some(len)` if a frame was received,
    /// with the frame data copied into `dst`. Returns `None` if no frame is
    /// pending.
    pub fn recv_frame(&mut self, dst: &mut [u8]) -> Option<usize> {
        let idx = self.rx_head;
        if self.rx_descs[idx].status & RXD_STATUS_DD == 0 {
            return None; // not done yet
        }

        let len = self.rx_descs[idx].length as usize;
        let len = core::cmp::min(len, dst.len());

        // Copy from the RX buffer (mapped through HHDM).
        let phys = self.rx_descs[idx].buffer_addr & !0xFFF;
        let kvirt = crate::memory::vmm::phys_to_virt(phys);
        // SAFETY: RX buffer was allocated from PMM and mapped through HHDM.
        unsafe {
            ptr::copy_nonoverlapping(kvirt as *const u8, dst.as_mut_ptr(), len);
        }

        // Reset descriptor for reuse.
        self.rx_descs[idx].status = 0;
        self.rx_descs[idx].length = 0;

        // Advance head and tell the NIC the descriptor is free again.
        self.rx_head = (idx + 1) % DESC_COUNT;
        unsafe {
            self.reg_write(REG_RDT, self.rx_head as u32);
        }

        Some(len)
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }
}

// ─── Global device handle ────────────────────────────────────────────────────

static E1000_DEV: Spinlock<Option<Arc<Spinlock<E1000>>>> = Spinlock::new(None);

/// Find an e1000 device on PCI bus, initialise it, and return the MAC.
///
/// `pci_devices` comes from `drivers::pci::enumerate()`. Returns Err when no
/// matching device exists or initialisation fails.
/// Placeholder — needs proper PCI device discovery integration.
/// See drivers/pci.rs for the enumeration API.
pub fn attach() -> Result<[u8; 6], ()> {
    Err(()) // TODO: implement in next session
}

/// Send a raw Ethernet frame through the e1000.
pub fn send(data: &[u8]) -> usize {
    let guard = E1000_DEV.lock();
    match guard.as_ref() {
        Some(arc) => arc.lock().send_frame(data),
        None => 0,
    }
}

/// Receive a raw Ethernet frame into `dst`. Returns bytes copied, or None.
pub fn recv(dst: &mut [u8]) -> Option<usize> {
    let guard = E1000_DEV.lock();
    match guard.as_ref() {
        Some(arc) => arc.lock().recv_frame(dst),
        None => None,
    }
}
