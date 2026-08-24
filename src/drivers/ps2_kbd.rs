// drivers/ps2_kbd.rs — PS/2 keyboard driver stub
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use crate::drivers::CharacterDevice;
use crate::sync::spinlock::Spinlock;
use alloc::sync::Arc;

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

    /// Non-destructive check - is at least one scancode queued?
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
    fn name(&self) -> &str {
        "keyboard"
    }

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
/// Set when the driver-level tracker sees Ctrl+C make, regardless of which
/// consumer later reads the scancode (shell editor OR a compat process's
/// stdin). The `lxrun` foreground wait polls this to terminate the child.
pub static CTRL_C: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// PID of the current foreground Compat_Process (set by `cmd_lxrun`).
/// When the driver detects ^C, it directly terminates this pid — the shell
/// may itself be blocked in read() and unable to poll a latch.
pub static FG_PID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// Driver-level modifier state (set-1): LCtrl 0x1D/0x9D, RCtrl E0 1D/E0 9D,
// C = 0x2E. Tracked here so ^C detection cannot be starved by a program
// that never decodes modifiers itself.
static DRV_CTRL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static DRV_EXTENDED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn irq_handler() {
    // SAFETY: Reading from port 0x60 is standard PS/2 data port access.
    let scancode: u8 = unsafe {
        let mut port = x86_64::instructions::port::Port::new(0x60);
        port.read()
    };

    // ── ^C tracking (make/break, E0-aware) ──
    use core::sync::atomic::Ordering;
    if scancode == 0xE0 {
        DRV_EXTENDED.store(true, Ordering::Relaxed);
    } else {
        let extended = DRV_EXTENDED.swap(false, Ordering::Relaxed);
        let make = scancode & 0x80 == 0;
        match (extended, scancode) {
            (false, 0x1D) => DRV_CTRL.store(make, Ordering::Relaxed),
            (true, 0x1D) => DRV_CTRL.store(make, Ordering::Relaxed),
            (false, 0x2E) if make && DRV_CTRL.load(Ordering::Relaxed) => {
                CTRL_C.store(true, Ordering::Relaxed);
                let fg = FG_PID.load(Ordering::Relaxed);
                if fg != 0
                    && crate::task::compat::compat_exists(fg)
                    && !crate::task::compat::compat_is_raw(fg)
                {
                    crate::task::scheduler::request_exit(fg);
                }
            }
            _ => {}
        }
    }

    if let Some(ref kbd) = *KEYBOARD.lock() {
        kbd.push_scancode(scancode);
    }
}

/// True when the ring buffer holds an unread scancode. Lets
/// poll/epoll report stdin readable only when a read can make progress.
pub fn has_pending() -> bool {
    KEYBOARD
        .lock()
        .as_ref()
        .map(|k| k.has_scancode())
        .unwrap_or(false)
}
