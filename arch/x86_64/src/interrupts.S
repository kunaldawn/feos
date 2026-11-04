/*
 * x86_64 interrupt entry stubs.
 *
 * Vectors 0..31 are CPU exceptions; vectors 32..47 are hardware IRQs (the
 * x86_64 port routes the PIT through vector 34). Each stub normalises the
 * stack to:
 *
 *     vector
 *     error code
 *     rip, cs, rflags, rsp, ss
 *
 * Long mode pushes SS:RSP even for same-privilege interrupts. The kernel's
 * `TrapFrame` mirrors that exact layout. `interrupt_common` then saves the
 * general-purpose and XMM registers.
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
ISR_NOERR 32
ISR_NOERR 33
ISR_NOERR 34
ISR_NOERR 35
ISR_NOERR 36
ISR_NOERR 37
ISR_NOERR 38
ISR_NOERR 39
ISR_NOERR 40
ISR_NOERR 41
ISR_NOERR 42
ISR_NOERR 43
ISR_NOERR 44
ISR_NOERR 45
ISR_NOERR 46
ISR_NOERR 47

/* Any unexpected interrupt returns without touching the stack. */
.global isr_spurious
isr_spurious:
    iretq

interrupt_common:
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

    /* XMM0..XMM15 occupy the first 256 bytes of the TrapFrame. `movups`
     * keeps this correct even when the interrupted stack is only 8-byte
     * aligned. */
    subq $256, %rsp
    movups %xmm0,   0(%rsp)
    movups %xmm1,  16(%rsp)
    movups %xmm2,  32(%rsp)
    movups %xmm3,  48(%rsp)
    movups %xmm4,  64(%rsp)
    movups %xmm5,  80(%rsp)
    movups %xmm6,  96(%rsp)
    movups %xmm7, 112(%rsp)
    movups %xmm8, 128(%rsp)
    movups %xmm9, 144(%rsp)
    movups %xmm10, 160(%rsp)
    movups %xmm11, 176(%rsp)
    movups %xmm12, 192(%rsp)
    movups %xmm13, 208(%rsp)
    movups %xmm14, 224(%rsp)
    movups %xmm15, 240(%rsp)

    /* Keep the frame pointer in a callee-saved register and give the Rust
     * helper a 16-byte aligned stack. */
    movq %rsp, %r15
    movq %rsp, %rdi
    andq $-16, %rsp
    call x86_interrupt_dispatch
    movq %r15, %rsp

    /* The CPU frame at frame_base + 392 is already a complete long-mode
     * IRET frame: RIP, CS, RFLAGS, RSP, SS. The scheduler may have replaced
     * it with another task's saved frame, but it is still in the required
     * layout. Point RSP at it and return directly; copying the frame to the
     * target task's stack would overlap the source when the interrupted RSP
     * was not 16-byte aligned (the CPU aligns the frame when pushing it). */
    movq %rsp, %r15
    leaq 392(%r15), %rsp

    movups   0(%r15), %xmm0
    movups  16(%r15), %xmm1
    movups  32(%r15), %xmm2
    movups  48(%r15), %xmm3
    movups  64(%r15), %xmm4
    movups  80(%r15), %xmm5
    movups  96(%r15), %xmm6
    movups 112(%r15), %xmm7
    movups 128(%r15), %xmm8
    movups 144(%r15), %xmm9
    movups 160(%r15), %xmm10
    movups 176(%r15), %xmm11
    movups 192(%r15), %xmm12
    movups 208(%r15), %xmm13
    movups 224(%r15), %xmm14
    movups 240(%r15), %xmm15

    movq 368(%r15), %rax
    movq 360(%r15), %rbx
    movq 352(%r15), %rcx
    movq 344(%r15), %rdx
    movq 336(%r15), %rsi
    movq 328(%r15), %rdi
    movq 320(%r15), %rbp
    movq 312(%r15), %r8
    movq 304(%r15), %r9
    movq 296(%r15), %r10
    movq 288(%r15), %r11
    movq 280(%r15), %r12
    movq 272(%r15), %r13
    movq 264(%r15), %r14
    movq 256(%r15), %r15
    iretq

.section .rodata
.balign 8
.global isr_stub_table
isr_stub_table:
    .quad isr0,  isr1,  isr2,  isr3,  isr4,  isr5,  isr6,  isr7
    .quad isr8,  isr9,  isr10, isr11, isr12, isr13, isr14, isr15
    .quad isr16, isr17, isr18, isr19, isr20, isr21, isr22, isr23
    .quad isr24, isr25, isr26, isr27, isr28, isr29, isr30, isr31
    .quad isr32, isr33, isr34, isr35, isr36, isr37, isr38, isr39
    .quad isr40, isr41, isr42, isr43, isr44, isr45, isr46, isr47

.section .note.GNU-stack, "", @progbits
