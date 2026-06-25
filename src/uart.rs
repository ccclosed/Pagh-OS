//! Direct ns16550 UART driver (the QEMU `virt` serial @ 0x1000_0000, discovered
//! from the DTB). Once initialized the console writes bytes straight to the
//! device's MMIO registers instead of going through the SBI console — the first
//! real device driver, and the basis for interrupt-driven input (PLIC) later.
//!
//! QEMU's `virt` ns16550 uses byte-wide registers at `reg-shift = 0`:
//!   offset 0 = RBR (read) / THR (write), offset 5 = LSR.
//! LSR bit 5 (THRE) = transmit holding register empty; bit 0 (DR) = data ready.

use core::sync::atomic::{AtomicUsize, Ordering};

use spin::Mutex;

/// LSR offset and the bits we use.
const LSR: usize = 5;
const LSR_THRE: u8 = 1 << 5;
const LSR_DR: u8 = 1 << 0;
/// IER offset (base + 1); bit 0 = "received data available" interrupt enable.
const IER: usize = 1;
const IER_RDA: u8 = 1 << 0;

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

/// Non-blocking read of one received byte directly from the device, if any
/// (used by the IRQ drain; the shell reads from the ring via [`getb`]).
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

/// Enable the UART "received data available" interrupt (IER bit 0). The PLIC
/// must already route the UART IRQ to the S-mode context.
pub fn enable_rx_interrupt() {
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    // SAFETY: identity-mapped MMIO write.
    unsafe { core::ptr::write_volatile((base + IER) as *mut u8, IER_RDA) };
}

/// A small single-producer (IRQ) / single-consumer (shell) byte ring.
struct Ring {
    buf: [u8; 256],
    head: usize,
    tail: usize,
}

impl Ring {
    const fn new() -> Self {
        Ring {
            buf: [0; 256],
            head: 0,
            tail: 0,
        }
    }
    fn push(&mut self, b: u8) {
        let next = (self.head + 1) % self.buf.len();
        if next != self.tail {
            self.buf[self.head] = b;
            self.head = next;
        }
        // else: full -> drop (back-pressure)
    }
    fn pop(&mut self) -> Option<u8> {
        if self.head == self.tail {
            None
        } else {
            let b = self.buf[self.tail];
            self.tail = (self.tail + 1) % self.buf.len();
            Some(b)
        }
    }
}

static RX_RING: Mutex<Ring> = Mutex::new(Ring::new());

/// Drain the device RX FIFO into the ring. Called from the external-interrupt
/// handler (interrupts already masked), so locking the ring cannot deadlock
/// against the consumer (which masks interrupts while it holds the lock).
pub fn drain_rx() {
    while let Some(b) = try_getb() {
        RX_RING.lock().push(b);
    }
}

/// Pop one byte received via the RX interrupt, if any. The consumer briefly
/// masks interrupts around the lock so the IRQ producer never contends with a
/// held lock (single hart).
pub fn getb() -> Option<u8> {
    // SAFETY: brief interrupt mask around a short critical section; re-enabled
    // immediately after so the caller can `wfi` for the next byte.
    unsafe { crate::cpu::disable_interrupts() };
    let b = RX_RING.lock().pop();
    // SAFETY: restore interrupts (the trap vector + SEIE remain configured).
    unsafe { crate::cpu::enable_interrupts() };
    b
}
