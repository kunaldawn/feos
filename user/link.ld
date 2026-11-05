/*
 * How a user program is laid out, the same on both architectures. Code, read-only
 * data and writable data are in separate pages, since a page has one set of
 * permissions and the loader refuses anything both writable and executable.
 */
ENTRY(_start)

PHDRS
{
    text PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0x1000400000;
    .text : ALIGN(4096) { *(.text .text.*) } :text

    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) } :rodata

    . = ALIGN(4096);
    .data : { *(.data .data.*) *(.got .got.*) } :data
    .bss : { *(.bss .bss.*) *(COMMON) } :data

    /DISCARD/ : { *(.comment) *(.eh_frame*) *(.note*) *(.gnu*) }
}
