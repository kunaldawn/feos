/*
 * Linker script for the QEMU `virt` machine.
 *
 * RAM starts at 0x4000_0000. We load the kernel at +0x8_0000, which is where
 * QEMU's arm64 boot loader expects a Linux-style image and leaves the first
 * 512 KiB free for the device tree / boot data.
 */
ENTRY(_start)

/* PL011 data register, for messages before the kernel has a console. */
__early_uart = 0x09000000;

SECTIONS
{
    . = 0x40080000;
    __kernel_start = .;

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

ASSERT(__kernel_start == 0x40080000, "the kernel must link at QEMU's load address")
ASSERT(__stack_top - __stack_bottom >= 0x40000, "the boot stack shrank")
ASSERT(__stack_top % 16 == 0, "the boot stack must be 16-byte aligned")
ASSERT(__bss_end > __bss_start, ".bss is missing")
/* Bigger than the 512 MiB the scripts give QEMU means changing them too. */
ASSERT(__kernel_end - __kernel_start <= 0x20000000, "the image needs more than the default 512 MiB of RAM")
