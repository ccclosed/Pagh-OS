// drivers/ps2_mouse.rs — PS/2 mouse driver (IRQ12)
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// Standard PS/2 "aux device" bring-up via the 8042 controller, plus a 3-byte
// packet assembler driven from the IRQ12 handler. The driver maintains an
// absolute cursor position (accumulated from relative movement packets and
// clamped to the screen) and the current button state, both readable through
// [`poll`]. It performs no rendering — the software cursor (`drivers::cursor`)
// and `paint` consume the state.

use crate::sync::spinlock::Spinlock;
use x86_64::instructions::port::Port;

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
    /// Accumulated vertical wheel delta in detents: positive is away from the
    /// user (scroll up / older text). The counter is cumulative and wrapping, so
    /// a consumer that polls at its own pace subtracts the previous value instead
    /// of racing a consume-on-read flag. A plain 3-byte mouse never changes it.
    pub wheel: i32,
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
    wheel: i32,
    // Packet assembly. IntelliMouse wheel mode reports four bytes per packet
    // (the fourth is the wheel axis); a plain mouse reports three.
    packet: [u8; 4],
    packet_len: usize,
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
    wheel: 0,
    packet: [0; 4],
    packet_len: 3,
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
        wheel: m.wheel,
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
    unsafe {
        Port::<u8>::new(CMD_PORT).write(cmd);
    }
}

/// Write a data byte to port 0x60 (controller config / mouse stream).
fn write_data(data: u8) {
    wait_write();
    // SAFETY: writing the 8042 data port.
    unsafe {
        Port::<u8>::new(DATA_PORT).write(data);
    }
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

    // Set defaults, then negotiate the IntelliMouse wheel extension: the magic
    // sample-rate sequence 200/100/80 followed by "get device id". A device that
    // answers id 3 reports four-byte packets, the fourth byte being the wheel
    // axis; anything else keeps the 3-byte packet the driver always had.
    //
    // This is the standard PS/2 tap into the wheel (the same one Linux's psmouse
    // uses). Without it QEMU's mouse emits only 3-byte packets and wheel events
    // are dropped by the emulation, so a wheel that "works sometimes" would be
    // this negotiation failing, not the scroll code.
    let ack1 = mouse_write(0xF6);
    let mut wheel = false;
    if ack1 == 0xFA {
        let magic_ok = [200u8, 100, 80]
            .iter()
            .all(|rate| mouse_write(0xF3) == 0xFA && mouse_write(*rate) == 0xFA);
        if magic_ok {
            let ack_id = mouse_write(0xF2);
            let id = read_data();
            wheel = ack_id == 0xFA && id == 0x03;
        }
    }
    // Enable data reporting only after the id probe: the probe itself does not
    // produce movement packets, and enabling first lets a wheel packet race the
    // id response.
    let ack2 = mouse_write(0xF4);

    let ok = ack1 == 0xFA && ack2 == 0xFA;
    INITIALIZED.store(ok, core::sync::atomic::Ordering::Relaxed);
    {
        let mut m = MOUSE.lock();
        m.packet_len = if wheel { 4 } else { 3 };
        m.index = 0;
    }
    if ok {
        crate::info!("[PS2MOUSE] mouse enabled ({}x{})", screen_w, screen_h);
        // Serial-only: a boot-screen line for a diagnostics feature would push
        // the console one row for every user, so the wheel report goes to the
        // log stream the E2E harness reads.
        crate::kprintln!(
            "[PS2MOUSE] wheel: {}",
            if wheel {
                "IntelliMouse 4-byte packets (id 3)"
            } else {
                "not reported (3-byte packets)"
            }
        );
    } else {
        crate::warn!(
            "[PS2MOUSE] mouse not detected (acks {:#x}/{:#x})",
            ack1,
            ack2
        );
    }
    ok
}

/// IRQ12 handler — read one byte from the mouse stream and assemble packets.
pub fn irq_handler() {
    // SAFETY: standard PS/2 data-port read in IRQ context.
    let byte: u8 = unsafe { Port::<u8>::new(DATA_PORT).read() };
    decode_byte(byte);
}

/// Inject a complete PS/2-format packet from another pointing device (USB HID).
///
/// The USB boot mouse report is `[buttons, dx, dy]` — the same information the
/// 8042 aux stream carries — so it is converted by the caller into this packet
/// shape and fed through the same decoder, which keeps cursor clamping, button
/// state and `seq` semantics identical for both sources.
pub fn inject_packet(packet: [u8; 3]) {
    let mut m = MOUSE.lock();
    m.packet = [packet[0], packet[1], packet[2], 0];
    m.index = 0;
    decode_packet(&mut m);
}

fn decode_packet(m: &mut MouseInner) {
    // Unchanged from the PS/2 path: this is the body of the old IRQ handler.
    let flags = m.packet[0];
    if flags & 0xC0 != 0 {
        return;
    }
    let mut dx = m.packet[1] as i32;
    let mut dy = m.packet[2] as i32;
    if flags & 0x10 != 0 {
        dx -= 256;
    }
    if flags & 0x20 != 0 {
        dy -= 256;
    }
    let max_x = m.max_x;
    let max_y = m.max_y;
    m.x = (m.x + dx).clamp(0, max_x);
    m.y = (m.y - dy).clamp(0, max_y);
    m.left = flags & 0x01 != 0;
    m.right = flags & 0x02 != 0;
    m.middle = flags & 0x04 != 0;
    // IntelliMouse wheel: the fourth byte carries the vertical delta as a signed
    // 4-bit value in its low nibble (the high nibble is button/sign state that
    // this driver does not use). Sign-extend before accumulating.
    if m.packet_len >= 4 {
        let nibble = m.packet[3] & 0x0F;
        let dz = if nibble & 0x08 != 0 {
            nibble as i8 - 16
        } else {
            nibble as i8
        };
        m.wheel = m.wheel.wrapping_add(dz as i32);
    }
    m.seq = m.seq.wrapping_add(1);
}

/// Feed one byte of the PS/2 aux stream through the assembler.
fn decode_byte(byte: u8) {
    let mut m = MOUSE.lock();

    // Resync: the first byte of a packet always has bit 3 set. If we are at
    // index 0 and that bit is clear, the byte is spurious — drop it.
    if m.index == 0 && (byte & 0x08) == 0 {
        return;
    }

    let idx = m.index;
    m.packet[idx] = byte;
    m.index += 1;

    if m.index < m.packet_len {
        return;
    }
    m.index = 0;
    decode_packet(&mut m);
}
