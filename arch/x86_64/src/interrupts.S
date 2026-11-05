/*
 * x86_64 interrupt entry stubs.
 *
 * Vectors 0..31 are CPU exceptions and the rest are interrupts: the local timer
 * on vector 34, the software switch on 48, IPIs on 49 and 50, and devices above
 * those. The IDT switches each one to another stack through the TSS, so none of
 * the frame is on the interrupted task's stack. For an interrupt, that "stack" is
 * the TrapFrame of whatever is running: the frame exactly fills it, so saving a
 * task needs no copying, and switching to another is a change of pointer. The
 * handler then runs on this CPU's interrupt stack. An exception's frame goes on
 * this CPU's fault stack instead, and its handler stays there.
 *
 * Each stub normalises the stack to:
 *
 *     vector
 *     error code
 *     rip, cs, rflags, rsp, ss
 *
 * Long mode pushes SS:RSP even for same-privilege interrupts. The kernel's
 * `TrapFrame` mirrors that exact layout. `interrupt_common` then saves the
 * general-purpose registers and the FXSAVE area.
 *
 * A trap from ring 3 takes the same path, into the process's user frame. All that
 * differs is a little on each side of it. The CPU's interrupt stack and index are at
 * the GS base, which a program can overwrite by loading a selector, so a program runs
 * with the kernel's in IA32_KERNEL_GS_BASE and `swapgs` brings it back. Flags a
 * program chose (AC, which turns SMAP off) and its floating point rounding and
 * exception masks don't follow it into the kernel.
 */

.section .text
.code64

.macro ISR_NOERR n
.global isr\n
isr\n:
    pushq $0
    pushq $\n
    jmp interrupt_common
.endm

.macro ISR_ERR n
.global isr\n
isr\n:
    pushq $\n
    jmp interrupt_common
.endm

ISR_NOERR 0
ISR_NOERR 1
ISR_NOERR 2
ISR_NOERR 3
ISR_NOERR 4
ISR_NOERR 5
ISR_NOERR 6
ISR_NOERR 7
ISR_ERR   8
ISR_NOERR 9
ISR_ERR   10
ISR_ERR   11
ISR_ERR   12
ISR_ERR   13
ISR_ERR   14
ISR_NOERR 15
ISR_NOERR 16
ISR_ERR   17
ISR_NOERR 18
ISR_NOERR 19
ISR_NOERR 20
ISR_ERR   21
ISR_NOERR 22
ISR_NOERR 23
ISR_NOERR 24
ISR_NOERR 25
ISR_NOERR 26
ISR_NOERR 27
ISR_NOERR 28
ISR_ERR   29
ISR_ERR   30
ISR_NOERR 31

/*
 * Vectors 32..=254: hardware interrupts, the software switch and IPIs, all alike.
 * Each stub has the same size, so the IDT computes the address of any of them.
 */
.balign 16
.global isr_irq_stubs
isr_irq_stubs:
.set vector, 32
.rept 223
.balign 16
    pushq $0
    pushq $vector
    jmp interrupt_common
.set vector, vector + 1
.endr
.balign 16

/* Any unexpected interrupt returns without touching the stack. */
.global isr_spurious
isr_spurious:
    iretq

interrupt_common:
    /* Rust assumes the direction flag is clear. */
    cld

    /* The stub's two words are on top of the CPU's frame: CS is at 24. */
    testb $3, 24(%rsp)
    jz 2f
    swapgs
    pushq $2
    popfq
2:

    /* Save GPRs. The order after the pushes is: r15, r14, ..., rax. */
    pushq %rax
    pushq %rbx
    pushq %rcx
    pushq %rdx
    pushq %rsi
    pushq %rdi
    pushq %rbp
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15

    /* The x87, MMX and SSE state, MXCSR included, occupy the first 512 bytes of
     * the TrapFrame. The kernel uses XMM registers for everything, so this comes
     * before any Rust runs. Every gate switches to a 16-byte aligned stack through
     * the TSS, so after the 5 CPU words, the 2 stub words and the 15 GPRs, RSP is
     * 16-byte aligned here, as FXSAVE needs. */
    subq $512, %rsp
    fxsave64 (%rsp)

    testb $3, 656(%rsp)
    jz 3f
    fninit
    ldmxcsr kernel_mxcsr(%rip)
3:
    /* An interrupt's handler moves to the CPU's interrupt stack, at gs:8 in its
     * CpuArea. Either stack is 16-byte aligned here, as the ABI asks of a call. */
    movq %rsp, %rdi
    cmpq $32, 632(%rsp)
    jb 1f
    movq %gs:8, %rsp
1:
    call x86_interrupt_dispatch

    /* It returns the frame to resume: this one, or another task's. The CPU frame
     * at its end is a complete long-mode IRET frame: RIP, CS, RFLAGS, RSP, SS. IRET
     * loads the task's own RSP from it. */
    movq %rax, %rsp
    fxrstor64 (%rsp)
    addq $512, %rsp
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rbp
    popq %rdi
    popq %rsi
    popq %rdx
    popq %rcx
    popq %rbx
    popq %rax
    /* Vector and error code. */
    addq $16, %rsp

    /* The frame going back to might not be the one that came in: CS is at 8. */
    testb $3, 8(%rsp)
    jz 4f
    swapgs
4:
    iretq

.section .rodata
kernel_mxcsr:
    .long 0x1f80
.balign 8
.global isr_stub_table
isr_stub_table:
    .quad isr0,  isr1,  isr2,  isr3,  isr4,  isr5,  isr6,  isr7
    .quad isr8,  isr9,  isr10, isr11, isr12, isr13, isr14, isr15
    .quad isr16, isr17, isr18, isr19, isr20, isr21, isr22, isr23
    .quad isr24, isr25, isr26, isr27, isr28, isr29, isr30, isr31

.section .note.GNU-stack, "", @progbits
