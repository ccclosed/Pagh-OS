// drivers/serial.rs — COM1 UART driver (minimal, no FIFO)
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use x86_64::instructions::port::Port;

const COM1: u16 = 0x3F8;

/// Tracks whether [`init`] has completed the COM1 bring-up sequence.
///
/// INVARIANT: the UART at the fixed `COM1` base is fully described by that
/// constant address, so no port *value* needs to be stored across calls. We
/// only need to remember whether the device has been programmed yet. A
/// `Port<u8>` is a zero-cost wrapper over a port number, so it is reconstructed
/// (`Port::new(COM1)`) on demand at each access instead of being cached in a
/// `static mut`. This flag is written exactly once in `init` (before
/// interrupts/other CPUs touch the port) and only read afterwards, so a
/// `Relaxed` ordering is sufficient.
static SERIAL_INITIALIZED: AtomicBool = AtomicBool::new(false);

pub fn init() {
    // The UART is addressed entirely by the fixed `COM1` base, so the
    // programming sequence below reconstructs the relevant ports via
    // `port_write` rather than holding a cached `Port` handle.
    // Disable interrupts
    port_write(COM1 + 1, 0x00);
    // Enable DLAB (set baud rate divisor)
    port_write(COM1 + 3, 0x80);
    // Set divisor to 1 (lo byte) 115200 baud
    port_write(COM1 + 0, 0x01);
    // (hi byte)
    port_write(COM1 + 1, 0x00);
    // 8 bits, no parity, one stop bit
    port_write(COM1 + 3, 0x03);
    // Enable FIFO, clear them, with 14-byte threshold
    port_write(COM1 + 2, 0xC7);
    // IRQs enabled, RTS/DSR set
    port_write(COM1 + 4, 0x0B);

    // Publish readiness only after the full bring-up sequence has run, so
    // `write_byte` observes the device as usable exactly when it was before.
    SERIAL_INITIALIZED.store(true, Ordering::Relaxed);
}

fn port_write(addr: u16, value: u8) {
    let mut port: Port<u8> = Port::new(addr);
    // SAFETY: `addr` is a COM1 UART register (`COM1 + offset`); writing a u8 to
    // a 16550-class UART register has no memory-safety effect and is the
    // intended hardware access.
    unsafe { port.write(value); }
}

fn write_byte(b: u8) {
    // Mirror the previous `Option`-guarded behavior: do nothing until `init`
    // has brought COM1 up.
    if !SERIAL_INITIALIZED.load(Ordering::Relaxed) {
        return;
    }
    // `Port<u8>` is a zero-cost wrapper over the port number; reconstructing it
    // from the fixed `COM1` base is equivalent to the previously-cached handle.
    let mut data: Port<u8> = Port::new(COM1);
    // SAFETY: COM1 is a fixed, valid UART base. We spin until the line-status
    // register reports the transmit-holding register is empty (bit 0x20), then
    // write one byte to the data register, which is the correct 16550 TX
    // protocol and carries no memory-safety hazard.
    unsafe {
        while (port_read(COM1 + 5) & 0x20) == 0 {
            core::hint::spin_loop();
        }
        data.write(b);
    }
}

fn port_read(addr: u16) -> u8 {
    let mut port: Port<u8> = Port::new(addr);
    // SAFETY: `addr` is a COM1 UART register (`COM1 + offset`); reading a u8
    // from a 16550-class UART register has no memory-safety effect.
    unsafe { port.read() }
}

/// Transmit each byte of `bytes` over COM1 verbatim, in order.
///
/// This is the byte-level counterpart to the `char`/`str`-oriented `_kprint`
/// path: it forwards every element of the slice to the private `write_byte`
/// unchanged, performing no character decoding or UTF-8 re-encoding. Bytes
/// outside the ASCII range (>= 0x80) are emitted as exactly one byte each,
/// preserving binary fidelity for consumers such as `/dev/serial`.
pub fn write_bytes(bytes: &[u8]) {
    for &b in bytes {
        write_byte(b);
    }
}

pub fn _kprint(args: fmt::Arguments) {
    use core::fmt::Write;
    struct SerialWriter;
    impl fmt::Write for SerialWriter {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            for b in s.bytes() {
                write_byte(b);
            }
            Ok(())
        }
    }
    SerialWriter.write_fmt(args).ok();
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => { $crate::drivers::serial::_kprint(format_args!($($arg)*)); };
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {{
        $crate::drivers::serial::_kprint(format_args!($($arg)*));
        $crate::drivers::serial::_kprint(format_args!("\n"));
    }};
}

/// Zero-sized serial console handle implementing the [`Console`](crate::drivers::Console)
/// trait. The serial UART is inherently a stream device backed by a module-level
/// port, so this carries no state and is trivially `Send + Sync`.
pub struct SerialConsole;

impl crate::drivers::Console for SerialConsole {
    fn write_str(&self, s: &str) {
        // `write_byte` is private to this module but reachable here since the
        // impl lives in the same module. It blocks on the UART THR-empty bit.
        for b in s.bytes() {
            write_byte(b);
        }
    }
}

/// The process-wide serial console handle.
pub static SERIAL_CONSOLE: SerialConsole = SerialConsole;

/// Accessor returning the static serial [`Console`](crate::drivers::Console)
/// handle, for use by the logging facade and other consumers.
pub fn console() -> &'static SerialConsole {
    &SERIAL_CONSOLE
}
