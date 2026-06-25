//! Supervisor trap handling. `stvec` points at `__trap_entry` (direct mode),
//! which saves the integer registers, calls the Rust dispatcher, restores them,
//! and returns via `sret`. For now only the supervisor timer interrupt is
//! handled; any other trap is reported and the hart is parked (it should not
//! occur before user mode / device IRQs are wired in later milestones).

use core::arch::asm;

// Trap entry: save x1, x3..x31 (skip x0=zero and x2=sp, which we adjust by hand)
// into a 256-byte frame, call the dispatcher, restore, and `sret`. `.align 2`
// keeps the entry 4-byte aligned as `stvec` requires (direct mode = low bits 0).
core::arch::global_asm!(
    r#"
    .section .text
    .align 2
    .globl __trap_entry
__trap_entry:
    addi sp, sp, -256
    sd ra,   1*8(sp)
    sd gp,   3*8(sp)
    sd tp,   4*8(sp)
    sd t0,   5*8(sp)
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
    addi sp, sp, 256
    sret
"#
);

extern "C" {
    fn __trap_entry();
}

/// `scause` code for a supervisor timer interrupt.
const SCAUSE_S_TIMER: usize = 5;

/// Install the trap vector.
pub fn init() {
    // SAFETY: `__trap_entry` is a valid, aligned trap handler that returns via sret.
    unsafe { crate::cpu::write_stvec(__trap_entry as *const () as usize) };
}

/// Rust trap dispatcher, called from `__trap_entry` with all GPRs saved.
#[no_mangle]
extern "C" fn __trap_handler() {
    let scause: usize;
    // SAFETY: reading trap CSRs is side-effect-free.
    unsafe { asm!("csrr {}, scause", out(reg) scause, options(nomem, nostack)) };

    let is_interrupt = (scause >> (usize::BITS - 1)) & 1 == 1;
    let code = scause & 0xfff;

    if is_interrupt && code == SCAUSE_S_TIMER {
        crate::timer::on_tick();
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
