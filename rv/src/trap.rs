//! Supervisor trap handling with a full saved frame, enabling **preemptive**
//! scheduling: the entry saves all GPRs plus `sepc`/`sstatus` into a 34-slot
//! frame on a per-trap kernel stack (selected via `sscratch`), the Rust
//! dispatcher may swap that frame between threads (timer preemption), and the
//! exit restores `sstatus`/`sepc`/GPRs and `sret`s into whatever thread the
//! frame now describes.
//!
//! Frame layout (`frame[i]`): 1=ra, 2=interrupted sp, 3..31 = the rest of
//! x3..x31, 32 = sepc, 33 = sstatus.

use core::arch::asm;

/// Dedicated kernel trap stack (interrupts are masked in-handler, no nesting).
const TRAP_STACK_SIZE: usize = 32 * 1024;

#[repr(align(16))]
struct TrapStack([u8; TRAP_STACK_SIZE]);

static mut TRAP_STACK: TrapStack = TrapStack([0; TRAP_STACK_SIZE]);

/// Saved-frame slot count (32 GPRs + sepc + sstatus); the on-stack frame is
/// padded to 288 bytes (16-aligned).
pub const FRAME_SLOTS: usize = 34;

core::arch::global_asm!(
    r#"
    .section .text
    .align 2
    .globl __trap_entry
__trap_entry:
    csrrw sp, sscratch, sp        # sp = kernel trap stack top; sscratch = interrupted sp
    addi sp, sp, -288
    sd ra,   1*8(sp)
    sd t0,   5*8(sp)
    csrr t0, sscratch             # t0 = interrupted sp
    sd t0,   2*8(sp)
    addi t0, sp, 288              # restore sscratch to the trap-stack top
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
    csrr t0, sepc
    sd t0,  32*8(sp)
    csrr t0, sstatus
    sd t0,  33*8(sp)
    mv a0, sp
    call __trap_handler
    ld t0,  33*8(sp)
    csrw sstatus, t0
    ld t0,  32*8(sp)
    csrw sepc, t0
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
    ld sp,   2*8(sp)
    sret
"#
);

extern "C" {
    fn __trap_entry();
}

const SCAUSE_S_TIMER: usize = 5;
const SCAUSE_S_EXTERNAL: usize = 9;
const SCAUSE_ECALL_U: usize = 8;

/// Install the trap vector and point `sscratch` at the kernel trap stack.
pub fn init() {
    // SAFETY: addr_of! avoids a reference to the mutable static; the top-of-stack
    // value satisfies the trap-entry contract.
    unsafe {
        let top = core::ptr::addr_of!(TRAP_STACK) as usize + TRAP_STACK_SIZE;
        crate::cpu::write_sscratch(top);
        crate::cpu::write_stvec(__trap_entry as *const () as usize);
    }
}

/// Rust trap dispatcher. `frame` points at the [`FRAME_SLOTS`]-slot saved array.
#[no_mangle]
extern "C" fn __trap_handler(frame: *mut usize) {
    let scause: usize;
    // SAFETY: side-effect-free CSR read.
    unsafe { asm!("csrr {}, scause", out(reg) scause, options(nomem, nostack)) };

    let is_interrupt = (scause >> (usize::BITS - 1)) & 1 == 1;
    let code = scause & 0xfff;

    if is_interrupt && code == SCAUSE_S_TIMER {
        crate::timer::on_tick();
        // Preempt: may swap `*frame` to another thread's saved frame.
        crate::sched::preempt(frame);
        return;
    }

    if is_interrupt && code == SCAUSE_S_EXTERNAL {
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
        // System call: a7=number, a0=arg; return in a0; resume past the ecall by
        // advancing the *saved* sepc (it is restored from the frame on exit).
        let (a7, a0) = unsafe { (*frame.add(17), *frame.add(10)) };
        let ret = crate::umode::syscall(a7, a0);
        unsafe {
            *frame.add(10) = ret;
            *frame.add(32) += 4;
        }
        return;
    }

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
