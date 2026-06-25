//! Sv39 virtual memory bring-up.
//!
//! For the initial port we install a simple **identity map** of the low 4 GiB
//! using 1 GiB gigapages (leaf PTEs at the top level), which covers both the
//! device MMIO window (UART/PLIC/CLINT, < 0x8000_0000) and physical RAM
//! (0x8000_0000+). Because the mapping is identity, the kernel — linked and
//! running at its physical address — keeps executing unchanged once `satp`
//! switches translation on. Fine-grained per-page mapping for user processes
//! and W^X is a later milestone.

// Sv39 PTE permission/status bits.
const PTE_V: u64 = 1 << 0; // valid
const PTE_R: u64 = 1 << 1; // readable
const PTE_W: u64 = 1 << 2; // writable
const PTE_X: u64 = 1 << 3; // executable
const PTE_U: u64 = 1 << 4; // user-accessible
const PTE_A: u64 = 1 << 6; // accessed
const PTE_D: u64 = 1 << 7; // dirty

/// `satp` MODE field for Sv39.
const SATP_SV39: u64 = 8 << 60;

/// Number of 1 GiB identity gigapages to map (covers 0x0 .. 0x1_0000_0000).
const IDENTITY_GIB: u64 = 4;

/// Physical address of the active root page table, recorded by [`init_identity`]
/// so later mappings (e.g. user pages) can be added to the live address space.
static ROOT_PA: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Build an identity-mapped Sv39 address space and activate it. The root table
/// is allocated from the PMM (which must already be initialized).
///
/// # Safety
/// Must run once, on the boot hart, with the PMM up and before any code relies
/// on virtual != physical. Writing `satp` takes effect immediately; the
/// surrounding `sfence.vma` flushes stale TLB state.
pub unsafe fn init_identity() {
    let root = crate::pmm::alloc_frame().expect("pmm: no frame for root page table") as *mut u64;

    // Clear all 512 entries.
    for i in 0..512 {
        root.add(i).write_volatile(0);
    }

    // Leaf gigapage entries: PPN = phys >> 12, shifted into the PTE PPN field.
    let flags = PTE_V | PTE_R | PTE_W | PTE_X | PTE_A | PTE_D;
    for gib in 0..IDENTITY_GIB {
        let phys = gib << 30; // gib * 1 GiB
        let pte = ((phys >> 12) << 10) | flags;
        root.add(gib as usize).write_volatile(pte);
    }

    let satp = SATP_SV39 | ((root as u64) >> 12);
    core::arch::asm!(
        "csrw satp, {satp}",
        "sfence.vma",
        satp = in(reg) satp,
        options(nostack),
    );

    ROOT_PA.store(root as usize, core::sync::atomic::Ordering::Relaxed);
}

/// Flush the whole TLB after mapping changes.
pub fn flush() {
    // SAFETY: `sfence.vma` with no operands flushes all S-mode translations.
    unsafe { core::arch::asm!("sfence.vma", options(nostack)) };
}

/// Map a single 4 KiB user page `va -> pa` into the active address space,
/// allocating intermediate Sv39 tables from the PMM as needed. The leaf is
/// always readable + user-accessible; `exec`/`write` add X / W. The `va`'s GiB
/// must not already be covered by an identity gigapage (user VAs live at
/// 0x1_0000_0000+, above the identity window).
///
/// # Safety
/// Paging must be active (`init_identity` ran). `pa` must be a free/owned frame.
pub unsafe fn map_user(va: usize, pa: usize, exec: bool, write: bool) {
    let root = ROOT_PA.load(core::sync::atomic::Ordering::Relaxed) as *mut u64;

    let mut table = root;
    // Walk levels 2 then 1, creating pointer PTEs (V only) where missing.
    for level in [2usize, 1usize] {
        let vpn = (va >> (12 + 9 * level)) & 0x1ff;
        let entry = table.add(vpn);
        let pte = entry.read_volatile();
        if pte & PTE_V == 0 {
            let next = crate::pmm::alloc_frame().expect("pmm: no frame for page table") as *mut u64;
            for i in 0..512 {
                next.add(i).write_volatile(0);
            }
            entry.write_volatile(((next as u64 >> 12) << 10) | PTE_V);
            table = next;
        } else {
            let next_pa = ((pte >> 10) & ((1 << 44) - 1)) << 12;
            table = next_pa as *mut u64;
        }
    }

    let vpn0 = (va >> 12) & 0x1ff;
    let mut flags = PTE_V | PTE_R | PTE_U | PTE_A | PTE_D;
    if exec {
        flags |= PTE_X;
    }
    if write {
        flags |= PTE_W;
    }
    table.add(vpn0).write_volatile(((pa as u64 >> 12) << 10) | flags);
}
