//! SBI (Supervisor Binary Interface) glue: the firmware console and formatted
//! kernel printing. While in S-mode the kernel asks OpenSBI (M-mode) to emit
//! console bytes via `ecall`; the legacy `console_putchar` extension (EID 0x01)
//! is the lowest-common-denominator and works on every OpenSBI build.

use core::fmt::{self, Write};

/// SBI legacy `console_putchar` (EID 0x01): write one byte to the firmware
/// console. `a7` carries the extension id, `a0` the character.
pub fn putchar(c: u8) {
    // SAFETY: legacy SBI console-putchar ecall; clobbers only a0/a1, no memory.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") 1usize,
            in("a0") c as usize,
            lateout("a0") _,
            lateout("a1") _,
            options(nostack),
        );
    }
}

/// Write a string to the SBI console, translating `\n` to CRLF so host serial
/// terminals render line breaks correctly.
pub fn print(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            putchar(b'\r');
        }
        putchar(b);
    }
}

/// A [`core::fmt::Write`] sink over the SBI console, so `write!`/`writeln!` and
/// the [`kprint!`]/[`kprintln!`] macros format without needing the heap.
pub struct Console;

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // Once the ns16550 MMIO driver is up, write straight to the device;
        // before that (early boot) use the SBI console.
        if crate::uart::ready() {
            crate::uart::print(s);
        } else {
            print(s);
        }
        Ok(())
    }
}

/// Backing function for the print macros.
pub fn _print(args: fmt::Arguments) {
    let _ = Console.write_fmt(args);
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => ($crate::sbi::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! kprintln {
    () => ($crate::kprint!("\n"));
    ($($arg:tt)*) => ($crate::kprint!("{}\n", format_args!($($arg)*)));
}
