//! Platform-Level Interrupt Controller (PLIC) for external device interrupts.
//!
//! QEMU `virt` fixed layout: PLIC @ 0x0c00_0000, the ns16550 UART0 is external
//! IRQ 10, and the boot hart's S-mode interrupt context is context 1. We give
//! the UART IRQ a non-zero priority, enable it for the S-mode context, drop the
//! context threshold to 0, then claim/complete in the external-interrupt trap.

const PLIC_BASE: usize = 0x0c00_0000;

/// ns16550 UART0 external interrupt line on the `virt` machine.
pub const UART_IRQ: u32 = 10;

/// Boot hart's S-mode interrupt context (M-mode = 0, S-mode = 1).
const S_CONTEXT: usize = 1;

fn priority_reg(irq: u32) -> *mut u32 {
    (PLIC_BASE + (irq as usize) * 4) as *mut u32
}
fn enable_reg(ctx: usize, irq: u32) -> *mut u32 {
    (PLIC_BASE + 0x2000 + ctx * 0x80 + (irq as usize / 32) * 4) as *mut u32
}
fn threshold_reg(ctx: usize) -> *mut u32 {
    (PLIC_BASE + 0x20_0000 + ctx * 0x1000) as *mut u32
}
fn claim_reg(ctx: usize) -> *mut u32 {
    (PLIC_BASE + 0x20_0000 + ctx * 0x1000 + 4) as *mut u32
}

/// Enable `UART_IRQ` for the S-mode context at priority 1, threshold 0.
pub fn init() {
    // SAFETY: identity-mapped MMIO writes to the PLIC register window.
    unsafe {
        priority_reg(UART_IRQ).write_volatile(1);
        let en = enable_reg(S_CONTEXT, UART_IRQ);
        en.write_volatile(en.read_volatile() | (1 << (UART_IRQ % 32)));
        threshold_reg(S_CONTEXT).write_volatile(0);
    }
}

/// Claim the highest-priority pending interrupt for the S-mode context (0 = none).
pub fn claim() -> u32 {
    // SAFETY: identity-mapped MMIO read.
    unsafe { claim_reg(S_CONTEXT).read_volatile() }
}

/// Signal completion of `irq` for the S-mode context.
pub fn complete(irq: u32) {
    // SAFETY: identity-mapped MMIO write.
    unsafe { claim_reg(S_CONTEXT).write_volatile(irq) }
}
