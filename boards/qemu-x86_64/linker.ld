/*
 * Linker script for QEMU's x86_64 PC machine.
 *
 * QEMU's multiboot loader reads the load addresses from the multiboot header
 * in the first 8 KiB of the flat image. The image is linked at 1 MiB, the
 * traditional PC kernel load address.
 */
ENTRY(_start)

SECTIONS
{
    . = 0x100000;
    __kernel_start = .;

    .multiboot : {
        KEEP(*(.multiboot))
    }

    .text : ALIGN(4K) {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
        *(.got .got.*)
    }

    .data : ALIGN(4K) {
        *(.data .data.*)
    }

    /* End of the file-backed image. QEMU zero-fills from here to
     * __kernel_end for the .bss and the boot stack. This is deliberately not
     * page-aligned: objcopy writes exactly this many bytes, and the
     * multiboot loader requires the file to contain load_end - load_addr. */
    __load_end = .;

    .bss (NOLOAD) : ALIGN(4K) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    .stack (NOLOAD) : ALIGN(16) {
        __stack_bottom = .;
        . += 0x40000;          /* 256 KiB kernel stack */
        __stack_top = .;
    }

    __kernel_end = .;

    /DISCARD/ : {
        *(.comment)
        *(.eh_frame .eh_frame.*)
        *(.note .note.*)
    }
}
