//! RAM disk: a block device backed by a boot-loaded image in memory.
//!
//! WHY THIS EXISTS. On real hardware the kernel can only mount a filesystem
//! from a device it has a driver for, and the driver list is short: virtio-blk
//! (QEMU) and NVMe. A machine with only SATA, or one where the operator does not
//! want to give pagh a whole disk, had nothing to mount: the boot log said
//! `fs (no disk)` and the system came up on ramfs alone — a shell with no
//! programs, because everything lives in the ext2 image at `/mnt`.
//!
//! Limine can load arbitrary files next to the kernel and hand them to it in
//! memory, so the image the system runs from can travel on the same USB stick or
//! ISO as `pagh.elf`. This module is the `BlockDevice` half of that: a 512-byte
//! sector view over one such buffer. `boot.rs` finds the module whose name ends
//! in `.img` and registers it, and the ordinary ext2 mount path takes it from
//! there — no filesystem code changes at all.
//!
//! WRITES GO TO MEMORY. The buffer is the Limine module, which the PMM keeps
//! reserved (it is part of the kernel-and-modules memmap region), so writes are
//! safe but do not survive a reboot: a RAM-disk boot is a live system whose
//! changes are discarded, like a live CD. Persisting them needs a writable
//! medium, which means a real storage driver (NVMe today, USB mass storage
//! later).
//!
//! The WAL on such a volume is therefore NOT a durability mechanism — the
//! medium dies with the boot and the image on the boot medium is never
//! modified, so there is nothing to replay on the next start. It still earns its
//! keep as intra-session atomicity (a mutation interrupted halfway leaves the
//! live tree consistent), and `fs/README.md` states the distinction rather than
//! letting "WAL" imply "safe across power loss" where it cannot be.

use alloc::sync::Arc;

use crate::drivers::BlockDevice;
use crate::sync::spinlock::Spinlock;

/// A block device over a caller-owned byte buffer.
///
/// The buffer outlives every use: it is the Limine module, reserved by the PMM
/// for the whole life of the kernel. The raw pointer (rather than a
/// `&'static mut [u8]`) keeps `read_block`/`write_block` — which take `&self` —
/// honest about the fact that both directions touch the same bytes.
pub struct MemDisk {
    name: &'static str,
    /// Start of the backing buffer (identity-mapped through the HHDM).
    base: usize,
    /// Buffer length in bytes; sectors are `len / 512`.
    len: usize,
}

// SAFETY: the backing buffer is kernel-owned memory that lives for the whole
// kernel lifetime, and every access is a bounds-checked byte copy. The
// `Spinlock` below serialises the accesses themselves.
unsafe impl Send for MemDisk {}
// SAFETY: as above — no interior state other than the immutable geometry.
unsafe impl Sync for MemDisk {}

/// The registered RAM disk, if a boot module provided one.
static MEMDISK: Spinlock<Option<Arc<MemDisk>>> = Spinlock::new(None);

/// Sector size of every `BlockDevice` in this kernel.
const SECTOR: usize = 512;

impl MemDisk {
    /// Wrap `base`/`len` as a block device named `name`.
    ///
    /// # Safety
    /// `base` must point at `len` readable and writable bytes that stay valid
    /// (and are not aliased by another `BlockDevice`) for the kernel lifetime.
    pub unsafe fn new(name: &'static str, base: usize, len: usize) -> Self {
        Self { name, base, len }
    }

    /// Sectors in the backing buffer (rounded down: a trailing partial sector is
    /// not addressable).
    pub fn sectors(&self) -> u64 {
        (self.len / SECTOR) as u64
    }

    fn copy_in(&self, offset: usize, out: &mut [u8]) {
        // SAFETY: `offset + out.len()` was bounds-checked by the caller against
        // `self.len`, which describes this allocation.
        unsafe {
            core::ptr::copy_nonoverlapping(
                (self.base + offset) as *const u8,
                out.as_mut_ptr(),
                out.len(),
            );
        }
    }

    fn copy_out(&self, offset: usize, data: &[u8]) {
        // SAFETY: as in `copy_in`; the region is exclusively owned by this device.
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr(),
                (self.base + offset) as *mut u8,
                data.len(),
            );
        }
    }
}

impl BlockDevice for MemDisk {
    fn name(&self) -> &str {
        self.name
    }

    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<usize, ()> {
        let offset = (block as usize).checked_mul(SECTOR).ok_or(())?;
        let end = offset.checked_add(buf.len()).ok_or(())?;
        if end > self.len {
            return Err(());
        }
        self.copy_in(offset, buf);
        Ok(buf.len())
    }

    fn write_block(&self, block: u64, buf: &[u8]) -> Result<usize, ()> {
        let offset = (block as usize).checked_mul(SECTOR).ok_or(())?;
        let end = offset.checked_add(buf.len()).ok_or(())?;
        if end > self.len {
            return Err(());
        }
        self.copy_out(offset, buf);
        Ok(buf.len())
    }

    fn sector_count(&self) -> u64 {
        self.sectors()
    }
}

/// Wrap a boot module's buffer and register it as a block device.
///
/// Returns the sector count so the caller can log what it is about to mount, or
/// `0` when the buffer is too small to hold a single sector.
///
/// # Safety
/// `base`/`len` must describe a buffer that stays valid for the kernel
/// lifetime and is not aliased by another `BlockDevice` — the Limine module
/// region satisfies both (the PMM keeps it reserved).
pub unsafe fn register(name: &'static str, base: usize, len: usize) -> u64 {
    let disk = Arc::new(unsafe { MemDisk::new(name, base, len) });
    let sectors = disk.sectors();
    if sectors == 0 {
        return 0;
    }
    crate::drivers::register_block(disk.clone());
    *MEMDISK.lock() = Some(disk);
    sectors
}

/// Total sectors of the registered RAM disk, or `0` when there is none.
pub fn sectors() -> u64 {
    MEMDISK.lock().as_ref().map(|d| d.sectors()).unwrap_or(0)
}

/// The registered RAM disk, if a boot module provided one.
pub fn device() -> Option<Arc<MemDisk>> {
    MEMDISK.lock().clone()
}
