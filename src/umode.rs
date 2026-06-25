//! User mode (U-mode) entry and the `ecall` system-call path.
//!
//! [`enter`] drops to U-mode via `sret`; [`syscall`] is the `ecall` dispatcher
//! (called from the trap handler). The program loaded into U-mode is produced by
//! the ELF loader ([`crate::elf`]); the demo set of syscalls below is minimal
//! (the Linux ABI mapping replaces it later).

use core::arch::asm;

/// System-call numbers (carried in `a7`).
pub const SYS_EXIT: usize = 1;
pub const SYS_PRINT_U64: usize = 2;

/// Top of the user stack VA (above the kernel's identity window at 0..4 GiB).
pub const USER_STACK_TOP: usize = 0x1_0001_0000;

/// Drop to U-mode: set `sepc` to the user entry, clear `sstatus.SPP` (so `sret`
/// returns to U-mode) and set `SPIE` (interrupts enabled in U-mode), point `sp`
/// at the user stack, and `sret`.
///
/// # Safety
/// `entry`/`sp` must reference mapped, user-accessible pages.
pub unsafe fn enter(entry: usize, sp: usize) -> ! {
    asm!(
        "csrw sepc, {entry}",
        "csrc sstatus, {spp}",   // SPP=0 -> return to U-mode
        "csrs sstatus, {spie}",  // SPIE=1 -> interrupts on after sret
        "mv sp, {usp}",
        "sret",
        entry = in(reg) entry,
        spp = in(reg) 1usize << 8,
        spie = in(reg) 1usize << 5,
        usp = in(reg) sp,
        options(noreturn),
    );
}

/// Dispatch a system call from the `ecall` trap. `a0` is the first argument;
/// the return value goes back in the user's `a0`. `SYS_EXIT` does not return —
/// it reports completion and launches the interactive shell.
pub fn syscall(num: usize, a0: usize) -> usize {
    match num {
        SYS_PRINT_U64 => {
            crate::kprintln!("    [user] print_u64({})", a0);
            0
        }
        SYS_EXIT => {
            crate::kprintln!("    [user] exit({})", a0);
            crate::kprintln!("rv: U-mode ELF made syscalls and exited.");
            crate::kprintln!("rv: Milestone D OK -- U-mode + ecall syscalls (real ELF).");
            crate::kprintln!("rv: all milestones up; launching interactive shell.");
            crate::shell::run();
        }
        _ => {
            crate::kprintln!("    [user] unknown syscall a7={}", num);
            usize::MAX
        }
    }
}
