//! Kernel heap / global allocator. Uses the same `good_memory_allocator`
//! (galloc) as the x86_64 kernel so the `alloc` types work and the allocator
//! stays size-binned (~O(1)), not a degrading first-fit free list.

use good_memory_allocator::SpinLockedAllocator;

#[global_allocator]
static ALLOCATOR: SpinLockedAllocator = SpinLockedAllocator::empty();

/// Hand the allocator a fixed, already-mapped region `[start, start+size)`.
///
/// # Safety
/// The region must be exclusively owned by the heap, mapped readable/writable
/// (the identity Sv39 map covers all RAM), and `init` must run exactly once
/// before the first allocation.
pub unsafe fn init(start: usize, size: usize) {
    ALLOCATOR.init(start, size);
}
