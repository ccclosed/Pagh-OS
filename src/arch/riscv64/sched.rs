//! Preemptive round-robin kernel-thread scheduler.
//!
//! Each thread owns a full saved trap frame (the [`crate::trap::FRAME_SLOTS`]
//! layout: GPRs + `sepc` + `sstatus`) and a stack. The timer interrupt calls
//! [`preempt`], which — if another runnable thread exists — saves the current
//! trap frame into the running thread's TCB and copies the next thread's frame
//! into the live frame; the trap exit then `sret`s into that thread. Threads
//! that call [`exit`] are marked finished and skipped, so once only one thread
//! remains runnable `preempt` becomes a no-op (and never touches a non-thread
//! frame such as a U-mode process or the post-demo boot flow).

use alloc::boxed::Box;
use alloc::vec::Vec;
use spin::Mutex;

use crate::trap::FRAME_SLOTS;

/// Frame slot indices.
const SP: usize = 2;
const RA: usize = 1;
const SEPC: usize = 32;
const SSTATUS: usize = 33;

/// `sstatus` bits for a kernel thread's initial frame: SPP=1 (sret → S-mode) and
/// SPIE=1 (interrupts enabled after sret).
const SSTATUS_SPP: usize = 1 << 8;
const SSTATUS_SPIE: usize = 1 << 5;

const STACK_SIZE: usize = 64 * 1024;

#[derive(PartialEq, Eq, Clone, Copy)]
enum State {
    Running,
    Finished,
}

struct Tcb {
    frame: [usize; FRAME_SLOTS],
    state: State,
    _stack: Vec<u8>,
}

struct Sched {
    threads: Vec<Box<Tcb>>,
    current: usize,
}

static SCHED: Mutex<Option<Sched>> = Mutex::new(None);

/// Run `f` with supervisor interrupts masked, then re-enable them. Thread-context
/// code that locks `SCHED` MUST use this: otherwise a timer interrupt taken while
/// the lock is held would deadlock `preempt` (which re-locks in the handler).
fn with_irqs_off<R>(f: impl FnOnce() -> R) -> R {
    // SAFETY: brief mask/unmask around a short critical section; all callers run
    // in thread context with interrupts on.
    unsafe { crate::cpu::disable_interrupts() };
    let r = f();
    unsafe { crate::cpu::enable_interrupts() };
    r
}

/// Initialize with thread 0 = the current (boot) context; its frame is captured
/// on the first preemption.
pub fn init() {
    with_irqs_off(|| {
        *SCHED.lock() = Some(Sched {
            threads: alloc::vec![Box::new(Tcb {
                frame: [0; FRAME_SLOTS],
                state: State::Running,
                _stack: Vec::new(),
            })],
            current: 0,
        });
    });
}

/// Spawn a kernel thread starting at `entry` (an `extern "C" fn() -> !`).
pub fn spawn(entry: extern "C" fn() -> !) {
    let mut stack = alloc::vec![0u8; STACK_SIZE];
    let top = (stack.as_mut_ptr() as usize + stack.len()) & !0xf;

    let mut frame = [0usize; FRAME_SLOTS];
    frame[SP] = top;
    frame[RA] = thread_trampoline as usize;
    frame[SEPC] = entry as usize;
    frame[SSTATUS] = SSTATUS_SPP | SSTATUS_SPIE;

    with_irqs_off(|| {
        if let Some(s) = SCHED.lock().as_mut() {
            s.threads.push(Box::new(Tcb {
                frame,
                state: State::Running,
                _stack: stack,
            }));
        }
    });
}

/// Fallback landing pad if a thread entry ever returns (our demo threads don't).
extern "C" fn thread_trampoline() -> ! {
    exit();
}

/// Mark the current thread finished and wait to be preempted away forever.
pub fn exit() -> ! {
    with_irqs_off(|| {
        if let Some(s) = SCHED.lock().as_mut() {
            let cur = s.current;
            s.threads[cur].state = State::Finished;
        }
    });
    loop {
        // SAFETY: wait for the next timer tick; preempt() will switch us away and
        // never schedule a Finished thread again.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}

/// Number of still-running threads (used by the demo to know when workers done).
pub fn running_count() -> usize {
    with_irqs_off(|| match SCHED.lock().as_ref() {
        Some(s) => s.threads.iter().filter(|t| t.state == State::Running).count(),
        None => 0,
    })
}

/// Timer-driven preemption. Called from the trap handler with `frame` pointing
/// at the live saved frame. Switches to the next runnable thread if one exists;
/// otherwise leaves `frame` untouched (so non-thread contexts are never swapped).
pub fn preempt(frame: *mut usize) {
    let (cur_ptr, next_ptr) = {
        let mut guard = SCHED.lock();
        let s = match guard.as_mut() {
            Some(s) => s,
            None => return,
        };
        let n = s.threads.len();
        if n < 2 {
            return;
        }
        let cur = s.current;
        // Find the next runnable thread after `cur` (excluding `cur`).
        let mut next = (cur + 1) % n;
        while next != cur && s.threads[next].state != State::Running {
            next = (next + 1) % n;
        }
        if next == cur {
            return; // no other runnable thread
        }
        s.current = next;
        let cur_ptr = s.threads[cur].frame.as_mut_ptr();
        let next_ptr = s.threads[next].frame.as_ptr();
        (cur_ptr, next_ptr)
    };

    // SAFETY: distinct heap-stable frames; the lock is released. Copy the live
    // frame out to the outgoing thread, and the incoming thread's frame in.
    unsafe {
        core::ptr::copy_nonoverlapping(frame as *const usize, cur_ptr, FRAME_SLOTS);
        core::ptr::copy_nonoverlapping(next_ptr, frame, FRAME_SLOTS);
    }
}
