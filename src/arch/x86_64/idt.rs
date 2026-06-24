// arch/x86_64/idt.rs — Interrupt Descriptor Table and exception handlers
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use core::cell::SyncUnsafeCell;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::registers::control::Cr2;
use x86_64::VirtAddr;

// The IDT lives in a `SyncUnsafeCell` rather than a `static mut` so that all
// access goes through the cell's `.get()` raw pointer, never through a
// reference to a mutable static (which the `static_mut_refs` lint forbids).
// `InterruptDescriptorTable` is not `Sync`, but `SyncUnsafeCell` provides the
// `Sync` impl required of a `static`; soundness is upheld by the init-once,
// single-threaded, pre-interrupt invariant documented at each access site.
static IDT: SyncUnsafeCell<InterruptDescriptorTable> =
    SyncUnsafeCell::new(InterruptDescriptorTable::new());

pub fn init() {
    // SAFETY: `init` is called exactly once during early boot, on the bootstrap
    // CPU, with interrupts still disabled and before any other code can observe
    // the IDT. No other thread or interrupt handler can access `IDT` while we
    // hold this `&mut`, so building the table through the cell's raw pointer is
    // free of aliasing.
    let idt = unsafe { &mut *IDT.get() };

    idt.divide_error.set_handler_fn(divide_error_handler);
    idt.debug.set_handler_fn(debug_handler);
    idt.non_maskable_interrupt.set_handler_fn(nmi_handler);
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    idt.overflow.set_handler_fn(overflow_handler);
    idt.bound_range_exceeded.set_handler_fn(bound_range_handler);
    idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
    idt.device_not_available.set_handler_fn(device_not_available_handler);
    idt.double_fault.set_handler_fn(double_fault_handler);
    idt.invalid_tss.set_handler_fn(invalid_tss_handler);
    idt.segment_not_present.set_handler_fn(segment_not_present_handler);
    idt.stack_segment_fault.set_handler_fn(stack_segment_handler);
    idt.general_protection_fault.set_handler_fn(gp_fault_handler);
    idt.page_fault.set_handler_fn(page_fault_handler);
    idt.x87_floating_point.set_handler_fn(x87_fpu_handler);
    idt.alignment_check.set_handler_fn(alignment_check_handler);
    idt.machine_check.set_handler_fn(machine_check_handler);
    idt.simd_floating_point.set_handler_fn(simd_fpu_handler);
    idt.virtualization.set_handler_fn(virtualization_handler);
    idt.hv_injection_exception.set_handler_fn(hv_injection_handler);
    idt.vmm_communication_exception.set_handler_fn(vmm_comm_handler);
    idt.security_exception.set_handler_fn(security_handler);

    // Vector 32: custom assembly stub for preemptive context switch.
    // SAFETY: the address is the entry point of the naked `irq32_stub`, which
    // implements the full interrupt prologue/epilogue contract for vector 32.
    unsafe {
        idt[32].set_handler_addr(VirtAddr::new(crate::task::switch::irq32_stub as *const () as u64));
    }
    // Vectors 33–47: standard IRQ handlers
    idt[33].set_handler_fn(irq33_handler);
    idt[34].set_handler_fn(irq34_handler);
    idt[35].set_handler_fn(irq35_handler);
    idt[36].set_handler_fn(irq36_handler);
    idt[37].set_handler_fn(irq37_handler);
    idt[38].set_handler_fn(irq38_handler);
    idt[39].set_handler_fn(irq39_handler);
    idt[40].set_handler_fn(irq40_handler);
    idt[41].set_handler_fn(irq41_handler);
    idt[42].set_handler_fn(irq42_handler);
    idt[43].set_handler_fn(irq43_handler);
    idt[44].set_handler_fn(irq44_handler);
    idt[45].set_handler_fn(irq45_handler);
    idt[46].set_handler_fn(irq46_handler);
    idt[47].set_handler_fn(irq47_handler);

    // Vector 0x80: ring-3-invokable system-call gate. DPL=3 so user code is
    // permitted to execute `int 0x80`; the naked stub marshals args and
    // dispatches (see `arch::x86_64::syscall::int80_stub`). Using a software
    // interrupt for syscalls reuses the CPU's automatic RSP0 stack switch and
    // clean `iretq` return to ring 3.
    // SAFETY: the address is the entry point of the naked `int80_stub`, which
    // honors the interrupt-gate calling contract for vector 0x80.
    unsafe {
        idt[0x80]
            .set_handler_addr(VirtAddr::new(crate::arch::x86_64::syscall::int80_stub as *const () as u64))
            .set_privilege_level(x86_64::PrivilegeLevel::Ring3);
    }

    // SAFETY: `load()` requires `&'static self`. The shared reference is derived
    // from the `'static` cell's pointer and is sound because the table is now
    // fully initialized and is never mutated again after this point — all later
    // access is read-only by the CPU when dispatching interrupts. The init-once
    // invariant above guarantees no concurrent `&mut` exists.
    unsafe { &*IDT.get() }.load();

    crate::debug!("IDT loaded: 32 exceptions + 16 IRQ (vec32=stub)");
}

// ─── Exception handlers ──────────────────────────────────────────────────

extern "x86-interrupt" fn divide_error_handler(stack: InterruptStackFrame) {
    crate::error!("[EXC #0] Divide Error RIP: 0x{:016x}", stack.instruction_pointer.as_u64());
    halt();
}
extern "x86-interrupt" fn debug_handler(stack: InterruptStackFrame) {
    crate::error!("[EXC #1] Debug RIP: 0x{:016x}", stack.instruction_pointer.as_u64());
    halt();
}
extern "x86-interrupt" fn nmi_handler(stack: InterruptStackFrame) {
    crate::error!("[EXC #2] NMI RIP: 0x{:016x}", stack.instruction_pointer.as_u64());
    halt();
}
extern "x86-interrupt" fn breakpoint_handler(stack: InterruptStackFrame) {
    crate::error!("[EXC #3] *** BREAKPOINT HIT *** RIP: 0x{:016x}", stack.instruction_pointer.as_u64());
    crate::error!("[EXC #3] IDT is working correctly!");
}
extern "x86-interrupt" fn overflow_handler(stack: InterruptStackFrame) {
    crate::error!("[EXC #4] Overflow RIP: 0x{:016x}", stack.instruction_pointer.as_u64());
    halt();
}
extern "x86-interrupt" fn bound_range_handler(stack: InterruptStackFrame) {
    crate::error!("[EXC #5] Bound Range RIP: 0x{:016x}", stack.instruction_pointer.as_u64());
    halt();
}
extern "x86-interrupt" fn invalid_opcode_handler(stack: InterruptStackFrame) {
    crate::error!("[EXC #6] Invalid Opcode RIP: 0x{:016x}", stack.instruction_pointer.as_u64());
    halt();
}
extern "x86-interrupt" fn device_not_available_handler(stack: InterruptStackFrame) {
    crate::error!("[EXC #7] Device NA RIP: 0x{:016x}", stack.instruction_pointer.as_u64());
    halt();
}
extern "x86-interrupt" fn double_fault_handler(stack: InterruptStackFrame, error_code: u64) -> ! {
    crate::error!("[EXC #8] DOUBLE FAULT err=0x{:x} RIP=0x{:016x}", error_code, stack.instruction_pointer.as_u64());
    crate::arch::cpu::halt_loop()
}
extern "x86-interrupt" fn invalid_tss_handler(_stack: InterruptStackFrame, error_code: u64) {
    crate::error!("[EXC #10] Invalid TSS err=0x{:x}", error_code);
    halt();
}
extern "x86-interrupt" fn segment_not_present_handler(_stack: InterruptStackFrame, error_code: u64) {
    crate::error!("[EXC #11] Segment NP err=0x{:x}", error_code);
    halt();
}
extern "x86-interrupt" fn stack_segment_handler(_stack: InterruptStackFrame, error_code: u64) {
    crate::error!("[EXC #12] Stack Fault err=0x{:x}", error_code);
    halt();
}
extern "x86-interrupt" fn gp_fault_handler(stack: InterruptStackFrame, error_code: u64) {
    crate::error!("[EXC #13] GP Fault err=0x{:x} RIP=0x{:016x}", error_code, stack.instruction_pointer.as_u64());
    halt();
}
extern "x86-interrupt" fn page_fault_handler(stack: InterruptStackFrame, error_code: PageFaultErrorCode) {
    let fault_addr = Cr2::read().unwrap_or(VirtAddr::new(0));
    crate::error!("[EXC #14] PAGE FAULT addr=0x{:016x} RIP=0x{:016x} P={} W={} U={}",
        fault_addr.as_u64(), stack.instruction_pointer.as_u64(),
        error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION),
        error_code.contains(PageFaultErrorCode::CAUSED_BY_WRITE),
        error_code.contains(PageFaultErrorCode::USER_MODE));
    halt();
}
extern "x86-interrupt" fn x87_fpu_handler(stack: InterruptStackFrame) {
    crate::error!("[EXC #16] x87 FPU RIP=0x{:016x}", stack.instruction_pointer.as_u64());
    halt();
}
extern "x86-interrupt" fn alignment_check_handler(_stack: InterruptStackFrame, error_code: u64) {
    crate::error!("[EXC #17] Alignment err=0x{:x}", error_code);
    halt();
}
extern "x86-interrupt" fn machine_check_handler(_stack: InterruptStackFrame) -> ! {
    crate::error!("[EXC #18] Machine Check — halting");
    crate::arch::cpu::halt_loop()
}
extern "x86-interrupt" fn simd_fpu_handler(stack: InterruptStackFrame) {
    crate::error!("[EXC #19] SIMD FPU RIP=0x{:016x}", stack.instruction_pointer.as_u64());
    halt();
}
extern "x86-interrupt" fn virtualization_handler(stack: InterruptStackFrame) {
    crate::error!("[EXC #20] Virt RIP=0x{:016x}", stack.instruction_pointer.as_u64());
    halt();
}
extern "x86-interrupt" fn hv_injection_handler(stack: InterruptStackFrame) {
    crate::error!("[EXC #28] HV Injection RIP=0x{:016x}", stack.instruction_pointer.as_u64());
    halt();
}
extern "x86-interrupt" fn vmm_comm_handler(_stack: InterruptStackFrame, error_code: u64) {
    crate::error!("[EXC #29] VMM Comm err=0x{:x}", error_code);
    halt();
}
extern "x86-interrupt" fn security_handler(_stack: InterruptStackFrame, error_code: u64) {
    crate::error!("[EXC #30] Security err=0x{:x}", error_code);
    halt();
}

// ─── IRQ handlers (vectors 33–47) ───────────────────────────────────────

macro_rules! irq_handler {
    ($name:ident, $vec:expr) => {
        extern "x86-interrupt" fn $name(_stack: InterruptStackFrame) {
            crate::arch::x86_64::apic::irq_dispatch($vec);
            crate::arch::x86_64::apic::send_eoi();
        }
    };
}

irq_handler!(irq33_handler, 33);
irq_handler!(irq34_handler, 34);
irq_handler!(irq35_handler, 35);
irq_handler!(irq36_handler, 36);
irq_handler!(irq37_handler, 37);
irq_handler!(irq38_handler, 38);
irq_handler!(irq39_handler, 39);
irq_handler!(irq40_handler, 40);
irq_handler!(irq41_handler, 41);
irq_handler!(irq42_handler, 42);
irq_handler!(irq43_handler, 43);
irq_handler!(irq44_handler, 44);
irq_handler!(irq45_handler, 45);
irq_handler!(irq46_handler, 46);
irq_handler!(irq47_handler, 47);

fn halt() -> ! {
    crate::arch::cpu::halt_loop()
}
