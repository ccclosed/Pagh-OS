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
    plan_brk, plan_mmap_base, plan_munmap, prot_to_flags, range_is_free, BrkOutcome, MmapRegion,
    MunmapPlan, VmRegionSet, MAP_ANONYMOUS, MAP_PRIVATE,
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
    compat::with_current_compat(|cs| brk_impl(&mut *cs.vm.lock(), addr)).ok_or(Errno::EINVAL)
}

fn brk_impl(vm: &mut VmRegionSet, requested: u64) -> u64 {
    // The brk heap grows upward in the same lower-half range the mmap bump
    // allocator uses, so a grow must not run into a tracked mmap region: clamp
    // the ceiling at the lowest region base above the current break (Linux
    // keeps the two areas apart; failing the grow is the POSIX-visible result).
    let ceiling = vm
        .mmaps
        .iter()
        .map(|r| r.base)
        .filter(|b| *b > vm.current_brk)
        .min()
        .unwrap_or(USER_ADDR_MAX);
    if requested > vm.current_brk && requested >= ceiling {
        return vm.current_brk;
    }
    match plan_brk(vm.initial_brk, vm.current_brk, requested) {
        BrkOutcome::Unchanged(v) => v,
        BrkOutcome::Shrink(v) => {
            vm.current_brk = v;
            v
        }
        BrkOutcome::Grow {
            new_brk, map_from, ..
        } => {
            // Lazy: no eager mapping. Pages in [brk_lazy_from, page_up(new_brk))
            // are backed by the page-fault handler on first touch; the recorded
            // floor only ever moves down.
            vm.brk_lazy_from = vm.brk_lazy_from.min(map_from);
            vm.current_brk = new_brk;
            new_brk
        }
    }
}

/// Additional Linux `mmap` flag bits handled here (beyond `MAP_PRIVATE` /
/// `MAP_ANONYMOUS` from the pure planner module).
const MAP_SHARED: u32 = 0x1;
const MAP_FIXED: u32 = 0x10;
/// Hint / bookkeeping-only bits, accepted and ignored:
/// `MAP_DENYWRITE | MAP_EXECUTABLE | MAP_NORESERVE | MAP_POPULATE | MAP_STACK`.
const MAP_IGNORED: u32 = 0x800 | 0x1000 | 0x4000 | 0x8000 | 0x2_0000;

/// `mmap` (9): map a private region (R4.1, R4.2) in the three shapes a dynamic loader
/// (`ld.so` mapping shared libraries) actually issues:
///
///   * anonymous `MAP_PRIVATE|MAP_ANONYMOUS` — zero-filled bump-pointer region
///     (the original behavior, R4.2);
///   * file-backed `MAP_PRIVATE` with `fd`/`off` — the region is backed by
///     fresh zeroed frames and the file bytes `[off, off + len)` are copied in
///     eagerly. There are no shared file mappings in this kernel to stay
///     coherent with, so an eager private copy is *exact* `MAP_PRIVATE`
///     semantics, and pages past EOF stay zero (the bss tail the loader
///     expects);
///   * `MAP_FIXED` at a page-aligned address — replaces any existing pages in
///     the range, exactly like Linux (the loader maps each ELF segment with
///     `MAP_FIXED` over the span reserved by its first mapping).
///
/// Returns the mapped base; `-EINVAL` for malformed requests and genuinely
/// unsupported shapes (`MAP_SHARED`, unknown flag bits — refuse loudly rather
/// than mis-map); `-EBADF` when a file-backed request names a descriptor that
/// is not an open regular file; `-ENOMEM` when the region cannot be placed
/// below the user ceiling or backed by frames (R4.4).
pub fn sys_mmap(
    addr: u64,
    len: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    off: u64,
) -> Result<u64, Errno> {
    let flags = flags as u32;
    if len == 0 || flags & MAP_SHARED != 0 || flags & MAP_PRIVATE == 0 {
        return Err(Errno::EINVAL);
    }
    if flags & !(MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED | MAP_IGNORED) != 0 {
        return Err(Errno::EINVAL);
    }
    let anon = flags & MAP_ANONYMOUS != 0;
    let fixed = flags & MAP_FIXED != 0;

    // File-backed: resolve and clone the backing node BEFORE taking the compat
    // lock, mirroring `read`/`pread64` (VFS I/O must run with the lock dropped).
    let node = if anon {
        None
    } else {
        if off & (PAGE_SIZE - 1) != 0 {
            return Err(Errno::EINVAL);
        }
        if (fd as i64) < 0 {
            return Err(Errno::EBADF);
        }
        match super::io_sys::file_node_for_fd(fd as u32) {
            Some(n) => Some(n),
            None => return Err(Errno::EBADF),
        }
    };

    let pages = len
        .checked_add(PAGE_SIZE - 1)
        .map(|n| n / PAGE_SIZE)
        .ok_or(Errno::ENOMEM)?;
    let span = pages.checked_mul(PAGE_SIZE).ok_or(Errno::ENOMEM)?;
    let (writable, nx) = prot_to_flags(prot as u32);
    let prot_none = prot == 0;

    // Phase 1 (under the compat lock): place the region, back it with zeroed
    // frames when eager, record it in the tracked set.
    let base = compat::with_current_compat(|cs| -> Result<u64, Errno> {
        let vm = &mut cs.vm.lock();
        let base = if fixed {
            if addr == 0 || addr & (PAGE_SIZE - 1) != 0 {
                return Err(Errno::EINVAL);
            }
            addr
        } else {
            // First-fit placement: reuse freed holes before extending past the
            // high-water mark (Linux reuses VA space; a bump-only allocator
            // makes long-lived processes walk into USER_ADDR_MAX and die of
            // ENOMEM with plenty of memory free).
            plan_mmap_base(&vm.mmaps, vm.mmap_floor, pages, USER_ADDR_MAX).ok_or(Errno::ENOMEM)?
        };
        let end = base.checked_add(span).ok_or(Errno::ENOMEM)?;
        if end > USER_ADDR_MAX {
            return Err(Errno::ENOMEM);
        }
        if fixed {
            // MAP_FIXED replaces whatever was there: return the old frames so
            // `map_zeroed_range` backs the whole span with fresh zeroed pages,
            // and cut the replaced span out of the tracked regions.
            for i in 0..pages {
                let page = base + i * PAGE_SIZE;
                if let Some(phys) = vmm::virt_to_phys(page) {
                    let _ = vmm::unmap(page);
                    pmm::free_frame(phys & !(PAGE_SIZE - 1));
                }
            }
            cut_regions(&mut vm.mmaps, base, pages);
        }
        // Anonymous regions (and PROT_NONE reserves of either kind) are lazy:
        // only the region is recorded here; the page-fault handler backs pages
        // on first touch. File-backed regions with access rights keep the
        // eager copy — there is no page cache to fault them in from.
        if !anon && !prot_none && !map_zeroed_range(base, end, leaf_flags(writable, nx)) {
            // OOM: existing mappings are untouched (R4.4).
            return Err(Errno::ENOMEM);
        }
        vm.mmaps.push(MmapRegion {
            base,
            pages,
            writable,
            nx,
            prot: prot as u32,
            anon,
        });
        if !fixed && end > vm.mmap_next_hint {
            vm.mmap_next_hint = end;
        }
        Ok(base)
    })
    .unwrap_or(Err(Errno::EINVAL))?;

    // Phase 2 (lock dropped): copy the file bytes in through the HHDM alias.
    // The just-installed PTEs may be read-only and/or NX under the final
    // protections, and CR0.WP makes supervisor writes honor read-only user
    // PTEs, so the copy goes through the always-writable HHDM alias instead of
    // the user virtual address.
    if let Some(node) = node {
        let size = node.size();
        let mut buf = [0u8; PAGE_SIZE as usize];
        for i in 0..pages {
            let file_off = match off.checked_add(i * PAGE_SIZE) {
                Some(o) => o,
                None => break,
            };
            if file_off >= size {
                break; // Rest of the region stays zero (bss tail).
            }
            let want = core::cmp::min(PAGE_SIZE, size - file_off) as usize;
            let n = node
                .read(file_off, &mut buf[..want])
                .map_err(|_| Errno::EINVAL)?;
            if n == 0 {
                break;
            }
            let page = base + i * PAGE_SIZE;
            if let Some(phys) = vmm::virt_to_phys(page) {
                // SAFETY: `page` was mapped by this very call to a fresh 4 KiB
                // frame; the HHDM alias of that frame is valid and writable,
                // and `n <= PAGE_SIZE`.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        buf.as_ptr(),
                        vmm::phys_to_virt(phys & !(PAGE_SIZE - 1)) as *mut u8,
                        n,
                    );
                }
            }
        }
    }
    Ok(base)
}

/// `munmap` (11): unmap a previously-`mmap`ped range (R4.3) or reject an
/// unaligned/uncovered request with `-EINVAL` (R4.7).
pub fn sys_munmap(addr: u64, len: u64) -> Result<u64, Errno> {
    compat::with_current_compat(|cs| munmap_impl(&mut cs.vm.lock(), addr, len))
        .unwrap_or(Err(Errno::EINVAL))
}

/// `mremap` (25): resize an anonymous private mapping. glibc's
/// realloc() tries mremap first for large blocks and falls back to
/// malloc+memcpy+free on ENOSYS, which logged an unsupported-syscall warning
/// on every CPython start. Shrinks drop the tail pages in place; growth
/// requires `MREMAP_MAYMOVE` (in-place growth is never attempted, matching
/// Linux when the next pages are taken): a fresh zeroed span is mapped, the
/// old frames are copied through the HHDM alias (immune to read-only/NX user
/// PTEs under CR0.WP) and the old range is unmapped.
pub fn sys_mremap(
    old_addr: u64,
    old_size: u64,
    new_size: u64,
    flags: u64,
    _new_addr: u64,
) -> Result<u64, Errno> {
    const MREMAP_MAYMOVE: u64 = 1;
    if old_addr & (PAGE_SIZE - 1) != 0 || old_size == 0 || new_size == 0 {
        return Err(Errno::EINVAL);
    }
    if flags & !MREMAP_MAYMOVE != 0 {
        // MREMAP_FIXED / MREMAP_DONTUNMAP are unsupported shapes — refuse
        // loudly rather than mis-map.
        return Err(Errno::EINVAL);
    }
    let old_pages = old_size
        .checked_add(PAGE_SIZE - 1)
        .map(|n| n / PAGE_SIZE)
        .ok_or(Errno::ENOMEM)?;
    let new_pages = new_size
        .checked_add(PAGE_SIZE - 1)
        .map(|n| n / PAGE_SIZE)
        .ok_or(Errno::ENOMEM)?;
    compat::with_current_compat(|cs| -> Result<u64, Errno> {
        let vm = &mut cs.vm.lock();
        // The whole old range must lie inside one tracked mmap region; its
        // protections carry over to the new placement.
        let old_span = old_pages.checked_mul(PAGE_SIZE).ok_or(Errno::ENOMEM)?;
        let idx = vm
            .mmaps
            .iter()
            .position(|r| r.base <= old_addr && old_addr + old_span <= r.base + r.pages * PAGE_SIZE)
            .ok_or(Errno::EFAULT)?;
        let old = vm.mmaps[idx];
        let flags_of = leaf_flags(old.writable, old.nx);

        if new_pages <= old_pages {
            if new_pages < old_pages {
                // Shrink in place: drop the tail pages.
                let _ = munmap_impl(
                    vm,
                    old_addr + new_pages * PAGE_SIZE,
                    (old_pages - new_pages) * PAGE_SIZE,
                );
            }
            return Ok(old_addr);
        }
        let delta = new_pages - old_pages;

        // In-place growth: when the span right after the region is free, just
        // widen the tracked region — no copy, no move. Anonymous tails stay
        // lazy; eager (file-backed) tails are zero-mapped here (mremap never
        // faults in more file data, matching Linux where the new pages simply
        // belong to the same VMA).
        if range_is_free(&vm.mmaps, old_addr + old_span, delta, USER_ADDR_MAX) {
            let tail = old_addr + old_span;
            if !old.anon && !map_zeroed_range(tail, tail + delta * PAGE_SIZE, flags_of) {
                return Err(Errno::ENOMEM);
            }
            vm.mmaps[idx].pages = new_pages;
            let end = old_addr + new_pages * PAGE_SIZE;
            if end > vm.mmap_next_hint {
                vm.mmap_next_hint = end;
            }
            return Ok(old_addr);
        }

        if flags & MREMAP_MAYMOVE == 0 {
            return Err(Errno::ENOMEM);
        }
        let span = new_pages.checked_mul(PAGE_SIZE).ok_or(Errno::ENOMEM)?;
        let base = plan_mmap_base(&vm.mmaps, vm.mmap_floor, new_pages, USER_ADDR_MAX)
            .ok_or(Errno::ENOMEM)?;
        let end = base.checked_add(span).ok_or(Errno::ENOMEM)?;
        if end > USER_ADDR_MAX {
            return Err(Errno::ENOMEM);
        }
        // Anonymous target: lazy — copy touches only pages that carry data.
        // Eager (file-backed) target: pre-map the whole span, then copy.
        if !old.anon && !map_zeroed_range(base, end, flags_of) {
            // OOM: the old mapping is untouched.
            return Err(Errno::ENOMEM);
        }
        let mut copied: Vec<u64> = Vec::new();
        for i in 0..old_pages {
            let src = vmm::virt_to_phys(old_addr + i * PAGE_SIZE);
            let Some(src) = src else {
                continue; // never-touched source page: target stays zero/lazy
            };
            let dst_page = base + i * PAGE_SIZE;
            if old.anon {
                // Ensure the destination page is backed before copying into it
                // (its frame cannot be written through a missing PTE; the HHDM
                // alias needs the frame to exist first).
                match pmm::alloc_frame() {
                    Some(frame) => {
                        zero_frame(frame);
                        if vmm::map(frame, dst_page, flags_of).is_err() {
                            pmm::free_frame(frame);
                            unwind(&copied);
                            return Err(Errno::ENOMEM);
                        }
                        copied.push(dst_page);
                    }
                    None => {
                        unwind(&copied);
                        return Err(Errno::ENOMEM);
                    }
                }
            }
            // SAFETY: both frames are 4 KiB, mapped by this process /
            // this very call; the HHDM aliases are valid and writable.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    vmm::phys_to_virt(src & !(PAGE_SIZE - 1)) as *const u8,
                    vmm::phys_to_virt(vmm::virt_to_phys(dst_page).unwrap()) as *mut u8,
                    PAGE_SIZE as usize,
                );
            }
        }
        vm.mmaps.insert(
            idx,
            MmapRegion {
                base,
                pages: new_pages,
                writable: old.writable,
                nx: old.nx,
                prot: old.prot,
                anon: old.anon,
            },
        );
        if end > vm.mmap_next_hint {
            vm.mmap_next_hint = end;
        }
        let _ = munmap_impl(vm, old_addr, old_span);
        Ok(base)
    })
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
                prot: r.prot,
                anon: r.anon,
            });
        }
        // Surviving right sub-range.
        if rend > uend {
            out.push(MmapRegion {
                base: uend,
                pages: (rend - uend) / PAGE_SIZE,
                writable: r.writable,
                nx: r.nx,
                prot: r.prot,
                anon: r.anon,
            });
        }
    }
    *regions = out;
}

/// `mprotect` (10): change protection on a range of mapped user pages (R4.5), or
/// `-ENOMEM` if any page in the range is not currently mapped (R4.8). An unaligned
/// base is `-EINVAL`.
pub fn sys_mprotect(addr: u64, len: u64, prot: u64) -> Result<u64, Errno> {
    compat::with_current_compat(|cs| mprotect_impl(&mut cs.vm.lock(), addr, len, prot as u32))
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

    // Coverage check: every page in the range must belong to a tracked mmap
    // region or the brk heap. Lazy (never-touched) pages are fine — Linux
    // mprotect operates on VMAs, not on resident PTEs.
    {
        let mut covered = vm.mmaps.clone();
        if vm.current_brk > vm.brk_lazy_from {
            covered.push(MmapRegion {
                base: vm.brk_lazy_from,
                pages: (vm.current_brk - vm.brk_lazy_from).div_ceil(PAGE_SIZE),
                writable: true,
                nx: true,
                prot: super::mem::PROT_READ | super::mem::PROT_WRITE,
                anon: true,
            });
        }
        if matches!(plan_munmap(addr, len, &covered), MunmapPlan::Reject(_)) {
            return Err(Errno::ENOMEM);
        }
    }

    let (writable, nx) = prot_to_flags(prot);
    let prot_none = prot == 0;
    // Second pass: only resident pages change hardware state. PROT_NONE drops
    // the PTE entirely (a later touch must SIGSEGV, not re-back the page);
    // everything else is re-mapped in place with the new bits.
    for i in 0..pages {
        let page = addr + i * PAGE_SIZE;
        if let Some(frame) = vmm::virt_to_phys(page) {
            let _ = vmm::unmap(page);
            if prot_none {
                pmm::free_frame(frame & !(PAGE_SIZE - 1));
            } else {
                let _ = vmm::map(frame & !(PAGE_SIZE - 1), page, leaf_flags(writable, nx));
            }
        }
    }

    update_region_flags(&mut vm.mmaps, addr, pages, writable, nx, prot);
    Ok(0)
}

/// Update the tracked `(writable, nx)`/`prot` of any region whose pages fall
/// entirely inside the reprotected span. Partial-overlap bookkeeping is
/// intentionally coarse: only fully-covered regions have their recorded flags
/// refreshed (the page tables themselves are always updated above).
fn update_region_flags(
    regions: &mut [MmapRegion],
    base: u64,
    pages: u64,
    writable: bool,
    nx: bool,
    prot: u32,
) {
    let ustart = base;
    let uend = base.saturating_add(pages.saturating_mul(PAGE_SIZE));
    for r in regions.iter_mut() {
        let rstart = r.base;
        let rend = r.base.saturating_add(r.pages.saturating_mul(PAGE_SIZE));
        if rstart >= ustart && rend <= uend {
            r.writable = writable;
            r.nx = nx;
            r.prot = prot;
        }
    }
}

/// Write-fault on a present page: copy-on-write service. The page must be a
/// `COW_BIT` leaf left by [`fork_user_space_cow`] AND live inside a writable
/// VMA (or the brk heap) — a write into read-only ELF text still SIGSEGVs.
/// Works for ring-3 faults and kernel-mode faults on user addresses
/// (`copy_out` over a fork-shared buffer).
fn handle_cow_fault(addr: u64, caused_by_write: bool) -> bool {
    if !caused_by_write || compat::compat_lock_held() {
        return false;
    }
    let page = addr & !(PAGE_SIZE - 1);
    compat::with_current_compat(|cs| {
        let vm = &mut cs.vm.lock();
        let brk_end = vm.current_brk.saturating_add(PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let in_writable_vma = vm.mmaps.iter().any(|r| {
            page >= r.base
                && page < r.base + r.pages * PAGE_SIZE
                && r.writable
                && r.prot & super::mem::PROT_WRITE != 0
        });
        let in_brk = page >= vm.brk_lazy_from && page < brk_end;
        if !in_writable_vma && !in_brk {
            return false;
        }
        crate::memory::vmm::cow_copy_page(page)
    })
    .unwrap_or(false)
}

/// Back the page at `addr` on demand (anonymous `mmap` regions and the `brk`
/// heap). Called from the #PF handler for every page fault, before any
/// diagnostic/SIGSEGV path.
///
/// Returns `true` when the fault was serviced: a zero frame was mapped with
/// the region's protection bits and the faulting instruction may simply retry.
/// Returns `false` when the address is not lazily servable — the caller's
/// existing SIGSEGV/diagnostic path applies:
///
///   * a PROTECTION_VIOLATION fault (page present, access type bad — NX/RO
///     violations must SIGSEGV; COW will hook here later);
///   * an address at or above `USER_ADDR_MAX` (kernel-side wild pointer);
///   * a page inside a `PROT_NONE` region (stack guard semantics);
///   * a page inside an eager (file-backed) region — those are always fully
///     mapped, so a missing PTE is a kernel bug;
///   * an address covered by no region and outside the brk heap.
///
/// Contract with the syscall layer (see the module docs): kernel code never
/// touches user memory while holding the compat registry lock, so this handler
/// can always take that lock. `compat::compat_lock_held()` is the tripwire for
/// violations of that rule — such a fault is reported as a segfault instead of
/// deadlocking the CPU on a non-reentrant spinlock.
pub fn handle_user_page_fault(
    addr: u64,
    protection_violation: bool,
    caused_by_write: bool,
) -> bool {
    if addr >= USER_ADDR_MAX {
        return false;
    }
    if protection_violation {
        return handle_cow_fault(addr, caused_by_write);
    }
    if compat::compat_lock_held() {
        crate::error!(
            "[PF] fault on user addr 0x{:x} inside with_current_compat - refusing re-entry",
            addr
        );
        return false;
    }
    let page = addr & !(PAGE_SIZE - 1);
    compat::with_current_compat(|cs| {
        let vm = &mut cs.vm.lock();
        // brk heap: pages in [brk_lazy_from, page_up(current_brk)) are backed
        // on first touch, always writable + NX.
        let brk_end = vm.current_brk.saturating_add(PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        if page >= vm.brk_lazy_from && page < brk_end {
            if vmm::virt_to_phys(page).is_some() {
                return true; // resident again (shrink/grow race); retry
            }
            return map_zeroed_range(page, page + PAGE_SIZE, leaf_flags(true, true));
        }
        // Tracked mmap regions.
        let Some(r) = vm
            .mmaps
            .iter()
            .find(|r| page >= r.base && page < r.base + r.pages * PAGE_SIZE)
        else {
            return false;
        };
        if r.prot == 0 {
            // PROT_NONE (e.g. a thread-stack guard page): a touch is a
            // genuine SIGSEGV, never a mapping opportunity.
            return false;
        }
        if vmm::virt_to_phys(page).is_some() {
            return true; // already resident; retry
        }
        if !r.anon {
            // File-backed regions are eagerly mapped; a missing PTE here is a
            // kernel bug, not a lazy-mapping case.
            crate::error!(
                "[PF] missing PTE in eager file-backed region base=0x{:x} addr=0x{:x}",
                r.base,
                addr
            );
            return false;
        }
        map_zeroed_range(page, page + PAGE_SIZE, leaf_flags(r.writable, r.nx))
    })
    .unwrap_or(false)
}
