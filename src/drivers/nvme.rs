//! NVMe host-controller driver over PCIe — polled, no IRQs.
//!
//! Bring-up: PCI probe (class 01h/08h) → map BAR0 → disable controller →
//! program AQA/ASQ/ACQ → enable → Identify Controller → Create I/O CQ + SQ →
//! Identify active namespaces → ready.
//!
//! Block I/O: every request runs in page-sized chunks against one
//! preallocated physically-contiguous scratch frame; each chunk is a separate
//! Read/Write command with a single PRP entry (PRP1 = scratch, PRP2 = 0), so
//! no PRP lists are built and caller buffers need no physical contiguity.
//! Data is copied to/from the caller through the HHDM window.
//!
//! Completion queues are POLLED via the phase bit in the status word;
//! interrupts stay masked. All register accesses are volatile u32s.

use crate::drivers::pci::{self, PciDevice};
use crate::drivers::BlockDevice;
use crate::memory::pmm;
use crate::sync::spinlock::Spinlock;
use crate::{info, warn};
use core::ptr;

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

const ADMIN_Q_ENTRIES: u32 = 32;
const IO_SQ_ENTRIES: u32 = 64;
const IO_CQ_ENTRIES: u32 = 64;

// Admin opcodes.
const OPC_CREATE_IOSQ: u8 = 0x01;
const OPC_CREATE_IOCQ: u8 = 0x05;
const OPC_IDENTIFY: u8 = 0x06;
// NVM opcodes.
const OPC_NVM_READ: u8 = 0x02;
const OPC_NVM_WRITE: u8 = 0x03;

const IDENTIFY_NS: u32 = 0;
const IDENTIFY_CTRL: u32 = 1;
const IDENTIFY_NS_ACTIVE_LIST: u32 = 2;

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

// ─── DMA helper ──────────────────────────────────────────────────────────────

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
        info!("nvme dma: frame {:#x} -> virt {:#x}", phys, virt);
        unsafe { ptr::write_bytes(virt as *mut u8, 0, 4096) };
        Ok(DmaFrame {
            phys: phys & !0xFFF,
            virt,
        })
    }

    fn phys(&self) -> u64 {
        self.phys
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
    data_page: DmaFrame,

    cid: u16,
    admin_head: u32,
    admin_tail: u32,
    io_cq_head: u32,
    io_sq_tail: u32,
    /// Phase-tag expectation per completion queue (admin, io).
    cq_phase_admin: u32,
    cq_phase_io: u32,

    nsid: u32,
    lbads: u8,
    nsze_lbas: u64,
}

unsafe impl Send for NvmeCtrl {}
unsafe impl Sync for NvmeCtrl {}

impl NvmeCtrl {
    #[inline]
    fn reg_read(&self, off: usize) -> u32 {
        unsafe { ptr::read_volatile((self.mmio + off) as *const u32) }
    }
    #[inline]
    fn reg_write(&self, off: usize, v: u32) {
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

    /// Write one 64-byte submission entry and ring the doorbell.
    #[allow(clippy::too_many_arguments)]
    fn submit(
        &mut self,
        sq_base: usize,
        qid: u32,
        tail: u32,
        opcode: u8,
        nsid: u32,
        prp1: u64,
        prp2: u64,
        cdw10: u32,
        cdw11: u32,
        cdw12: u32,
    ) -> u16 {
        self.cid = self.cid.wrapping_add(1);
        let cid = self.cid;
        let e = sq_base + tail as usize * core::mem::size_of::<SqEntry>();
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
        let next = (tail + 1) % self.sq_entries(qid);
        self.sq_doorbell(qid, next as u16);
        unsafe {
            let p = e as *const u32;
            crate::warn!(
                "[DIAG] posted q={} slot={} dw0={:08x} dw2={:08x} prp={:08x} cdw10={:08x}",
                qid,
                tail,
                ptr::read_volatile(p),
                ptr::read_volatile(p.add(2)),
                ptr::read_volatile(p.add(8)),
                ptr::read_volatile(p.add(12))
            );
        }
        cid
    }

    pub(crate) fn id_page_phys_dbg(&self) -> u64 {
        self.id_page.phys()
    }

    fn sq_entries(&self, qid: u32) -> u32 {
        if qid == 0 {
            ADMIN_Q_ENTRIES
        } else {
            IO_SQ_ENTRIES
        }
    }

    /// Poll the completion queue until an entry with matching CID arrives.
    ///
    /// Gate on the PHASE TAG (status bit0): the host zeroes the queue memory,
    /// so empty slots read phase 0 while the first controller pass writes
    /// phase 1; the tag inverts on every wrap of the completion head.
    fn complete(&mut self, cq_base: usize, qid: u32, cq_entries: u32, cid: u16) -> Result<u32, ()> {
        let mut spins = 0u64;
        loop {
            let head = if qid == 0 {
                self.admin_head
            } else {
                self.io_cq_head
            };
            let e = cq_base + head as usize * core::mem::size_of::<CqEntry>();
            let status = unsafe { ptr::read_volatile((e + 14) as *const u16) };
            // The controller inverts the phase tag on every CQ wrap; track it
            // per queue (starting phase is 1 because the host zeroes the CQ).
            let expected_phase = if qid == 0 {
                self.cq_phase_admin
            } else {
                self.cq_phase_io
            };
            if u32::from(status & 1) == expected_phase {
                let dw0 = unsafe { ptr::read_volatile(e as *const u32) };
                // Advance head (wrapping); invert the phase tag on wrap.
                let new_head = (head + 1) % cq_entries;
                if new_head == 0 {
                    if qid == 0 {
                        self.cq_phase_admin ^= 1;
                    } else {
                        self.cq_phase_io ^= 1;
                    }
                }
                if qid == 0 {
                    self.admin_head = new_head;
                } else {
                    self.io_cq_head = new_head;
                }
                self.cq_doorbell(qid, new_head as u16);

                let cid_echo = unsafe { ptr::read_volatile((e + 12) as *const u16) };
                if cid_echo != 0 && cid_echo != cid {
                    warn!(
                        "[DIAG] nvme cq cid mismatch: echo={:#06x} want={:#06x}",
                        cid_echo, cid
                    );
                }
                // Status field bits 15:01 carry SCT/SC/DNR.
                let sc = ((status >> 1) & 0xFF) as u16;
                if sc != 0 {
                    warn!("[DIAG] nvme qid={} status={:#06x} (SC={})", qid, status, sc);
                    return Err(());
                }
                return Ok(dw0);
            }
            spins += 1;
            if spins > COMPLETION_SPINS {
                warn!(
                    "[DIAG] nvme completion TIMEOUT qid={} head={} cfs={}",
                    qid,
                    if qid == 0 {
                        self.admin_head
                    } else {
                        self.io_cq_head
                    },
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
        let tail = self.admin_tail;
        let cid = self.submit(
            self.admin_sq.virt,
            0,
            tail,
            opcode,
            nsid,
            prp1,
            prp2,
            cdw10,
            cdw11,
            0,
        );
        let res = self.complete(self.admin_cq.virt, 0, ADMIN_Q_ENTRIES, cid);
        self.admin_tail = (tail + 1) % ADMIN_Q_ENTRIES;
        res
    }

    /// TEMP: run one raw admin command and dump its full completion slot.
    fn admin_dump(&mut self, tag: &str, opcode: u8, nsid: u32, cdw10: u32) {
        let tail = self.admin_tail;
        let cid = self.submit(
            self.admin_sq.virt,
            0,
            tail,
            opcode,
            nsid,
            self.id_page.phys(),
            0,
            cdw10,
            0,
            0,
        );
        let mut spins = 0u64;
        loop {
            let e = self.admin_cq.virt + self.admin_head as usize * 16;
            let st = unsafe { ptr::read_volatile((e + 14) as *const u16) };
            if (st & 1) as u32 == self.cq_phase_admin {
                let mut row = alloc::string::String::new();
                for k in 0..16usize {
                    let b = unsafe { ptr::read_volatile((e + k) as *const u8) };
                    row.push_str(&alloc::format!("{:02x} ", b));
                }
                crate::warn!("[DIAG] {} ourcid={} cqe bytes: {}", tag, cid, row);
                self.admin_head += 1;
                if self.admin_head == ADMIN_Q_ENTRIES {
                    self.admin_head = 0;
                    self.cq_phase_admin ^= 1;
                }
                self.cq_doorbell(0, self.admin_head as u16);
                self.admin_tail = (tail + 1) % ADMIN_Q_ENTRIES;
                return;
            }
            spins += 1;
            if spins > COMPLETION_SPINS {
                crate::warn!("[DIAG] {} TIMEOUT", tag);
                return;
            }
            core::hint::spin_loop();
        }
    }

    fn identify(&mut self, cns: u32, nsid: u32) -> Result<(), ()> {
        self.admin(OPC_IDENTIFY, nsid, self.id_page.phys(), 0, cns, 0)?;
        Ok(())
    }

    fn io_rw(
        &mut self,
        write: bool,
        slba: u64,
        nlb_zero_based: u32,
        data_phys: u64,
    ) -> Result<(), ()> {
        let tail = self.io_sq_tail;
        let opcode = if write { OPC_NVM_WRITE } else { OPC_NVM_READ };
        let cid = self.submit(
            self.io_sq.virt,
            1,
            tail,
            opcode,
            self.nsid,
            data_phys,
            0,
            slba as u32,
            (slba >> 32) as u32,
            nlb_zero_based,
        );
        self.complete(self.io_cq.virt, 1, IO_CQ_ENTRIES, cid)?;
        self.io_sq_tail = (tail + 1) % IO_SQ_ENTRIES;
        Ok(())
    }
}

// ─── Attach ──────────────────────────────────────────────────────────────────

/// TEMP helper: submit without touching queue tails bookkeeping.
#[allow(clippy::too_many_arguments)]
fn self_submit_raw(
    ctl: &mut NvmeCtrl,
    opcode: u8,
    nsid: u32,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
) -> u16 {
    ctl.submit(
        ctl.admin_sq.virt,
        0,
        0,
        opcode,
        nsid,
        prp1,
        prp2,
        cdw10,
        cdw11,
        0,
    )
}

fn r_ok_marker() -> () {}

fn is_nvme(dev: &PciDevice) -> bool {
    dev.class == 0x01 && dev.subclass == 0x08
}

static NVME_DEV: Spinlock<Option<NvmeCtrl>> = Spinlock::new(None);
static NS_SIZE_SECTORS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn bar0_mmio(dev_addr: pci::PciAddress) -> Result<usize, ()> {
    let lo = pci::config_read_u32(dev_addr, 0x10);
    let hi = pci::config_read_u32(dev_addr, 0x14);
    if lo & 1 != 0 {
        return Err(());
    }
    let bar0 = (((hi as u64) << 32) | (lo & 0xFFFF_FFF0) as u64) & !0xF;
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
    let cap_lo = unsafe { ptr::read_volatile((mmio + CAP_CAP_LO) as *const u32) };
    let cap_hi = unsafe { ptr::read_volatile((mmio + CAP_CAP_HI) as *const u32) };
    let dstrd = (cap_hi >> 12) & 0xF;
    info!("nvme: MQES={} DSTRD={}", (cap_lo & 0xFFFF) + 1, dstrd);

    // Disable, then wait for CSTS.RDY to clear.
    unsafe {
        let cc = ptr::read_volatile((mmio + CC) as *mut u32);
        ptr::write_volatile((mmio + CC) as *mut u32, cc & !CC_EN);
    }
    let mut spins = 0u64;
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
    let data_page = DmaFrame::new_zeroed()?;

    // Program queue bases BEFORE enabling.
    unsafe {
        ptr::write_volatile(
            (mmio + AQA) as *mut u32,
            ((ADMIN_Q_ENTRIES - 1) << 16) | (ADMIN_Q_ENTRIES - 1),
        );
        ptr::write_volatile((mmio + ASQ) as *mut u32, admin_sq.phys() as u32);
        ptr::write_volatile((mmio + ASQ_HI) as *mut u32, (admin_sq.phys() >> 32) as u32);
        ptr::write_volatile((mmio + ACQ) as *mut u32, admin_cq.phys() as u32);
        ptr::write_volatile((mmio + ACQ_HI) as *mut u32, (admin_cq.phys() >> 32) as u32);
    }

    // Enable: single CC write with all fields (matches the sequence that
    // worked against QEMU's model from the very first bring-up run).
    unsafe {
        ptr::write_volatile(
            (mmio + CC) as *mut u32,
            CC_EN | CC_CSS_NVM | CC_MPS_4K | CC_IOSQES_64 | CC_IOCQES_16,
        );
    }
    spins = 0;
    loop {
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
        data_page,
        cid: 0,
        admin_tail: 0,
        admin_head: 0,
        io_sq_tail: 0,
        io_cq_head: 0,
        cq_phase_admin: 1,
        cq_phase_io: 1,
        nsid: 0,
        lbads: 9,
        nsze_lbas: 0,
    };

    // TEMP matrix: identical opcode, different payloads — watch which CQE fields move.
    ctl.admin_dump("id-ctrl nsid=0", OPC_IDENTIFY, 0, 1);
    ctl.admin_dump("id-ctrl nsid=FFFF", OPC_IDENTIFY, 0xFFFF_FFFF, 1);

    ctl.identify(IDENTIFY_CTRL, 0)?;
    // Identify Controller: Number of Namespaces (NN) lives at byte 0x1FC,
    // vendor ID (IEEE OID prefix) at bytes 0..2.
    let nn = unsafe { ptr::read_volatile((ctl.id_page.virt + 508) as *const u32) };
    let vid_lo = unsafe { ptr::read_volatile(ctl.id_page.virt as *const u16) };
    info!("nvme: ctrl VID={:#06x} NN={}", vid_lo, nn);
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
    info!("nvme: step id-ns-done");
    let nsze = unsafe { ptr::read_volatile(ctl.id_page.virt as *const u64) };
    let flbas_idx = unsafe { ptr::read_volatile((ctl.id_page.virt + 26) as *const u8) };
    let lf_off = ctl.id_page.virt + 128 + (flbas_idx as usize) * 4;
    let ds = unsafe { ptr::read_volatile((lf_off + 2) as *const u8) };
    if ds == 0 || ds > 20 || nsze == 0 {
        warn!("nvme: bogus NSZE={} LBADS={}", nsze, ds);
        return Err(());
    }
    ctl.lbads = ds;
    ctl.nsze_lbas = nsze;

    // Create IOCQ (qid 1) then IOSQ (qid 1, bound to CQ 1).
    if ctl
        .admin(OPC_CREATE_IOCQ, 0, ctl.io_cq.phys(), 0, IO_CQ_ENTRIES, 1)
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
            IO_SQ_ENTRIES,
            1 | (1 << 16),
        )
        .is_err()
    {
        warn!("nvme: create IOSQ failed");
        return Err(());
    }
    info!("nvme: ready nsid={} LBA={}B", nsid, 1u64 << ds);

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
        NS_SIZE_SECTORS.load(core::sync::atomic::Ordering::Relaxed)
    }
}

/// Chunked transfer: `start_byte = block * 512`, length = buf.len(). Each
/// device-LBA-aligned chunk goes out as its own single-PRP command against
/// the preallocated scratch frame.
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
    let lba_size = 1u64 << c.lbads;
    let lba_mask = lba_size - 1;

    let total = match (&src, &dst) {
        (Some(s), None) => s.len(),
        (None, Some(d)) => d.len(),
        _ => return Err(()),
    };
    let mut byte_off = block * 512;
    let end = byte_off + total as u64;
    let mut done = 0usize;

    while byte_off < end {
        let lba = byte_off / lba_size;
        let within = (byte_off & lba_mask) as usize;
        let chunk_len = core::cmp::min(lba_size as usize - within, (end - byte_off) as usize);
        let nlb_units = ((within + chunk_len + lba_size as usize - 1) / lba_size as usize) - 1;

        let scratch = c.data_page.virt as *mut u8;
        if write {
            if let Some(s) = src {
                unsafe { ptr::copy_nonoverlapping(s[done..].as_ptr(), scratch, chunk_len) };
            }
            if within + chunk_len < lba_size as usize {
                unsafe {
                    ptr::write_bytes(
                        scratch.add(chunk_len),
                        0,
                        lba_size as usize - within - chunk_len,
                    )
                };
            }
        }
        c.io_rw(write, lba, nlb_units as u32, c.data_page.phys())?;
        if !write {
            if let Some(d) = dst.as_deref_mut() {
                unsafe {
                    ptr::copy_nonoverlapping(
                        scratch as *const u8,
                        d[done..].as_mut_ptr(),
                        chunk_len,
                    )
                };
            }
        }

        byte_off += chunk_len as u64;
        done += chunk_len;
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
