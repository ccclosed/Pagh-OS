// drivers/ps2_mouse.rs — PS/2 mouse driver (IRQ12)
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// Standard PS/2 "aux device" bring-up via the 8042 controller, plus a 3-byte
// packet assembler driven from the IRQ12 handler. The driver maintains an
// absolute cursor position (accumulated from relative movement packets and
// clamped to the screen) and the current button state, both readable through
// [`poll`]. It performs no rendering — the software cursor (`drivers::cursor`)
// and `paint` consume the state.

use x86_64::instructions::port::Port;
use crate::sync::spinlock::Spinlock;

const DATA_PORT: u16 = 0x60;
const CMD_PORT: u16 = 0x64;

/// A snapshot of the mouse state at a moment in time.
#[derive(Debug, Clone, Copy, Default)]
pub struct MouseState {
    pub x: usize,
    pub y: usize,
    pub left: bool,
    pub right: bool,
    pub middle: bool,
    /// Monotonically increasing counter bumped on every processed packet, so
    /// consumers can cheaply detect "did anything change since last poll".
    pub seq: u64,
}

struct MouseInner {
    x: i32,
    y: i32,
    max_x: i32,
    max_y: i32,
    left: bool,
    right: bool,
    middle: bool,
    seq: u64,
    // Packet assembly.
    packet: [u8; 3],
    index: usize,
}

static MOUSE: Spinlock<MouseInner> = Spinlock::new(MouseInner {
    x: 0,
    y: 0,
    max_x: 0,
    max_y: 0,
    left: false,
    right: false,
    middle: false,
    seq: 0,
    packet: [0; 3],
    index: 0,
});

static INITIALIZED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Returns `true` once [`init`] has successfully brought the mouse up.
pub fn is_present() -> bool {
    INITIALIZED.load(core::sync::atomic::Ordering::Relaxed)
}

/// Current mouse state snapshot.
pub fn poll() -> MouseState {
    let m = MOUSE.lock();
    MouseState {
        x: m.x.max(0) as usize,
        y: m.y.max(0) as usize,
        left: m.left,
        right: m.right,
        middle: m.middle,
        seq: m.seq,
    }
}

// ─── 8042 controller wait helpers ───────────────────────────────────────────

/// Wait until the controller input buffer is empty (safe to write a command).
fn wait_write() {
    let mut status: Port<u8> = Port::new(CMD_PORT);
    for _ in 0..100_000 {
        // SAFETY: reading the 8042 status port is a side-effect-free port read.
        if unsafe { status.read() } & 0x02 == 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

/// Wait until the controller output buffer is full (data ready to read).
fn wait_read() {
    let mut status: Port<u8> = Port::new(CMD_PORT);
    for _ in 0..100_000 {
        // SAFETY: side-effect-free port read.
        if unsafe { status.read() } & 0x01 != 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

/// Send a command byte to the controller (port 0x64).
fn write_cmd(cmd: u8) {
    wait_write();
    // SAFETY: writing the 8042 command port with a known command byte.
    unsafe { Port::<u8>::new(CMD_PORT).write(cmd); }
}

/// Write a data byte to port 0x60 (controller config / mouse stream).
fn write_data(data: u8) {
    wait_write();
    // SAFETY: writing the 8042 data port.
    unsafe { Port::<u8>::new(DATA_PORT).write(data); }
}

/// Read a data byte from port 0x60.
fn read_data() -> u8 {
    wait_read();
    // SAFETY: reading the 8042 data port.
    unsafe { Port::<u8>::new(DATA_PORT).read() }
}

/// Send a command to the mouse device (prefixed with 0xD4) and read its ack.
fn mouse_write(cmd: u8) -> u8 {
    write_cmd(0xD4);
    write_data(cmd);
    read_data() // 0xFA = ACK
}

/// Initialize the PS/2 mouse and set the screen bounds for cursor clamping.
///
/// Returns `true` if the device acknowledged the enable sequence. On failure
/// the driver stays disabled and [`is_present`] returns `false`; callers should
/// degrade gracefully (no cursor).
pub fn init(screen_w: usize, screen_h: usize) -> bool {
    {
        let mut m = MOUSE.lock();
        m.max_x = screen_w.saturating_sub(1) as i32;
        m.max_y = screen_h.saturating_sub(1) as i32;
        m.x = (screen_w / 2) as i32;
        m.y = (screen_h / 2) as i32;
        m.index = 0;
    }

    // Enable the auxiliary (mouse) PS/2 device.
    write_cmd(0xA8);

    // Read the controller config byte, enable IRQ12 (bit 1) and the mouse
    // clock (clear bit 5), then write it back.
    write_cmd(0x20);
    let mut config = read_data();
    config |= 0x02; // enable IRQ12
    config &= !0x20; // enable mouse clock
    write_cmd(0x60);
    write_data(config);

    // Set defaults, then enable data reporting. Expect 0xFA acks.
    let ack1 = mouse_write(0xF6);
    let ack2 = mouse_write(0xF4);

    let ok = ack1 == 0xFA && ack2 == 0xFA;
    INITIALIZED.store(ok, core::sync::atomic::Ordering::Relaxed);
    if ok {
        crate::info!("[PS2MOUSE] mouse enabled ({}x{})", screen_w, screen_h);
    } else {
        crate::warn!("[PS2MOUSE] mouse not detected (acks {:#x}/{:#x})", ack1, ack2);
    }
    ok
}

/// IRQ12 handler — read one byte from the mouse stream and assemble packets.
pub fn irq_handler() {
    // SAFETY: standard PS/2 data-port read in IRQ context.
    let byte: u8 = unsafe { Port::<u8>::new(DATA_PORT).read() };

    let mut m = MOUSE.lock();

    // Resync: the first byte of a packet always has bit 3 set. If we are at
    // index 0 and that bit is clear, the byte is spurious — drop it.
    if m.index == 0 && (byte & 0x08) == 0 {
        return;
    }

    let idx = m.index;
    m.packet[idx] = byte;
    m.index += 1;

    if m.index < 3 {
        return;
    }
    m.index = 0;

    let flags = m.packet[0];
    // Discard packets reporting overflow — their deltas are meaningless.
    if flags & 0xC0 != 0 {
        return;
    }

    // Sign-extend the 9-bit relative movement deltas.
    let mut dx = m.packet[1] as i32;
    let mut dy = m.packet[2] as i32;
    if flags & 0x10 != 0 {
        dx -= 256;
    }
    if flags & 0x20 != 0 {
        dy -= 256;
    }

    // Screen Y grows downward; mouse Y grows upward, so subtract dy.
    let max_x = m.max_x;
    let max_y = m.max_y;
    m.x = (m.x + dx).clamp(0, max_x);
    m.y = (m.y - dy).clamp(0, max_y);

    m.left = flags & 0x01 != 0;
    m.right = flags & 0x02 != 0;
    m.middle = flags & 0x04 != 0;
    m.seq = m.seq.wrapping_add(1);
}
