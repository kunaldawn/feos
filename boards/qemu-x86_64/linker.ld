/*
 * Linker script for QEMU's x86_64 PC machine.
 *
 * QEMU's multiboot loader reads the load addresses from the multiboot header
 * in the first 8 KiB of the flat image. The image is linked at 1 MiB, the
 * traditional PC kernel load address.
 */
ENTRY(_start)

/* COM1 data port, for messages before the kernel has a console. */
__early_uart = 0x3f8;

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
        /* Never used. An overflow lands here first, which leaves the scheduler's
         * tick time to notice it before .bss is overwritten. */
        . += 0x40000;
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

ASSERT(__kernel_start == 0x100000, "the kernel must link at the multiboot load address")
ASSERT(ADDR(.multiboot) - __kernel_start < 8192, "the multiboot header must be in the first 8 KiB")
ASSERT(__load_end <= __bss_start, "file-backed data must end before .bss")
ASSERT(__stack_top - __stack_bottom >= 0x40000, "the boot stack shrank")
ASSERT(__stack_top % 16 == 0, "the boot stack must be 16-byte aligned")
ASSERT(__bss_end > __bss_start, ".bss is missing")
/* Bigger than the 512 MiB the scripts give QEMU means changing them too. */
ASSERT(__kernel_end - __kernel_start <= 0x20000000, "the image needs more than the default 512 MiB of RAM")
