/*
 * x86_64 multiboot entry.
 *
 * QEMU loads the flat image at 1 MiB (the address in the multiboot header)
 * and enters `_start` in 32-bit protected mode with:
 *   eax = 0x2BADB002
 *   ebx = physical address of the multiboot information structure
 *
 * We keep the image in a flat binary because QEMU's multiboot loader only
 * understands EM_386 ELF images. The build converts the linked ELF with
 * `objcopy -O binary`; `scripts/qemu-x64.sh` does that automatically.
 */

.set MB_MAGIC,    0x1BADB002
.set MB_FLAGS,    0x00010003          /* page align, memory info, load addrs */
.set MB_CHECKSUM, -(MB_MAGIC + MB_FLAGS)

.section .multiboot, "a"
.balign 4
    .long MB_MAGIC
    .long MB_FLAGS
    .long MB_CHECKSUM
    .long __kernel_start             /* header_addr */
    .long __kernel_start             /* load_addr */
    .long __load_end                 /* load_end_addr */
    .long __kernel_end               /* bss_end_addr */
    .long _start                     /* entry_addr */

.section .text.boot, "ax"
.code32
.global _start
.type _start, @function
_start:
    cli

    /* Keep the multiboot info pointer in a register that the BSS clear and
     * page-table setup below do not touch. */
    movl %ebx, %esi

    cld

    /* Without these checks, a CPU or machine that can't run the image would
     * triple-fault with no output. Everything from here to the far jump below
     * must not use the stack, which may be past the end of RAM. */
    movl $0x80000000, %eax
    cpuid
    movl $message_no_long_mode, %edi
    cmpl $0x80000001, %eax
    jb .Lboot_fail
    movl $0x80000001, %eax
    cpuid
    testl $(1 << 29), %edx
    jz .Lboot_fail

    /* The multiboot memory fields are valid when flag bit 0 is set. `mem_upper`
     * is the KiB of RAM above 1 MiB, and the whole image must fit in it. */
    testl $1, (%esi)
    jz .Lram_ok
    movl 8(%esi), %eax
    shll $10, %eax
    addl $0x100000, %eax
    movl $message_no_ram, %edi
    cmpl $__kernel_end, %eax
    jb .Lboot_fail
.Lram_ok:

    /* Zero .bss before using the page tables and stack that live there.
     * The region is 4-byte aligned and sized; `stosl` is four times fewer
     * iterations than `stosb` under emulation. */
    movl $__bss_start, %edi
    movl $__bss_end, %ecx
    subl %edi, %ecx
    shrl $2, %ecx
    xorl %eax, %eax
    rep stosl

    /* PML4[0] -> PDPT, present + writable. */
    movl $pml4, %edi
    movl $pdpt, %eax
    orl $0x3, %eax
    movl %eax, (%edi)
    movl $0, 4(%edi)

    /* PDPT[0..4] -> four page directories, present + writable. */
    movl $pdpt, %edi
    movl $page_directories, %eax
    orl $0x3, %eax
    movl $4, %ecx
.Lmap_directories:
    movl %eax, (%edi)
    movl $0, 4(%edi)
    addl $4096, %eax
    addl $8, %edi
    loop .Lmap_directories

    /* 2048 identity-mapped 2 MiB pages cover the first 4 GiB: all RAM, the
     * framebuffer, PCI MMIO, the IOAPIC and the local APIC. 2 MiB pages work on
     * every long-mode CPU, unlike 1 GiB pages. Everything from 2 GiB up is
     * uncached (PCD | PWT), because that's where the device windows are. */
    movl $page_directories, %edi
    xorl %ebx, %ebx
    movl $2048, %ecx
.Lmap_pages:
    movl %ebx, %eax
    orl $0x83, %eax                  /* present + writable + page size */
    testl $0x80000000, %ebx
    jz 2f
    orl $0x18, %eax                  /* PCD | PWT */
2:
    movl %eax, (%edi)
    movl $0, 4(%edi)
    addl $0x200000, %ebx
    addl $8, %edi
    loop .Lmap_pages

    /* Load our own GDT and reload CS with the flat 32-bit code descriptor.
     * Loading the pointer through a register keeps the operand absolute; a
     * direct symbol operand is assembled as RIP-relative in an ELF64 object,
     * which is not encodable in 32-bit mode. */
    movl $gdt_ptr, %eax
    lgdt (%eax)
    ljmp $0x18, $1f
1:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    movw %ax, %gs
    movw %ax, %ss

    /* Enable PAE. */
    movl %cr4, %eax
    orl $0x20, %eax
    movl %eax, %cr4

    /* Set EFER.LME. */
    movl $0xC0000080, %ecx
    rdmsr
    orl $0x100, %eax
    wrmsr

    /* Point CR3 at the PML4 and turn on paging (which enters long mode once
     * we load the 64-bit code segment below). Caches are on: the mapping above
     * is what keeps device windows uncached. */
    movl $pml4, %eax
    movl %eax, %cr3

    movl %cr0, %eax
    andl $0x9FFFFFFB, %eax           /* clear CD | NW | EM */
    orl $0x80010022, %eax            /* PG | WP | MP | NE */
    movl %eax, %cr0

    ljmp $0x08, $long_mode

/* Writes the NUL-terminated string at edi to COM1 and stops. No stack needed. */
.Lboot_fail:
    movw $__early_uart, %dx
1:  movb (%edi), %al
    testb %al, %al
    jz 2f
    outb %al, %dx
    incl %edi
    jmp 1b
2:  hlt
    jmp 2b
.size _start, . - _start

.code64
long_mode:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    movw %ax, %gs
    movw %ax, %ss

    /* The linker script reserves a 256 KiB boot stack. */
    movq $__stack_top, %rsp
    andq $-16, %rsp

    /* Enable SSE2 so the compiler may use the XMM registers for copies and
     * integer formatting. Interrupt entry saves and restores them. */
    movq %cr4, %rax
    orq $0x600, %rax                 /* OSFXSR | OSXMMEXCPT */
    movq %rax, %cr4

    movq %cr0, %rax
    andq $-5, %rax                   /* clear EM */
    orq $0x22, %rax                  /* MP | NE */
    movq %rax, %cr0
    fninit

    movl %esi, %edi                  /* multiboot info, zero-extended */
    call kmain

.Lhalt:
    cli
    hlt
    jmp .Lhalt

.section .rodata
.balign 8
gdt_start:
    .quad 0                          /* 0x00 null */
    .quad 0x00AF9A000000FFFF         /* 0x08 64-bit code, DPL0 */
    .quad 0x00CF92000000FFFF         /* 0x10 flat data */
    .quad 0x00CF9A000000FFFF         /* 0x18 flat 32-bit code */
gdt_end:
gdt_ptr:
    .word gdt_end - gdt_start - 1
    .long gdt_start
message_no_long_mode:
    .asciz "\r\nfeos: this CPU has no long mode (use -cpu max or qemu64)\r\n"
message_no_ram:
    .asciz "\r\nfeos: not enough RAM for the kernel image\r\n"

.section .bss
.balign 4096
pml4:
    .skip 4096
.balign 4096
pdpt:
    .skip 4096
.balign 4096
page_directories:
    .skip 4 * 4096

.section .note.GNU-stack, "", @progbits
