// memory/vmm.rs — 4-level paging (Virtual Memory Manager)
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use core::ptr;
use core::sync::atomic::Ordering;
use x86_64::structures::paging::page_table::{PageTableEntry, PageTableIndex};
use x86_64::structures::paging::{PageTable, PageTableFlags, PhysFrame};
use x86_64::instructions::tlb;
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

    crate::debug!("VMM Initialized: HHDM offset=0x{:x}, PML4=0x{:x}",
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
        let intermediate_flags =
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | user;

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

    // Set the PTE (Page Table Entry)
    pt[virt.p1_index()].set_addr(phys, flags | PageTableFlags::PRESENT);

    // Flush TLB for this virtual address
    tlb::flush(virt);

    Ok(())
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
