// memory/vmm.rs — 4-level paging (Virtual Memory Manager)
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use core::ptr;
use core::sync::atomic::Ordering;
use x86_64::instructions::tlb;
use x86_64::structures::paging::page_table::{PageTableEntry, PageTableIndex};
use x86_64::structures::paging::{PageTable, PageTableFlags, PhysFrame};
use x86_64::{PhysAddr, VirtAddr};

/// Typed errors returned by the virtual memory manager.
///
/// Replaces the previous ad-hoc `&'static str` errors so callers can match on
/// the failure mode instead of comparing strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmError {
    /// The PMM had no free frame to back an intermediate page table.
    OutOfMemory,
    /// An entry along the page-table walk was not present.
    NotMapped,
}

impl core::fmt::Display for VmError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            VmError::OutOfMemory => "out of memory",
            VmError::NotMapped => "not mapped",
        };
        f.write_str(s)
    }
}

/// Initialize the VMM. Stores the Limine HHDM offset.
pub fn init(hhdm_offset: u64) {
    crate::HHDM_OFFSET.store(hhdm_offset, Ordering::Relaxed);

    crate::debug!(
        "VMM Initialized: HHDM offset=0x{:x}, PML4=0x{:x}",
        hhdm_offset,
        current_pml4_phys(),
    );
}

/// Convert a physical address to a virtual address via HHDM.
pub fn phys_to_virt(phys: u64) -> u64 {
    phys + crate::HHDM_OFFSET.load(Ordering::Relaxed)
}

/// Get the *active* PML4 physical address by reading CR3.
///
/// This reads the live CR3 rather than a cached static, so page-table walks
/// (`map`/`unmap`/`virt_to_phys`) always target the address space that is
/// actually installed. This matters for user processes: while a ring-3 task is
/// current, CR3 holds its user PML4, and mapping/translation must operate on
/// that PML4 (not the kernel PML4). At boot — and whenever a kernel thread is
/// current — CR3 holds the kernel PML4, so the result is unchanged from the
/// previous cached-static behaviour.
pub fn current_pml4_phys() -> u64 {
    // SAFETY: Reading CR3 is a side-effect-free privileged read; always valid.
    let (cr3_frame, _): (PhysFrame, _) = x86_64::registers::control::Cr3::read();
    cr3_frame.start_address().as_u64()
}

/// Reload CR3 with the page table at physical address `phys`.
///
/// This is the SINGLE place in the kernel that writes CR3 on a context switch
/// (Requirement 11.5). Both the preemptive tick path (`scheduler_tick_irq`) and
/// the cooperative `yield_current` path call through here, so there is exactly
/// one CR3 reload site. The write is unconditional: rewriting CR3 with the same
/// or a new PML4 also flushes the non-global TLB, ensuring the next task's stack
/// and address-space mappings are reloaded.
///
/// # Safety
/// `phys` must be the physical base address of a valid, fully-initialized PML4
/// containing the kernel higher-half mappings. Loading a malformed table will
/// fault on the next memory access.
#[inline]
pub unsafe fn load_cr3(phys: u64) {
    x86_64::registers::control::Cr3::write(
        PhysFrame::containing_address(PhysAddr::new(phys)),
        x86_64::registers::control::Cr3Flags::empty(),
    );
}

/// A safe abstraction over the recursive page-table walk.
///
/// All of the raw `unsafe` needed to turn an HHDM-mapped physical address into a
/// `&PageTable`/`&mut PageTable` is confined to this type's `table`/`table_mut`
/// helpers. Callers in `map`/`unmap`/`virt_to_phys` therefore read as safe code.
struct PageTableWalker {
    hhdm: u64,
}

impl PageTableWalker {
    /// Construct a walker bound to the active HHDM offset.
    fn new() -> Self {
        Self {
            hhdm: crate::HHDM_OFFSET.load(Ordering::Relaxed),
        }
    }

    /// Borrow the page table located at physical address `phys`.
    fn table(&self, phys: u64) -> &'static PageTable {
        // SAFETY: Every page-table frame is mapped into the HHDM window by
        // Limine, so `phys + hhdm` is a valid, aligned, readable pointer to a
        // `PageTable` for the lifetime of the kernel's address space.
        unsafe { &*((phys + self.hhdm) as *const PageTable) }
    }

    /// Mutably borrow the page table located at physical address `phys`.
    fn table_mut(&self, phys: u64) -> &'static mut PageTable {
        // SAFETY: Same HHDM-validity invariant as `table`. Each level of the
        // walk points at a distinct frame, so the `'static mut` references handed
        // out for successive levels never alias the same memory.
        unsafe { &mut *((phys + self.hhdm) as *mut PageTable) }
    }

    /// The active PML4 (read-only).
    fn root(&self) -> &'static PageTable {
        self.table(current_pml4_phys())
    }

    /// The active PML4 (mutable).
    fn root_mut(&self) -> &'static mut PageTable {
        self.table_mut(current_pml4_phys())
    }

    /// Follow a present entry to the next-level table, or `None` if absent.
    fn next_mut(&self, entry: &PageTableEntry) -> Option<&'static mut PageTable> {
        if !entry.flags().contains(PageTableFlags::PRESENT) {
            return None;
        }
        Some(self.table_mut(entry.addr().as_u64()))
    }

    /// Ensure an intermediate table exists at `table[idx]`, allocating and
    /// zeroing a fresh frame from the PMM when the entry is absent, then return
    /// the next-level table.
    ///
    /// # Intermediate-entry flag policy
    ///
    /// An intermediate PML4/PDPT/PD entry is *not* a leaf mapping — it only
    /// points at the next-level table — so it must carry the minimal flags that
    /// keep the whole sub-tree usable rather than the leaf's flags:
    ///
    /// - It is always `PRESENT | WRITABLE`. Writability on an intermediate does
    ///   not by itself make any leaf writable (the leaf PTE governs that), and
    ///   forcing it writable keeps later writable leaf mappings under the same
    ///   intermediate working.
    /// - `USER_ACCESSIBLE` is propagated *iff* the leaf mapping requested it
    ///   (`leaf_flags & USER_ACCESSIBLE`). This satisfies Property 4: every
    ///   intermediate on a `USER_ACCESSIBLE` page's walk must also carry
    ///   `USER_ACCESSIBLE`, or the CPU denies ring-3 access to the leaf.
    /// - Leaf-only flags (`NO_EXECUTE`, `NO_CACHE`, `HUGE_PAGE`, `GLOBAL`) and
    ///   the leaf's physical address are deliberately *not* propagated. An NX
    ///   bit on a higher-level entry disables execution for the entire sub-tree,
    ///   and `NO_CACHE` on an intermediate would needlessly mark sibling
    ///   mappings uncacheable — both would poison unrelated mappings.
    ///
    /// When the intermediate already exists, it is *upgraded* to
    /// `USER_ACCESSIBLE` if the new leaf mapping needs it but the existing entry
    /// (e.g. first created for a kernel mapping) lacks it, preserving the entry's
    /// existing address and other flags.
    fn ensure_next(
        &self,
        table: &mut PageTable,
        idx: PageTableIndex,
        flags: PageTableFlags,
    ) -> Result<&'static mut PageTable, VmError> {
        // Flags an intermediate entry should carry: present + writable, plus
        // user-accessibility only when the leaf mapping requested it.
        let user = flags & PageTableFlags::USER_ACCESSIBLE;
        let intermediate_flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | user;

        if !table[idx].flags().contains(PageTableFlags::PRESENT) {
            let frame = crate::memory::pmm::alloc_frame().ok_or(VmError::OutOfMemory)?;
            // SAFETY: `frame` was just allocated and is mapped via the HHDM, so
            // this writes zeroes over exactly one owned, page-aligned frame.
            unsafe {
                ptr::write_bytes((frame + self.hhdm) as *mut u8, 0, 4096);
            }
            table[idx].set_addr(PhysAddr::new(frame), intermediate_flags);
        } else if user.contains(PageTableFlags::USER_ACCESSIBLE)
            && !table[idx].flags().contains(PageTableFlags::USER_ACCESSIBLE)
        {
            // Upgrade case (Property 4): an intermediate first created for a
            // kernel mapping must gain USER_ACCESSIBLE so a later user mapping
            // beneath it is actually reachable from ring 3. Preserve the
            // existing address and any other flags it already carries.
            let addr = table[idx].addr();
            let upgraded = table[idx].flags() | PageTableFlags::USER_ACCESSIBLE;
            table[idx].set_addr(addr, upgraded);
        }
        Ok(self.table_mut(table[idx].addr().as_u64()))
    }
}

/// Walk page tables to translate a virtual address to a physical address.
pub fn virt_to_phys(virt: u64) -> Option<u64> {
    let virt_addr = VirtAddr::new(virt);
    let p4_idx = virt_addr.p4_index();
    let p3_idx = virt_addr.p3_index();
    let p2_idx = virt_addr.p2_index();
    let p1_idx = virt_addr.p1_index();

    let walker = PageTableWalker::new();

    let pml4 = walker.root();
    let pml4_entry = &pml4[p4_idx];
    if !pml4_entry.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }

    let pdpt = walker.table(pml4_entry.addr().as_u64());
    let pdpt_entry = &pdpt[p3_idx];
    if !pdpt_entry.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }

    // Check for 1GiB huge page
    if pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        let offset = virt_addr.as_u64() & 0x3FFF_FFFF; // 1GiB - 1
        return Some(pdpt_entry.addr().as_u64() + offset);
    }

    let pd = walker.table(pdpt_entry.addr().as_u64());
    let pd_entry = &pd[p2_idx];
    if !pd_entry.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }

    // Check for 2MiB huge page
    if pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        let offset = virt_addr.as_u64() & 0x1F_FFFF; // 2MiB - 1
        return Some(pd_entry.addr().as_u64() + offset);
    }

    let pt = walker.table(pd_entry.addr().as_u64());
    let pt_entry = &pt[p1_idx];
    if !pt_entry.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }

    let offset = virt_addr.as_u64() & 0xFFF; // 4KiB - 1
    Some(pt_entry.addr().as_u64() + offset)
}

/// Return the effective leaf flags for a mapped virtual address.
/// Every level must be present; user accessibility is intersected across the
/// complete walk so a kernel-only ancestor cannot be mistaken for a user page.
pub fn page_flags(virt: u64) -> Option<PageTableFlags> {
    let va = VirtAddr::new(virt);
    let walker = PageTableWalker::new();
    let p4e = &walker.root()[va.p4_index()];
    if !p4e.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    let p3 = walker.table(p4e.addr().as_u64());
    let p3e = &p3[va.p3_index()];
    if !p3e.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    let mut effective = p4e.flags() & p3e.flags();
    if p3e.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Some(effective);
    }
    let p2 = walker.table(p3e.addr().as_u64());
    let p2e = &p2[va.p2_index()];
    if !p2e.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    effective &= p2e.flags();
    if p2e.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Some(effective);
    }
    let p1 = walker.table(p2e.addr().as_u64());
    let p1e = &p1[va.p1_index()];
    if !p1e.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    Some(effective & p1e.flags())
}

/// Map a physical frame to a virtual page.
///
/// Allocates intermediate page tables as needed via PMM.
pub fn map(phys_addr: u64, virt_addr: u64, flags: PageTableFlags) -> Result<(), VmError> {
    let phys = PhysAddr::new(phys_addr);
    let virt = VirtAddr::new(virt_addr);

    let walker = PageTableWalker::new();

    // Walk (allocating intermediates) down to the PT. The walker confines all
    // page-table-deref `unsafe`, so this reads as safe code.
    let pml4 = walker.root_mut();
    let pdpt = walker.ensure_next(pml4, virt.p4_index(), flags)?;
    let pd = walker.ensure_next(pdpt, virt.p3_index(), flags)?;
    let pt = walker.ensure_next(pd, virt.p2_index(), flags)?;

    // Never silently overwrite a live translation. A present
    // PTE pointing at a *different* frame means two owners believe they own
    // this virtual page: the old frame leaks and whoever still uses the old
    // mapping is one re-allocation away from reading someone else's memory.
    // Keep the overwrite (previous behaviour, nothing regresses) but log it.
    {
        let entry = &pt[virt.p1_index()];
        if entry.flags().contains(PageTableFlags::PRESENT) && entry.addr().as_u64() != phys.as_u64()
        {
            crate::warn!(
                "[VMM] remap virt=0x{:016x}: old_phys=0x{:x} -> new_phys=0x{:x} (old frame leaked/aliased)",
                virt_addr, entry.addr().as_u64(), phys.as_u64()
            );
        }
    }
    // Set the PTE (Page Table Entry)
    pt[virt.p1_index()].set_addr(phys, flags | PageTableFlags::PRESENT);

    // Flush TLB for this virtual address
    tlb::flush(virt);

    Ok(())
}

/// Log the raw 4-level page-table walk for `virt_addr` in the
/// *currently active* address space, one line per level, stopping at the first
/// non-present entry. Post-mortem this distinguishes "a single PTE vanished"
/// from "a whole intermediate table vanished" (e.g. its frame was
/// double-allocated and zero-filled by another owner) - the two failure shapes
/// point at different culprits.
/// Return a copy of the leaf PTE for `virt_addr` in the CURRENT address
/// space, if the whole walk is present. Used by the page-fault handler for
/// copy-on-write diagnosis.
pub fn walk_pte(virt_addr: u64) -> Option<PageTableEntry> {
    const SHIFTS: [u64; 3] = [39, 30, 21];
    let mut table_phys = current_pml4_phys();
    for shift in SHIFTS {
        let idx = ((virt_addr >> shift) & 0x1ff) as usize;
        // SAFETY: page-table frames are always readable through the HHDM.
        let entry =
            unsafe { core::ptr::read_volatile((phys_to_virt(table_phys) as *const u64).add(idx)) };
        if entry & 1 == 0 {
            return None;
        }
        table_phys = entry & 0x000f_ffff_ffff_f000;
    }
    let idx = (virt_addr >> 12) & 0x1ff;
    // SAFETY: as above.
    let pte = unsafe {
        core::ptr::read_volatile(
            (phys_to_virt(table_phys) as *const PageTableEntry).add(idx as usize),
        )
    };
    if !pte.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    Some(pte)
}

/// Set the WRITABLE bit on the leaf PTE for `virt_addr` in the CURRENT
/// address space and invalidate the TLB entry. Returns false if the walk is
/// not fully present. Used by the page-fault handler to fix up pages whose
/// VmRegion tracking says "writable" but whose PTE lost the bit.
pub fn set_pte_writable(virt_addr: u64) -> bool {
    const SHIFTS: [u64; 3] = [39, 30, 21];
    let mut table_phys = current_pml4_phys();
    for shift in SHIFTS {
        let idx = ((virt_addr >> shift) & 0x1ff) as usize;
        // SAFETY: page-table frames are always readable through the HHDM.
        let entry =
            unsafe { core::ptr::read_volatile((phys_to_virt(table_phys) as *const u64).add(idx)) };
        if entry & 1 == 0 {
            return false;
        }
        table_phys = entry & 0x000f_ffff_ffff_f000;
    }
    let idx = (virt_addr >> 12) & 0x1ff;
    let pte_virt = phys_to_virt(table_phys) + idx * 8;
    // SAFETY: PTE slot in a mapped page table.
    let pte = unsafe { core::ptr::read_volatile(pte_virt as *const u64) };
    if pte & 1 == 0 {
        return false;
    }
    unsafe {
        core::ptr::write_volatile(pte_virt as *mut u64, pte | PageTableFlags::WRITABLE.bits())
    };
    x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(virt_addr));
    true
}

pub fn dump_translation(virt_addr: u64) {
    const NAMES: [&str; 4] = ["PML4E", "PDPTE", "PDE", "PTE"];
    const SHIFTS: [u64; 4] = [39, 30, 21, 12];
    let mut table_phys = current_pml4_phys();
    for level in 0..4 {
        let idx = ((virt_addr >> SHIFTS[level]) & 0x1ff) as usize;
        // SAFETY: page-table frames are always readable through the HHDM.
        let entry =
            unsafe { core::ptr::read_volatile((phys_to_virt(table_phys) as *const u64).add(idx)) };
        if entry & 1 == 0 {
            crate::error!(
                "[VMM] walk 0x{:012x}: {}[{}] = 0x{:016x} NOT PRESENT (table@phys=0x{:x})",
                virt_addr,
                NAMES[level],
                idx,
                entry,
                table_phys
            );
            return;
        }
        crate::error!(
            "[VMM] walk 0x{:012x}: {}[{}] = 0x{:016x} (table@phys=0x{:x})",
            virt_addr,
            NAMES[level],
            idx,
            entry,
            table_phys
        );
        if level > 0 && level < 3 && entry & (1 << 7) != 0 {
            return; // huge page - the walk legitimately ends here
        }
        table_phys = entry & 0x000f_ffff_ffff_f000;
    }
}

/// Unmap a virtual page.
pub fn unmap(virt_addr: u64) -> Result<(), VmError> {
    let virt = VirtAddr::new(virt_addr);

    let walker = PageTableWalker::new();

    let pml4 = walker.root_mut();
    let pdpt = walker
        .next_mut(&pml4[virt.p4_index()])
        .ok_or(VmError::NotMapped)?;
    let pd = walker
        .next_mut(&pdpt[virt.p3_index()])
        .ok_or(VmError::NotMapped)?;
    let pt = walker
        .next_mut(&pd[virt.p2_index()])
        .ok_or(VmError::NotMapped)?;

    // Clear the PTE
    pt[virt.p1_index()].set_unused();
    tlb::flush(virt);

    Ok(())
}

/// Create a new PML4 table for a user process.
/// Copies kernel higher-half mappings from the current PML4.
pub fn new_user_pml4() -> Result<u64, VmError> {
    let walker = PageTableWalker::new();

    let new_pml4_frame = crate::memory::pmm::alloc_frame().ok_or(VmError::OutOfMemory)?;

    // Zero the new PML4, then copy the kernel higher-half entries (256..512).
    let new_pml4 = walker.table_mut(new_pml4_frame);
    new_pml4.zero();

    let current_pml4 = walker.root();
    for i in 256usize..512 {
        new_pml4[i] = current_pml4[i].clone();
    }

    crate::debug!("Created new user PML4 at phys=0x{:x}", new_pml4_frame);

    Ok(new_pml4_frame)
}

/// Map a region of physical MMIO into the kernel address space as
/// non-cacheable and return its virtual base address.
///
/// `len` bytes starting at `phys` are mapped page-by-page (rounded up to whole
/// 4 KiB pages) with `PRESENT | WRITABLE | NO_CACHE | NO_EXECUTE`. MMIO is
/// reached through the HHDM window (`virt = phys_to_virt(phys)`, the same
/// convention as `crate::memory::layout::mmio_virt_for`), so the returned base
/// matches how the LAPIC/IOAPIC MMIO is mapped today.
///
/// MMIO is device memory: it is mapped `NO_CACHE` so writes/reads hit the
/// device, `NO_EXECUTE` since it is never code, and is *never* `USER_ACCESSIBLE`
/// — these regions belong to the kernel alone.
pub fn map_mmio(phys: u64, len: u64) -> Result<u64, VmError> {
    let page_size = 4096u64;

    // Page-align the base down and the end up so the whole requested region is
    // covered even when `phys`/`len` are not page-aligned.
    let start = phys & !(page_size - 1);
    let end = (phys + len + (page_size - 1)) & !(page_size - 1);

    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_CACHE
        | PageTableFlags::NO_EXECUTE;

    let mut p = start;
    while p < end {
        map(p, phys_to_virt(p), flags)?;
        p += page_size;
    }

    Ok(phys_to_virt(phys))
}

// ─── fork support ────────────────────────────────────────────────────

/// Deep-copy the user lower half (PML4 entries 0..256) of
/// `src_pml4` into `dst_pml4`, eagerly duplicating every mapped 4 KiB frame
/// (no copy-on-write). All access goes through the HHDM window, so neither
/// address space needs to be active in CR3 and no TLB shootdown is needed
/// (the destination has never been loaded). Leaf flags are preserved;
/// intermediate tables are rebuilt with the standard user policy. Huge pages
/// never appear in the user lower half here (the loader and mmap only map
/// 4 KiB pages), so a huge leaf is reported as `NotMapped` instead of being
/// silently shared between two address spaces.
pub fn clone_user_space(src_pml4: u64, dst_pml4: u64) -> Result<(), VmError> {
    let walker = PageTableWalker::new();
    let user = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    let src4 = walker.table(src_pml4);
    for i4 in 0..256usize {
        let e4 = &src4[i4];
        if !e4.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }
        let src3 = walker.table(e4.addr().as_u64());
        for i3 in 0..512usize {
            let e3 = &src3[i3];
            if !e3.flags().contains(PageTableFlags::PRESENT) {
                continue;
            }
            if e3.flags().contains(PageTableFlags::HUGE_PAGE) {
                return Err(VmError::NotMapped);
            }
            let src2 = walker.table(e3.addr().as_u64());
            for i2 in 0..512usize {
                let e2 = &src2[i2];
                if !e2.flags().contains(PageTableFlags::PRESENT) {
                    continue;
                }
                if e2.flags().contains(PageTableFlags::HUGE_PAGE) {
                    return Err(VmError::NotMapped);
                }
                let src1 = walker.table(e2.addr().as_u64());
                // Build the destination chain lazily — only once this P1 proves
                // to hold at least one present leaf.
                let mut dst1: Option<&mut PageTable> = None;
                for i1 in 0..512usize {
                    let e1 = &src1[i1];
                    if !e1.flags().contains(PageTableFlags::PRESENT) {
                        continue;
                    }
                    if dst1.is_none() {
                        let p4 = walker.table_mut(dst_pml4);
                        let p3 = walker.ensure_next(
                            p4,
                            x86_64::structures::paging::PageTableIndex::new(i4 as u16),
                            user,
                        )?;
                        let p2 = walker.ensure_next(
                            p3,
                            x86_64::structures::paging::PageTableIndex::new(i3 as u16),
                            user,
                        )?;
                        let p1 = walker.ensure_next(
                            p2,
                            x86_64::structures::paging::PageTableIndex::new(i2 as u16),
                            user,
                        )?;
                        dst1 = Some(p1);
                    }
                    let frame = crate::memory::pmm::alloc_frame().ok_or(VmError::OutOfMemory)?;
                    // SAFETY: both frames are ordinary RAM reachable through
                    // the HHDM window; copy one whole page.
                    unsafe {
                        ptr::copy_nonoverlapping(
                            phys_to_virt(e1.addr().as_u64()) as *const u8,
                            phys_to_virt(frame) as *mut u8,
                            4096,
                        );
                    }
                    if let Some(pt) = dst1.as_deref_mut() {
                        pt[i1].set_addr(x86_64::PhysAddr::new(frame), e1.flags());
                    }
                }
            }
        }
    }
    Ok(())
}

/// Translate a user virtual address in the (inactive)
/// address space rooted at `pml4` — used to write CLONE_CHILD_SETTID into the
/// child's copied memory. 4 KiB walks only; None on any non-present entry.
pub fn virt_to_phys_in(pml4: u64, virt: u64) -> Option<u64> {
    let walker = PageTableWalker::new();
    let idx4 = ((virt >> 39) & 0x1ff) as usize;
    let idx3 = ((virt >> 30) & 0x1ff) as usize;
    let idx2 = ((virt >> 21) & 0x1ff) as usize;
    let idx1 = ((virt >> 12) & 0x1ff) as usize;
    let e4 = &walker.table(pml4)[idx4];
    if !e4.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    let e3 = &walker.table(e4.addr().as_u64())[idx3];
    if !e3.flags().contains(PageTableFlags::PRESENT)
        || e3.flags().contains(PageTableFlags::HUGE_PAGE)
    {
        return None;
    }
    let e2 = &walker.table(e3.addr().as_u64())[idx2];
    if !e2.flags().contains(PageTableFlags::PRESENT)
        || e2.flags().contains(PageTableFlags::HUGE_PAGE)
    {
        return None;
    }
    let e1 = &walker.table(e2.addr().as_u64())[idx1];
    if !e1.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    Some(e1.addr().as_u64() + (virt & 0xfff))
}
