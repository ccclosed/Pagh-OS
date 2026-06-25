//! User mode (U-mode) entry and the `ecall` system-call path.
//!
//! A tiny hand-assembled user program is mapped into a fresh user page (U-bit
//! set), the kernel drops to U-mode via `sret`, and the program makes two
//! `ecall` system calls — `print_u64` then `exit`. This proves the privilege
//! drop and the `ecall` trap/dispatch round trip before the real Linux ELF
//! loader + syscall ABI are ported on top (Milestone D follow-on).

use core::arch::asm;

/// System-call numbers (carried in `a7`). A minimal demo set; the Linux ABI
/// mapping replaces this later.
pub const SYS_EXIT: usize = 1;
pub const SYS_PRINT_U64: usize = 2;

/// Hand-assembled RV64 user program (position-independent: only `li`/`ecall`/
/// `j`, no memory access), mapped at [`USER_CODE_VA`]:
///   li a0, 42; li a7, 2 (print_u64); ecall;
///   li a0, 0;  li a7, 1 (exit);      ecall;
///   j .  (safety loop)
static USER_PROG: [u32; 7] = [
    0x02a0_0513, // addi a0, x0, 42
    0x0020_0893, // addi a7, x0, 2   (SYS_PRINT_U64)
    0x0000_0073, // ecall
    0x0000_0513, // addi a0, x0, 0
    0x0010_0893, // addi a7, x0, 1   (SYS_EXIT)
    0x0000_0073, // ecall
    0x0000_006f, // jal x0, 0        (j .)
];

/// User virtual addresses (above the kernel's identity window at 0..4 GiB).
const USER_CODE_VA: usize = 0x1_0000_0000;
const USER_STACK_TOP: usize = 0x1_0001_0000;

/// Map the user code + stack pages and return `(entry_va, user_sp)`.
pub fn setup() -> (usize, usize) {
    let code = crate::pmm::alloc_frame().expect("pmm: user code frame");
    // SAFETY: `code` is an owned frame, identity-mapped, so writable here.
    unsafe {
        let dst = code as *mut u32;
        for (i, w) in USER_PROG.iter().enumerate() {
            dst.add(i).write_volatile(*w);
        }
    }

    let stack = crate::pmm::alloc_frame().expect("pmm: user stack frame");

    // SAFETY: paging is active; these VAs are above the identity window.
    unsafe {
        crate::paging::map_user(USER_CODE_VA, code, true, false); // R+X+U
        crate::paging::map_user(USER_STACK_TOP - 0x1000, stack, false, true); // R+W+U
    }
    crate::paging::flush();

    (USER_CODE_VA, USER_STACK_TOP & !0xf)
}

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
/// it reports completion and parks (the demo's terminal state).
pub fn syscall(num: usize, a0: usize) -> usize {
    match num {
        SYS_PRINT_U64 => {
            crate::kprintln!("    [user] print_u64({})", a0);
            0
        }
        SYS_EXIT => {
            crate::kprintln!("    [user] exit({})", a0);
            crate::kprintln!("rv: U-mode process made syscalls and exited.");
            crate::kprintln!("rv: Milestone D OK -- U-mode + ecall syscalls.");
            crate::kprintln!("rv: next: PLIC + interactive UART RX, then virtio-net + smoltcp.");
            crate::cpu::park();
        }
        _ => {
            crate::kprintln!("    [user] unknown syscall a7={}", num);
            usize::MAX
        }
    }
}
