# feos-abi v2: the kernel interface for a POSIX userland

This is the design of everything phase 1 of `docs/selfhost-plan.md` adds to `feos-abi`
(`libs/abi`): sections 1a to 1f. It's meant to be complete enough that relibc's
`platform/feos` backend, relibc's `ld_so` and Rust std for `target_os = "feos"` can be
written against it with nothing left to invent. The last part is the ordered list of
implementation stages and the tests that prove each one on both ports.

It was written against:

- the kernel as of stage 1a.1 (`libs/abi` numbers 0..=30, `kernel/src/user.rs`,
  `kernel/src/sched.rs`, `libs/sched`, `libs/vm`, `libs/elf`, `libs/fs`, `libs/net`);
- relibc `master` at `50c054e2`, the `ports/src/relibc` submodule: the `Pal`,
  `PalSignal`, `PalSocket`, `PalEpoll` and `PalPtrace` traits in `src/platform/pal/`,
  the Linux and Redox backends, `src/ld_so`, `src/pthread`, and the headers that reach
  the kernel without going through `Pal` (`ioctl`, `sysconf`, `select`, `netdb`);
- rust-lang/libc `main` at `42bea6ee`. The `ports/src/libc` submodule is pinned to
  `0.2.189` instead, the version rust 1.99.1's std locks, and the feos module goes on
  that line; the `src/unix` layout this doc relies on is the same in both.

Contents:

1. [Rules every call follows](#1-rules-every-call-follows)
2. [What the kernel keeps, in v2](#2-what-the-kernel-keeps-in-v2)
3. [The calls](#3-the-calls) (numbers 31 to 108)
4. [Structures](#4-structures)
5. [Constants](#5-constants)
6. [Address-space layout](#6-address-space-layout)
7. [Starting a program](#7-starting-a-program)
8. [Signals in detail](#8-signals-in-detail)
9. [relibc's `Pal`, function by function](#9-relibcs-pal-function-by-function)
10. [Beyond `Pal`: relibc, std and the tools](#10-beyond-pal-relibc-std-and-the-tools)
11. [Changes to existing calls](#11-changes-to-existing-calls)
12. [Implementation stages](#12-implementation-stages)
13. [Risks and open points](#13-risks-and-open-points)

## 1. Rules every call follows

**Calling convention.** Unchanged from v1: `svc #0` with the number in `x8` and arguments
in `x0`..`x5` on AArch64; `int $0xf0` with the number in `rax` and arguments in `rdi`,
`rsi`, `rdx`, `r10`, `r8`, `r9` on x86_64. One result register; `-4095..0` is a negated
error. No call takes more than six arguments: anything bigger is a `repr(C)` struct passed
by pointer.

What a call preserves: every general register except the result register (`x0`, `rax`),
the stack pointer, the TLS register, and all FP/SIMD state (`v0`..`v31`, `FPCR`, `FPSR`;
`xmm0`..`xmm15`, `MXCSR`, the x87 state). The condition flags (`NZCV`; the arithmetic
bits of `RFLAGS`) are *not* promised to survive, so a caller's inline assembly must not
say `preserves_flags`. Two exceptions, both in [section 8](#8-signals-in-detail): a call a
handler interrupted with `SA_RESTART` comes back with its arguments as they were at entry,
and `SIGRETURN` sets every register from the context.

**What user mode may do without a call**, fixed so relibc's and compiler-rt's assembly can
rely on it (stage 1b.2 sets it; x86_64 is already this way, and before 1b.2 AArch64 traps
`CTR_EL0`, `dc zva` and `CNTVCT_EL0` too):

| | Allowed at EL0 / ring 3 | Trapping (`SIGILL`, or `SIGSEGV` for x86 `#GP`) |
|---|---|---|
| AArch64 | `mrs CTR_EL0` (`SCTLR_EL1.UCT`), `dc zva` (`SCTLR_EL1.DZE`; `DCZID_EL0` readable), `mrs CNTVCT_EL0` and `CNTFRQ_EL0` (`CNTKCTL_EL1.EL0VCTEN`), `msr`/`mrs TPIDR_EL0`, `TPIDRRO_EL0` read (always 0) | `dc cvau`, `dc civac`, `ic ivau` (`SCTLR_EL1.UCI` clear: nothing generates code, so there's no instruction cache to maintain), the physical counter and every timer register, `msr daif`, `wfi` |
| x86_64 | `rdtsc`, `rdtscp` (`CR4.TSD` clear), `cpuid` (no faulting), `rdrand`/`rdseed` where the CPU has them, `pause`, `lfence`/`mfence`/`sfence` | `sgdt`, `sidt`, `sldt`, `smsw`, `str` (`CR4.UMIP` where the CPU has it), `rdpmc`, `wrfsbase` and friends (`CR4.FSGSBASE` clear), `cli`/`sti`/`in`/`out` (IOPL 0) |

**Numbers.** New calls take the numbers 31 to 108 below, grouped by plan section. A number
is never reused, and a call that turns out wrong gets a new number, not a new meaning.
Every number from 31 to 108 is reserved from stage 1a.3 on and returns `ENOSYS` until the
stage that implements it lands, as any unknown number does today; the `malicious` probe
`unimplemented-numbers-enosys` checks the not-yet-implemented ones and is updated by each
stage.

**Names.** A call that has a Linux counterpart takes the Linux name (`PIPE2`, `DUP3`,
`WAIT4`, `OPENAT`, `NANOSLEEP`), even when it replaces a v1 call with another name (`OPEN`,
`SLEEP`). A feos revision of a feos call or struct with no Linux counterpart takes a `2`
suffix (`MMAP2`, `SPAWN2`, `FUTEX_WAIT2`, `Stat2`). Each v1 `nr::` constant whose call is
superseded gets a "superseded by" line in its doc comment in the stage that adds the
replacement.

**Compatibility.** A call may come to accept arguments it used to refuse (a new flag bit, a
range it used to call `EINVAL`), but it never changes what it does with arguments it
already accepted. The handful of v1 behaviours this design does change, before any binary
outside the tree depends on them, are listed one by one in [section 11](#11-changes-to-existing-calls).

**Version and features.** `feos_abi::ABI_VERSION` becomes `2` in stage 1a.6, the stage
that changes the layout constants; until then it stays `1`, whatever calls have landed.
Because calls arrive over twenty-odd stages, the version alone can't tell relibc whether a
kernel is complete, so the kernel also reports a **feature mask**: bit `n` is set once
stage `n` of the table in [section 12](#12-implementation-stages) is in. [`SYSINFO`](#81-sysinfo)
reports both (`abi_version`, `features`), and so do the aux vector entries `AT_FEOS_ABI`
and `AT_FEOS_FEATURES`. relibc's `Pal::verify` checks `abi_version >= 2` and that every
feature bit it was built against is set. Bits are never reused or cleared.

**Arguments are validated, every one, before anything changes:**

| Kind | Rule | Error |
|---|---|---|
| 32-bit typed argument | a register argument whose C type is 32 bits (`int`, `pid_t`, `uid_t`, `gid_t`, `mode_t`, a `c_int` flag word, an fd) is its low 32 bits; the upper 32 bits must be all zero or the sign extension of bit 31, so a C caller's `-1` and a Rust caller's `u32::MAX` both arrive as `-1` | `EINVAL` (`EBADF` for an fd) |
| File descriptor | `0 <= fd < MAX_FDS` and open | `EBADF` |
| Directory FD (`dirfd`) | as a descriptor, or `AT_FDCWD` (`-100`); must be a directory unless the path is absolute | `EBADF`, `ENOTDIR` |
| Path | `(pointer, length)`, not NUL-terminated, as in v1; `1 <= length <= PATH_MAX`; no NUL byte inside; UTF-8 | `ENOENT` for empty (unless `AT_EMPTY_PATH`), `ENAMETOOLONG`, `EINVAL` for a NUL, `EILSEQ` for bytes that aren't UTF-8 |
| Pointer to a struct or buffer | the whole range in the user window and mapped with the access needed, through `libs/vm` (fault in, then validate, then copy) | `EFAULT` |
| Flags | every bit known for this call; unknown bits refused | `EINVAL` |
| Counts and sizes | the limits named in each call | `EINVAL`, `E2BIG`, `ENAMETOOLONG` |
| Signal number | `1..=64` (`0` only where a call says so) | `EINVAL` |
| Reserved fields of a request struct | zero | `EINVAL` |
| Padding of a Linux-layout struct | ignored on input, written as zero on output | |

The 32-bit typed arguments, call by call: every `fd` and `dirfd`; `mode` of `OPENAT`,
`MKDIRAT`, `FCHMODAT` and `UMASK`; `uid` and `gid` of `FCHOWNAT`; `pid` of `WAIT4`,
`KILL`, `GETPGID`, `SETPGID` (both arguments) and `GETSID`; `tid` of `THREAD_KILL`;
`expected` of `FUTEX_WAIT2` (a `u32` word); `signal` everywhere; `who` of `GETRUSAGE`;
`how` of `SIGPROCMASK` and `SHUTDOWN`; `domain`, `kind` and `protocol` of `SOCKET` and
`SOCKETPAIR`; `level` and `name` of the socket options; `backlog`; `operation` of `FLOCK`;
`command` of `FCNTL`, and its `arg` for every command but the lock commands (a pointer).
Everything else is a full 64-bit value (`usize`, or `isize`/`i64` where a call says so).

Non-UTF-8 names can't be created or looked up: a `gix` checkout or `tar` extraction of
such a name fails with `EILSEQ` for that entry. Nothing in the trees this plan builds has
one.

**Request structs** are the ones feos defines for a call's input and libc fills in fresh:
[`PathAt`](#pathat), [`SpawnRequest`](#spawnrequest), [`SpawnAction`](#spawnaction),
[`ExecRequest`](#execrequest), [`ITimerVal`](#itimerval).
Their reserved fields must be zero. **Linux-layout structs** copy a C layout whose
padding libc never zeroes: [`Termios`](#termios), [`SigStack`](#sigstack), [`Flock`](#flock),
[`SigAction`](#sigaction) (the padding after a C `sa_flags`), [`SockAddrIn`](#sockaddrin)
(`sin_zero`), [`PollFd`](#pollfd), [`Timespec`](#timespec);
their padding is ignored on input and zero on output. [Section 4](#4-structures) marks
each struct's kind.

**Each user struct is read exactly once.** Another thread of the process may change user
memory at any moment, so every struct and array a call takes (requests, action lists,
string tables, `PollFd` arrays, `Timespec`s, `Flock`s, `SigAction`s, the `UContext` in
`SIGRETURN`, the ioctl structs) is copied into kernel memory once, and validation and use
both read only that copy. The kernel never reads back what it wrote to user memory,
signal frames included. The `malicious` probe `toctou-spawn-lengths` (a thread flipping a
`UserStr` length during `SPAWN2`: `E2BIG`/`EFAULT`, never a kernel fault) checks it, with a
probe of the same shape for each later struct-taking stage.

The kernel never dereferences a user address: every copy walks the tables in `libs/vm`
and goes through the frame's identity mapping. With threads (stage 1b.1) a copy also pins
each frame it uses, so a `munmap` in another thread can't free a frame a copy is using.

**Error numbers are Linux's** (v1's already were), so relibc's `errno` values need no
translation. New ones are in [section 5.1](#51-error-numbers).

**Constant values are the same on both ports**, and are relibc's Linux values (which are the
x86_64 Linux values: relibc doesn't use arm64 Linux's different `O_DIRECTORY` and
`O_NOFOLLOW`). The libc crate's `feos` module uses the same values on both
architectures.

**Structures are `repr(C)`, made only of fixed-size integers and arrays, with explicit
padding fields, and laid out identically on both ports.** `libs/abi` asserts each one's
size and every field's offset with `const _: () = assert!(...)`, as the arch crates do for
`TrapFrame`, so a mistake fails to build. The two exceptions are the per-port signal
contexts in [section 8](#8-signals-in-detail), which hold registers.

**What isn't Linux, deliberately.** Each case is named where it comes up, with why it's
safe for the programs this plan needs. The full list:

- No Linux numbers, no `clone` flags, no `ioctl` request numbers taken from Linux, no
  `prctl`, no `arch_prctl`; paths are `(pointer, length)` ([above](#1-rules-every-call-follows)).
- `/proc` is only `/proc/self/exe` and `/proc/self/fd/` ([`/dev` and `/proc`](#dev-and-procself)).
- Anonymous memory is never executable and nothing is ever writable and executable;
  `mmap(PROT_EXEC)` of a file open for writing, and writable opens of a file mapped
  executable, are `ETXTBSY` ([W^X](#31-mmap2)).
- `pwrite` on an `O_APPEND` fd writes at the offset ([`PWRITE`](#57-pread-58-pwrite)).
- dirfds and the cwd are remembered canonical paths ([3.3](#33-files-and-directories-1c)).
- A `CLOCK_REALTIME` futex deadline doesn't move with `CLOCK_SETTIME` ([`FUTEX_WAIT2`](#38-futex_wait2)).
- Real-time signals don't queue ([8.2](#82-generation)).
- No PTYs, no controlling-terminal acquisition ([`IOCTL`](#70-ioctl)).
- `KILL` with a thread id that isn't a process id is `ESRCH` ([`KILL`](#82-kill-83-thread_kill)).
- `epoll`, `io_uring`, `ptrace`, POSIX timers, `sigtimedwait`, `mremap`, named FIFOs and
  `AF_UNIX` names aren't offered ([section 9](#9-relibcs-pal-function-by-function)).

## 2. What the kernel keeps, in v2

The calls rest on kernel objects that don't exist yet. None of these are ABI, but every
call below is specified in their terms, so they're listed here once.

| Object | Owned by | Holds | Replaces |
|---|---|---|---|
| **Frame metadata** | `libs/frames` (a table beside the bitmap, 4 bytes per frame) | a 20-bit map count (page-table entries pointing at the frame), a 9-bit pin count (copies in flight), and flags: owner kind (anonymous or page cache), copy-on-write, instruction cache synced. A map or fork that would saturate the map count fails with `ENOMEM`; a pin at the cap fails the copy with `EAGAIN`. The page cache's own reference is the owner kind, never a map count | nothing: frames have no counts today |
| **Mm** (address space) | refcounted (`Arc<Mm>`), one reference per thread of the process using it, plus one while a fork or exec is building it | the page tables, the VMA list, the unbacked (promised) count, the ASID, the set of CPUs that may hold its TLB entries, behind one sleeping mutex | `Process::space` |
| **Process** | refcounted, one reference per thread plus one per child's exit record pointing at it by pid | its `Arc<Mm>`, the FD table, cwd, umask, signal dispositions, process-pending signals, pid (the thread-group id), parent pid, process group, session, uid and gid, the exe path, exit status once decided, its children's preallocated exit records, CPU time of exited threads and of reaped children, a per-process kernel-heap budget, and a mutex serialising `FORK`, `EXEC` and `THREAD_CREATE` | `PROCESSES[slot]` and `FILES[slot]` in `kernel/src/user.rs` |
| **Thread** | the scheduler slot | `tgid` (the process's pid) in `libs/sched::Task`; the user frame, TLS register value, signal mask and pending thread signals (per-slot atomics), alternate signal stack, clear-word address, the original first argument of a restartable call, the in-call count | the slot's `IN_USER`/`USER_FRAMES`, plus new per-slot statics |
| **VMA** | the Mm (`libs/vm`) | a range, protection, `private`/`shared`, anonymous, shared anonymous (a reference to a shared-anonymous object) or a file (node identity and offset), whether it's charged. Reservations and `PROT_NONE` ranges exist only here: no page-table entries until a page is committed | the single `mmap_next` bump pointer, and v1's soft PTEs |
| **Shared-anonymous object** | refcounted, by every VMA mapping it (across fork) | an offset-to-frame table filled under its own lock, so a page first touched in either process is the same frame in both | nothing |
| **Page cache** | the kernel, policy in a new host-tested `libs/pagecache` | frames of file pages, keyed by `(node identity, page index)`, with a dirty bit per page, an LRU of unmapped clean pages, and per file a bounded list of the VMAs mapping it (the reverse map truncation and write-back use) | nothing: files are copied in today |
| **Node identity** | the VFS | `(dev, ino)`, where `ino` is stable for a file's whole life and never reused while any kernel reference (OFD, VMA, cache page, lock) holds it, across rename too. The RAM, FAT and exFAT mounts take `ino` from the VFS node id, so the VFS keeps that id across rename and doesn't recycle it while referenced; the native format's inode numbers already are | `Stat::ino`, "unique on its filesystem" |
| **Open file description (OFD)** | refcounted, shared by `dup`, `fork` and inheritance | what's open (a VFS file handle and its canonical path, a directory, a pipe end, a socket, the console, a `/dev` node, an `O_PATH` reference), offset, status flags (`O_APPEND`, `O_NONBLOCK`), flock state, directory listing snapshot | `Fd`/`OpenFile` (a path and an offset in a 16-entry array) |
| **FD table** | the Process | a growable array of `(Option<OFD>, cloexec)`, up to `MAX_FDS` | `Files::fds` |
| **Lock table** | the kernel, keyed by node identity | `flock` holders, `fcntl` byte-range locks, and per file the hold count (OFDs + VMAs + page-cache pins, for orphaning), the writable-open count (writable OFDs and writable shared mappings) and the executable-mapping count (for `ETXTBSY`) | nothing |
| **Exit record** | the parent Process, allocated when the child is created | pid, status (exit, signal, stopped, continued), resource use | v1's 64-entry ring, for user parents |

Faults from user mode stop being resolved in the trap handler with interrupts masked.
From stage 1a.2 a user page fault is turned into a kernel thread on the task's own kernel
stack (the way `on_syscall` already turns a system call into one), which can block on a
lock or a disk read for a file page, and then returns to the program, or delivers a
signal. That's also the single place signal frames are built.

### 2.1 Kernel rules the calls depend on

These aren't ABI either, but each one is what makes a call below safe with threads, and
each is introduced by the stage named.

- **A user thread is never reaped in kernel mode** (1a.2). Today `Table::reapable` reaps a
  task at any point with `LOCK_DEPTH == 0`, abandoning its kernel stack; that's only safe
  while everything a process owns sits in per-slot statics that the exit sweep frees. v2
  holds `Arc`s, pins, cache references and heap buffers on the call's stack, so each user
  slot gets an **in-call count** (separate from `LOCK_DEPTH`, so fault containment keeps
  working) that `TaskFacts` reports, and `reapable` is false for a user slot unless it's
  `IN_USER`. A kill sets `kill_pending` and wakes the thread; every blocking primitive a
  call can reach (pipe, futex, sleep, lock table, `WAIT4`, `POLL`, sockets, the console)
  returns `Killed` when it's set; the call unwinds normally to `syscall_entry`, which ends
  the thread there. Waits that can't be interrupted (a disk transfer) are bounded by the
  driver's timeout. Kernel apps keep today's rule.
- **Stops happen at the user boundary** (1a.2). A stop request on a user thread (the
  shell's job control today, `SIGSTOP` from 1c.8) is pending until the thread is about to
  resume its user frame, so a thread can't be frozen holding the VFS mutex or any other
  kernel lock. `Table::set_stopped` stays immediate for kernel apps only.
- **Teardown runs in task context** (1a.2). `collect_exits` only moves a dead thread's
  `Process` and `Mm` references into a fixed-size lock-free queue; the queue is drained,
  with interrupts on, by the exiting thread itself before its final `Request::Exit` when it
  can, and otherwise by a reaper kernel task. `reschedule` no longer calls `finish`, so no
  OFD close, cache release or frame freeing runs on an exception stack with interrupts
  masked.
- **A fault from user mode while the slot isn't `IN_USER` stops the machine** (1a.2):
  the fault entry builds its frame at the top of the kernel stack, which is only free while
  the task is at EL0/ring 3. Signal frames are built from `USER_FRAMES[slot]`, never from
  live registers.
- **Lock order: Mm, then page cache, then VFS** (1a.4), never the reverse. The fault path
  looks up the VMA under the Mm lock, drops it, reads through the cache, retakes it,
  re-checks the entry and installs. A call that copies to or from user memory faults in
  and pins its buffer, or bounces it through a bounded kernel buffer, before it takes any
  file or pipe lock; a `PIPE_BUF` write bounces its at most 4 KiB so it can be atomic.
  `SCHEDULER` is innermost: nothing takes the Mm, a process's signal state or any sleeping
  lock while holding it.
- **No pins across an indefinite wait** (1b.1). A call that waits for data (pipe, socket,
  console) waits first, then pins and copies; it never blocks with user frames pinned,
  except for the bounded disk transfers above. At most one `IO_CHUNK` is pinned per thread.
- **Promises are taken before frames, never checked then acted on** (1a.3). Every frame
  allocation for user memory goes `promise(n)`, allocate, then `release` what wasn't used;
  `room_for` followed by a map is replaced. `release` is checked: an underflow stops the
  machine in debug builds and is clamped and logged in release builds.
- **A fault or panic in one thread's call ends its whole process** (1b.2), with status 139
  (`SIGSEGV`): the process is marked failed and its other threads are killed through the
  cooperative path, since their FD table or VMAs may be half updated. Leaks are counted as
  today.
- **User-originated kernel memory is bounded** (1a.3 onward, each stage for what it adds):
  a per-process cap on VMAs (`MAX_VMAS` 16384, `ENOMEM`, like `vm.max_map_count`), a
  per-process kernel-heap budget charged by user-originated allocations (`try_reserve`,
  `ENOMEM`), per-process caps on threads (`EAGAIN`), unreaped children (`EAGAIN`), record
  locks (`ENOLCK`) and pipes (`ENFILE`), and 64 task slots kept for kernel apps, so a
  thread or fork bomb can't stop the shell from running `kill`.

## 3. The calls

Signatures are written Rust-style with the register types the kernel receives. `-> !`
means the call doesn't return on success. Every call can also fail with `EFAULT` for a bad
pointer and `EINVAL` for unknown flags; those are only repeated where there's more to say.

| # | Call | Stage | | # | Call | Stage |
|---|---|---|---|---|---|---|
| 31 | `MMAP2` | 1a.3, 1a.4, 1a.5 | | 70 | `IOCTL` | 1c.11 |
| 32 | `MADVISE` | 1a.3 | | 71 | `POLL` | 1c.11 |
| 33 | `MSYNC` | 1a.3, 1a.5 | | 72 | `SPAWN2` | 1c.6 |
| 34 | `THREAD_CREATE` | 1b.2 | | 73 | `EXEC` | 1c.7 |
| 35 | `THREAD_EXIT` | 1b.2 | | 74 | `WAIT4` | 1c.8 |
| 36 | `GETTID` | 1b.2 | | 75 | `GETPPID` | 1c.6 |
| 37 | `SET_TLS` | 1b.2 | | 76 | `GETPGID` | 1c.6 |
| 38 | `FUTEX_WAIT2` | 1b.3 | | 77 | `SETPGID` | 1c.6 |
| 39 | `CLOCK_GETTIME` | 1b.3 | | 78 | `GETSID` | 1c.6 |
| 40 | `CLOCK_GETRES` | 1b.3 | | 79 | `SETSID` | 1c.6 |
| 41 | `NANOSLEEP` | 1b.3 | | 80 | `GETRUSAGE` | 1c.6, 1c.8 |
| 42 | `OPENAT` | 1c.2 | | 81 | `SYSINFO` | 1c.6 |
| 43 | `FSTATAT` | 1c.2 | | 82 | `KILL` | 1c.8 |
| 44 | `GETDENTS` | 1c.2 | | 83 | `THREAD_KILL` | 1c.10 |
| 45 | `MKDIRAT` | 1c.2 | | 84 | `SIGACTION` | 1c.9 |
| 46 | `UNLINKAT` | 1c.2 | | 85 | `SIGPROCMASK` | 1c.10 |
| 47 | `RENAMEAT` | 1c.4 | | 86 | `SIGALTSTACK` | 1c.10 |
| 48 | `LINKAT` | 1c.4 | | 87 | `SIGPENDING` | 1c.10 |
| 49 | `SYMLINKAT` | 1c.4 | | 88 | `SIGSUSPEND` | 1c.10 |
| 50 | `READLINKAT` | 1c.4 | | 89 | `SIGRETURN` | 1c.9 |
| 51 | `FCHMODAT` | 1c.4 | | 90 | `FORK` | 1d.1 |
| 52 | `FCHOWNAT` | 1c.4 | | 91 | `CLOCK_SETTIME` | 1e.1 |
| 53 | `UTIMENSAT` | 1c.4 | | 92 | `GETRANDOM` | 1e.1 |
| 54 | `FACCESSAT` | 1c.4 | | 93 | `SOCKET` | 1e.3 |
| 55 | `FTRUNCATE` | 1c.2 | | 94 | `SOCKETPAIR` | 1e.4 |
| 56 | `FSYNC` | 1c.2 | | 95 | `BIND` | 1e.3 |
| 57 | `PREAD` | 1c.2 | | 96 | `LISTEN` | 1e.3 |
| 58 | `PWRITE` | 1c.2 | | 97 | `ACCEPT` | 1e.3 |
| 59 | `DUP3` | 1c.1 | | 98 | `CONNECT` | 1e.3 |
| 60 | `FCNTL` | 1c.1, 1c.5 | | 99 | `SENDTO` | 1e.3 |
| 61 | `PIPE2` | 1c.1 | | 100 | `RECVFROM` | 1e.3 |
| 62 | `FCHDIR` | 1c.2 | | 101 | `SHUTDOWN` | 1e.3 |
| 63 | `GETCWD` | 1c.2 | | 102 | `GETSOCKOPT` | 1e.3 |
| 64 | `UMASK` | 1c.2 | | 103 | `SETSOCKOPT` | 1e.3 |
| 65 | `FD_PATH` | 1c.2 | | 104 | `GETSOCKNAME` | 1e.3 |
| 66 | `CURRENT_EXE` | 1c.2 | | 105 | `GETPEERNAME` | 1e.3 |
| 67 | `FSTATVFS` | 1c.4 | | 106 | `RESOLVE` | 1e.4 |
| 68 | `SYNC` | 1c.4 | | 107 | `CLOSE_RANGE` | 1c.1 |
| 69 | `FLOCK` | 1c.5 | | 108 | `SETITIMER` | 1c.10 |

Clocks (39 to 41) are numbered with the threads because `FUTEX_WAIT2`'s deadlines need
them; the RTC-backed wall clock is part of the same stage. Only setting the clock is left
for 1e. `CLOSE_RANGE` (107) and `SETITIMER` (108) were added after the first numbering and
take the next numbers, as every later call will. Section 1f adds no calls: dynamic
programs are loaded by `SPAWN2` and `EXEC`.

### 3.1 Memory (1a)

#### 31 `MMAP2`

```rust
fn mmap2(hint: usize, length: usize, prot: usize, flags: usize, fd: usize, offset: usize) -> usize
```

Maps memory and returns its address. v1's `MMAP` (8) stays as it is: it's
`mmap2(0, length, prot, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)` except that it keeps v1's
charging, a promise for every page whatever `prot` says, so a v1 program that maps
`PROT_NONE` and later makes it writable can't newly fail.

Validation, in order:

1. `length != 0`, rounded up to a page; at most the per-process limit. `EINVAL`, `ENOMEM`.
2. `prot` is a subset of `PROT_READ | PROT_WRITE | PROT_EXEC` (`EINVAL`). `PROT_WRITE |
   PROT_EXEC` together is refused with `EACCES`. `PROT_EXEC` without a file is refused with
   `EACCES`.
3. `flags` has exactly one of `MAP_SHARED`, `MAP_PRIVATE` (`MAP_SHARED_VALIDATE` counts as
   shared: since every flag is validated anyway, it means the same) plus any of
   `MAP_FIXED`, `MAP_FIXED_NOREPLACE`, `MAP_ANONYMOUS`, `MAP_NORESERVE`, `MAP_POPULATE`,
   `MAP_STACK` (accepted, no effect). Anything else, `MAP_GROWSDOWN` included: `EINVAL`.
   `MAP_FIXED` and `MAP_FIXED_NOREPLACE` together: `EINVAL`.
4. With `MAP_ANONYMOUS`, `fd` must be `-1` (a 32-bit argument) or `0`, and `offset` zero
   (`EINVAL`). Without it, `fd` must be an OFD for a regular file opened for reading
   (`EBADF`; `EACCES` if not readable; `ENODEV` for a pipe, socket, directory, the console
   or a `/dev` node other than `/dev/zero`, which maps as anonymous memory), and `offset` a
   multiple of the page size (`EINVAL`) with `offset + length` not overflowing (`EOVERFLOW`).
5. `MAP_SHARED` with `PROT_WRITE` on a file needs the file opened for writing too
   (`EACCES`), and counts as a writable open for `ETXTBSY` from then until it's unmapped.
6. `PROT_EXEC` on a file whose lock-table entry counts a writable open: `ETXTBSY`. The
   check and the count change happen in one critical section of the lock table.
7. Placement:
   - `MAP_FIXED`: `hint` must be page aligned (`EINVAL`) and the whole range inside the
     mmap part of the window (`ENOMEM`); whatever user mapping is there is replaced,
     atomically (the old pages are gone and the new ones there, or nothing changed). The
     window surface can't be replaced (`EINVAL`).
   - `MAP_FIXED_NOREPLACE`: as `MAP_FIXED`, but `EEXIST` if anything is mapped in the range.
   - otherwise a non-zero `hint` is used, rounded down to a page, if the whole range is
     free and inside the window; if not, it's ignored. Without a usable hint the kernel
     picks the lowest free gap above the image area with one unmapped guard page on each
     side (first fit, as the v1 bump allocator did, so a program behaves the same).
8. Charging (see below): `ENOMEM` if the charge can't be promised. `ENOMEM` too if the
   process already has `MAX_VMAS` (16384) VMAs and this would add one.

Semantics:

- **Anonymous private**: zeroed pages, each given its frame on first touch, as in 1a.1.
  A read of a page that was never written maps one machine-wide zero frame read-only;
  the first write gives it a frame of its own.
- **Anonymous shared**: backed by a shared-anonymous object, so a page gets one frame
  whichever process touches it first, and `FORK` shares the object rather than the
  frames that happen to exist.
- **File private**: pages read through the page cache on first touch. Reading maps the
  cache's frame read-only; the first write to a writable private mapping copies it (copy
  on write) and the copy is the process's. A page wholly past the end of the file faults
  with `SIGBUS` (`BUS_ADRERR`); the part of the last page past the end reads as zeroes.
  The file may grow later, and a page that's no longer past the end then reads normally.
- **File shared, read-only**: the cache's frames, mapped read-only. Writes through
  `write`/`pwrite` (by anyone, kernel apps included) update the cached pages in place, so a
  mapping always sees the file's current contents. `ftruncate` to below a mapped page
  makes it fault with `SIGBUS`: truncation walks the file's reverse map, clears the entries
  past the new end in every mapping and shoots down their TLB entries before it returns.
- **File shared, writable** (stage 1a.5): the cache's frames, mapped writable once the
  first write fault marks the page dirty. `write`, `read` and every mapping of the file see
  one copy. Dirty pages are written back to the filesystem at `MSYNC(MS_SYNC)` of their
  range, `FSYNC` of the file, `MUNMAP` and the last close, and at process exit; there is no
  background write-back, so a crash loses what wasn't synced, as POSIX allows. A page stays
  dirty while it's mapped writable, so each of those points writes it again (simple, and
  cheap for the users here: SQLite's `-shm` file and POSIX shared memory on `/dev/shm`,
  whose RAM mount has nothing to write back to). Kernel apps reading the file through the
  VFS see mapped writes after the next write-back.
- `MAP_POPULATE`: every page is given its frame (anonymous) or read (file) before the call
  returns; `ENOMEM` if that fails, with nothing mapped.

**Charging.** A promise (stage 1a.1's machine-wide `UNBACKED` accounting) is taken only for
pages that could need a frame of the process's own that can't be taken back: writable
private pages (anonymous or file) and writable shared anonymous pages, unless
`MAP_NORESERVE`. Read-only and `PROT_NONE` anonymous pages, file pages that are only read,
shared file pages (the cache can always write them back and reuse the frame), and
`MAP_NORESERVE` ranges take none. `MPROTECT` to writable takes the promise then, and fails
with `ENOMEM` if it can't. A `MAP_NORESERVE` page that can't get a frame when touched ends
the process with `SIGBUS`, not a kernel failure. This is Linux's strict overcommit mode,
and it's what lets a program reserve tens of GiB of `PROT_NONE` (relibc's loader, rustc's
arenas) on a machine with a few.

Uncharged pages never take a frame a promise counts on: the page cache, the zero frame
aside, fills a page only from frames free beyond `UNBACKED + SPARE_FRAMES`, reclaiming
unmapped clean pages first. When there are none, a fault on a file page ends the process
with `SIGBUS` and a `READ` through the cache returns `ENOMEM`; a promised page is never
the one that fails.

Page tables are charged too. A reservation builds no page-table entries (it lives only in
the VMA list), so `PROT_NONE` costs nothing but its VMA. A charged range also promises the
leaf and middle tables that could be needed to map all of it; an uncharged range takes the
promise for a table when a fault first needs one, and ends with `SIGBUS` if it can't. Leaf
tables left empty by `MUNMAP` are freed after the TLB shootdown. Table frames count against
the process's `USER_MEMORY_LIMIT`, which from stage 1a.3 means "pages charged or owned"
(promised, committed, or tables), not "pages mapped".

**W^X.** A page is executable only if it's a file page mapped straight from the page cache,
never a private copy and never anonymous memory, and its VMA isn't writable. So:
`MPROTECT(PROT_EXEC)` on anonymous memory, or on a private file range where any page has
been copied, fails with `EACCES` and changes nothing; making an executable file mapping
writable is allowed (it stops being executable); and once a page is written it can never
run. Writable opens of a file with executable mappings fail with `ETXTBSY`, and so do
executable mappings of a file open for writing (or mapped shared and writable), so writing
a file isn't a way around it either. Kernel apps write through the VFS without OFDs, so the
VFS's write and truncate hook enforces the same rule for them: a write or truncate of a
file with executable mappings is `ETXTBSY` (the shell's `cp` over a running program fails,
as on Linux). The hook clears a cached page's "instruction cache synced" flag whenever it
changes the page, so on AArch64 the next executable mapping of new contents is synced
again. Nothing in this plan generates code at run time.

**Deviation from Linux.** `PROT_WRITE | PROT_EXEC` and `PROT_EXEC` on anonymous memory
always fail; Linux allows both. `ETXTBSY` applies to `mmap(PROT_EXEC)` and to writes from
kernel apps; Linux applies it only to running executables.

#### `MUNMAP` (9) and `MPROTECT` (30), extended

`munmap(address, length)` and `mprotect(address, length, prot)` keep their numbers, and
from 1a.3 work on any user mapping (the program image, the stack, anything `MMAP2` made),
not only v1 `MMAP` memory. Ranges may cover holes (`munmap` ignores them; `mprotect` fails
with `ENOMEM` if any page in the range isn't mapped, and changes nothing), and may split a
VMA. The window surface stays refused (`EINVAL`). `mprotect` gains `PROT_EXEC` under the
W^X rule above. Both clear the entries, flush the TLB on every CPU that may hold the Mm's
entries and wait for each to acknowledge, and only then free frames, which `munmap` does
only once no copy in flight pins them. `munmap` (and `MADV_DONTNEED`) of a page wakes every
futex waiter keyed on it, so a waiter can't sleep on a page that's gone.

#### 32 `MADVISE`

```rust
fn madvise(address: usize, length: usize, advice: usize) -> 0
```

`address` page aligned and the range inside the window (`EINVAL`), and mapped (`ENOMEM`).

| Advice | Effect |
|---|---|
| `MADV_NORMAL` 0, `MADV_RANDOM` 1, `MADV_SEQUENTIAL` 2 | none |
| `MADV_WILLNEED` 3 | reads file pages into the cache, in the call |
| `MADV_DONTNEED` 4, `MADV_FREE` 8 | private pages lose their frames: anonymous pages read as zeroes next time, file pages as the file. Shared pages (anonymous or file) keep their contents and only leave this mapping. The promise stays |
| `MADV_DONTFORK` 10, `MADV_DOFORK` 11, `MADV_HUGEPAGE` 14, `MADV_NOHUGEPAGE` 15, `MADV_DONTDUMP` 16, `MADV_DODUMP` 17 | none |
| anything else | `EINVAL` |

`MADV_FREE` is treated as `MADV_DONTNEED`, which is allowed (the contents may be lost).

#### 33 `MSYNC`

```rust
fn msync(address: usize, length: usize, flags: usize) -> 0
```

Exactly one of `MS_ASYNC` 1, `MS_SYNC` 4, optionally `MS_INVALIDATE` 2 (`EINVAL`
otherwise); range rules as `MADVISE`. `MS_SYNC` writes back the dirty pages of shared file
mappings in the range before it returns (`EIO` if the filesystem fails); `MS_ASYNC` does
nothing (there's no background write-back to start); `MS_INVALIDATE` has nothing to do,
since every mapping of a file shares the cache's frames. Until stage 1a.5 nothing can be
dirty, so the call only validates.

`mremap`, `mlock`, `munlock`, `mlockall`, `munlockall` and `brk` have no calls: see the
`Pal` table.

### 3.2 Threads and time (1b)

A thread is a scheduler task whose Process is shared with its creator. Thread ids come
from the same allocator as process ids, so the two never collide: the thread a process
starts with has `tid == pid`, and every other thread's tid is a number no process has. A
pid or tid isn't reused while anything holds it: a living thread or process, an exit
record not yet reaped, or a process group or session that still has members. Every thread shares the address space, FD table,
cwd, umask, signal dispositions, credentials and process group; each has its own
registers, TLS register, signal mask, pending signals, alternate stack and kernel stack.

#### 34 `THREAD_CREATE`

```rust
fn thread_create(entry: usize, stack_top: usize, tls: usize, arg: usize, clear_word: usize) -> tid
```

- `entry`: in the window and on an executable page (`EFAULT`).
- `stack_top`: in the window and 16-byte aligned (`EINVAL`). Not checked further: a bad
  stack faults in the new thread, which is the process's own business.
- `tls`: zero, or inside the user window (`EINVAL`). On x86_64 this is what keeps a
  non-canonical value out of `FS_BASE`, where `wrmsr` would fault in the kernel.
- `clear_word`: zero, or a 4-byte aligned address in the window (`EINVAL`).
- `EAGAIN` with no free task slot open to user threads, or at the process's thread cap;
  `ENOMEM` if a kernel stack can't be had.

`THREAD_CREATE`, `FORK` and `EXEC` take the process's creation mutex, so a fork never
copies a half-created thread and an exec never races a fork.

The new thread starts at `entry` with `stack_top` as its stack pointer, `arg` in the first
argument register, every other general register zero, floating point in its initial state,
the TLS register set to `tls`, the creator's signal mask, no alternate stack and no pending
signals. When it ends, for any reason, and the process is still running, the kernel writes
a zero `u32` to `clear_word` (if non-zero and still mapped and writable) and wakes every
`FUTEX` waiter on it. That's what `pthread_join` can wait on, though relibc's own join
uses its `Waitval` instead.

relibc's `rlct_clone(stack, ...)` pushes its start words on the new stack and expects the
child to start with the stack pointer at them. Its feos version passes `entry` = a
two-instruction trampoline that pops them, as the Linux version's inline assembly does
after `clone` returns zero, and `arg` = 0.

**Deviation.** No `clone`: no flags, no new-process mode, no parent-TID or child-TID
writes beyond `clear_word`.

#### 35 `THREAD_EXIT`

```rust
fn thread_exit(unmap_address: usize, unmap_length: usize) -> !
```

Ends the calling thread. If `unmap_length` isn't zero, the range must be page aligned and
something `MUNMAP` could take (`EINVAL`; like `EXEC`, a `-> !` call does return when its
arguments fail validation). Then, with the thread off the user stack and on its kernel
stack, and in one critical section under the Mm lock: `clear_word` (if set, and still
mapped and writable) is written with zero, then the range is unmapped. Only after the lock
is released are `clear_word`'s waiters woken. So a joiner that wakes and reuses the
addresses can't have its new mapping destroyed by the late unmap, and a `clear_word`
inside the range is written before the range goes. relibc's
`exit_thread(stack_base, stack_size)` frees its own stack this way. If this was the
process's last thread, the process exits with status 0.

`EXIT` (0) keeps its number and now ends the whole process: every thread, with the given
status. That's POSIX `_exit` and Linux's `exit_group`.

#### 36 `GETTID`

```rust
fn gettid() -> tid
```

Can't fail. `GETPID` (7) returns the process's pid, which is the first thread's tid.

#### 37 `SET_TLS`

```rust
fn set_tls(value: usize) -> 0
```

Sets the calling thread's TLS register: `FS_BASE` on x86_64, `TPIDR_EL0` on AArch64.
`value` is zero or in the user window (`EINVAL`). The kernel restores it per thread on
every switch to a user thread. On x86_64 it writes `FS_BASE` unconditionally at each
switch-in, with no per-CPU cache of what the CPU holds: a program can change the hardware
FS base by loading a selector, so a cache would let one process leave another an FS base
it chose. `CR4.FSGSBASE` stays off (it would let a program set the GS base the kernel's
`swapgs` relies on). On AArch64 a program may also write `TPIDR_EL0` itself, and the kernel
saves whatever it finds there whenever the thread leaves the CPU, including while it's in a
call, so the window check is moot there; the `msr tpidr_el0, xzr` in `mmu::activate` goes.
relibc's `Tcb::os_arch_activate` calls this on both ports.

#### 38 `FUTEX_WAIT2`

```rust
fn futex_wait2(address: usize, expected: usize, deadline: usize, clock: usize, flags: usize) -> 0
```

As `FUTEX_WAIT` (12): blocks while the aligned `u32` at `address` equals `expected`
(`EAGAIN` if it doesn't when called). `deadline` is zero for none, or a pointer to a
[`Timespec`](#timespec) holding an **absolute** time on `clock` (`CLOCK_MONOTONIC` or
`CLOCK_REALTIME`, `EINVAL` otherwise; `EINVAL` for `nsec` outside `0..1e9` or a negative
`sec`). Returns 0 on a wake (which may be spurious), `ETIMEDOUT` when the deadline passes,
`EINTR` when a signal handler ran. `flags` must be zero. `FUTEX_WAKE` (13) is unchanged.

**Keys.** A user futex is keyed by what the word *is*, not by the frame that happens to
hold it, so copy-on-write, page-cache reclaim and the zero frame can't split or merge
waiters:

- in a private mapping (anonymous or file), by `(Mm, virtual address)`: Linux's private
  futex. A fork's copy-on-write moving the page to a new frame doesn't change the key, and
  a child's waiters never meet the parent's;
- in a shared mapping, by `(shared object, offset)`: the shared-anonymous object, or the
  file's node identity and the offset in the file, so processes mapping the same page at
  different addresses meet.

The page is faulted in as the process's own (writable private pages are copied first)
before the key is taken, and its frame is pinned from the value check until the waiter is
resumed, so the check under the scheduler lock reads a live frame. User keys are their own
`libs/sched` variant (`Wait::UserKey`), never equal to a kernel `Wait::Key`, so a user
waiter can't absorb a wake meant for a kernel `feos_sync` lock. `munmap` and
`MADV_DONTNEED` of the page wake its waiters. `FUTEX_WAKE` (13) computes the key the same
way. v1's `FUTEX_WAIT` (12) moves to these keys in stage 1b.1; for a single-threaded v1
process nothing observable changes.

A `CLOCK_REALTIME` deadline is converted to monotonic when the call starts; a later
`CLOCK_SETTIME` doesn't move it (Linux does move it; nothing here depends on that).

relibc's `futex_wait(addr, val, deadline)` passes an absolute monotonic deadline, which
this takes as it is. There's no bitset variant: relibc doesn't use one.

#### 39 `CLOCK_GETTIME`, 40 `CLOCK_GETRES`

```rust
fn clock_gettime(clock: usize, out: *mut Timespec) -> 0
fn clock_getres(clock: usize, out: *mut Timespec) -> 0   // out may be 0
```

| Clock | Value | |
|---|---|---|
| `CLOCK_REALTIME` 0, `CLOCK_REALTIME_COARSE` 5 | wall clock: the RTC's reading at boot plus monotonic time since, plus whatever `CLOCK_SETTIME` added | |
| `CLOCK_MONOTONIC` 1, `CLOCK_MONOTONIC_RAW` 4, `CLOCK_MONOTONIC_COARSE` 6, `CLOCK_BOOTTIME` 7 | nanoseconds since boot, from the port's counter (`Clock::ticks`), never going back | the machine doesn't suspend, so boot time is monotonic time |
| `CLOCK_PROCESS_CPUTIME_ID` 2 | CPU time of every thread of the process, live and ended | |
| `CLOCK_THREAD_CPUTIME_ID` 3 | the calling thread's CPU time | |
| anything else | `EINVAL` | |

The resolution reported is one period of the counter, rounded up to a nanosecond, for
the first two groups, and the scheduler's accounting precision for CPU clocks. With no
RTC, `CLOCK_REALTIME` starts at the Unix epoch, and `SYSINFO` says so.

The RTC (PL031 or CMOS, both already drivers) counts whole seconds, so at boot the kernel
waits for the second to change before taking its reading, from a task, so boot doesn't
wait: until then `CLOCK_REALTIME` reads the coarse value.

#### 41 `NANOSLEEP`

```rust
fn nanosleep(clock: usize, flags: usize, request: *const Timespec, remain: *mut Timespec) -> 0
```

`clock_nanosleep`'s shape: `clock` is `CLOCK_MONOTONIC`, `CLOCK_REALTIME` or
`CLOCK_BOOTTIME`; `CLOCK_PROCESS_CPUTIME_ID` is `EOPNOTSUPP` (a deviation: Linux sleeps on
it), `CLOCK_THREAD_CPUTIME_ID` and anything else `EINVAL`, as Linux; `flags` is 0 or
`TIMER_ABSTIME` 1. `request` valid as for `FUTEX_WAIT2`. Sleeps until the time passes
(returns 0) or a signal handler runs (`EINTR`, and for a relative sleep the time left is
written to `remain` if it isn't zero). Never restarted. v1's `SLEEP` (5) stays.

### 3.3 Files and directories (1c)

All path calls resolve a relative path against `dirfd` (`AT_FDCWD` for the cwd). Until the
VFS can look up relative to a node (phase 7), a directory OFD remembers the canonical
absolute path it was opened by (every symbolic link resolved, no `.` or `..`), and
resolution joins strings, so a directory renamed after it was opened is found by its old
name. Because the remembered path is canonical, `..` from a dirfd or the cwd is physical:
it's the parent of the directory itself, never of a link that led there. That's the one
place this differs from POSIX `*at` semantics, and it's written down so relibc and std
don't rely on the difference either way.

Symbolic links are followed in every component but the last unless a call says otherwise;
more than 40 links in one resolution is `ELOOP`. Trailing slashes follow POSIX: a path
ending in `/` must name a directory (`ENOTDIR` for anything else, after following a final
link), so `OPENAT("file/")` is `ENOTDIR` while `MKDIRAT("a/")` creates `a`, and
`RENAMEAT("a/", "b/")` renames a directory and is `ENOTDIR` if `a` isn't one. Permission checks are those of v1:
search permission on each directory, the file's own bits, root may do anything but run a
file with no execute bit, and a filesystem that keeps no owners allows everything the
mount allows.

The VFS's errors map to: not found `ENOENT`, exists `EEXIST`, not a directory `ENOTDIR`,
is a directory `EISDIR`, directory not empty `ENOTEMPTY`, permission `EACCES`, read-only
mount or file `EROFS`, no space `ENOSPC`, out of memory `ENOMEM`, unsupported `EOPNOTSUPP`,
busy `EBUSY`, across mounts `EXDEV`, bad name `EINVAL`, anything else `EIO`. (v1's calls
keep their old mapping, which says `EINVAL` for not-a-directory and `EBUSY` for not-empty.)

**Transfers.** From stage 1c.1, `READ`, `WRITE`, `PREAD` and `PWRITE` on a regular file
move the whole request, up to the end of file for a read and until `ENOSPC` or `EFBIG` for
a write, in `IO_CHUNK` pieces inside the kernel; v1's cap of one `IO_CHUNK` per call
(`with_user_buffer`) goes for files. A write past the end of file fills the gap with
zeroes (SQLite writes page N of a new database directly). A blocking write to a pipe or
socketpair of any size completes unless a signal handler interrupts it (then the partial
count), and one of at most `PIPE_BUF` bytes is atomic. `READ` on a directory is `EISDIR`;
directories are read with `GETDENTS`.

**Times.** From stage 1c.4 a file's times follow POSIX:

- `WRITE`, `PWRITE`, `FTRUNCATE` (when the size changes, and `O_TRUNC`) set `mtime` and
  `ctime` to now, as does write-back of a page dirtied through a shared mapping;
- creating, unlinking or renaming an entry sets the directory's `mtime` and `ctime`;
- `FCHMODAT`, `FCHOWNAT`, `LINKAT` (on the file), `RENAMEAT` (on the moved file) and
  `UTIMENSAT` set `ctime`;
- `atime` is `noatime`: never updated by reads, only by `UTIMENSAT`. Nothing here reads
  it, and it saves a metadata write per read.

They're stored at nanosecond resolution on the RAM mounts and the native format (which
gains nanosecond fields in 1c.4, reading older images as `.0`), at 10 ms on exFAT and at
2 s (modification) on FAT, which is why builds belong on a native or RAM mount; `Stat2`
reports what's stored.

#### 42 `OPENAT`

```rust
fn openat(dirfd: usize, path: usize, path_length: usize, flags: usize, mode: usize) -> fd
```

Flags: one access mode (`O_RDONLY` 0, `O_WRONLY` 1, `O_RDWR` 2; 3 is `EINVAL`), plus
`O_CREAT`, `O_EXCL`, `O_TRUNC`, `O_APPEND`, `O_NONBLOCK`, `O_DIRECTORY`, `O_NOFOLLOW`,
`O_CLOEXEC`, `O_PATH`, `O_DSYNC`, `O_SYNC`, and `O_NOCTTY`, `O_LARGEFILE`, `O_NOATIME`,
`O_DIRECT`, which are accepted and do nothing. `O_TMPFILE` is `EOPNOTSUPP`; `O_ASYNC`
`EINVAL`. `mode` (with `O_CREAT`) is masked by `0o7777` and the umask.

- The lowest free descriptor is returned. `EMFILE` when the table is full.
- `O_CREAT` creates a regular file owned by the caller (`EEXIST` with `O_EXCL` if anything
  is there, a dangling link included). Without `O_CREAT`, a missing file is `ENOENT`.
- `O_DIRECTORY`: `ENOTDIR` unless it's a directory. A directory may be opened `O_RDONLY`
  (for `GETDENTS`, `*at` and `FCHDIR`); with write access it's `EISDIR`.
- `O_NOFOLLOW`: `ELOOP` if the last component is a symbolic link, unless `O_PATH` too, in
  which case the fd refers to the link itself.
- `O_PATH`: needs only search permission on the way; the fd works with `*at` calls,
  `FSTATAT` with `AT_EMPTY_PATH`, `FCHDIR`, `FD_PATH`, `CLOSE`, `DUP3`, `FCNTL`
  (`F_GETFD`, `F_SETFD`, `F_GETFL`, `F_DUPFD*`) and nothing else (`EBADF`).
- `O_TRUNC` with write access empties a regular file (`ETXTBSY` if it's mapped executable).
- Write access to a file with an executable mapping: `ETXTBSY`.
- `O_APPEND`: every write goes to the end, atomically with respect to other writes
  through the kernel.
- `O_DSYNC`, `O_SYNC`: every write is followed by an `FSYNC` of the file.
- `O_NONBLOCK`: on pipes, sockets and the console, a read or write that would wait fails
  with `EAGAIN` instead. On files and directories it has no effect.
- `/dev` and `/proc/self` nodes behave as in [the section below](#dev-and-procself).

v1's `OPEN` (3) is `openat(AT_FDCWD, path, length, v1 flags translated, 0o666)`.

#### 43 `FSTATAT`

```rust
fn fstatat(dirfd: usize, path: usize, path_length: usize, out: *mut Stat2, flags: usize) -> 0
```

Flags: `AT_SYMLINK_NOFOLLOW` (the link itself), `AT_EMPTY_PATH` (with `path_length` 0: the
file `dirfd` refers to, which may be any fd: a pipe says `S_IFIFO`, a socket `S_IFSOCK`,
the console `S_IFCHR`), `AT_NO_AUTOMOUNT` (accepted, no effect). Needs search permission on
the way, nothing on the file. Fills a [`Stat2`](#stat2). `fstat` is `fstatat(fd, "", 0,
out, AT_EMPTY_PATH)`.

#### 44 `GETDENTS`

```rust
fn getdents(fd: usize, buffer: usize, length: usize) -> bytes
```

Reads directory entries from a directory OFD into `buffer` as a sequence of
[`Dirent`](#dirent) records, as many whole ones as fit, and returns the bytes written;
zero at the end. `EINVAL` if not even one record fits; `ENOTDIR` for anything but a
directory.

The first call after opening (or after `SEEK(fd, 0, SET)`) takes a snapshot of the whole
listing into the OFD (the VFS returns whole listings), with `.` and `..` first; later
calls continue through it. Each record's `off` is a cookie for the entry after it.
Entries added or removed after the snapshot aren't seen until a rewind (POSIX allows
either). `/.feos-orphans` is never listed.

`SEEK` on a directory OFD works in cookies, which is what relibc's `dir_seek`
(`telldir`/`seekdir`) and its generic `posix_getdents` (`lseek(fd, 0, SEEK_CUR)` before a
batch, `lseek(fd, next, SEEK_SET)` after) use:

- `SEEK_CUR` with offset 0 returns the cookie of the next unread entry (0 before the first
  read); any other offset is `EINVAL`;
- `SEEK_SET` to 0 rewinds and drops the snapshot; to a cookie, positions in the snapshot,
  taking one first if there's none (cookies are entry positions, so they stay meaningful
  across snapshots of an unchanged directory);
- a cookie past the snapshot's end, or negative, is `EINVAL`, and so is `SEEK_END`.

`Pal::getdents`'s `opaque_offset` argument is ignored, as on Linux: the OFD's position is
what counts.

#### 45 `MKDIRAT`

```rust
fn mkdirat(dirfd: usize, path: usize, path_length: usize, mode: usize) -> 0
```

Mode masked as `OPENAT`'s. Needs write and search permission on the parent. `EEXIST` if
anything is there. Owned by the caller.

#### 46 `UNLINKAT`

```rust
fn unlinkat(dirfd: usize, path: usize, path_length: usize, flags: usize) -> 0
```

`flags` is 0 (a non-directory: `EISDIR` for a directory) or `AT_REMOVEDIR` (an empty
directory: `ENOTDIR`, `ENOTEMPTY`; `EBUSY` for a mount point or `/`; `EINVAL` for a last
component of `.`). Never follows the last component.

**Unlinking a file in use.** POSIX keeps an unlinked file's data until nothing uses it
(`tempfile::tempfile()` opens and unlinks at once; `rm -rf target/` runs while rustc has
`.rmeta` files mapped and a build script is running). The VFS can't promise that on every
filesystem, so the kernel does it itself: if the lock table's hold count for the file
(OFDs, VMAs and page-cache pins together) isn't zero, the last link is renamed to
`/.feos-orphans/<dev>-<ino>` on the same mount (created on demand, mode 0o700, owner root)
instead of removed, and the orphan is removed when the hold count reaches zero, and at
mount time (orphans left by a crash). A running program's text, a library `ld.so` mapped
and a mapped pack file all keep working after their names go. On a read-only mount it
can't happen. The directory is hidden: `GETDENTS` of `/` never lists it, and a user
program's lookup of it is `ENOENT`.

#### 55 `FTRUNCATE`

```rust
fn ftruncate(fd: usize, length: usize) -> 0
```

`length` as `i64`, `EINVAL` if negative; `EFBIG` above what the filesystem holds. The fd
must be a regular file opened for writing (`EBADF`; `EINVAL` for a non-file). `ETXTBSY` for
a file mapped executable. Mapped pages past the new end fault with `SIGBUS` (the file's
reverse map is walked before the call returns, [3.1](#31-mmap2)).

#### 56 `FSYNC`

```rust
fn fsync(fd: usize, flags: usize) -> 0
```

`flags` 0 (`fsync`) or `FSYNC_DATA` 1 (`fdatasync`). The fd is a regular file or a
directory (including `O_PATH`-less directory OFDs); `EINVAL` for anything else, as Linux
says for pipes, sockets and the console. Dirty pages of the file's shared mappings are
written back first. Until the VFS can sync one file, this then syncs the file's mount
(`VfsTree::sync`), which is correct but slower.

#### 57 `PREAD`, 58 `PWRITE`

```rust
fn pread(fd: usize, buffer: usize, length: usize, offset: usize) -> count
fn pwrite(fd: usize, buffer: usize, length: usize, offset: usize) -> count
```

As `READ` and `WRITE` at `offset` (as `i64`, `EINVAL` if negative), not moving the OFD's
offset. `ESPIPE` on a pipe, socket or the console; `EISDIR` on a directory. **Deviation:**
`pwrite` on an `O_APPEND` fd writes at `offset`, as POSIX says, not at the end, as Linux
does.

#### 59 `DUP3`

```rust
fn dup3(old: usize, new: usize, flags: usize) -> new
```

Makes `new` refer to `old`'s OFD, closing `new` first if it was open (closing errors are
ignored, as POSIX says). `flags` is 0 or `O_CLOEXEC`. `old == new` is `EINVAL` (relibc's
`dup2` checks `old` with `F_GETFD` and returns it). `new` must be below `MAX_FDS`
(`EBADF`); the table grows to hold it (`EMFILE` if it can't). `dup` is
`FCNTL(old, F_DUPFD, 0)`.

#### 107 `CLOSE_RANGE`

```rust
fn close_range(first: usize, last: usize, flags: usize) -> 0
```

Closes every open descriptor from `first` to `last` inclusive (`last` may be `u32::MAX`;
both are 32-bit arguments; `EINVAL` if `first > last`), or with `CLOSE_RANGE_CLOEXEC` 4
marks them `cloexec` instead. Close errors are ignored. Other flags, `CLOSE_RANGE_UNSHARE`
2 included, are `EINVAL` (the FD table is never shared between processes). CPython's
`subprocess` and `os.closerange` use it instead of a close per descriptor up to the limit.

#### 60 `FCNTL`

```rust
fn fcntl(fd: usize, command: usize, arg: usize) -> value
```

| Command | `arg` | Result |
|---|---|---|
| `F_DUPFD` 0, `F_DUPFD_CLOEXEC` 1030 | lowest fd to use, below `MAX_FDS` (`EINVAL`) | the new fd |
| `F_GETFD` 1 | | `FD_CLOEXEC` 1 or 0 |
| `F_SETFD` 2 | `FD_CLOEXEC` or 0 (`EINVAL` for other bits) | 0 |
| `F_GETFL` 3 | | access mode, `O_APPEND`, `O_NONBLOCK`, `O_PATH`, `O_DIRECTORY` as opened |
| `F_SETFL` 4 | flags | 0; only `O_APPEND` and `O_NONBLOCK` change. The other defined `O_*` bits that can't be changed after open (the access mode, `O_CREAT`, `O_EXCL`, `O_NOCTTY`, `O_TRUNC`, `O_DSYNC`, `O_SYNC`, `O_DIRECT`, `O_LARGEFILE`, `O_DIRECTORY`, `O_NOFOLLOW`, `O_NOATIME`, `O_CLOEXEC`, `O_PATH`) are ignored, as POSIX says, so `fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) \| O_NONBLOCK)` works; undefined bits and `O_ASYNC` are `EINVAL` |
| `F_GETLK` 5, `F_SETLK` 6, `F_SETLKW` 7 | pointer to [`Flock`](#flock) | 0 (stage 1c.5, below) |
| anything else | | `EINVAL` |

Record locks (1c.5): POSIX advisory byte-range locks, `F_RDLCK` 0 / `F_WRLCK` 1 / `F_UNLCK`
2, `whence` 0/1/2 as `SEEK`, `len` 0 meaning to the end. Owned by the process; they
split, merge and convert as POSIX says; all of a process's locks on a file go when it
closes *any* fd for that file, or exits. They survive `EXEC`, as POSIX says, except that
the `cloexec` descriptors `EXEC` closes drop them as any close does. `F_SETLK` on a
conflict: `EAGAIN`. `F_SETLKW` blocks (`EINTR` on a signal);
there's no deadlock detection (a deviation POSIX allows: `EDEADLK` is "may"). `F_GETLK`
writes the first conflicting lock with its owner's pid, or `F_UNLCK`. A read lock needs an
fd open for reading, a write lock one open for writing (`EBADF`). `ENOLCK` past 4096 locks
machine-wide, or past 256 held by one process. Not inherited by `FORK` or `SPAWN2`. These exist for SQLite (cargo's global
cache tracker goes through rusqlite's default `unix` VFS).

#### 61 `PIPE2`

```rust
fn pipe2(fds: *mut [i32; 2], flags: usize) -> 0
```

`flags`: `O_CLOEXEC`, `O_NONBLOCK`. Read end, then write end. `EMFILE` for the table,
`ENFILE` when the machine's pipes run out. v1's `PIPE` (14) is `pipe2(fds, 0)`.

Pipe semantics for v2 (v1's pool changes underneath, see stage 1b.5): capacity at least
16 KiB; a write of at most `PIPE_BUF` (4096) bytes is atomic; a larger blocking write
completes unless interrupted; a read returns what's there as soon as there's something;
end of file once every write OFD has closed; a write with no read OFD left fails with
`EPIPE` and raises `SIGPIPE` on the writing thread (stage 1c.9). `ENFILE` past the
machine's pipes or 256 held by one process.

#### 62 `FCHDIR`, 63 `GETCWD`

```rust
fn fchdir(fd: usize) -> 0
fn getcwd(buffer: usize, length: usize) -> bytes
```

`FCHDIR` makes the directory `fd` refers to (`O_PATH` is fine) the cwd: `ENOTDIR`,
`EACCES` without search permission. `chdir(path)` is `OPENAT(path, O_PATH | O_DIRECTORY |
O_CLOEXEC)`, `FCHDIR`, `CLOSE` in libc.

`GETCWD` writes the absolute, canonical cwd, without a NUL, and returns its length, as
every other path-returning call here does (`FD_PATH`, `CURRENT_EXE`, `READLINKAT`);
`ERANGE` if it doesn't fit (`length` 0 is `ERANGE` too). relibc's backend passes one byte
less than its buffer and adds the terminator.

#### 64 `UMASK`

```rust
fn umask(mask: usize) -> old
```

Sets the process's mask (`mask & 0o777`; other bits `EINVAL`) and returns the old one.
Applied by `OPENAT`, `MKDIRAT` and `SPAWN2`'s open actions. Inherited across `SPAWN2`,
`FORK` and `EXEC`. The initial process's is `0o022`.

#### 65 `FD_PATH`

```rust
fn fd_path(fd: usize, buffer: usize, length: usize) -> bytes
```

The absolute path the fd's file or directory was opened by, with symbolic links resolved,
no NUL. `ERANGE` if it doesn't fit. The console says `/dev/console` and a `/dev` node its
own path; a pipe or socket is `ENOENT`. relibc's `realpath` is `OPENAT(O_PATH)` then this
(its `Sys::fpath`).

#### 66 `CURRENT_EXE`

```rust
fn current_exe(buffer: usize, length: usize) -> bytes
```

The absolute path, links resolved, of the program this process is running (for a dynamic
program, the program, not its interpreter), as of the `SPAWN2` or `EXEC` that started it.
`ERANGE` if it doesn't fit. rustc finds its sysroot from this; `/proc/self/exe` is a link
to the same path, and the aux vector's `AT_FEOS_EXE` points at a copy of it.

#### 47 `RENAMEAT`

```rust
fn renameat(from: *const PathAt, to: *const PathAt, flags: usize) -> 0
```

Two [`PathAt`](#pathat)s, since two dirfds, two paths and flags don't fit in six
registers. `flags`: 0 or `RENAME_NOREPLACE` 1 (`EEXIST` if `to` exists); `RENAME_EXCHANGE`
2 and `RENAME_WHITEOUT` 4 are `EINVAL`. Replacing an existing `to` is atomic: at every
moment `to` names either the old file or the new one (the VFS gains this guarantee in
1c.4; cargo, rustc, lld and every `tempfile` user depend on it). The file `to` named
before is unlinked as `UNLINKAT` would, so if it's in use (open, mapped, running) it
becomes an orphan and keeps working until released. A directory may replace an
empty directory only (`ENOTEMPTY`, `ENOTDIR`, `EISDIR` as POSIX); `EXDEV` across mounts;
`EINVAL` for moving a directory under itself; `EBUSY` for a mount point. Write and search
permission on both parents. Doesn't follow either last component.

#### 48 `LINKAT`

```rust
fn linkat(existing: *const PathAt, new: *const PathAt, flags: usize) -> 0
```

`flags`: `AT_SYMLINK_FOLLOW` (follow `existing` if it's a link; without it the link
itself is linked), `AT_EMPTY_PATH` (`existing` is its `dirfd`'s file). `EPERM` for a
directory, `EXDEV` across mounts, `EEXIST`, `EMLINK`, `EOPNOTSUPP` where the filesystem
has no hard links. v1's `LINK` (29) stays.

#### 49 `SYMLINKAT`

```rust
fn symlinkat(target: usize, target_length: usize, dirfd: usize, path: usize, path_length: usize) -> 0
```

`target` follows the path rule (up to `PATH_MAX`, no NUL: `EINVAL`, UTF-8: `EILSEQ`), so
every link can be resolved; it isn't checked to exist. `EEXIST`; `EOPNOTSUPP` where the
filesystem has no links (FAT).

#### 50 `READLINKAT`

```rust
fn readlinkat(dirfd: usize, path: usize, path_length: usize, buffer: usize, length: usize) -> bytes
```

The link's target, no NUL, cut short silently at `length` (POSIX `readlink`). `EINVAL` if
it isn't a link or `length` is 0. `AT_EMPTY_PATH` behaviour (`path_length` 0) reads the
link an `O_PATH | O_NOFOLLOW` fd refers to.

#### 51 `FCHMODAT`

```rust
fn fchmodat(dirfd: usize, path: usize, path_length: usize, mode: usize, flags: usize) -> 0
```

`mode & !0o7777` is `EINVAL`. `flags`: `AT_EMPTY_PATH` (`fchmod`); `AT_SYMLINK_NOFOLLOW`
is `EOPNOTSUPP`, as Linux. The owner or root (`EPERM`), as v1's `CHMOD`.

#### 52 `FCHOWNAT`

```rust
fn fchownat(dirfd: usize, path: usize, path_length: usize, uid: usize, gid: usize, flags: usize) -> 0
```

`uid`/`gid` are 32-bit arguments: `-1` (`u32::MAX`, or v1's `KEEP`, `usize::MAX`, which
is its sign extension) leaves one alone; `libs/abi` names it `KEEP` for both. `flags`: `AT_SYMLINK_NOFOLLOW`
(`lchown`), `AT_EMPTY_PATH` (`fchown`). Root only (`EPERM`), except that leaving both alone
always succeeds.

#### 53 `UTIMENSAT`

```rust
fn utimensat(dirfd: usize, path: usize, path_length: usize, times: *const [Timespec; 2], flags: usize) -> 0
```

`times` is access then modification; zero means both now. A `nsec` of `UTIME_NOW`
(`0x3fff_ffff`) means now, `UTIME_OMIT` (`0x3fff_fffe`) leaves it; otherwise `0..1e9`
(`EINVAL`). `flags`: `AT_SYMLINK_NOFOLLOW`, `AT_EMPTY_PATH` (`futimens`). Setting both to
now needs write permission or ownership; anything else needs ownership or root (`EPERM`).
Stored at the filesystem's resolution (see **Times** above: nanoseconds on the RAM mounts
and the native format).

#### 54 `FACCESSAT`

```rust
fn faccessat(dirfd: usize, path: usize, path_length: usize, mode: usize, flags: usize) -> 0
```

`mode` is `F_OK` 0 or any of `R_OK` 4, `W_OK` 2, `X_OK` 1 (`EINVAL` for more). `flags`:
`AT_EACCESS` (the same as without: there are no set-user-ID programs, so real and
effective ids are equal), `AT_SYMLINK_NOFOLLOW`, `AT_EMPTY_PATH`. `EACCES`, `ENOENT`,
`EROFS` for `W_OK` on a read-only mount.

#### 67 `FSTATVFS`

```rust
fn fstatvfs(fd: usize, out: *mut StatVfs) -> 0
```

The filesystem `fd`'s file is on, from `VfsTree::statfs`: a [`StatVfs`](#statvfs).
`statvfs(path)` is open, this, close. A pipe, socket or console: `EINVAL`.

#### 68 `SYNC`

```rust
fn sync() -> 0
```

`VfsTree::sync`. Can't fail.

#### 69 `FLOCK`

```rust
fn flock(fd: usize, operation: usize) -> 0
```

`LOCK_SH` 1, `LOCK_EX` 2 or `LOCK_UN` 8, optionally `LOCK_NB` 4 (`EINVAL` otherwise). BSD
semantics: the lock belongs to the OFD, so it's shared by `dup`, `FORK` and inheritance,
and goes when the last descriptor for that OFD closes. Converting between shared and
exclusive isn't atomic (BSD isn't either). With `LOCK_NB` a conflict is `EAGAIN`
(`EWOULDBLOCK`); without it the call blocks (`EINTR` on a signal). Any fd for a regular
file or directory, any access mode. cargo's `target/` and registry locks are this.

#### 70 `IOCTL`

```rust
fn ioctl(fd: usize, request: usize, arg: usize) -> value
```

feos's own request numbers, not Linux's; relibc's feos `ioctl` translates `TCGETS`,
`TCSETS*`, `TIOCGWINSZ`, `TIOCSWINSZ`, `TIOCGPGRP`, `TIOCSPGRP`, `TCSBRK` (`tcdrain`),
`TCFLSH` (`tcflush`), `TCXONC` (`tcflow`), `TIOCGSID` (`tcgetsid`) and `FIONREAD` to them,
and does `FIONBIO`, `FIOCLEX` and `FIONCLEX` itself with `FCNTL`. `TIOCSCTTY` and
`TIOCNOTTY` are answered in relibc: 0 on a terminal fd, `ENOTTY` otherwise, since there's
no controlling-terminal acquisition (a program's terminal is the one it was started on).

| Request | `arg` | Meaning |
|---|---|---|
| `TTY_GET_ATTR` 1 | `*mut Termios` | the terminal's settings |
| `TTY_SET_ATTR` 2, `TTY_SET_ATTR_DRAIN` 3, `TTY_SET_ATTR_FLUSH` 4 | `*const Termios` | set them now, after output drains, or after output drains and pending input is dropped |
| `TTY_GET_WINSIZE` 5 | `*mut WinSize` | the terminal's size in characters (and pixels, or 0) |
| `TTY_SET_WINSIZE` 6 | `*const WinSize` | stored and reported; raises `SIGWINCH` on the foreground group |
| `TTY_GET_PGRP` 7 | `*mut i32` | the foreground process group |
| `TTY_SET_PGRP` 8 | `*const i32` | set it: a group in the caller's session (`EPERM`) |
| `FD_NREAD` 9 | `*mut i32` | bytes a read would return now: pipes, sockets, the console; for a file, size minus offset (clamped to `i32::MAX`) |
| `TTY_DRAIN` 10 | 0 | wait until output is written (`tcdrain`) |
| `TTY_FLUSH` 11 | `TCIFLUSH` 0, `TCOFLUSH` 1, `TCIOFLUSH` 2 | drop pending input, output or both (`EINVAL` otherwise) |
| `TTY_FLOW` 12 | `TCOOFF` 0, `TCOON` 1, `TCIOFF` 2, `TCION` 3 | suspend or resume output; the input ones are accepted with no effect (`EINVAL` otherwise) |
| `TTY_GET_SID` 13 | `*mut i32` | the session the terminal belongs to |

Requests 1 to 8 and 10 to 13 on anything but a terminal are `ENOTTY`, which is how
`isatty` answers.
An fd is a terminal when its OFD is the console stream a program got from the terminal it
was started on (feos-sh marks it so when it starts a program in the foreground); a
redirected stream is a pipe or file and isn't. Of the `Termios` flags the console honours
`ICANON`, `ECHO`, `ECHOE`, `ISIG` (`VINTR` raising `SIGINT`, `VQUIT` `SIGQUIT`, `VSUSP`
`SIGTSTP` on the foreground group), `ICRNL` and `OPOST | ONLCR`, and `VMIN`/`VTIME` in
non-canonical mode; the rest are stored and reported unchanged. Unknown requests:
`EINVAL`.

**Background groups.** A `READ` of the terminal by a process not in its foreground group
raises `SIGTTIN` on that process's group (whose default action stops it) and fails with
`EINTR`, or fails with `EIO` if the reader ignores or blocks `SIGTTIN` or its group is
orphaned. Writes from a background group are allowed: `TOSTOP` is stored, not honoured.
Settings changes (`TTY_SET_*`) from a background group are allowed too (a deviation:
POSIX raises `SIGTTOU`; nothing here relies on it).

**No PTYs.** There's no `/dev/ptmx` and no pseudo-terminal pairs in phase 1, so relibc's
`posix_openpt`/`openpty` fail with `ENOENT`, Python's `pty` module doesn't work, and the
few lit tests that need one are marked unsupported on feos.

#### 71 `POLL`

```rust
fn poll(fds: *mut [PollFd], count: usize, timeout_ns: usize, sigmask: *const u64) -> ready
```

`count` up to `MAX_FDS` (`EINVAL`). `timeout_ns` as `i64`: negative waits forever, 0
doesn't wait. `events` bits other than those below (`POLLRDHUP`, `POLLRDBAND`,
`POLLWRBAND`, `POLLMSG`) are accepted and never reported: `events` is a request, not a
flag word, and the unknown-bit rule doesn't apply to it. `sigmask` zero, or a mask to use while waiting (`ppoll`, `pselect`).
Returns how many entries have non-zero `revents`; 0 on timeout; `EINTR` if a handler ran.
A negative `fd` is skipped (`revents` 0); a closed one gets `POLLNVAL` (std's startup
check of fds 0 to 2 relies on this).

| Object | `POLLIN` | `POLLOUT` | `POLLHUP` | `POLLERR` |
|---|---|---|---|---|
| file, directory | always | always (file opened for writing) | | |
| pipe, read end | data, or no writers | | no writers left | |
| pipe, write end | | at least `PIPE_BUF` free | | no readers left |
| console | input ready (a whole line in canonical mode) | always | | |
| TCP listener | a connection to accept | | | |
| TCP stream | data, or the peer's FIN | connected and send space | both directions closed | an error is pending (`SO_ERROR`) |
| TCP connecting | | when the connection finishes or fails | | if it failed |
| UDP | a datagram queued | always | | |
| socketpair end | data, or peer closed | space | peer closed | |
| `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/urandom`, `/dev/random` (once seeded) | always | always | | |

`POLLRDNORM` and `POLLWRNORM` mirror `POLLIN` and `POLLOUT`; `POLLPRI` is never set.
Readiness is level-triggered. Waiting uses one machine-wide readiness word that pipes, the
console, socketpairs and the network service all advance and wake on, then rechecks every
entry: simple and correct, and the place to optimise later (a waiter list per object)
if many pollers show up in profiles.

`select` and `pselect` are relibc over this: relibc's own `select` is written on `epoll`,
which feos doesn't have, so its feos build uses a `poll`-based `select` (stage 2 patch).

#### `/dev` and `/proc/self`

Nearly every program on the path opens `/dev/null` (std's `Stdio::null()` and its repair
of closed fds 0 to 2 at startup, Ninja's `posix_spawn_file_actions_addopen(0,
"/dev/null")` for every non-console command, LLVM's `Program.inc`, every `2>/dev/null` in
a script), and several fall back to `/dev/urandom` (LLVM, SQLite, OpenSSL, libgit2). LLVM's
`getMainExecutable` checks `exists("/proc/self/exe")` before it reads it, and CPython,
LLVM and the jobserver crate list `/proc/self/fd`. So from stage 1c.3 the kernel mounts two
synthesised filesystems, the same on both ports, that answer every path call consistently
(`OPENAT`, `FSTATAT`, `FACCESSAT`, `READLINKAT`, `GETDENTS`); anything that would change
them (create, unlink, rename, chmod) is `EPERM`, except inside `/dev/shm`.

| Path | `Stat2` | What it does |
|---|---|---|
| `/dev/null` | `S_IFCHR \| 0o666`, `rdev` `1 << 8 \| 3` | reads end of file, writes are discarded |
| `/dev/zero` | `S_IFCHR \| 0o666`, `1 << 8 \| 5` | reads zeroes, writes discarded; `MMAP2` of it is anonymous memory |
| `/dev/full` | `S_IFCHR \| 0o666`, `1 << 8 \| 7` | reads zeroes, writes `ENOSPC` |
| `/dev/random`, `/dev/urandom` | `S_IFCHR \| 0o666`, `1 << 8 \| 8` and `1 << 8 \| 9` | `GETRANDOM` with no flags and with `GRND_INSECURE`: `/dev/random` waits for seeding, `/dev/urandom` never does (stage 1e.1; until then both are `ENOENT`) |
| `/dev/tty` | `S_IFCHR \| 0o666`, `5 << 8 \| 0` | opening it gives a new OFD for the caller's terminal, the one its terminal-marked fds refer to; `ENXIO` if it has none (stage 1c.11; `ENXIO` until then) |
| `/dev/console` | `S_IFCHR \| 0o600`, `5 << 8 \| 1` | the console stream, as `FD_PATH` names it |
| `/dev/stdin`, `/dev/stdout`, `/dev/stderr` | links to `/proc/self/fd/0`, `1`, `2` | |
| `/dev/fd` | a link to `/proc/self/fd` | |
| `/dev/shm` | a RAM directory, `0o1777` | POSIX shared memory (relibc's `shm_open` opens files here); regular files that `MMAP2` can map shared and writable |
| `/proc/self` | a link to `/proc/<pid>` | |
| `/proc/<pid>/exe` | a link to `CURRENT_EXE`'s path | only the caller's own pid is visible (`ENOENT` for others) |
| `/proc/<pid>/fd/` | a directory listing the caller's open descriptors | each entry `N` is a link to `FD_PATH(N)` (a pipe or socket reads as `pipe:[n]` or `socket:[n]`), and opening it opens the file anew (a `/dev` or console node, a pipe end or a socket gives a new descriptor for the *same* OFD, as Linux does for those) |

`/proc` has nothing else: no `maps`, no `cpuinfo`, no other pids. libffi's closures (which
read `/proc/self/maps` and need writable executable memory) don't work under W^X anyway,
so CPython's `ctypes` callbacks are unsupported; nothing on the build path uses them.

### 3.4 Processes (1c)

#### 72 `SPAWN2`

```rust
fn spawn2(request: *const SpawnRequest) -> pid
```

Starts a program in a new process, in `posix_spawn`'s shape. The [`SpawnRequest`](#spawnrequest)
names the program by [`PathAt`](#pathat), the argument and environment strings (arrays of
`UserStr`, each environment string `KEY=VALUE`), a list of [`SpawnAction`](#spawnaction)s,
and attributes.

Done in the caller's call, before the child exists, so every failure is the call's
result and not a child's exit status:

1. Validate the request: reserved fields zero, `argc` at least 1 (`EINVAL`), at most 32768
   strings and `SPAWN_ARG_MAX` (1 MiB) of strings and pointers together (`E2BIG`), each
   string at most 128 KiB (`E2BIG`), UTF-8 is *not* required (environment values needn't
   be), no NUL inside one (`EINVAL`), at most 256 actions (`EINVAL`). Everything is copied
   once into staging frames taken through the caller's promise and charged to its limit,
   not the kernel heap (`ENOMEM`), so concurrent spawns can't exhaust the heap; the
   staging frames become the child's argument area.
2. Build the child's FD table: a copy of the caller's (sharing OFDs), then the actions in
   order, then every `cloexec` descriptor closed. Actions run against the child's table,
   cwd and umask: `CLOSE` (closing a descriptor that isn't open is ignored, as glibc and
   musl do; `EBADF` only for `fd` outside `0..MAX_FDS`), `DUP2` (`EBADF` if `fd` isn't open;
   `fd == to` clears `cloexec`, which is how POSIX 2024 passes a descriptor through),
   `OPEN` (`OPENAT` rules, result placed at `fd`), `CHDIR`, `FCHDIR`, `CLOSEFROM` (every
   descriptor from `fd` up, glibc's `addclosefrom_np`, CPython 3.13's spawn path), and
   `TCSETPGRP` (make the child's process group the foreground group of the terminal `fd`
   refers to, glibc's `addtcsetpgrp_np`, for shells; `ENOTTY`, `EPERM` as `TTY_SET_PGRP`;
   it runs after the group is set). The first failing action's error is the call's.
3. Resolve the program against the child's cwd: a regular file (`EACCES` for a directory),
   execute permission (`EACCES`), and a valid ELF for this machine or a `#!` script
   ([section 7.4](#74-scripts)); `ENOEXEC` otherwise. For a dynamic program its interpreter
   is resolved and checked the same way. The image's file is held open from here.
4. Take a task slot (`EAGAIN` if none is open to user programs, or the caller is at its
   cap of unreaped children), a pid, and the child's exit record in the caller.

Then the child is created with: the table from step 2; the child's cwd; the caller's umask,
uid and gid, ignored signals (handlers go back to default, since the image is new), signal
mask (unless `SETSIGMASK`); signals in `sigdefault` reset to default (`SETSIGDEF`);
process group and session as the caller's unless `SETPGROUP` (group `pgroup`, or the child's
own pid if `pgroup` is 0: `EPERM`/`ESRCH` rules as `SETPGID`) or `SETSID`. Not inherited:
handlers, alternate stack, pending signals, `fcntl` locks, mappings, the window surface,
other threads. The pid is returned. Building the address space happens in the child; if
that fails (out of memory) the child ends with status 126 (`EXIT_NOT_EXECUTABLE`).

`flags`: `SPAWN_RESETIDS` 1 (no effect: no set-user-ID), `SPAWN_SETPGROUP` 2,
`SPAWN_SETSIGDEF` 4, `SPAWN_SETSIGMASK` 8, `SPAWN_USEVFORK` 0x40 (no effect),
`SPAWN_SETSID` 0x80, and `SPAWN_EMPTY_PATH` 0x1000 (the program is `path.dirfd`'s file
itself, `fexecve`-like; `path.length` must be 0). These are glibc's and musl's bit values.
glibc's `SETSCHEDPARAM` 0x10 and `SETSCHEDULER` 0x20 are `EINVAL`: there are no scheduling
policies to set, and nothing on the path asks for them.

relibc can't translate its own values: it declares `POSIX_SPAWN_*` as bitflags numbered 1
to 7 in sequence (`src/header/spawn/spawn_attr.rs`), so `SETSIGDEF` (3) is already
`RESETIDS | SETPGROUP` when a C program ORs the constants, std's `SETSIGDEF | SETPGROUP`
is 3, and Ninja's `SETSIGMASK | SETSIGDEF | SETPGROUP` is 7, which is `SETSID`. So relibc's
feos headers, and the libc crate's `feos` module, define them with the values above, and
relibc's feos `spawn` passes them through unchanged; `posix_spawnattr_setflags` accepts
those eight bits (`USEVFORK` included, which Ninja tests with `#ifdef`).

A path with no `/` is *not* searched in `PATH`: `posix_spawnp` and `execvp` search in libc
and try each candidate.

v1's `SPAWN` (10) is `SPAWN2` with an empty environment and a child table of exactly
`{0: stdin_fd or inherited, 1: stdout_fd or inherited, 2: inherited stderr}`, as before, and
it keeps v1's error behaviour: a program that's missing or not executable gives a pid
whose child exits 127 or 126, a full table is `EBUSY`, and too many arguments
`ENAMETOOLONG`.

#### 73 `EXEC`

```rust
fn exec(request: *const ExecRequest) -> !
```

Replaces the calling process's image. The [`ExecRequest`](#execrequest) is a
`SpawnRequest` without actions or attributes. Steps 1 and 3 of `SPAWN2` run first, with
the same errors, and the call returns the error if one fails (a `-> !` call returns when
validation fails). Then, past the point of no return, holding the process's creation
mutex: every other thread is killed through the cooperative path ([2.1](#21-kernel-rules-the-calls-depend-on))
and the caller waits until each has *exited*, not merely left the CPU, since a sibling
blocked in a call still walks the old tables and holds pins and OFDs. A fresh `Mm` is
installed and the old one dropped (freed by the reaper once the last reference goes), the
surface goes, `cloexec` descriptors close, handlers return to default (ignored signals
stay ignored), the alternate stack goes, the signal mask and pending process signals stay,
`fcntl` locks stay (but those on the closed `cloexec` descriptors' files go), and the new
image is built. The caller keeps its tid: if it wasn't the first thread, `GETTID` and
`GETPID` now differ for a single-threaded process, which POSIX allows and nothing here
depends on. If building fails now, the process ends
by `SIGSEGV`, as Linux's `execve` does past its point of no return. pid, parent, process
group, session, cwd, umask and credentials are unchanged. `CURRENT_EXE` reports the new
program.

#### 74 `WAIT4`

```rust
fn wait4(pid: usize, status: *mut i32, options: usize, rusage: *mut Rusage) -> pid
```

`pid` (32-bit): positive for that child, `-1` for any child, `0` for any child in the
caller's group, below `-1` for any child in group `-pid`. `options`: `WNOHANG` 1 (return 0
if none has changed state), `WUNTRACED` 2 (also report a child that stopped, status
`0x7f | signal << 8`), `WCONTINUED` 8 (also report a child that continued, status
`0xffff`). Returns the child's pid, writing its status (if `status` isn't zero) in POSIX
encoding ([section 5.6](#56-wait-statuses)) and its resource use (if `rusage` isn't zero);
a stop or continue is reported once and doesn't reap. `ECHILD` if no child matches,
running or not; `EINTR` if a handler ran (restarted under `SA_RESTART`).

**Zombies.** A user process's children's exit records are allocated in the parent when
each child is created (step 4 of `SPAWN2`, or `FORK`), so a child's exit only fills its
record in, under the scheduler lock, without allocating. A task slot is never held by a
zombie, and a child is never lost to v1's 64-entry ring. A parent may have at most 1024
children not yet reaped, running or ended; past that `SPAWN2` and `FORK` fail with
`EAGAIN`. The record is complete before `SIGCHLD` is raised, so a `SIGCHLD` handler's
`WAIT4(WNOHANG)` (Ninja's, make's) always finds it. Records refer to the child by pid,
and a child's pid isn't reused until its record is reaped or dropped.

When a parent ends, its unreaped records go, and its living children have no parent
(`GETPPID` says 1, the shell, which doesn't reap them; their records are dropped when they
end). A parent whose `SIGCHLD` disposition is explicitly `SIG_IGN`, or set with
`SA_NOCLDWAIT`, keeps no records, and `WAIT4` then waits for all children and returns
`ECHILD`, as POSIX. The *default* disposition of `SIGCHLD` also ignores the signal, but
records are kept, as POSIX requires and Ninja and make rely on. Kernel-app parents (the
shell) keep using v1's ring and `Processes::wait(pid)`.

v1's `WAIT` (11) is `wait4(pid, ..., 0, 0)` returning the v1 status: the exit code, or
128 plus the signal for a process a signal ended (139 for a fault, as before).

#### 75 `GETPPID`, 76 `GETPGID`, 77 `SETPGID`, 78 `GETSID`, 79 `SETSID`

```rust
fn getppid() -> pid
fn getpgid(pid: usize) -> pgid        // 0 is the caller
fn setpgid(pid: usize, pgid: usize) -> 0
fn getsid(pid: usize) -> sid          // 0 is the caller
fn setsid() -> sid
```

Process groups and sessions as POSIX, without controlling-terminal acquisition (the
console's foreground group is feos-sh's to set, or `TTY_SET_PGRP`). `SETPGID`: `pid` 0 is
the caller, `pgid` 0 is `pid`; the target is the caller or a child of it in its session
(`ESRCH`; `EACCES` once the child has `EXEC`ed); a session leader can't move (`EPERM`);
`pgid` must be `pid` or an existing group in the same session (`EPERM`). `SETSID`: `EPERM`
if the caller is a group leader. `GETPGID`/`GETSID` of a process that doesn't exist:
`ESRCH`. A process started by a kernel app gets its own group and the session of the
terminal it was started in.

#### 80 `GETRUSAGE`

```rust
fn getrusage(who: usize, out: *mut Rusage) -> 0
```

`who` (32-bit): `RUSAGE_SELF` 0, `RUSAGE_CHILDREN` -1 (reaped children, and theirs, from
stage 1c.8; zero before), `RUSAGE_THREAD` 1. Fills `utime` with time in user mode and `stime` with time in calls and
faults, `maxrss` with the peak of owned pages in KiB, `minflt` and `majflt` (page-cache
misses), `nvcsw` and `nivcsw`. Other fields zero.

#### 81 `SYSINFO`

```rust
fn sysinfo(out: *mut SysInfo, size: usize) -> 0
```

Copies `min(size, size_of::<SysInfo>())` bytes of a [`SysInfo`](#sysinfo), so the struct
can grow at its end. relibc's `sysconf` (`_SC_NPROCESSORS_ONLN`, `_SC_NPROCESSORS_CONF`,
`_SC_PHYS_PAGES`, `_SC_AVPHYS_PAGES`, `_SC_ARG_MAX`, `_SC_OPEN_MAX`, `_SC_PAGESIZE`) and
`uname` (`release`, `machine`) read it, and std's `available_parallelism` reaches it
through `sysconf`.

### 3.5 Signals (1c)

The model is in [section 8](#8-signals-in-detail); these are the calls.

#### 82 `KILL`, 83 `THREAD_KILL`

```rust
fn kill(pid: usize, signal: usize) -> 0
fn thread_kill(tid: usize, signal: usize) -> 0
```

`KILL`: `pid` (32-bit): positive for a process, 0 for the caller's group, `-1` for every
user process the caller may signal except itself, below `-1` for group `-pid`. `signal`
0 checks only. Permitted if the caller is root or has the target's uid (`EPERM`); kernel
apps can't be signalled from user programs (`EPERM`). A child that has ended but isn't
reaped yet still exists: `KILL` on it succeeds and does nothing, as for a POSIX zombie
(std's `Child::kill` and Python's `send_signal` rely on that). `ESRCH` if nothing matches,
including a thread id that isn't a process id (a deviation: Linux signals that thread's
process; tids and pids share one number space, so they can't be confused). A signal sent
to a process is flagged on the process and taken by one of its threads ([8.3](#83-delivery)).
`THREAD_KILL`: a thread of the caller's own process (`ESRCH` otherwise), for `raise`,
`pthread_kill` and relibc's `rlct_kill` (thread cancellation).

v1's way of stopping a program (the shell's kill, `Processes::kill`) is unchanged; from
1c.8 the shell's `kill` builtin sends `SIGTERM`/`SIGKILL` through the same path, and the
shell sends `SIGINT` to a foreground user program on Ctrl-C (the console's own `ISIG`
arrives in 1c.11).

#### 84 `SIGACTION`

```rust
fn sigaction(signal: usize, new: *const SigAction, old: *mut SigAction) -> 0
```

Either pointer may be zero. `SIGKILL` and `SIGSTOP` can't be caught or ignored (`EINVAL`
with a `new`). A [`SigAction`](#sigaction) with a handler (anything but `SIG_DFL` 0 and
`SIG_IGN` 1) must have `SA_RESTORER` set (`EINVAL`), and both the handler and the
`restorer` must be addresses in the window (`EFAULT`). Flags: `SA_NOCLDSTOP`, `SA_NOCLDWAIT`,
`SA_SIGINFO` (always behaves as if set: the handler always gets three arguments, which a
one-argument handler ignores), `SA_ONSTACK`, `SA_RESTART`, `SA_NODEFER`, `SA_RESETHAND`,
`SA_RESTORER`; others `EINVAL`. `mask` may name `SIGKILL`/`SIGSTOP`, which are dropped.
Setting `SIG_IGN` on a pending signal discards it.

relibc's feos `sigaction` builds a fresh `SigAction` (its `sa_flags` is a `c_int`, so
`SA_RESETHAND` is negative there and must not be sign-extended into the 64-bit word) and
fills `restorer` with its `__restore_rt`, as its Linux one does.

#### 85 `SIGPROCMASK`

```rust
fn sigprocmask(how: usize, set: *const u64, old: *mut u64) -> 0
```

The calling thread's mask. `how`: `SIG_BLOCK` 0, `SIG_UNBLOCK` 1, `SIG_SETMASK` 2
(`EINVAL`; ignored when `set` is zero). `SIGKILL` and `SIGSTOP` are silently left
unblocked. A signal that becomes unblocked and is pending is delivered before the call
returns to the program.

#### 86 `SIGALTSTACK`

```rust
fn sigaltstack(new: *const SigStack, old: *mut SigStack) -> 0
```

The calling thread's alternate stack. `new.flags` is 0 or `SS_DISABLE` 2 (`EINVAL`);
`size` at least `MINSIGSTKSZ` (2048, `ENOMEM`); the range in the window (`EFAULT`).
`EPERM` while running on it. `old.flags` reports `SS_ONSTACK` 1 while on it.

#### 87 `SIGPENDING`, 88 `SIGSUSPEND`

```rust
fn sigpending(out: *mut u64) -> 0
fn sigsuspend(mask: *const u64) -> EINTR
```

`SIGPENDING`: signals pending for the thread or the process that the thread blocks.
`SIGSUSPEND`: replaces the thread's mask with `*mask` until a handler has run, then
restores it and returns `EINTR` (always).

#### 89 `SIGRETURN`

```rust
fn sigreturn(context: *const UContext) -> !
```

What a handler's `restorer` calls with the stack pointer it had at entry, which points at
the `UContext` ([section 8.4](#84-frames)). The kernel reads it and resumes with its
registers, mask (minus `SIGKILL`/`SIGSTOP`) and alternate-stack state. It trusts nothing in
it: see section 8.5 for what's checked. A bad context ends the process with `SIGSEGV`.
Calling it without a handler running is harmless: it's the same as a program setting its
own registers.

#### 108 `SETITIMER`

```rust
fn setitimer(which: usize, new: *const ITimerVal, old: *mut ITimerVal) -> 0
```

`ITIMER_REAL` 0 only (`ITIMER_VIRTUAL` 1 and `ITIMER_PROF` 2 are `EINVAL`): a per-process
deadline on the monotonic clock that raises `SIGALRM` on the process when it passes, then
re-arms by `interval` if that isn't zero. `new` zero only reads the current value into
`old` (`getitimer`); a zero `value` disarms. `usec` outside `0..1_000_000` is `EINVAL`.
Not inherited by `FORK` or `SPAWN2`; kept across `EXEC`, as POSIX. relibc's `alarm()` is
this, which configure test programs, coreutils `timeout` and Python's `signal.alarm`
need to be interrupted rather than hang.

`sigqueue` (as `KILL`, value dropped), `sigtimedwait` and the POSIX timers have no calls;
see the `Pal` table.

### 3.6 fork (1d)

#### 90 `FORK`

```rust
fn fork() -> pid   // 0 in the child
```

A new process whose only thread is a copy of the caller: same registers (the call returns
0 in it), TLS register, signal mask and alternate stack; no pending signals (a `SIGCHLD`
pending in the parent stays the parent's), no `clear_word` (the child's thread has none:
the parent's word belongs to a parent thread), no interval timer. Its tid is the child's
pid; relibc's feos `fork` refreshes the os tid cached in the child's TCB. The FD table is
copied, sharing OFDs (and so offsets and `flock` locks); `fcntl` locks aren't inherited.
Cwd, umask, dispositions, credentials, group, session and exe path are the parent's;
resource use starts at zero; the window surface isn't inherited (the child's is unmapped).
The parent's creation mutex is held throughout, so no thread is half created.

**The address space** is copied on write, under the parent's Mm lock:

- A private page that's present becomes read-only in both and copy-on-write, with its map
  count raised; the first writer copies. At map count 1 the writer may reuse the frame in
  place only if it's an anonymous frame with no pins: a page-cache frame (a private file
  page not yet written) is always copied, because writing it would change the file for
  every reader.
- A private writable page with pins (a kernel copy in flight in another parent thread) is
  copied for the child at once, so data the kernel writes through the pin after the fork
  can't show up in the child. A write pin on a copy-on-write page breaks the copy-on-write
  before it's taken, so the kernel never writes a shared frame.
- Shared mappings share their object (shared-anonymous or file).
- Reservations, `PROT_NONE` ranges and not-yet-touched pages are VMA entries only, and are
  copied as such.
- Page tables are copied at the fork. Other threads of the parent keep running; their TLB
  entries for pages that just became copy-on-write are shot down before the call returns.

**Charging is strict and follows frames, not processes.** Each extra reference a fork
adds to a writable private present page adds one promise, so the machine-wide debt is
always the sum over frames of (map count - 1) for such pages, plus every untouched charged
page. A copy-on-write copy consumes one; unmapping or exiting with a page whose count is
above 1 releases one; the last reference releases nothing more. So whichever process
writes first, its copy was promised, and a copy never fails later. `ENOMEM` if the fork's
promises can't all be taken (`fork-enomem`). Page tables copied for the child are charged
too. Per-process `USER_MEMORY_LIMIT` attribution is separate: the child is charged its
writable private footprint against its own limit, without touching the machine-wide
count twice. `EAGAIN` with no free slot or at the parent's cap of unreaped children.

Strict charging makes forking a process with gigabytes of heap expensive, so the tools
that would do it don't fork: cargo and rustc reach std's `fork` path only through the
`jobserver` crate's `pre_exec`, and its feos arm leaves the two jobserver descriptors
without `FD_CLOEXEC` from the start instead of clearing it in a `pre_exec` closure
([10.2](#102-std-and-the-libc-crate)), so they go through `posix_spawn`. The programs that do fork (CPython's `subprocess`, make, CMake's
libuv) are small.

`vfork` is `fork` in libc.

### 3.7 Time, randomness and network (1e)

#### 91 `CLOCK_SETTIME`

```rust
fn clock_settime(clock: usize, time: *const Timespec) -> 0
```

`CLOCK_REALTIME` only (`EINVAL` for the rest), root only (`EPERM`). Moves the wall clock's
offset; monotonic time is untouched. The RTC isn't written.

#### 92 `GETRANDOM`

```rust
fn getrandom(buffer: usize, length: usize, flags: usize) -> count
```

Bytes from the kernel's CSPRNG. `flags`: `GRND_NONBLOCK` 1, `GRND_RANDOM` 2 (the same
generator), `GRND_INSECURE` 4 (never waits; `EINVAL` with `GRND_RANDOM`). Up to `IO_CHUNK`
(1 MiB) per call; requests of 256 bytes or fewer are never short. Until the generator has
been seeded with 256 bits from trusted sources, the call waits (`EAGAIN` with
`GRND_NONBLOCK`, `EINTR` if a handler runs), except with `GRND_INSECURE`.

The generator is ChaCha20 with fast key erasure (each request takes a block to rekey, so
a later compromise can't recover earlier output), in a new host-tested crate
`libs/csprng`. It's seeded, and reseeded every 5 minutes or 1 GiB of output, from every
source the machine has:

| Source | Port | Trusted |
|---|---|---|
| `RDSEED`, else `RDRAND` (CPUID-checked, retried) | x86_64 | yes |
| `RNDR`/`RNDRRS` (`ID_AA64ISAR0_EL1.RNDR` non-zero: QEMU `-cpu max`, newer KVM hosts; not the default Cortex-A53) | AArch64 | yes |
| virtio-rng (`virtio-rng-device` on `virt`, `virtio-rng-pci` on q35): a new driver on `drivers/virtio/core`; the run scripts add the device (stage 1e.2) | both | yes |
| `/chosen/rng-seed` in the DTB (QEMU's `virt` fills 32 bytes) | AArch64 | yes |
| `EFI_RNG_PROTOCOL`, passed in the UEFI loader's `Handoff` (stage 1e.2) | both, UEFI boot | yes |
| counter jitter at interrupts, device timings | both | mixed in, never counted |

Boot logs which sources it found. With no trusted source at all (q35's default `qemu64`
CPU has no `RDRAND`/`RDSEED`, and no virtio-rng device) the generator never reaches 256
trusted bits, and boot says so loudly: every `GETRANDOM` without `GRND_INSECURE` would
wait forever. The run scripts and the boot test always add virtio-rng for that reason.
Seeding-independent users must ask for `GRND_INSECURE`: std's `HashMap` keys do (its
`hashmap_random_keys` uses `GRND_INSECURE` on Linux, and the feos arm does the same), and
so does `/dev/urandom`.

`AT_RANDOM`'s 16 bytes come from here; before stage 1e.1 they come from the counter, which
[section 7.2](#72-the-aux-vector) flags.

#### 93 `SOCKET`

```rust
fn socket(domain: usize, kind: usize, protocol: usize) -> fd
```

`domain` `AF_INET` 2 (`AF_UNIX` 1 only through `SOCKETPAIR`: `EAFNOSUPPORT`; `AF_INET6` 10:
`EAFNOSUPPORT`). `kind` `SOCK_STREAM` 1 (protocol 0 or `IPPROTO_TCP` 6) or `SOCK_DGRAM` 2
(0 or `IPPROTO_UDP` 17), with `SOCK_NONBLOCK` and `SOCK_CLOEXEC` or'd in; others
`ESOCKTNOSUPPORT`, a mismatched protocol `EPROTONOSUPPORT`. `EMFILE`; `ENOBUFS` when the
stack's sockets run out. A socket OFD owns its `libs/net` socket and closes it when the
last descriptor goes (the stack's orphan sweep by pid doesn't apply to it).

#### 94 `SOCKETPAIR`

```rust
fn socketpair(domain: usize, kind: usize, protocol: usize, fds: *mut [i32; 2]) -> 0
```

`AF_UNIX`, `SOCK_STREAM` (with `SOCK_NONBLOCK`/`SOCK_CLOEXEC`), protocol 0; `SOCK_DGRAM`
and `SOCK_SEQPACKET` are `EOPNOTSUPP`. Two connected, bidirectional byte streams, made of
two kernel pipes; `SHUTDOWN` closes a direction, `POLL` and `FD_NREAD` work. No ancillary
data. libuv (CMake) makes its child processes' stdio and its signal pipe with this.

#### 95 `BIND`, 96 `LISTEN`, 97 `ACCEPT`, 98 `CONNECT`

```rust
fn bind(fd: usize, address: *const SockAddrIn, length: usize) -> 0
fn listen(fd: usize, backlog: usize) -> 0
fn accept(fd: usize, address: *mut SockAddrIn, length: *mut u32, flags: usize) -> fd
fn connect(fd: usize, address: *const SockAddrIn, length: usize) -> 0
```

Addresses are [`SockAddrIn`](#sockaddrin), `length` at least 16 (`EINVAL`), family
`AF_INET` (`EAFNOSUPPORT`). `ENOTSOCK` for an fd that isn't a socket.

- `BIND`: the address must be `0.0.0.0`, `127.0.0.1` or the interface's
  (`EADDRNOTAVAIL`); port 0 picks one at `LISTEN` or the first send. `EADDRINUSE`;
  `EINVAL` if already bound. On a stream socket that then `CONNECT`s, only port 0 is
  supported for now (`EOPNOTSUPP` otherwise: `libs/net` picks client ports itself, and no
  client here binds one).
- `LISTEN`: a bound or unbound (port picked) stream socket; `backlog` clamped to
  `1..=32`. `EOPNOTSUPP` for UDP.
- `ACCEPT`: `flags` `SOCK_NONBLOCK`, `SOCK_CLOEXEC`. Blocks (`EAGAIN` when non-blocking,
  `EINTR`). Writes the peer's address if `address` isn't zero, `*length` updated to 16.
  `EINVAL` if not listening.
- `CONNECT`: stream sockets connect, blocking until established (`ECONNREFUSED`,
  `ETIMEDOUT`, `ENETUNREACH`, `ENETDOWN` without a lease, `EINTR`); non-blocking ones
  return `EINPROGRESS` and `POLLOUT` reports the end, with the result in `SO_ERROR`; a
  second call while in progress is `EALREADY`, after success `EISCONN`. UDP sockets record
  the default peer (and filter what they receive to it); family `AF_UNSPEC` 0 dissolves it.

Loopback (`127.0.0.0/8`) is new in `libs/net`: frames to it go straight back into the
stack's input without touching the device. std's own network tests, Python's and the
remote test server's local use need it.

#### 99 `SENDTO`, 100 `RECVFROM`

```rust
fn sendto(fd: usize, buffer: usize, length: usize, flags: usize, to: *const SockAddrIn, to_length: usize) -> count
fn recvfrom(fd: usize, buffer: usize, length: usize, flags: usize, from: *mut SockAddrIn, from_length: *mut u32) -> count
```

`flags`: `MSG_DONTWAIT` 0x40 (non-blocking for this call), `MSG_NOSIGNAL` 0x4000 (no
`SIGPIPE` on a broken stream), `MSG_PEEK` 2 and `MSG_WAITALL` 0x100 (receive only);
`MSG_OOB` 1 is `EOPNOTSUPP`; others `EINVAL`. Streams: `to` must be zero (`EISCONN`
otherwise) and `ENOTCONN` before connecting; a send on a stream the peer closed fails with
`EPIPE` and raises `SIGPIPE` unless `MSG_NOSIGNAL`; a receive returns 0 at the peer's FIN.
UDP: `to` is needed unless connected (`EDESTADDRREQ`); a datagram over 65507 bytes is
`EMSGSIZE`; a receive into a short buffer truncates the datagram (and drops the rest), and
writes the sender if `from` isn't zero. Timeouts from `SO_RCVTIMEO`/`SO_SNDTIMEO` end a
blocking call with `EAGAIN`. `READ` and `WRITE` work on sockets too, as receive and send
with no flags.

`sendmsg` and `recvmsg` are relibc over these, gathering and scattering `iovec`s through a
buffer; control data isn't supported (`msg_controllen` must be 0 on send, `EOPNOTSUPP`;
it's always 0 on receive).

#### 101 `SHUTDOWN`

```rust
fn shutdown(fd: usize, how: usize) -> 0
```

`SHUT_RD` 0, `SHUT_WR` 1, `SHUT_RDWR` 2 (`EINVAL`). `ENOTCONN` on an unconnected stream.
For TCP, `SHUT_WR` sends a FIN once the queue drains (`tcp_shutdown`).

#### 102 `GETSOCKOPT`, 103 `SETSOCKOPT`

```rust
fn getsockopt(fd: usize, level: usize, name: usize, value: usize, length: *mut u32) -> 0
fn setsockopt(fd: usize, level: usize, name: usize, value: usize, length: usize) -> 0
```

Values are `i32` unless the table says otherwise; `EINVAL` if `length` is too small.
Unknown options: `ENOPROTOOPT`.

| Level | Option | Get | Set | Effect |
|---|---|---|---|---|
| `SOL_SOCKET` 1 | `SO_REUSEADDR` 2 | yes | yes | stored; `LISTEN` on a port in `TIME_WAIT` is allowed regardless |
| | `SO_TYPE` 3 | yes | | `SOCK_STREAM`/`SOCK_DGRAM` |
| | `SO_ERROR` 4 | yes | | the pending error as an errno, then cleared |
| | `SO_BROADCAST` 6 | yes | yes | UDP to `255.255.255.255` allowed |
| | `SO_SNDBUF` 7, `SO_RCVBUF` 8 | yes | yes (no effect) | the stack's fixed sizes |
| | `SO_KEEPALIVE` 9 | yes | yes | stored; no keepalive probes yet |
| | `SO_LINGER` 13 (`Linger`) | yes | yes | stored; zero linger closes with a reset |
| | `SO_RCVTIMEO` 20, `SO_SNDTIMEO` 21 (`Timeval`) | yes | yes | timeouts for blocking calls |
| | `SO_ACCEPTCONN` 30 | yes | | listening or not |
| | `SO_DOMAIN` 39, `SO_PROTOCOL` 38 | yes | | |
| `IPPROTO_TCP` 6 | `TCP_NODELAY` 1 | yes | yes | stored; the stack sends at once anyway |
| | `TCP_KEEPIDLE` 4, `TCP_KEEPINTVL` 5, `TCP_KEEPCNT` 6 | yes | yes | stored |
| `IPPROTO_IP` 0 | `IP_TTL` 2 | yes | yes | the TTL of what this socket sends |

#### 104 `GETSOCKNAME`, 105 `GETPEERNAME`

```rust
fn getsockname(fd: usize, address: *mut SockAddrIn, length: *mut u32) -> 0
fn getpeername(fd: usize, address: *mut SockAddrIn, length: *mut u32) -> 0
```

The local and remote ends; `ENOTCONN` for a peer that isn't there. A socketpair end
reports family `AF_UNIX` only, with `*length` set to 2.

#### 106 `RESOLVE`

```rust
fn resolve(name: usize, name_length: usize, out: *mut [u8; 4], max: usize, timeout_ms: usize) -> count
```

IPv4 addresses for a host name, through `libs/net`'s DNS client (`Network::resolve`, which
retries and uses the name server DHCP gave). `name` up to 253 bytes of a valid host name
(`EINVAL`); a dotted quad is returned as it is; `localhost` is `127.0.0.1`. Writes up to
`max` (at least 1, `EINVAL`) addresses and returns how many. `ENOENT` for a name that
doesn't exist, `ETIMEDOUT`, `ENETDOWN` without a lease, `EINTR`. `timeout_ms` 0 means the
stack's default.

relibc's feos `lookup_host` calls this instead of reading `/etc/resolv.conf` and asking a
name server over UDP itself (which would also work, through `SOCKET`/`CONNECT` with
`SO_RCVTIMEO`, if `/etc/resolv.conf` is written). `/etc/hosts` stays relibc's.

## 4. Structures

Every structure is in `libs/abi`, with its size and offsets asserted. Offsets are in bytes.
`i64` time fields keep the layouts the same as 64-bit Linux's where relibc can then copy
them straight through. Each struct is marked **request** (reserved fields must be zero),
**Linux layout** (padding ignored on input, zero on output) or **output** (written by the
kernel, reserved fields zero), as [section 1](#1-rules-every-call-follows) defines. Every
fd field is validated as an `i32`, whatever its width.

### `PathAt`

24 bytes, request. A path relative to a directory, for calls with two paths and for
requests.

| Offset | Field | Type |
|---|---|---|
| 0 | `dirfd` | `i32` (`AT_FDCWD` or an fd) |
| 4 | `reserved` | `u32`, 0 |
| 8 | `pointer` | `u64` |
| 16 | `length` | `u64` |

### `Timespec`

16 bytes, Linux layout: `sec: i64` at 0, `nsec: i64` at 8 (`0..1_000_000_000`; the
whole `i64` is checked, since 64-bit Linux's `tv_nsec` is a `long` with no padding).

### `Timeval`

16 bytes, Linux layout: `sec: i64` at 0, `usec: i64` at 8. For socket timeouts, `Rusage`
and `ITimerVal`.

### `ITimerVal`

32 bytes, request: `interval: Timeval` 0, `value: Timeval` 16, Linux's `struct
itimerval`.

### `Stat2`

144 bytes, output.

| Offset | Field | Type | |
|---|---|---|---|
| 0 | `dev` | `u64` | the mount's number (new in the VFS's `Stat`) |
| 8 | `ino` | `u64` | stable for the file's life, never reused while referenced ([section 2](#2-what-the-kernel-keeps-in-v2)) |
| 16 | `mode` | `u32` | type and permission bits, `S_IF*` |
| 20 | `nlink` | `u32` | |
| 24 | `uid` | `u32` | |
| 28 | `gid` | `u32` | |
| 32 | `rdev` | `u64` | 0, or a `/dev` node's number ([`/dev`](#dev-and-procself)): the console `5 << 8 \| 1` |
| 40 | `size` | `i64` | a link's target length for a link |
| 48 | `blksize` | `i64` | the filesystem's cluster or block size, 4096 at least |
| 56 | `blocks` | `i64` | 512-byte units allocated |
| 64 | `atime` | `Timespec` | |
| 80 | `mtime` | `Timespec` | |
| 96 | `ctime` | `Timespec` | status change (the VFS's `NodeTimes` gains `changed` in 1c.4); `mtime` where the filesystem doesn't keep one |
| 112 | `btime` | `Timespec` | creation, if `flags & STAT_BTIME` |
| 128 | `flags` | `u32` | `STAT_POSIX` 1 (owners and modes enforced, as v1's `posix`), `STAT_BTIME` 2, `STAT_READ_ONLY` 4 (the mount) |
| 132 | `reserved0` | `u32` | 0 |
| 136 | `reserved1` | `u64` | 0 |

Times the filesystem doesn't have are zero. v1's `Stat` (for `STAT`, 24) is unchanged.

### `Dirent`

A record in `GETDENTS`'s buffer: the layout of Linux's `linux_dirent64`, which is
relibc's `dirent` header, so relibc's `dent_reclen_offset` reads it as its Linux backend
does.

| Offset | Field | Type | |
|---|---|---|---|
| 0 | `ino` | `u64` | |
| 8 | `off` | `i64` | cookie for the next entry |
| 16 | `reclen` | `u16` | this record's length, a multiple of 8 |
| 18 | `kind` | `u8` | `DT_UNKNOWN` 0, `DT_FIFO` 1, `DT_CHR` 2, `DT_DIR` 4, `DT_REG` 8, `DT_LNK` 10, `DT_SOCK` 12 |
| 19 | `name` | bytes | NUL-terminated, up to `NAME_MAX` (255) bytes before the NUL, then zero padding to `reclen` |

### `StatVfs`

88 bytes, all `u64`: `bsize` 0, `frsize` 8, `blocks` 16, `bfree` 24, `bavail` 32, `files`
40, `ffree` 48, `favail` 56, `fsid` 64 (the `dev`), `flags` 72 (`ST_RDONLY` 1, `ST_NOSUID`
2 always), `namemax` 80 (255). Block counts are in `frsize` units.

### `Flock`

32 bytes, Linux layout, Linux's `struct flock` on 64-bit:

| Offset | Field | Type |
|---|---|---|
| 0 | `kind` | `i16` (`F_RDLCK` 0, `F_WRLCK` 1, `F_UNLCK` 2) |
| 2 | `whence` | `i16` |
| 4 | `reserved0` | `u32` (ignored on input, 0 on output) |
| 8 | `start` | `i64` |
| 16 | `length` | `i64` |
| 24 | `pid` | `i32` (output) |
| 28 | `reserved1` | `u32` (ignored on input, 0 on output) |

### `PollFd`

8 bytes, Linux layout: `fd: i32` at 0, `events: i16` at 4, `revents: i16` at 6.

### `Termios`

60 bytes, Linux layout: `iflag: u32` 0, `oflag: u32` 4, `cflag: u32` 8, `lflag: u32` 12,
`line: u8` 16, `cc: [u8; 32]` 17, `reserved: [u8; 3]` 49 (C's implicit padding: ignored),
`ispeed: u32` 52, `ospeed: u32` 56. Flag and `cc` index values are Linux's generic ones (relibc's).

### `WinSize`

8 bytes, Linux layout: `rows: u16` 0, `columns: u16` 2, `x_pixels: u16` 4, `y_pixels: u16` 6.

### `SpawnRequest`

128 bytes, request.

| Offset | Field | Type | |
|---|---|---|---|
| 0 | `path` | `PathAt` | the program |
| 24 | `argv` | `u64` | pointer to `argc` `UserStr`s |
| 32 | `argc` | `u64` | |
| 40 | `envp` | `u64` | pointer to `envc` `UserStr`s, `KEY=VALUE` |
| 48 | `envc` | `u64` | |
| 56 | `actions` | `u64` | pointer to `action_count` `SpawnAction`s |
| 64 | `action_count` | `u64` | |
| 72 | `flags` | `u64` | `SPAWN_*` |
| 80 | `pgroup` | `i64` | with `SPAWN_SETPGROUP` |
| 88 | `sigmask` | `u64` | with `SPAWN_SETSIGMASK`; bit `n - 1` is signal `n` |
| 96 | `sigdefault` | `u64` | with `SPAWN_SETSIGDEF` |
| 104 | `reserved` | `[u64; 3]` | 0 |

### `SpawnAction`

48 bytes, request.

| Offset | Field | Type | |
|---|---|---|---|
| 0 | `kind` | `u32` | `CLOSE` 1, `DUP2` 2, `OPEN` 3, `CHDIR` 4, `FCHDIR` 5, `CLOSEFROM` 6, `TCSETPGRP` 7 |
| 4 | `fd` | `i32` | `CLOSE`, `FCHDIR`, `TCSETPGRP`: the fd; `CLOSEFROM`: the lowest to close; `DUP2`: from; `OPEN`: where to put it |
| 8 | `to` | `i32` | `DUP2`: to |
| 12 | `open_flags` | `u32` | `OPEN` |
| 16 | `mode` | `u32` | `OPEN` |
| 20 | `reserved` | `u32` | 0 |
| 24 | `path` | `PathAt` | `OPEN`, `CHDIR` |

Fields a kind doesn't use must be zero.

### `ExecRequest`

80 bytes, request: `path: PathAt` 0, `argv: u64` 24, `argc: u64` 32, `envp: u64` 40, `envc: u64` 48,
`flags: u64` 56 (`SPAWN_EMPTY_PATH` only), `reserved: [u64; 2]` 64.

### `Rusage`

144 bytes, output, Linux's `struct rusage` on 64-bit: `utime: Timeval` 0, `stime: Timeval` 16,
then fourteen `i64`s from 32: `maxrss`, `ixrss`, `idrss`, `isrss`, `minflt`, `majflt`,
`nswap`, `inblock`, `oublock`, `msgsnd`, `msgrcv`, `nsignals`, `nvcsw`, `nivcsw`.

### `SysInfo`

200 bytes, output.

| Offset | Field | Type | |
|---|---|---|---|
| 0 | `abi_version` | `u32` | 2 |
| 4 | `page_size` | `u32` | 4096 |
| 8 | `cpus_online` | `u32` | |
| 12 | `cpus_configured` | `u32` | |
| 16 | `total_pages` | `u64` | RAM in pages |
| 24 | `free_pages` | `u64` | free and not promised |
| 32 | `uptime_ns` | `u64` | |
| 40 | `arg_max` | `u64` | `SPAWN_ARG_MAX` |
| 48 | `fd_max` | `u32` | `MAX_FDS` |
| 52 | `task_max` | `u32` | task slots, threads included |
| 56 | `path_max` | `u32` | `PATH_MAX` |
| 60 | `name_max` | `u32` | 255 |
| 64 | `memory_limit` | `u64` | per-process limit in bytes |
| 72 | `release` | `[u8; 64]` | NUL-padded, the kernel's version |
| 136 | `machine` | `[u8; 16]` | `x86_64` or `aarch64` |
| 152 | `flags` | `u64` | `SYSINFO_NO_RTC` 1 (wall clock started at the epoch), `SYSINFO_RNG_SEEDED` 2, `SYSINFO_RNG_TRUSTED` 4 (a trusted source exists, so seeding will finish) |
| 160 | `features` | `u64` | the stage feature mask ([section 1](#1-rules-every-call-follows), [section 12](#12-implementation-stages)) |
| 168 | `reserved` | `[u64; 4]` | 0 |

### `SigAction`

32 bytes, Linux layout: `handler: u64` 0 (`SIG_DFL` 0, `SIG_IGN` 1, or
an address), `flags: u32` 8 (the `SA_*` bits, all below bit 32), `reserved: u32` 12
(ignored on input, since a C `sa_flags` is followed by padding; 0 on output), `restorer:
u64` 16, `mask: u64` 24.

### `SigStack`

24 bytes, Linux layout, Linux's `stack_t`: `sp: u64` 0, `flags: i32` 8, `reserved: u32` 12
(C's implicit padding: ignored on input, 0 on output), `size: u64` 16.

### `SigInfo`

128 bytes, output, Linux's `siginfo_t` layout on 64-bit: `signo: i32` 0, `errno: i32` 4 (0),
`code: i32` 8, `reserved: i32` 12, then a union from 16:

| Signal | Fields from 16 |
|---|---|
| sent by `KILL`/`THREAD_KILL` (`SI_USER` 0, `SI_TKILL` -6) | `pid: i32` 16, `uid: u32` 20 |
| `SIGCHLD` (`CLD_EXITED` 1, `CLD_KILLED` 2, `CLD_STOPPED` 5, `CLD_CONTINUED` 6) | `pid: i32` 16, `uid: u32` 20, `status: i32` 24, `reserved: i32` 28, `utime: i64` 32, `stime: i64` 40 (clock ticks of 10 ms) |
| `SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGFPE`, `SIGTRAP` | `address: u64` 16 |
| raised by the kernel (`SIGPIPE`, `SIGINT` from the console: `SI_KERNEL` 0x80) | nothing |

Codes: `SEGV_MAPERR` 1, `SEGV_ACCERR` 2; `BUS_ADRALN` 1, `BUS_ADRERR` 2; `ILL_ILLOPC` 1;
`FPE_INTDIV` 1; `TRAP_BRKPT` 1. The rest of the 128 bytes is zero.

### `SockAddrIn`

16 bytes, Linux layout, BSD's `sockaddr_in`: `family: u16` 0 (native order), `port: u16`
2 (network order), `address: [u8; 4]` 4, `zero: [u8; 8]` 8 (`sin_zero`: ignored on input,
as Linux does, 0 on output).

### `Linger`

8 bytes, Linux layout: `on: i32` 0, `seconds: i32` 4.

## 5. Constants

All in `libs/abi`, in modules named after their prefix (`abi::o::CREAT`, `abi::map::FIXED`,
and so on), with the C names in the doc comments for the libc side.

### 5.1 Error numbers

Each stage adds to `libs/abi` the error numbers it is the first to return (so `ENODEV`
arrives in 1a.3, `ETXTBSY` and `EOVERFLOW` in 1a.4, `EINTR` in 1b.3; the cooperative kill
of 1a.2 unwinds with an internal `Killed`, never an errno a program sees), and `make test-abi` asserts every one
against the Linux value. New, all Linux's numbers: `ESRCH` 3, `EINTR` 4, `ENXIO` 6, `E2BIG` 7, `EXDEV` 18,
`ENODEV` 19, `ENOTDIR` 20, `ENFILE` 23, `ENOTTY` 25, `ETXTBSY` 26, `EFBIG` 27, `ESPIPE` 29,
`EROFS` 30, `EMLINK` 31, `ERANGE` 34, `EDEADLK` 35, `ENOLCK` 37, `ENOTEMPTY` 39, `ELOOP`
40, `EOVERFLOW` 75, `ENOTSOCK` 88, `EDESTADDRREQ` 89, `EMSGSIZE` 90, `EPROTOTYPE` 91,
`EILSEQ` 84, `ENOPROTOOPT` 92, `EPROTONOSUPPORT` 93, `ESOCKTNOSUPPORT` 94, `EOPNOTSUPP` 95 (= v1's
`ENOTSUP`; this document only uses the name `EOPNOTSUPP`), `EAFNOSUPPORT` 97, `EADDRINUSE` 98, `EADDRNOTAVAIL` 99, `ENETDOWN` 100,
`ENETUNREACH` 101, `ECONNABORTED` 103, `ECONNRESET` 104, `ENOBUFS` 105, `EISCONN` 106,
`ENOTCONN` 107, `ECONNREFUSED` 111, `EHOSTUNREACH` 113, `EALREADY` 114, `EINPROGRESS`
115. `EWOULDBLOCK` is `EAGAIN`.

### 5.2 Files

| Group | Values |
|---|---|
| `O_*` | `RDONLY` 0, `WRONLY` 1, `RDWR` 2, `ACCMODE` 3, `CREAT` 0o100, `EXCL` 0o200, `NOCTTY` 0o400, `TRUNC` 0o1000, `APPEND` 0o2000, `NONBLOCK` 0o4000, `DSYNC` 0o10000, `ASYNC` 0o20000, `DIRECT` 0o40000, `LARGEFILE` 0o100000, `DIRECTORY` 0o200000, `NOFOLLOW` 0o400000, `NOATIME` 0o1000000, `CLOEXEC` 0o2000000, `SYNC` 0o4010000, `PATH` 0o10000000, `TMPFILE` 0o20200000 |
| `AT_*` | `FDCWD` -100, `SYMLINK_NOFOLLOW` 0x100, `REMOVEDIR` 0x200, `EACCESS` 0x200 (`FACCESSAT` only), `SYMLINK_FOLLOW` 0x400, `NO_AUTOMOUNT` 0x800, `EMPTY_PATH` 0x1000 |
| `S_IF*` | `SOCK` 0o140000, `LNK` 0o120000, `REG` 0o100000, `BLK` 0o60000, `DIR` 0o40000, `CHR` 0o20000, `FIFO` 0o10000 |
| access | `F_OK` 0, `X_OK` 1, `W_OK` 2, `R_OK` 4 |
| `F_*` | `DUPFD` 0, `GETFD` 1, `SETFD` 2, `GETFL` 3, `SETFL` 4, `GETLK` 5, `SETLK` 6, `SETLKW` 7, `DUPFD_CLOEXEC` 1030; `FD_CLOEXEC` 1; `RDLCK` 0, `WRLCK` 1, `UNLCK` 2 |
| `LOCK_*` | `SH` 1, `EX` 2, `NB` 4, `UN` 8 |
| `RENAME_*` | `NOREPLACE` 1, `EXCHANGE` 2, `WHITEOUT` 4 |
| `UTIME_*` | `NOW` 0x3fff_ffff, `OMIT` 0x3fff_fffe |
| `FSYNC_DATA` | 1 |
| `CLOSE_RANGE_*` | `UNSHARE` 2 (refused), `CLOEXEC` 4 |
| `KEEP` | `usize::MAX` (v1), and `-1` as a 32-bit argument: leave an owner alone |
| limits | `PATH_MAX` 4096 (was 192), `NAME_MAX` 255, `MAX_FDS` 1024 (was 16), `PIPE_BUF` 4096 |

### 5.3 Memory

`PROT_NONE` 0, `PROT_READ` 1, `PROT_WRITE` 2, `PROT_EXEC` 4 (v1's `prot` gains `EXEC`).
`MAP_SHARED` 1, `MAP_PRIVATE` 2, `MAP_SHARED_VALIDATE` 3, `MAP_FIXED` 0x10, `MAP_ANONYMOUS`
0x20, `MAP_NORESERVE` 0x4000, `MAP_POPULATE` 0x8000, `MAP_STACK` 0x20000,
`MAP_FIXED_NOREPLACE` 0x100000. `MAP_FAILED` doesn't exist: errors are negative results.
`MADV_*` and `MS_*` as in 3.1.

### 5.4 Signals

Linux's numbers, the same on both ports: `SIGHUP` 1, `SIGINT` 2, `SIGQUIT` 3, `SIGILL` 4,
`SIGTRAP` 5, `SIGABRT` 6, `SIGBUS` 7, `SIGFPE` 8, `SIGKILL` 9, `SIGUSR1` 10, `SIGSEGV` 11,
`SIGUSR2` 12, `SIGPIPE` 13, `SIGALRM` 14, `SIGTERM` 15, `SIGSTKFLT` 16, `SIGCHLD` 17,
`SIGCONT` 18, `SIGSTOP` 19, `SIGTSTP` 20, `SIGTTIN` 21, `SIGTTOU` 22, `SIGURG` 23,
`SIGXCPU` 24, `SIGXFSZ` 25, `SIGVTALRM` 26, `SIGPROF` 27, `SIGWINCH` 28, `SIGIO` 29,
`SIGPWR` 30, `SIGSYS` 31, real-time 32 to 64 (relibc reserves 33 and 34 for itself). A
signal set is a `u64` with bit `n - 1` for signal `n`.

`SA_NOCLDSTOP` 1, `SA_NOCLDWAIT` 2, `SA_SIGINFO` 4, `SA_RESTORER` 0x0400_0000, `SA_ONSTACK`
0x0800_0000, `SA_RESTART` 0x1000_0000, `SA_NODEFER` 0x4000_0000, `SA_RESETHAND`
0x8000_0000. `SIG_BLOCK` 0, `SIG_UNBLOCK` 1, `SIG_SETMASK` 2. `SS_ONSTACK` 1, `SS_DISABLE`
2, `MINSIGSTKSZ` 2048, `SIGSTKSZ` 8192. `ITIMER_REAL` 0, `ITIMER_VIRTUAL` 1, `ITIMER_PROF` 2.

### 5.5 Time, randomness, processes

`CLOCK_*` as in 3.2; `TIMER_ABSTIME` 1. `GRND_NONBLOCK` 1, `GRND_RANDOM` 2,
`GRND_INSECURE` 4. `WNOHANG` 1, `WUNTRACED` 2, `WCONTINUED` 8. `RUSAGE_SELF` 0,
`RUSAGE_CHILDREN` -1, `RUSAGE_THREAD` 1. `SPAWN_RESETIDS` 1, `SPAWN_SETPGROUP` 2,
`SPAWN_SETSIGDEF` 4, `SPAWN_SETSIGMASK` 8, `SPAWN_USEVFORK` 0x40, `SPAWN_SETSID` 0x80,
`SPAWN_EMPTY_PATH` 0x1000; `SPAWN_ARG_MAX` 1 MiB. Spawn action kinds as in `SpawnAction`.

### 5.6 Wait statuses

POSIX encoding, as Linux's (relibc's `W*` macros work unchanged): an exit with code `c` is
`(c & 0xff) << 8`; an end by signal `s` is `s` (bit 7, the core flag, is never set: there
are no core dumps); a stop by signal `s` (reported only with `WUNTRACED`) is `0x7f | s << 8`;
a continue (only with `WCONTINUED`) is `0xffff`. `WIFEXITED(x)` is `x & 0x7f == 0`. The
kernel keeps a status as an enum and encodes it per call: v1's `WAIT` and the shell see
`c`, or `128 + s`.

### 5.7 Sockets and polling

`AF_UNSPEC` 0, `AF_UNIX` 1, `AF_INET` 2, `AF_INET6` 10. `SOCK_STREAM` 1, `SOCK_DGRAM` 2,
`SOCK_SEQPACKET` 5, `SOCK_NONBLOCK` 0o4000, `SOCK_CLOEXEC` 0o2000000. `IPPROTO_IP` 0,
`IPPROTO_TCP` 6, `IPPROTO_UDP` 17. `SHUT_RD` 0, `SHUT_WR` 1, `SHUT_RDWR` 2. `MSG_OOB` 1,
`MSG_PEEK` 2, `MSG_DONTWAIT` 0x40, `MSG_WAITALL` 0x100, `MSG_NOSIGNAL` 0x4000. Options as in
the table in 3.7. `POLLIN` 1, `POLLPRI` 2, `POLLOUT` 4, `POLLERR` 8, `POLLHUP` 0x10,
`POLLNVAL` 0x20, `POLLRDNORM` 0x40, `POLLWRNORM` 0x100.

## 6. Address-space layout

The window grows to the whole of the 3-level tree `libs/vm` walks: 64 GiB to 512 GiB.
Both ports already give a process a whole 512 GiB top-level entry (AArch64's 39-bit
`TTBR0` table, x86_64's PML4 entry 0 with its own PDPT), and the kernel's identity map
stays below `USER_BASE` (stage 1a.6 adds a boot check that RAM and every mapped device
region end below it, and stops the machine with a clear message if not). All of this is
stage 1a.6, which is also when `ABI_VERSION` becomes 2.

| Constant | v1 | v2 | |
|---|---|---|---|
| `USER_BASE` | `0x10_0000_0000` | same | lowest user address |
| program image | `USER_BASE..USER_IMAGE_END` | same, 4 GiB | `ET_EXEC` programs link here (as `user/link.ld` does) |
| `USER_PIE_BASE` | | `0x10_0000_0000` | load bias of an `ET_DYN` program |
| `USER_INTERP_BASE` | | `0x11_0000_0000` | load bias of the interpreter (1 GiB area) |
| `USER_MMAP_BASE` | `0x11_0000_0000` (`USER_IMAGE_END`) | `0x11_4000_0000` | where `MMAP2` looks first |
| `USER_MMAP_END` | `USER_STACK_TOP - stack - guard` | `USER_STACK_TOP - USER_ARGS_SIZE - USER_STACK_SIZE - USER_STACK_GUARD` (`0x7f_bf5f_f000`) | derived, so `the_layout_fits_together`'s equality still holds |
| `USER_STACK_TOP` | `0x13_ffff_f000` | `0x7f_ffff_f000` | one past the argument area, an unmapped page above |
| `USER_ARGS_SIZE` | | 2 MiB | the argument area: the initial stack block of [7.1](#71-the-initial-stack-and-registers) (argc to the strings, at most `SPAWN_ARG_MAX` of strings and pointers plus the aux vector and `UserStr`s), reserved separately so it never eats into the stack below |
| `USER_STACK_SIZE` | 128 KiB, mapped at once | 8 MiB, reserved and charged, given frames on first touch | the main thread's stack, directly below the argument area: the full 8 MiB is free at `main` whatever the arguments, which clang's cc1 (it notes the stack bottom at `main` and assumes `DesiredStackSize`) and rustc's main thread rely on |
| `USER_STACK_GUARD` | 16 MiB | 1 GiB | unmapped below the stack |
| `USER_END` | `0x14_0000_0000` | `0x80_0000_0000` | one past the highest user address |
| `USER_MEMORY_LIMIT` | 4 GiB | 64 GiB | most memory one process may have charged or owned (from 1a.3: promised, committed and page tables; before 1a.3, mapped); the machine says `ENOMEM` sooner |

`RLIMIT_STACK` is reported as 8 MiB, soft and hard: the main stack doesn't grow, and
`setrlimit` to more is `EPERM` (to 8 MiB or less, it's accepted with no effect). A program
wanting a bigger stack runs its work on a thread, as clang and rustc already do when the
limit is too low.

There's no address randomisation in phase 1 (debugging and reproducibility matter more
now); a later `SPAWN` flag can add a random bias from the CSPRNG.

A window beyond 512 GiB needs a fourth level in `libs/vm` and a second top-level entry; it
isn't needed for this plan.

## 7. Starting a program

### 7.1 The initial stack and registers

Every program started by `SPAWN2`, `EXEC` (and so by v1's `SPAWN` and the shell) gets the
System V initial stack, which relibc's `_start` and `ld.so` read, and which the C runtime
of any ELF toolchain expects. From the stack pointer up:

```
sp ->   argc                      u64
        argv[0] .. argv[argc-1]   pointers to NUL-terminated strings
        0
        envp[0] .. envp[n-1]      pointers to NUL-terminated strings
        0
        auxv pairs                (type: u64, value: u64), ending with (AT_NULL, 0)
        UserStr[argc]             feos's own (pointer, length) for each argument
        padding
        16 random bytes           AT_RANDOM points here
        strings                   argv, envp, AT_EXECFN, AT_FEOS_EXE, AT_PLATFORM, NUL-terminated
        unmapped page             USER_STACK_TOP
```

All of it lies in the argument area (`USER_STACK_TOP - USER_ARGS_SIZE` up), and the 8 MiB
stack starts below the `argc` word.

`sp` is 16-byte aligned. Registers: x86_64 `rdi` = argc, `rsi` = the `UserStr` array, `rdx`
= 0 (System V's "function for `atexit`", none), everything else zero; AArch64 `x0` =
argc, `x1` = the `UserStr` array, everything else zero. So programs written for v1's
convention (argc and `UserStr`s in the first two argument registers, which `user/`'s
runtime reads) work unchanged, and relibc's `_start`, which ignores registers and reads
the stack, works too. Floating point is in its initial state, the TLS register is zero,
and no signals are blocked unless `SPAWN2` said so. Strings are copied as given, not
required to be UTF-8 (`UserStr` lengths count bytes).

### 7.2 The aux vector

| Type | Value |
|---|---|
| `AT_PHDR` 3 | address of the program's program headers (from `PT_PHDR`, or the first `PT_LOAD` that covers `e_phoff`; `ENOEXEC` if none does) |
| `AT_PHENT` 4 | 56 |
| `AT_PHNUM` 5 | program header count |
| `AT_PAGESZ` 6 | 4096 |
| `AT_BASE` 7 | the interpreter's load bias, or 0 |
| `AT_FLAGS` 8 | 0 |
| `AT_ENTRY` 9 | the program's entry, biased |
| `AT_UID` 11, `AT_EUID` 12, `AT_GID` 13, `AT_EGID` 14 | credentials |
| `AT_PLATFORM` 15 | pointer to `"x86_64"` or `"aarch64"` |
| `AT_HWCAP` 16 | x86_64: `CPUID.1:EDX`. AArch64: Linux's `HWCAP_*` bits for what the ID registers say (`FP`, `ASIMD`, `AES`, `PMULL`, `SHA1`, `SHA2`, `CRC32`, `ATOMICS`, `FPHP`, `ASIMDHP`; never `HWCAP_CPUID`, because EL0 can't read the ID registers, and never `HWCAP_SVE`, because SVE stays off) |
| `AT_CLKTCK` 17 | 100 |
| `AT_SECURE` 23 | 0 |
| `AT_RANDOM` 25 | address of the 16 random bytes |
| `AT_HWCAP2` 26 | AArch64: Linux's `HWCAP2_*` (`RNG` when there's `RNDR`); x86_64: 0 |
| `AT_EXECFN` 31 | pointer to the program's path as given |
| `AT_FEOS_EXE` 0x6665_0004 | pointer to the program's canonical absolute path, NUL-terminated, the same as `CURRENT_EXE` (for a script, the script, not its interpreter) |
| `AT_MINSIGSTKSZ` 51 | the largest signal frame this port builds, rounded up (fits in `MINSIGSTKSZ`) |
| `AT_FEOS_ABI` 0x6665_0001 | `ABI_VERSION` |
| `AT_FEOS_FLAGS` 0x6665_0002 | bit 0: `AT_RANDOM` came from the CSPRNG (clear before stage 1e.1) |
| `AT_FEOS_FEATURES` 0x6665_0003 | the stage feature mask, as `SysInfo::features` |
| `AT_NULL` 0 | end |

relibc's `getauxval` (a stub returning 0 today) reads this vector in the feos build.

relibc's `ld.so` finds the program it's loading by `argv[0]` and a `PATH` search
(`resolve_path_name` in `ld_so/start.rs`), and derives `$ORIGIN` from that string. That
fails for a login shell's `-sh`, for `Command::arg0`, for a relative `argv[0]` after a
`chdir`, for a program not on `PATH`, and gives an unresolved `$ORIGIN` through a symbolic
link (rustc's `$ORIGIN/../lib` needs the real one). Its feos build uses `AT_FEOS_EXE`
instead whenever it isn't run as a command, for both the file it opens and `$ORIGIN`.

### 7.3 ELF programs

`libs/elf` grows from "static only" to:

- `ET_EXEC` as today, loaded at its addresses inside the image area.
- `ET_DYN` with or without `PT_INTERP` (PIE, static-PIE, or an interpreter itself), its
  lowest `PT_LOAD` page placed at `USER_PIE_BASE` (or `USER_INTERP_BASE` for an
  interpreter), the whole span inside that area (`ENOEXEC`).
- `PT_INTERP`: a NUL-terminated absolute path up to `PATH_MAX`; the interpreter must be
  `ET_DYN` with no `PT_INTERP` of its own (`ENOEXEC`). The kernel loads both and starts at
  the interpreter's entry, with `AT_BASE` and `AT_ENTRY` set. relibc's `ld.so` is
  self-relocating, which is all this needs.
- `PT_LOAD` segments are mapped as private file mappings through the page cache (not
  copied: stage 1a.4), so a program's text is shared by every process running it and
  read on first touch; the part of a segment past its file size is anonymous zeroed
  memory, with the page that straddles the boundary copied and its tail cleared at load.
- `PT_TLS` is the program's business (libc reads it through `AT_PHDR`). `PT_GNU_STACK`
  is ignored: the stack is never executable. `PT_GNU_RELRO`, `PT_GNU_EH_FRAME`,
  `PT_GNU_PROPERTY` and `PT_NOTE` are accepted and left to the program. `PT_DYNAMIC` is
  allowed.
- W^X stays: a segment both writable and executable is `ENOEXEC`; two segments sharing a
  page are still refused.
- Executable mappings of the program and interpreter take part in `ETXTBSY`.

### 7.4 Scripts

A file starting `#!` names an interpreter: the rest of the first line (up to 255 bytes
after `#!`; `ENOEXEC` if no newline by then) is split at the first space or tab into an
absolute interpreter path and at most one argument. The program becomes the interpreter
with `argv = [interpreter, argument (if any), path as resolved, argv[1..]]`. Interpreters
may themselves be scripts, four deep (`ELOOP` beyond). `#!/usr/bin/env python3` works when
`/usr/bin/env` exists. A file that's neither ELF nor `#!` is `ENOEXEC`, and `execvp` and
`posix_spawnp` in relibc then retry with `/bin/sh path args` as POSIX says.

## 8. Signals in detail

### 8.1 State

Per process: a disposition for each of the 64 signals (default, ignore, or a
`SigAction`), behind the process's signal lock, and a set of signals pending for the
process. Per thread: a mask, a set pending for that thread, an alternate stack, and
whether it's on it.

The words the scheduler looks at (each thread's mask and pending set, and its process's
pending set, kept in a per-slot array indexed through the slot's `tgid`) are atomics, so
the resume-time check can read them under `SCHEDULER` with interrupts masked without
taking the process's signal lock or allocating. The lock order is: process signal lock,
then `SCHEDULER`; `reschedule` never takes the signal lock.

### 8.2 Generation

| Source | Signal | Pending on |
|---|---|---|
| `KILL`, `Processes::signal` from the shell | as given | the process |
| `THREAD_KILL` | as given | the thread |
| a CPU fault the kernel can't resolve | `SIGSEGV` (unmapped: `SEGV_MAPERR`; no access: `SEGV_ACCERR`), `SIGBUS` (file page past end, `MAP_NORESERVE` page with no frame, misaligned where the CPU traps), `SIGILL` (undefined instruction), `SIGFPE` (x86_64 divide error), `SIGTRAP` (`int3`, `brk`) | the faulting thread, synchronously |
| a write to a pipe, socketpair or stream with no reader | `SIGPIPE` | the writing thread |
| a child ending, stopping or continuing | `SIGCHLD` (`CLD_EXITED`, `CLD_KILLED`, `CLD_STOPPED`, `CLD_CONTINUED`; the last two not with `SA_NOCLDSTOP`), raised after the exit record is filled in | the parent |
| an `ITIMER_REAL` deadline | `SIGALRM` | the process |
| a terminal read from a background group ([`IOCTL`](#70-ioctl)) | `SIGTTIN` | the reader's process group |
| the console's line discipline (`ISIG`) | `SIGINT`, `SIGQUIT`, `SIGTSTP` | the foreground process group |
| `TTY_SET_WINSIZE`, a terminal resize | `SIGWINCH` | the foreground process group |

Standard signals don't queue: a second one pending is merged. Real-time signals don't
queue either (a deviation; nothing here sends them more than once at a time).

### 8.3 Delivery

A thread takes a signal when the scheduler is about to resume its user frame: returning
from a call, returning from a fault, or being resumed after preemption. That includes the
`ToUser` fast path in `on_switch_request`, which today resumes without `reschedule` ("a
kill waits for the tick"); it gains the same check, so a call's return never skips a
signal. A signal sent to a thread running on another CPU makes that CPU reschedule (an
IPI), so delivery is prompt.

A process signal is taken by exactly one thread: a thread that doesn't block it claims it
by clearing its bit in the process-pending word with a compare-and-swap, so two threads
on two CPUs can't both take it, and a thread whose mask changes while it looks simply
fails the claim. When `SIGPROCMASK` (or `SIGSUSPEND`, or a handler's return) unblocks a
signal, the thread re-checks the process-pending word before returning to user mode. If
every thread blocks it, it stays pending for the process.

When a deliverable signal is found, the resume is diverted into a kernel thread on the
task's kernel stack (the way calls and faults already run), which can fault in the user
stack and block, and which then:

- **Ignored** (explicitly, or default-ignore: `SIGCHLD`, `SIGURG`, `SIGWINCH`, and
  `SIGCONT` after continuing): dropped.
- **Default stop** (`SIGSTOP`, `SIGTSTP`, `SIGTTIN`, `SIGTTOU`): the whole process stops
  until `SIGCONT`: each thread parks at its next return to user mode, never inside a call
  ([2.1](#21-kernel-rules-the-calls-depend-on)). The stop is recorded for the parent's
  `WAIT4(WUNTRACED)` and `SIGCHLD` once every thread has parked.
- **Default terminate** (everything else, including `SIGKILL`, which no mask or handler can
  stop): the process ends with that signal as its status.
- **Handler**: builds a frame (8.4), adds `mask` and the signal itself (unless
  `SA_NODEFER`) to the thread's mask, resets the disposition if `SA_RESETHAND`, and
  resumes at the handler.

A synchronous fault whose signal is blocked, ignored or default, or whose frame can't be
written (a bad stack pointer, or stack overflow without an alternate stack), ends the
process with that signal, so a fault without a handler is still status 139 for `SIGSEGV`,
as today.

**Interrupted calls.** A call that blocks (pipe, console and socket reads and writes,
`WAIT4`, `ACCEPT`, `CONNECT`, `FLOCK`, `F_SETLKW`, `FUTEX_WAIT2`, `NANOSLEEP`, `POLL`,
`SIGSUSPEND`, `GETRANDOM` before seeding) returns early when a handler is to run. The
`interrupted` hook these waits poll (today only `kill_requested`) becomes "kill pending,
or a deliverable signal pending" for user threads; kernel apps keep today's hook.
`feos_sync` mutex waits stay uninterruptible, and every wait tolerates spurious wakes
(a written rule, since a signal's wake of a thread is a wake of whatever it waits on), so
a mutex waiter woken by a signal just waits again. If it
has moved data, it returns the partial count. Otherwise it returns `EINTR`, unless the
handler has `SA_RESTART` and the call is restartable: then the frame the handler returns
to is the call itself (the program counter set back to the trap instruction, 2 bytes for
`int $0xf0`, 4 for `svc`, the number restored in `rax` on x86_64 and the original first
argument in `x0` on AArch64, both kept by the kernel at entry). Restartable: `READ`,
`WRITE`, `PREAD`, `PWRITE`, `WAIT4`, `ACCEPT`, `RECVFROM`, `SENDTO`, `FLOCK`, `FCNTL`
(`F_SETLKW`), `OPENAT` (if it was waiting), `SPAWN2` and v1's `WAIT`. Never restarted:
`POLL`, `NANOSLEEP`, `FUTEX_WAIT2`, `SIGSUSPEND`, `CONNECT` (Linux's behaviour for each).

A blocked thread asked to end for a fatal signal or `SIGKILL` leaves its call at once,
unwinds it, and ends at the user boundary: the cooperative kill of [2.1](#21-kernel-rules-the-calls-depend-on).

### 8.4 Frames

The frame goes on the alternate stack if the handler has `SA_ONSTACK`, an alternate stack is
set and the thread isn't already on it; otherwise below the interrupted stack pointer
(below the 128-byte red zone on x86_64). Layout from the handler's entry stack pointer:

**x86_64.** `[rsp]` is the restorer's address (the handler's return address), and `rsp + 8`
is 16-byte aligned, as at any function entry. Then:

| Offset from `rsp + 8` | | Bytes |
|---|---|---|
| 0 | `UContext` | 816 |
| 816 | `SigInfo` | 128 |

`UContext` on x86_64:

| Offset | Field | |
|---|---|---|
| 0 | `flags: u64` | `UC_FEOS_FXSAVE` 1 |
| 8 | `link: u64` | 0 |
| 16 | `stack: SigStack` | the alternate stack as it was |
| 40 | `mask: u64` | the mask to restore |
| 48 | `gregs: [u64; 23]` | Linux's order: `r8`..`r15`, `rdi`, `rsi`, `rbp`, `rbx`, `rdx`, `rax`, `rcx`, `rsp`, `rip`, `rflags`, `cs_gs_fs` (`cs` in the low 16 bits), `err`, `trapno`, `oldmask` (0), `cr2` (the fault address) |
| 232 | `fpregs: u64` | address of `fxsave` below, for the handler; ignored by `SIGRETURN` |
| 240 | `reserved: [u64; 8]` | 0 |
| 304 | `fxsave: [u8; 512]` | the `FXSAVE64` image, 16-byte aligned |

Handler registers: `rdi` = signal, `rsi` = `&SigInfo`, `rdx` = `&UContext`, `rax` = 0,
`rip` = handler, direction flag clear, x87/SSE state initialised (`MXCSR` 0x1f80). The
restorer (relibc's `__restore_rt`): `mov rdi, rsp; mov eax, 89; int $0xf0`, since after
`ret` the stack pointer is the `UContext`'s address.

**AArch64.** `sp` is the `UContext`'s address, 16-byte aligned, and `x30` the restorer.

| Offset from `sp` | | Bytes |
|---|---|---|
| 0 | `UContext` | 880 |
| 880 | `SigInfo` | 128 |

`UContext` on AArch64:

| Offset | Field | |
|---|---|---|
| 0 | `flags: u64` | 0 |
| 8 | `link: u64` | 0 |
| 16 | `stack: SigStack` | |
| 40 | `mask: u64` | |
| 48 | `fault_address: u64` | |
| 56 | `regs: [u64; 31]` | `x0`..`x30` |
| 304 | `sp: u64` | |
| 312 | `pc: u64` | |
| 320 | `pstate: u64` | |
| 328 | `fpsr: u32` | |
| 332 | `fpcr: u32` | |
| 336 | `v: [u128; 32]` | `q0`..`q31` |
| 848 | `reserved: [u64; 4]` | 0 |

Handler registers: `x0` = signal, `x1` = `&SigInfo`, `x2` = `&UContext`, `x30` =
restorer, `pc` = handler, `pstate` EL0t with flags clear, `fpcr` and `fpsr` zero. The
restorer: `mov x0, sp; mov x8, 89; svc #0`.

On x86_64, `flags` bit 0 (`UC_FEOS_FXSAVE`) is set: the FP area is a plain `FXSAVE64`
image. A later XSAVE frame (AVX, if `XCR0` ever enables it) will clear it and grow the
frame, with `AT_MINSIGSTKSZ` growing to match; `SIGRETURN` refuses a context whose flags
it doesn't know. On AArch64 `flags` is 0.

`libs/abi` defines both contexts unconditionally, as `ucontext::X86_64` and
`ucontext::Aarch64`, with the size and offset assertions on both, and aliases `UContext`
by `target_arch`; so the host tests on an x86_64 machine check the AArch64 layout too.

Both layouts follow Linux's register order where it has one, but they're feos's own: relibc
defines `ucontext_t` and `mcontext_t` for feos from these tables. Nothing in rustc, cargo,
LLVM, CPython, CMake, Ninja or make reads a `ucontext` beyond passing it along; std's
stack-overflow handler reads only `si_addr`.

### 8.5 What `SIGRETURN` checks

The check is structural: the kernel copies the context once, then **builds a new
`TrapFrame` from a whitelist of its fields**; nothing else in the context reaches the
frame. A context that fails a check ends the process with `SIGSEGV`.

- The context is readable, in the window, and its `flags` are known.
- The program counter and stack pointer are inside the user window. On x86_64 this keeps a
  non-canonical `rip` from faulting `iretq` in the kernel.
- x86_64: taken are the sixteen general registers, `rip`, `rsp`, a sanitised `rflags` and
  a validated `FXSAVE` image. `cs`, `ss`, `vector` and `error` are never copied: `cs` and
  `ss` are the user selectors, since the return path picks `swapgs` from `cs` and a copied
  `0x08` would mean ring 0 at a chosen `rip`. `rflags` keeps only `CF`, `PF`, `AF`, `ZF`,
  `SF`, `DF`, `OF` and `AC`, with `IF` and bit 1 set and `TF`, `NT`, `IOPL`, `RF`, `VM`,
  `VIF`, `VIP` and `ID` clear. `MXCSR` in the `FXSAVE` image has no reserved bit set (it
  would fault `fxrstor` in the kernel), checked against the CPU's `MXCSR_MASK`. If a
  `syscall`/`sysret` path is ever added, `rcx` and `r11` must be checked canonical too.
- AArch64: taken are `x0`..`x30`, `sp`, `pc`, `fpcr`/`fpsr` (architected bits only) and
  `q0`..`q31`. `SPSR` is built as `NZCV` from the context's `pstate` bits 31 to 28 and
  `EL0t`, everything else zero (`DAIF`, `IL`, `SS`, `PAN`, `UAO`, `TCO`, `BTYPE` clear).
- The mask restored drops `SIGKILL` and `SIGSTOP`.
- The alternate stack state is the kernel's own record, not the frame's.

Frames the kernel builds on a user stack have every reserved and padding byte written
as zero from a zeroed kernel buffer, so nothing from the kernel leaks out.

## 9. relibc's `Pal`, function by function

Every function in relibc's platform traits at `50c054e2`, and how its `platform/feos`
does it. "libc" means relibc does it itself, on the calls named. ENOSYS entries say why
nothing on this plan's path needs them.

### 9.1 `Pal` (`src/platform/pal/mod.rs`)

| Function | feos | How, or why not |
|---|---|---|
| `access` | `FACCESSAT` | default impl: `faccessat(AT_FDCWD, ...)` |
| `faccessat` | `FACCESSAT` | |
| `brk` | libc: `ENOMEM` | relibc's allocator (dlmalloc-rs) and every program here allocate with `mmap`; nothing calls `sbrk` |
| `chdir` | `OPENAT` + `FCHDIR` + `CLOSE` | `O_PATH \| O_DIRECTORY \| O_CLOEXEC` |
| `chmod` | `FCHMODAT` | default impl |
| `chown` | `FCHOWNAT` | default impl |
| `clock_getres` | `CLOCK_GETRES` | |
| `clock_gettime` | `CLOCK_GETTIME` | |
| `clock_settime` | `CLOCK_SETTIME` | |
| `close` | `CLOSE` (4) | |
| `dup` | `FCNTL(F_DUPFD, 0)` | default impl |
| `dup2` | `DUP3`, or `FCNTL(F_GETFD)` when equal | |
| `execve` | `EXEC` | argv and envp counted and turned into `UserStr`s in libc |
| `fexecve` | `EXEC` with `SPAWN_EMPTY_PATH` | |
| `exit` | `EXIT` (0) | ends every thread |
| `exit_thread` | `THREAD_EXIT(stack_base, stack_size)` | the kernel frees the stack after leaving it |
| `fchdir` | `FCHDIR` | |
| `fchmod` | `FCHMODAT(fd, "", AT_EMPTY_PATH)` | |
| `fchmodat` | `FCHMODAT` | |
| `fchown` | `FCHOWNAT(fd, "", AT_EMPTY_PATH)` | |
| `fchownat` | `FCHOWNAT` | |
| `fdatasync` | `FSYNC(fd, FSYNC_DATA)` | |
| `flock` | `FLOCK` | |
| `fstat` | `FSTATAT(fd, "", AT_EMPTY_PATH)` | `Stat2` converted to relibc's `stat` |
| `lstat` | `FSTATAT(AT_FDCWD, path, AT_SYMLINK_NOFOLLOW)` | |
| `stat` | `FSTATAT(AT_FDCWD, path, 0)` | |
| `fstatat` | `FSTATAT` | |
| `fstatvfs` | `FSTATVFS` | |
| `fcntl` | `FCNTL` | |
| `fork` | `FORK` | then refreshes the os tid cached in the child's TCB (`GETTID`) |
| `fpath` | `FD_PATH` | |
| `fsync` | `FSYNC(fd, 0)` | |
| `ftruncate` | `FTRUNCATE` | |
| `futex_wait` | `FUTEX_WAIT2(addr, val, deadline, CLOCK_MONOTONIC, 0)` | absolute deadline, as relibc passes it |
| `futex_wake` | `FUTEX_WAKE` (13) | |
| `futimens` | `UTIMENSAT(fd, "", times, AT_EMPTY_PATH)` | |
| `utimens` | `UTIMENSAT(AT_FDCWD, ...)` | default impl |
| `utimensat` | `UTIMENSAT` | |
| `getcwd` | `GETCWD` | with one byte less than the buffer; adds the NUL |
| `getdents` | `GETDENTS` | `opaque_offset` ignored, as on Linux |
| `dir_seek` | `SEEK(fd, cookie, SET)` (15) | on a directory OFD; `SEEK_CUR` gives the current cookie ([`GETDENTS`](#44-getdents)) |
| `dent_reclen_offset` | libc | reads `reclen` and `off` from the `Dirent` layout, as the Linux backend |
| `getegid` | `GETGID` (22) | no set-group-ID programs: effective is real |
| `geteuid` | `GETUID` (21) | the same |
| `getgid` | `GETGID` (22) | |
| `getgroups` | libc | the one group, `GETGID`; feos has no supplementary groups |
| `getpagesize` | libc: 4096 | also `AT_PAGESZ` and `SysInfo::page_size` |
| `getpgid` | `GETPGID` | |
| `getpid` | `GETPID` (7) | the process's pid |
| `getppid` | `GETPPID` | |
| `getpriority` | libc: 0 | nice 0 for everyone; LLVM and Python only read it |
| `getrandom` | `GETRANDOM` | |
| `getresgid` | `GETGID` | all three the same |
| `getresuid` | `GETUID` | all three the same |
| `getrlimit` | libc, from `SYSINFO` | soft = hard: `RLIMIT_NOFILE` = `fd_max`, `RLIMIT_STACK` = 8 MiB (the main stack, [section 6](#6-address-space-layout)), `RLIMIT_AS`/`RLIMIT_DATA` = `memory_limit`, `RLIMIT_NPROC` = `task_max`, `RLIMIT_CORE` = 0, others infinite |
| `setrlimit` | libc | succeeds for values at or below the hard limit without effect (`EPERM` above): no program here needs a limit enforced, cargo only raises `RLIMIT_NOFILE` on macOS, and clang's attempt to raise `RLIMIT_STACK` fails with `EPERM`, after which it runs cc1 on an 8 MiB thread as it does on any system with a fixed limit |
| `getrusage` | `GETRUSAGE` | |
| `getsid` | `GETSID` | |
| `gettid` | `GETTID` | |
| `gettimeofday` | `CLOCK_GETTIME(CLOCK_REALTIME)` | the time zone is zeroed |
| `getuid` | `GETUID` (21) | |
| `lchown` | `FCHOWNAT(..., AT_SYMLINK_NOFOLLOW)` | default impl |
| `link` | `LINKAT` | default impl |
| `linkat` | `LINKAT` | |
| `lseek` | `SEEK` (15) | `ESPIPE` on pipes, sockets, console |
| `mkdirat` | `MKDIRAT` | |
| `mkdir` | `MKDIRAT(AT_FDCWD, ...)` | default impl |
| `mkfifoat`, `mkfifo` | ENOSYS | GNU make 4.4 defaults to a FIFO jobserver and, when `mkfifo` fails, prints an error on every run before falling back to a pipe; so phase 5 configures make with `ac_cv_func_mkfifo=no` (no `HAVE_MKFIFO`), which makes the pipe style its default. cargo's and rustc's jobserver uses pipes; CPython's build and Ninja make no FIFOs. Named FIFOs are later filesystem work |
| `mknodat`, `mknod` | libc | `S_IFREG` becomes `OPENAT(O_CREAT \| O_EXCL)` + `CLOSE`; anything else is `EPERM` (no device nodes) |
| `mlock`, `munlock`, `mlockall`, `munlockall` | libc: validate, 0 | nothing is ever paged out once it has its frame, so they have nothing to do |
| `mmap` | `MMAP2` | |
| `mremap` | ENOSYS | dlmalloc-rs is built without `HAVE_MREMAP` (as on Redox) and `realloc` copies; std and LLVM don't call it |
| `mprotect` | `MPROTECT` (30) | |
| `msync` | `MSYNC` | |
| `madvise` | `MADVISE` | |
| `munmap` | `MUNMAP` (9) | |
| `nanosleep` | `NANOSLEEP(CLOCK_MONOTONIC, 0, ...)` | |
| `open` | `OPENAT(AT_FDCWD, ...)` | default impl |
| `openat` | `OPENAT` | |
| `pipe2` | `PIPE2` | |
| `poll` | `POLL(fds, n, timeout_ms * 1e6, 0)` | |
| `ppoll` | `POLL(fds, n, timeout, sigmask)` | |
| `posix_fallocate` | libc | `FSTATAT`, then `FTRUNCATE` if the range ends past the size: no preallocation, so `ENOSPC` comes at the write. LLVM's `resize_file` uses `ftruncate` off Linux anyway |
| `posix_getdents` | `GETDENTS` | |
| `rlct_clone` | `THREAD_CREATE` | entry = a trampoline that pops relibc's start words, as its Linux backend's asm does after `clone` |
| `rlct_kill` | `THREAD_KILL` | |
| `current_os_tid` | `GETTID` | |
| `read` | `READ` (2) | |
| `pread` | `PREAD` | |
| `readv` | libc: `READ` per buffer | stops at a short read |
| `readlink` | `READLINKAT(AT_FDCWD, ...)` | `/proc/self/exe` is a real link in the kernel's `/proc` ([3.3](#dev-and-procself)), so nothing is intercepted |
| `readlinkat` | `READLINKAT` | |
| `rename` | `RENAMEAT` | default impl |
| `renameat` | `RENAMEAT(..., 0)` | |
| `renameat2` | `RENAMEAT(..., flags)` | `RENAME_EXCHANGE` is `EINVAL` |
| `rmdir` | `UNLINKAT(..., AT_REMOVEDIR)` | default impl |
| `sched_yield` | `YIELD` (6) | |
| `setgroups` | libc | 0 for root with at most one group equal to its gid, `EPERM` otherwise |
| `setpgid` | `SETPGID` | |
| `setpriority` | libc: 0 | accepted for any value, no effect yet; LLVM's background-priority threads and `nice` don't depend on it. A later stage can map nice to `Table::set_priority` |
| `setresgid` | `SETUID` (23) | only root, only with all three equal (or `-1`); `EPERM` otherwise: feos has no saved ids |
| `setresuid` | `SETUID` (23) | the same |
| `setsid` | `SETSID` | |
| `spawn` | `SPAWN2` | file actions map one to one (`addclose`, `adddup2`, `addopen`, `addchdir`, `addfchdir`, and the `_np` aliases `posix_spawn_file_actions_addchdir_np`/`addfchdir_np` that std calls, plus `addclosefrom_np` and `addtcsetpgrp_np`); attribute flags pass through unchanged, since the feos headers use glibc's bit values ([`SPAWN2`](#72-spawn2)). `posix_spawnp` keeps searching `PATH` after `EACCES` (remembering it for the final error, as glibc does) and uses `/bin:/usr/bin` when `PATH` is unset |
| `symlink` | `SYMLINKAT` | default impl |
| `symlinkat` | `SYMLINKAT` | |
| `sync` | `SYNC` | |
| `timer_create`, `timer_delete`, `timer_gettime`, `timer_settime` | ENOSYS | POSIX timers: none of rustc, cargo, LLVM, lld, clang, CMake, Ninja, make or CPython's build uses them (CPython's test suite does). Redox does them in userspace with a thread; the feos backend can copy `platform/redox/timer.rs` later |
| `umask` | `UMASK` | |
| `uname` | libc, from `SYSINFO` | `sysname` "feos", `nodename` from `/etc/hostname` (or "feos"), `release` and `version` from `SysInfo::release`, `machine` from `SysInfo::machine` |
| `unlink` | `UNLINKAT(..., 0)` | default impl |
| `unlinkat` | `UNLINKAT` | |
| `waitpid` | `WAIT4(pid, status, options, 0)` | |
| `write` | `WRITE` (1) | |
| `pwrite` | `PWRITE` | |
| `writev` | libc | copied into one buffer of up to 64 KiB and written with one `WRITE` (so a `writev` to a pipe of up to `PIPE_BUF` stays atomic), larger ones buffer by buffer |
| `verify` | `SYSINFO` | `abi_version >= 2` and every feature bit relibc was built for set ([section 1](#1-rules-every-call-follows)) |

relibc's Redox-only `Sys::relative_to_absolute_foffset` isn't needed.

### 9.2 `PalSignal` (`src/platform/pal/signal.rs`)

| Function | feos | How, or why not |
|---|---|---|
| `getitimer`, `setitimer` | `SETITIMER` | `ITIMER_REAL` only; `getitimer` is `SETITIMER(which, 0, old)`. `alarm()` is relibc over it, so configure test programs, coreutils `timeout` and Python's `signal.alarm` are interrupted rather than hang. `ITIMER_VIRTUAL`/`ITIMER_PROF` are `EINVAL` |
| `kill` | `KILL` | |
| `sigqueue` | `KILL` | the value is dropped and `si_code` is `SI_USER`: no program here reads `si_value` |
| `killpg` | `KILL(-pgrp, sig)` | |
| `raise` | `THREAD_KILL(gettid, sig)` | |
| `sigaction` | `SIGACTION` | `SA_RESTORER` and `__restore_rt` added, as the Linux backend |
| `sigaltstack` | `SIGALTSTACK` | |
| `sigpending` | `SIGPENDING` | |
| `sigprocmask` | `SIGPROCMASK` | |
| `sigsuspend` | `SIGSUSPEND` | |
| `sigtimedwait` | ENOSYS | `sigwait`-style synchronous signal handling isn't used by any tool here: signal-hook and ctrlc (in Rust programs) use a self-pipe, Ninja and make use handlers. Addable later on the same pending sets |

### 9.3 `PalSocket` (`src/platform/pal/socket.rs`)

| Function | feos | How |
|---|---|---|
| `accept` | `ACCEPT(fd, addr, len, 0)` | `accept4` passes flags |
| `bind` | `BIND` | |
| `connect` | `CONNECT` | |
| `getpeername` | `GETPEERNAME` | |
| `getsockname` | `GETSOCKNAME` | |
| `getsockopt` | `GETSOCKOPT` | |
| `listen` | `LISTEN` | |
| `recvfrom` | `RECVFROM` | |
| `recvmsg` | libc over `RECVFROM` | scatter into the `iovec`s; `msg_controllen` set to 0, `msg_flags` to 0 or `MSG_TRUNC` |
| `sendmsg` | libc over `SENDTO` | gather; `EOPNOTSUPP` if there's control data |
| `sendto` | `SENDTO` | |
| `setsockopt` | `SETSOCKOPT` | |
| `shutdown` | `SHUTDOWN` | |
| `socket` | `SOCKET` | |
| `socketpair` | `SOCKETPAIR` | |

### 9.4 The rest

| Trait, function | feos | Why that's safe |
|---|---|---|
| `PalEpoll::epoll_create1`, `epoll_ctl`, `epoll_pwait` | ENOSYS | libuv (CMake) is built with its portable `poll` backend; Ninja and make use `ppoll`/`pselect`. mio *is* on the path: gix's blocking HTTPS transport is `reqwest::blocking` (curl, the other choice, is C), which runs tokio and so mio underneath. mio has a `poll(2)` selector with a pipe waker (the one it uses where there's no epoll or kqueue), and its feos arm selects it ([10.2](#102-std-and-the-libc-crate)). relibc's `select` is written on epoll, so the feos build replaces it with one on `POLL` (10.1) |
| `PalPtrace::ptrace` | ENOSYS | no debugger is on the path. lldb and gdb are later work |
| `PalIOUring::io_uring_setup`, `io_uring_enter`, `io_uring_enter2`, `io_uring_register` | ENOSYS | not exported through `platform::Pal*` at this revision, and nothing uses it |

## 10. Beyond `Pal`: relibc, std and the tools

### 10.1 relibc outside `Pal`

These go to the kernel without the `Pal` traits, or assume something only Linux or Redox
provides, and need the feos backend or a small patch:

| Where | What it needs | feos |
|---|---|---|
| `header/sys_ioctl` | `Sys::ioctl` (Linux only) | an `ioctl_inner` for feos: `IOCTL` for terminal requests and `FIONREAD`; `FIONBIO`, `FIOCLEX`, `FIONCLEX` through `FCNTL`; else `EINVAL` |
| `header/termios`, `isatty` | `tcgetattr` through `ioctl(TCGETS)` | `IOCTL(TTY_GET_ATTR)`; `ENOTTY` makes `isatty` 0 |
| `header/unistd/sysconf` | per-OS file (`linux.rs` returns -1 for CPU counts) | a `feos.rs` reading `SYSINFO` |
| `header/sys_select` | `select` and `pselect` implemented with epoll | a `poll`-based implementation for feos: `fd_set`s to `PollFd`s and back |
| `header/netdb` | `get_dns_server` from `/etc/resolv.conf`, DNS over UDP | `lookup_host` calls `RESOLVE` |
| `header/sys_auxv::getauxval` | a stub returning 0 | reads the aux vector saved at start |
| `header/signal` | `ucontext_t`, `mcontext_t`, `__restore_rt` per OS | from section 8.4 |
| `ld_so/tcb.rs` `os_arch_activate` | `arch_prctl(ARCH_SET_FS)` on Linux x86_64, `msr tpidr_el0` on AArch64 | `SET_TLS` on both |
| `ld_so/mod.rs` | `arch_prctl` | `SET_TLS` |
| `start.rs` | System V stack, aux vector | section 7, unchanged code |
| `ld_so/start.rs` `resolve_path_name`, `ld_so/dso.rs` `$ORIGIN` | the program found by `argv[0]` and `PATH` | **must change for feos**: the program's path and `$ORIGIN` from `AT_FEOS_EXE` ([7.2](#72-the-aux-vector)) |
| `ld_so/dso.rs` `mmap_and_copy` | maps one anonymous read-write region per object, copies segments into it, then `mprotect`s text to `PROT_READ \| PROT_EXEC` | **must change for feos**: map each `PT_LOAD` as a private file mapping at its place inside a `PROT_NONE` reservation (`MAP_FIXED`), with the right protections from the start, and anonymous memory only for `.bss` past the file. That's what glibc and musl do; it's what lets `libLLVM.so` and `librustc_driver.so` be shared through the page cache instead of copied into every process; and anonymous memory can't be made executable here. Objects with `DT_TEXTREL` are refused (nothing in this toolchain makes them) |
| `pthread` | `rlct_clone` stack protocol, `exit_thread` | as in 9.1 |
| `platform/allocator` | dlmalloc over `mmap` | unchanged; `HAVE_MREMAP` off |
| `realpath` | `Sys::fpath` | `FD_PATH` |
| `_paths`, `pwd`, `grp`, `shadow` | `/etc/passwd`, `/etc/group` | files in the image (the native filesystem already keeps owners), [10.4](#104-needed-by-phase-1s-users-but-not-abi) |
| `header/spawn` | `POSIX_SPAWN_*` numbered 1 to 7 | glibc's bit values in the feos headers; the `_np` file-action aliases, `addclosefrom_np`, `addtcsetpgrp_np`; `posix_spawnp` continuing after `EACCES`, default `PATH` `/bin:/usr/bin` ([9.1](#91-pal-srcplatformpalmodrs)) |
| `header/sys_mman` `shm_open` | files under `/dev/shm/` | unchanged: `/dev/shm` is a RAM directory ([3.3](#dev-and-procself)), mapped with `MAP_SHARED` (1a.5) |
| `header/semaphore` `sem_open` | `todo!("named semaphores")` | implemented for feos (and upstreamable): a file in `/dev/shm/sem.<name>` mapped shared, holding the same futex word an unnamed `sem_t` uses. CPython's `multiprocessing` `SemLock`, and so `lit`'s `BoundedSemaphore` and `Pool`, need it for a parallel `check-llvm` |
| `header/sys_times` `times` | `todo_panic!` | implemented from `GETRUSAGE(SELF)` and `GETRUSAGE(CHILDREN)` in `AT_CLKTCK` ticks, and `CLOCK_MONOTONIC` for the return value (CPython's `os.times`, bash's `times`) |
| `header/stdlib` `getloadavg` | Linux reads `/proc/loadavg` | -1: there's no load average (Ninja's `-l` is then ignored; make's `-l` too) |
| `pthread_getattr_np` | the main thread's stack: null and 0 on the Linux arm | the main thread reports `USER_STACK_TOP - USER_ARGS_SIZE - 8 MiB` and 8 MiB with a 1 GiB guard; other threads the stack relibc allocated. std's `stack_overflow` guard page and stacker/psm (rustc's `ensure_sufficient_stack`) both read it |
| `fork` | the os tid cached in the TCB | refreshed in the child (`GETTID`) |
| `posix_openpt`, `openpty` | `/dev/ptmx` | `ENOENT`: no PTYs ([`IOCTL`](#70-ioctl)) |
| `header/sys_ioctl` `TIOCSCTTY`, `TIOCNOTTY` | Linux ioctls | 0 on a terminal fd, `ENOTTY` otherwise |

### 10.2 std and the libc crate

The libc crate gets a `feos` module modelled on `redox`, with the constants and layouts of
relibc's feos headers (which section 5 fixes; the `POSIX_SPAWN_*` values are glibc's).
std uses its `unix` implementation, with `target_os = "feos"` added where std lists
operating systems. Joining the redox arms wholesale would get the wrong answers (redox's
`set_times` is `Unsupported`, it uses `link` instead of `linkat`, the generic
`remove_dir_all` without a `dirfd`, its own random module, and none of the `File::lock`,
`stack_overflow` or `posix_spawn` lists), so the port says, file by file, which arms feos
joins:

| std file (`library/std/src/...`) | feos joins | So |
|---|---|---|
| `sys/fs/unix.rs` `File::set_times`, `FileTimes` | the Linux/BSD `futimens`/`utimensat` arm | cargo's and `filetime`'s mtime setting works |
| `sys/fs/unix.rs` `link` | the `linkat` arm | `fs::hard_link` doesn't follow a final link |
| `sys/fs/unix.rs` `File::lock`, `lock_shared`, `try_lock`, `try_lock_shared`, `unlock` | the `flock` list (Linux, the BSDs) | recent cargo's `target/` and registry locks, which go through `File::lock` |
| `sys/fs/unix.rs` `ReadDir`, `Dir`, `remove_dir_all` | the `openat`/`fdopendir`/`dirfd` arm (`remove_dir_all` in `sys/fs/unix/remove_dir_impl`, the modern one) | race-resistant recursive removal; the dirfd deviation in 3.3 only weakens it against concurrent renames |
| `sys/fs/unix.rs` `copy` | the generic read/write loop | |
| `sys/pal/unix/mod.rs` `init` | the arm that `poll`s fds 0 to 2 and reopens `/dev/null` on `POLLNVAL`, and sets `SIGPIPE` to ignore | `POLL`, `/dev/null`, `SIGACTION` |
| `sys/pal/unix/stack_overflow.rs` | the Linux arm: `sigaltstack`, `SIGSEGV`/`SIGBUS` handlers, guard from `pthread_getattr_np` | from stage 1c.10; rustc's "stack overflow" report |
| `sys/process/unix/unix.rs` `posix_spawn` | the list that uses `posix_spawn`, including `addchdir_np` for a cwd | every `Command` without `pre_exec` avoids `fork` |
| `sys/pipe/unix.rs`, `sys/fd/unix.rs` | the `pipe2(O_CLOEXEC)`, `F_DUPFD_CLOEXEC` and `accept4` arms | no window where a descriptor leaks into a child |
| `sys/random` | the Linux `getrandom` arm (`linux.rs`), with `GRND_INSECURE` for `hashmap_random_keys` and plain `getrandom` for `fill_bytes` | `HashMap` never waits for seeding |
| `sys/sync` (`futex`), `sys/thread_parking` | the futex arm, through a futex relibc exports (`__feos_futex_wait`, `__feos_futex_wake`, over `FUTEX_WAIT2`/`FUTEX_WAKE`), as the libc crate exposes it | std's `Mutex`, `Condvar`, `Once` and parking as on Linux |
| `sys/thread/unix.rs` `available_parallelism` | the `sysconf(_SC_NPROCESSORS_ONLN)` list | `SYSINFO` |
| `sys/thread/unix.rs` `set_name` | `pthread_setname_np` (may fail: std ignores it) | |
| `os/` | new `std::os::feos::{raw, fs}`, as `os::linux::fs` (`MetadataExt` over `Stat2`'s fields) | |
| `env::current_exe` (`sys/pal/unix/os.rs`) | the Linux arm, `read_link("/proc/self/exe")` | `/proc/self/exe` is real ([3.3](#dev-and-procself)) |
| `std_detect` (AArch64) | a feos arm reading `getauxval(AT_HWCAP)`/`AT_HWCAP2`, as Linux's | `is_aarch64_feature_detected!`; on x86_64 `cpuid` works directly |

Other std facts the plan relies on:

| std area | What it calls | Status |
|---|---|---|
| `process::Command` | `posix_spawn` (above), `fork`+`exec` when `pre_exec` closures are set | the `jobserver` crate's `Client::configure` registers a `pre_exec` closure (to clear `FD_CLOEXEC` on its two pipe fds, `src/unix.rs`), which would force every cargo-to-rustc and rustc-to-linker spawn through `fork`. Its feos arm instead clears `FD_CLOEXEC` on the two descriptors once, when the client is created, and registers no closure, so std takes `posix_spawn`; this is what lets cargo run before stage 1d.1 and keeps rustc's multi-GiB address space out of `FORK`'s strict charging |
| process exit-error pipe | `pipe2(O_CLOEXEC)`, read until EOF after `fork` | `EXEC` closing `cloexec` fds is what ends the read |
| `Child::kill`, `try_wait` | `kill(SIGKILL)`, `waitpid(WNOHANG)` | `KILL` (0 on an unreaped child), `WAIT4` |
| `fs` | `open` with `O_CLOEXEC`, `fstat`/`stat`/`lstat`, `opendir`/`readdir` (`d_type`), `mkdir`, `rmdir`, `unlink`, `rename`, `linkat`, `symlink`, `readlink`, `chmod`/`fchmod`, `futimens`/`utimensat`, `fsync`/`fdatasync`, `ftruncate`, `realpath`, `flock` | all mapped in 9.1 |
| `net` | `socket(SOCK_CLOEXEC)`, `connect`, `accept4`, `setsockopt` (`TCP_NODELAY`, `SO_RCVTIMEO`, `SO_SNDTIMEO`, `IP_TTL`), `SO_ERROR`, `ioctl(FIONBIO)`, `send` with `MSG_NOSIGNAL`, `getaddrinfo` | mapped |
| `time` | `clock_gettime(CLOCK_MONOTONIC / CLOCK_REALTIME)` | |
| `thread` | `pthread_create` with a stack size, `PTHREAD_STACK_MIN` (16384), `nanosleep`, `sched_yield` | |
| thread-locals | `#[thread_local]` with static TLS through the TLS register | `SET_TLS` from relibc |
| `os::unix::net` (`UnixStream`, `UnixListener`) | named `AF_UNIX` sockets | `EAFNOSUPPORT`; nothing on the path uses them |

**Crates with per-OS code on the path**, and the `target_os = "feos"` arm each needs (all
small patches, carried as `[patch]` entries in the vendored tree, phase 0, and offered
upstream):

| Crate | Used by | The feos arm |
|---|---|---|
| `libc` | everything | the `feos` module above |
| `rustix` (0.38 and 1.x) | `tempfile`, `terminal_size`, bootstrap's `fd-lock`, gix | its `libc` backend (never `linux_raw`) with feos in the lists of OSes that have `openat`, `utimensat`, `flock`, `pipe2`, `getrandom`, `accept4` and `dup3` |
| `getrandom` 0.2, 0.3 | `rand`, `ahash`, rustls's `ring` | the `getrandom` backend (0.2 `compile_error!`s on an unknown OS; 0.3 falls into `unsupported`) |
| `errno` | rustix, cargo | `__errno_location`, as Linux |
| `socket2` | mio, reqwest, cargo's http | the Linux-like arms: `SOCK_CLOEXEC`, `accept4`, `MSG_NOSIGNAL`; no `SO_REUSEPORT`, no `TCP_KEEPINTVL` beyond what 3.7 stores |
| `mio` | tokio under `reqwest::blocking` (gix's HTTPS) | the `poll(2)` selector with the pipe waker (`src/sys/unix/mod.rs` lists the OSes that use `selector/poll.rs`) |
| `stacker`, `psm` | rustc's `ensure_sufficient_stack` | stacker's `pthread_getattr_np` arm (without one, it can't find the stack's end and allocates a fresh stack on every call); psm's stack switching is per-arch assembly and already fine |
| `jobserver` | cargo, rustc, `cc` | clear `FD_CLOEXEC` at creation instead of `pre_exec` (above); pipe style only (no FIFO) |
| `filetime` | cargo | the `utimensat` arm (it otherwise falls back to `utimes`, which loses nanoseconds) |
| `memmap2` | rustc, gix | the Linux arm minus `mremap`; `MAP_POPULATE` and `madvise` work |
| `home` | cargo, rustup-less setups | `$HOME`, then `getpwuid_r` |
| `libsqlite3-sys` (bundled SQLite) | cargo's global cache tracker | built for `SQLITE_OS_UNIX` with `HAVE_POSIX_FALLOCATE=0`; WAL needs `MAP_SHARED` writable file mappings (1a.5) |
| `libz-sys` / `libz-rs-sys` | cargo, gix | the Rust zlib (`zlib-rs`) where the crate allows it; otherwise plain C zlib, which is portable |
| `curl-sys`, `libgit2-sys`, `openssl-sys` | cargo's registry and git fetches | not built: phase 5 builds cargo with its `gix` and `rustls` features (`http-transport-reqwest`, no `libgit2`), offline from the vendored registry. If a C dependency turns out unavoidable, it goes through clang on relibc like any C program |

### 10.3 What each tool needs, checked

| Tool | Needs beyond the basics | Covered by |
|---|---|---|
| rustc | `dlopen` of proc-macro `.so`s, `mmap` of `.rlib`/`.rmeta` (read-only private), rayon threads, 8 MiB stacks, `current_exe`, `sigaltstack` + `SIGSEGV` handler (its stack-overflow report), `fork` via jobserver, `@argfile` when the command line is long | `MMAP2` file mappings, threads, layout, `CURRENT_EXE`, signals, `FORK`, `SPAWN_ARG_MAX` |
| cargo | `flock` on `target/` and the registry (std's `File::lock`), SQLite in WAL mode (`fcntl` record locks and a `MAP_SHARED` read-write `-shm` file) for its cache tracker, atomic `rename`, `utimensat` with nanoseconds (`filetime`; equal mtimes count as stale), `isatty`/window size, `posix_spawn` + jobserver pipes, `SIGINT` | `FLOCK`, `FCNTL` locks, 1a.5, `RENAMEAT`, `UTIMENSAT` and 1c.4's times, `IOCTL`, `SPAWN2`, signals |
| LLVM, clang, lld | `mmap` of inputs, `FileOutputBuffer` (a shared writable mapping of the output, 1a.5), `getrandom` (`/dev/urandom` as fallback), signal handlers on ~15 signals plus `sigaltstack`, `sysconf` CPUs, `getrusage`, threads, `posix_spawn` (clang driver runs lld) with `/dev/null` redirects, `getMainExecutable` through `/proc/self/exe`, cc1's 8 MiB stack | 3.1, `GETRANDOM`, `/dev` and `/proc/self`, `SIGACTION` on every valid number, `SYSINFO`, `GETRUSAGE`, section 6 |
| lit (`check-llvm`) | CPython's `multiprocessing` (`Pool`, `BoundedSemaphore`: `sem_open` over `/dev/shm`), `subprocess`, `/dev/null` | 1a.5, relibc's `sem_open` (10.1), `FORK` |
| CPython | `fork` (its `subprocess` uses `fork_exec`, closing descriptors with `close_range` or by listing `/proc/self/fd`), `pipe2`, `poll`/`select`, `getrandom` (`os.urandom`), `sigaction` (`SIGINT`), `alarm`, `times`, `uname`, `getpwuid`, `dup3`, `fstatvfs`; not `pty` and not `ctypes` callbacks | all above, `CLOSE_RANGE`, `SETITIMER` |
| CMake (libuv) | `socketpair(AF_UNIX)` for child stdio and its signal pipe, `fork` (its `uv_spawn`), `pipe2`, `poll` backend, `SIGCHLD` handler | `SOCKETPAIR`, `FORK`, `POLL`, signals |
| Ninja | `posix_spawn` with `SETSIGMASK`, `SETSIGDEF`, `SETPGROUP` (and `USEVFORK` when defined), `addopen(0, "/dev/null")`, `ppoll`/`pselect` with a mask, `SIGINT`/`SIGTERM`/`SIGHUP` handlers, `waitpid(WNOHANG)` from its `SIGCHLD` path, `getloadavg` (optional: may fail) | `SPAWN2` attributes at glibc's values, `/dev/null`, `POLL` with `sigmask`, exit records complete before `SIGCHLD` |
| GNU make | `fork`/`exec` or `posix_spawn`, `pipe` jobserver (built without `HAVE_MKFIFO`), `SIGCHLD` with records kept under the default disposition, `pselect`, `/bin/sh -c`, a writable `/tmp` (`-O`) | `FORK`, `EXEC`, signals, `/bin/sh` (stage 1c.12), the image layout (1c.3) |
| gitoxide + rustls | blocking TCP through `reqwest::blocking` (tokio and mio's `poll` selector), DNS, `getrandom`, wall clock (certificate validity), CA certificates in the image, threads, `mmap` of pack files, racy-index checks on nanosecond mtimes | 1e, `POLL`, `MMAP2`, 1c.3, 1c.4 |

### 10.4 Needed by phase 1's users but not ABI

- **A `/bin/sh` that's a user program** (stage 1c.12). make, CMake, Ninja, configure
  scripts, cargo's `build.rs` runners, `execvp`'s `ENOEXEC` fallback and Python's
  `subprocess(shell=True)` all run `/bin/sh -c`. feos-sh is linked into the kernel today
  and can't be `EXEC`ed or inherit arbitrary descriptors, so it's built as a program in
  `user/` (`libs/sh` and `libs/coreutils` over these calls) with its own small runtime,
  before relibc exists. The ABI is complete for it.
- **The image layout** (stage 1c.3), in `boards/qemu-virt/fs/` and the root seeding: a
  writable `/tmp` with the sticky bit (`0o1777`, a RAM mount, std's `temp_dir`, make `-O`,
  `tempfile`); `/root` and `/home/<user>` as `$HOME`; `/etc/passwd`, `/etc/group`,
  `/etc/hosts` (`localhost`), `/etc/services`, `/etc/hostname`, `/etc/localtime` (UTC),
  `/etc/shells`; `/etc/ssl/certs/ca-certificates.crt` for rustls (webpki roots also work,
  compiled in); `/bin` and `/usr/bin` (a link to `/bin`); `/usr/lib` and `/usr/include` for
  the sysroot later. `/dev` and `/proc` are the kernel's mounts.
- **feos-sh passes its environment** to programs it starts (the todo's item): the kernel's
  `Processes::spawn_user` gains an environment, carried to `SPAWN2`'s layout.
- The VFS gains: a mount number in `Stat` (`dev`); stable node ids (`ino` kept across
  rename and not recycled while referenced, [section 2](#2-what-the-kernel-keeps-in-v2));
  atomic replacing `rename`; a hook called on every write and truncate of a file (page-cache
  coherence, and `ETXTBSY` for files mapped executable, kernel apps included); nanosecond
  times and a `changed` (ctime) time, with the update rules of 3.3; and the orphan directory
  convention of `UNLINKAT`, hidden from listings.

## 11. Changes to existing calls

The v1 behaviours this design changes. Everything else in v1 stays exactly as it is,
including two that v2's calls do differently and v1's keep: v1 `MMAP` (8) still promises
every page, `PROT_NONE` included ([`MMAP2`](#31-mmap2)), and v1 `SPAWN` (10) still reports
a missing or non-executable program as a child exiting 127 or 126, a full table as
`EBUSY` and too many arguments as `ENAMETOOLONG` ([`SPAWN2`](#72-spawn2)).

| Call or constant | Change | Why it's acceptable |
|---|---|---|
| `EXIT` (0) | ends every thread of the process | a v1 process has one thread, so nothing differs for it |
| `GETPID` (7) | the process's pid, not the thread's | the same for one thread |
| `MUNMAP` (9), `MPROTECT` (30) | accept any user mapping, holes in `munmap`, and `PROT_EXEC` (W^X) | they only accept more; the `malicious` probes `mprotect-own-code` and `munmap-own-code` change expectation (below) |
| `SEEK` (15) | `ESPIPE` instead of `EBADF` on pipes and the console; on a directory, takes a `GETDENTS` cookie | a corrected error code; nothing outside the tree depends on it |
| `READ`, `WRITE` (2, 1) | `EAGAIN` for `O_NONBLOCK`, `EINTR` and restart under signals, `SIGPIPE` on a broken pipe, work on sockets; on regular files, transfers longer than one `IO_CHUNK` complete instead of coming back short (1c.1); `EISDIR` on a directory | only with v2 state set; a v1 program never sees them, except `SIGPIPE`, whose default (ending the process) matches what v1 programs that ignore `EPIPE` would do next anyway, and the longer transfers, which a program written for short counts handles already |
| `FUTEX_WAIT` (12), `FUTEX_WAKE` (13) | keyed by `(Mm, address)` or `(shared object, offset)` instead of the physical address (1b.1) | identical for a single-threaded process; it's what keeps copy-on-write from losing wakes |
| `CLOSE` (4) | drops `fcntl` locks on the file, the OFD's `flock` with its last descriptor | only with v2 locks |
| `PATH_MAX` | 192 to 4096 | a limit that only grows; kernel path buffers move off the stack |
| `MAX_FDS` | 16 to 1024, growable table | the same |
| `USER_STACK_TOP`, `USER_END`, `USER_MMAP_END`, `USER_STACK_SIZE`, `USER_STACK_GUARD`, `USER_MEMORY_LIMIT`, new `USER_ARGS_SIZE` | new values (section 6, stage 1a.6) | layout constants describe where the kernel puts things; programs use the stack pointer they're given. The only in-tree uses are `malicious` probes, rebuilt with the change. This is a one-time revision recorded by `ABI_VERSION` 2 |
| `USER_MEMORY_LIMIT`'s meaning | from "pages mapped" to "pages charged or owned" (promised, committed, page tables), stage 1a.3 | it only lets more through (a `PROT_NONE` reservation no longer counts), except that page tables now count, which no v1 program comes near |
| program start | the System V stack layout above the registers v1 sets | registers are unchanged, so v1 programs are too |
| exit statuses | a signal-ended process has status "signal `s`" internally; v1's `WAIT` and the shell still see `128 + s` (139 for a fault) | identical to v1 for faults, the only signal v1 had |
| exit status 135 | a new status v1's `WAIT` and the shell can see from stage 1a.4: `128 + SIGBUS`, for a file page past the end, a `MAP_NORESERVE` page with no frame, or a cache page with no frame | it arises from v2 mappings (file mappings and `MAP_NORESERVE` aren't reachable through v1 calls), and from one v1 case: a program's text is demand-paged through the cache from 1a.4, so on a machine out of unpromised frames a text page fault ends it with 135, where v1 copied the image at spawn and the same shortage failed the spawn with 126 |

`malicious` probes whose expected result changes: `mprotect-own-code` (RW now succeeds by
dropping execute, so the probe becomes "make it writable, write, make it executable again:
`EACCES`"), `munmap-own-code` (succeeds, and the child faults with 139 when it next runs
code there: the probe becomes "the parent survives"), and the probes using
`USER_STACK_TOP` keep their names with the new value.

## 12. Implementation stages

In dependency order. Each is one engineer-agent's work: implement, test on both ports,
tick the todo line. Every stage also keeps the standing checks green:

- `make check-arm check-x64 clippy` and `make test`;
- boot phases `user`, `fault`, `ram` and `sched` on both ports, at smp=1 and smp=4,
  including `utest`, `malicious` (every probe blocked) and the frame-leak count (free
  frames after the suite equal those before);
- the AArch64 phases also once with `FEOS_BOOT_TEST_CPU=max` (the knob 1a.2 adds; the
  default stays `cortex-a53`);
- `unimplemented-numbers-enosys` updated to the numbers still unimplemented;
- the stage's errno constants added to `libs/abi` and asserted against Linux's values in
  `make test-abi`;
- the stage's feature bit (below) set in `SysInfo::features` and `AT_FEOS_FEATURES` once
  they exist (1c.6), backfilled for the stages before.

New host-test crates get `test-<name>` targets in the Makefile, listed in `test` and
`.PHONY`. New boot phases are added to the `FEOS_BOOT_TEST_ONLY` list in
`scripts/boot-test.py`. "Both ports" always means x86_64 and AArch64. Every stage whose
calls take a struct adds a `toctou-*` probe for it ([section 1](#1-rules-every-call-follows)).

| Bit | Stage | | Bit | Stage |
|---|---|---|---|---|
| 0 | 1a.1 memory on first touch (done) | | 15 | 1c.5 locks |
| 1 | 1a.2 user work in kernel mode | | 16 | 1c.6 `SPAWN2` and process groups |
| 2 | 1a.3 frame counts, VMAs, anonymous `MMAP2` | | 17 | 1c.7 `EXEC` and scripts |
| 3 | 1a.4 page cache and file mappings | | 18 | 1c.8 `WAIT4`, exit records, `KILL` |
| 4 | 1a.5 shared writable file mappings | | 19 | 1c.9 signal handlers and `SIGRETURN` |
| 5 | 1a.6 the v2 layout, `ABI_VERSION` 2 | | 20 | 1c.10 masks, restart, `SETITIMER` |
| 6 | 1b.1 shared `Process` and `Mm` | | 21 | 1c.11 `POLL`, `IOCTL`, the terminal |
| 7 | 1b.2 threads | | 22 | 1c.12 `/bin/sh` as a user program |
| 8 | 1b.3 time and deadlines | | 23 | 1d.1 `FORK` |
| 9 | 1b.4 1024 task slots | | 24 | 1e.1 CSPRNG, `GETRANDOM`, `CLOCK_SETTIME` |
| 10 | 1b.5 what scales with tasks | | 25 | 1e.2 virtio-rng and `EFI_RNG_PROTOCOL` |
| 11 | 1c.1 OFDs and the FD table | | 26 | 1e.3 sockets |
| 12 | 1c.2 opening, looking, directories | | 27 | 1e.4 `SOCKETPAIR`, `RESOLVE` |
| 13 | 1c.3 `/dev`, `/proc/self`, the image layout | | 28 | 1f.1 dynamic programs |
| 14 | 1c.4 changing the tree, times | | | |

### 1a.2 User work in kernel mode (no new calls)

- **Kernel:** user page faults resolved by a kernel thread on the task's kernel stack
  (`fault_entry` beside `syscall_entry`), with interrupts on, instead of in the trap with
  them masked; the fault entry stops the machine if the slot isn't `IN_USER`; a per-slot
  in-call count reported by `TaskFacts`, and `Table::reapable` false for a user slot in a
  call; the cooperative kill (`kill_pending` wakes the thread, pipe, futex, sleep and
  console waits return `Killed`, the call unwinds to `syscall_entry`, which ends the
  thread); stops of user threads deferred to the return to user mode; `collect_exits`
  queueing dead processes' state in a fixed lock-free queue that the exiting thread or a
  reaper kernel task drains with interrupts on, and `reschedule` no longer calling
  `finish` ([2.1](#21-kernel-rules-the-calls-depend-on)). `scripts/boot-test.py` gains
  `FEOS_BOOT_TEST_CPU` and `FEOS_BOOT_TEST_MEM` (today `-cpu cortex-a53` and `512M` are
  hard-coded).
- **Files:** `kernel/src/sched.rs`, `kernel/src/user.rs`, `kernel/src/fatal.rs`,
  `libs/sched/src/lib.rs`, `libs/app/src/pipe.rs`, `arch/x86_64/src/{interrupts.S,lib.rs}`,
  `arch/aarch64/src/{exceptions.rs,lib.rs}`, `kernel/src/ktest.rs`, `scripts/boot-test.py`,
  `user/src/bin/{utest,malicious}.rs`.
- **Host tests:** `make test-sched`: `user_slot_in_call_not_reapable`,
  `user_slot_in_user_reapable`, `kernel_app_reaping_unchanged`, `user_stop_deferred_in_call`,
  `kernel_app_stop_immediate`; `make test-app`: `pipe_wait_returns_killed`.
- **User checks** (`utest`): `fault-in-kernel-thread` (a program touching 10,000 lazily
  backed pages while `spin` runs: both finish, the timer keeps ticking),
  `kill-during-blocked-read` (a child blocked on a pipe is killed; the parent's write end
  then gets `EPIPE`), `kill-during-long-write` (a child writing 1 MiB at a time to a file is
  killed; heap and free frames return to their baseline).
- **Malicious:** `kill-mid-call-storm` (a child looping on `SPAWN`, `WRITE` and `READ` is
  killed and respawned 200 times; frames and heap back to baseline), `fault-storm` (a child
  touching a million reserved pages in a loop while the parent checks it can still be
  killed).
- **Ktest:** `ktest teardown` (100 tasks killed mid-call; every teardown ran in task context
  with interrupts on).
- **Boot:** `user`, `fault`, `sched`, `jobs` (the shell stops a user program in the middle
  of a file write; another program writes the same mount while it's stopped; it resumes and
  finishes), both ports, smp=1 and smp=4.
- **As built (deviations):**
  - v1 has no call for a program to kill another (`KILL` is 1c.8), so the checks whose
    parent kills a child mid-call (`kill-during-blocked-read`, `kill-during-long-write`,
    `kill-mid-call-storm`, `fault-storm`) are driven by the kernel instead, as
    `ktest ukill`, with `utest --block-read`, `utest --long-write PATH MIB`,
    `malicious --call-loop` and `malicious --fault-loop` as the children. `user` and
    `fault` run it, with `ktest teardown`. They move into `utest` and `malicious` proper
    once `KILL` exists. `fault-in-kernel-thread` is in `utest` as specified.
  - `fault-storm` can't reserve a million pages before 1a.3: `MMAP` still promises a
    frame per page, so the child maps, touches and unmaps 16 MiB at a time, forever, and
    is killed three times while at it; each kill must take under two seconds.
  - Teardown records the exit only once it's done: a reaped task is "running" to
    `wait` until the reaper has freed its frames and closed its pipes, so a parent that
    has waited never sees a child's pipe end still open. The queue is `MAX_TASKS * 2`
    entries; `SPAWN` drains it itself if it's ever full.
  - The reaper is a kernel task in a slot of its own, protected like PID 1
    (`Table::set_protected`: no kill, no stop), so 30 slots are left for everything else.
  - No arch change was needed: `sched::on_user_fault` copies the trap's frame into the
    slot's user frame when it isn't already there (x86_64 takes exceptions on the CPU's
    fault stack), then starts `fault_entry` exactly as `on_syscall` starts `syscall_entry`.
    Every fault from user mode goes through `fault_entry`, not only those with an address.
  - The in-call count is `TaskFacts::in_call`, nonzero only for user programs; the table
    has no separate user flag. A stop sets `stopped` at once as before, and
    `Table::switch` just doesn't hold a task whose count is nonzero; the kernel sends the
    return to the program through the scheduler when `sched::ATTENTION` says a stop or
    kill may be waiting.
  - `FEOS_BOOT_TEST_CPU` applies to both ports (default `cortex-a53` on AArch64, `max` on
    x86_64), and `FEOS_BOOT_TEST_MEM` to every guest whose phase doesn't choose its RAM.
  - `ktest teardown` kills its 100 programs blocked in a read, and 20 `spin`s at EL0 or
    ring 3 beside them, so the reaper's path is taken too; it needs all 100 mid-call and
    all 120 torn down in task context.
  - `fault-in-kernel-thread` shows the timer ticking by a `utest --ticker` child that
    sleeps 2 ms at a time and sends its wake times through a pipe: at least one has to fall
    inside the fault loop, on one CPU as on several.
  - `ktest ukill` adds `kill-while-spawning`: a kernel task kills each of 50 programs by
    the PID it's about to get, often before `spawn` returns, and each must end killed, not
    see the kill as an error from its first read.

### 1a.3 Frame counts, VMAs, anonymous `MMAP2`, `MADVISE`, `MSYNC`

- **Calls:** 31 `MMAP2` (anonymous only: a file fd is `ENODEV` until 1a.4), 32 `MADVISE`, 33
  `MSYNC` (validation only); `MUNMAP` and `MPROTECT` over VMAs; v1 `MMAP` as a wrapper
  keeping v1's charging. Numbers 31 to 108 reserved, `ENOSYS` until their stage.
- **Kernel:** frame metadata in `libs/frames` (map count, pin count, flags,
  [section 2](#2-what-the-kernel-keeps-in-v2)); a VMA list in `libs/vm` (first fit with
  guard pages, split and merge, atomic `MAP_FIXED` replace, `NOREPLACE`, `MAX_VMAS`);
  reservations and `PROT_NONE` as VMAs only, replacing 1a.1's soft PTEs, with the fault path
  consulting the VMA; the zero frame for reads of untouched private anonymous pages;
  shared-anonymous objects; page tables charged and empty leaf tables freed after
  `MUNMAP`; the charging rules of 3.1 with promise-then-allocate everywhere and a checked
  `release`; `USER_MEMORY_LIMIT` counting charged or owned pages; `MAP_POPULATE`. The
  promise accounting moves into `libs/vm` as a host-tested `Promises` type.
- **Files:** `libs/abi/src/lib.rs`, `libs/frames/src/lib.rs`, `libs/vm/src/lib.rs` (new
  `vma.rs`, `promise.rs`), `kernel/src/user.rs` (split into `kernel/src/user/{mod,memory}.rs`),
  `kernel/src/memory.rs`, `user/src/lib.rs`, new `user/src/bin/mtest.rs`,
  `user/src/bin/malicious.rs`, `scripts/boot-test.py`.
- **Host tests:** `make test-frames`: `count_up_and_down_frees_at_zero`,
  `double_free_refused`, `map_count_saturation_refused`, `pin_cap_refused`;
  `make test-vm`: `vma_first_fit_leaves_guards`, `vma_munmap_splits`,
  `fixed_replaces_atomically`, `fixed_noreplace_is_eexist`, `hint_used_when_free`,
  `hint_ignored_when_taken`, `prot_none_takes_no_charge`, `prot_none_builds_no_tables`,
  `mprotect_to_write_charges`, `charged_range_promises_its_tables`,
  `empty_leaf_tables_freed`, `dontneed_reads_zero`, `read_fault_maps_zero_frame`,
  `shared_anon_object_one_frame`, `pinned_frame_outlives_unmap`, `vma_cap_is_enomem`,
  `promise_then_release`, `release_underflow_detected`, `concurrent_promises_never_overcommit`
  (host threads racing `promise`); the existing tests use the `feos_abi` window constants
  instead of `0x10_0000_0000`/`0x14_0000_0000`; `make test-abi` (new constants).
- **User checks** (`mtest`, ends with `mtest: all ok`): `mmap2-anon`, `mmap2-hint`,
  `mmap2-hint-taken`, `mmap2-fixed`, `mmap2-fixed-noreplace`, `mmap2-noreserve-sparse`
  (reserve 8 GiB `PROT_NONE` under the 4 GiB limit, `mprotect` and touch 100 scattered pages:
  free frames drop by at most 100 plus their tables), `prot-none-is-free` (a 4 GiB
  `PROT_NONE` reservation leaves free frames unchanged), `mmap2-populate`,
  `madvise-dontneed-zeroes`, `msync-validates`, `munmap-hole`, `munmap-splits`,
  `mprotect-splits`, `v1-mmap-unchanged` (v1 `MMAP(PROT_NONE)` still takes its promise).
- **Malicious:** `mmap2-anon-exec` (`EACCES`), `mmap2-write-exec` (`EACCES`),
  `mmap2-unknown-flag` (`EINVAL`), `mmap2-shared-and-private` (`EINVAL`),
  `mmap2-anon-fd-5` (`EINVAL`), `mmap2-fixed-unaligned` (`EINVAL`),
  `mmap2-fixed-outside-window` (`ENOMEM`), `mmap2-fixed-over-surface` (`EINVAL`),
  `mmap2-huge` (`ENOMEM`), `madvise-kernel-address` (`EINVAL`), `mprotect-anon-exec`
  (`EACCES`), `vma-exhaustion` (16384 one-page mappings, then `ENOMEM`; machine up, frames
  back), `table-sweep` (`mmap`/`munmap` sweeps across the window 1000 times: free frames
  back to baseline), `unimplemented-numbers-enosys` (34 to 108).
- **Boot:** new phase `mmap` (runs `mtest`, smp=1 and smp=4, both ports); `user`.

### 1a.4 Page cache, stable node ids, file mappings, `PROT_EXEC` on files, `ETXTBSY`

- **Calls:** `MMAP2` with a file (private copy-on-write, shared read-only; shared writable
  stays `ENODEV` until 1a.5), `PROT_EXEC` on file mappings under W^X; `ETXTBSY` on v1
  `OPEN` for writing (`OPENAT` arrives in 1c.2) and on truncation.
- **Kernel:** `libs/pagecache` (index by node identity and page, the dirty bit, LRU of
  unmapped clean pages, reclaim, fills only from frames beyond every promise, the per-file
  reverse map); the VFS's `Stat.dev`, stable node ids (kept across rename, not recycled
  while referenced) and the write/truncate hook (cache coherence, `ETXTBSY` for files mapped
  executable, kernel apps included, clearing "instruction cache synced"); truncation
  zapping mapped pages through the reverse map; `SIGBUS` ending the process with status
  135 until signals exist; the first lock table (executable-mapping and writable-open
  counts, changed with their check in one critical section); the ELF loader
  (`kernel/src/user.rs::load`) mapping segments through the cache instead of copying; the
  lock order of [2.1](#21-kernel-rules-the-calls-depend-on); AArch64 instruction-cache sync
  once per frame.
- **Files:** new `libs/pagecache`, `libs/fs/src/vfs/mod.rs` (dev, node ids, hook),
  `libs/fs/src/{fat32,exfat}.rs` and `libs/fs/src/vfs/ramfs.rs` (ids kept across rename),
  `libs/vm`, `kernel/src/user/`, new `kernel/src/pagecache.rs`, `arch/aarch64/src/mmu.rs`,
  `user/src/bin/{mtest,malicious}.rs`, `Makefile`.
- **Host tests:** `make test-pagecache`: `hit_shares_frame`, `miss_reads_once`,
  `write_hook_updates_cached_page`, `write_hook_clears_icache_synced`,
  `truncate_drops_pages_past_end`, `reverse_map_lists_mappers`,
  `lru_reclaims_unmapped_only`, `reclaim_never_frees_mapped`, `fill_respects_promises`;
  `make test-vm`: `private_file_page_copies_on_write`, `exec_refused_after_copy`,
  `cache_frame_never_written_in_place`; `make test-fs`: `stat_has_dev`, `write_hook_called`,
  `write_to_exec_mapped_refused`, `ino_stable_across_rename` (RAM, FAT32, exFAT, native),
  `ino_not_recycled_while_referenced`.
- **User checks:** `mmap-file-private-reads`, `mmap-file-private-cow` (write, reread the
  file: unchanged), `mmap-file-shared-sees-write` (`write` to the file, the mapping sees
  it), `mmap-file-tail-zero`, `mmap-file-past-end-sigbus` (child ends 135),
  `mmap-file-truncate-sigbus` (map, truncate below the page through v1 `OPEN` with
  truncation, touch: 135), `mmap-file-exec` (map a file holding `ret`/`ret` code, `PROT_READ
  | PROT_EXEC`, call it), `exec-sees-rewritten-file` (map executable, call, unmap, rewrite
  the file, map executable again, call: the new code runs, which on AArch64 proves the
  instruction cache was synced again), `mprotect-file-exec-after-write` (`EACCES`),
  `etxtbsy-open-write`, `etxtbsy-map-exec`, `rename-keeps-ino`, `program-text-shared` (two
  copies of `spin` share frames: free frames drop by less than two images).
- **Malicious:** `mmap-file-shared-write` (`ENODEV`; the expectation changes in 1a.5),
  `mmap-dir` (`ENODEV`), `mmap-pipe` (`ENODEV`), `mmap-writeonly-fd` (`EACCES`),
  `mmap-unaligned-offset` (`EINVAL`), `mmap-offset-overflow` (`EOVERFLOW`),
  `exec-own-written-page` (write code into a private file mapping, `mprotect` exec:
  `EACCES`), `write-running-binary` (open `/bin/malicious` for writing: `ETXTBSY`).
- **Boot:** `mmap`, `user`, `native` (the page cache over the native filesystem),
  `persistence`, `ram` at 192 MiB (`mtest`'s `cache-pressure` check: map files totalling
  more than free memory and read them through; reclaim keeps up and no promised page
  fails), `terminal` (the shell's `cp` over a running program reports "text file busy").

### 1a.5 Shared writable file mappings

- **Calls:** `MMAP2` with `MAP_SHARED | PROT_WRITE` on a file opened for reading and
  writing; `MSYNC(MS_SYNC)` writing back.
- **Kernel:** shared file pages mapped read-only until the first write fault marks them
  dirty and maps them writable; write-back at `MSYNC(MS_SYNC)`, `MUNMAP`, the last v1
  `CLOSE` and process exit (and `FSYNC` from 1c.2); a writable shared mapping counted as a
  writable open for `ETXTBSY`; `READ` through the kernel consulting the cache for a file
  with dirty pages, so it sees mapped writes.
- **Files:** `libs/pagecache`, `libs/vm`, `kernel/src/user/`, `kernel/src/pagecache.rs`,
  `user/src/bin/{mtest,malicious}.rs`.
- **Host tests:** `make test-pagecache`: `dirty_on_first_write`, `msync_writes_back`,
  `munmap_writes_back`, `write_back_error_is_eio`, `read_sees_dirty_page`; `make test-vm`:
  `shared_file_write_fault_marks_dirty`.
- **User checks:** `mmap-shared-write-read` (write through the mapping, `READ` sees it),
  `mmap-shared-two-mappings` (two mappings of one file at different addresses see each
  other's writes), `mmap-shared-msync-persists` (`MSYNC`, then a child started with v1
  `SPAWN` reads the file and sees the data), `mmap-shared-munmap-persists`,
  `shm-file-pattern` (SQLite's `-shm` pattern: extend a file to 32 KiB by writing zeroes,
  map it shared read-write, write, unmap, reread).
- **Malicious:** `mmap-file-shared-write` (now `EACCES` on a read-only fd),
  `mmap-shared-write-of-exec-mapped` (`ETXTBSY`), `exec-map-of-shared-writable` (`ETXTBSY`),
  `dirty-storm` (a child dirtying every page of a 64 MiB shared mapping in a loop is
  killed; the file holds whole pages of old or new data, and frames come back).
- **Boot:** `mmap`, `native`, `persistence` (data written through a mapping and synced
  survives a reboot, on the native and FAT mounts).

### 1a.6 The larger window and the v2 layout, `ABI_VERSION` 2

- **Kernel:** `USER_END` to 512 GiB and the rest of section 6; the argument area reserved
  separately above an 8 MiB lazily backed main stack with a 1 GiB guard;
  `USER_MEMORY_LIMIT` 64 GiB; a boot check that RAM and mapped device regions end below
  `USER_BASE`; `ABI_VERSION` 2.
- **Files:** `libs/abi`, `kernel/src/user/`, `kernel/src/memory.rs`, `boards/*` (the
  check), `libs/vm` (tests), `user/src/bin/{malicious,mtest}.rs`, `docs/architecture.md`.
- **Host tests:** `make test-abi`: `the_layout_fits_together` (now `USER_MMAP_END +
  USER_STACK_GUARD + USER_STACK_SIZE + USER_ARGS_SIZE == USER_STACK_TOP`, everything page
  aligned, the interpreter area below `USER_MMAP_BASE`), `abi_version_is_2`; `make test-vm`
  on the new constants.
- **User checks:** `mmap-above-64g` (a 1 GiB `PROT_NONE` reservation at 300 GiB with a
  hint), `mmap2-noreserve-64g` (reserve 64 GiB `PROT_NONE`, touch pages after `mprotect`),
  `stack-8m` (recurse to use 7.5 MiB of stack), `stack-below-args` (the entry stack pointer
  is inside the argument area and 8 MiB below it is usable), `stack-overflow-faults`
  (child passes 8 MiB: 139).
- **Malicious:** the existing stack probes at the new `USER_STACK_TOP`;
  `mmap2-fixed-at-user-end` (`ENOMEM`); `mmap2-fixed-below-base` (`ENOMEM`).
- **Boot:** `mmap`, `user`, `ram` with `FEOS_BOOT_TEST_MEM=8G` on both ports (on x86_64
  this also exercises the 64-bit PCI hole check of [section 13](#13-risks-and-open-points)).

### 1b.1 `Process` and `Mm` shared and refcounted (no new calls)

- **Kernel:** `PROCESSES[slot]` and `FILES[slot]` become a refcounted `Process` with an
  `Arc<Mm>` that a slot points to; the Mm behind a sleeping mutex, with commit, protect,
  unmap and fork re-checking entries under it, translate-and-pin as one critical section,
  and copies on pinned frames with the lock released (the `SAFETY` comment in
  `with_user_buffer` becomes a pin); no pins across indefinite waits; user futex keys
  (`Wait::UserKey`, by `(Mm, address)` or `(object, offset)`, the frame pinned from the
  check to the resume, waiters woken on unmap) for v1 `FUTEX_WAIT`/`FUTEX_WAKE`; on x86_64 a
  TLB shootdown IPI to every CPU that may hold the Mm's entries, the initiator waiting for
  acknowledgement with interrupts on and no scheduler or IRQ-masking lock held, membership
  published (`SeqCst`) before `CR3` is loaded and read after the entry is cleared, frames
  freed only after the acknowledgements, and no `CR3` reload between threads of one Mm; on
  AArch64 one ASID per Mm in a per-slot atomic beside `ROOT`, flushed and freed only when
  the Mm drops, `TCR_EL1.AS` and 16-bit ASIDs checked at boot, and with 8-bit ASIDs a
  generation rollover using `tlbi vmalle1is` and per-CPU active-ASID tracking; `GETPID`
  returning the process's pid; `EXIT` ending the process (trivially, one thread for now).
- **Files:** `kernel/src/user/`, `kernel/src/sched.rs`, `arch/x86_64/src/{mmu,lib}.rs`,
  `arch/aarch64/src/{mmu,lib}.rs`, `libs/hal/src/lib.rs` (`Arch` gains a remote TLB flush),
  `libs/vm`, `libs/sched` (`Wait::UserKey`), `kernel/src/ktest.rs`.
- **Host tests:** `make test-vm`: `unmap_with_pinned_page_defers_free`,
  `translate_and_pin_is_atomic`; `make test-sched`: `user_key_never_matches_kernel_key`,
  `user_key_by_space_and_address`.
- **Checks:** every existing suite passes unchanged (`utest`, `malicious`, `vmcheck`,
  `permtest`).
- **Ktest:** `ktest tlb` (map a page in a process, read it on CPU 1, unmap from CPU 0, check
  CPU 1 faults and the frame was freed only after its acknowledgement), `ktest asid-rollover`
  (AArch64 forced into 8-bit mode: 300 processes created and destroyed, each sees only its
  own memory), `ktest futex-namespace` (a kernel waiter and a user waiter whose words have
  the same physical address: neither wake reaches the other).
- **Boot:** `user`, `smp`, `sched`, `fault`, both ports, smp=4.

### 1b.2 Threads: `THREAD_CREATE`, `THREAD_EXIT`, `GETTID`, `SET_TLS`, FP state, user-mode controls

- **Calls:** 34, 35, 36, 37.
- **Kernel:** `tgid` in `libs/sched::Task`, with kill, stop and wait working on the tgid and
  flagging every thread, the exit status stored in the Process and the exit record written
  once when its last thread is collected, and `HEAP_BYTES` charged by slot; a task that
  joins an existing `Process`; tids from the pid allocator; the creation mutex; the
  per-process thread cap; per-thread TLS (x86_64 `FS_BASE` written unconditionally at every
  switch-in to a user thread; AArch64 `TPIDR_EL0` saved at every switch-out, in a call or
  not, and the `msr tpidr_el0, xzr` in `mmu::activate` removed); `THREAD_EXIT`'s
  `clear_word`-then-unmap order under the Mm lock, waking after; `EXIT` and a fault in any
  thread's call ending the whole process through the cooperative kill; `CPUID`/`XCR0`
  checked so AVX stays off (`CR4.OSXSAVE` clear) and `AT_HWCAP` never shows SVE; the
  user-mode controls of [section 1](#1-rules-every-call-follows) (`SCTLR_EL1.UCT` and `DZE`,
  `CNTKCTL_EL1.EL0VCTEN`; `CR4.TSD` clear and `UMIP` where present).
- **Files:** `libs/abi`, `kernel/src/user/`, `kernel/src/sched.rs`,
  `arch/*/src/{irq,lib,mmu,cpu}.rs`, `arch/aarch64/src/boot.S` (`SCTLR_EL1_KNOWN`),
  `libs/hal/src/lib.rs` (`Arch::set_user_tls`/`user_tls`), `libs/sched`, `libs/app`
  (`ProcessInfo::threads`), `apps/commands/src/ps.rs`, `user/src/lib.rs`, new
  `user/src/bin/ttest.rs`, `kernel/src/ktest.rs`.
- **Host tests:** `make test-sched`: `tasks_share_tgid`, `kill_tgid_flags_every_thread`,
  `find_pid_survives_leader_exit`, `exit_record_once_at_last_thread`,
  `heap_charged_by_slot`, `wake_key_unaffected_by_threads`; `make test-abi`.
- **User checks** (`ttest`): `thread-runs`, `thread-arg`, `gettid-differs`, `getpid-same`,
  `tls-per-thread` (8 threads, each sets TLS and reads it back after yielding 1000 times),
  `tls-survives-migration` (with affinity changes), `tls-msr-direct` (AArch64: a thread
  writes `TPIDR_EL0` with `msr`, then yields and migrates 1000 times: still there),
  `fs-selector-reload` (x86_64: a thread loads a null selector into `fs` and yields; a
  thread of another process checks its TLS is intact), `clear-word-wakes`,
  `clear-word-inside-stack` (the word lies in the range `THREAD_EXIT` unmaps: it's written
  first and the joiner wakes), `thread-exit-unmaps-stack`, `joiner-reuses-stack-range` (the
  joiner maps over the freed range at once; its mapping survives), `exit-ends-all-threads`,
  `leader-exits-first` (the first thread calls `THREAD_EXIT`, the others carry on, `GETPID`
  is unchanged, and the parent's v1 `WAIT` returns only after the last thread),
  `fp-per-thread` (threads doing different float sums concurrently),
  `shared-memory-visible`, `futex-between-threads`, `munmap-while-other-thread-reads` (one
  thread `read`s a pipe into a buffer another thread unmaps: `EFAULT` or data, never a
  kernel fault), `user-mode-controls` (AArch64: `CTR_EL0` reads, `dc zva` zeroes a block,
  `CNTVCT_EL0` advances; x86_64: `rdtsc` advances), `16-threads`.
- **Malicious:** `thread-create-kernel-entry` (`EFAULT`), `thread-create-unaligned-stack`
  (`EINVAL`), `thread-create-noncanonical-tls` (`EINVAL`), `set-tls-kernel-address`
  (`EINVAL`), `clear-word-kernel-address` (`EINVAL`), `thread-exit-unmap-image-only`
  (allowed, the process survives in its other thread), `thread-exit-bad-range` (`EINVAL`,
  still running), `thread-bomb` (create until `EAGAIN` at the thread cap; the shell can
  still run `ps` and `kill`, and every slot comes back), `ic-ivau-traps` (AArch64: ends
  139), `umip-sgdt` (x86_64 where the CPU has UMIP: ends 139).
- **Ktest:** `ktest thread-fault` (a fault injected into one thread's call ends the whole
  process with 139 and leaves the others' state freed).
- **Boot:** new phase `threads` (smp=1, smp=4), `user`, `fault`.

### 1b.3 Time and deadlines: `FUTEX_WAIT2`, `CLOCK_GETTIME`, `CLOCK_GETRES`, `NANOSLEEP`

- **Calls:** 38, 39, 40, 41.
- **Kernel:** nanosecond monotonic time from the port counter; the wall clock from the RTC
  (second-edge sampling in a task); CPU-time clocks from scheduler accounting (exited
  threads folded into the process); absolute deadlines on user futex keys in `libs/sched`
  (`Wait::UserKeyUntil`, counter-based like `Wait::KeyUntil`); `EINTR` from these waits
  once signals exist (the constant now).
- **Files:** `libs/abi`, `kernel/src/user/`, `kernel/src/sched.rs`, `libs/sched`, `boards/*`
  (RTC at boot), new `user/src/bin/rtest.rs`.
- **Host tests:** `make test-sched`: `deadline_wakes_at_deadline`, `wake_before_deadline`;
  `make test-abi` (`Timespec` layout); `make test-rtc`.
- **User checks** (`rtest`): `monotonic-never-backwards` (a million reads, two threads),
  `realtime-near-rtc` (within 2 s of what the boot test passes in), `getres-positive`,
  `nanosleep-relative` (sleeps at least the request), `nanosleep-absolute`,
  `futex-deadline-times-out`, `futex-deadline-in-past`, `process-cputime-grows`,
  `thread-cputime-per-thread`.
- **Malicious:** `nanosleep-negative` (`EINVAL`), `nanosleep-nsec-overflow` (`EINVAL`),
  `nanosleep-process-cputime` (`EOPNOTSUPP`), `nanosleep-thread-cputime` (`EINVAL`),
  `clock-bad-id` (`EINVAL`), `futex-wait2-kernel-deadline-pointer` (`EFAULT`),
  `futex-wait2-flags` (`EINVAL`), `futex-wait2-expected-upper-bits` (`EINVAL`),
  `toctou-nanosleep` (a thread rewriting the `Timespec` during the call).
- **Boot:** new phase `time` (the boot test passes the host's UTC time to `rtest` and checks
  the guest's), both ports.

### 1b.4 1024 task slots: the scheduler and kernel stacks

- **Kernel:** `MAX_PROCESSES` (tasks) to 1024, with `Table::current` widened to `u16`
  (today `slot as u8 + 1` truncates past 255); kernel stacks allocated per slot from frames
  into a reserved kernel virtual range with guard gaps, 64 KiB for user threads (painted and
  measured: a ktest drives the deepest calls, the native filesystem's journal included, and
  checks half is unused) and 256 KiB for kernel apps, with `stack_overflow_at` and
  `stack_bottom` following them; per-priority ready bitmaps instead of scanning; an
  exit-pending bitmap so `collect_exits` doesn't scan every slot; futex hash buckets with
  per-bucket wait lists (fixed arrays per slot, still allocation-free) so `wake_key` doesn't
  scan every task; 64 slots kept for kernel apps; per-uid caps on processes and threads.
- **Files:** `libs/app/src/lib.rs`, `libs/sched/src/lib.rs`, `kernel/src/sched.rs`,
  `kernel/src/memory.rs`, `arch/*` (kernel stack mapping), `kernel/src/ktest.rs`.
- **Host tests:** `make test-sched`: `bitmap_pick_matches_scan` (a differential test of the
  new pick against the old one on random traces), `exit_bitmap_matches_scan`,
  `futex_buckets_match_scan`, `table_of_1024`, `current_beyond_255`,
  `kernel_reserve_kept`.
- **User checks:** `ttest` `64-threads` and `500-threads`.
- **Malicious:** `thread-bomb` again (it stops at 960 tasks machine-wide; the shell spawns
  `ps` and `kill` fine).
- **Ktest:** `ktest kstack`.
- **Boot:** `sched`, `threads`, `user`, `smp` (the shell runs `spawn 200 spin` and `ps`
  lists them).

### 1b.5 What scales with tasks

- **Kernel:** every `[ProcessInfo; MAX_PROCESSES]` on a stack (`ps`, `taskmgr`, `init`,
  `desktop`, `host`) moved to a heap snapshot API; the pipe pool growing on demand from the
  heap with a per-process cap and holders tracked beyond 32 (by OFD from 1c.1);
  `MAX_SOCKETS` 64 to 1024 in `libs/net`; AArch64 ASIDs for 1024 live Mms (the 8-bit
  generation path under load).
- **Files:** `libs/app/src/{lib,pipe}.rs`, `apps/commands/src/{ps,taskmgr,host}.rs`,
  `libs/init`, `libs/desktop`, `libs/net/src/stack.rs`, `kernel/src/sched.rs`.
- **Host tests:** `make test-app`: `pipe_pool_grows`, `holders_beyond_32`,
  `process_snapshot_of_1024`; `make test-net`: `sockets_beyond_64`; `make test-init` and
  `make test-desktop` (the snapshot API).
- **Malicious:** `pipe-exhaustion` (`ENFILE` at the per-process cap; machine up, another
  process can still make pipes).
- **Boot:** `sched`, `user`, `net`, `terminal` (`spawn 500 spin`, then `ps` and `taskmgr`
  list them).

### 1c.1 Open file descriptions, the growable FD table, `DUP3`, `FCNTL`, `PIPE2`, `CLOSE_RANGE`

- **Calls:** 59, 60 (no locks), 61, 107; v1 `OPEN`, `READ`, `WRITE`, `CLOSE`, `SEEK`,
  `PIPE` on OFDs; `MAX_FDS` 1024; `PATH_MAX` 4096 with heap path buffers; whole transfers
  on regular files, zero-filled gaps, `EISDIR` for `READ` on a directory.
- **Kernel:** refcounted OFDs (files through a VFS handle, pipes, console), released through
  the task-context teardown; `cloexec` per descriptor; `O_NONBLOCK` on pipes and console;
  atomic `PIPE_BUF` writes bounced through a kernel buffer, larger blocking writes
  completing; user buffers pinned or bounced before any file or pipe lock; `F_SETFL`'s
  rules; the new FS error mapping for v2 calls.
- **Files:** `libs/abi`, new `libs/fdtable` (host-tested table: lowest free, growth, `dup3`,
  cloexec sweep, ranges), `kernel/src/user/files.rs`, `libs/app/src/pipe.rs`, new
  `user/src/bin/ftest.rs`.
- **Host tests:** `make test-fdtable`: `lowest_free`, `grows_to_max`, `dup3_replaces`,
  `cloexec_sweep`, `close_range`, `close_range_cloexec`; `make test-app`:
  `pipe_buf_writes_atomic`, `nonblocking_read_eagain`, `large_write_completes`.
- **User checks** (`ftest`): `dup3-shares-offset`, `dup3-cloexec`, `fcntl-dupfd-lowest`,
  `fcntl-getfl-setfl` (`F_SETFL(F_GETFL | O_NONBLOCK)` works, the access mode is
  unchanged), `pipe2-nonblock-eagain`, `pipe2-cloexec`, `pipe-eof-after-last-writer`,
  `pipe-epipe`, `pipe-large-write-completes` (1 MiB in one `WRITE` to a child reading in
  small pieces), `file-write-4mib-one-call`, `write-past-eof-zero-fills`, `read-dir-eisdir`,
  `close-range`, `close-range-cloexec`, `fd-1000` (open 1000 files), `path-4000` (a
  4000-byte path).
- **Malicious:** `dup3-to-1024` (`EBADF`), `dup3-same` (`EINVAL`), `fcntl-bad-cmd` (`EINVAL`),
  `fcntl-setfl-undefined-bit` (`EINVAL`), `fd-exhaustion` (`EMFILE`, machine up),
  `fd-upper-bits` (an fd with garbage in its upper 32 bits: `EBADF`), `pipe2-flags`
  (`EINVAL`), `path-4097` (`ENAMETOOLONG`), `close-range-unshare` (`EINVAL`),
  `close-range-reversed` (`EINVAL`).
- **Boot:** new phase `posix-fs` (runs `ftest` on the RAM root, a FAT mount and a native
  mount), `user`, `permtest` in `native`.

### 1c.2 Opening, looking and directories

- **Calls:** 42 `OPENAT`, 43 `FSTATAT`, 44 `GETDENTS`, 45 `MKDIRAT`, 46 `UNLINKAT` (with
  orphans), 55 `FTRUNCATE`, 56 `FSYNC`, 57 `PREAD`, 58 `PWRITE`, 62 `FCHDIR`, 63 `GETCWD`,
  64 `UMASK`, 65 `FD_PATH`, 66 `CURRENT_EXE`.
- **Kernel:** dirfd resolution over canonical remembered paths, physical `..`, POSIX
  trailing slashes, `EILSEQ`; `O_PATH`, `O_DIRECTORY`, `O_NOFOLLOW`; directory snapshots and
  cookies with `SEEK_CUR`/`SEEK_SET` semantics; the lock table's hold count (OFDs, VMAs,
  cache pins) and the orphan rename on unlink of a file in use, cleanup at mount, and the
  orphan directory hidden; `ETXTBSY` on `OPENAT`; `FSYNC` writing back dirty shared pages;
  `Stat2`.
- **Files:** `libs/abi`, `kernel/src/user/files.rs`, `libs/fs/src/vfs/mod.rs` (dev in
  `Stat`, orphan cleanup and hiding), new `libs/fs/src/vfs/path.rs` (host-tested
  resolution), `user/src/bin/ftest.rs`.
- **Host tests:** `make test-abi` (`Stat2`, `Dirent`, `PathAt` layouts); `make test-fs`:
  `orphans_cleaned_at_mount`, `orphans_hidden_from_listing`, `trailing_slash_rules`,
  `dotdot_is_physical`, `forty_links_is_eloop`, `cookies_survive_resnapshot`.
- **User checks:** `openat-relative-to-dirfd`, `openat-excl`, `openat-directory-enotdir`,
  `openat-nofollow-eloop`, `openat-trailing-slash-enotdir`, `mkdirat-trailing-slash`,
  `dotdot-after-symlink-is-physical`, `o-path-fstat`, `fstatat-nofollow`,
  `fstatat-empty-path-pipe` (`S_IFIFO`), `getdents-lists-all` (500 entries, small buffer),
  `getdents-seek-cookie`, `getdents-seek-cur` (relibc's `posix_getdents` pattern:
  `SEEK_CUR` before each batch, `SEEK_SET` to the next cookie after),
  `getdents-orphans-hidden`, `mkdirat-umask`, `unlinkat-removedir-enotempty`,
  `unlink-open-file-still-reads`, `unlink-mapped-file-still-reads`,
  `unlink-running-program` (a copy of `spin` is unlinked while it runs; it finishes
  normally and its orphan goes), `ftruncate-grow-shrink`, `pread-pwrite-keep-offset`,
  `pwrite-append-at-offset`, `fsync-writes-back-mapping`, `fchdir-getcwd`, `getcwd-no-nul`,
  `getcwd-erange`, `fd-path-canonical`, `current-exe`.
- **Malicious:** `openat-bad-dirfd` (`EBADF`), `openat-file-as-dirfd` (`ENOTDIR`),
  `openat-flags` (`EINVAL`), `path-invalid-utf8` (`EILSEQ`), `fstatat-kernel-out`
  (`EFAULT`), `getdents-tiny-buffer` (`EINVAL`), `getdents-on-file` (`ENOTDIR`),
  `getdents-bad-cookie` (`EINVAL`), `getdents-seek-end` (`EINVAL`), `o-path-read` (`EBADF`),
  `unlinkat-root` (`EBUSY`), `umask-high-bits` (`EINVAL`), `orphan-dir-lookup` (`ENOENT`),
  `toctou-openat-path` (a thread flipping the path length during `OPENAT`: an error or the
  file, never a kernel fault).
- **Boot:** `posix-fs`, `native`, `persistence`.

### 1c.3 `/dev`, `/proc/self` and the image layout

- **Kernel:** the synthesised `/dev` (`null`, `zero`, `full`, `console`, `stdin`, `stdout`,
  `stderr`, `fd`, and `shm` as a RAM directory; `tty` and `random`/`urandom` answer `ENXIO`
  and `ENOENT` until 1c.11 and 1e.1) and `/proc` (`self`, `<pid>/exe`, `<pid>/fd/`) of
  [3.3](#dev-and-procself), consistent across every path call; `MMAP2` of `/dev/zero` as
  anonymous memory; the image layout of [10.4](#104-needed-by-phase-1s-users-but-not-abi):
  a RAM `/tmp` with mode `0o1777` and the sticky bit enforced, `/root`, `/home`, the `/etc`
  files, CA certificates, `/usr/bin`.
- **Files:** new `libs/fs/src/vfs/devfs.rs` (host-tested, with the process-specific parts
  behind a trait the kernel implements), new `kernel/src/user/devfs.rs`,
  `libs/fs/src/vfs/boot.rs` (mounting `/dev`, `/proc`, `/tmp`), `boards/qemu-virt/fs/etc/*`,
  `boards/common/fs_image_build.rs`, `user/src/bin/ftest.rs`.
- **Host tests:** `make test-fs`: `devfs_nodes_and_modes`, `devfs_refuses_changes`,
  `procfs_lists_fds` (against a fake descriptor table), `procfs_hides_other_pids`,
  `sticky_tmp_refuses_foreign_unlink`.
- **User checks** (`ftest`): `dev-null-read-eof`, `dev-null-write-discards`,
  `dev-zero-reads-zero`, `dev-zero-mmap-is-anon`, `dev-full-enospc`, `dev-fd-dups` (writing
  `/dev/fd/1` reaches the same pipe), `dev-stdout-link`, `dev-shm-map-shared` (a file in
  `/dev/shm` mapped shared and writable; a reopen reads the data), `proc-self-exe-exists`
  (`FACCESSAT` and `FSTATAT` on it, LLVM's check), `proc-self-exe-matches-current-exe`,
  `proc-self-fd-lists-open` (open three files; the listing and each link match `FD_PATH`),
  `proc-other-pid-enoent`, `tmp-sticky` (a non-root user can't remove root's file in `/tmp`:
  `EPERM`), `etc-files-present` (`passwd`, `group`, `hosts` parse).
- **Malicious:** `dev-create` (`EPERM`), `dev-unlink-null` (`EPERM`), `proc-create`
  (`EPERM`), `proc-fd-of-other-pid` (`ENOENT`), `dev-tty-without-terminal` (`ENXIO`).
- **Boot:** `posix-fs`, `user`, `persistence` (`/tmp` is empty after a reboot, `/home`
  isn't).

### 1c.4 Changing the tree, and times

- **Calls:** 47 `RENAMEAT`, 48 `LINKAT`, 49 `SYMLINKAT`, 50 `READLINKAT`, 51 `FCHMODAT`,
  52 `FCHOWNAT`, 53 `UTIMENSAT`, 54 `FACCESSAT`, 67 `FSTATVFS`, 68 `SYNC`.
- **Kernel:** the VFS's atomic replacing rename (RAM, native, FAT, exFAT: a host test
  each), with a replaced file in use orphaned; `UTIME_NOW`/`UTIME_OMIT`; `NodeTimes` in
  nanoseconds with a `changed` time, stored at nanoseconds by the RAM mounts and by the
  native format (new nanosecond fields, a format version bump, older images read as `.0`),
  at 10 ms by exFAT and 2 s by FAT; the time update rules of 3.3; `KEEP` in both widths.
- **Files:** `libs/abi`, `kernel/src/user/files.rs`, `libs/fs/src/{lib.rs,vfs/mod.rs,vfs/ramfs.rs,native.rs,fat32.rs,exfat.rs}`,
  `user/src/bin/{ftest,permtest}.rs`.
- **Host tests:** `make test-fs`: `rename_replaces_file_atomically` (each filesystem; a
  crash point between every block write leaves the old or the new file),
  `rename_dir_over_empty_dir`, `rename_noreplace`, `rename_over_file_in_use_orphans_it`,
  `ram_times_are_nanoseconds`, `native_times_are_nanoseconds`,
  `native_old_image_reads_zero_nanoseconds`, `exfat_times_10ms`, `time_update_rules`
  (write sets mtime and ctime; create, unlink and rename set the directory's; chmod sets
  ctime; reads change nothing).
- **User checks:** `rename-over-existing`, `rename-noreplace-eexist`, `rename-exdev`,
  `rename-dir-into-itself-einval`, `rename-over-open-file-still-reads`,
  `rename-over-running-program` (the `tempfile` persist pattern over a copy of a running
  `spin`: the rename succeeds and the old one keeps running), `linkat-follow`,
  `symlinkat-readlinkat`, `readlink-truncates`, `fchmodat-empty-path`, `fchownat-root-only`,
  `fchownat-keep-both-widths`, `utimensat-now-omit`, `utimensat-nanoseconds`,
  `mtime-advances-within-second` (two writes 10 ms apart on the RAM root and a native mount
  give increasing mtimes), `ctime-on-chmod`, `dir-mtime-on-create`, `faccessat-modes`,
  `fstatvfs-counts`, `sync-ok`; `permtest` extended to the `*at` calls on a native mount as
  a non-root user.
- **Malicious:** `renameat-kernel-pathat` (`EFAULT`), `renameat-exchange` (`EINVAL`),
  `pathat-reserved-nonzero` (`EINVAL`), `symlinkat-long-target` (`ENAMETOOLONG`),
  `symlinkat-invalid-utf8` (`EILSEQ`), `fchownat-nonroot` (`EPERM`), `utimensat-bad-nsec`
  (`EINVAL`), `fchmodat-nofollow` (`EOPNOTSUPP`), `toctou-renameat` (a thread flipping a
  `PathAt` length during the call).
- **Boot:** `posix-fs`, `native`, `persistence` (a rename and nanosecond times survive a
  reboot on the native mount).

### 1c.5 Locks: `FLOCK` and `fcntl` record locks

- **Calls:** 69 `FLOCK`; `FCNTL` `F_GETLK`, `F_SETLK`, `F_SETLKW`.
- **Kernel:** the lock table keyed by node identity; BSD and POSIX ownership rules;
  blocking waits that a kill or signal ends; the machine-wide (4096) and per-process (256)
  caps.
- **Files:** new `libs/locks` (host-tested range arithmetic and conflict rules),
  `kernel/src/user/files.rs`, `user/src/bin/ftest.rs`.
- **Host tests:** `make test-locks`: `range_split_merge`, `read_locks_share`,
  `write_conflicts`, `unlock_middle_splits`, `close_any_fd_drops_posix_locks`,
  `flock_follows_ofd`, `per_owner_cap`.
- **User checks:** `flock-exclusive-blocks` (two processes via v1 `SPAWN` with a pipe for
  sync), `flock-nb-eagain`, `flock-shared-shares`, `flock-released-at-exit`,
  `fcntl-setlk-conflict`, `fcntl-getlk-reports-pid`, `fcntl-locks-dropped-on-close`,
  `flock-struct-padding-ignored` (garbage in both reserved fields: the lock works),
  `sqlite-style` (the lock sequence SQLite uses: shared, reserved, pending, exclusive bytes).
- **Malicious:** `flock-bad-op` (`EINVAL`), `setlk-kernel-pointer` (`EFAULT`),
  `setlk-write-on-readonly-fd` (`EBADF`), `lock-exhaustion` (`ENOLCK` at a process's 256;
  another process can still lock), `setlkw-killed` (a child blocked in `F_SETLKW` is killed;
  the lock table is clean), `toctou-setlk`.
- **Boot:** `posix-fs`.

### 1c.6 `SPAWN2`, environments, process groups, `SYSINFO`, `GETRUSAGE`

- **Calls:** 72 `SPAWN2`, 75 `GETPPID`, 76 `GETPGID`, 77 `SETPGID`, 78 `GETSID`, 79
  `SETSID`, 80 `GETRUSAGE` (`RUSAGE_CHILDREN` is zero until 1c.8), 81 `SYSINFO`; v1 `SPAWN`
  on top of `SPAWN2`, keeping its errors; the System V initial stack with the
  static-program aux vector (7.1, 7.2 without `AT_BASE`), `AT_FEOS_EXE` and
  `AT_FEOS_FEATURES` included.
- **Kernel:** parent-side validation into staging frames charged to the caller; FD-table
  construction with the actions (`TCSETPGRP` is `EINVAL` until 1c.11, which has terminals);
  `SETSIGMASK` and `SETSIGDEF` stored into the child's signal state (observable from
  1c.10); the child's exit record allocated in the parent, and the unreaped-children cap
  (records are filled in 1c.8); pid lifetime rules; the feature mask, backfilled;
  `Processes::spawn_user` gains an environment; feos-sh passes its exported variables.
- **Files:** `libs/abi`, `kernel/src/user/{proc,load}.rs`, `kernel/src/sched.rs`,
  `libs/app/src/lib.rs`, `apps/shell`, `libs/sh` (environment to programs), `user/src/lib.rs`
  (reads envp and auxv), new `user/src/bin/ptest.rs`.
- **Host tests:** `make test-abi` (`SpawnRequest`, `SpawnAction`, `SysInfo` layouts, the
  `SPAWN_*` values equal glibc's); `make test-sh` (exported variables reach the spawn
  request).
- **User checks** (`ptest`): `spawn2-argv-envp` (the child echoes them back through a pipe),
  `spawn2-env-from-shell` (`FOO=bar /bin/ptest --env` prints `bar`), `spawn2-dup2-action`,
  `spawn2-close-action`, `spawn2-close-not-open-ignored`, `spawn2-closefrom`,
  `spawn2-open-action`, `spawn2-open-dev-null` (Ninja's action), `spawn2-chdir-action`,
  `spawn2-cloexec-closed`, `spawn2-inherits-fds`, `spawn2-setpgroup`, `spawn2-setsid`,
  `spawn2-flags-combined` (`SETSIGMASK | SETSIGDEF | SETPGROUP | USEVFORK` accepted, the
  child in its own group), `spawn2-enoent-synchronous`, `spawn2-eacces-synchronous`,
  `spawn2-enoexec-synchronous`, `spawn2-1mib-args`, `v1-spawn-errors-unchanged` (a missing
  program still gives a pid whose child exits 127), `auxv-present` (`AT_PAGESZ`, `AT_PHDR`
  matches the program's own headers, `AT_RANDOM` readable), `auxv-feos-exe` (started by a
  relative path after a `chdir`, and with `argv[0]` set to `-sh`, `AT_FEOS_EXE` still equals
  `CURRENT_EXE`), `sysinfo-cpus`, `sysinfo-features` (every bit up to this stage set, and
  equal to `AT_FEOS_FEATURES`), `getrusage-self`, `getppid`.
- **Malicious:** `spawn2-kernel-request` (`EFAULT`), `spawn2-reserved-nonzero` (`EINVAL`),
  `spawn2-argc-zero` (`EINVAL`), `spawn2-args-too-big` (`E2BIG`), `spawn2-string-with-nul`
  (`EINVAL`), `spawn2-action-bad-kind` (`EINVAL`), `spawn2-dup2-from-closed` (`EBADF`),
  `spawn2-close-fd-1024` (`EBADF`), `spawn2-strings-in-kernel` (`EFAULT`),
  `spawn2-schedparam` (`EINVAL`), `setpgid-other-session` (`EPERM`), `toctou-spawn-lengths`
  (`E2BIG`/`EFAULT`, never a kernel fault), `spawn2-concurrent-1mib` (64 threads spawning
  with 1 MiB of arguments at once: success or `ENOMEM`, the kernel heap untouched),
  `children-cap` (1025 unreaped children: `EAGAIN`), `spawn-storm` (spawn until `EAGAIN`;
  every slot and frame comes back).
- **Boot:** new phase `posix-proc` (both ports, smp=1 and 4), `user`, `jobs` (the shell's
  job control still works).

### 1c.7 `EXEC` and scripts

- **Calls:** 73 `EXEC`.
- **Kernel:** the point-of-no-return exec under the creation mutex (siblings killed
  cooperatively and waited for until they've exited, a fresh `Mm`, the old one dropped
  through the reaper, `cloexec` descriptors closed with their record locks, dispositions
  reset, the caller's tid kept); `#!` scripts (7.4).
- **Files:** `libs/abi`, `kernel/src/user/{proc,load}.rs`, `kernel/src/sched.rs`,
  `libs/elf` (shebang parsing, fuzzed), `user/src/bin/ptest.rs`.
- **Host tests:** `make test-elf` (`shebang_*`, fuzz over random first lines).
- **User checks:** `exec-replaces` (pid the same; `CURRENT_EXE` and `/proc/self/exe`
  change), `exec-closes-cloexec`, `exec-keeps-fds`, `exec-from-second-thread` (other threads
  gone, tid kept, `GETPID` unchanged), `exec-while-sibling-blocked` (a sibling blocked in a
  pipe read: the exec completes and the pipe's writer sees the read end close),
  `exec-error-returns`, `exec-script` (a `#!/bin/ptest --script` file), `exec-script-arg`,
  `exec-drops-locks-of-cloexec-fds`, `exec-keeps-locks-of-kept-fds`.
- **Malicious:** `exec-kernel-request` (`EFAULT`), `exec-shebang-loop` (`ELOOP`),
  `exec-shebang-long` (`ENOEXEC`), `exec-then-fault` (status 139 seen by v1 `WAIT`),
  `exec-two-threads-at-once` (one exec wins; the process is consistent),
  `toctou-exec-lengths`.
- **Boot:** `posix-proc`, `user`.

### 1c.8 `WAIT4`, exit records, `KILL` with default actions, stops

- **Calls:** 74 `WAIT4`, 82 `KILL` (default actions only: terminate, ignore, stop,
  continue); v1 `WAIT` on top; `RUSAGE_CHILDREN`.
- **Kernel:** the exit status as an enum encoded per call; records filled under the
  scheduler lock without allocating, complete before `SIGCHLD` is made pending; stop and
  continue at the user boundary, reported with `WUNTRACED`/`WCONTINUED`; `KILL` on an
  unreaped child succeeding; a non-leader tid `ESRCH`; pids held until reaped; the shell's
  `kill` builtin sending `SIGTERM`/`SIGKILL`, and the shell sending `SIGINT` to a
  foreground user program on Ctrl-C.
- **Files:** `libs/abi`, `kernel/src/user/proc.rs`, `kernel/src/sched.rs`, `libs/sched`,
  new `libs/signal` (host-tested: default actions, status encoding), `apps/shell`,
  `user/src/bin/ptest.rs`.
- **Host tests:** `make test-signal`: `default_action_table`, `status_encoding_roundtrip`
  (exit, signal, stop, continue), `v1_status_is_128_plus`; `make test-sched`:
  `pid_not_reused_while_held`.
- **User checks:** `wait4-any`, `wait4-wnohang`, `wait4-pgroup`, `wait4-status-exit`,
  `wait4-status-signal`, `wait4-rusage`, `wait4-echild`, `wait4-wuntraced` (`0x137f` for
  `SIGSTOP`), `wait4-wcontinued` (`0xffff`), `getrusage-children`, `kill-term`,
  `kill-zero-checks`, `kill-pgroup`, `kill-stop-cont`, `kill-unreaped-child-ok`,
  `kill-tid-esrch`, `stop-mid-call-no-lockup` (a child stopped during a 64 MiB file write;
  another process writes the same mount meanwhile), `pid-not-reused-while-unreaped`
  (2000 children spawned and reaped while one stays a zombie: its pid never comes back),
  `orphan-runs-on`.
- **Malicious:** `kill-shell` (`EPERM`), `kill-other-user` (`EPERM`, via `runas`),
  `kill-pid-upper-bits` (`EINVAL`), `wait4-bad-options` (`EINVAL`), `zombie-flood` (1024
  unreaped children, then `EAGAIN`; when the parent exits everything is freed).
- **Boot:** `posix-proc`, `user`, `jobs`, `terminal` (Ctrl-C ends a user program, through
  the shell's `SIGINT`).

### 1c.9 Signal handlers: `SIGACTION`, frames, `SIGRETURN`, faults as signals

- **Calls:** 84 `SIGACTION`, 89 `SIGRETURN`.
- **Kernel:** delivery when a user frame is resumed (8.3), the `ToUser` path included,
  diverted into a kernel thread; per-slot atomic pending words and the claim by
  compare-and-swap; frames per port (8.4) built from `USER_FRAMES` in a zeroed buffer;
  `SIGRETURN` rebuilding the frame from a whitelist (8.5); faults to `SIGSEGV`, `SIGBUS`,
  `SIGILL`, `SIGFPE`, `SIGTRAP`; `SIGPIPE`; `SIGCHLD` with `CLD_*` codes, explicit `SIG_IGN`
  and `SA_NOCLDWAIT` reaping, `SA_NOCLDSTOP`; `SA_RESETHAND`, `SA_NODEFER`; the lock order of
  8.1.
- **Files:** `libs/abi` (both contexts, unconditionally), `libs/signal` (frame builder and
  checker as byte layouts, host-tested for both ports on any host),
  `kernel/src/user/signal.rs`, `kernel/src/sched.rs`, `kernel/src/fatal.rs`,
  `arch/*/src/irq.rs`, new `user/src/bin/sigtest.rs`.
- **Host tests:** `make test-signal`: `frame_layout_x86_64`, `frame_layout_aarch64`
  (offsets against 8.4), `frame_padding_zeroed`, `sigreturn_never_copies_cs_ss`,
  `sigreturn_sanitises_rflags`, `sigreturn_rejects_noncanonical_rip`,
  `sigreturn_rejects_bad_mxcsr`, `sigreturn_builds_spsr_from_nzcv`,
  `sigreturn_rejects_unknown_flags`, `process_signal_claimed_once` (a model racing claims);
  `make test-abi` (both `UContext` layouts on any host).
- **User checks** (`sigtest`): `handler-runs`, `siginfo-pid-uid`, `ucontext-has-registers`,
  `sigreturn-restores-registers` (all general and FP registers survive a handler),
  `sa-resethand`, `sa-nodefer`, `segv-handler-skips-instruction`, `sigpipe-default-kills`,
  `sigpipe-ignored-epipe`, `sigill`, `sigtrap`, `sigfpe` (x86_64), `divide-by-zero-no-signal`
  (AArch64: integer division by zero gives 0 and raises nothing), `sigchld-handler-finds-record`
  (the handler's `WAIT4(WNOHANG)` always reaps), `sigchld-ign-autoreaps`,
  `sigchld-default-keeps-records`, `sigchld-stop-reported` (`CLD_STOPPED`, and none with
  `SA_NOCLDSTOP`), `signal-on-call-return` (a signal sent while the target is in a call
  runs its handler as the call returns, well within a tick).
- **Malicious:** `sigaction-kill` (`EINVAL`), `sigaction-no-restorer` (`EINVAL`),
  `sigaction-kernel-handler` (`EFAULT`), `sigaction-kernel-restorer` (`EFAULT`),
  `sigaction-unknown-flag` (`EINVAL`), `sigreturn-kernel-pc` (ends 139),
  `sigreturn-kernel-stack` (ends 139), `sigreturn-iopl` (x86_64: runs, `IOPL` still 0:
  `cli` faults), `sigreturn-ring0-cs` (x86_64: `cs` 0x08 in the context; it runs at ring 3),
  `sigreturn-el1` (AArch64: runs at EL0), `sigreturn-bad-mxcsr` (ends 139, kernel fine),
  `sigreturn-unknown-flags` (ends 139), `sigreturn-unmapped` (ends 139),
  `handler-that-faults` (ends 139), `toctou-sigreturn` (a thread rewriting the context
  during `SIGRETURN`: one consistent copy or 139, never a kernel fault), `toctou-sigaction`.
- **Boot:** new phase `signals`, `user`, `fault`.

### 1c.10 Masks, `THREAD_KILL`, alternate stacks, interruption and restart, `SETITIMER`

- **Calls:** 83 `THREAD_KILL`, 85 `SIGPROCMASK`, 86 `SIGALTSTACK`, 87 `SIGPENDING`, 88
  `SIGSUSPEND`, 108 `SETITIMER`.
- **Kernel:** masks, with the process-pending re-check when `SIGPROCMASK`, `SIGSUSPEND` or
  `SIGRETURN` unblocks; alternate stacks; the `interrupted` hook ("kill or deliverable
  signal") for user threads and `EINTR` with partial counts; `SA_RESTART` (the number kept
  in `rax` on x86_64, the original `x0` on AArch64); an IPI to a target running on another
  CPU; `SPAWN2`'s `SETSIGMASK`/`SETSIGDEF` now in effect; `ITIMER_REAL`.
- **Files:** `libs/abi`, `libs/signal`, `kernel/src/user/signal.rs`, `kernel/src/sched.rs`,
  `libs/app/src/pipe.rs`, `arch/*/src/irq.rs`, `user/src/bin/{sigtest,ptest}.rs`.
- **Host tests:** `make test-signal`: `mask_never_blocks_kill`, `restart_rewinds_by_port`,
  `itimer_rearms_by_interval`.
- **User checks:** `mask-blocks-then-delivers`, `sigprocmask-unblock-delivers-before-return`,
  `sigpending`, `sigsuspend`, `altstack-used`, `stack-overflow-handler` (overflow the main
  stack, catch `SIGSEGV` on the alternate stack, `si_addr` in the guard), `eintr-read`,
  `sa-restart-read`, `sa-restart-wait4`, `thread-kill-targets-thread`,
  `process-signal-unblocked-thread`, `process-signal-once` (with 8 threads not blocking it,
  each of 10,000 process signals is handled exactly once), `ipi-prompt-delivery` (a thread
  spinning on another CPU runs a `THREAD_KILL` handler within 1 ms), `spawn2-sigmask`,
  `spawn2-setsigdef`, `alarm-interrupts-read` (`SETITIMER` 50 ms; a blocked read returns
  `EINTR`), `setitimer-interval`, `getitimer-reads`.
- **Malicious:** `sigprocmask-bad-how` (`EINVAL`), `altstack-in-kernel` (`EFAULT`),
  `altstack-too-small` (`ENOMEM`), `thread-kill-other-process` (`ESRCH`),
  `setitimer-virtual` (`EINVAL`), `setitimer-bad-usec` (`EINVAL`), `signal-storm` (a
  thousand signals a second at a busy child; it still exits normally), `toctou-setitimer`.
- **Boot:** `signals` (smp=1 and 4), `user`, `fault`.

### 1c.11 `POLL`, `IOCTL`, the terminal

- **Calls:** 70 `IOCTL`, 71 `POLL`.
- **Kernel:** the machine-wide readiness word and rechecking waits; the console line
  discipline (canonical, echo, `ISIG` raising `SIGINT`, `SIGQUIT`, `SIGTSTP` on the
  foreground group, `VMIN`/`VTIME`); terminal marking at spawn; window size from the
  terminal app; the foreground group and `SPAWN2`'s `TCSETPGRP`; background reads
  (`SIGTTIN` or `EIO`); drain, flush, flow and `GET_SID`; `/dev/tty`.
- **Files:** `libs/abi`, `kernel/src/user/{files,tty}.rs`, new `libs/tty` (the line
  discipline), `libs/app/src/pipe.rs`, `apps/shell`, `libs/desktop` (window size to the
  console), new `user/src/bin/polltest.rs`.
- **Host tests:** `make test-app`: `readiness_word_moves_on_pipe_change`; `make test-tty`:
  `canonical_lines`, `erase`, `isig_raises`, `vmin_vtime`, `flush_drops_input`,
  `background_read_policy`.
- **User checks:** `poll-pipe-in-out`, `poll-hup`, `poll-nval`, `poll-timeout`,
  `poll-sigmask`, `poll-many` (500 pipes), `poll-unknown-events-accepted` (`POLLRDHUP` and
  `POLLWRBAND` asked for: no error), `poll-woken-by-other-thread` (mio's waker pattern: a
  thread blocked in `POLL` on a pipe wakes when another thread writes it mid-wait),
  `poll-dev-null`, `isatty-console`, `isatty-pipe-false`, `winsize`, `tcsetattr-raw`
  (driven by the boot test: raw mode reads one key), `fionread`, `tcdrain`, `tcflush`,
  `tcgetsid`, `dev-tty-opens-terminal`, `background-read-sigttin`, `spawn2-tcsetpgrp`.
- **Malicious:** `poll-count-too-big` (`EINVAL`), `poll-kernel-array` (`EFAULT`),
  `ioctl-unknown` (`EINVAL`), `ioctl-tty-on-file` (`ENOTTY`), `ioctl-kernel-pointer`
  (`EFAULT`), `tty-flush-bad-queue` (`EINVAL`), `toctou-poll-array`.
- **Boot:** new phase `poll`, `terminal` (Ctrl-C now through the kernel's `ISIG`), `user`.

### 1c.12 `/bin/sh` as a user program

- **Kernel:** none; no new calls.
- **User space:** feos-sh (`libs/sh` with `libs/coreutils`) built as `/bin/sh` in `user/`,
  over a small runtime in `user/src/lib.rs` (environment, cwd, `SPAWN2` with actions,
  `EXEC`, `WAIT4`, `SIGACTION`, `POLL`), since relibc comes only in phase 2. It runs `-c`
  strings and scripts, pipelines, redirections (`2>/dev/null`, `>file`, `2>&1`), `$?`,
  `exec`, `cd`, `export`, `trap` for `INT` and `TERM`, and job control on the terminal
  through `TCSETPGRP`. The in-kernel feos-sh stays the console shell.
- **Files:** new `user/src/bin/sh.rs`, `user/src/lib.rs`, `libs/sh` and `libs/coreutils`
  (a backend trait the user runtime implements beside the kernel's), `user/Cargo.toml`,
  `Makefile` (`user-*` builds it).
- **Host tests:** `make test-sh` and `make test-coreutils` against the new backend's mock
  (`spawn_actions_for_redirections`, `pipeline_closes_unused_ends`, `exit_status_of_signal`).
- **User checks** (driven by the boot test): `sh-c-echo`, `sh-status` (`false; echo $?`
  prints 1), `sh-pipeline` (`ptest --emit | ptest --count`), `sh-redirections`
  (`2>/dev/null`, `>file`, `2>&1`), `sh-cd-relative-program`, `sh-script-file`,
  `sh-shebang-via-exec`, `sh-enoexec-fallback` (`ptest`'s `execvp` of a file without `#!`
  runs it through `/bin/sh`), `sh-env-to-children`, `sh-exec-builtin`, `sh-trap-term`,
  `sh-ctrl-c-ends-job-not-shell`, `sh-no-fd-leak` (a child lists `/proc/self/fd`: only 0,
  1 and 2).
- **Malicious:** `sh-deep-recursion` (a script recursing 10,000 functions ends cleanly,
  with an error or 139, machine fine).
- **Boot:** new phase `usersh`, `posix-proc`, `terminal`.

### 1d.1 `FORK`

- **Calls:** 90.
- **Kernel:** copy-on-write in `libs/vm`, with one port-independent software bit (bit 57,
  free on x86_64 and software-owned on AArch64) added to `SOFT_BITS`; write faults copying,
  or reusing in place only an anonymous unpinned frame at map count 1, never a page-cache
  frame; pinned private writable pages copied for the child at fork; write pins breaking
  copy-on-write first; page tables copied and charged; shared-anonymous objects and file
  mappings shared; strict charging by frame transitions with per-process limit attribution
  kept apart; the parent's TLB shootdown; the FD table copied sharing OFDs; signal state
  copied, `clear_word` and the interval timer not; the child's exit record; the creation
  mutex.
- **Files:** `libs/vm`, `libs/frames`, `kernel/src/user/{memory,proc}.rs`,
  `kernel/src/sched.rs`, `user/src/bin/ptest.rs`, `kernel/src/ktest.rs`.
- **Host tests:** `make test-vm`, each run with both `BITS` and `ARM_BITS`:
  `fork_shares_until_write`, `cow_copy_at_count_two`, `cow_reuse_at_count_one_anon_only`,
  `cow_never_reuses_cache_frame`, `pinned_page_copied_at_fork`, `write_pin_breaks_cow`,
  `shared_stays_shared`, `untouched_shared_anon_shared_after_fork`,
  `fork_debt_is_sum_of_extra_refs`, `cow_copy_consumes_promise`,
  `unmap_of_shared_page_releases_one`, `fork_frees_all_on_exit` (counts back to zero),
  `cow_bit_is_57_on_both`.
- **User checks:** `fork-returns-twice`, `fork-memory-isolated` (both write the same page),
  `fork-shared-anon-shared` (including a page neither touched before the fork),
  `fork-shared-futex` (parent and child wait and wake on one `MAP_SHARED` word),
  `fork-file-private-cow`, `fork-fds-share-offset`, `fork-flock-shared`,
  `fork-in-threaded-parent` (only the caller is copied; the parent's other threads keep
  running and see their own writes), `fork-while-sibling-futex-waits` (a parent thread
  blocked in `FUTEX_WAIT2` on a heap word; another forks, then writes the word and wakes:
  the waiter wakes), `fork-child-futex-isolated` (a child's wake on the same address
  doesn't wake the parent's waiter), `fork-tid-refreshed` (in the child `GETTID ==
  GETPID`), `fork-no-clear-word`, `fork-exec`, `fork-wait`, `fork-1000` (sequential
  fork/exit, frames back to baseline), `vmcheck` over `fork`.
- **Malicious:** `fork-bomb` (until `EAGAIN`; the machine stays up and recovers),
  `fork-then-munmap-race` (the parent unmaps while the child reads), `fork-enomem` (fork a
  process with a huge writable private mapping on a small machine: `ENOMEM`, parent fine),
  `cow-write-from-kernel` (a child `read`s into a copy-on-write page: the kernel copies
  first, the parent's page unchanged), `fork-during-disk-read` (a parent thread's `READ`
  from a file into a buffer is in flight while another forks: the child's copy of the
  buffer never shows data read after the fork).
- **Ktest:** `ktest fork-promises` (10,000 forks, exits and writes in mixed orders;
  `UNBACKED` back to its baseline and never negative).
- **Boot:** new phase `fork`, `posix-proc`, `user`, `ram`.

### 1e.1 Randomness and setting the clock: the CSPRNG, CPU and firmware sources, `GETRANDOM`, `CLOCK_SETTIME`

- **Calls:** 91, 92.
- **Kernel:** `libs/csprng` (ChaCha20, fast key erasure, reseed schedule); x86_64
  `RDSEED`/`RDRAND`, AArch64 `RNDR`, the DTB `rng-seed`; jitter mixed in but never counted;
  the loud boot warning with no trusted source and `SYSINFO_RNG_TRUSTED`; `AT_RANDOM` from
  the CSPRNG; `/dev/random` and `/dev/urandom`.
- **Files:** new `libs/csprng`, `boards/*`, `libs/firmware` (DTB `rng-seed`),
  `kernel/src/user/`, `kernel/src/user/devfs.rs`, `Makefile`, `user/src/bin/rtest.rs`.
- **Host tests:** `make test-csprng` (RFC 8439 ChaCha20 vectors; key erasure: output after a
  state capture can't be recomputed backwards; reseed changes output); `make test-firmware`
  (`rng_seed_read`).
- **User checks:** `getrandom-fills`, `getrandom-distinct` (ten 32-byte reads all differ),
  `getrandom-monobit` (1 MiB within 4 sigma), `getrandom-nonblock-seeded`,
  `getrandom-insecure-never-waits`, `at-random-from-csprng` (`AT_FEOS_FLAGS` bit),
  `dev-urandom-reads`, `dev-random-reads`, `clock-settime-root`,
  `clock-settime-nonroot-eperm`.
- **Malicious:** `getrandom-kernel-buffer` (`EFAULT`), `getrandom-flags` (`EINVAL`),
  `getrandom-insecure-and-random` (`EINVAL`), `getrandom-huge` (short count, no error),
  `clock-settime-monotonic` (`EINVAL`), `toctou-clock-settime`.
- **Boot:** new phase `random`: x86_64 with `-cpu max` (seeded from `RDSEED`), and with
  `FEOS_BOOT_TEST_CPU=max,-rdrand,-rdseed` and no virtio-rng (the warning is logged,
  `GRND_NONBLOCK` gives `EAGAIN`, `GRND_INSECURE` and `/dev/urandom` work); AArch64 with
  `cortex-a53` (seeded from the DTB) and `-cpu max` (`RNDR`); two boots give different
  output.

### 1e.2 virtio-rng and `EFI_RNG_PROTOCOL`

- **Kernel:** a virtio-rng driver (`drivers/virtio/rng`) on both transports, and the
  `-device` lines in `scripts/qemu.sh`, `scripts/qemu-x64.sh` and the boot test; the UEFI
  loader's `EFI_RNG_PROTOCOL` bytes in `Handoff`.
- **Files:** new `drivers/virtio/rng`, `boards/*`, `boot/efi`, `libs/bootinfo`,
  `scripts/*`, `Makefile`.
- **Host tests:** `make test-virtio-rng` (queue handling against a fake device); `make
  test-bootinfo` (the new `Handoff` field).
- **User checks:** `rtest`'s `getrandom-*` again under each source alone.
- **Boot:** `random` with virtio-rng added: x86_64 `-cpu max,-rdrand,-rdseed` is now
  seeded and `GETRANDOM(0)` returns; both ports with and without virtio-rng, the log naming
  the sources; `efi` (seeded from `EFI_RNG_PROTOCOL` on both ports).

### 1e.3 Sockets: TCP and UDP

- **Calls:** 93 `SOCKET`, 95 `BIND`, 96 `LISTEN`, 97 `ACCEPT`, 98 `CONNECT`, 99 `SENDTO`,
  100 `RECVFROM`, 101 `SHUTDOWN`, 102 `GETSOCKOPT`, 103 `SETSOCKOPT`, 104 `GETSOCKNAME`,
  105 `GETPEERNAME`; `READ`/`WRITE`/`POLL`/`FD_NREAD` on sockets.
- **Kernel:** non-blocking operations and a readiness query on `feos_hal::net::Network`
  (the stack already returns `Option`s); socket OFDs owning their sockets; loopback in
  `libs/net`; socket timeouts; `SO_ERROR`; `SIGPIPE` and `MSG_NOSIGNAL`; the network service
  advancing the readiness word.
- **Files:** `libs/hal/src/net.rs`, `libs/net/src/{service,stack,tcp}.rs`, `libs/abi`,
  `kernel/src/user/net.rs`, new `user/src/bin/ntest.rs`.
- **Host tests:** `make test-net`: `loopback_tcp`, `loopback_udp`,
  `nonblocking_connect_in_progress`, `readiness_reports_in_out_hup`,
  `socket_owned_by_kernel_not_swept`, `so_error_after_refused`.
- **User checks** (`ntest`): `tcp-loopback-echo`, `tcp-nonblocking-connect`,
  `tcp-nonblocking-connect-refused` (`POLLOUT | POLLERR`, then `SO_ERROR` is
  `ECONNREFUSED`), `tcp-accept-nonblock`, `tcp-shutdown-wr`, `tcp-refused`, `udp-loopback`,
  `udp-connect-filters`, `udp-truncates`, `sockopt-rcvtimeo`, `getsockname-ephemeral`,
  `sin-zero-ignored`, `poll-socket`, `poll-woken-by-socket-data` (a thread blocked in `POLL`
  on a socket and a waker pipe wakes when the peer sends mid-wait: mio's selector pattern),
  `tcp-to-host` (connects to a server the boot test runs on the host through slirp and
  checks an echo), `msg-nosignal`.
- **Malicious:** `socket-bad-domain` (`EAFNOSUPPORT`), `socket-raw` (`ESOCKTNOSUPPORT`),
  `bind-kernel-address` (`EFAULT`), `bind-short-length` (`EINVAL`), `sendto-on-file`
  (`ENOTSOCK`), `setsockopt-unknown` (`ENOPROTOOPT`), `socket-exhaustion` (`ENOBUFS`,
  machine up), `socket-left-open-at-kill` (the socket closes when the process is killed),
  `toctou-sockaddr`.
- **Boot:** new phase `netuser` (both ports, with the host-side echo server), `net`.

### 1e.4 `SOCKETPAIR` and `RESOLVE`

- **Calls:** 94, 106.
- **Kernel:** socketpairs as two pipes with shutdown; `RESOLVE` over `Network::resolve`
  with several answers kept.
- **Files:** `libs/abi`, `kernel/src/user/net.rs`, `libs/net/src/dns.rs` (several A records),
  `user/src/bin/ntest.rs`.
- **Host tests:** `make test-net` (`dns_several_answers`); `make test-app`
  (`socketpair_both_directions`).
- **User checks:** `socketpair-echo`, `socketpair-shutdown`, `socketpair-poll`,
  `socketpair-across-spawn` (libuv's pattern: child stdio over a socketpair),
  `resolve-localhost`, `resolve-dotted`, `resolve-host` (a name the boot test's slirp DNS
  answers), `resolve-nxdomain`.
- **Malicious:** `socketpair-dgram` (`EOPNOTSUPP`), `resolve-long-name` (`EINVAL`),
  `resolve-kernel-out` (`EFAULT`), `resolve-max-zero` (`EINVAL`).
- **Boot:** `netuser`.

### 1f.1 Dynamic programs: `ET_DYN`, PIE, `PT_INTERP`, the full aux vector

- **Kernel:** `libs/elf` accepting `ET_DYN` and `PT_INTERP` (with `AT_PHDR` resolution);
  the loader biasing PIE and interpreter, mapping both through the page cache,
  `AT_BASE`/`AT_ENTRY`; both in the `ETXTBSY` counts; `AT_FEOS_EXE` naming the program, not
  the interpreter.
- **Files:** `libs/elf/src/lib.rs`, `kernel/src/user/load.rs`, `user/` (a `-C
  relocation-model=pie` build of one program; new `user/src/bin/ldtest.rs`, a static-PIE
  that relocates itself from its `_DYNAMIC`, opens the program named by `AT_FEOS_EXE`,
  applies `R_*_RELATIVE` to it as mapped at `AT_PHDR`/`AT_ENTRY`, checks the aux vector and
  jumps to `AT_ENTRY`; `dynhello`, a PIE whose `PT_INTERP` is `/bin/ldtest`),
  `user/Cargo.toml`, `Makefile` (`user-*` builds them).
- **Host tests:** `make test-elf`: `et_dyn_accepted`, `interp_path_read`,
  `interp_must_be_dyn`, `phdr_from_pt_phdr`, `phdr_from_first_load`, `pie_bias_applied`,
  `gnu_stack_ignored`, and the mutation fuzz extended to dynamic files (`make fuzz`).
- **User checks:** `pie-runs` (a PIE `hello` at `USER_PIE_BASE`), `interp-runs`
  (`dynhello` prints through `ldtest`), `interp-finds-program-by-exe` (`dynhello` started
  with `argv[0]` `-x`, and by a relative path after a `chdir`, still runs: `ldtest` used
  `AT_FEOS_EXE`, not `argv[0]`), `exe-through-symlink-is-canonical` (started through a
  link, `AT_FEOS_EXE` is the real path), `auxv-base-entry`, `interp-shared-text`,
  `exec-dynamic`, `spawn2-dynamic`.
- **Malicious:** `interp-missing` (`ENOENT` from `SPAWN2`), `interp-not-dyn` (`ENOEXEC`),
  `interp-recursive` (`ENOEXEC`), `interp-path-unterminated` (`ENOEXEC`), `pie-too-big`
  (`ENOEXEC`), `wx-segment` (`ENOEXEC`, unchanged).
- **Boot:** new phase `loader`, `user`.

After 1f.1, phase 1 is complete, and the next milestone is relibc's C hello world on feos
(phase 2).

## 13. Risks and open points

- **The VFS is path-based.** dirfds and the cwd are remembered paths (3.3), and orphaned
  files are renamed rather than kept by handle. Both are honest workarounds that phase 7's
  root-capable native filesystem should replace with node-relative lookups and
  open-unlinked inodes; the ABI doesn't change when it does.
- **Coarse times on FAT.** 1c.4 gives the RAM mounts and the native format nanosecond
  times, but FAT keeps 2 s and exFAT 10 ms. cargo treats equal mtimes as stale (rebuilding
  forever when a generated file and its input land in one tick) and make treats them as up
  to date (missing a change), so builds must run on a RAM or native mount until phase 7
  makes the native format the root; phase 5's recipes put `target/` and build trees there.
- **One readiness word.** `POLL` wakes every poller on every change. Fine for make, Ninja
  and cargo (a handful of pollers); a server with many connections would want per-object
  waiter lists.
- **Kernel stacks of 64 KiB for user threads** need the measurement in 1b.4 to hold; if a
  deep filesystem path doesn't fit, user threads get 128 KiB and the task limit stays the
  same with more memory.
- **AArch64 ASIDs.** With 16-bit ASIDs (the Cortex-A53 has them), one per Mm is simple. A
  CPU with 8-bit ASIDs needs generation-based recycling (`tlbi vmalle1is` at rollover,
  per-CPU active ASIDs); 1b.1 checks `ID_AA64MMFR0_EL1.ASIDBits`, implements it, and
  `ktest asid-rollover` forces that path on every CPU.
- **The cooperative kill** (1a.2) changes when a user task can die: a call that waits
  without polling `kill_pending` would make a process unkillable. Every blocking primitive
  reachable from a call must take the hook, and `kill-mid-call-storm` plus each stage's
  `*-killed` probes are the check. Disk transfers are the bounded exception.
- **Strict fork charging** makes forking a large process cost its whole writable footprint
  in promises. The jobserver arm (10.2) keeps cargo and rustc off `fork`; if another
  large forker turns up, the fallback is Linux's heuristic for forked pages only (charge a
  copy when it happens and end the child with `SIGBUS` if it can't), which keeps every
  other promise exact.
- **Write-back of shared mappings is synchronous only** (1a.5): a crash loses what wasn't
  synced. That's POSIX, and SQLite and `sem_open` don't need more; background write-back is
  phase 7 work.
- **x86_64 MMIO above `USER_BASE`.** q35's 64-bit PCI hole sits above RAM; with a large
  guest it can reach 64 GiB. 1a.4's boot check catches it; the fix then is moving the
  window up a PML4 entry, which needs a fourth level in `libs/vm`.
- **`/bin/sh` as a user program** is stage 1c.12, built over a small runtime of its own;
  once relibc exists (phase 2) it can move onto it, or a ported shell can replace it.
- **Signals are the riskiest piece for isolation**: the kernel writes frames to user
  stacks and reads registers back. 1c.9's host tests of the frame builder and checker on
  both layouts, the whitelist rebuild, and its `sigreturn-*` and `toctou-sigreturn` probes
  are the defence; it deserves its own review.
