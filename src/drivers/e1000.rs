//! Intel 8254x-family (e1000) NIC driver — QEMU `-device e1000`.
//!
//! This is the kernel's own NIC driver: it replaces `virtio-net` entirely and
//! needs no external crate. It works on real Intel PRO/1000 hardware, QEMU,
//! VirtualBox and VMware. The device is driven in POLLING mode from the network
//! thread (`net::poll`) — no IRQ handler is registered, which keeps the whole
//! stack out of interrupt context (the locking discipline the rest of the
//! networking code relies on: every entry point runs in thread context).
//!
//! ## Hardware model
//!
//! * BAR 0 (MMIO, 128 KiB) holds the register file. Every access is a volatile
//!   u32 read/write at a fixed offset.
//! * The MAC address lives in the attached EEPROM; it is read word-by-word
//!   through the EERD register.
//! * TX and RX are descriptor rings in physically contiguous memory:
//!   * legacy TX descriptor: 16 bytes {addr, len, cso, cmd, status, css, special}
//!   * legacy RX descriptor: same 16-byte shape {addr, len, checksum, status,
//!     errors, special}
//!
//!   The NIC DMAs frames to/from per-descriptor data buffers that this driver
//!   also allocates from physically contiguous PMM frames.
//! * Polling RX: a received descriptor has STATUS.DD set; after consuming we
//!   clear DD and return the buffer by advancing RDT.
//! * Polling TX: CMD.RS asks the NIC to write STATUS.DD when the frame left;
//!   a descriptor may be reused only once its DD bit is observed.
//!
//! ## Memory discipline
//!
//! All DMA memory comes from `pmm::alloc_frames_contiguous` and is accessed
//! through the HHDM window (`vmm::phys_to_virt`). Descriptor rings and both
//! buffer pools are allocated once at attach time and never freed while the
//! kernel runs (the device owns them for its lifetime). No heap allocation
//! happens on the send/receive paths.

use crate::drivers::pci::{PciDevice, VENDOR_INTEL};
use crate::sync::spinlock::Spinlock;
use crate::{info, warn};
use core::ptr;

// ─── PCI identification ──────────────────────────────────────────────────────

/// Intel 82540EM (QEMU `-device e1000`, the default).
pub const DEVICE_ID_82540EM: u16 = 0x100E;
/// Intel 82545EM (QEMU `-device e1000-82545em`).
pub const DEVICE_ID_82545EM: u16 = 0x100F;

// ─── MMIO register offsets ───────────────────────────────────────────────────

const REG_CTRL: usize = 0x0000;
const REG_STATUS: usize = 0x0008;
const REG_EERD: usize = 0x0014; // EEPROM read
const REG_IMC: usize = 0x00D8; // interrupt mask clear (write 1s = mask)
const REG_ICR: usize = 0x00C0; // interrupt cause (write 1 to clear)
const REG_RCTL: usize = 0x0100; // receive control
const REG_TCTL: usize = 0x0400; // transmit control
const REG_RDBAL: usize = 0x2800; // RX desc base addr low
const REG_RDBAH: usize = 0x2804; // RX desc base addr high
const REG_RDLEN: usize = 0x2808; // RX desc ring length (bytes)
const REG_RDH: usize = 0x2810; // RX desc head (NIC-owned write pointer)
const REG_RDT: usize = 0x2818; // RX desc tail (driver-owned return pointer)
const REG_TDBAL: usize = 0x3800; // TX desc base addr low
const REG_TDBAH: usize = 0x3804; // TX desc base addr high
const REG_TDLEN: usize = 0x3808; // TX desc ring length (bytes)
const REG_TDH: usize = 0x3810; // TX desc head (NIC completion pointer)
const REG_TDT: usize = 0x3818; // TX desc tail (driver enqueue pointer)

// CTRL bits
const CTRL_RST: u32 = 1 << 26; // full device reset
const CTRL_SLU: u32 = 1 << 6; // set link up

// STATUS bits
const STATUS_LU: u32 = 1 << 1; // link up

// RCTL bits
const RCTL_EN: u32 = 1 << 1;
const RCTL_SBP: u32 = 1 << 2; // store bad packets
const RCTL_UPE: u32 = 1 << 3; // unicast promiscuous
const RCTL_MPE: u32 = 1 << 4; // multicast promiscuous
const RCTL_BAM: u32 = 1 << 15; // broadcast accept mode
const RCTL_BSIZE_2048: u32 = 0 << 16; // BSIZE=00 & BSEX=0 -> 2048-byte buffers
const RCTL_SECRC: u32 = 1 << 26; // strip ethernet CRC

// TCTL bits
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3; // pad short packets
const TCTL_CT_SHIFT: u32 = 8; // collision threshold (bits 15:8), 0x0F typical
const TCTL_COLD_SHIFT: u32 = 20; // collision distance (bits 27:20), 0x40 = FD

// TX descriptor command bits
const TXD_CMD_EOP: u8 = 1 << 0; // end of packet
const TXD_CMD_IFCS: u8 = 1 << 1; // insert FCS/CRC
const TXD_CMD_RS: u8 = 1 << 3; // report status

// Descriptor status bits
const RXD_STATUS_DD: u8 = 1 << 0; // descriptor done
const TXD_STATUS_DD: u8 = 1 << 0; // descriptor done

/// Descriptors per ring. Both rings share this depth.
const DESC_COUNT: usize = 64;
/// Per-descriptor data buffer size. Must be >= max frame (1514 + VLAN slack);
/// 2048 matches RCTL BSIZE=2048 so one RX descriptor always holds one frame.
const BUF_SIZE: usize = 2048;
/// Ring bytes (TDLEN/RDLEN take BYTES).
const RING_BYTES: usize = DESC_COUNT * core::mem::size_of::<Desc>();
/// Frames per ring allocation (both 16-byte descriptor rings fit in one page,
/// but request them separately for clarity).
const RING_FRAMES: usize = RING_BYTES.div_ceil(4096);
/// Buffer-pool pages: 64 buffers x 2048 bytes = 128 KiB = 32 pages.
const POOL_FRAMES: usize = (DESC_COUNT * BUF_SIZE).div_ceil(4096);
/// How long to wait for the CTRL.RST self-clear, in poll iterations.
const RESET_SPINS: u32 = 1_000_000;
/// How long to wait for the link (STATUS.LU), in poll iterations (~a few s).
const LINK_SPINS: u32 = 5_000_000;

/// Set after the first oversized-TX-frame warning, so a caller pushing huge
/// buffers in a loop does not flood the console.
static OVERSIZED_WARNED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

// ─── Descriptors ─────────────────────────────────────────────────────────────

/// Legacy transmit/receive descriptor (16 bytes). The trailing halfwords mean
/// different things per direction:
///   TX: [length][cso|cmd][status|css][special]
///   RX: [length][checksum ][status|errs][special]
/// i.e. the DD status byte sits at offset 12 in BOTH directions.
#[repr(C)]
#[derive(Clone, Copy)]
struct Desc {
    buffer_addr: u64,
    length: u16,
    /// RX: full 16-bit checksum. TX: low byte cso, high byte cmd.
    meta0: u16,
    /// Both directions: low byte = status (DD lives here).
    /// TX high byte: css. RX high byte: errors.
    meta1: u16,
    special: u16,
}
const _: () = assert!(core::mem::size_of::<Desc>() == 16);

impl Desc {
    const fn zeroed() -> Self {
        Desc {
            buffer_addr: 0,
            length: 0,
            meta0: 0,
            meta1: 0,
            special: 0,
        }
    }

    fn set_tx_cmd(&mut self, cmd: u8) {
        self.meta0 = (self.meta0 & 0x00FF) | ((cmd as u16) << 8);
    }

    fn status_byte(&self) -> u8 {
        self.meta1 as u8
    }

    fn clear_status_dd(&mut self) {
        self.meta1 &= !(TXD_STATUS_DD as u16);
    }
}

// ─── Device state ────────────────────────────────────────────────────────────

pub struct E1000 {
    mmio_base: usize, // HHDM virtual address of BAR 0
    mac: [u8; 6],

    /// Physically contiguous descriptor rings + buffer pools (PMM-owned).
    tx_ring_phys: u64,
    tx_ring: &'static mut [Desc],
    rx_ring_phys: u64,
    rx_ring: &'static mut [Desc],
    tx_pool_phys: u64,
    tx_pool_virt: usize,
    rx_pool_phys: u64,
    rx_pool_virt: usize,

    tx_head: usize, // oldest descriptor possibly not yet completed (DD)
    tx_tail: usize, // next descriptor to fill
    tx_free: usize, // descriptors known free
    rx_head: usize, // next descriptor to check for a completed frame
}

// SAFETY: all device state is only ever reached through the module-global
// Spinlock below (single holder); the raw pointers point at PMM-owned DMA
// memory that stays mapped for the lifetime of the kernel.
unsafe impl Send for E1000 {}
unsafe impl Sync for E1000 {}

fn pool_buf_virt(pool_virt: usize, i: usize) -> *mut u8 {
    unsafe { (pool_virt as *mut u8).add(i * BUF_SIZE) }
}

fn pool_buf_phys(pool_phys: u64, i: usize) -> u64 {
    pool_phys + (i * BUF_SIZE) as u64
}

impl E1000 {
    // ── MMIO access ──

    unsafe fn reg_read(&self, offset: usize) -> u32 {
        ptr::read_volatile((self.mmio_base + offset) as *const u32)
    }

    unsafe fn reg_write(&self, offset: usize, value: u32) {
        ptr::write_volatile((self.mmio_base + offset) as *mut u32, value);
    }

    /// Read one EEPROM word (bits 31:16 of EERD after DONE).
    fn eeprom_read_word(&self, word_addr: u8) -> Option<u16> {
        unsafe {
            self.reg_write(REG_EERD, ((word_addr as u32) << 8) | 1);
            let mut spins = 0;
            while spins < 100_000 {
                let v = self.reg_read(REG_EERD);
                if v & (1 << 4) != 0 {
                    // done: data in bits 31:16
                    return Some((v >> 16) as u16);
                }
                spins += 1;
            }
        }
        None
    }

    /// Read the 48-bit MAC from EEPROM words 0..=2. Falls back to a fixed local
    /// address when the EEPROM is unreadable (should not happen under QEMU).
    fn read_mac(&self) -> [u8; 6] {
        let mut mac = [0u8; 6];
        let mut ok = true;
        for i in 0u8..3 {
            match self.eeprom_read_word(i) {
                Some(w) => {
                    mac[(i * 2) as usize] = (w & 0xFF) as u8;
                    mac[(i * 2 + 1) as usize] = (w >> 8) as u8;
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok || mac == [0; 6] {
            warn!("e1000: EEPROM MAC unreadable, using fallback 52:54:00:12:34:56");
            return [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        }
        mac
    }

    /// Reset the device, mask interrupts (polling mode), bring the link up.
    fn reset_and_link_up(&mut self) -> Result<(), ()> {
        unsafe {
            // Mask ALL interrupts: the driver is polled from the net thread.
            self.reg_write(REG_IMC, 0xFFFF_FFFF);
            // Clear any pending causes.
            self.reg_write(REG_ICR, 0xFFFF_FFFF);

            // Full reset. CTRL.RST self-clears when the reset completes.
            self.reg_write(REG_CTRL, CTRL_RST);
            let mut spins = 0;
            while spins < RESET_SPINS {
                if self.reg_read(REG_CTRL) & CTRL_RST == 0 {
                    break;
                }
                spins += 1;
            }
            if spins >= RESET_SPINS {
                return Err(());
            }

            // Re-mask after reset (reset restores defaults).
            self.reg_write(REG_IMC, 0xFFFF_FFFF);

            // Set Link Up. Leave speed/duplex to auto-neg.
            let ctrl = self.reg_read(REG_CTRL);
            self.reg_write(REG_CTRL, ctrl | CTRL_SLU);
        }

        // Wait for link (QEMU links immediately; real PHYs need auto-neg time).
        let mut spins = 0;
        while spins < LINK_SPINS {
            if unsafe { self.reg_read(REG_STATUS) } & STATUS_LU != 0 {
                return Ok(());
            }
            spins += 1;
        }
        // Link not up yet: not fatal for polling drivers, but report honestly.
        warn!("e1000: link did not come up within the wait budget");
        Ok(())
    }

    /// Allocate and program the TX/RX descriptor rings and buffer pools.
    fn init_rings(&mut self) -> Result<(), ()> {
        // Descriptor rings: physically contiguous, page-aligned via PMM.
        let tx_ring_phys = crate::memory::pmm::alloc_frames_contiguous(RING_FRAMES).ok_or(())?;
        let rx_ring_phys = crate::memory::pmm::alloc_frames_contiguous(RING_FRAMES).ok_or(())?;

        let tx_ring_virt = crate::memory::vmm::phys_to_virt(tx_ring_phys) as usize;
        let rx_ring_virt = crate::memory::vmm::phys_to_virt(rx_ring_phys) as usize;

        // Zero both rings (all descriptors unused).
        unsafe {
            ptr::write_bytes(tx_ring_virt as *mut Desc, 0, DESC_COUNT);
            ptr::write_bytes(rx_ring_virt as *mut Desc, 0, DESC_COUNT);
        }

        // Buffer pools: one contiguous run each, split into DESC_COUNT slots.
        let tx_pool_phys = crate::memory::pmm::alloc_frames_contiguous(POOL_FRAMES).ok_or(())?;
        let rx_pool_phys = crate::memory::pmm::alloc_frames_contiguous(POOL_FRAMES).ok_or(())?;
        let tx_pool_virt = crate::memory::vmm::phys_to_virt(tx_pool_phys) as usize;
        let rx_pool_virt = crate::memory::vmm::phys_to_virt(rx_pool_phys) as usize;

        // Arm every RX descriptor with its own buffer.
        let rx_ring =
            unsafe { core::slice::from_raw_parts_mut(rx_ring_virt as *mut Desc, DESC_COUNT) };
        {
            let mut arr = [Desc::zeroed(); DESC_COUNT];
            for (i, d) in arr.iter_mut().enumerate() {
                d.buffer_addr = pool_buf_phys(rx_pool_phys, i);
                d.length = BUF_SIZE as u16;
            }
            rx_ring.copy_from_slice(&arr);
        }

        let tx_ring_slice =
            unsafe { core::slice::from_raw_parts_mut(tx_ring_virt as *mut Desc, DESC_COUNT) };

        self.tx_ring_phys = tx_ring_phys;
        self.tx_ring = tx_ring_slice;
        self.rx_ring_phys = rx_ring_phys;
        self.rx_ring = rx_ring;
        self.tx_pool_phys = tx_pool_phys;
        self.tx_pool_virt = tx_pool_virt;
        self.rx_pool_phys = rx_pool_phys;
        self.rx_pool_virt = rx_pool_virt;

        // Program the rings into the device.
        unsafe {
            self.reg_write(REG_TDBAL, (tx_ring_phys & 0xFFFF_FFFF) as u32);
            self.reg_write(REG_TDBAH, (tx_ring_phys >> 32) as u32);
            self.reg_write(REG_TDLEN, RING_BYTES as u32);
            self.reg_write(REG_TDH, 0);
            self.reg_write(REG_TDT, 0);

            self.reg_write(REG_RDBAL, (rx_ring_phys & 0xFFFF_FFFF) as u32);
            self.reg_write(REG_RDBAH, (rx_ring_phys >> 32) as u32);
            self.reg_write(REG_RDLEN, RING_BYTES as u32);
            self.reg_write(REG_RDH, 0);
            // Tail = count tells the NIC every descriptor is available.
            self.reg_write(REG_RDT, DESC_COUNT as u32);
        }

        self.tx_head = 0;
        self.tx_tail = 0;
        self.tx_free = DESC_COUNT;
        self.rx_head = 0;
        Ok(())
    }

    /// Enable the RX and TX engines. Called once after the rings are programmed.
    fn enable(&mut self) {
        unsafe {
            // RX: enable, accept broadcast, 2048-byte buffers, strip CRC.
            self.reg_write(
                REG_RCTL,
                RCTL_EN | RCTL_SBP | RCTL_UPE | RCTL_MPE | RCTL_BAM | RCTL_BSIZE_2048 | RCTL_SECRC,
            );
            // TX: enable, pad short packets, collision threshold/distance for FD.
            self.reg_write(
                REG_TCTL,
                TCTL_EN | TCTL_PSP | (0x0F << TCTL_CT_SHIFT) | (0x40 << TCTL_COLD_SHIFT),
            );
        }
    }

    /// Reclaim completed TX descriptors (STATUS.DD), returning their buffer
    /// slots to the pool. Called opportunistically from [`send_frame`].
    fn reclaim_tx(&mut self) {
        while self.tx_free < DESC_COUNT {
            let done = self.tx_ring[self.tx_head].status_byte() & TXD_STATUS_DD != 0;
            if !done {
                break;
            }
            self.tx_ring[self.tx_head].clear_status_dd();
            self.tx_head = (self.tx_head + 1) % DESC_COUNT;
            self.tx_free += 1;
        }
    }

    /// Send one Ethernet frame. Returns the number of bytes queued to the NIC
    /// (frame length on success), or 0 when the TX ring is momentarily full or
    /// the frame is too large for a single TX buffer (dropped, not truncated).
    pub fn send_frame(&mut self, data: &[u8]) -> usize {
        self.reclaim_tx();
        if self.tx_free == 0 {
            return 0;
        }
        let max = BUF_SIZE - 4;
        if data.len() > max {
            // A frame larger than one DMA buffer cannot be split across
            // descriptors here; silently truncating it sent a corrupt packet
            // whose length lied to the receiver. Normal Ethernet frames are
            // at most 1514 bytes, so drop this one and say so (once).
            if !OVERSIZED_WARNED.swap(true, core::sync::atomic::Ordering::AcqRel) {
                warn!(
                    "e1000: dropped {}-byte TX frame (max {}); oversized frames are not splittable",
                    data.len(),
                    max
                );
            }
            return 0;
        }
        let len = data.len();

        // Copy the frame into this slot's pre-allocated DMA buffer.
        let idx = self.tx_tail;
        unsafe {
            let dst = pool_buf_virt(self.tx_pool_virt, idx);
            ptr::copy_nonoverlapping(data.as_ptr(), dst, len);
        }

        // Fill the descriptor and kick the tail.
        let d = &mut self.tx_ring[idx];
        d.buffer_addr = pool_buf_phys(self.tx_pool_phys, idx);
        d.length = len as u16;
        d.set_tx_cmd(TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS);
        d.clear_status_dd();

        self.tx_tail = (idx + 1) % DESC_COUNT;
        self.tx_free -= 1;
        unsafe {
            self.reg_write(REG_TDT, self.tx_tail as u32);
        }
        len
    }

    /// Try to receive one frame into `dst`. Returns `Some(n)` with the copied
    /// length, or `None` when no completed descriptor is pending.
    pub fn recv_frame(&mut self, dst: &mut [u8]) -> Option<usize> {
        let idx = self.rx_head;
        if self.rx_ring[idx].status_byte() & RXD_STATUS_DD == 0 {
            return None;
        }
        let d = &mut self.rx_ring[idx];
        let len = (d.length as usize).min(BUF_SIZE);
        let copy_len = len.min(dst.len());

        // Frame bytes live in the slot's DMA buffer (HHDM-mapped).
        unsafe {
            let src = pool_buf_virt(self.rx_pool_virt, idx);
            ptr::copy_nonoverlapping(src as *const u8, dst.as_mut_ptr(), copy_len);
        }

        // Return the buffer to the NIC: clear DD/length, advance head, publish
        // the new tail.
        d.length = BUF_SIZE as u16;
        d.clear_status_dd();

        self.rx_head = (idx + 1) % DESC_COUNT;
        unsafe {
            self.reg_write(REG_RDT, idx as u32);
        }
        Some(copy_len)
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    pub fn link_up(&self) -> bool {
        unsafe { self.reg_read(REG_STATUS) & STATUS_LU != 0 }
    }
}

// ─── Global device handle ────────────────────────────────────────────────────

static E1000_DEV: Spinlock<Option<E1000>> = Spinlock::new(None);

/// True when a PCI function is an e1000 controller this driver supports.
fn is_e1000(dev: &PciDevice) -> bool {
    dev.vendor_id == VENDOR_INTEL
        && (dev.device_id == DEVICE_ID_82540EM || dev.device_id == DEVICE_ID_82545EM)
}

/// Read BAR 0 (offset 0x10) and return its physical MMIO base. Only memory-
/// mapped 32-bit BARs are supported (that is what QEMU programs).
fn bar0_mmio_base(addr: crate::drivers::pci::PciAddress) -> Result<u64, ()> {
    let raw = crate::drivers::pci::config_read_u32(addr, 0x10);
    if raw & 0x1 != 0 {
        // I/O-space BAR: unusable for MMIO.
        return Err(());
    }
    Ok((raw & 0xFFFF_FFF0) as u64)
}

/// Find an e1000 device among `devices` (from `pci::enumerate()`), map its
/// MMIO registers, initialise the rings, and park the device handle in the
/// module-global slot. Returns the NIC's MAC address.
///
/// Errors (`Err(())`) mean "no usable e1000 present / init failed"; callers log
/// and continue booting without networking (same policy as the old virtio-net
/// attach path, R17.3).
pub fn attach(devices: &[PciDevice]) -> Result<[u8; 6], ()> {
    let already = E1000_DEV.lock().is_some();
    if already {
        return Err(());
    }

    let dev = match devices.iter().find(|d| is_e1000(d)) {
        Some(d) => d,
        None => return Err(()),
    };
    let addr = dev.address;
    info!(
        "e1000: found device {:02x}:{:02x}.{} (id {:#06x})",
        addr.bus, addr.device, addr.function, dev.device_id
    );

    // Enable memory-space decoding + bus mastering so MMIO and DMA work.
    crate::drivers::pci::enable_bus_master(addr);

    let bar0_phys = bar0_mmio_base(addr)?;
    info!("e1000: BAR0 mmio phys {:#x}", bar0_phys);

    let mmio_virt = crate::memory::vmm::map_mmio(bar0_phys, 0x2_0000).map_err(|e| {
        warn!("e1000: map_mmio failed: {:?}", e);
    })? as usize;

    // Build the driver object. Ring/pool allocation happens in init_rings; the
    // initial ring slices are empty over an aligned dangling pointer (never
    // dereferenced before init_rings replaces them).
    let dangling = core::ptr::NonNull::dangling().as_ptr();
    let mut nic = E1000 {
        mmio_base: mmio_virt,
        mac: [0; 6],
        tx_ring_phys: 0,
        tx_ring: unsafe { core::slice::from_raw_parts_mut(dangling, 0) },
        rx_ring_phys: 0,
        rx_ring: unsafe { core::slice::from_raw_parts_mut(dangling, 0) },
        tx_pool_phys: 0,
        tx_pool_virt: 0,
        rx_pool_phys: 0,
        rx_pool_virt: 0,
        tx_head: 0,
        tx_tail: 0,
        tx_free: DESC_COUNT,
        rx_head: 0,
    };

    nic.reset_and_link_up()?;
    nic.init_rings()?;
    nic.mac = nic.read_mac();
    nic.enable();

    let mac = nic.mac_address();
    info!(
        "e1000: attached, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, link={}",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5],
        nic.link_up()
    );

    *E1000_DEV.lock() = Some(nic);
    Ok(mac)
}

/// Send one raw Ethernet frame through the e1000. Returns bytes enqueued
/// (0 = TX ring full or no device).
pub fn send(data: &[u8]) -> usize {
    let mut guard = E1000_DEV.lock();
    match guard.as_mut() {
        Some(nic) => nic.send_frame(data),
        None => 0,
    }
}

/// Receive one raw Ethernet frame into `dst`. Returns `Some(bytes_copied)`
/// or `None` when nothing is pending (or no device is attached).
pub fn recv(dst: &mut [u8]) -> Option<usize> {
    let mut guard = E1000_DEV.lock();
    guard.as_mut()?.recv_frame(dst)
}

/// MAC of the attached device, if any.
#[allow(dead_code)]
pub fn mac_address() -> Option<[u8; 6]> {
    let guard = E1000_DEV.lock();
    guard.as_ref().map(|n| n.mac_address())
}

/// Is a device attached and its link up?
#[allow(dead_code)]
pub fn is_attached() -> bool {
    E1000_DEV.lock().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desc_layout_is_16_bytes() {
        assert_eq!(core::mem::size_of::<Desc>(), 16);
    }
}
