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

/// Whether an 8042 controller answers at all.
///
/// The status port reads back as `0xFF` on machines without one (the bus floats
/// high), which is exactly the machine this kernel's USB path exists for. The
/// boot menu prints this so a screen-only field report can tell "no keyboard
/// because no PS/2" from "PS/2 present but silent".
pub fn controller_present() -> bool {
    use x86_64::instructions::port::Port;
    // SAFETY: port 0x64 is the 8042 status register; a read has no side effects
    // beyond consuming a pending output byte's status.
    unsafe {
        let mut status: Port<u8> = Port::new(0x64);
        status.read() != 0xFF
    }
}

/// Take one scancode out of the ring, if any is queued.
///
/// The shell reads the ring through its own decoder; this is the same operation
/// for any other kernel consumer (the in-QEMU selftest drives the HID translator
/// and reads back what it produced, which is the only way to test that path
/// without a human at the keyboard).
pub fn take_scancode() -> Option<u8> {
    KEYBOARD.lock().as_ref().and_then(|kbd| kbd.read_char())
}

/// Inject a scancode from another keyboard source (USB HID), exactly as if the
/// 8042 had delivered it.
///
/// The whole point of feeding THIS ring rather than a second queue: every
/// consumer — the shell's decoder, the `^C`/`^Z` latches, a compat process's
/// stdin — already reads here, so a USB keyboard needs no changes downstream.
/// The modifier tracking the driver does for `^C` is deliberately NOT duplicated
/// here: it runs in `irq_handler`, and duplicating it would double-count a
/// Ctrl press that arrived on both paths (firmware PS/2 translation plus a real
/// USB device) and latch `^C` without a keypress.
pub fn inject_scancode(byte: u8) {
    if let Some(kbd) = KEYBOARD.lock().as_ref() {
        kbd.push_scancode(byte);
    }
}

/// Whether at least one scancode is currently queued (non-destructive).
///
/// The shell's REPL wakes on every timer tick; this is how it tells a real
/// keypress from an idle wake-up, so it can skip the overlay/repaint cycle (and
/// the frame flush it ends with) when there is nothing to do.
pub fn has_scancode() -> bool {
    KEYBOARD
        .lock()
        .as_ref()
        .map(|kbd| kbd.has_scancode())
        .unwrap_or(false)
}

/// IRQ1 handler — reads scancode from port 0x60.
/// Set when the driver-level tracker sees Ctrl+C make, regardless of which
/// consumer later reads the scancode (shell editor OR a compat process's
/// stdin). The `lxrun` foreground wait polls this to terminate the child.
pub static CTRL_C: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Set when the driver sees Ctrl+Z make; the `lxrun` foreground wait turns it
/// into SIGTSTP for the foreground process group.
pub static CTRL_Z: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

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
            (false, 0x2C) if make && DRV_CTRL.load(Ordering::Relaxed) => {
                CTRL_Z.store(true, Ordering::Relaxed);
            }
            (false, 0x2E) if make && DRV_CTRL.load(Ordering::Relaxed) => {
                CTRL_C.store(true, Ordering::Relaxed);
                let fg = FG_PID.load(Ordering::Relaxed);
                if fg != 0
                    && crate::task::compat::compat_exists(fg)
                    && !crate::task::compat::compat_is_raw(fg)
                {
                    // Phase-2 signals: ^C delivers a real SIGINT to the
                    // terminal's FOREGROUND PROCESS GROUP — a job that forked
                    // has several thread groups and every one of them must see
                    // the interrupt, not only the pid the shell records. A
                    // program with a handler gets it (the blocking-wait EINTR
                    // checks wake it); without one the default-terminate action
                    // kills it at the next syscall return — the classic
                    // driver-level bypass remains only as the fallback for a
                    // foreground group that has no live member (e.g. ^C racing
                    // the handoff in `cmd_lxrun`).
                    let pgid = crate::drivers::tty::foreground_pgid();
                    if crate::arch::x86_64::linux::signal::send_signal_to_pgid_all(
                        pgid,
                        crate::arch::x86_64::linux::signal_frame::SIGINT,
                    )
                    .is_err()
                    {
                        crate::task::scheduler::request_exit(fg);
                    }
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
