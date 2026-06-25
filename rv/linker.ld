/* RISC-V S-mode kernel link layout for the QEMU `virt` machine.
 *
 * OpenSBI (M-mode, loaded as -bios) jumps to the supervisor payload at
 * 0x8020_0000, where QEMU's -kernel ELF loader places this image. _start
 * (in .text.entry) must therefore be the very first thing at that address. */
OUTPUT_ARCH(riscv)
ENTRY(_start)

SECTIONS
{
    . = 0x80200000;

    .text : {
        *(.text.entry)
        *(.text .text.*)
    }

    .rodata : { *(.rodata .rodata.*) }
    .data   : { *(.data .data.*) }

    .bss : {
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        . = ALIGN(16);
        _stack_bottom = .;
        . += 0x10000;          /* 64 KiB boot stack */
        _stack_top = .;
    }

    /DISCARD/ : { *(.eh_frame .eh_frame_hdr .comment) }
}
