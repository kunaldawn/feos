/*
 * Linker script for the QEMU `virt` machine.
 *
 * RAM starts at 0x4000_0000. QEMU puts the device tree there when it starts a bare
 * ELF like this one, but only if all of it fits below the kernel, and it builds a
 * tree of 1 MiB. So the kernel loads 2 MiB up.
 */
ENTRY(_start)

/* PL011 data register, for messages before the kernel has a console. */
__early_uart = 0x09000000;
/* GIC distributor, for what only secure EL3 can set up. */
__early_gicd = 0x08000000;

SECTIONS
{
    . = 0x40200000;
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

    .stack (NOLOAD) : ALIGN(4K) {
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

ASSERT(__kernel_start == 0x40200000, "the kernel must link at QEMU's load address")
ASSERT(__stack_top - __stack_bottom >= 0x40000, "the boot stack shrank")
ASSERT(__stack_top % 16 == 0, "the boot stack must be 16-byte aligned")
ASSERT(__bss_end > __bss_start, ".bss is missing")
/* Bigger than the 512 MiB the scripts give QEMU means changing them too. */
ASSERT(__kernel_end - __kernel_start <= 0x20000000, "the image needs more than the default 512 MiB of RAM")
