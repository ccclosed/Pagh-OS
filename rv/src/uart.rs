//! Direct ns16550 UART driver (the QEMU `virt` serial @ 0x1000_0000, discovered
//! from the DTB). Once initialized the console writes bytes straight to the
//! device's MMIO registers instead of going through the SBI console — the first
//! real device driver, and the basis for interrupt-driven input (PLIC) later.
//!
//! QEMU's `virt` ns16550 uses byte-wide registers at `reg-shift = 0`:
//!   offset 0 = RBR (read) / THR (write), offset 5 = LSR.
//! LSR bit 5 (THRE) = transmit holding register empty; bit 0 (DR) = data ready.

use core::sync::atomic::{AtomicUsize, Ordering};

/// LSR offset and the bits we use.
const LSR: usize = 5;
const LSR_THRE: u8 = 1 << 5;
const LSR_DR: u8 = 1 << 0;

/// Device MMIO base (0 = not yet initialized → callers fall back to SBI).
static BASE: AtomicUsize = AtomicUsize::new(0);

/// Record the device base (from the DTB). The MMIO window is covered by the
/// identity Sv39 map, so it is directly accessible in S-mode.
pub fn init(base: usize) {
    BASE.store(base, Ordering::Relaxed);
}

/// Whether the MMIO UART is initialized (so the console should use it).
pub fn ready() -> bool {
    BASE.load(Ordering::Relaxed) != 0
}

/// Write one byte, busy-waiting for the transmit holding register to drain.
pub fn putb(b: u8) {
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    // SAFETY: `base` is the DTB-reported MMIO window, identity-mapped.
    unsafe {
        let lsr = (base + LSR) as *const u8;
        while core::ptr::read_volatile(lsr) & LSR_THRE == 0 {}
        core::ptr::write_volatile(base as *mut u8, b);
    }
}

/// Write a string, translating `\n` to CRLF.
pub fn print(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            putb(b'\r');
        }
        putb(b);
    }
}

/// Non-blocking read of one received byte, if any (polled; the IRQ path arrives
/// with the PLIC in a later step).
pub fn try_getb() -> Option<u8> {
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 {
        return None;
    }
    // SAFETY: identity-mapped MMIO window.
    unsafe {
        let lsr = (base + LSR) as *const u8;
        if core::ptr::read_volatile(lsr) & LSR_DR != 0 {
            Some(core::ptr::read_volatile(base as *const u8))
        } else {
            None
        }
    }
}
