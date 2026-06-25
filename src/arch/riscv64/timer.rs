//! Supervisor timer: a periodic ~100 Hz tick via the SBI legacy `set_timer`
//! call. Each timer interrupt re-arms the next deadline and bumps the global
//! tick counter (the scheduler's future preemption source).

use core::sync::atomic::{AtomicU64, Ordering};

/// Timer base frequency on the QEMU `virt` machine (aclint-mtimer @ 10 MHz, as
/// reported by OpenSBI). A later refinement reads `/cpus/timebase-frequency`
/// from the DTB instead of hard-coding it.
const TIMEBASE_HZ: u64 = 10_000_000;
/// Preemption frequency.
const HZ: u64 = 100;
/// Timer ticks between interrupts.
const INTERVAL: u64 = TIMEBASE_HZ / HZ;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// SBI legacy `set_timer` (EID 0x00): schedule the next supervisor timer
/// interrupt at absolute time `t` (and clear any pending one).
fn sbi_set_timer(t: u64) {
    // SAFETY: legacy SBI set_timer ecall; clobbers only a0/a1.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") 0usize,
            in("a0") t as usize,
            lateout("a0") _,
            lateout("a1") _,
            options(nostack),
        );
    }
}

/// Arm the first tick and enable supervisor timer interrupts in `sie`.
pub fn init() {
    sbi_set_timer(crate::cpu::read_time() + INTERVAL);
    // SAFETY: the trap vector is installed before this is called.
    unsafe { crate::cpu::sie_set(crate::cpu::SIE_STIE) };
}

/// Called from the trap handler on each supervisor timer interrupt: count it
/// and re-arm the next deadline.
pub fn on_tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    sbi_set_timer(crate::cpu::read_time() + INTERVAL);
}

/// Ticks elapsed since boot (~100 Hz).
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}
