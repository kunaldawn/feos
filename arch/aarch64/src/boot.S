/*
 * AArch64 entry point.
 *
 * QEMU loads the ELF at 0x4008_0000 and starts us at EL1 with:
 *   x0 = physical address of the device tree blob
 *   MMU off, caches off, interrupts masked
 */

.section .text.boot, "ax"
.global _start
.type _start, %function
_start:
    /* Keep the device tree pointer for kmain. */
    mov x19, x0

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

    /* SP_EL1 is this CPU's private exception stack. The boot thread, like every
     * thread, runs on SP_EL0 (EL1t), so taking an exception switches stacks in
     * hardware. See vectors.S. */
    adrp x0, irq_stacks
    add  x0, x0, :lo12:irq_stacks
    mov  x1, #IRQ_STACK_SIZE
    add  x0, x0, x1
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
 * Give each one private stacks and the shared exception table, then hand it to
 * Rust. As on the boot CPU, SP_EL1 is the exception stack and threads use SP_EL0. The Rust handler installs its CPU interface and joins the
 * scheduler; it does not return.
 */
.equ SMP_MAX_CPUS, 8
.equ SMP_STACK_SIZE, 0x8000           /* 32 KiB until the first tick */
.equ IRQ_STACK_SIZE, 0x8000           /* 32 KiB per CPU */

.section .text.boot, "ax"
.global secondary_entry
.type secondary_entry, %function
secondary_entry:
    mov x19, x0
    msr tpidr_el1, x0

    /* sp = irq_stacks + (index + 1) * IRQ_STACK_SIZE */
    adrp x1, irq_stacks
    add  x1, x1, :lo12:irq_stacks
    mov  x2, #IRQ_STACK_SIZE
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

.section .bss
.balign 16
secondary_stacks:
    .skip SMP_MAX_CPUS * SMP_STACK_SIZE
.balign 16
irq_stacks:
    .skip SMP_MAX_CPUS * IRQ_STACK_SIZE
