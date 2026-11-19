# Kernel issues found by the toolchain track

Kernel bugs and missing kernel behaviour the toolchain port runs into. The
toolchain track doesn't change the kernel (another workflow owns `kernel/`,
`libs/`, `user/` and the rest): each is recorded here, with the call, the
symptom and a repro, and the toolchain side works around it or marks its test
pending.

| Found in | Call | Symptom | Repro | Workaround |
|---|---|---|---|---|
| T1 (base `refs/feos-checkpoints/1c.6`) | `THREAD_CREATE` (34) | Not a bug, a design gap: `stack_top` must be 16-byte aligned, and the design has relibc's trampoline find its start words at the stack pointer. On x86_64 relibc pushes seven words (56 bytes) below a page-aligned top, so the words' address isn't 16-byte aligned and can't be the stack top. | `pthread_create` with relibc's unmodified start-word protocol on x86_64 | relibc passes the words' address as the thread's argument and a 16-byte aligned top below them; the trampoline moves the stack pointer to the words before popping them (`docs/toolchain.md`, deviations). The kernel needs no change. |
| T1 (base 1c.6) | console, standard streams | Behaviour to know, not a bug: feos-sh gives a foreground program a pipe each for fds 0, 1 and 2 and copies them to the console, so a program's stdout and stderr lines can reach the console in another order than written (seen on AArch64), and `isatty` is 0 on all three. | `/bin/t1-hello` (writes stderr between two stdout lines) on arm | Tests check stderr apart from stdout. Terminal marking of console fds is stage 1c.11; `/bin/sh` as a program, 1c.12. |
| T1 review (base 1c.6) | `RECVFROM` (100), `recvmsg` | A doc inconsistency, not a kernel bug: `docs/feos-abi-v2.md`'s 10.x `recvmsg` row says `msg_flags` is "0 or `MSG_TRUNC`", but section 3.7's `RECVFROM` returns at most the buffer's length, drops the rest of a longer datagram and takes no `MSG_TRUNC` flag (`EINVAL`), so libc can't tell a truncated datagram from one that fit. | `recvmsg` of a 100-byte UDP datagram into a 10-byte iovec (once 1e.3 is in) | relibc sets `msg_flags` to 0 and documents that `MSG_TRUNC` isn't reported. Reporting it needs `RECVFROM` to accept `MSG_TRUNC` and return the datagram's full length, as Linux does; then relibc can pass it and compare. |
