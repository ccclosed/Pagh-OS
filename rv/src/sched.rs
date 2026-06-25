//! Minimal kernel-thread scheduler with RISC-V context switching.
//!
//! This milestone implements **cooperative** round-robin scheduling over kernel
//! threads: a thread runs until it calls [`yield_now`], which saves its
//! callee-saved registers + `sp`/`ra` and restores the next thread's. Preemptive
//! switching driven from the timer trap, and U-mode processes, build on this
//! `Context`/`__switch` primitive in later milestones.

use alloc::boxed::Box;
use alloc::vec::Vec;
use spin::Mutex;

/// Saved callee-saved state for a context switch: `ra`, `sp`, and `s0..s11`.
/// `#[repr(C)]` with this exact field order matches the offsets in `__switch`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Context {
    ra: usize,
    sp: usize,
    s: [usize; 12],
}

impl Context {
    const fn zeroed() -> Self {
        Context { ra: 0, sp: 0, s: [0; 12] }
    }
}

// __switch(old: *mut Context, new: *const Context): store the live callee-saved
// registers into `*old`, load them from `*new`, and `ret` — which returns into
// `new`'s saved `ra` on `new`'s saved `sp`. Offsets: ra=0, sp=8, s0..s11=16..104.
core::arch::global_asm!(
    r#"
    .section .text
    .globl __switch
__switch:
    sd ra,   0(a0)
    sd sp,   8(a0)
    sd s0,   16(a0)
    sd s1,   24(a0)
    sd s2,   32(a0)
    sd s3,   40(a0)
    sd s4,   48(a0)
    sd s5,   56(a0)
    sd s6,   64(a0)
    sd s7,   72(a0)
    sd s8,   80(a0)
    sd s9,   88(a0)
    sd s10,  96(a0)
    sd s11,  104(a0)
    ld ra,   0(a1)
    ld sp,   8(a1)
    ld s0,   16(a1)
    ld s1,   24(a1)
    ld s2,   32(a1)
    ld s3,   40(a1)
    ld s4,   48(a1)
    ld s5,   56(a1)
    ld s6,   64(a1)
    ld s7,   72(a1)
    ld s8,   80(a1)
    ld s9,   88(a1)
    ld s10,  96(a1)
    ld s11,  104(a1)
    ret
"#
);

extern "C" {
    fn __switch(old: *mut Context, new: *const Context);
}

/// Per-thread kernel stack size.
const STACK_SIZE: usize = 64 * 1024;

/// A kernel thread: its saved context plus the stack backing it (kept alive in
/// the scheduler so the `sp` in `ctx` stays valid).
struct Thread {
    ctx: Context,
    _stack: Vec<u8>,
}

struct Sched {
    threads: Vec<Box<Thread>>,
    current: usize,
}

static SCHED: Mutex<Option<Sched>> = Mutex::new(None);

/// Initialize the scheduler with thread 0 = the current (boot) context, whose
/// registers are filled in the first time it yields away.
pub fn init() {
    *SCHED.lock() = Some(Sched {
        threads: alloc::vec![Box::new(Thread {
            ctx: Context::zeroed(),
            _stack: Vec::new(),
        })],
        current: 0,
    });
}

/// Spawn a kernel thread that begins at `entry` (which must never return).
pub fn spawn(entry: extern "C" fn() -> !) {
    let mut stack = alloc::vec![0u8; STACK_SIZE];
    let top = (stack.as_mut_ptr() as usize + stack.len()) & !0xf;
    let ctx = Context {
        ra: entry as usize,
        sp: top,
        s: [0; 12],
    };
    if let Some(s) = SCHED.lock().as_mut() {
        s.threads.push(Box::new(Thread { ctx, _stack: stack }));
    }
}

/// Cooperatively yield to the next thread (round-robin). No-op with fewer than
/// two threads.
pub fn yield_now() {
    let (old_ptr, new_ptr) = {
        let mut guard = SCHED.lock();
        let s = match guard.as_mut() {
            Some(s) => s,
            None => return,
        };
        let n = s.threads.len();
        if n < 2 {
            return;
        }
        let old = s.current;
        let next = (s.current + 1) % n;
        s.current = next;
        // Two shared borrows (old != next), with `old` reinterpreted as *mut.
        // Each `ctx` lives in a heap-stable Box, so the pointers stay valid after
        // the guard is dropped below — which it must be, since __switch transfers
        // control and would otherwise hold the lock across the switch.
        let old_ptr = &s.threads[old].ctx as *const Context as *mut Context;
        let new_ptr = &s.threads[next].ctx as *const Context;
        (old_ptr, new_ptr)
    };
    // SAFETY: distinct, valid, heap-stable Context pointers; the lock is released.
    unsafe { __switch(old_ptr, new_ptr) };
}
