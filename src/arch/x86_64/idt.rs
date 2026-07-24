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
/// Last syscall observed by `linux_dispatch`, for #GP post-mortem context
/// (pid in the high-level sense, raw syscall nr). Written on every dispatch.
pub static LAST_SYSCALL_PID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(u64::MAX);
pub static LAST_SYSCALL_NR: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(u64::MAX);

/// Record the syscall a task is about to run (called from `linux_dispatch`).
pub fn note_syscall(pid: u64, nr: u64) {
    LAST_SYSCALL_PID.store(pid, core::sync::atomic::Ordering::Relaxed);
    LAST_SYSCALL_NR.store(nr, core::sync::atomic::Ordering::Relaxed);
}

extern "x86-interrupt" fn gp_fault_handler(stack: InterruptStackFrame, error_code: u64) {
    crate::error!("[EXC #13] GP Fault err=0x{:x} RIP=0x{:016x} RSP=0x{:016x}", error_code, stack.instruction_pointer.as_u64(), stack.stack_pointer.as_u64());
    let pid = crate::task::scheduler::current_pid();
    crate::error!("[EXC #13] current pid={} last_dispatch: pid={} nr={}", pid,
        LAST_SYSCALL_PID.load(core::sync::atomic::Ordering::Relaxed),
        LAST_SYSCALL_NR.load(core::sync::atomic::Ordering::Relaxed));
    // For iretq/sysretq exit faults: dump the words the return instruction
    // was consuming (at RSP) plus the GPR area popped just before it.
    let frsp = stack.stack_pointer.as_u64();
    if frsp >= 0xffff_8000_0000_0000 {
        crate::error!("--- Words around fault RSP 0x{:x} ---", frsp);
        let mut off: i64 = -0x48;
        while off <= 0x28 {
            let addr = (frsp as i64 + off) as u64;
            let sign = if off < 0 { "-" } else { "+" };
            // Never touch unmapped memory from inside the #GP handler: the
            // fault RSP can sit right at the stack top, and reading past it
            // cascades into a page fault on the guard page.
            if crate::memory::vmm::virt_to_phys(addr).is_none() {
                crate::error!("  [rsp{}0x{:02x}] <unmapped>", sign, off.unsigned_abs());
            } else {
                let val = unsafe { core::ptr::read_volatile(addr as *const u64) };
                crate::error!("  [rsp{}0x{:02x}] 0x{:016x}", sign, off.unsigned_abs(), val);
            }
            off += 8;
        }
    }
    // Post-mortem for syscall-exit faults (iretq/sysretq in the stubs): by the
    // time the exit tail faults, the window frame has been popped but its
    // bytes are still intact at the top of the task's kernel stack. Dump them:
    //   [top-0x08] per-task user-RSP slot   [top-0x10] rax  [top-0x18] rbx
    //   [top-0x20] rcx (sysretq user RIP!)  [top-0x60] r11 (user RFLAGS) ...
    if !crate::task::scheduler::is_idle(pid) {
        let (_guard, _base, top) = crate::memory::layout::kernel_stack_for_pid(pid);
        crate::error!("--- Top 24 words of kernel stack for pid {} (top=0x{:x}) ---", pid, top);
        let mut i = 24u64;
        while i >= 1 {
            let addr = top - i * 8;
            let val = unsafe { core::ptr::read_volatile(addr as *const u64) };
            crate::error!("  [top-0x{:02x}] 0x{:016x}", i * 8, val);
            i -= 1;
        }
    }
    // STAGE-13.8 USER-FAULT ISOLATION: a #GP taken while executing ring-3
    // code is a bug in the user program (or our compat layer), not in the
    // kernel. Kill only the offending Compat_Process (exit code 139 =
    // 128+SIGSEGV) and keep the kernel and shell running; previously this
    // halted the whole machine.
    if stack.instruction_pointer.as_u64() < 0x8000_0000_0000
        && crate::task::compat::compat_exists(pid)
    {
        crate::task::compat::with_current_compat(|cs| cs.exit_code = Some(139));
        crate::error!("[EXC #13] killing Compat_Process pid={} (SIGSEGV) - kernel keeps running", pid);
        crate::task::scheduler::exit_current();
    }
    halt();
}
extern "x86-interrupt" fn page_fault_handler(stack: InterruptStackFrame, error_code: PageFaultErrorCode) {
    let fault_addr = Cr2::read().unwrap_or(VirtAddr::new(0));
    let rsp = stack.stack_pointer.as_u64();
    crate::error!(
        "[EXC #14] PAGE FAULT addr=0x{:016x} RIP=0x{:016x} RSP=0x{:016x} P={} W={} U={} I={} ec=0x{:x}",
        fault_addr.as_u64(),
        stack.instruction_pointer.as_u64(),
        rsp,
        error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION),
        error_code.contains(PageFaultErrorCode::CAUSED_BY_WRITE),
        error_code.contains(PageFaultErrorCode::USER_MODE),
        error_code.contains(PageFaultErrorCode::INSTRUCTION_FETCH),
        error_code.bits(),
    );
    // For a kernel-mode fault, RIP/RBP may be garbage (control-flow corruption,
    // e.g. RIP=0x1), so dump a heap-free stack scan of the faulting stack for
    // return addresses in the kernel image — this reconstructs the call chain
    // that was live at the fault. Then park (do not return: returning would just
    // re-execute the faulting instruction and loop).
    if !error_code.contains(PageFaultErrorCode::USER_MODE) {
        crate::debug::unwind::stack_scan_backtrace(rsp, 8192);
        crate::arch::cpu::halt_loop();
    }
    // STAGE-13.8 USER-FAULT ISOLATION: a ring-3 page fault must not take the
    // machine down. Print post-mortem context and the top of the *user*
    // stack (return-address candidates for symbolizing the crash), then kill
    // only the faulting Compat_Process and keep the kernel running.
    let pid = crate::task::scheduler::current_pid();
    crate::error!("[EXC #14] current pid={} last_dispatch: pid={} nr={}", pid,
        LAST_SYSCALL_PID.load(core::sync::atomic::Ordering::Relaxed),
        LAST_SYSCALL_NR.load(core::sync::atomic::Ordering::Relaxed));
    // STAGE-16.1 DIAG: which address space was live, and at which level does
    // the translation of the faulting address (and of RIP) break?
    crate::error!("[EXC #14] CR3=0x{:016x}", crate::memory::vmm::current_pml4_phys());
    crate::memory::vmm::dump_translation(fault_addr.as_u64());
    crate::memory::vmm::dump_translation(stack.instruction_pointer.as_u64());
    crate::error!("--- Top 24 words of user stack (rsp=0x{:x}) ---", rsp);
    let mut i = 0u64;
    while i < 24 {
        let addr = rsp.wrapping_add(i * 8);
        if crate::memory::vmm::virt_to_phys(addr).is_none() {
            crate::error!("  [rsp+0x{:02x}] <unmapped>", i * 8);
        } else {
            let val = unsafe { core::ptr::read_volatile(addr as *const u64) };
            crate::error!("  [rsp+0x{:02x}] 0x{:016x}", i * 8, val);
        }
        i += 1;
    }
    if crate::task::compat::compat_exists(pid) {
        crate::task::compat::with_current_compat(|cs| cs.exit_code = Some(139));
        crate::error!("[EXC #14] killing Compat_Process pid={} (SIGSEGV) - kernel keeps running", pid);
        crate::task::scheduler::exit_current();
    }
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
