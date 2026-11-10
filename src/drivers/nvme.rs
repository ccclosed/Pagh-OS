//! NVMe host-controller driver over PCIe — polled, no IRQs.
//!
//! Bring-up: PCI probe (class 01h/08h) → map BAR0 → disable controller →
//! program AQA/ASQ/ACQ → enable → Identify Controller → Create I/O CQ + SQ →
//! Identify active namespaces → ready.
//!
//! Block I/O is **PRP-list based and batched**:
//!
//! * A command may transfer up to `IO_BUFFER_PAGES * 4 KiB` (capped by the
//!   controller's Identify-Controller MDTS). PRP1 names the first page, PRP2 the
//!   second page for a two-page transfer, and otherwise points at a 4 KiB PRP
//!   list whose entries are the second, third, … pages. The pure arithmetic
//!   lives in [`crate::drivers::nvme_prp`] so it is host property-tested.
//! * Up to [`IO_QUEUE_DEPTH`] commands are submitted before the first completion
//!   is reaped. Each in-flight command owns a physically-contiguous data slot
//!   plus its own PRP-list page, so submissions do not alias. Completions are
//!   matched to commands by CID, not by arrival order, and the SQ doorbell is
//!   rung once per batch.
//!
//! Completion queues are POLLED via the phase bit in the status word;
//! interrupts stay masked. All register accesses are volatile u32s.
//!
//! Writes whose byte range is not LBA-aligned take a read-modify-write path
//! (read the containing LBA, patch the caller's bytes, write it back) instead
//! of zeroing (and therefore corrupting) the neighbouring sectors.
//!
//! Queue sizes handed to the controller are *zero-based* (`QSIZE = entries - 1`)
//! and clamped to CAP.MQES; the host rings use the same entry count.

use crate::drivers::nvme_prp::{
    build_prp, plan_chunk, queue_entries, CqRing, SqRing, PAGE_SIZE, PRP_LIST_ENTRIES,
};
use crate::drivers::pci::{self, PciDevice};
use crate::drivers::BlockDevice;
use crate::memory::pmm;
use crate::sync::spinlock::Spinlock;
use crate::{info, warn};
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

// ─── Register offsets (BAR0) ─────────────────────────────────────────────────

const CAP_CAP_LO: usize = 0x00;
const CAP_CAP_HI: usize = 0x04;
const CC: usize = 0x14;
const CSTS: usize = 0x1C;
const AQA: usize = 0x24;
const ASQ: usize = 0x28;
const ASQ_HI: usize = 0x2C;
const ACQ: usize = 0x30;
const ACQ_HI: usize = 0x34;

const CC_EN: u32 = 1 << 0;
const CC_CSS_NVM: u32 = 0 << 1;
const CC_MPS_4K: u32 = 0 << 4;
const CC_IOSQES_64: u32 = 6 << 16;
const CC_IOCQES_16: u32 = 4 << 20;
const CSTS_RDY: u32 = 1 << 0;
const CSTS_CFS: u32 = 1 << 1;

/// Desired admin / I/O queue sizes (clamped to CAP.MQES at attach).
const ADMIN_Q_ENTRIES: u32 = 32;
const IO_Q_ENTRIES: u32 = 64;

/// Number of commands kept in flight between two completion-drain passes.
/// This is the queue depth the block API actually uses, not just a ring size.
const IO_QUEUE_DEPTH: usize = 4;

/// Largest data slot in 4 KiB pages (one command's maximum transfer).
/// A physically-contiguous slot is allocated per in-flight command, so this
/// bounds both the per-command transfer and the driver's DMA footprint
/// (4 slots × 16 pages = 256 KiB).
const IO_BUFFER_PAGES: usize = 16;
const _: () = assert!(IO_BUFFER_PAGES.is_power_of_two());

// Admin opcodes.
const OPC_CREATE_IOSQ: u8 = 0x01;
const OPC_CREATE_IOCQ: u8 = 0x05;
const OPC_IDENTIFY: u8 = 0x06;
// NVM opcodes (NVMe Base Spec, Figure 100): Flush=0x00, Write=0x01,
// Read=0x02. 0x08 is Write Zeroes, whose old value here as "Flush" would have
// silently zeroed data instead of making the WAL durable.
const OPC_NVM_FLUSH: u8 = 0x00;
const OPC_NVM_WRITE: u8 = 0x01;
const OPC_NVM_READ: u8 = 0x02;

const IDENTIFY_NS: u32 = 0;
const IDENTIFY_CTRL: u32 = 1;
const IDENTIFY_NS_ACTIVE_LIST: u32 = 2;

/// Offset of the MDTS byte in the Identify Controller data structure
/// (NVMe Base Spec, Figure 275).
const ID_CTRL_MDTS_OFF: usize = 77;

/// Spin bound for completion polling (~seconds even under TCG).
const COMPLETION_SPINS: u64 = 500_000_000;

#[repr(C)]
#[derive(Clone, Copy)]
struct SqEntry {
    dw: [u32; 16],
}
const _: () = assert!(core::mem::size_of::<SqEntry>() == 64);

#[repr(C)]
#[derive(Clone, Copy)]
struct CqEntry {
    dw: [u32; 4],
}
const _: () = assert!(core::mem::size_of::<CqEntry>() == 16);

// ─── DMA helpers ─────────────────────────────────────────────────────────────

/// One zeroed, page-aligned DMA page (queue, Identify buffer, PRP list).
struct DmaFrame {
    phys: u64,
    virt: usize,
}

impl DmaFrame {
    fn new_zeroed() -> Result<Self, ()> {
        let phys = match pmm::alloc_frame() {
            Some(p) => p,
            None => {
                warn!("nvme dma: alloc_frame returned None");
                return Err(());
            }
        };
        let virt = crate::memory::vmm::phys_to_virt(phys) as usize;
        // SAFETY: the PMM just handed us this exclusive 4 KiB frame, which is
        // mapped in the HHDM, so writing the whole frame is valid.
        unsafe { ptr::write_bytes(virt as *mut u8, 0, PAGE_SIZE) };
        Ok(DmaFrame {
            phys: phys & !0xFFF,
            virt,
        })
    }

    fn phys(&self) -> u64 {
        self.phys
    }
}

/// A physically-contiguous multi-page DMA buffer.
///
/// The PRP-list path relies on the pages being contiguous: page `i` is simply
/// `phys + i * 4 KiB`, which is how the list entries are built.
struct DmaBuffer {
    phys: u64,
    virt: usize,
    pages: usize,
}

impl DmaBuffer {
    fn new_zeroed(pages: usize) -> Result<Self, ()> {
        if pages == 0 {
            return Err(());
        }
        let phys = match pmm::alloc_frames_contiguous(pages) {
            Some(p) => p,
            None => {
                warn!("nvme dma: no {}-frame contiguous run", pages);
                return Err(());
            }
        };
        let virt = crate::memory::vmm::phys_to_virt(phys) as usize;
        // SAFETY: `alloc_frames_contiguous` returned `pages` consecutive frames
        // that are exclusively ours and mapped by the HHDM.
        unsafe { ptr::write_bytes(virt as *mut u8, 0, pages * PAGE_SIZE) };
        info!(
            "nvme dma: {}-page buffer {:#x} -> virt {:#x}",
            pages, phys, virt
        );
        Ok(DmaBuffer { phys, virt, pages })
    }

    fn base_phys(&self) -> u64 {
        self.phys
    }
}

/// Per-in-flight-command state: a data slot and its PRP-list page.
struct IoSlot {
    data: DmaBuffer,
    prp_list: DmaFrame,
}

impl IoSlot {
    fn new(pages: usize) -> Result<Self, ()> {
        Ok(Self {
            data: DmaBuffer::new_zeroed(pages)?,
            prp_list: DmaFrame::new_zeroed()?,
        })
    }
}

/// One Read/Write submission for [`NvmeCtrl::submit_io_batch`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IoOp {
    write: bool,
    lba: u64,
    lbas: u32,
    /// Offset of the caller's data inside the command's first LBA.
    within: usize,
    /// Caller bytes carried by this command.
    data_len: usize,
}

impl IoOp {
    fn command_bytes(self, lba_size: u64) -> usize {
        self.lbas as usize * lba_size as usize
    }
}

// ─── Device state ────────────────────────────────────────────────────────────

pub struct NvmeCtrl {
    mmio: usize,
    dstrd: u32,

    admin_sq: DmaFrame,
    admin_cq: DmaFrame,
    io_sq: DmaFrame,
    io_cq: DmaFrame,
    id_page: DmaFrame,
    io_slots: Vec<IoSlot>,

    admin_sq_ring: SqRing,
    admin_cq_ring: CqRing,
    io_sq_ring: SqRing,
    io_cq_ring: CqRing,

    cid: u16,
    /// Set when the completion protocol is broken (timeout/unknown/duplicate
    /// CID). Further I/O is refused rather than risking a mis-assigned
    /// completion writing into the wrong caller buffer.
    io_failed: bool,

    nsid: u32,
    lbads: u8,
    /// Largest single-command transfer the slot buffers and MDTS allow.
    io_max_bytes: usize,
}

unsafe impl Send for NvmeCtrl {}
unsafe impl Sync for NvmeCtrl {}

impl NvmeCtrl {
    #[inline]
    fn reg_read(&self, off: usize) -> u32 {
        // SAFETY: `mmio` is the mapped BAR0 window; every caller passes an
        // offset inside the mapped 0x4000 bytes.
        unsafe { ptr::read_volatile((self.mmio + off) as *const u32) }
    }
    #[inline]
    fn reg_write(&self, off: usize, v: u32) {
        // SAFETY: same mapped BAR0 window as `reg_read`.
        unsafe { ptr::write_volatile((self.mmio + off) as *mut u32, v) }
    }

    #[inline]
    fn sq_doorbell(&self, qid: u32, value: u16) {
        self.reg_write(
            0x1000 + (2 * qid) as usize * (4 << self.dstrd),
            value as u32,
        );
    }
    #[inline]
    fn cq_doorbell(&self, qid: u32, value: u16) {
        self.reg_write(
            0x1000 + ((2 * qid + 1) as usize) * (4 << self.dstrd),
            value as u32,
        );
    }

    /// Make all preceding memory writes (SQ entries, PRP lists, copied data)
    /// visible before the MMIO doorbell write that starts DMA.
    #[inline]
    fn ring_sq(&self, qid: u32, tail: u32) {
        core::sync::atomic::fence(Ordering::SeqCst);
        self.sq_doorbell(qid, tail as u16);
    }

    #[inline]
    fn next_cid(&mut self) -> u16 {
        self.cid = self.cid.wrapping_add(1);
        self.cid
    }

    /// Write one 64-byte submission entry. The doorbell is rung separately so a
    /// batch of commands costs one MMIO write, not one per command.
    #[allow(clippy::too_many_arguments)]
    fn write_sq_entry(
        &self,
        sq_virt: usize,
        slot: u32,
        cid: u16,
        opcode: u8,
        nsid: u32,
        prp1: u64,
        prp2: u64,
        cdw10: u32,
        cdw11: u32,
        cdw12: u32,
    ) {
        let e = sq_virt + slot as usize * core::mem::size_of::<SqEntry>();
        // SAFETY: `slot` is the ring tail (< queue entries) and the SQ frame
        // holds `entries` 64-byte slots; all writes are inside that frame.
        unsafe {
            let p = e as *mut u32;
            // Spec offsets: DW0 cmd @0, NSID(DW1) @4, MPTR(DW4-5) @16,
            // PRP1(DW6-7) @24, PRP2(DW8-9) @32, CDW10(DW10) @40.
            ptr::write_volatile(p.add(0), (cid as u32) << 16 | opcode as u32);
            ptr::write_volatile(p.add(1), nsid); // DW1 = NSID
            for k in 2..6 {
                ptr::write_volatile(p.add(k), 0); // DW2..DW5 reserved/MPTR
            }
            ptr::write_volatile(p.add(6), prp1 as u32);
            ptr::write_volatile(p.add(7), (prp1 >> 32) as u32);
            ptr::write_volatile(p.add(8), prp2 as u32);
            ptr::write_volatile(p.add(9), (prp2 >> 32) as u32);
            ptr::write_volatile(p.add(10), cdw10);
            ptr::write_volatile(p.add(11), cdw11);
            ptr::write_volatile(p.add(12), cdw12);
            ptr::write_volatile(p.add(13), 0);
            ptr::write_volatile(p.add(14), 0);
            ptr::write_volatile(p.add(15), 0);
        }
    }

    /// Poll the admin completion queue for `cid` and return DW0.
    fn reap_admin(&mut self, cid: u16) -> Result<u32, ()> {
        let mut spins = 0u64;
        loop {
            let head = self.admin_cq_ring.head();
            let e = self.admin_cq.virt + head as usize * core::mem::size_of::<CqEntry>();
            // SAFETY: `head` indexes the admin CQ frame.
            let status = unsafe { ptr::read_volatile((e + 14) as *const u16) };
            // Gate on the PHASE TAG (status bit0): the host zeroes the queue
            // memory, so empty slots read phase 0 while the first controller
            // pass writes phase 1; the tag inverts on every wrap of the head.
            if self.admin_cq_ring.ready(status) {
                // SAFETY: same CQ entry; DW0 and the CID echo are readable once
                // the phase tag says the entry is fresh.
                let dw0 = unsafe { ptr::read_volatile(e as *const u32) };
                let cid_echo = unsafe { ptr::read_volatile((e + 12) as *const u16) };
                let _ = self.admin_cq_ring.consume();
                self.cq_doorbell(0, self.admin_cq_ring.head() as u16);
                if cid_echo != cid {
                    warn!(
                        "[DIAG] nvme admin cq cid mismatch: echo={:#06x} want={:#06x}",
                        cid_echo, cid
                    );
                    return Err(());
                }
                // Status field bits 15:01 carry SCT/SC/DNR.
                let sc = ((status >> 1) & 0xFF) as u16;
                if sc != 0 {
                    warn!("[DIAG] nvme admin status={:#06x} (SC={})", status, sc);
                    return Err(());
                }
                return Ok(dw0);
            }
            spins += 1;
            if spins > COMPLETION_SPINS {
                warn!(
                    "[DIAG] nvme admin completion TIMEOUT head={} cfs={}",
                    head,
                    self.reg_read(CSTS) & CSTS_CFS != 0
                );
                return Err(());
            }
            core::hint::spin_loop();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn admin(
        &mut self,
        opcode: u8,
        nsid: u32,
        prp1: u64,
        prp2: u64,
        cdw10: u32,
        cdw11: u32,
    ) -> Result<u32, ()> {
        let cid = self.next_cid();
        let tail = self.admin_sq_ring.tail();
        self.write_sq_entry(
            self.admin_sq.virt,
            tail,
            cid,
            opcode,
            nsid,
            prp1,
            prp2,
            cdw10,
            cdw11,
            0,
        );
        self.admin_sq_ring.advance();
        self.ring_sq(0, self.admin_sq_ring.tail());
        self.reap_admin(cid)
    }

    fn identify(&mut self, cns: u32, nsid: u32) -> Result<(), ()> {
        self.admin(OPC_IDENTIFY, nsid, self.id_page.phys(), 0, cns, 0)?;
        Ok(())
    }

    /// Reap `cids.len()` completions, matching by CID rather than arrival
    /// order. Each CQE is consumed exactly once; the CQ head doorbell is rung
    /// once after the batch.
    fn reap_io(&mut self, cids: &[u16]) -> Result<(), ()> {
        debug_assert!(!cids.is_empty() && cids.len() <= IO_QUEUE_DEPTH);
        let mut done = [false; IO_QUEUE_DEPTH];
        let mut status_ok = [true; IO_QUEUE_DEPTH];
        let mut remaining = cids.len();
        let mut spins = 0u64;

        while remaining > 0 {
            let head = self.io_cq_ring.head();
            let e = self.io_cq.virt + head as usize * core::mem::size_of::<CqEntry>();
            // SAFETY: `head` indexes the I/O CQ frame.
            let status = unsafe { ptr::read_volatile((e + 14) as *const u16) };
            if !self.io_cq_ring.ready(status) {
                spins += 1;
                if spins > COMPLETION_SPINS {
                    // A completion may still arrive later; the next I/O would
                    // then see it as an unknown CID, so refuse all further I/O.
                    self.io_failed = true;
                    warn!(
                        "[DIAG] nvme I/O completion TIMEOUT head={} phase={} still={}",
                        head,
                        self.io_cq_ring.phase(),
                        remaining
                    );
                    return Err(());
                }
                core::hint::spin_loop();
                continue;
            }

            // SAFETY: the phase tag marks this slot as a fresh completion.
            let cid_echo = unsafe { ptr::read_volatile((e + 12) as *const u16) };
            let sc = (status >> 1) & 0xFF;
            let _ = self.io_cq_ring.consume();

            match cids.iter().position(|&c| c == cid_echo) {
                Some(i) if !done[i] => {
                    done[i] = true;
                    remaining -= 1;
                    if sc != 0 {
                        status_ok[i] = false;
                        warn!(
                            "[DIAG] nvme I/O status={:#06x} (SC={}) cid={:#06x}",
                            status, sc, cid_echo
                        );
                    }
                }
                Some(_) => {
                    self.io_failed = true;
                    warn!("[DIAG] nvme duplicate completion cid={:#06x}", cid_echo);
                    self.cq_doorbell(1, self.io_cq_ring.head() as u16);
                    return Err(());
                }
                None => {
                    self.io_failed = true;
                    warn!("[DIAG] nvme completion for unknown cid={:#06x}", cid_echo);
                    self.cq_doorbell(1, self.io_cq_ring.head() as u16);
                    return Err(());
                }
            }

            if remaining == 0 {
                self.cq_doorbell(1, self.io_cq_ring.head() as u16);
            }
            spins = 0;
        }

        if status_ok[..cids.len()].iter().all(|&ok| ok) {
            Ok(())
        } else {
            Err(())
        }
    }

    /// Submit a batch of Read/Write commands (one per slot) and reap them.
    ///
    /// All PRP plans are validated *before* the first SQ slot is written, so a
    /// rejected plan cannot leave a half-built batch that the controller would
    /// later consume.
    fn submit_io_batch(&mut self, ops: &[IoOp], lba_size: u64) -> Result<(), ()> {
        if ops.is_empty() || ops.len() > IO_QUEUE_DEPTH || ops.len() > self.io_slots.len() {
            return Err(());
        }
        IO_MAX_IN_FLIGHT.fetch_max(ops.len() as u32, Ordering::Relaxed);

        let mut prps = [(0u64, 0u64); IO_QUEUE_DEPTH];
        for (i, op) in ops.iter().enumerate() {
            let cmd_bytes = op.command_bytes(lba_size);
            let base = self.io_slots[i].data.base_phys();
            let pages = self.io_slots[i].data.pages;
            let list_phys = self.io_slots[i].prp_list.phys();
            let list_ptr = self.io_slots[i].prp_list.virt as *mut u64;
            // SAFETY: the PRP-list page was allocated for this slot and is
            // exclusively owned by it; `PRP_LIST_ENTRIES` u64s exactly fill 4 KiB.
            let list = unsafe { core::slice::from_raw_parts_mut(list_ptr, PRP_LIST_ENTRIES) };
            // The command's data starts at slot offset 0 (the LBA boundary);
            // `op.within` is only where the caller's bytes sit inside it.
            match build_prp(base, pages, 0, cmd_bytes, list_phys, list) {
                Ok(plan) => prps[i] = (plan.prp1, plan.prp2),
                Err(e) => {
                    warn!(
                        "nvme: PRP plan rejected for {}-byte command: {:?}",
                        cmd_bytes, e
                    );
                    return Err(());
                }
            }
        }

        let mut cids = [0u16; IO_QUEUE_DEPTH];
        for (i, op) in ops.iter().enumerate() {
            let cid = self.next_cid();
            cids[i] = cid;
            let tail = self.io_sq_ring.tail();
            self.write_sq_entry(
                self.io_sq.virt,
                tail,
                cid,
                if op.write {
                    OPC_NVM_WRITE
                } else {
                    OPC_NVM_READ
                },
                self.nsid,
                prps[i].0,
                prps[i].1,
                op.lba as u32,
                (op.lba >> 32) as u32,
                op.lbas - 1,
            );
            self.io_sq_ring.advance();
        }
        self.ring_sq(1, self.io_sq_ring.tail());

        self.reap_io(&cids[..ops.len()])
    }

    /// NVM Flush (opcode 0x00): make every write this namespace has acknowledged
    /// durable in non-volatile storage.
    ///
    /// No data transfer and no LBA range — CDW10..CDW12 stay zero, which the spec
    /// defines as "flush everything". Issued unconditionally rather than gated on
    /// the Identify-Controller VWC bit: for a controller with no volatile write
    /// cache the command is a defined no-op, so the gate would only add a way to
    /// silently lose the flush.
    fn io_flush(&mut self) -> Result<(), ()> {
        if self.io_failed {
            return Err(());
        }
        let cid = self.next_cid();
        let tail = self.io_sq_ring.tail();
        self.write_sq_entry(
            self.io_sq.virt,
            tail,
            cid,
            OPC_NVM_FLUSH,
            self.nsid,
            0,
            0,
            0,
            0,
            0,
        );
        self.io_sq_ring.advance();
        self.ring_sq(1, self.io_sq_ring.tail());
        self.reap_io(&[cid])
    }

    /// Correct, non-destructive write path for a byte range that does not cover
    /// whole LBAs: read each overlapped LBA, patch the caller's bytes, write it
    /// back. The aligned fast path never needs this.
    fn write_unaligned(&mut self, byte_off: u64, src: &[u8], lba_size: u64) -> Result<(), ()> {
        let mut off = byte_off;
        let mut done = 0usize;
        while done < src.len() {
            let lba = off / lba_size;
            let within = (off % lba_size) as usize;
            let n = core::cmp::min(lba_size as usize - within, src.len() - done);

            let read_op = IoOp {
                write: false,
                lba,
                lbas: 1,
                within: 0,
                data_len: 0,
            };
            self.submit_io_batch(&[read_op], lba_size)?;

            // SAFETY: the slot buffer is at least one LBA and was filled by the
            // read above; `within + n` stays inside that LBA.
            unsafe {
                let dst = (self.io_slots[0].data.virt + within) as *mut u8;
                ptr::copy_nonoverlapping(src[done..].as_ptr(), dst, n);
            }

            let write_op = IoOp {
                write: true,
                lba,
                lbas: 1,
                within: 0,
                data_len: 0,
            };
            self.submit_io_batch(&[write_op], lba_size)?;

            off += n as u64;
            done += n;
        }
        Ok(())
    }
}

// ─── Attach ──────────────────────────────────────────────────────────────────

fn is_nvme(dev: &PciDevice) -> bool {
    dev.class == 0x01 && dev.subclass == 0x08
}

static NVME_DEV: Spinlock<Option<NvmeCtrl>> = Spinlock::new(None);
static NS_SIZE_SECTORS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// High-water mark of commands submitted in one I/O batch.
///
/// A value of 1 means the block path never had more than one command in
/// flight; the in-QEMU suite reads this to prove the queue-depth path was
/// actually exercised (the driver itself does not branch on it).
static IO_MAX_IN_FLIGHT: AtomicU32 = AtomicU32::new(0);

/// Largest batch submitted since boot (observability for `src/test.rs`).
pub fn max_io_batch_depth() -> u32 {
    IO_MAX_IN_FLIGHT.load(Ordering::Relaxed)
}

fn bar0_mmio(dev_addr: pci::PciAddress) -> Result<usize, ()> {
    let lo = pci::config_read_u32(dev_addr, 0x10);
    let hi = pci::config_read_u32(dev_addr, 0x14);
    // An I/O-space BAR (bit 0 set) is not MMIO at all.
    if lo & 1 != 0 {
        return Err(());
    }
    let base_lo = lo & 0xFFFF_FFF0;
    // A BAR that reads back all ones (or zero) means the device implements no
    // address here. Mapping it anyway asked `map_mmio` for a region at
    // 0xFFFFFFF0, and on a BIOS boot that walk split a huge page in a part of the
    // address space shared with the kernel image — the boot then died on an
    // instruction fetch into its own text. Validate before mapping: a device
    // without a usable BAR0 is not a device we can drive.
    if base_lo == 0 || base_lo == 0xFFFF_FFF0 || (base_lo == 0 && hi == 0) {
        crate::warn!(
            "nvme: BAR0 of {:02x}:{:02x}.{} reads {:#010x}{:08x}; no address assigned",
            dev_addr.bus,
            dev_addr.device,
            dev_addr.function,
            hi,
            lo
        );
        return Err(());
    }
    if hi == 0xFFFF_FFFF {
        crate::warn!("nvme: BAR0 high half is all ones; ignoring device");
        return Err(());
    }
    let bar0 = (((hi as u64) << 32) | base_lo as u64) & !0xF;
    // The controller's registers must live in real physical address space.
    if bar0 == 0 || bar0 >= 0x0000_FFFF_FFFF_F000 {
        return Err(());
    }
    crate::memory::vmm::map_mmio(bar0, 0x4000)
        .map(|v| v as usize)
        .map_err(|_| ())
}

pub fn attach(devices: &[PciDevice]) -> Result<bool, ()> {
    if NVME_DEV.lock().is_some() {
        return Ok(false);
    }
    let Some(dev) = devices.iter().find(|d| is_nvme(d)) else {
        return Ok(false);
    };
    let addr = dev.address;
    info!(
        "nvme: found controller {:02x}:{:02x}.{}",
        addr.bus, addr.device, addr.function
    );
    pci::enable_bus_master(addr);

    let mmio = bar0_mmio(addr)?;
    // SAFETY: BAR0 is mapped (map_mmio above) for the 64-bit CAP register.
    let cap_lo = unsafe { ptr::read_volatile((mmio + CAP_CAP_LO) as *const u32) };
    let cap_hi = unsafe { ptr::read_volatile((mmio + CAP_CAP_HI) as *const u32) };
    // CAP[35:32] = DSTRD (doorbell stride) lives in the low nibble of the high
    // dword. Reading the wrong bits only happens to work on DSTRD=0 controllers.
    let dstrd = cap_hi & 0xF;
    let mqes = (cap_lo & 0xFFFF).saturating_add(1);
    let mpsmin = (cap_hi >> 16) & 0xF;
    info!("nvme: MQES={} DSTRD={} MPSMIN={}", mqes, dstrd, mpsmin);
    if mpsmin != 0 {
        // CC.MPS below is 4 KiB; a controller requiring larger pages cannot be
        // programmed by this driver.
        warn!("nvme: CAP.MPSMIN={} (only 4 KiB pages supported)", mpsmin);
        return Err(());
    }

    let admin_entries = queue_entries(ADMIN_Q_ENTRIES, mqes);
    let io_entries = queue_entries(IO_Q_ENTRIES, mqes);
    if admin_entries < 2 || io_entries < (IO_QUEUE_DEPTH as u32 + 1) {
        warn!(
            "nvme: queue sizes too small (admin={} io={} MQES={})",
            admin_entries, io_entries, mqes
        );
        return Err(());
    }

    // Disable, then wait for CSTS.RDY to clear.
    // SAFETY: `mmio` is the mapped BAR0 window; CC/CSTS are inside it.
    unsafe {
        let cc = ptr::read_volatile((mmio + CC) as *mut u32);
        ptr::write_volatile((mmio + CC) as *mut u32, cc & !CC_EN);
    }
    let mut spins = 0u64;
    // SAFETY: CSTS is a mapped read-only register in BAR0.
    while unsafe { ptr::read_volatile((mmio + CSTS) as *mut u32) } & CSTS_RDY != 0 {
        spins += 1;
        if spins > 50_000_000 {
            warn!("nvme: CSTS.RDY stuck high during disable");
            return Err(());
        }
        core::hint::spin_loop();
    }

    let admin_sq = DmaFrame::new_zeroed()?;
    let admin_cq = DmaFrame::new_zeroed()?;
    let io_sq = DmaFrame::new_zeroed()?;
    let io_cq = DmaFrame::new_zeroed()?;
    let id_page = DmaFrame::new_zeroed()?;

    // Program queue bases BEFORE enabling. AQA's two fields are zero-based.
    // SAFETY: AQA/ASQ/ACQ/ASQ_HI/ACQ_HI are mapped BAR0 registers.
    unsafe {
        ptr::write_volatile(
            (mmio + AQA) as *mut u32,
            ((admin_entries - 1) << 16) | (admin_entries - 1),
        );
        ptr::write_volatile((mmio + ASQ) as *mut u32, admin_sq.phys() as u32);
        ptr::write_volatile((mmio + ASQ_HI) as *mut u32, (admin_sq.phys() >> 32) as u32);
        ptr::write_volatile((mmio + ACQ) as *mut u32, admin_cq.phys() as u32);
        ptr::write_volatile((mmio + ACQ_HI) as *mut u32, (admin_cq.phys() >> 32) as u32);
    }

    // Enable: single CC write with all fields.
    // SAFETY: CC is a mapped BAR0 register.
    unsafe {
        ptr::write_volatile(
            (mmio + CC) as *mut u32,
            CC_EN | CC_CSS_NVM | CC_MPS_4K | CC_IOSQES_64 | CC_IOCQES_16,
        );
    }
    spins = 0;
    loop {
        // SAFETY: CSTS is mapped; read-only polling.
        let csts = unsafe { ptr::read_volatile((mmio + CSTS) as *const u32) };
        if csts & CSTS_RDY != 0 {
            break;
        }
        if csts & CSTS_CFS != 0 {
            warn!("nvme: controller fatal status (CFS)");
            return Err(());
        }
        spins += 1;
        if spins > 2_000_000_000 {
            warn!("nvme: CSTS.RDY never set (csts={:08x})", csts);
            return Err(());
        }
        core::hint::spin_loop();
    }

    let mut ctl = NvmeCtrl {
        mmio,
        dstrd,
        admin_sq,
        admin_cq,
        io_sq,
        io_cq,
        id_page,
        io_slots: Vec::new(),
        admin_sq_ring: SqRing::new(admin_entries),
        admin_cq_ring: CqRing::new(admin_entries),
        io_sq_ring: SqRing::new(io_entries),
        io_cq_ring: CqRing::new(io_entries),
        cid: 0,
        io_failed: false,
        nsid: 0,
        lbads: 9,
        io_max_bytes: 0,
    };

    ctl.identify(IDENTIFY_CTRL, 0)?;
    // Identify Controller: Number of Namespaces (NN) lives at bytes 516..519
    // (0x204), vendor ID at bytes 0..2, MDTS at byte 77.
    // SAFETY: `id_page` is a PMM frame and the identify completed successfully.
    let nn = unsafe { ptr::read_volatile((ctl.id_page.virt + 516) as *const u32) };
    let vid_lo = unsafe { ptr::read_volatile(ctl.id_page.virt as *const u16) };
    let mdts = unsafe { ptr::read_volatile((ctl.id_page.virt + ID_CTRL_MDTS_OFF) as *const u8) };
    info!("nvme: ctrl VID={:#06x} NN={} MDTS={}", vid_lo, nn, mdts);
    if vid_lo == 0 {
        warn!("nvme: identify data is all zeroes (DMA did not land)");
        return Err(());
    }
    if nn == 0 {
        warn!("nvme: controller reports zero namespaces");
        return Err(());
    }

    // Active namespace list → enumerate valid NSIDs.
    let mut nsid: u32 = 0;
    match ctl.identify(IDENTIFY_NS_ACTIVE_LIST, 0) {
        Ok(_) => {
            for i in 0..8usize {
                // SAFETY: `id_page` holds the active-list response.
                let v = unsafe { ptr::read_volatile((ctl.id_page.virt + i * 4) as *const u32) };
                info!("nvme: nslist[{}] = {}", i, v);
                if nsid == 0 && v != 0 {
                    nsid = v;
                }
            }
        }
        Err(_) => warn!("nvme: active list rejected"),
    }
    if nsid == 0 {
        for cand in 1u32..=256u32 {
            if ctl.identify(IDENTIFY_NS, cand).is_ok() {
                // SAFETY: `id_page` holds the NS identify response; NSZE is at 0.
                let nsze_c = unsafe { ptr::read_volatile(ctl.id_page.virt as *const u64) };
                if nsze_c > 0 {
                    info!("nvme: scanned nsid={} NSZE={}", cand, nsze_c);
                    nsid = cand;
                    break;
                }
            }
        }
    }
    if nsid == 0 {
        warn!("nvme: no usable namespace found");
        return Err(());
    }
    ctl.nsid = nsid;

    // Identify namespace → NSZE + selected LBA format.
    if ctl.identify(IDENTIFY_NS, nsid).is_err() {
        warn!("nvme: identify ns failed");
        return Err(());
    }
    // SAFETY: `id_page` now holds the Identify Namespace response.
    let nsze = unsafe { ptr::read_volatile(ctl.id_page.virt as *const u64) };
    let flbas_idx = unsafe { ptr::read_volatile((ctl.id_page.virt + 26) as *const u8) };
    let lf_off = ctl.id_page.virt + 128 + (flbas_idx as usize) * 4;
    let ds = unsafe { ptr::read_volatile((lf_off + 2) as *const u8) };
    if !(9..=20).contains(&ds) || nsze == 0 {
        warn!("nvme: bogus NSZE={} LBADS={}", nsze, ds);
        return Err(());
    }
    ctl.lbads = ds;
    let lba_size = 1usize << ds;

    // A command may cover at most MDTS × MPS bytes, where MPS is 4 KiB here.
    // MDTS=0 means "no limit"; either way the slot buffer is the real cap.
    let mdts_pages = if mdts == 0 {
        IO_BUFFER_PAGES
    } else {
        let limit = 1usize.checked_shl(mdts as u32).unwrap_or(usize::MAX);
        limit.min(IO_BUFFER_PAGES)
    };
    if mdts_pages == 0 {
        warn!("nvme: MDTS={} yields no transfer, refusing", mdts);
        return Err(());
    }
    let io_max_bytes = mdts_pages * PAGE_SIZE;
    if lba_size > io_max_bytes || io_max_bytes % lba_size != 0 {
        warn!(
            "nvme: LBA size {} does not fit the {}-byte transfer cap",
            lba_size, io_max_bytes
        );
        return Err(());
    }
    ctl.io_max_bytes = io_max_bytes;

    // Allocate one data slot (plus its PRP-list page) per in-flight command.
    for _ in 0..IO_QUEUE_DEPTH {
        ctl.io_slots.push(IoSlot::new(io_max_bytes / PAGE_SIZE)?);
    }

    // Create IOCQ (qid 1) then IOSQ (qid 1, bound to CQ 1).
    //
    // CDW10 layout for both commands: queue ID in bits 15:0 and QSIZE in bits
    // 31:16 (zero-based, so a 64-entry ring is created with 63 — otherwise the
    // controller wraps one entry later than the host and reads past the ring).
    // CDW11 carries PC for both queues, plus the bound CQID in its bits 31:16
    // for Create-IOSQ.
    let create_cdw10 = ((io_entries - 1) << 16) | 1;
    if ctl
        .admin(OPC_CREATE_IOCQ, 0, ctl.io_cq.phys(), 0, create_cdw10, 1)
        .is_err()
    {
        warn!("nvme: create IOCQ failed");
        return Err(());
    }
    if ctl
        .admin(
            OPC_CREATE_IOSQ,
            0,
            ctl.io_sq.phys(),
            0,
            create_cdw10,
            1 | (1 << 16),
        )
        .is_err()
    {
        warn!("nvme: create IOSQ failed");
        return Err(());
    }
    info!(
        "nvme: ready nsid={} LBA={}B io_queue={} in-flight={} max-xfer={}B (PRP list)",
        nsid, lba_size, io_entries, IO_QUEUE_DEPTH, io_max_bytes
    );

    // BlockDevice is sector-addressed (512 B), while the namespace is
    // LBA-addressed. Report the whole namespace so ext2 can size a fresh
    // filesystem; without this the device claims zero capacity and boot's
    // blank-disk format path refuses with OutOfSpace.
    let bytes_per_lba = 1u64 << ds;
    let sectors = nsze.saturating_mul(bytes_per_lba / 512);
    NS_SIZE_SECTORS.store(sectors, Ordering::Relaxed);

    *NVME_DEV.lock() = Some(ctl);
    Ok(true)
}

// ─── BlockDevice impl ────────────────────────────────────────────────────────

struct NvmeBlock;

impl BlockDevice for NvmeBlock {
    fn name(&self) -> &str {
        "nvme0"
    }

    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<usize, ()> {
        rw_sectors(block, buf, false)?;
        Ok(buf.len())
    }

    fn write_block(&self, block: u64, buf: &[u8]) -> Result<usize, ()> {
        rw_sectors_buf(block, buf)?;
        Ok(buf.len())
    }

    fn sector_count(&self) -> u64 {
        NS_SIZE_SECTORS.load(Ordering::Relaxed)
    }

    /// NVMe FLUSH on the attached namespace (issue #15): the journal's ordering
    /// guarantee is only real if the controller's volatile cache is drained at
    /// transaction boundaries.
    fn flush(&self) -> Result<(), ()> {
        let mut guard = NVME_DEV.lock();
        match guard.as_mut() {
            Some(c) => c.io_flush(),
            None => Err(()),
        }
    }
}

/// Chunked transfer: `start_byte = block * 512`, length = `buf.len()`. Each
/// plan covers whole LBAs and up to `io_max_bytes`; up to `IO_QUEUE_DEPTH`
/// plans are submitted before the first completion is reaped.
fn rw_common(
    block: u64,
    src: Option<&[u8]>,
    mut dst: Option<&mut [u8]>,
    write: bool,
) -> Result<(), ()> {
    let mut guard = NVME_DEV.lock();
    let Some(c) = guard.as_mut() else {
        return Err(());
    };
    if c.io_failed {
        return Err(());
    }
    let lba_size = 1u64 << c.lbads;

    let total = match (&src, &dst) {
        (Some(s), None) => s.len(),
        (None, Some(d)) => d.len(),
        _ => return Err(()),
    };
    if total == 0 {
        return Ok(());
    }

    let byte_off = block.checked_mul(512).ok_or(())?;
    let end = byte_off.checked_add(total as u64).ok_or(())?;

    // A write may only be issued for whole LBAs (the controller writes every
    // byte of an LBA). Unaligned writes use the read-modify-write path so the
    // bytes outside the caller's range are preserved, not zeroed.
    if write && (byte_off % lba_size != 0 || (total as u64) % lba_size != 0) {
        let src = src.ok_or(())?;
        return c.write_unaligned(byte_off, src, lba_size);
    }

    let mut cursor = byte_off;
    let mut done = 0usize;
    while cursor < end {
        let mut ops = [IoOp {
            write,
            lba: 0,
            lbas: 1,
            within: 0,
            data_len: 0,
        }; IO_QUEUE_DEPTH];
        let mut n = 0usize;
        let mut planned = cursor;

        while n < IO_QUEUE_DEPTH && planned < end {
            let Some(plan) =
                plan_chunk(planned, (end - planned) as usize, lba_size, c.io_max_bytes)
            else {
                warn!(
                    "nvme: cannot plan chunk at {} (lba={} max={})",
                    planned, lba_size, c.io_max_bytes
                );
                return Err(());
            };
            ops[n] = IoOp {
                write,
                lba: plan.lba,
                lbas: plan.lbas,
                within: plan.within,
                data_len: plan.data_len,
            };
            planned += plan.data_len as u64;
            n += 1;
        }
        let batch_bytes = (planned - cursor) as usize;

        if write {
            let source = src.ok_or(())?;
            let mut src_off = done;
            for (i, op) in ops[..n].iter().enumerate() {
                let cmd_bytes = op.command_bytes(lba_size);
                let slot = &mut c.io_slots[i];
                debug_assert!(cmd_bytes <= slot.data.pages * PAGE_SIZE);
                debug_assert!(op.within + op.data_len <= slot.data.pages * PAGE_SIZE);
                // SAFETY: `plan_chunk` keeps the LBA-aligned command (which
                // starts at slot offset 0) and the caller's bytes at `within`
                // inside the slot buffer; `src_off + data_len` stays inside the
                // caller's buffer (the plans partition the requested range).
                unsafe {
                    let base = slot.data.virt + op.within;
                    ptr::copy_nonoverlapping(
                        source.as_ptr().add(src_off),
                        base as *mut u8,
                        op.data_len,
                    );
                    let written = op.within + op.data_len;
                    if cmd_bytes > written {
                        ptr::write_bytes(
                            (slot.data.virt + written) as *mut u8,
                            0,
                            cmd_bytes - written,
                        );
                    }
                }
                src_off += op.data_len;
            }
        }

        c.submit_io_batch(&ops[..n], lba_size)?;

        if !write {
            let dest = dst.as_deref_mut().ok_or(())?;
            for (i, op) in ops[..n].iter().enumerate() {
                // SAFETY: the read command filled `within + command_bytes`
                // bytes of the slot; copying the caller's `data_len` from
                // `within` stays inside both the slot and `dest`.
                unsafe {
                    ptr::copy_nonoverlapping(
                        (c.io_slots[i].data.virt + op.within) as *const u8,
                        dest.as_mut_ptr().add(done),
                        op.data_len,
                    );
                }
                done += op.data_len;
            }
        } else {
            done += batch_bytes;
        }

        cursor = planned;
    }
    Ok(())
}

fn rw_sectors(block: u64, buf: &mut [u8], write: bool) -> Result<(), ()> {
    rw_common(block, None, Some(buf), write)
}

fn rw_sectors_buf(block: u64, buf: &[u8]) -> Result<(), ()> {
    rw_common(block, Some(buf), None, true)
}

/// Register the NVMe block device (if attached) in the global registry.
pub fn register_block_device() {
    if NVME_DEV.lock().is_some() {
        crate::drivers::register_block(alloc::sync::Arc::new(NvmeBlock));
    }
}
