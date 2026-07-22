OUTPUT_FORMAT("elf64-x86-64")
OUTPUT_ARCH(i386:x86-64)

ENTRY(_start)

PHDRS
{
    text    PT_LOAD FLAGS((1 << 0) | (1 << 2)); /* Execute + Read */
    rodata  PT_LOAD FLAGS((1 << 2));            /* Read Only */
    data    PT_LOAD FLAGS((1 << 1) | (1 << 2)); /* Write + Read */
}

SECTIONS
{
    . = 0xffffffff80000000;

    __kernel_start = .;

    /* Без этой секции Limine скажет, что конфиг не содержит валидных записей для ядер */
    .requests : ALIGN(4K) {
        KEEP(*(.requests .requests.*))
    } :text

    .text : ALIGN(4K) {
        *(.text .text.*)
    } :text

    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
    } :rodata

    .data : ALIGN(4K) {
        *(.data .data.*)
    } :data

    .got : ALIGN(4K) {
        *(.got .got.*)
    } :data

    .bss : ALIGN(4K) {
        __bss_start = .;
        *(COMMON)
        *(.bss .bss.*)
        __bss_end = .;
    } :data

    . = ALIGN(4K);
    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame)
        *(.note .note.*)
    }
}