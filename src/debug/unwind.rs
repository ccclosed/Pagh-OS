// debug/unwind.rs — RBP-based stack trace (frame pointer unwinding)
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// Works WITHOUT heap allocation — safe to call from panic handler.

const MAX_FRAMES: usize = 32;

/// Print a stack trace starting from the current stack frame.
///
/// Walks the RBP chain, printing each return address.
/// Stops when RBP is null or max depth is reached.
pub fn stack_trace() {
    let rbp: u64;
    // SAFETY: Reading RBP is always safe; it's a general-purpose register.
    unsafe {
        core::arch::asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack));
    }
    stack_trace_from(rbp);
}

/// Print a stack trace starting from a given RBP value.
///
/// Each stack frame layout (growing downward):
///   [rbp + 0] = saved previous RBP
///   [rbp + 8] = return address (RIP after call)
pub fn stack_trace_from(mut rbp: u64) {
    // SAFETY: kprintln! may use the serial port spinlock.
    // Since we're in a panic context with interrupts disabled,
    // this is safe as long as the serial lock isn't held by
    // the same CPU (it won't be — panic disables interrupts).
    crate::kprintln!("--- Stack trace (RBP unwind) ---");

    for depth in 0..MAX_FRAMES {
        if rbp == 0 {
            break;
        }

        // A valid frame pointer should be in kernel address space
        // (higher half: >= 0xFFFF8000_00000000).
        // For kernel stack traces, all frames are in higher half.
        if rbp < 0xFFFF8000_00000000 {
            crate::kprintln!("  [{}] (invalid RBP 0x{:016x} — not in kernel space)", depth, rbp);
            break;
        }

        // The saved RBP is at [rbp + 0]; the return address is at [rbp + 8].
        // SAFETY: We read from kernel stack addresses. RBP was verified to be
        // in kernel space. The pages are mapped and accessible.
        let saved_rbp_ptr = rbp as *const u64;
        let return_addr_ptr = (rbp + 8) as *const u64;

        let saved_rbp: u64;
        let return_addr: u64;
        unsafe {
            saved_rbp = core::ptr::read_volatile(saved_rbp_ptr);
            return_addr = core::ptr::read_volatile(return_addr_ptr);
        }

        if return_addr == 0 {
            crate::kprintln!("  [{}] (null return address — end of trace)", depth);
            break;
        }

        crate::kprintln!("  [{:>2}] 0x{:016x}", depth, return_addr);

        // Don't loop infinitely on the same frame
        if saved_rbp == rbp {
            crate::kprintln!("  [{}] (RBP loop detected)", depth + 1);
            break;
        }

        rbp = saved_rbp;
    }

    crate::kprintln!("--- End trace ---");
}
