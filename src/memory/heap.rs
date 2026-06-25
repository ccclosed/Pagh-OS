// memory/heap.rs — Kernel heap / global allocator.
// 64-bit x86_64 Limine kernel in Rust (#![no_std])
//
// The kernel uses the `linked_list_allocator` crate's `LockedHeap` as its
// `#[global_allocator]` so the `alloc` types (Vec, Box, Arc, ...) work. This
// replaces the previous hand-rolled free-list allocator (Requirements 10.1,
// 10.2).
//
// The heap is a FIXED-SIZE region: `init()` maps `HEAP_INITIAL_PAGES` pages
// starting at `layout::heap_base()` and hands that contiguous region to the
// allocator. `LockedHeap` does not grow on demand — when an allocation cannot
// be satisfied it returns a null pointer (Requirement 10.4), which the `alloc`
// machinery turns into an allocation-error abort. If the kernel ever needs a
// larger heap, raise `HEAP_INITIAL_PAGES` in `memory::layout` rather than
// reintroducing custom growth logic.

use linked_list_allocator::LockedHeap;

/// The global allocator instance (Requirement 10.1). Declared exactly once.
#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// Initialize the kernel heap.
///
/// Derives the heap base/size from `memory::layout` (Requirement 4.3), maps the
/// backing physical frames, then initializes the `LockedHeap` over that region.
/// The heap is boot-critical, so mapping failures panic.
pub fn init() {
    let heap_base = crate::memory::layout::heap_base();
    let initial_pages = crate::memory::layout::HEAP_INITIAL_PAGES;
    let heap_size = initial_pages * crate::memory::layout::PAGE_SIZE;

    // Map the initial heap pages: one physical frame per page, mapped W^X
    // (writable, never executable).
    let mut addr = heap_base;
    for _ in 0..initial_pages {
        let frame = crate::memory::pmm::alloc_frame()
            .expect("PMM: failed to allocate frame for kernel heap");
        let flags = x86_64::structures::paging::PageTableFlags::PRESENT
            | x86_64::structures::paging::PageTableFlags::WRITABLE
            | x86_64::structures::paging::PageTableFlags::NO_EXECUTE;
        crate::memory::vmm::map(frame, addr, flags)
            .expect("VMM: failed to map kernel heap page");
        addr += crate::memory::layout::PAGE_SIZE;
    }

    // Hand the freshly mapped region to the allocator.
    //
    // SAFETY: the region `[heap_base, heap_base + heap_size)` was just mapped
    // above as present + writable and is owned exclusively by the heap. It is
    // valid for the `'static` lifetime (kernel heap is never torn down).
    unsafe {
        ALLOCATOR.lock().init(heap_base as *mut u8, heap_size as usize);
    }

    crate::debug!(
        "Kernel heap initialized: 0x{:x}..0x{:x} ({} KB, fixed)",
        heap_base,
        heap_base + heap_size,
        heap_size / 1024
    );
}

/// Report the kernel heap accounting as `(size, used, free)` bytes.
///
/// DIAGNOSTIC helper (Part B, apt-update parse-stage crash investigation): used
/// by the `lx_bigindex` self-test and `apt::update`'s feature-gated progress
/// logging to watch allocator headroom as the big index is parsed, ruling
/// allocator exhaustion/corruption in or out. `LockedHeap` exposes live
/// `size`/`used`/`free` counters; this just snapshots them under the lock.
pub fn stats() -> (usize, usize, usize) {
    let h = ALLOCATOR.lock();
    (h.size(), h.used(), h.free())
}
