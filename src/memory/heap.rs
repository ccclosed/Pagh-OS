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

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

use good_memory_allocator::SpinLockedAllocator;

/// Interrupt-safe wrapper around galloc.
///
/// `SpinLockedAllocator`'s internal spin lock does NOT disable interrupts, so
/// the timer could preempt a thread MID-ALLOCATION while it held the heap
/// lock. Any other context that then allocated while holding one of the
/// kernel's interrupt-disabling `Spinlock`s (VFS, ext2, console, ...) would
/// spin on the heap lock with interrupts off -- the descheduled owner could
/// never run again and the whole kernel froze. The apt index parser allocates
/// millions of times, which made first-boot provisioning the reliable
/// trigger.
///
/// Fix: disable interrupts for the (microsecond-scale) duration of every
/// heap operation, exactly like `crate::sync::spinlock::Spinlock` does. The
/// heap-lock owner can then never be preempted mid-hold, so every waiter is
/// spinning against a RUNNING owner and the wait is bounded.
struct IrqSafeAllocator {
    inner: SpinLockedAllocator,
}

impl IrqSafeAllocator {
    #[inline]
    fn guarded<R>(&self, f: impl FnOnce(&SpinLockedAllocator) -> R) -> R {
        let were_enabled = crate::arch::cpu::interrupts_enabled();
        crate::arch::cpu::disable_interrupts();
        let r = f(&self.inner);
        if were_enabled {
            crate::arch::cpu::enable_interrupts();
        }
        r
    }
}

/// Live bytes handed out by successful allocations (each request's size is
/// added on alloc/realloc-grow and subtracted on free/shrink). Diagnostics
/// only (`stats`); it counts requested bytes, not allocator overhead, so the
/// real footprint is slightly higher.
static HEAP_USED: AtomicUsize = AtomicUsize::new(0);

/// Subtract `bytes` from [`HEAP_USED`] without underflowing (a stray/double
/// free must not wrap the counter into astronomic "used" values).
fn heap_used_sub(bytes: usize) {
    let mut cur = HEAP_USED.load(Ordering::Relaxed);
    loop {
        if cur == 0 {
            return;
        }
        match HEAP_USED.compare_exchange_weak(
            cur,
            cur.saturating_sub(bytes),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(now) => cur = now,
        }
    }
}

// SAFETY: delegates to `SpinLockedAllocator` (a correct `GlobalAlloc`); the
// wrapper only adds interrupt masking around each operation and live-byte
// accounting for `stats`.
unsafe impl GlobalAlloc for IrqSafeAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = self.guarded(|a| a.alloc(layout));
        if !ptr.is_null() {
            HEAP_USED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.guarded(|a| a.dealloc(ptr, layout));
        heap_used_sub(layout.size());
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = self.guarded(|a| a.alloc_zeroed(layout));
        if !ptr.is_null() {
            HEAP_USED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = self.guarded(|a| a.realloc(ptr, layout, new_size));
        if !new_ptr.is_null() {
            // On failure galloc leaves the original allocation live, so only
            // a successful resize moves the counter (old size out, new in).
            heap_used_sub(layout.size());
            HEAP_USED.fetch_add(new_size, Ordering::Relaxed);
        }
        new_ptr
    }
}

/// The global allocator instance (Requirement 10.1). Declared exactly once.
#[global_allocator]
static ALLOCATOR: IrqSafeAllocator = IrqSafeAllocator {
    inner: SpinLockedAllocator::empty(),
};

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
        crate::memory::vmm::map(frame, addr, flags).expect("VMM: failed to map kernel heap page");
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
        ALLOCATOR.inner.init(heap_base as usize, heap_size as usize);
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
/// `used` counts the live requested bytes (successful allocations minus
/// frees), so it excludes per-chunk allocator overhead — the real footprint is
/// slightly higher. `free` is the configured heap size minus `used`.
pub fn stats() -> (usize, usize, usize) {
    let size = HEAP_SIZE.load(Ordering::Relaxed);
    let used = HEAP_USED.load(Ordering::Relaxed).min(size);
    (size, used, size - used)
}
