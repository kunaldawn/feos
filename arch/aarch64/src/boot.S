/*
 * AArch64 entry point.
 *
 * QEMU loads the ELF at 0x4020_0000 and starts us with:
 *   x0 = physical address of the device tree blob, or 0, which is what a bare ELF
 *        gets, with the tree at the start of RAM
 *   MMU off, caches off, interrupts masked
 *
 * That's at EL1 by default, EL2 with virtualization=on and EL3 with secure=on. Firmware
 * can start us at EL1 or EL2. `el_setup` gets each CPU to EL1 with a known SCTLR_EL1.
 * The EFI loader passes a handoff in x0 instead, with the MMU off as here.
 *
 * Boot turns the MMU on with an identity map, which is what makes RAM Normal
 * memory: see below.
 */

.equ SMP_MAX_CPUS, 8
.equ SMP_STACK_SIZE, 0x8000           /* 32 KiB until the first tick */
.equ IRQ_STACK_SIZE, 0x8000           /* 32 KiB per CPU: vectors.S has this too */
.equ TRAP_FRAME_SIZE, 800             /* size of irq::TrapFrame */

.section .text.boot, "ax"
.global _start
.type _start, %function
_start:
    /* Keep the device tree pointer for kmain. */
    mov x19, x0
    bl  el_setup
    bl  invalidate_dcache

    /*
     * CPUs started through PSCI enter at `secondary_entry` below. If a loader
     * starts every core at `_start` instead, keep the non-boot CPUs parked.
     */
    mrs x0, mpidr_el1
    and x0, x0, #0xff
    cbnz x0, .Lpark

    /* The boot CPU is logical CPU 0; `cpu::index` reads this register. */
    mov x0, #0
    msr tpidr_el1, x0

    /* Catch a fault from here until the kernel installs its own vectors. */
    adrp x0, early_vector_table
    add  x0, x0, :lo12:early_vector_table
    msr  vbar_el1, x0
    isb

    /* Touch the last word of the image, including .bss and the stack. QEMU
     * doesn't check the image against the RAM size, so without this a small
     * -m would fault silently during the clear below. */
    adrp x0, __kernel_end
    add  x0, x0, :lo12:__kernel_end
    ldr  x0, [x0, #-8]

    /* SP_EL1 is just past the frame traps save into, until the scheduler gives
     * this CPU a task's. The boot thread, like every thread, runs on SP_EL0 (EL1t),
     * so taking an exception switches stacks in hardware. See vectors.S. */
    adrp x0, boot_frames
    add  x0, x0, :lo12:boot_frames
    add  x0, x0, #TRAP_FRAME_SIZE
    mov  sp, x0
    adrp x0, __stack_top
    add  x0, x0, :lo12:__stack_top
    msr  sp_el0, x0
    msr  spsel, #0

    /* Enable FP/SIMD at EL1 and EL0 (CPACR_EL1.FPEN = 0b11). Rust may use
     * NEON registers for memcpy and formatting, which otherwise trap. */
    mrs x0, cpacr_el1
    orr x0, x0, #(0x3 << 20)
    msr cpacr_el1, x0
    isb

    /* Zero the .bss section (Rust assumes statics start as zero). */
    adrp x0, __bss_start
    add  x0, x0, :lo12:__bss_start
    adrp x1, __bss_end
    add  x1, x1, :lo12:__bss_end
.Lbss_loop:
    cmp x0, x1
    b.hs .Lbss_done
    str xzr, [x0], #8
    b   .Lbss_loop
.Lbss_done:
    bl  build_page_table
    bl  enable_mmu

    mov x0, x19
    bl  kmain

.Lpark:
    wfe
    b .Lpark
.size _start, . - _start

/*
 * PSCI CPU_ON entry point.
 *
 *   x0 = context id chosen by the caller (the logical CPU index)
 *
 * Secondary CPUs arrive with the MMU, caches and interrupts off and no stack.
 * Give each one a private stack and the shared exception table, then hand it to
 * Rust. As on the boot CPU, SP_EL1 is where traps save and threads use SP_EL0. The Rust handler installs its CPU interface and joins the
 * scheduler; it does not return.
 */

.section .text.boot, "ax"
.global secondary_entry
.type secondary_entry, %function
secondary_entry:
    mov x19, x0
    bl  el_setup
    bl  invalidate_dcache
    mov x0, x19
    msr tpidr_el1, x0

    /* sp = boot_frames + (index + 1) * TRAP_FRAME_SIZE */
    adrp x1, boot_frames
    add  x1, x1, :lo12:boot_frames
    mov  x2, #TRAP_FRAME_SIZE
    madd x1, x0, x2, x1
    add  x1, x1, x2
    mov  sp, x1

    /* sp_el0 = secondary_stacks + (index + 1) * SMP_STACK_SIZE */
    adrp x1, secondary_stacks
    add  x1, x1, :lo12:secondary_stacks
    mov  x2, #SMP_STACK_SIZE
    madd x1, x0, x2, x1
    add  x1, x1, x2
    msr  sp_el0, x1
    msr  spsel, #0

    /* Enable FP/SIMD at EL1, as the boot CPU did. */
    mrs x1, cpacr_el1
    orr x1, x1, #(0x3 << 20)
    msr cpacr_el1, x1
    isb

    /* The boot CPU built the page table before starting this CPU. */
    bl  enable_mmu

    /* Every core needs VBAR_EL1 pointing at the shared vector table. */
    adrp x1, exception_vector_table
    add  x1, x1, :lo12:exception_vector_table
    msr vbar_el1, x1
    isb

    mov x0, x19
    bl  secondary_main

.Lsecondary_park:
    wfe
    b .Lsecondary_park
.size secondary_entry, . - secondary_entry

/*
 * Gets this CPU to EL1, with SCTLR_EL1 in a state that's known instead of whatever
 * came before. Returns at EL1, with the stack pointer not yet set. Clobbers x0-x2.
 *
 * Nothing's above to trap: the kernel doesn't use EL2 or EL3, and this leaves the
 * minimum that lets EL1 run as if they weren't there.
 */
.equ SCTLR_EL1_KNOWN, 0x30d00800      /* the RES1 bits; MMU and caches off, little endian */
.equ SCTLR_EL2_KNOWN, 0x30c50830      /* the same for EL2 */
.equ HCR_RW, (1 << 31)                /* EL1 is AArch64, and nothing else is set */
.equ CPTR_EL2_KNOWN, 0x33ff           /* the RES1 bits; FP/SIMD doesn't trap */
.equ CNTHCTL_EL1_ACCESS, 3            /* EL1 may use the physical counter and timer */
.equ SPSR_EL1H, 0x3c5                 /* EL1 on SP_EL1, everything masked */
.equ SPSR_EL2H, 0x3c9                 /* EL2 on SP_EL2, everything masked */
.equ SCR_EL3_KNOWN, 0x531             /* non-secure below, EL2 allowed, AArch64, RES1 */
.equ ICC_SRE_ENABLE, 0x9              /* SRE and Enable: EL1 may use the GICv3 sysregs */

.type el_setup, %function
el_setup:
    mrs  x0, CurrentEL
    and  x0, x0, #0xc
    cmp  x0, #0xc
    b.ne 2f

    /*
     * EL3, which only secure=on reaches without firmware. A GICv2 has its security
     * extensions there, with every interrupt in group 0, which non-secure EL1 can't
     * see. Everything goes to group 1, on this CPU for its own and for all the rest.
     * That's all there is for the machine to be without secure firmware: a real
     * machine would have some, and start this at a lower level.
     */
    mrs  x0, id_aa64pfr0_el1
    ubfx x0, x0, #24, #4              /* a GICv3's system registers? */
    cbnz x0, 1f
    adrp x1, __early_gicd
    add  x1, x1, :lo12:__early_gicd
    mov  x0, #-1
    mov  x2, #0x80
0:  str  w0, [x1, x2]                 /* GICD_IGROUPRn */
    add  x2, x2, #4
    cmp  x2, #0x100
    b.lo 0b
    mov  w0, #3
    str  w0, [x1]                     /* GICD_CTLR, both groups */
    movz x1, #0x0801, lsl #16
    str  w0, [x1]                     /* GICC_CTLR: the kernel enables its own group */
    mov  w0, #0xff
    str  w0, [x1, #4]                 /* GICC_PMR: non-secure can't raise it from zero */
1:
    mov  x0, #SCR_EL3_KNOWN
    msr  scr_el3, x0
    msr  cptr_el3, xzr
    msr  mdcr_el3, xzr
    mrs  x0, id_aa64pfr0_el1
    ubfx x0, x0, #8, #4               /* EL2 implemented? */
    mov  x1, #SPSR_EL1H
    cbz  x0, 1f
    mov  x1, #SPSR_EL2H
1:  msr  spsr_el3, x1
    adr  x1, 2f
    msr  elr_el3, x1
    eret

2:  mrs  x0, CurrentEL
    and  x0, x0, #0xc
    cmp  x0, #8
    b.ne 4f

    /* EL2: everything off, then EL1 running as if nothing's above it. */
    ldr  x0, =SCTLR_EL2_KNOWN
    msr  sctlr_el2, x0
    isb
    ldr  x0, =HCR_RW
    msr  hcr_el2, x0
    ldr  x0, =CPTR_EL2_KNOWN
    msr  cptr_el2, x0
    msr  hstr_el2, xzr
    mrs  x0, cnthctl_el2
    orr  x0, x0, #CNTHCTL_EL1_ACCESS
    msr  cnthctl_el2, x0
    msr  cntvoff_el2, xzr
    /* What EL1 reads as its ID registers. Left unset, they're anything. */
    mrs  x0, midr_el1
    msr  vpidr_el2, x0
    mrs  x0, mpidr_el1
    msr  vmpidr_el2, x0
    msr  vttbr_el2, xzr
    /* A GICv3's CPU interface is reached through system registers, if this says so. */
    mrs  x0, id_aa64pfr0_el1
    ubfx x0, x0, #24, #4
    cbz  x0, 3f
    mrs  x0, S3_4_C12_C9_5            /* ICC_SRE_EL2 */
    mov  x1, #ICC_SRE_ENABLE
    orr  x0, x0, x1
    msr  S3_4_C12_C9_5, x0
    isb
3:  ldr  x0, =SCTLR_EL1_KNOWN
    msr  sctlr_el1, x0
    mov  x0, #SPSR_EL1H
    msr  spsr_el2, x0
    msr  elr_el2, x30
    eret

4:  ldr  x0, =SCTLR_EL1_KNOWN
    msr  sctlr_el1, x0
    isb
    ret
.size el_setup, . - el_setup

/*
 * Replaces the descriptor at x0 with x1, on the way through no translation at all.
 *
 * The architecture wants a live block replaced by way of an invalid entry and a TLB
 * invalidation (break-before-make), and that can't be done to the block this code is
 * running from: with the entry invalid, there's no next instruction. With the MMU off
 * and the map an identity map, the addresses are the same and none of them use the
 * entry. Nothing here touches the stack or any data other than the entry, which is
 * why this is all registers.
 *
 * For the boot CPU with interrupts masked and no other CPU running.
 */
.section .text.boot, "ax"
.global replace_descriptor
.type replace_descriptor, %function
replace_descriptor:
    dsb  ish
    dc   civac, x0                    /* no cached copy to differ from what's written */
    dsb  ish
    mrs  x2, sctlr_el1
    bic  x3, x2, #1
    msr  sctlr_el1, x3
    isb
    str  x1, [x0]
    dsb  sy
    tlbi vmalle1is
    dsb  ish
    ic   iallu
    dsb  ish
    isb
    msr  sctlr_el1, x2
    isb
    ret
.size replace_descriptor, . - replace_descriptor

/*
 * Invalidates this CPU's data caches, by set and way, down to the point of
 * coherency. Runs with the MMU off, first thing, because the architecture doesn't
 * promise they're empty after reset: the stale lines of a warm reboot or of firmware
 * would otherwise come back to life once the MMU turns caching on. The walk has no
 * effect on QEMU's TCG, which models no caches. Clobbers x0-x11, x16 and x17.
 */
.type invalidate_dcache, %function
invalidate_dcache:
    mrs  x0, clidr_el1
    ubfx w3, w0, #24, #3              /* levels to the point of coherency */
    lsl  w3, w3, #1                   /* as CSSELR counts them */
    cbz  w3, 5f
    mov  w10, #0                      /* twice the level */
    mov  w8, #1
1:  add  w2, w10, w10, lsr #1         /* three times the level */
    lsr  w1, w0, w2
    and  w1, w1, #7
    cmp  w1, #2
    b.lt 4f                           /* no data cache here */
    msr  csselr_el1, x10
    isb
    mrs  x1, ccsidr_el1
    and  w2, w1, #7
    add  w2, w2, #4                   /* log2 of the line size */
    ubfx w4, w1, #3, #10              /* ways, less one */
    clz  w5, w4
    lsl  w9, w4, w5
    lsl  w16, w8, w5
2:  ubfx w7, w1, #13, #15             /* sets, less one */
    lsl  w7, w7, w2
    lsl  w17, w8, w2
3:  orr  w11, w10, w9
    orr  w11, w11, w7
    dc   isw, x11
    subs w7, w7, w17
    b.ge 3b
    subs x9, x9, x16
    b.ge 2b
4:  add  w10, w10, #2
    cmp  w3, w10
    b.gt 1b
    dsb  sy
    isb
5:  ret
.size invalidate_dcache, . - invalidate_dcache

/*
 * The identity map, with 1 GiB blocks:
 *
 *   0x00_0000_0000  1 GiB    Device   GIC, UART, fw_cfg, virtio, low PCIe MMIO
 *   0x00_4000_0000  3 GiB    Normal   RAM
 *   0x40_0000_0000  1 GiB    Device   PCIe ECAM
 *
 * Exclusive access to RAM needs Normal memory: with the MMU off, every access
 * is Device-nGnRnE, where load-exclusive is implementation-defined and the
 * first spin lock could hang on anything but QEMU's TCG.
 *
 * Clobbers x0-x3.
 */
.equ PT_UXN, (1 << 54)                /* a user program can't run this */
.equ PT_BLOCK_NORMAL, 0x705 + PT_UXN  /* AF, inner shareable, MAIR 1, block; EL1 only */
.equ PT_XN, (3 << 53)                 /* execute never, at every level */
.equ PT_BLOCK_DEVICE, 0x401 + PT_XN   /* AF, MAIR 0, block */

.equ RAM_BLOCK_FIRST, 1
.equ RAM_BLOCK_COUNT, 3
.equ ECAM_BLOCK, 256
.equ BLOCK_SIZE, 0x40000000

.equ MAIR_VALUE, 0xff00               /* 0: Device-nGnRnE, 1: Normal write-back */
/* T0SZ=25 (39-bit VA, starts at level 1), write-back walks, inner shareable,
 * 4 KiB granule, TTBR1 walks disabled, 40-bit physical addresses. */
.equ TCR_VALUE, 25 + (1 << 8) + (1 << 10) + (3 << 12) + (1 << 23) + (2 << 32)
.equ SCTLR_M, 1 << 0
.equ SCTLR_C, 1 << 2
.equ SCTLR_I, 1 << 12

.section .text.boot, "ax"
.type build_page_table, %function
build_page_table:
    adrp x0, page_table
    add  x0, x0, :lo12:page_table

    ldr  x1, =PT_BLOCK_DEVICE
    str  x1, [x0]

    ldr  x3, =PT_BLOCK_NORMAL
    mov  x1, #RAM_BLOCK_FIRST
1:  lsl  x2, x1, #30
    orr  x2, x2, x3
    str  x2, [x0, x1, lsl #3]
    add  x1, x1, #1
    cmp  x1, #RAM_BLOCK_FIRST + RAM_BLOCK_COUNT
    b.lo 1b

    ldr  x1, =(ECAM_BLOCK * BLOCK_SIZE + PT_BLOCK_DEVICE)
    str  x1, [x0, #ECAM_BLOCK * 8]
    dsb  ish
    ret
.size build_page_table, . - build_page_table

.type enable_mmu, %function
enable_mmu:
    ldr  x0, =MAIR_VALUE
    msr  mair_el1, x0
    ldr  x0, =TCR_VALUE
    msr  tcr_el1, x0
    adrp x0, page_table
    add  x0, x0, :lo12:page_table
    msr  ttbr0_el1, x0
    isb
    tlbi vmalle1
    ic   iallu
    dsb  nsh
    isb
    mrs  x0, sctlr_el1
    mov  x1, #(SCTLR_M | SCTLR_C | SCTLR_I)
    orr  x0, x0, x1
    msr  sctlr_el1, x0
    isb
    ret
.size enable_mmu, . - enable_mmu
.ltorg

.section .bss
.balign 4096
.global page_table
page_table:
    .skip 4096
.balign 16
secondary_stacks:
    .skip SMP_MAX_CPUS * SMP_STACK_SIZE
.balign 16
boot_frames:
    .skip SMP_MAX_CPUS * TRAP_FRAME_SIZE
.balign 16
.global irq_stacks
irq_stacks:
    .skip SMP_MAX_CPUS * IRQ_STACK_SIZE
