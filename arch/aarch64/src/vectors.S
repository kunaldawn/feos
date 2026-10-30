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
 * Synchronous faults and unexpected exceptions report the fault and halt.
 * IRQs taken from EL1 with SP_EL1 (slot 5, the only context the kernel runs
 * in) go through irq_entry, which saves a TrapFrame and lets the kernel
 * scheduler choose the next process.
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

.macro irq_ventry index
    .balign 128
    mov x0, #\index
    b   irq_entry
.endm

    ventry 0          /* current EL with SP0: synchronous */
    ventry 1          /* current EL with SP0: IRQ */
    ventry 2          /* current EL with SP0: FIQ */
    ventry 3          /* current EL with SP0: SError */

    ventry 4          /* current EL with SPx: synchronous */
    irq_ventry 5      /* current EL with SPx: IRQ */
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

/*
 * Recoverable IRQ entry.
 *
 * Save a TrapFrame (800 bytes, matching arch/aarch64/src/irq.rs):
 *   [   0, 248)  x0..x30
 *   [ 248, 256)  padding
 *   [ 256, 768)  q0..q31
 *   [ 768, 776)  interrupted SP
 *   [ 776, 784)  ELR_EL1
 *   [ 784, 792)  SPSR_EL1
 *   [ 792, 800)  padding
 *
 * irq_dispatch() may replace the frame in place with another process's saved
 * state. We then restore that state and eret into it.
 */
.type irq_entry, %function
irq_entry:
    sub sp, sp, #800

    stp x0, x1, [sp, #0]
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    str x30, [sp, #240]

    stp q0, q1, [sp, #256]
    stp q2, q3, [sp, #288]
    stp q4, q5, [sp, #320]
    stp q6, q7, [sp, #352]
    stp q8, q9, [sp, #384]
    stp q10, q11, [sp, #416]
    stp q12, q13, [sp, #448]
    stp q14, q15, [sp, #480]
    stp q16, q17, [sp, #512]
    stp q18, q19, [sp, #544]
    stp q20, q21, [sp, #576]
    stp q22, q23, [sp, #608]
    stp q24, q25, [sp, #640]
    stp q26, q27, [sp, #672]
    stp q28, q29, [sp, #704]
    stp q30, q31, [sp, #736]

    add x9, sp, #800
    str x9, [sp, #768]
    mrs x9, elr_el1
    str x9, [sp, #776]
    mrs x9, spsr_el1
    str x9, [sp, #784]

    mov x0, sp
    bl  irq_dispatch

    /* Restore from the (possibly replaced) frame. x9 is the base until the
     * very end, because x9 itself is one of the registers being restored. */
    mov x9, sp

    ldp q0, q1, [x9, #256]
    ldp q2, q3, [x9, #288]
    ldp q4, q5, [x9, #320]
    ldp q6, q7, [x9, #352]
    ldp q8, q9, [x9, #384]
    ldp q10, q11, [x9, #416]
    ldp q12, q13, [x9, #448]
    ldp q14, q15, [x9, #480]
    ldp q16, q17, [x9, #512]
    ldp q18, q19, [x9, #544]
    ldp q20, q21, [x9, #576]
    ldp q22, q23, [x9, #608]
    ldp q24, q25, [x9, #640]
    ldp q26, q27, [x9, #672]
    ldp q28, q29, [x9, #704]
    ldp q30, q31, [x9, #736]

    ldr x10, [x9, #768]
    ldr x11, [x9, #776]
    ldr x12, [x9, #784]
    msr elr_el1, x11
    msr spsr_el1, x12
    mov sp, x10

    ldp x0, x1, [x9, #0]
    ldp x2, x3, [x9, #16]
    ldp x4, x5, [x9, #32]
    ldp x6, x7, [x9, #48]
    ldr x8, [x9, #64]
    ldr x10, [x9, #80]
    ldp x11, x12, [x9, #88]
    ldp x13, x14, [x9, #104]
    ldp x15, x16, [x9, #120]
    ldp x17, x18, [x9, #136]
    ldp x19, x20, [x9, #152]
    ldp x21, x22, [x9, #168]
    ldp x23, x24, [x9, #184]
    ldp x25, x26, [x9, #200]
    ldp x27, x28, [x9, #216]
    ldr x29, [x9, #232]
    ldr x30, [x9, #240]
    ldr x9, [x9, #72]

    eret
.size irq_entry, . - irq_entry
