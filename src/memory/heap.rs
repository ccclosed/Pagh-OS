// memory/heap.rs — Kernel heap / global allocator.
// 64-bit x86_64 Limine kernel in Rust (#![no_std])
//
// The kernel uses `good_memory_allocator` (galloc) as its `#[global_allocator]`
// so the `alloc` types (Vec, Box, Arc, ...) work.
//
// WHY NOT linked_list_allocator: that allocator is a pure first-fit free list,
// O(n) per allocation in the number of free blocks. Under the `apt` package
// index parser — which allocates and frees tens of thousands of small objects
// (a per-stanza `BTreeMap<String,String>` plus its `String`s, ~10 per stanza ×
// ~60k stanzas) — the free list grows huge and the per-alloc scan degrades to
// roughly O(n²). That showed up as a multi-minute "hang" parsing a large index
// and was the leading suspect for heap corruption at scale. galloc keeps a free
// list but adds size-binned "smallbins" (dlmalloc-style), giving ~O(1) typical
// allocate/free, so the churn stays linear.
//
// The heap is a FIXED-SIZE region: `init()` maps `HEAP_INITIAL_PAGES` pages
// starting at `layout::heap_base()` and hands that contiguous region to the
// allocator. galloc does not grow on demand — when an allocation cannot be
// satisfied it returns a null pointer (Requirement 10.4), which the `alloc`
// machinery turns into an allocation-error abort. If the kernel ever needs a
// larger heap, raise `HEAP_INITIAL_PAGES` in `memory::layout`.

use core::sync::atomic::{AtomicUsize, Ordering};

use good_memory_allocator::SpinLockedAllocator;

/// The global allocator instance (Requirement 10.1). Declared exactly once.
#[global_allocator]
static ALLOCATOR: SpinLockedAllocator = SpinLockedAllocator::empty();

/// Total bytes handed to the allocator at [`init`], recorded for [`stats`].
static HEAP_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Initialize the kernel heap.
///
/// Derives the heap base/size from `memory::layout` (Requirement 4.3), maps the
/// backing physical frames, then initializes the allocator over that region.
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
    // valid for the `'static` lifetime (kernel heap is never torn down). `init`
    // must be called exactly once, before any allocation; boot calls it in the
    // ordered init sequence before any heap user runs.
    unsafe {
        ALLOCATOR.init(heap_base as usize, heap_size as usize);
    }
    HEAP_SIZE.store(heap_size as usize, Ordering::Relaxed);

    crate::debug!(
        "Kernel heap initialized: 0x{:x}..0x{:x} ({} KB, fixed, galloc)",
        heap_base,
        heap_base + heap_size,
        heap_size / 1024
    );
}

/// Report the kernel heap accounting as `(size, used, free)` bytes.
///
/// DIAGNOSTIC helper (apt-update parse-stage crash investigation): used by the
/// `lx_bigindex` self-test and `apt::update`'s feature-gated progress logging.
/// galloc does not expose live used/free counters, so only the total configured
/// size is reported (`used`/`free` are 0); the heap-exhaustion question this was
/// added to answer has already been settled (the arena at ~4 MiB decompressed is
/// far under the 256 MiB heap). Kept with a stable signature so its callers
/// compile unchanged.
pub fn stats() -> (usize, usize, usize) {
    let size = HEAP_SIZE.load(Ordering::Relaxed);
    (size, 0, 0)
}
