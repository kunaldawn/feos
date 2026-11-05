/*
 * AArch64 exception vector table.
 *
 * VBAR_EL1 points at a 2 KiB aligned table with 16 entries of 128 bytes:
 * four groups of four (synchronous, IRQ, FIQ, SError):
 *   0-3   current EL, SP_EL0   the kernel's threads run here (EL1t)
 *   4-7   current EL, SP_ELx   the exception handlers themselves (EL1h)
 *   8-11  lower EL, AArch64      user programs (EL0t)
 *   12-15 lower EL, AArch32
 *
 * Every entry switches to SP_EL1 in hardware. While a thread runs, SP_EL1 points
 * just past that thread's TrapFrame, so the frame "pushed" here lands right in it:
 * saving a thread needs no copying, and switching to another is a change of
 * pointer. The handler then runs on this CPU's private exception stack. A task's
 * stack therefore holds none of its saved state once it has been offered to another
 * CPU, and a task whose stack pointer is bad can still fault cleanly.
 *
 * A trap from a user program is no different. SP_EL1 is just past the frame the
 * scheduler resumed, here the process's user frame, and the program's own stack
 * pointer is in SP_EL0. So nothing a program does to its registers reaches kernel
 * memory: it can't choose where the trap saves.
 */

.section .text.vectors, "ax"
.balign 2048
.global exception_vector_table
exception_vector_table:
.macro ventry index
    .balign 128
    /* x0 and x1 are saved before the vector index overwrites x0. */
    sub sp, sp, #800
    stp x0, x1, [sp, #0]
    mov x0, #\index
    b   trap_common
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

/*
 * Common trap entry. x0 holds the vector index; x0 and x1 are already saved.
 *
 * Save a TrapFrame (800 bytes, matching arch/aarch64/src/irq.rs):
 *   [   0, 248)  x0..x30
 *   [ 248, 256)  FPCR
 *   [ 256, 768)  q0..q31
 *   [ 768, 776)  interrupted SP
 *   [ 776, 784)  ELR_EL1
 *   [ 784, 792)  SPSR_EL1
 *   [ 792, 800)  FPSR
 *
 * trap_dispatch() returns the frame to resume: this one, or another thread's. We
 * restore it and eret, which leaves SP_EL1 just past that frame, ready for the next
 * trap. It only returns for traps taken from a thread or a program, both on SP_EL0, so
 * the restore targets SP_EL0, and SPSR_EL1 in the frame says which level to go to. A
 * trap from the handlers themselves (groups 4-7) is already on the exception stack,
 * and doesn't come back.
 */
.type trap_common, %function
trap_common:
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

    /* Groups 4-7 interrupted this stack; the others interrupted SP_EL0. */
    mrs x9, sp_el0
    add x10, sp, #800
    sub x11, x0, #4
    cmp x11, #4
    csel x9, x10, x9, lo
    str x9, [sp, #768]

    mrs x9, elr_el1
    str x9, [sp, #776]
    mrs x9, spsr_el1
    str x9, [sp, #784]
    mrs x9, fpcr
    str x9, [sp, #248]
    mrs x9, fpsr
    str x9, [sp, #792]

    /* A program chooses its own rounding mode and traps; the kernel's floating
     * point mustn't run with them. */
    cmp x0, #8
    b.lo 0f
    msr fpcr, xzr
0:
    mov x1, sp
    /* Groups 4-7 are already on the exception stack. */
    sub x11, x0, #4
    cmp x11, #4
    b.lo 1f
    /* sp = irq_stacks + (index + 1) * IRQ_STACK_SIZE */
    mrs x9, tpidr_el1
    adrp x10, irq_stacks
    add  x10, x10, :lo12:irq_stacks
    mov  x11, #0x8000
    madd x10, x9, x11, x10
    add  x10, x10, x11
    mov  sp, x10
1:
    bl  trap_dispatch
    mov sp, x0

    ldr x9, [sp, #768]
    msr sp_el0, x9
    ldr x9, [sp, #776]
    msr elr_el1, x9
    ldr x9, [sp, #784]
    msr spsr_el1, x9
    ldr x9, [sp, #248]
    msr fpcr, x9
    ldr x9, [sp, #792]
    msr fpsr, x9

    ldp q0, q1, [sp, #256]
    ldp q2, q3, [sp, #288]
    ldp q4, q5, [sp, #320]
    ldp q6, q7, [sp, #352]
    ldp q8, q9, [sp, #384]
    ldp q10, q11, [sp, #416]
    ldp q12, q13, [sp, #448]
    ldp q14, q15, [sp, #480]
    ldp q16, q17, [sp, #512]
    ldp q18, q19, [sp, #544]
    ldp q20, q21, [sp, #576]
    ldp q22, q23, [sp, #608]
    ldp q24, q25, [sp, #640]
    ldp q26, q27, [sp, #672]
    ldp q28, q29, [sp, #704]
    ldp q30, q31, [sp, #736]

    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldp x20, x21, [sp, #160]
    ldp x22, x23, [sp, #176]
    ldp x24, x25, [sp, #192]
    ldp x26, x27, [sp, #208]
    ldp x28, x29, [sp, #224]
    ldr x30, [sp, #240]
    ldp x0, x1, [sp, #0]
    add sp, sp, #800

    eret
.size trap_common, . - trap_common

/*
 * Vectors used until the kernel installs the table above. Nothing can recover
 * this early, and the likely cause is the image not fitting in RAM.
 */
.balign 2048
.global early_vector_table
early_vector_table:
.rept 16
    .balign 128
    b   early_fault
.endr

.type early_fault, %function
early_fault:
    adrp x0, early_fault_message
    add  x0, x0, :lo12:early_fault_message
    b    early_print
.size early_fault, . - early_fault

/* Writes the NUL-terminated string at x0 to the board's UART, then parks. */
.global early_print
.type early_print, %function
early_print:
    ldr  x1, =__early_uart
1:  ldrb w2, [x0], #1
    cbz  w2, 2f
    str  w2, [x1]
    b    1b
2:  msr  daifset, #0xf
3:  wfe
    b    3b
.size early_print, . - early_print
.ltorg

.section .rodata
early_fault_message:
    .asciz "\r\nfeos: fault before the kernel started; does the machine have enough RAM for the image?\r\n"
