// arch/x86_64/gdt.rs — Global Descriptor Table, TSS, IST stacks
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;
use core::cell::SyncUnsafeCell;
use core::sync::atomic::{AtomicU16, Ordering};

// ─── IST (Interrupt Stack Table) stack allocations ─────────────────────

/// Double Fault IST stack (16KB, aligned to 16 bytes).
#[repr(C, align(16))]
struct IstStack([u8; 16384]);

// The IST stacks, TSS and GDT live in `SyncUnsafeCell`s rather than `static mut`
// items so that every access goes through the cell's `.get()` raw pointer and we
// never create a `&`/`&mut` reference to a mutable static (which the
// `static_mut_refs` lint forbids). None of these types are `Sync`, but
// `SyncUnsafeCell` supplies the `Sync` impl a `static` requires; soundness rests
// on the init-once, single-threaded, pre-interrupt invariant documented at each
// access site.
static IST1_DOUBLE_FAULT: SyncUnsafeCell<IstStack> = SyncUnsafeCell::new(IstStack([0; 16384]));
static IST2_PAGE_FAULT: SyncUnsafeCell<IstStack> = SyncUnsafeCell::new(IstStack([0; 16384]));

// ─── TSS ────────────────────────────────────────────────────────────────

static TSS: SyncUnsafeCell<TaskStateSegment> = SyncUnsafeCell::new(TaskStateSegment::new());

// ─── GDT ────────────────────────────────────────────────────────────────

static GDT: SyncUnsafeCell<GlobalDescriptorTable> =
    SyncUnsafeCell::new(GlobalDescriptorTable::new());

/// Public segment selectors, set after `init()`.
pub struct Selectors;

// Selector values are stored as raw `u16`s in `AtomicU16`s: `SegmentSelector` is
// a `#[repr(transparent)]` wrapper around a `u16`, so storing `.0` in `init` and
// rebuilding the wrapper on read is an exact, lock-free, reference-free
// substitute for the previous `static mut SegmentSelector` items.
static KERNEL_CODE_SEL: AtomicU16 = AtomicU16::new(0);
static KERNEL_DATA_SEL: AtomicU16 = AtomicU16::new(0);
static USER_CODE_SEL: AtomicU16 = AtomicU16::new(0);
static USER_DATA_SEL: AtomicU16 = AtomicU16::new(0);
static TSS_SEL: AtomicU16 = AtomicU16::new(0);

impl Selectors {
    pub fn kernel_code() -> SegmentSelector {
        // Set once during init, then read-only; rebuild the wrapper from the raw value.
        SegmentSelector(KERNEL_CODE_SEL.load(Ordering::Relaxed))
    }
    pub fn kernel_data() -> SegmentSelector {
        // Set once during init, then read-only; rebuild the wrapper from the raw value.
        SegmentSelector(KERNEL_DATA_SEL.load(Ordering::Relaxed))
    }
    pub fn user_code() -> SegmentSelector {
        // Set once during init, then read-only; rebuild the wrapper from the raw value.
        SegmentSelector(USER_CODE_SEL.load(Ordering::Relaxed))
    }
    pub fn user_data() -> SegmentSelector {
        // Set once during init, then read-only; rebuild the wrapper from the raw value.
        SegmentSelector(USER_DATA_SEL.load(Ordering::Relaxed))
    }
}

// ─── IST indices (must match IDT entries) ───────────────────────────────

pub const IST_DOUBLE_FAULT: u16 = 1;
pub const IST_PAGE_FAULT: u16 = 2;

// ─── TSS RSP0 (ring-3 → ring-0 stack) ───────────────────────────────────

/// Program the privileged stack pointer `TSS.privilege_stack_table[0]` (RSP0).
///
/// When the CPU takes an interrupt (e.g. the preemptive timer tick) or executes
/// `int 0x80` *while running in ring 3*, it performs a privilege-level switch
/// and loads the kernel stack pointer from RSP0 in the TSS. If RSP0 is left
/// unset (zero), that first ring-3 → ring-0 transition pushes onto address 0 and
/// faults. A ring-3 task therefore cannot run until RSP0 points at a valid
/// kernel stack.
///
/// `rsp0` must be the (exclusive) top of a mapped, 16-byte-aligned kernel stack.
/// The CPU re-reads this field from TSS memory on every privilege switch, so
/// updating it after `load_tss` (which we do per ring-3 task in
/// `task::process::create_user_process`) takes effect immediately.
///
/// LIMITATION (single-task): there is exactly one RSP0 slot, so this design
/// supports only ONE ring-3 task at a time. With multiple user tasks RSP0 would
/// have to be reprogrammed on every switch into a ring-3 task; that is out of
/// scope for the current single embedded-test-process bring-up.
pub fn set_kernel_stack(rsp0: u64) {
    // SAFETY: init-once, single-threaded, pre-interrupt invariant — the TSS is
    // owned by this module's `SyncUnsafeCell` and is only mutated here and in
    // `init`, with no concurrent access. RSP0 is written through the cell's raw
    // pointer as a single aligned 64-bit store; the CPU re-reads it on the next
    // privilege transition, so the write through `.get()` (never a `&mut` to a
    // static) takes effect immediately.
    unsafe {
        (*TSS.get()).privilege_stack_table[0] = VirtAddr::new(rsp0);
    }
}

// ─── init() ─────────────────────────────────────────────────────────────

/// Initialize GDT, TSS with IST stacks, and load them into the CPU.
///
/// SAFETY: Must be called once during early boot, before interrupts are enabled.
pub fn init() {
    // IST stack tops. `.get()` yields the `*mut IstStack` backing each cell; the
    // pointer-to-integer cast gives the stack base, and + 16384 gives its
    // (exclusive) top. No reference to a static is created.
    let df_stack_top = IST1_DOUBLE_FAULT.get() as u64 + 16384;
    let pf_stack_top = IST2_PAGE_FAULT.get() as u64 + 16384;

    // SAFETY: init-once, single-threaded, pre-interrupt invariant — exclusive
    // access to the TSS through its cell's raw pointer; no other code observes
    // or mutates the TSS during early boot.
    unsafe {
        let tss = TSS.get();
        (*tss).interrupt_stack_table[IST_DOUBLE_FAULT as usize] = VirtAddr::new(df_stack_top);
        (*tss).interrupt_stack_table[IST_PAGE_FAULT as usize] = VirtAddr::new(pf_stack_top);
    }

    // Build GDT descriptors.
    // SAFETY: init-once, single-threaded, pre-interrupt invariant — exclusive
    // access to the GDT through its cell's raw pointer. The `&'static TSS` fed to
    // `Descriptor::tss_segment` is taken from the TSS cell, which is fully
    // initialized above and never mutated concurrently; the reference only needs
    // to remain valid for the descriptor, and the TSS lives for the whole program.
    unsafe {
        let gdt = &mut *GDT.get();
        KERNEL_CODE_SEL.store(gdt.append(Descriptor::kernel_code_segment()).0, Ordering::Relaxed);
        KERNEL_DATA_SEL.store(gdt.append(Descriptor::kernel_data_segment()).0, Ordering::Relaxed);
        USER_CODE_SEL.store(gdt.append(Descriptor::user_code_segment()).0, Ordering::Relaxed);
        USER_DATA_SEL.store(gdt.append(Descriptor::user_data_segment()).0, Ordering::Relaxed);
        let tss_ref: &'static TaskStateSegment = &*TSS.get();
        TSS_SEL.store(gdt.append(Descriptor::tss_segment(tss_ref)).0, Ordering::Relaxed);
    }

    // Load GDT and TSS.
    // SAFETY: init-once, single-threaded, pre-interrupt invariant. The GDT holds
    // valid descriptors built above and the TSS is initialized; access is through
    // the GDT cell's raw pointer. The `&'static` borrow required by `load()` is
    // satisfied because the GDT lives for the whole program and is not mutated
    // after this point. The selector reloads use the just-stored atomic values.
    unsafe {
        let gdt: &'static GlobalDescriptorTable = &*GDT.get();
        gdt.load();
        // Reload code segment with a far jump
        core::arch::asm!(
            "push {sel}",
            "lea {tmp}, [2f + rip]",
            "push {tmp}",
            "retfq",
            "2:",
            sel = in(reg) KERNEL_CODE_SEL.load(Ordering::Relaxed) as u64,
            tmp = lateout(reg) _,
            options(preserves_flags),
        );
        // Reload data segments — INCLUDING SS.
        //
        // SS must be reloaded here, not just DS/ES/FS/GS. On entry the CPU is
        // still running with the stale stack selector the bootloader (Limine)
        // loaded — its 64-bit data selector happens to be 0x30. After
        // `GDT.load()` swaps in *our* GDT, selector 0x30 is the high half of
        // the 16-byte TSS system descriptor (GDT index 6), i.e. NOT a valid
        // data segment. If SS is left at 0x30, the first preemptive timer tick
        // pushes SS=0x30 into the interrupt frame and the matching `iretq`
        // (irq32_stub) reloads it against our GDT, faulting with #GP e=0x30.
        // Loading SS = KERNEL_DATA_SEL (0x10) makes the running stack selector
        // a valid present DPL0 data segment so the iret restore path is sound
        // (Requirements 11.1, 11.2). `mov ss` also naturally inhibits IRQs for
        // the following instruction, so RSP stays consistent across the load.
        core::arch::asm!(
            "mov ds, {sel:x}",
            "mov es, {sel:x}",
            "mov fs, {sel:x}",
            "mov gs, {sel:x}",
            "mov ss, {sel:x}",
            sel = in(reg) KERNEL_DATA_SEL.load(Ordering::Relaxed),
            options(nomem, nostack),
        );
        // Load TSS
        x86_64::instructions::tables::load_tss(SegmentSelector(TSS_SEL.load(Ordering::Relaxed)));
    }

    crate::debug!("GDT loaded: kernel CS={:#x}, DS={:#x}, user CS={:#x}, DS={:#x}, TSS={:#x}",
        KERNEL_CODE_SEL.load(Ordering::Relaxed),
        KERNEL_DATA_SEL.load(Ordering::Relaxed),
        USER_CODE_SEL.load(Ordering::Relaxed),
        USER_DATA_SEL.load(Ordering::Relaxed),
        TSS_SEL.load(Ordering::Relaxed),
    );
}
