//! pagh OS — RISC-V (riscv64gc) boot seed (branch `riscv-port`, Milestone A).
//!
//! A minimal, verifiable S-mode kernel: OpenSBI hands control to `_start` at
//! 0x8020_0000, we set up a stack and call [`kmain`], which prints over the SBI
//! console and parks the hart. This proves the toolchain (riscv64gc + build-std),
//! the link layout, and the OpenSBI → S-mode boot path before the real arch layer
//! (traps, Sv39 paging, PLIC/CLINT, virtio-mmio) is grown on top per the spec.
#![no_std]
#![no_main]

use core::panic::PanicInfo;

// Entry trampoline. This is the first code at 0x8020_0000 (.text.entry). Set the
// stack pointer to the top of the linker-provided boot stack, then jump to Rust.
// `a0` (hartid) and `a1` (DTB pointer) are preserved as the SysV C arguments to
// kmain so later milestones can parse the device tree.
core::arch::global_asm!(
    r#"
    .section .text.entry
    .globl _start
_start:
    la      sp, _stack_top
    call    kmain
.hang:
    wfi
    j       .hang
"#
);

/// SBI legacy `console_putchar` (Extension ID 0x01): write one byte to the
/// firmware console. `a7` carries the extension id; `a0` the character.
fn sbi_putchar(c: u8) {
    // SAFETY: a plain SBI ecall with the legacy console-putchar calling
    // convention; clobbers only a0/a1 and does not touch memory or the stack.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") 1usize,
            in("a0") c as usize,
            lateout("a0") _,
            lateout("a1") _,
            options(nostack),
        );
    }
}

/// Write a string to the SBI console, translating `\n` to CRLF so host serial
/// terminals render line breaks correctly.
fn sbi_print(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            sbi_putchar(b'\r');
        }
        sbi_putchar(b);
    }
}

/// Park the current hart low-power until the next interrupt, forever.
fn park() -> ! {
    loop {
        // SAFETY: `wfi` is always valid; it simply waits for an interrupt.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}

/// Rust entry called from `_start` once the stack is set up.
#[no_mangle]
pub extern "C" fn kmain() -> ! {
    sbi_print("\n");
    sbi_print("========================================\n");
    sbi_print("  pagh OS  --  riscv64 (S-mode, OpenSBI)\n");
    sbi_print("========================================\n");
    sbi_print("rv: Milestone A boot OK -- SBI console up\n");
    sbi_print("rv: parking hart (wfi). Next: DTB parse + PMM + Sv39.\n");
    park();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    sbi_print("\nrv: PANIC: ");
    if let Some(s) = info.message().as_str() {
        sbi_print(s);
    } else {
        sbi_print("(no message)");
    }
    sbi_print("\n");
    park();
}
