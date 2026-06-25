//! Safe-ish wrappers around RISC-V supervisor CSRs and the few privileged
//! instructions the kernel needs. The analogue of the x86_64 `arch::cpu`.

use core::arch::asm;

/// `sstatus.SIE` (supervisor interrupt enable) bit.
const SSTATUS_SIE: usize = 1 << 1;
/// `sie.STIE` (supervisor timer interrupt enable) bit.
pub const SIE_STIE: usize = 1 << 5;

/// Read the `time` CSR (cycle-count-derived wall time, in timebase ticks).
pub fn read_time() -> u64 {
    let t: u64;
    // SAFETY: reading the `time` CSR is side-effect-free (zicntr present on virt).
    unsafe { asm!("csrr {}, time", out(reg) t, options(nomem, nostack)) };
    t
}

/// Install the trap vector base (`stvec`), direct mode (low bits 0). `addr` must
/// be 4-byte aligned.
///
/// # Safety
/// `addr` must point at a valid trap entry that preserves/restores state and
/// returns via `sret`.
pub unsafe fn write_stvec(addr: usize) {
    asm!("csrw stvec, {}", in(reg) addr, options(nomem, nostack));
}

/// Set bits in `sie` (e.g. [`SIE_STIE`]).
///
/// # Safety
/// Enables a class of interrupts; the corresponding handler must be installed.
pub unsafe fn sie_set(bits: usize) {
    asm!("csrs sie, {}", in(reg) bits, options(nomem, nostack));
}

/// Globally enable supervisor interrupts (`sstatus.SIE = 1`).
///
/// # Safety
/// The trap vector must already be installed.
pub unsafe fn enable_interrupts() {
    asm!("csrs sstatus, {}", in(reg) SSTATUS_SIE, options(nomem, nostack));
}

/// Globally disable supervisor interrupts (`sstatus.SIE = 0`).
///
/// # Safety
/// Caller is responsible for re-enabling when appropriate.
pub unsafe fn disable_interrupts() {
    asm!("csrc sstatus, {}", in(reg) SSTATUS_SIE, options(nomem, nostack));
}

/// Park the current hart low-power until the next interrupt, forever.
pub fn park() -> ! {
    loop {
        // SAFETY: `wfi` simply waits for an interrupt.
        unsafe { asm!("wfi", options(nomem, nostack)) };
    }
}
