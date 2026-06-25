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

/// Heap-free, best-effort backtrace by SCANNING the kernel stack for return
/// addresses that lie within the kernel code image.
///
/// Used by fatal exception handlers when the normal RBP chain is useless — e.g.
/// after a control-flow corruption that transferred to a garbage address
/// (`RIP=0x1`), where neither RIP nor RBP point at real frames. We instead walk
/// up the faulting stack from `rsp` and print every 8-byte slot whose value
/// falls in `[kernel_start, kernel_end)`; those are almost all genuine return
/// addresses, so the list reconstructs the call chain that was live at the fault.
///
/// SAFETY/robustness: the scan is capped at the TOP of `rsp`'s per-PID kernel
/// stack slot (derived from `layout::KERNEL_STACK_REGION_BASE`/`STRIDE`) so it
/// can never read into the adjacent unmapped guard page and trigger a nested
/// fault. If `rsp` is not in the per-PID stack region (e.g. the boot/idle
/// stack), a small fixed window is scanned instead.
pub fn stack_scan_backtrace(rsp: u64, max_qwords: usize) {
    use crate::memory::layout;

    crate::kprintln!("--- Stack scan from RSP=0x{:016x} (return addrs in kernel image) ---", rsp);

    // Must be a canonical higher-half, 8-aligned pointer to be usable.
    if rsp < 0xFFFF_8000_0000_0000 || (rsp & 7) != 0 {
        crate::kprintln!("  (RSP not a usable kernel stack pointer)");
        crate::kprintln!("--- End scan ---");
        return;
    }

    let kstart = layout::kernel_start();
    let kend = layout::kernel_end();

    // Upper bound: never read past the top of this stack slot (next slot's
    // unmapped guard page), so the scan itself cannot fault.
    let default_limit = rsp.saturating_add((max_qwords as u64) * 8);
    let limit = if rsp >= layout::KERNEL_STACK_REGION_BASE {
        let off = rsp - layout::KERNEL_STACK_REGION_BASE;
        let slot = off / layout::KERNEL_STACK_STRIDE;
        let slot_top = layout::KERNEL_STACK_REGION_BASE
            + slot * layout::KERNEL_STACK_STRIDE
            + layout::KERNEL_STACK_STRIDE; // start of next slot = its guard page
        core::cmp::min(default_limit, slot_top)
    } else {
        default_limit
    };

    let mut printed = 0usize;
    let mut addr = rsp;
    while addr + 8 <= limit {
        // SAFETY: `addr` is 8-aligned, higher-half, and strictly below the top
        // of the mapped stack slot, so the read is within mapped memory.
        let val = unsafe { core::ptr::read_volatile(addr as *const u64) };
        if val >= kstart && val < kend {
            crate::kprintln!("  [{:>2}] @0x{:016x} -> 0x{:016x}", printed, addr, val);
            printed += 1;
            if printed >= 24 {
                crate::kprintln!("  ... (truncated at 24 frames)");
                break;
            }
        }
        addr += 8;
    }
    if printed == 0 {
        crate::kprintln!("  (no in-image return addresses found on the stack)");
    }
    crate::kprintln!("--- End scan ---");
}
