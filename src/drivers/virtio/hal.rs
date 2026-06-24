// drivers/virtio/hal.rs — `virtio_drivers::Hal` implementation for pagh
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// `PaghHal` bridges the `virtio-drivers` crate's hardware-abstraction layer to
// pagh's physical memory manager (`pmm`) and virtual memory manager (`vmm`).
// The crate uses this trait for every DMA allocation, MMIO BAR mapping, and
// buffer share/unshare during virtqueue operation. `PhysAddr` in
// virtio-drivers 0.11 is `usize`.
//
// DMA physical-contiguity (R4): all of physical RAM is identity-mapped into the
// higher-half via the HHDM, but the kernel heap (`memory::heap`) maps *one
// independently-allocated physical frame per virtual page*. A buffer handed to
// `share` is therefore *virtually* contiguous but may be *physically*
// fragmented when it crosses a page boundary. Returning `virt_to_phys` of only
// the first byte would make the device DMA into unrelated physical frames past
// the first page. To stay correct, `share` checks whether the buffer's spanned
// pages are physically contiguous; if not, it allocates a physically-contiguous
// bounce buffer, copies the bytes per `BufferDirection`, and hands the device
// the bounce physical base (whose first byte is the buffer's first byte).
// `unshare` reconciles the bytes back to the kernel buffer for device-written
// directions and frees the bounce frames. Physically-contiguous buffers (the
// single-page / already-contiguous case, including `dma_alloc` regions) take
// the unchanged direct path with no bounce allocation.

use core::ptr::NonNull;

use alloc::collections::BTreeMap;

use virtio_drivers::{BufferDirection, Hal, PhysAddr};

use crate::memory::{pmm, vmm};
use crate::sync::spinlock::Spinlock;

/// Page size used for physical-contiguity reasoning and bounce sizing.
const PAGE_SIZE: u64 = 4096;

/// Bookkeeping for one in-flight bounce buffer, keyed in [`BOUNCE`] by the
/// device-visible physical base returned from [`PaghHal::share`].
struct BounceRecord {
    /// Kernel virtual base of the original (fragmented) buffer.
    orig_ptr: *mut u8,
    /// Original buffer length in bytes.
    len: usize,
    /// Physically-contiguous frames handed to the device.
    bounce_paddr: u64,
    /// Number of frames backing the bounce buffer.
    pages: usize,
    /// Transfer direction, deciding which way bytes are reconciled.
    direction: BufferDirection,
}

// SAFETY: `orig_ptr` is a kernel virtual address owned by the driver that
// shared the buffer. The single-owner share/unshare discipline guarantees the
// record is produced and consumed by the paired `share`/`unshare` calls and is
// never used to alias the buffer concurrently, so moving the record (and its
// pointer) across the spinlock boundary is sound.
unsafe impl Send for BounceRecord {}

/// Registry of active bounce buffers, keyed by the bounce physical base. Only
/// fragmented buffers are recorded here; the contiguous direct path inserts
/// nothing, so the map stays bounded by the number of in-flight bounced
/// buffers.
static BOUNCE: Spinlock<BTreeMap<u64, BounceRecord>> = Spinlock::new(BTreeMap::new());

/// The pagh implementation of `virtio_drivers::Hal`.
///
/// Zero-sized: all state lives in the global `pmm`/`vmm` and the [`BOUNCE`]
/// registry. Usable as the `H` type parameter for `VirtIOBlk<PaghHal, _>` /
/// `VirtIONet<PaghHal, _>`.
pub struct PaghHal;

/// Number of physical pages a `[vaddr, vaddr + len)` range spans.
fn pages_spanned(vaddr: u64, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let first_page = vaddr & !(PAGE_SIZE - 1);
    let last_page = (vaddr + len as u64 - 1) & !(PAGE_SIZE - 1);
    ((last_page - first_page) / PAGE_SIZE) as usize + 1
}

/// Whether the `len` bytes starting at `vaddr` are backed by physically
/// contiguous frames: each spanned page's `virt_to_phys` must equal the first
/// spanned page's physical base plus `page_index * PAGE_SIZE`. Sub-page and
/// single-page buffers are always contiguous.
fn is_phys_contiguous(vaddr: u64, len: usize) -> bool {
    let spanned = pages_spanned(vaddr, len);
    if spanned <= 1 {
        return true;
    }
    let first_page = vaddr & !(PAGE_SIZE - 1);
    let base = match vmm::virt_to_phys(first_page) {
        Some(p) => p,
        None => return false,
    };
    for i in 1..spanned as u64 {
        match vmm::virt_to_phys(first_page + i * PAGE_SIZE) {
            Some(p) if p == base + i * PAGE_SIZE => {}
            _ => return false,
        }
    }
    true
}

// SAFETY: the methods below uphold the `Hal` implementation-safety contract:
// `dma_alloc` returns a page-aligned, zeroed, physically-contiguous region of
// `pages * 4096` bytes whose virtual pointer is the unique HHDM mapping of the
// returned physical address (so it aliases no other live allocation until
// `dma_dealloc`); `mmio_phys_to_virt` returns the unique HHDM mapping of an
// MMIO BAR; `share`/`unshare` give the device a physically-contiguous view of
// the buffer (directly when already contiguous, otherwise via a bounce buffer)
// and reconcile the bytes on share and unshare.
unsafe impl Hal for PaghHal {
    /// Allocate `pages` physically-contiguous frames, zero them, and return the
    /// physical base plus the HHDM virtual pointer (`vaddr == phys_to_virt(paddr)`).
    ///
    /// Allocation failure is boot-fatal (the device cannot operate without its
    /// DMA region), so we `expect` rather than return a sentinel — consistent
    /// with the kernel's existing boot-time fatal handling.
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        let paddr = pmm::alloc_frames_contiguous(pages)
            .expect("PaghHal::dma_alloc: out of contiguous DMA frames");

        let vaddr = vmm::phys_to_virt(paddr);

        // Zero the whole region (the Hal contract requires zeroed pages).
        // SAFETY: `vaddr` is the HHDM mapping of `pages` freshly-allocated
        // contiguous frames we now exclusively own, so the `pages * 4096`-byte
        // range is valid, writable, and unaliased.
        unsafe {
            core::ptr::write_bytes(vaddr as *mut u8, 0, pages * 4096);
        }

        let ptr = NonNull::new(vaddr as *mut u8)
            .expect("PaghHal::dma_alloc: phys_to_virt produced a null pointer");
        (paddr as PhysAddr, ptr)
    }

    /// Free a region previously returned by [`Self::dma_alloc`].
    ///
    /// # Safety
    /// `paddr`/`pages` must be the values returned by a prior `dma_alloc` that
    /// has not yet been freed.
    unsafe fn dma_dealloc(paddr: PhysAddr, _vaddr: NonNull<u8>, pages: usize) -> i32 {
        pmm::free_frames_contiguous(paddr as u64, pages);
        0
    }

    /// Translate an MMIO BAR physical address to a higher-half virtual pointer.
    ///
    /// MMIO BARs are reached through the HHDM window, but the HHDM only
    /// *direct-maps* the RAM Limine reported (typically up to a few GiB). QEMU
    /// places virtio devices' 64-bit memory BARs in the high PCI MMIO window
    /// *above* the top of the HHDM, so those pages are not present in the page
    /// tables even though their HHDM virtual address is well-defined. We
    /// therefore call `vmm::map_mmio`, which page-maps the region at its HHDM
    /// virtual address (`phys_to_virt(paddr)`) with `NO_CACHE | NO_EXECUTE` and
    /// returns that base. For a BAR already inside the direct-mapped window this
    /// simply re-establishes the same mapping; for a high BAR it creates the
    /// missing entries. Mapping failure is boot-fatal (the device cannot operate
    /// without its registers), consistent with the kernel's boot-time handling.
    ///
    /// # Safety
    /// `paddr`/`size` must describe a valid MMIO region of the device.
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
        let vaddr = vmm::map_mmio(paddr as u64, size as u64)
            .expect("PaghHal::mmio_phys_to_virt: failed to map MMIO BAR region");
        NonNull::new(vaddr as *mut u8)
            .expect("PaghHal::mmio_phys_to_virt: map_mmio produced a null pointer")
    }

    /// Share a kernel buffer with the device and return a device-visible
    /// physical base whose first byte corresponds to the buffer's first byte.
    ///
    /// If the buffer's spanned pages are physically contiguous (always true for
    /// single-page / sub-page buffers and for `dma_alloc` regions) the device
    /// can DMA directly, so we return `virt_to_phys` of the first byte. If the
    /// buffer is physically fragmented (the normal kernel-heap case once it
    /// crosses a page boundary), we allocate a physically-contiguous bounce
    /// buffer of `ceil(len / 4096)` frames, copy the bytes into it for
    /// device-readable directions, record it in [`BOUNCE`], and return the
    /// bounce physical base instead.
    ///
    /// # Safety
    /// `buffer` must be a valid pointer to a non-empty memory range not
    /// accessed by any other thread for the duration of the call.
    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> PhysAddr {
        let ptr = buffer.cast::<u8>().as_ptr();
        let vaddr = ptr as u64;
        let len = buffer.len();

        // Direct path: empty or physically-contiguous buffers DMA in place.
        if len == 0 || is_phys_contiguous(vaddr, len) {
            let paddr = vmm::virt_to_phys(vaddr)
                .expect("PaghHal::share: buffer virtual address is not mapped");
            return paddr as PhysAddr;
        }

        // Bounce path: allocate contiguous frames covering the whole length.
        let pages = ((len as u64 + PAGE_SIZE - 1) / PAGE_SIZE) as usize;
        let bounce_paddr = pmm::alloc_frames_contiguous(pages)
            .expect("PaghHal::share: out of contiguous bounce-buffer frames");
        let bounce_vaddr = vmm::phys_to_virt(bounce_paddr);

        // Copy kernel -> bounce for directions the device reads from.
        match direction {
            BufferDirection::DriverToDevice | BufferDirection::Both => {
                // SAFETY: `ptr` is the caller's valid `len`-byte buffer and
                // `bounce_vaddr` is the HHDM mapping of `pages` freshly-owned
                // contiguous frames (>= `len` bytes); the regions live in
                // distinct physical frames, so they do not overlap.
                unsafe {
                    core::ptr::copy_nonoverlapping(ptr, bounce_vaddr as *mut u8, len);
                }
            }
            BufferDirection::DeviceToDriver => {}
        }

        BOUNCE.lock().insert(
            bounce_paddr,
            BounceRecord {
                orig_ptr: ptr,
                len,
                bounce_paddr,
                pages,
                direction,
            },
        );

        bounce_paddr as PhysAddr
    }

    /// Unshare a buffer previously shared with the device.
    ///
    /// For a buffer that took the direct (contiguous) path there is no registry
    /// entry and nothing to do. For a bounced buffer we look up its record,
    /// copy the bounce contents back into the kernel buffer for device-written
    /// directions (`DeviceToDriver`/`Both`), and free the bounce frames.
    ///
    /// # Safety
    /// `paddr` must be the value returned by the corresponding `share` call.
    unsafe fn unshare(paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {
        // Remove (and release the lock) before doing the copy / free work.
        let record = BOUNCE.lock().remove(&(paddr as u64));
        let record = match record {
            Some(r) => r,
            None => return, // direct (contiguous) path: nothing to reconcile.
        };

        let bounce_vaddr = vmm::phys_to_virt(record.bounce_paddr);

        // Copy bounce -> kernel for directions the device writes into.
        match record.direction {
            BufferDirection::DeviceToDriver | BufferDirection::Both => {
                // SAFETY: `bounce_vaddr` is the HHDM mapping of the contiguous
                // frames recorded at `share` (>= `record.len` bytes) and
                // `record.orig_ptr` is the original `record.len`-byte buffer;
                // they occupy distinct physical frames, so they do not overlap.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        bounce_vaddr as *const u8,
                        record.orig_ptr,
                        record.len,
                    );
                }
            }
            BufferDirection::DriverToDevice => {}
        }

        pmm::free_frames_contiguous(record.bounce_paddr, record.pages);
    }
}
