// drivers/virtio/hal.rs — `virtio_drivers::Hal` implementation for pagh
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// `PaghHal` bridges the `virtio-drivers` crate's hardware-abstraction layer to
// pagh's physical memory manager (`pmm`) and virtual memory manager (`vmm`).
// The crate uses this trait for every DMA allocation, MMIO BAR mapping, and
// buffer share/unshare during virtqueue operation. `PhysAddr` in
// virtio-drivers 0.11 is `usize`.
//
// Single-owner buffer discipline (R2.6): the kernel hands a buffer to the
// device via `share`, and the device returns ownership to the driver before
// the buffer is reused. Because all of physical RAM is identity-mapped into the
// higher-half via the HHDM, the device-visible physical address is simply the
// translation of the buffer's kernel virtual address — there are no bounce
// buffers, so `unshare` is a no-op. The driver must not touch a shared buffer
// until the device has returned it (enforced by the higher-level driver, not
// here), which keeps the buffer owned by exactly one side at any time.

use core::ptr::NonNull;

use virtio_drivers::{BufferDirection, Hal, PhysAddr};

use crate::memory::{pmm, vmm};

/// The pagh implementation of `virtio_drivers::Hal`.
///
/// Zero-sized: all state lives in the global `pmm`/`vmm`. Usable as the `H`
/// type parameter for `VirtIOBlk<PaghHal, _>` / `VirtIONet<PaghHal, _>`.
pub struct PaghHal;

// SAFETY: the methods below uphold the `Hal` implementation-safety contract:
// `dma_alloc` returns a page-aligned, zeroed, physically-contiguous region of
// `pages * 4096` bytes whose virtual pointer is the unique HHDM mapping of the
// returned physical address (so it aliases no other live allocation until
// `dma_dealloc`); `mmio_phys_to_virt` returns the unique HHDM mapping of an
// MMIO BAR; `share`/`unshare` honor the single-owner discipline documented
// above.
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

    /// Share a kernel buffer with the device: return its physical address.
    ///
    /// The buffer is ordinary kernel memory reachable through the HHDM, so its
    /// device-visible physical address is `virt_to_phys` of its virtual
    /// pointer. No bounce buffer is allocated (single-owner discipline, R2.6).
    ///
    /// # Safety
    /// `buffer` must be a valid pointer to a non-empty memory range not
    /// accessed by any other thread for the duration of the call.
    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        let vaddr = buffer.cast::<u8>().as_ptr() as u64;
        let paddr = vmm::virt_to_phys(vaddr)
            .expect("PaghHal::share: buffer virtual address is not mapped");
        paddr as PhysAddr
    }

    /// Unshare a buffer previously shared with the device.
    ///
    /// A no-op: we never allocate bounce buffers, so there is nothing to copy
    /// back. Ownership returns to the driver by convention (the device has
    /// finished with the buffer before `unshare` is called).
    ///
    /// # Safety
    /// `paddr` must be the value returned by the corresponding `share` call.
    unsafe fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {
        // No bounce buffers: nothing to do.
    }
}
