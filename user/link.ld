/*
 * How a user program is laid out, the same on both architectures. Code, read-only
 * data and writable data are in separate pages, since a page has one set of
 * permissions and the loader refuses anything both writable and executable.
 *
 * The ELF and program headers are loaded too, at the start of the read-only segment,
 * so the aux vector's `AT_PHDR` points at them (section 7.2 of docs/feos-abi-v2.md).
 */
ENTRY(_start)

PHDRS
{
    headers PT_PHDR PHDRS;
    rodata PT_LOAD FILEHDR PHDRS FLAGS(4);
    text PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0x1000400000 + SIZEOF_HEADERS;
    .rodata : { *(.rodata .rodata.*) } :rodata

    . = ALIGN(4096);
    .text : { *(.text .text.*) } :text

    . = ALIGN(4096);
    .data : { *(.data .data.*) *(.got .got.*) } :data
    .bss : { *(.bss .bss.*) *(COMMON) } :data

    /DISCARD/ : { *(.comment) *(.eh_frame*) *(.note*) *(.gnu*) }
}
