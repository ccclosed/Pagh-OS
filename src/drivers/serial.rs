// drivers/serial.rs — COM1 UART driver (minimal, no FIFO)
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use core::fmt;
use x86_64::instructions::port::Port;

const COM1: u16 = 0x3F8;

static mut SERIAL: Option<Port<u8>> = None;

pub fn init() {
    let mut port: Port<u8> = Port::new(COM1);
    
    unsafe {
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
        
        SERIAL = Some(port);
    }
}

fn port_write(addr: u16, value: u8) {
    let mut port: Port<u8> = Port::new(addr);
    unsafe { port.write(value); }
}

fn write_byte(b: u8) {
    if let Some(ref mut port) = unsafe { SERIAL.as_mut() } {
        unsafe {
            while (port_read(COM1 + 5) & 0x20) == 0 {
                core::hint::spin_loop();
            }
            port.write(b);
        }
    }
}

fn port_read(addr: u16) -> u8 {
    let mut port: Port<u8> = Port::new(addr);
    unsafe { port.read() }
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

    fn clear(&self) {
        // No-op: serial is a byte stream with no addressable screen to clear.
    }
}

/// The process-wide serial console handle.
pub static SERIAL_CONSOLE: SerialConsole = SerialConsole;

/// Accessor returning the static serial [`Console`](crate::drivers::Console)
/// handle, for use by the logging facade and other consumers.
pub fn console() -> &'static SerialConsole {
    &SERIAL_CONSOLE
}
