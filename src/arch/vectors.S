/*
 * AArch64 exception vector table.
 *
 * VBAR_EL1 points at a 2 KiB aligned table with 16 entries of 128 bytes:
 * four groups of four (synchronous, IRQ, FIQ, SError):
 *   0-3   current EL, SP_EL0
 *   4-7   current EL, SP_ELx
 *   8-11  lower EL, AArch64
 *   12-15 lower EL, AArch32
 *
 * This skeleton has no interrupt controller yet, so every entry reports the
 * fault and halts instead of trying to recover.
 */

.section .text.vectors, "ax"
.balign 2048
.global exception_vector_table
exception_vector_table:
.macro ventry index
    .balign 128
    mov x0, #\index
    b   exception_common
.endm

    ventry 0          /* current EL with SP0: synchronous */
    ventry 1          /* current EL with SP0: IRQ */
    ventry 2          /* current EL with SP0: FIQ */
    ventry 3          /* current EL with SP0: SError */

    ventry 4          /* current EL with SPx: synchronous */
    ventry 5          /* current EL with SPx: IRQ */
    ventry 6          /* current EL with SPx: FIQ */
    ventry 7          /* current EL with SPx: SError */

    ventry 8          /* lower EL, AArch64: synchronous */
    ventry 9          /* lower EL, AArch64: IRQ */
    ventry 10         /* lower EL, AArch64: FIQ */
    ventry 11         /* lower EL, AArch64: SError */

    ventry 12         /* lower EL, AArch32: synchronous */
    ventry 13         /* lower EL, AArch32: IRQ */
    ventry 14         /* lower EL, AArch32: FIQ */
    ventry 15         /* lower EL, AArch32: SError */
.size exception_vector_table, . - exception_vector_table

.type exception_common, %function
exception_common:
    /* Arguments for exception_handler(): index, ESR, ELR, FAR, SPSR, SP. */
    mrs x1, esr_el1
    mrs x2, elr_el1
    mrs x3, far_el1
    mrs x4, spsr_el1
    mov x5, sp
    bl  exception_handler

.Lhang:
    wfe
    b .Lhang
.size exception_common, . - exception_common
