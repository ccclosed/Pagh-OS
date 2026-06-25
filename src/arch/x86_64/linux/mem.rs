//! Memory-management syscall planning and effectful handlers.
//!
//! This task (3.1) implements only the **pure** planning logic for `brk`, `mmap`,
//! `munmap`, and the shared protection-flag mapping: allocation-free, `core`-only
//! functions that compute outcomes without touching the VMM/PMM, the per-process
//! `VmRegionSet`, hardware, or global mutable state (R11.6). The effectful handlers
//! that wire these plans to real page tables land in task 12.3.
//!
//! The `Errno` type is shared from the sibling `errno` module via `super::errno`,
//! and `USER_ADDR_MAX` from `super::validate`, so the same source compiles both in
//! the kernel (`crate::arch::x86_64::linux::{errno, validate}`) and when included
//! into the `host-tests` crate as crate-root sibling modules (R11.6).
#![allow(dead_code)]

use super::errno::Errno;
use super::validate::USER_ADDR_MAX;

/// Page size used for all rounding and page-count arithmetic (x86_64 4 KiB pages).
const PAGE_SIZE: u64 = 4096;

/// `mmap`/`mprotect` protection bit: the region may be read.
pub const PROT_READ: u32 = 1;
/// `mmap`/`mprotect` protection bit: the region may be written.
pub const PROT_WRITE: u32 = 2;
/// `mmap`/`mprotect` protection bit: the region may be executed.
pub const PROT_EXEC: u32 = 4;

/// `mmap` flag: the mapping is private (copy-on-write, not shared).
pub const MAP_PRIVATE: u32 = 0x2;
/// `mmap` flag: the mapping is not backed by any file (zero-filled).
pub const MAP_ANONYMOUS: u32 = 0x20;

/// A previously-mapped anonymous region, as tracked by the per-process
/// `VmRegionSet`. Used by [`plan_munmap`] to confirm a requested range is covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MmapRegion {
    /// 4 KiB-aligned base virtual address of the region.
    pub base: u64,
    /// Number of 4 KiB pages in the region.
    pub pages: u64,
    /// Whether the region's pages carry the writable bit (`PROT_WRITE`).
    pub writable: bool,
    /// Whether the region's pages carry the no-execute bit (`!PROT_EXEC`).
    pub nx: bool,
}

/// Outcome of a pure `brk` planning step (R3.1–R3.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrkOutcome {
    /// The break is unchanged; the syscall reports this value in `rax`.
    Unchanged(u64),
    /// The break grows to `new_brk`; pages over `[map_from, map_to)` must be mapped.
    Grow {
        /// The new program break, also the value reported in `rax`.
        new_brk: u64,
        /// Page-aligned (down) start of the range that must be backed by pages.
        map_from: u64,
        /// Page-aligned (up) end of the range that must be backed by pages.
        map_to: u64,
    },
    /// The break shrinks to this value, which is also reported in `rax`.
    Shrink(u64),
}

/// Outcome of a pure `mmap` planning step (R4.1, R4.2, R4.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmapPlan {
    /// The request is invalid; the syscall returns this negated errno.
    Reject(Errno),
    /// Map `pages` zero-filled pages at `base` with the given protection bits.
    Map {
        /// 4 KiB-aligned base virtual address of the new mapping.
        base: u64,
        /// Number of 4 KiB pages to map (length rounded up).
        pages: u64,
        /// Whether the pages carry the writable bit (`PROT_WRITE` requested).
        writable: bool,
        /// Whether the pages carry the no-execute bit (`PROT_EXEC` not requested).
        nx: bool,
    },
}

/// Outcome of a pure `munmap` planning step (R4.3, R4.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MunmapPlan {
    /// The request is invalid; the syscall returns this negated errno.
    Reject(Errno),
    /// Unmap `pages` pages starting at `base`.
    Unmap {
        /// 4 KiB-aligned base virtual address to unmap.
        base: u64,
        /// Number of 4 KiB pages to unmap (length rounded up).
        pages: u64,
    },
}

/// Round `addr` down to its containing page base.
#[inline]
fn page_down(addr: u64) -> u64 {
    addr & !(PAGE_SIZE - 1)
}

/// Round `addr` up to the next page boundary, saturating at `u64::MAX`.
///
/// Callers only pass values already known to be below `USER_ADDR_MAX`, so the
/// saturating add never actually saturates in practice; it keeps the helper total.
#[inline]
fn page_up(addr: u64) -> u64 {
    page_down(addr.saturating_add(PAGE_SIZE - 1))
}

/// Compute `ceil(len / PAGE_SIZE)` without overflowing.
///
/// Returns `None` only if `len + (PAGE_SIZE - 1)` would overflow `u64`, which a
/// caller treats as an invalid request.
#[inline]
fn pages_for_len(len: u64) -> Option<u64> {
    len.checked_add(PAGE_SIZE - 1).map(|n| n / PAGE_SIZE)
}

/// Map a protection bitmask to the `(writable, nx)` page-attribute pair (R4.2,
/// R4.5, and — via `PF_W`/`PF_X` — R5.4).
///
/// * `writable` is set iff `PROT_WRITE` is present.
/// * `nx` (no-execute) is set iff `PROT_EXEC` is **absent**.
///
/// This single mapping is reused by `mmap`, `mprotect`, and the ELF loader, so the
/// writable/no-execute policy stays identical across all three (Property 14).
pub fn prot_to_flags(prot: u32) -> (bool, bool) {
    let writable = prot & PROT_WRITE != 0;
    let nx = prot & PROT_EXEC == 0;
    (writable, nx)
}

/// Plan a `brk` transition from `current` toward `requested`, given the process's
/// `initial` break, without mapping anything (R3.1–R3.6, Property 11).
///
/// The branches, in order:
///   * `requested == 0` → [`BrkOutcome::Unchanged`] (query the current break, R3.1).
///   * `requested >= USER_ADDR_MAX` → [`BrkOutcome::Unchanged`] (R3.5).
///   * `requested < initial` → [`BrkOutcome::Unchanged`] (below the floor, R3.6).
///   * `initial <= requested <= current` → [`BrkOutcome::Shrink`] (R3.3).
///   * `current < requested < USER_ADDR_MAX` → [`BrkOutcome::Grow`] mapping the page
///     span `[page_down(current), page_up(requested))` (R3.2).
pub fn plan_brk(initial: u64, current: u64, requested: u64) -> BrkOutcome {
    if requested == 0 || requested >= USER_ADDR_MAX || requested < initial {
        return BrkOutcome::Unchanged(current);
    }
    if requested <= current {
        // initial <= requested <= current
        BrkOutcome::Shrink(requested)
    } else {
        // current < requested < USER_ADDR_MAX
        BrkOutcome::Grow {
            new_brk: requested,
            map_from: page_down(current),
            map_to: page_up(requested),
        }
    }
}

/// Plan an anonymous `mmap` of `len` bytes with protection `prot` and `flags` at the
/// pre-aligned `hint_base`, without mapping anything (R4.1, R4.2, R4.6, Property 12).
///
/// Returns [`MmapPlan::Reject`] with `EINVAL` when:
///   * `len == 0`, or
///   * `flags` are not exactly `MAP_ANONYMOUS | MAP_PRIVATE`, or
///   * `fd != -1`, or
///   * the page count for `len` would overflow.
///
/// Otherwise returns [`MmapPlan::Map`] with `pages = ceil(len / 4096)`, the supplied
/// (assumed 4 KiB-aligned) `hint_base`, and `(writable, nx)` from [`prot_to_flags`].
pub fn plan_mmap(len: u64, prot: u32, flags: u32, fd: i64, hint_base: u64) -> MmapPlan {
    if len == 0 || flags != (MAP_ANONYMOUS | MAP_PRIVATE) || fd != -1 {
        return MmapPlan::Reject(Errno::EINVAL);
    }
    let pages = match pages_for_len(len) {
        Some(p) => p,
        None => return MmapPlan::Reject(Errno::EINVAL),
    };
    let (writable, nx) = prot_to_flags(prot);
    MmapPlan::Map {
        base: hint_base,
        pages,
        writable,
        nx,
    }
}

/// Plan a `munmap` of `[base, base + len)` against the set of previously-mapped
/// `regions`, without unmapping anything (R4.3, R4.7, Property 13).
///
/// Returns [`MunmapPlan::Reject`] with `EINVAL` when:
///   * `base` is not 4 KiB-aligned, or
///   * `len == 0`, or
///   * the page count / range arithmetic would overflow, or
///   * any 4 KiB page in the range is not covered by some region in `regions`.
///
/// Otherwise returns [`MunmapPlan::Unmap`] with `pages = ceil(len / 4096)`.
pub fn plan_munmap(base: u64, len: u64, regions: &[MmapRegion]) -> MunmapPlan {
    if base & (PAGE_SIZE - 1) != 0 || len == 0 {
        return MunmapPlan::Reject(Errno::EINVAL);
    }
    let pages = match pages_for_len(len) {
        Some(p) => p,
        None => return MunmapPlan::Reject(Errno::EINVAL),
    };
    // Every page in [base, base + pages*4096) must be covered by some region.
    for i in 0..pages {
        let page = match i
            .checked_mul(PAGE_SIZE)
            .and_then(|off| base.checked_add(off))
        {
            Some(p) => p,
            None => return MunmapPlan::Reject(Errno::EINVAL),
        };
        if !regions.iter().any(|r| region_contains_page(r, page)) {
            return MunmapPlan::Reject(Errno::EINVAL);
        }
    }
    MunmapPlan::Unmap { base, pages }
}

/// Whether the 4 KiB page at `page` lies within `region`'s `[base, base + pages*4096)`.
#[inline]
fn region_contains_page(region: &MmapRegion, page: u64) -> bool {
    let span = match region.pages.checked_mul(PAGE_SIZE) {
        Some(s) => s,
        None => return false,
    };
    let end = match region.base.checked_add(span) {
        Some(e) => e,
        None => return false,
    };
    page >= region.base && page < end
}

/// Per-`Compat_Process` virtual-memory bookkeeping: the program break plus the
/// set of anonymous `mmap` regions (design "MmapRegion / VM tracking").
///
/// This is the state the effectful `brk`/`mmap`/`munmap`/`mprotect` handlers
/// (task 12.3) mutate after consulting the pure `plan_*` planners above; it holds
/// no hardware state and stays `core` + `alloc` only so this module remains
/// host-testable (R11.6). A populated instance is seeded by `run_linux_binary`
/// (task 13.3) from the loader's `initial_brk` and the lower-half mmap hint base.
#[derive(Clone, Debug)]
pub struct VmRegionSet {
    /// The program break at process start (the floor `brk` may not drop below).
    pub initial_brk: u64,
    /// The current program break (top of the heap region).
    pub current_brk: u64,
    /// Anonymous `mmap` regions currently mapped for the process.
    pub mmaps: alloc::vec::Vec<MmapRegion>,
    /// Bump-pointer hint for the next anonymous `mmap` base in the user range.
    pub mmap_next_hint: u64,
}

impl VmRegionSet {
    /// Create an empty region set for a process whose initial (and current)
    /// program break is `initial_brk` and whose anonymous `mmap` allocations
    /// start bumping from `mmap_hint_base`.
    pub fn new(initial_brk: u64, mmap_hint_base: u64) -> Self {
        Self {
            initial_brk,
            current_brk: initial_brk,
            mmaps: alloc::vec::Vec::new(),
            mmap_next_hint: mmap_hint_base,
        }
    }
}
