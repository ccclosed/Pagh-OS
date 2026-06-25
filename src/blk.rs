//! virtio-blk over the virtio-mmio transport (QEMU `virt`).
//!
//! Reuses the `virtio-drivers` crate (the same one the x86_64 kernel drives over
//! PCI). The [`Hal`] implementation is trivial here because the kernel runs with
//! an identity Sv39 map: physical == virtual, and PMM frames are physically
//! contiguous, so DMA needs no bounce buffers or address translation.

use core::ptr::NonNull;

use virtio_drivers::device::blk::{VirtIOBlk, SECTOR_SIZE};
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};
use virtio_drivers::transport::{DeviceType, Transport};
use virtio_drivers::{BufferDirection, Hal, PhysAddr};

/// `virtio_drivers::Hal` over the identity-mapped PMM.
pub struct HalImpl;

// SAFETY: identity mapping (phys == virt) and PMM-backed contiguous DMA frames
// satisfy every Hal contract; share/unshare are no-ops because no translation or
// bouncing is needed.
unsafe impl Hal for HalImpl {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        let pa = crate::pmm::alloc_contig(pages).expect("virtio dma_alloc: out of frames");
        // SAFETY: freshly-owned contiguous frames, identity-mapped.
        unsafe { core::ptr::write_bytes(pa as *mut u8, 0, pages * crate::pmm::FRAME_SIZE) };
        (pa, NonNull::new(pa as *mut u8).unwrap())
    }

    unsafe fn dma_dealloc(paddr: PhysAddr, _vaddr: NonNull<u8>, pages: usize) -> i32 {
        for i in 0..pages {
            crate::pmm::free_frame(paddr + i * crate::pmm::FRAME_SIZE);
        }
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new(paddr as *mut u8).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        buffer.cast::<u8>().as_ptr() as PhysAddr
    }

    unsafe fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {}
}

/// A discovered virtio-blk device.
pub type Blk = VirtIOBlk<HalImpl, MmioTransport<'static>>;

/// `Send` wrapper so the device can live in a `static` Mutex (single-hart).
struct BlkCell(Option<Blk>);
// SAFETY: all access is serialized through BLK.lock() on the boot hart.
unsafe impl Send for BlkCell {}

static BLK: spin::Mutex<BlkCell> = spin::Mutex::new(BlkCell(None));

/// Scan the DTB's virtio-mmio nodes and return the first that is a block device.
///
/// We read the device-type from the MMIO `DeviceID` register directly and only
/// construct an `MmioTransport` for the matching slot. This matters: dropping an
/// `MmioTransport` resets its device, so creating+dropping transports over slots
/// we don't keep would reset other (already-initialized) virtio devices.
pub fn probe(dtb: usize) -> Option<Blk> {
    // SAFETY: valid DTB pointer from OpenSBI.
    let fdt = unsafe { fdt::Fdt::from_ptr(dtb as *const u8) }.ok()?;
    for node in fdt.all_nodes() {
        let is_vmmio = node
            .compatible()
            .map(|c| c.all().any(|s| s == "virtio,mmio"))
            .unwrap_or(false);
        if !is_vmmio {
            continue;
        }
        let mut regs = match node.reg() {
            Some(r) => r,
            None => continue,
        };
        let (base, size) = match regs.next() {
            Some(r) => (r.starting_address as usize, r.size.unwrap_or(0x1000)),
            None => continue,
        };
        // Identify the device without building (and dropping) a transport.
        // virtio-mmio: 0x00 = MagicValue ("virt"), 0x08 = DeviceID (2 = block).
        // SAFETY: identity-mapped MMIO reads.
        let (magic, dev_id) = unsafe {
            (
                core::ptr::read_volatile(base as *const u32),
                core::ptr::read_volatile((base + 8) as *const u32),
            )
        };
        if magic != 0x7472_6976 || dev_id != 2 {
            continue;
        }
        let header = NonNull::new(base as *mut VirtIOHeader)?;
        // SAFETY: confirmed virtio-mmio block device at this identity-mapped window.
        let transport = match unsafe { MmioTransport::new(header, size) } {
            Ok(t) => t,
            Err(_) => continue,
        };
        if transport.device_type() == DeviceType::Block {
            if let Ok(blk) = Blk::new(transport) {
                return Some(blk);
            }
        }
    }
    None
}

/// Probe for a virtio-blk disk and store it in the global [`BLK`]. Returns the
/// capacity in 512-byte sectors on success.
pub fn init(dtb: usize) -> Option<u64> {
    let blk = probe(dtb)?;
    let cap = blk.capacity();
    BLK.lock().0 = Some(blk);
    Some(cap)
}

/// Device capacity in 512-byte sectors, if a disk is attached.
pub fn capacity() -> Option<u64> {
    BLK.lock().0.as_ref().map(|b| b.capacity())
}

/// `crate::drivers::BlockDevice` adapter over the global virtio-blk device, so
/// the ported ext2/journal stack can use it as "virtio-blk0".
pub struct VirtioBlk;

impl crate::drivers::BlockDevice for VirtioBlk {
    fn name(&self) -> &str {
        "virtio-blk0"
    }
    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<usize, ()> {
        if buf.is_empty() || buf.len() % 512 != 0 {
            return Err(());
        }
        let n = buf.len() / 512;
        for i in 0..n {
            if !read_sector(block as usize + i, &mut buf[i * 512..(i + 1) * 512]) {
                return Err(());
            }
        }
        Ok(buf.len())
    }
    fn write_block(&self, block: u64, buf: &[u8]) -> Result<usize, ()> {
        if buf.is_empty() || buf.len() % 512 != 0 {
            return Err(());
        }
        let n = buf.len() / 512;
        for i in 0..n {
            if !write_sector(block as usize + i, &buf[i * 512..(i + 1) * 512]) {
                return Err(());
            }
        }
        Ok(buf.len())
    }
    fn sector_count(&self) -> u64 {
        capacity().unwrap_or(0)
    }
}

/// Register the virtio-blk device in the driver registry as "virtio-blk0".
pub fn register() {
    crate::drivers::register_block(alloc::sync::Arc::new(VirtioBlk));
}

/// Read one 512-byte sector into `buf` (must be `SECTOR_SIZE`). Returns success.
pub fn read_sector(sector: usize, buf: &mut [u8]) -> bool {
    match BLK.lock().0.as_mut() {
        Some(b) => b.read_blocks(sector, buf).is_ok(),
        None => false,
    }
}

/// Write one 512-byte sector from `buf` (must be `SECTOR_SIZE`). Returns success.
pub fn write_sector(sector: usize, buf: &[u8]) -> bool {
    match BLK.lock().0.as_mut() {
        Some(b) => b.write_blocks(sector, buf).is_ok(),
        None => false,
    }
}

/// Run a read/write round-trip self-test against the last sector (scratch),
/// printing the result. Uses the attached device in [`BLK`].
pub fn selftest() {
    let cap = match capacity() {
        Some(c) => c,
        None => {
            crate::kprintln!("rv: no virtio-blk device found on virtio-mmio");
            return;
        }
    };
    crate::kprintln!(
        "rv: virtio-blk found -- {} sectors ({} MiB)",
        cap,
        cap * SECTOR_SIZE as u64 / (1024 * 1024)
    );

    let mut buf = [0u8; SECTOR_SIZE];
    if !read_sector(0, &mut buf) {
        crate::kprintln!("rv: virtio-blk read sector 0 FAILED");
        return;
    }
    crate::kprint!("rv: blk sector 0 first 16 bytes:");
    for b in &buf[..16] {
        crate::kprint!(" {:02x}", b);
    }
    crate::kprintln!();

    let scratch = (cap - 1) as usize;
    let mut wbuf = [0u8; SECTOR_SIZE];
    for (i, b) in wbuf.iter_mut().enumerate() {
        *b = (i as u8) ^ 0xa5;
    }
    let mut rbuf = [0u8; SECTOR_SIZE];
    let ok = {
        let mut guard = BLK.lock();
        let blk = guard.0.as_mut().unwrap();
        blk.write_blocks(scratch, &wbuf).is_ok() && blk.read_blocks(scratch, &mut rbuf).is_ok()
    };
    if ok && rbuf == wbuf {
        crate::kprintln!("rv: virtio-blk write/read round-trip PASS (sector {})", scratch);
    } else {
        crate::kprintln!("rv: virtio-blk write/read round-trip FAILED");
    }
}
