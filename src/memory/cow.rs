// memory/cow.rs — copy-on-write frame ownership accounting
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// This module is deliberately `core` + `alloc` only (no paging, no locks): the
// `host-tests` crate includes it with `#[path]` so the ownership invariant below
// is property-tested on the host against the exact source the kernel compiles.
// `pmm.rs` wraps one [`CowRefs`] in its spinlock.
//
// ## Invariant
//
// A frame that is mapped by more than one user address space as a COW leaf
// carries one reference per address space that maps it. The frame may be
// returned to the free bitmap only when the LAST reference is released; until
// then every owner still has a live PTE pointing at it.
//
// `fork_user_space_cow` registers the sharers (`acquire`), and every path that
// detaches a COW leaf (`cow_copy_page`, `munmap`, `mprotect(PROT_NONE)`,
// `MAP_FIXED` replacement, address-space teardown) must call `release` and free
// the frame only when it returns `true`. Missing that call is the F-r2-arch-1
// use-after-free: a frame returned to the PMM while another address space still
// maps it writable, later handed out as a page table.

use alloc::collections::BTreeMap;

/// Architectural page size — frames are keyed by their page-aligned base.
const FRAME_SIZE: u64 = 4096;

/// Map of COW-shared frame -> number of address spaces referencing it.
///
/// Frames enter on fork and leave on every leaf detach/copy/teardown. Frames
/// with a single private owner are NOT tracked: the plain alloc/free protocol
/// stays the fast path, and only frames that are actually shared pay the map
/// lookup.
pub struct CowRefs {
    map: BTreeMap<u64, u32>,
}

impl CowRefs {
    /// Empty accounting table (const so it can initialize a static spinlock).
    pub const fn new() -> Self {
        Self {
            map: BTreeMap::new(),
        }
    }

    /// Register one address space as a sharer of `frame`. Returns the new share
    /// count. The address is page-aligned before keying so a caller that passes
    /// an offset (e.g. a translated `virt_to_phys`) still hits the same entry.
    pub fn acquire(&mut self, frame: u64) -> u32 {
        let e = self.map.entry(frame & !(FRAME_SIZE - 1)).or_insert(0);
        *e += 1;
        *e
    }

    /// Drop one sharer of `frame`.
    ///
    /// Returns `true` when the caller MUST return the frame to the free bitmap:
    /// this was the last reference. Returns `false` while at least one address
    /// space still maps the frame.
    ///
    /// A frame that is not in the table is treated as privately owned
    /// (`true`): callers reach this only after checking the PTE's COW marker,
    /// so an untracked frame means accounting was already lost and freeing it
    /// matches the historical private path. Every kernel caller is expected to
    /// hold a PTE that keeps the frame alive in that case.
    pub fn release(&mut self, frame: u64) -> bool {
        let key = frame & !(FRAME_SIZE - 1);
        match self.map.get_mut(&key) {
            Some(1) => {
                self.map.remove(&key);
                true
            }
            Some(n) => {
                *n -= 1;
                false
            }
            None => true,
        }
    }

    /// Current number of sharers of `frame` (0 when privately owned/untracked).
    pub fn refs(&self, frame: u64) -> u32 {
        self.map
            .get(&(frame & !(FRAME_SIZE - 1)))
            .copied()
            .unwrap_or(0)
    }

    /// Number of frames currently tracked as COW-shared.
    pub fn tracked_frames(&self) -> usize {
        self.map.len()
    }
}

impl Default for CowRefs {
    fn default() -> Self {
        Self::new()
    }
}
