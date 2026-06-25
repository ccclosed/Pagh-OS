// memory/layout.rs — Single source of truth for fixed kernel virtual regions.
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// This module is the canonical definition of the kernel's fixed virtual-memory
// layout (Requirement 4.1). Historically the magic constants below were scattered
// across `task::scheduler`, `task::process`, and `memory::allocator`. They are
// collected here so the rest of the kernel can derive addresses from one place.
//
// NOTE: This task (5.1) ONLY defines the constants/helpers. The call sites in
// scheduler.rs / process.rs / allocator.rs are migrated to use these in task 5.2,
// so some of these items will report `dead_code` warnings until then.

/// Architectural page size (4 KiB).
pub const PAGE_SIZE: u64 = 4096;

// ─── Kernel per-PID stack region ─────────────────────────────────────────────

/// Base of the per-PID kernel-stack region.
///
/// Each PID is assigned a fixed slot starting at this base. Within a slot the
/// lowest page is a guard page (left unmapped to catch stack overflow) followed
/// by `KERNEL_STACK_PAGES` mapped stack pages.
///
/// Source: `task::scheduler::kernel_thread_spawn`.
pub const KERNEL_STACK_REGION_BASE: u64 = 0xFFFF_FE00_0000_0000;

/// Number of mapped stack pages per kernel stack.
///
/// Canonical value is 32 (the larger, safer of the two values that previously
/// existed: `scheduler.rs` used 32, `process.rs` used 8). `process.rs` adopts
/// this canonical constant in task 5.2.
pub const KERNEL_STACK_PAGES: u64 = 32;

/// Number of guard pages preceding each kernel stack (unmapped overflow guard).
pub const KERNEL_STACK_GUARD_PAGES: u64 = 1;

/// Bytes reserved per PID in the kernel-stack region (guard pages + stack pages).
pub const KERNEL_STACK_STRIDE: u64 =
    (KERNEL_STACK_PAGES + KERNEL_STACK_GUARD_PAGES) * PAGE_SIZE;

/// Compute the kernel-stack addresses for a given PID.
///
/// Returns `(guard_base, stack_base, stack_top)` where:
/// - `guard_base` is the start of the PID's slot (the guard page lives here),
/// - `stack_base` is the lowest mapped stack address (guard_base + guard pages),
/// - `stack_top`  is the exclusive top of the stack (initial RSP grows down from here).
///
/// This mirrors the layout `kernel_thread_spawn` builds today:
/// `guard_base = REGION_BASE + pid * STRIDE`, `stack_base = guard_base + PAGE_SIZE`,
/// `stack_top = stack_base + KERNEL_STACK_PAGES * PAGE_SIZE`.
pub fn kernel_stack_for_pid(pid: u64) -> (u64 /*guard_base*/, u64 /*stack_base*/, u64 /*stack_top*/) {
    let guard_base = KERNEL_STACK_REGION_BASE + pid * KERNEL_STACK_STRIDE;
    let stack_base = guard_base + KERNEL_STACK_GUARD_PAGES * PAGE_SIZE;
    let stack_top = stack_base + KERNEL_STACK_PAGES * PAGE_SIZE;
    (guard_base, stack_base, stack_top)
}

// ─── User stack ──────────────────────────────────────────────────────────────

/// Exclusive top of the user-mode stack region.
///
/// Source: `task::process` (`USER_STACK_TOP`).
pub const USER_STACK_TOP: u64 = 0x7000_8000_0000;

/// Number of pages mapped for the user-mode stack.
///
/// Source: `task::process` (`USER_STACK_PAGES`).
pub const USER_STACK_PAGES: u64 = 8;

/// Base virtual address from which anonymous `mmap` regions are bump-allocated
/// for a `Compat_Process` (the lower-half `mmap` hint base seeded into its
/// `VmRegionSet`).
///
/// Chosen at 32 TiB: comfortably above where any static binary's program break
/// (`brk`) could plausibly grow (static `ET_EXEC` images load near `0x40_0000`
/// and static-PIE near `PIE_BASE = 0x1_0000`) and well below both the user stack
/// (`USER_STACK_TOP = 0x7000_8000_0000`) and the lower-half ceiling
/// `User_Addr_Max = 0x0000_8000_0000_0000`, so anonymous mappings never collide
/// with the heap or the stack.
pub const USER_MMAP_BASE: u64 = 0x2000_0000_0000;

// ─── Kernel heap ─────────────────────────────────────────────────────────────

/// Initial kernel heap size, in pages (65536 × 4 KiB = 256 MiB).
///
/// History: 64 pages (256 KiB) → 4096 (16 MiB, for the `paint` full-screen
/// backing buffers) → 65536 (256 MiB). The big jump is for the `apt` package
/// manager: the in-RAM Debian `Packages` index is large — the compressed
/// download is ~10 MiB and a moderate decompressed index plus its parsed
/// records run to tens of MiB, all held on this single fixed heap at once.
/// With only 16 MiB the download/decompress overran the heap and the
/// `linked_list_allocator` (which never grows) returned null → the `alloc`
/// error handler aborted under `panic = "abort"`, which looked like a hang.
///
/// The heap does NOT grow on demand (see `memory::heap`), so this is the hard
/// ceiling. QEMU is launched with 512 MiB, leaving ~256 MiB for the kernel
/// image, HHDM page tables, framebuffer, and PMM bookkeeping — comfortable.
/// The full Debian `main` index (~150 MiB decompressed) is intentionally larger
/// than the index decompression cap (`pkg::deb::MAX_INDEX_DECOMPRESSED`), so
/// `apt update` against it fails with a clear error rather than exhausting the
/// heap; use a smaller component / local mirror (or raise both limits + QEMU
/// `-m`) for a full index.
pub const HEAP_INITIAL_PAGES: u64 = 65536;

extern "C" {
    /// Start of the kernel image — provided by the linker script
    /// (`__kernel_start = 0xffffffff80000000`). This is the VIRTUAL
    /// higher-half base of the kernel image.
    static __kernel_start: u8;
    /// End of kernel image (BSS) — provided by the linker script
    /// (page-aligned). The heap begins at the next page boundary above this
    /// symbol.
    static __kernel_end: u8;
}

/// Virtual base address of the kernel image (`__kernel_start` linker symbol).
pub fn kernel_start() -> u64 {
    &raw const __kernel_start as u64
}

/// Virtual end address of the kernel image (`__kernel_end` linker symbol,
/// page-aligned by the linker script).
pub fn kernel_end() -> u64 {
    &raw const __kernel_end as u64
}

/// Size of the kernel image in bytes, computed from the linker symbols
/// (`__kernel_end - __kernel_start`). Both symbols are virtual higher-half
/// addresses laid out contiguously, so their difference is the image extent.
pub fn kernel_size() -> u64 {
    kernel_end() - kernel_start()
}

/// Canonical kernel heap base: the address of the linker symbol `__kernel_end`
/// rounded up to the next page boundary.
///
/// The heap base is dynamic (it depends on the linked image size), so it is
/// exposed as a function rather than a `const`.
pub fn heap_base() -> u64 {
    (kernel_end() + (PAGE_SIZE - 1)) & !(PAGE_SIZE - 1)
}

// ─── MMIO window ─────────────────────────────────────────────────────────────

// The MMIO window is, under the current scheme, the HHDM (higher-half direct
// map) region: device MMIO (LAPIC/IOAPIC/framebuffer) is reached at
// `HHDM_OFFSET + phys`. There is therefore no separate fixed MMIO base; the
// `virt = hhdm + phys` convention is centralized in `memory::vmm`
// (`phys_to_virt` / `map_mmio`).
