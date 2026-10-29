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
     * Only the boot CPU continues. Secondary cores are parked; this kernel is
     * single-core for now (QEMU is started with -smp 1 anyway).
     */
    mrs x0, mpidr_el1
    and x0, x0, #0xff
    cbnz x0, .Lpark

    /* Install the stack from the linker script. */
    adrp x0, __stack_top
    add  x0, x0, :lo12:__stack_top
    mov  sp, x0

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
