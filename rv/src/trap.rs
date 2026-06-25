//! Supervisor trap handling. `stvec` points at `__trap_entry` (direct mode),
//! which switches to a dedicated kernel trap stack (via `sscratch`), saves the
//! integer registers, calls the Rust dispatcher, restores them, and returns via
//! `sret`. Handles the supervisor timer interrupt and `ecall` from U-mode; any
//! other trap is reported and the hart is parked.
//!
//! ## Why the `sscratch` stack swap
//! On a trap from U-mode the hardware does not change `sp`, so the handler would
//! otherwise run on the *user* stack (a U-page), which S-mode may not touch
//! (no `SUM`) — causing a nested-fault storm. We keep `sscratch` = the kernel
//! trap-stack top at all times; the entry swaps `sp`<->`sscratch`, runs the
//! handler on the kernel stack, stashes the interrupted `sp` in the frame, and
//! restores it on exit. This works uniformly for traps from kernel or user mode.

use core::arch::asm;

/// Dedicated kernel trap stack (interrupts are masked in-handler, so one is
/// enough — no nesting).
const TRAP_STACK_SIZE: usize = 16 * 1024;

#[repr(align(16))]
struct TrapStack([u8; TRAP_STACK_SIZE]);

static mut TRAP_STACK: TrapStack = TrapStack([0; TRAP_STACK_SIZE]);

// Trap entry. Frame layout (offset = i*8 holds x_i): x1=ra, x2=interrupted sp
// (stashed from sscratch), x3..x31 as usual. `.align 2` keeps it 4-byte aligned
// for stvec direct mode.
core::arch::global_asm!(
    r#"
    .section .text
    .align 2
    .globl __trap_entry
__trap_entry:
    csrrw sp, sscratch, sp        # sp = kernel trap stack top; sscratch = interrupted sp
    addi sp, sp, -256
    sd ra,   1*8(sp)
    sd t0,   5*8(sp)
    csrr t0, sscratch             # t0 = interrupted sp
    sd t0,   2*8(sp)              # frame[x2] = interrupted sp
    addi t0, sp, 256              # restore sscratch to the trap-stack top
    csrw sscratch, t0
    sd gp,   3*8(sp)
    sd tp,   4*8(sp)
    sd t1,   6*8(sp)
    sd t2,   7*8(sp)
    sd s0,   8*8(sp)
    sd s1,   9*8(sp)
    sd a0,  10*8(sp)
    sd a1,  11*8(sp)
    sd a2,  12*8(sp)
    sd a3,  13*8(sp)
    sd a4,  14*8(sp)
    sd a5,  15*8(sp)
    sd a6,  16*8(sp)
    sd a7,  17*8(sp)
    sd s2,  18*8(sp)
    sd s3,  19*8(sp)
    sd s4,  20*8(sp)
    sd s5,  21*8(sp)
    sd s6,  22*8(sp)
    sd s7,  23*8(sp)
    sd s8,  24*8(sp)
    sd s9,  25*8(sp)
    sd s10, 26*8(sp)
    sd s11, 27*8(sp)
    sd t3,  28*8(sp)
    sd t4,  29*8(sp)
    sd t5,  30*8(sp)
    sd t6,  31*8(sp)
    mv a0, sp
    call __trap_handler
    ld ra,   1*8(sp)
    ld gp,   3*8(sp)
    ld tp,   4*8(sp)
    ld t0,   5*8(sp)
    ld t1,   6*8(sp)
    ld t2,   7*8(sp)
    ld s0,   8*8(sp)
    ld s1,   9*8(sp)
    ld a0,  10*8(sp)
    ld a1,  11*8(sp)
    ld a2,  12*8(sp)
    ld a3,  13*8(sp)
    ld a4,  14*8(sp)
    ld a5,  15*8(sp)
    ld a6,  16*8(sp)
    ld a7,  17*8(sp)
    ld s2,  18*8(sp)
    ld s3,  19*8(sp)
    ld s4,  20*8(sp)
    ld s5,  21*8(sp)
    ld s6,  22*8(sp)
    ld s7,  23*8(sp)
    ld s8,  24*8(sp)
    ld s9,  25*8(sp)
    ld s10, 26*8(sp)
    ld s11, 27*8(sp)
    ld t3,  28*8(sp)
    ld t4,  29*8(sp)
    ld t5,  30*8(sp)
    ld t6,  31*8(sp)
    ld sp,   2*8(sp)              # restore interrupted sp (also leaves the frame)
    sret
"#
);

extern "C" {
    fn __trap_entry();
}

/// `scause` code for a supervisor timer interrupt.
const SCAUSE_S_TIMER: usize = 5;
/// `scause` code for a supervisor external interrupt (PLIC).
const SCAUSE_S_EXTERNAL: usize = 9;
/// `scause` code for an environment call from U-mode.
const SCAUSE_ECALL_U: usize = 8;

/// Install the trap vector and point `sscratch` at the kernel trap stack.
pub fn init() {
    // SAFETY: addr_of! avoids forming a reference to the mutable static; the
    // top-of-stack value satisfies the trap-entry contract.
    unsafe {
        let top = core::ptr::addr_of!(TRAP_STACK) as usize + TRAP_STACK_SIZE;
        crate::cpu::write_sscratch(top);
        crate::cpu::write_stvec(__trap_entry as *const () as usize);
    }
}

/// Rust trap dispatcher, called from `__trap_entry` with all GPRs saved on the
/// kernel trap stack. `frame[i]` is `x_i` (e.g. `frame[10]`=a0, `frame[17]`=a7).
#[no_mangle]
extern "C" fn __trap_handler(frame: *mut usize) {
    let scause: usize;
    // SAFETY: reading trap CSRs is side-effect-free.
    unsafe { asm!("csrr {}, scause", out(reg) scause, options(nomem, nostack)) };

    let is_interrupt = (scause >> (usize::BITS - 1)) & 1 == 1;
    let code = scause & 0xfff;

    if is_interrupt && code == SCAUSE_S_TIMER {
        crate::timer::on_tick();
        return;
    }

    if is_interrupt && code == SCAUSE_S_EXTERNAL {
        // PLIC: claim the pending IRQ, service it, complete it.
        let irq = crate::plic::claim();
        if irq == crate::plic::UART_IRQ {
            crate::uart::drain_rx();
        }
        if irq != 0 {
            crate::plic::complete(irq);
        }
        return;
    }

    if !is_interrupt && code == SCAUSE_ECALL_U {
        // System call: number in a7, first arg in a0; return value back in a0.
        // SAFETY: `frame` is the saved register array from __trap_entry.
        let (a7, a0) = unsafe { (*frame.add(17), *frame.add(10)) };
        let ret = crate::umode::syscall(a7, a0);
        unsafe { *frame.add(10) = ret };

        // Advance past the 4-byte `ecall` so `sret` resumes at the next insn.
        let mut sepc: usize;
        // SAFETY: side-effect-free CSR read/write of sepc.
        unsafe {
            asm!("csrr {}, sepc", out(reg) sepc, options(nomem, nostack));
            sepc += 4;
            asm!("csrw sepc, {}", in(reg) sepc, options(nomem, nostack));
        }
        return;
    }

    // Anything else is unexpected at this stage: report and park.
    let sepc: usize;
    let stval: usize;
    // SAFETY: side-effect-free CSR reads.
    unsafe {
        asm!("csrr {}, sepc", out(reg) sepc, options(nomem, nostack));
        asm!("csrr {}, stval", out(reg) stval, options(nomem, nostack));
    }
    crate::kprintln!(
        "rv: UNEXPECTED TRAP scause={:#x} sepc={:#x} stval={:#x} -- parking",
        scause,
        sepc,
        stval
    );
    crate::cpu::park();
}
