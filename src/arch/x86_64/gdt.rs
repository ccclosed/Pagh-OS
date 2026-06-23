// arch/x86_64/gdt.rs — Global Descriptor Table, TSS, IST stacks
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::PrivilegeLevel;
use x86_64::VirtAddr;
use core::ptr::addr_of;

// ─── IST (Interrupt Stack Table) stack allocations ─────────────────────

/// Double Fault IST stack (16KB, aligned to 16 bytes).
#[repr(C, align(16))]
struct IstStack([u8; 16384]);

static mut IST1_DOUBLE_FAULT: IstStack = IstStack([0; 16384]);
static mut IST2_PAGE_FAULT: IstStack = IstStack([0; 16384]);

// ─── TSS ────────────────────────────────────────────────────────────────

static mut TSS: TaskStateSegment = TaskStateSegment::new();

// ─── GDT ────────────────────────────────────────────────────────────────

static mut GDT: GlobalDescriptorTable = GlobalDescriptorTable::new();

/// Public segment selectors, set after `init()`.
pub struct Selectors;

// These are set by `init()` and immutable thereafter.
static mut KERNEL_CODE_SEL: SegmentSelector = SegmentSelector::new(0, PrivilegeLevel::Ring0);
static mut KERNEL_DATA_SEL: SegmentSelector = SegmentSelector::new(0, PrivilegeLevel::Ring0);
static mut USER_CODE_SEL: SegmentSelector = SegmentSelector::new(0, PrivilegeLevel::Ring0);
static mut USER_DATA_SEL: SegmentSelector = SegmentSelector::new(0, PrivilegeLevel::Ring0);
static mut TSS_SEL: SegmentSelector = SegmentSelector::new(0, PrivilegeLevel::Ring0);

impl Selectors {
    pub fn kernel_code() -> SegmentSelector {
        // SAFETY: Set once during init, then read-only.
        unsafe { KERNEL_CODE_SEL }
    }
    pub fn kernel_data() -> SegmentSelector {
        // SAFETY: Set once during init, then read-only.
        unsafe { KERNEL_DATA_SEL }
    }
    pub fn user_code() -> SegmentSelector {
        // SAFETY: Set once during init, then read-only.
        unsafe { USER_CODE_SEL }
    }
    pub fn user_data() -> SegmentSelector {
        // SAFETY: Set once during init, then read-only.
        unsafe { USER_DATA_SEL }
    }
    pub fn tss() -> SegmentSelector {
        // SAFETY: Set once during init, then read-only.
        unsafe { TSS_SEL }
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
    // SAFETY: `TSS` is a module-private mutable static. RSP0 is written here and
    // read by the CPU on privilege transitions; the write is a single aligned
    // 64-bit store and there is no concurrent mutation of this field elsewhere.
    unsafe {
        TSS.privilege_stack_table[0] = VirtAddr::new(rsp0);
    }
}

// ─── init() ─────────────────────────────────────────────────────────────

/// Initialize GDT, TSS with IST stacks, and load them into the CPU.
///
/// SAFETY: Must be called once during early boot, before interrupts are enabled.
pub fn init() {
    // SAFETY: Setting up IST stacks during init — no concurrent access.
    unsafe {
        // Set IST entries in TSS
        let df_stack_top = addr_of!(IST1_DOUBLE_FAULT) as u64 + 16384;
        let pf_stack_top = addr_of!(IST2_PAGE_FAULT) as u64 + 16384;

        TSS.interrupt_stack_table[IST_DOUBLE_FAULT as usize] =
            VirtAddr::new(df_stack_top);
        TSS.interrupt_stack_table[IST_PAGE_FAULT as usize] =
            VirtAddr::new(pf_stack_top);
    }

    // Build GDT descriptors
    // SAFETY: GDT is a mutable static; init is called once.
    unsafe {
        KERNEL_CODE_SEL = GDT.append(Descriptor::kernel_code_segment());
        KERNEL_DATA_SEL = GDT.append(Descriptor::kernel_data_segment());
        USER_CODE_SEL = GDT.append(Descriptor::user_code_segment());
        USER_DATA_SEL = GDT.append(Descriptor::user_data_segment());
        TSS_SEL = GDT.append(Descriptor::tss_segment(&TSS));
    }

    // Load GDT and TSS
    // SAFETY: GDT contains valid descriptors; TSS is properly initialized.
    unsafe {
        GDT.load();
        // Reload code segment with a far jump
        core::arch::asm!(
            "push {sel}",
            "lea {tmp}, [2f + rip]",
            "push {tmp}",
            "retfq",
            "2:",
            sel = in(reg) KERNEL_CODE_SEL.0 as u64,
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
            sel = in(reg) KERNEL_DATA_SEL.0,
            options(nomem, nostack),
        );
        // Load TSS
        x86_64::instructions::tables::load_tss(TSS_SEL);
    }

    crate::debug!("GDT loaded: kernel CS={:#x}, DS={:#x}, user CS={:#x}, DS={:#x}, TSS={:#x}",
        unsafe { KERNEL_CODE_SEL.0 },
        unsafe { KERNEL_DATA_SEL.0 },
        unsafe { USER_CODE_SEL.0 },
        unsafe { USER_DATA_SEL.0 },
        unsafe { TSS_SEL.0 },
    );
}
