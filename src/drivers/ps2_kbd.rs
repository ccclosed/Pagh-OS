// drivers/ps2_kbd.rs — PS/2 keyboard driver stub
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use alloc::sync::Arc;
use crate::sync::spinlock::Spinlock;
use crate::drivers::CharacterDevice;

const BUF_SIZE: usize = 128;

struct KeyboardInner {
    buf: [u8; BUF_SIZE],
    head: usize,
    tail: usize,
}

pub struct Ps2Keyboard {
    inner: Spinlock<KeyboardInner>,
}

impl Ps2Keyboard {
    pub fn new() -> Self {
        Ps2Keyboard {
            inner: Spinlock::new(KeyboardInner {
                buf: [0; BUF_SIZE],
                head: 0,
                tail: 0,
            }),
        }
    }

    /// STAGE-16.3: non-destructive check - is at least one scancode queued?
    pub fn has_scancode(&self) -> bool {
        let inner = self.inner.lock();
        inner.head != inner.tail
    }

    /// Push a scancode byte into the ring buffer (called from IRQ context).
    /// Allocation-free.
    pub fn push_scancode(&self, byte: u8) {
        let mut inner = self.inner.lock();
        let tail = inner.tail;
        let next = (tail + 1) % BUF_SIZE;
        if next != inner.head {
            inner.buf[tail] = byte;
            inner.tail = next;
        }
    }
}

impl CharacterDevice for Ps2Keyboard {
    fn name(&self) -> &str { "keyboard" }

    fn read_char(&self) -> Option<u8> {
        let mut inner = self.inner.lock();
        if inner.head != inner.tail {
            let byte = inner.buf[inner.head];
            inner.head = (inner.head + 1) % BUF_SIZE;
            Some(byte)
        } else {
            None
        }
    }
}

static KEYBOARD: Spinlock<Option<Arc<Ps2Keyboard>>> = Spinlock::new(None);

/// Initialize the PS/2 keyboard driver.
pub fn init() {
    let kbd = Arc::new(Ps2Keyboard::new());
    crate::drivers::register_char(kbd.clone());
    *KEYBOARD.lock() = Some(kbd);
    crate::debug!("[PS2KBD] Keyboard driver initialized (IRQ1 not wired)");
}

/// IRQ1 handler — reads scancode from port 0x60.
pub fn irq_handler() {
    // SAFETY: Reading from port 0x60 is standard PS/2 data port access.
    let scancode: u8 = unsafe {
        let mut port = x86_64::instructions::port::Port::new(0x60);
        port.read()
    };

    if let Some(ref kbd) = *KEYBOARD.lock() {
        kbd.push_scancode(scancode);
    }
}

/// STAGE-16.3: true when the ring buffer holds an unread scancode. Lets
/// poll/epoll report stdin readable only when a read can make progress.
pub fn has_pending() -> bool {
    KEYBOARD.lock().as_ref().map(|k| k.has_scancode()).unwrap_or(false)
}
