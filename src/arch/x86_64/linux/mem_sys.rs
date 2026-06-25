//! Effectful Linux memory-management syscall handlers (task 12.3).
//!
//! This is the **kernel-only** half of the `mem` component. It wires the pure
//! planners in [`super::mem`] (`plan_brk`/`plan_mmap`/`plan_munmap`/`prot_to_flags`)
//! to the running `Compat_Process`'s [`VmRegionSet`] and to the page tables via
//! `memory::vmm`/`memory::pmm`.
//!
//! It lives in its OWN file (not `mem.rs`) on purpose: `mem.rs` is `#[path]`-included
//! verbatim by the `host-tests` crate so its pure planners can be property-tested on
//! the host (R11.6). These handlers use kernel-only paging APIs that do not exist on
//! the host, so keeping them here leaves `mem.rs` purely host-testable while this file
//! is compiled only as part of the kernel.
//!
//! ## Address space
//!
//! During a syscall the active CR3 is the calling process's user PML4, so
//! `vmm::map`/`vmm::unmap`/`vmm::virt_to_phys` operate directly on that process's
//! address space — no CR3 switch is needed here. PMM/VMM use their own brief
//! spinlocks and never wait on a device interrupt, so it is safe to run this work
//! inside the [`crate::task::compat::with_current_compat`] closure (which holds the
//! `COMPAT_STATES` lock).
//!
//! ## OOM rollback (R3.4, R4.4)
//!
//! `brk`-grow and `mmap` allocate frames page-by-page; on the first
//! `pmm::alloc_frame` failure (or a `vmm::map` failure) every page mapped so far in
//! that call is unwound (`vmm::unmap` + `pmm::free_frame`) and the operation returns
//! with the process's memory state unchanged: `brk` reports the unchanged break,
//! `mmap` returns `-ENOMEM`.
#![allow(dead_code)]

use alloc::vec::Vec;

use x86_64::structures::paging::PageTableFlags;

use crate::memory::{pmm, vmm};
use crate::task::compat;

use super::errno::Errno;
use super::mem::{
    plan_brk, plan_mmap, plan_munmap, prot_to_flags, BrkOutcome, MmapPlan, MmapRegion, MunmapPlan,
    VmRegionSet,
};
use super::validate::USER_ADDR_MAX;

/// Architectural page size (4 KiB).
const PAGE_SIZE: u64 = 4096;

/// Zero the 4 KiB physical frame at `frame` through the HHDM alias.
fn zero_frame(frame: u64) {
    // SAFETY: `frame` was just allocated from the PMM and is mapped into the HHDM
    // window, so `phys_to_virt(frame)` is a valid, writable, page-aligned pointer.
    unsafe {
        core::ptr::write_bytes(vmm::phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE as usize);
    }
}

/// Build the leaf PTE flags for a user data page from `(writable, nx)` (always
/// `PRESENT | USER_ACCESSIBLE`).
fn leaf_flags(writable: bool, nx: bool) -> PageTableFlags {
    let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if writable {
        flags |= PageTableFlags::WRITABLE;
    }
    if nx {
        flags |= PageTableFlags::NO_EXECUTE;
    }
    flags
}

/// Map zero-filled pages over `[from, to)` (page-aligned), skipping pages already
/// present. On the first allocation/mapping failure, unwind every page mapped by
/// this call and return `false` (caller leaves state unchanged).
fn map_zeroed_range(from: u64, to: u64, flags: PageTableFlags) -> bool {
    let mut mapped: Vec<u64> = Vec::new();
    let mut page = from;
    let mut ok = true;
    while page < to {
        if vmm::virt_to_phys(page).is_none() {
            match pmm::alloc_frame() {
                Some(frame) => {
                    zero_frame(frame);
                    if vmm::map(frame, page, flags).is_err() {
                        // Frame not referenced by any PTE; free it directly.
                        pmm::free_frame(frame);
                        ok = false;
                        break;
                    }
                    mapped.push(page);
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }
        page += PAGE_SIZE;
    }
    if !ok {
        unwind(&mapped);
    }
    ok
}

/// Unmap a set of pages this call mapped, returning their frames to the PMM.
fn unwind(pages: &[u64]) {
    for &p in pages {
        if let Some(phys) = vmm::virt_to_phys(p) {
            let _ = vmm::unmap(p);
            pmm::free_frame(phys & !(PAGE_SIZE - 1));
        }
    }
}

/// `brk` (12): query/move the program break (R3.1–R3.6). On a grow that cannot be
/// backed by physical memory, the break is left unchanged (R3.4).
pub fn sys_brk(addr: u64) -> Result<u64, Errno> {
    compat::with_current_compat(|cs| brk_impl(&mut cs.vm, addr))
        .ok_or(Errno::EINVAL)
}

fn brk_impl(vm: &mut VmRegionSet, requested: u64) -> u64 {
    match plan_brk(vm.initial_brk, vm.current_brk, requested) {
        BrkOutcome::Unchanged(v) => v,
        BrkOutcome::Shrink(v) => {
            vm.current_brk = v;
            v
        }
        BrkOutcome::Grow {
            new_brk,
            map_from,
            map_to,
        } => {
            let flags = leaf_flags(true, true); // heap: writable, non-executable
            if map_zeroed_range(map_from, map_to, flags) {
                vm.current_brk = new_brk;
                new_brk
            } else {
                // OOM: leave the break unchanged (R3.4).
                vm.current_brk
            }
        }
    }
}

/// `mmap` (9): map an anonymous, private, zero-filled region (R4.1, R4.2). Returns
/// the page-aligned base, or `-EINVAL` for a bad request (R4.6) / `-ENOMEM` when it
/// cannot be placed in the user half or backed by memory (R4.4).
pub fn sys_mmap(
    _addr: u64,
    len: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    _off: u64,
) -> Result<u64, Errno> {
    compat::with_current_compat(|cs| mmap_impl(&mut cs.vm, len, prot as u32, flags as u32, fd as i64))
        .unwrap_or(Err(Errno::EINVAL))
}

fn mmap_impl(vm: &mut VmRegionSet, len: u64, prot: u32, flags: u32, fd: i64) -> Result<u64, Errno> {
    match plan_mmap(len, prot, flags, fd, vm.mmap_next_hint) {
        MmapPlan::Reject(e) => Err(e),
        MmapPlan::Map {
            base,
            pages,
            writable,
            nx,
        } => {
            // The bump-pointer region must fit below the user ceiling (R4.4).
            let span = pages.checked_mul(PAGE_SIZE).ok_or(Errno::ENOMEM)?;
            let end = base.checked_add(span).ok_or(Errno::ENOMEM)?;
            if end > USER_ADDR_MAX {
                return Err(Errno::ENOMEM);
            }
            if !map_zeroed_range(base, end, leaf_flags(writable, nx)) {
                // OOM: existing mappings are untouched (R4.4).
                return Err(Errno::ENOMEM);
            }
            vm.mmaps.push(MmapRegion {
                base,
                pages,
                writable,
                nx,
            });
            vm.mmap_next_hint = end;
            Ok(base)
        }
    }
}

/// `munmap` (11): unmap a previously-`mmap`ped range (R4.3) or reject an
/// unaligned/uncovered request with `-EINVAL` (R4.7).
pub fn sys_munmap(addr: u64, len: u64) -> Result<u64, Errno> {
    compat::with_current_compat(|cs| munmap_impl(&mut cs.vm, addr, len))
        .unwrap_or(Err(Errno::EINVAL))
}

fn munmap_impl(vm: &mut VmRegionSet, base: u64, len: u64) -> Result<u64, Errno> {
    match plan_munmap(base, len, &vm.mmaps) {
        MunmapPlan::Reject(e) => Err(e),
        MunmapPlan::Unmap { base, pages } => {
            for i in 0..pages {
                let page = base + i * PAGE_SIZE;
                if let Some(phys) = vmm::virt_to_phys(page) {
                    let _ = vmm::unmap(page);
                    pmm::free_frame(phys & !(PAGE_SIZE - 1));
                }
            }
            cut_regions(&mut vm.mmaps, base, pages);
            Ok(0)
        }
    }
}

/// Remove the page span `[base, base + pages*4096)` from the tracked region set,
/// splitting any region it partially overlaps into its surviving sub-ranges.
fn cut_regions(regions: &mut Vec<MmapRegion>, base: u64, pages: u64) {
    let ustart = base;
    let uend = base + pages * PAGE_SIZE;
    let mut out: Vec<MmapRegion> = Vec::new();
    for r in regions.iter() {
        let rstart = r.base;
        let rend = r.base + r.pages * PAGE_SIZE;
        if rend <= ustart || rstart >= uend {
            // No overlap: keep intact.
            out.push(*r);
            continue;
        }
        // Surviving left sub-range.
        if rstart < ustart {
            out.push(MmapRegion {
                base: rstart,
                pages: (ustart - rstart) / PAGE_SIZE,
                writable: r.writable,
                nx: r.nx,
            });
        }
        // Surviving right sub-range.
        if rend > uend {
            out.push(MmapRegion {
                base: uend,
                pages: (rend - uend) / PAGE_SIZE,
                writable: r.writable,
                nx: r.nx,
            });
        }
    }
    *regions = out;
}

/// `mprotect` (10): change protection on a range of mapped user pages (R4.5), or
/// `-ENOMEM` if any page in the range is not currently mapped (R4.8). An unaligned
/// base is `-EINVAL`.
pub fn sys_mprotect(addr: u64, len: u64, prot: u64) -> Result<u64, Errno> {
    compat::with_current_compat(|cs| mprotect_impl(&mut cs.vm, addr, len, prot as u32))
        .unwrap_or(Err(Errno::EINVAL))
}

fn mprotect_impl(vm: &mut VmRegionSet, addr: u64, len: u64, prot: u32) -> Result<u64, Errno> {
    if addr & (PAGE_SIZE - 1) != 0 {
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    let pages = len
        .checked_add(PAGE_SIZE - 1)
        .map(|n| n / PAGE_SIZE)
        .ok_or(Errno::EINVAL)?;

    // First pass: every page in the range must be mapped, else -ENOMEM with no
    // change applied (R4.8).
    for i in 0..pages {
        let page = addr
            .checked_add(i.checked_mul(PAGE_SIZE).ok_or(Errno::EINVAL)?)
            .ok_or(Errno::EINVAL)?;
        if vmm::virt_to_phys(page).is_none() {
            return Err(Errno::ENOMEM);
        }
    }

    // Second pass: re-map each page with the new protection bits.
    let (writable, nx) = prot_to_flags(prot);
    let flags = leaf_flags(writable, nx);
    for i in 0..pages {
        let page = addr + i * PAGE_SIZE;
        let frame = vmm::virt_to_phys(page).unwrap() & !(PAGE_SIZE - 1);
        let _ = vmm::map(frame, page, flags);
    }

    update_region_flags(&mut vm.mmaps, addr, pages, writable, nx);
    Ok(0)
}

/// Update the tracked `(writable, nx)` of any region whose pages fall entirely
/// inside the reprotected span. Partial-overlap bookkeeping is intentionally
/// coarse: only fully-covered regions have their recorded flags refreshed (the
/// page tables themselves are always updated above).
fn update_region_flags(
    regions: &mut [MmapRegion],
    base: u64,
    pages: u64,
    writable: bool,
    nx: bool,
) {
    let ustart = base;
    let uend = base + pages * PAGE_SIZE;
    for r in regions.iter_mut() {
        let rstart = r.base;
        let rend = r.base + r.pages * PAGE_SIZE;
        if rstart >= ustart && rend <= uend {
            r.writable = writable;
            r.nx = nx;
        }
    }
}
