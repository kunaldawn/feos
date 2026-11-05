/*
 * x86_64 multiboot entry.
 *
 * QEMU loads the flat image at 32 MiB (the address in the multiboot header)
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

/*
 * From protected mode with a flat stack-free state to long mode. Shared by the
 * boot CPU and the others: enables PAE, sets EFER.LME, loads the boot page tables
 * and turns on paging, then far-jumps to \target. Clobbers eax, ecx, edx.
 */
.macro ENTER_LONG_MODE target
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
     * we load the 64-bit code segment below). Caches are on: the mapping
     * is what keeps device windows uncached. */
    movl $pml4, %eax
    movl %eax, %cr3

    movl %cr0, %eax
    andl $0x9FFFFFFB, %eax           /* clear CD | NW | EM */
    orl $0x80010022, %eax            /* PG | WP | MP | NE */
    movl %eax, %cr0

    ljmp $0x08, $\target
.endm

/*
 * What every CPU needs in long mode before any Rust: the data segments and the
 * x87/SSE state. The compiler uses XMM registers for everything, including float
 * arithmetic, so CR0.EM must be clear and CR4.OSFXSR set before the first call.
 * Interrupt entry saves and restores the state. Clobbers rax.
 */
.macro CPU_LONG_MODE_SETUP
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    movw %ax, %gs
    movw %ax, %ss

    movq %cr4, %rax
    orq $0x600, %rax                 /* OSFXSR | OSXMMEXCPT */
    movq %rax, %cr4

    movq %cr0, %rax
    andq $-5, %rax                   /* clear EM */
    orq $0x22, %rax                  /* MP | NE */
    movq %rax, %cr0
    fninit
.endm

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
/* How the EFI loader finds the entry point firmware needs. */
.balign 8
    .ascii "FEOSEFI\0"
    .quad _start_efi

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
    ENTER_LONG_MODE long_mode

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

/*
 * Where the EFI loader comes in, in long mode, on firmware's identity map, with
 * rdi = the handoff, below 4 GiB. It sets up what the 32-bit path sets up, and joins
 * it. No stack until this has its own.
 */
.code64
.global _start_efi
.type _start_efi, @function
_start_efi:
    cli
    cld
    clts
    movq %rdi, %r12

    movl $__bss_start, %edi
    movl $__bss_end, %ecx
    subq %rdi, %rcx
    shrq $3, %rcx
    xorl %eax, %eax
    rep stosq

    movl $pdpt, %eax
    orq $0x3, %rax
    movl $pml4, %edi
    movq %rax, (%rdi)

    movl $page_directories, %eax
    orq $0x3, %rax
    movl $pdpt, %edi
    movl $4, %ecx
1:  movq %rax, (%rdi)
    addq $4096, %rax
    addq $8, %rdi
    loop 1b

    /* The same 2 MiB pages as the 32-bit path: see there. */
    movl $page_directories, %edi
    xorl %ebx, %ebx
    movl $2048, %ecx
2:  movq %rbx, %rax
    orq $0x83, %rax
    testl $0x80000000, %ebx
    jz 3f
    orq $0x18, %rax
3:  movq %rax, (%rdi)
    addq $0x200000, %rbx
    addq $8, %rdi
    loop 2b

    movl $pml4, %eax
    movq %rax, %cr3
    lgdtq gdt_ptr(%rip)
    pushq $0x08
    leaq 4f(%rip), %rax
    pushq %rax
    lretq
4:  movq %r12, %rsi
    jmp long_mode
.size _start_efi, . - _start_efi

long_mode:
    CPU_LONG_MODE_SETUP

    /* The linker script reserves a 256 KiB boot stack. */
    movq $__stack_top, %rsp
    andq $-16, %rsp

    /* GS points at this CPU's area, for `cpu::index`. It's zero until the area is
     * written, which is right for the boot CPU. */
    movl $0xC0000101, %ecx           /* IA32_GS_BASE */
    movl $CPU_AREAS, %eax
    xorl %edx, %edx
    wrmsr

    movl %esi, %edi                  /* multiboot info, zero-extended */
    call kmain

.Lhalt:
    cli
    hlt
    jmp .Lhalt

/*
 * Where the other CPUs start. A startup IPI enters real mode at a page-aligned
 * address below 1 MiB, so `smp::start_secondaries` copies this page-relocatable
 * stub to AP_TRAMPOLINE. It only gets to 32-bit protected mode, and the rest runs
 * from the kernel image like the boot CPU's path.
 */
.set AP_TRAMPOLINE, 0x8000

.code16
.global ap_trampoline_start
ap_trampoline_start:
    cli
    cld
    xorw %ax, %ax
    movw %ax, %ds
    lgdtl (AP_TRAMPOLINE + (ap_gdt_ptr - ap_trampoline_start))
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmpl $0x18, $ap_protected
.balign 8
ap_gdt_ptr:
    .word gdt_end - gdt_start - 1
    .long gdt_start
.global ap_trampoline_end
ap_trampoline_end:

.code32
ap_protected:
    ENTER_LONG_MODE ap_long_mode

.code64
ap_long_mode:
    CPU_LONG_MODE_SETUP
    /* The starting CPU waits for `x86_ap_entry` before writing these again. */
    movq ap_boot_stack, %rsp
    andq $-16, %rsp
    movq ap_boot_index, %rdi
    call x86_ap_entry
    jmp .Lhalt

.section .rodata
.balign 8
gdt_start:
    .quad 0                          /* 0x00 null */
    .quad 0x00AF9A000000FFFF         /* 0x08 64-bit code, DPL0 */
    .quad 0x00CF92000000FFFF         /* 0x10 flat data */
    .quad 0x00CF9A000000FFFF         /* 0x18 flat 32-bit code */
gdt_end:
/* Six bytes for a 32-bit LGDT, and the whole of it for a 64-bit one. */
gdt_ptr:
    .word gdt_end - gdt_start - 1
    .quad gdt_start
message_no_long_mode:
    .asciz "\r\nfeos: this CPU has no long mode (use -cpu max or qemu64)\r\n"
message_no_ram:
    .asciz "\r\nfeos: not enough RAM for the kernel image\r\n"

.section .bss
.global ap_boot_stack
.balign 8
ap_boot_stack:
    .skip 8
.global ap_boot_index
ap_boot_index:
    .skip 8
.balign 4096
.global pml4
pml4:
    .skip 4096
.balign 4096
.global pdpt
pdpt:
    .skip 4096
.balign 4096
.global page_directories
page_directories:
    .skip 4 * 4096

.section .note.GNU-stack, "", @progbits
