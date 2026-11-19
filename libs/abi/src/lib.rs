//! The contract between the kernel and user programs: system call numbers, error
//! codes, flags and where things are in a process's address space.
//!
//! Both sides build from this crate. It's a stable ABI: a number here never changes
//! its meaning, and new calls take new numbers.
//!
//! # Making a call
//!
//! | | AArch64 | x86_64 |
//! |---|---|---|
//! | Trap | `svc #0` | `int $0xf0` |
//! | Number | `x8` | `rax` |
//! | Arguments | `x0`..`x5` | `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9` |
//! | Result | `x0` | `rax` |
//!
//! Every other register comes back as it was. A result in `-4095..0` is a negated
//! error code; anything else is a success.
//!
//! # Starting a program
//!
//! A program starts at its ELF entry point with the argument count in the first
//! argument register, a pointer to `argc` [`UserStr`]s in the second, and a 16-byte
//! aligned stack pointer at the System V initial stack: `argc`, the `argv` pointers and a
//! zero, the `envp` pointers and a zero, then the aux vector ([`aux`]), ending with
//! [`aux::NULL`]. The strings, the [`UserStr`]s and the stack pointer are in the
//! argument area at the top of the window ([`USER_ARGS_SIZE`] below [`USER_STACK_TOP`]),
//! and the [`USER_STACK_SIZE`] bytes below that are the stack's, untouched.

#![cfg_attr(not(test), no_std)]

/// The revision of this interface. 2 is the layout below (a 512 GiB window, an
/// argument area of its own and an 8 MiB main stack); 1 was the 16 GiB window
/// (64..80 GiB) with a 128 KiB stack. Only the layout constants differ. `SYSINFO` and
/// the aux vector's [`aux::FEOS_ABI`] report it, with [`FEATURES`].
pub const ABI_VERSION: u32 = 2;

/// The stage feature mask (section 1 and the table in section 12 of
/// `docs/feos-abi-v2.md`): bit `n` is set once stage `n` is in. Bits are never reused or
/// cleared. Up to bit 20, stage 1c.10 (masks, `THREAD_KILL`, alternate stacks,
/// interruption and restart, `SETITIMER`).
pub const FEATURES: u64 = (1 << 21) - 1;

/// Most children a process may have that it hasn't reaped, running or ended: past this
/// `SPAWN2` (and `FORK`) are `EAGAIN`.
pub const MAX_CHILDREN: usize = 1024;

/// Lowest address a user program can have mapped.
pub const USER_BASE: usize = 0x10_0000_0000;
/// One past the highest address a user program can have mapped: the whole of the
/// 3-level tree both ports give a process, 512 GiB.
pub const USER_END: usize = 0x80_0000_0000;
/// One past the program image: an `ET_EXEC` program links in `USER_BASE..USER_IMAGE_END`.
pub const USER_IMAGE_END: usize = 0x11_0000_0000;
/// Where an `ET_DYN` program's lowest segment is placed.
pub const USER_PIE_BASE: usize = USER_BASE;
/// Where an interpreter's lowest segment is placed, in a 1 GiB area of its own.
pub const USER_INTERP_BASE: usize = USER_IMAGE_END;
/// Where `mmap` hands out memory from, above the interpreter's area.
pub const USER_MMAP_BASE: usize = 0x11_4000_0000;
/// One past the top of the argument area. There's an unmapped page above it.
pub const USER_STACK_TOP: usize = USER_END - 0x1000;
/// Bytes of the argument area, at the top: the strings and the pointers to them a
/// program starts with, kept apart so that however many there are, they never take
/// from the stack below.
pub const USER_ARGS_SIZE: usize = 2 * 1024 * 1024;
/// Bytes of the main stack, directly below the argument area: promised from the start
/// and given frames on first touch, so all of it is free at `main`.
pub const USER_STACK_SIZE: usize = 8 * 1024 * 1024;
/// Unmapped space below the stack, so that an overflow faults instead of reaching
/// anything `mmap` handed out.
pub const USER_STACK_GUARD: usize = 1024 * 1024 * 1024;
/// One past the addresses `mmap` hands out.
pub const USER_MMAP_END: usize = USER_STACK_TOP - USER_ARGS_SIZE - USER_STACK_SIZE - USER_STACK_GUARD;

/// Bytes of a page: the granularity of `mmap` and of an ELF segment's placement.
pub const PAGE_SIZE: usize = 4096;
/// Most memory one process can have charged or owned, in bytes: pages promised to it,
/// frames it has that no promise covers, and its page tables. A reservation with no
/// access, or one made with `MAP_NORESERVE`, isn't charged, so it can be larger. A
/// machine with less to spare says `ENOMEM` sooner.
pub const USER_MEMORY_LIMIT: usize = 64 * 1024 * 1024 * 1024;
/// Most mappings one process can have: each `mmap` adds one, unless it joins a
/// neighbour, and a `munmap` or `mprotect` in the middle of one splits it.
pub const MAX_VMAS: usize = 16384;
/// Most kernel heap, in bytes, one process's shared-anonymous objects may hold in the
/// part sized by the object rather than by what's touched: a small header each, and a
/// pointer per 128 KiB of the object. Room for objects totalling about
/// [`USER_MEMORY_LIMIT`], or `MAX_VMAS` small ones; past it, `mmap` of shared memory is
/// `ENOMEM`.
pub const OBJECT_HEAP_LIMIT: usize = 4 * 1024 * 1024;
/// Kernel heap one process's calls can hold for it beyond what has a budget of its own
/// ([`OBJECT_HEAP_LIMIT`]): from stage 1c.1, its open file descriptions and the paths
/// they remember. Room for all [`MAX_FDS`] descriptors with paths of about 2000 bytes, or
/// about 500 with paths of [`PATH_MAX`]; past it, the call that would make one more is
/// `ENOMEM`. Every process's together are held to a quarter of the kernel heap too.
pub const PROCESS_HEAP_LIMIT: usize = 2 * 1024 * 1024;
/// Open files one process can have, standard streams included: descriptors are
/// `0..MAX_FDS`. The table grows as it's used. (16 before stage 1c.1.)
pub const MAX_FDS: usize = 1024;
/// Longest path a call accepts, in bytes. (192 before stage 1c.1.)
pub const PATH_MAX: usize = 4096;
/// Longest name of one entry in a directory.
pub const NAME_MAX: usize = 255;
/// Most bytes a write to a pipe moves in one piece: a write of at most this many is
/// never interleaved with another writer's.
pub const PIPE_BUF: usize = 4096;
/// Most arguments v1's `spawn` accepts.
pub const MAX_ARGS: usize = 32;
/// Most bytes a `read` or `write` moves through the kernel at once. One on a regular file
/// goes on in pieces this big until it's whole, up to the end of the file for a read and
/// until the volume is full for a write (stage 1c.1); before that, a longer one came back
/// short. One on a pipe or the console may still come back short.
pub const IO_CHUNK: usize = 1024 * 1024;
/// Longest a side of a window surface may be, in pixels.
pub const SURFACE_MAX_SIDE: usize = 1920;

/// Exit status of a process a CPU fault ended.
pub const EXIT_FAULTED: i32 = 139;
/// Exit status of a process ended because a page it touched couldn't be given a frame:
/// a `MAP_NORESERVE` page on a machine with none to spare. `128 + SIGBUS`.
pub const EXIT_BUS: i32 = 135;
/// Exit status when the file could be read but isn't a program this machine can run.
pub const EXIT_NOT_EXECUTABLE: i32 = 126;
/// Exit status when there's no such file.
pub const EXIT_NOT_FOUND: i32 = 127;

/// System call numbers.
pub mod nr {
    /// `exit(status: i32) -> !`: ends the process, every thread of it, with `status`.
    pub const EXIT: usize = 0;
    /// `write(fd, buffer, length) -> count`
    pub const WRITE: usize = 1;
    /// `read(fd, buffer, length) -> count`, where zero is end of file.
    pub const READ: usize = 2;
    /// `open(path, path_length, flags) -> fd`, flags from [`open`](super::open).
    ///
    /// Superseded by [`OPENAT`]: this is `openat(AT_FDCWD, path, path_length, flags
    /// translated, 0o666)`, with v1's error numbers (`EINVAL` for a component that isn't a
    /// directory).
    pub const OPEN: usize = 3;
    /// `close(fd) -> 0`
    pub const CLOSE: usize = 4;
    /// `sleep(milliseconds) -> 0`
    ///
    /// Superseded by [`NANOSLEEP`], which is this with a relative `CLOCK_MONOTONIC`
    /// request in nanoseconds.
    pub const SLEEP: usize = 5;
    /// `yield() -> 0`
    pub const YIELD: usize = 6;
    /// `getpid() -> pid`: the process's, which is its first thread's id, whichever thread asks.
    pub const GETPID: usize = 7;
    /// `mmap(length, prot) -> address`: zeroed, private, anonymous memory. `prot` is
    /// from [`prot`](super::prot), or zero for none, as a guard. The memory is promised
    /// when this returns, whatever `prot` says, and gets its frames when it's first
    /// touched.
    ///
    /// Superseded by [`MMAP2`], which is this with `MAP_PRIVATE | MAP_ANONYMOUS`, except
    /// that `MMAP2` promises only the pages that could need a frame.
    pub const MMAP: usize = 8;
    /// `munmap(address, length) -> 0`: any of the program's mappings but the window
    /// surface, holes and all. What's unmapped faults when it's next touched.
    pub const MUNMAP: usize = 9;
    /// `spawn(path, path_length, argv, argc, stdin_fd, stdout_fd) -> pid`, where
    /// `argv` points at `argc` [`UserStr`](super::UserStr)s and a stream of
    /// [`INHERIT`](super::INHERIT) inherits this process's. A program that's missing or
    /// can't be run still gives a pid, whose child exits 127 or 126.
    ///
    /// Superseded by [`SPAWN2`]: this is `SPAWN2` with an empty environment and a child
    /// table of exactly `{0: stdin_fd or inherited, 1: stdout_fd or inherited, 2:
    /// inherited stderr}`, keeping v1's errors (a full table `EBUSY`, too many or too long
    /// arguments `ENAMETOOLONG`).
    pub const SPAWN: usize = 10;
    /// `wait(pid) -> status`: this process's children only. The status is the exit code,
    /// or 128 plus the signal that ended it (139 for a fault).
    ///
    /// Superseded by [`WAIT4`]: this is `wait4(pid, ..., 0, 0)` for one child, returning
    /// that status rather than writing the POSIX one.
    pub const WAIT: usize = 11;
    /// `futex_wait(address, expected, timeout_ms) -> 0`, or `EAGAIN` if the word at
    /// `address` isn't `expected`, or `ETIMEDOUT`. A timeout of zero is none.
    ///
    /// Superseded by [`FUTEX_WAIT2`], which takes an absolute deadline on a clock.
    pub const FUTEX_WAIT: usize = 12;
    /// `futex_wake(address, count) -> woken`
    pub const FUTEX_WAKE: usize = 13;
    /// `pipe(fds: *mut [u32; 2]) -> 0`: read end, then write end. `EMFILE` with fewer
    /// than two descriptors free, `ENFILE` with as many pipes alive as one process may
    /// have made (64, wherever their ends have gone) or the machine's pool full.
    ///
    /// Superseded by [`PIPE2`], which is this with flags; this is `pipe2(fds, 0)`.
    pub const PIPE: usize = 14;
    /// `seek(fd, offset, whence) -> position`, `whence` from [`seek`](super::seek).
    /// `ESPIPE` on a pipe or the console (`EBADF` before stage 1c.1).
    pub const SEEK: usize = 15;
    /// `getcpu() -> cpu`
    pub const GETCPU: usize = 16;
    /// `uptime() -> milliseconds`
    ///
    /// Superseded by [`CLOCK_GETTIME`] of `CLOCK_MONOTONIC`, in nanoseconds.
    pub const UPTIME: usize = 17;
    /// `surface_create(width, height) -> address`: a window surface of 32-bit
    /// blue-first pixels, one per process, shown by whichever window hosts it.
    pub const SURFACE_CREATE: usize = 18;
    /// `surface_present() -> 0`: the surface is ready to show.
    pub const SURFACE_PRESENT: usize = 19;
    /// `poll_event(event: *mut Event) -> 0 or 1`: the window's next input event.
    pub const POLL_EVENT: usize = 20;
    /// `getuid() -> uid`
    pub const GETUID: usize = 21;
    /// `getgid() -> gid`
    pub const GETGID: usize = 22;
    /// `setuid(uid, gid) -> 0`: only root may, and it's for good. A process and what it
    /// starts have the same user.
    pub const SETUID: usize = 23;
    /// `stat(path, length, out: *mut Stat) -> 0`
    ///
    /// Superseded by [`FSTATAT`], which fills a [`Stat2`](super::Stat2).
    pub const STAT: usize = 24;
    /// `chmod(path, length, mode) -> 0`: the owner or root.
    ///
    /// Superseded by [`FCHMODAT`], which takes a `dirfd` and works on a descriptor.
    pub const CHMOD: usize = 25;
    /// `chown(path, length, uid, gid) -> 0`: root, and [`KEEP`](crate::KEEP) leaves one as it is.
    ///
    /// Superseded by [`FCHOWNAT`], which takes a `dirfd`, a link itself and a descriptor.
    pub const CHOWN: usize = 26;
    /// `mkdir(path, length) -> 0`
    ///
    /// Superseded by [`MKDIRAT`], which takes a mode.
    pub const MKDIR: usize = 27;
    /// `unlink(path, length) -> 0`: a file, link or empty directory.
    ///
    /// Superseded by [`UNLINKAT`], which says which of the two it's to remove and keeps a
    /// file in use working.
    pub const UNLINK: usize = 28;
    /// `link(existing, length, path, length) -> 0`: another name for a file.
    ///
    /// Superseded by [`LINKAT`], which takes `dirfd`s and says whether to follow a link.
    pub const LINK: usize = 29;
    /// `mprotect(address, length, prot) -> 0`: changes what can be done with any of the
    /// program's mappings but the window surface. `prot` is from [`prot`](super::prot),
    /// and zero is no access at all; a page keeps what's in it either way. `ENOMEM` if
    /// part of the range isn't mapped, and nothing changes. Only the program's own code,
    /// never written, may be executable: `EACCES` for anything else.
    pub const MPROTECT: usize = 30;
    /// `mmap2(hint, length, prot, flags, fd, offset) -> address`, flags from
    /// [`map`](super::map). Only anonymous memory so far: a file is `ENODEV`.
    pub const MMAP2: usize = 31;
    /// `madvise(address, length, advice) -> 0`, advice from [`madv`](super::madv).
    pub const MADVISE: usize = 32;
    /// `msync(address, length, flags) -> 0`, flags from [`ms`](super::ms). Nothing can
    /// be dirty yet, so it only checks its arguments.
    pub const MSYNC: usize = 33;
    /// `thread_create(entry, stack_top, tls, arg, clear_word) -> tid`: a new thread of
    /// this process, at `entry` (executable) with `arg` in the first argument register,
    /// every other register zero, `stack_top` (16-byte aligned) as its stack pointer and
    /// `tls` (zero or in the window) as its thread pointer. When it ends and the process
    /// carries on, a zero `u32` is written to `clear_word` (zero for none, else 4-byte
    /// aligned in the window) and its futex waiters are woken. `EAGAIN` at the
    /// process's thread cap or with no task free.
    pub const THREAD_CREATE: usize = 34;
    /// `thread_exit(unmap_address, unmap_length) -> !`: ends the calling thread, having
    /// written its `clear_word` and then unmapped the range, which is page aligned and
    /// something `MUNMAP` could take (`EINVAL`, and the call returns), or empty. The
    /// process ends with status 0 with its last thread. [`EXIT`] ends every thread.
    pub const THREAD_EXIT: usize = 35;
    /// `gettid() -> tid`: the first thread's is the process's pid.
    pub const GETTID: usize = 36;
    /// `set_tls(value) -> 0`: the calling thread's thread pointer, `TPIDR_EL0` or the FS
    /// base, zero or in the window (`EINVAL`).
    pub const SET_TLS: usize = 37;
    /// `futex_wait2(address, expected, deadline, clock, flags) -> 0`: as [`FUTEX_WAIT`],
    /// `EAGAIN` if the aligned `u32` at `address` isn't `expected` (a 32-bit argument), but
    /// with `deadline` zero for none or pointing at a [`Timespec`](super::Timespec) holding
    /// an absolute time on `clock`, [`clock::MONOTONIC`](super::clock::MONOTONIC) or
    /// [`clock::REALTIME`](super::clock::REALTIME). `ETIMEDOUT` once it passes. A realtime
    /// deadline is made monotonic when the call starts. `flags` is zero. A wake may be
    /// spurious.
    pub const FUTEX_WAIT2: usize = 38;
    /// `clock_gettime(clock, out: *mut Timespec) -> 0`, clocks from [`clock`](super::clock).
    pub const CLOCK_GETTIME: usize = 39;
    /// `clock_getres(clock, out: *mut Timespec) -> 0`: the clock's resolution, or only
    /// whether it exists if `out` is zero.
    pub const CLOCK_GETRES: usize = 40;
    /// `nanosleep(clock, flags, request: *const Timespec, remain: *mut Timespec) -> 0`:
    /// `clock_nanosleep`. `clock` is `MONOTONIC`, `REALTIME` or `BOOTTIME`
    /// (`PROCESS_CPUTIME_ID` is `EOPNOTSUPP`, anything else `EINVAL`), `flags` zero or
    /// [`timer::ABSTIME`](super::timer::ABSTIME). `EINTR` when a signal handler runs, with
    /// the time left of a relative sleep in `remain` if that isn't zero.
    pub const NANOSLEEP: usize = 41;
    /// `openat(dirfd, path, path_length, flags, mode) -> fd`: flags from [`o`](super::o),
    /// a relative path from `dirfd` ([`at::FDCWD`](super::at::FDCWD) for the working
    /// directory). The lowest free descriptor. `mode` (with `O_CREAT`) is masked by
    /// `0o7777` and the umask.
    ///
    /// Supersedes [`OPEN`], which is `openat(AT_FDCWD, path, length, its flags translated,
    /// 0o666)`.
    pub const OPENAT: usize = 42;
    /// `fstatat(dirfd, path, path_length, out: *mut Stat2, flags) -> 0`, flags from
    /// [`at`](super::at): `SYMLINK_NOFOLLOW`, `EMPTY_PATH` (with `path_length` 0: what
    /// `dirfd` is, any descriptor) and `NO_AUTOMOUNT` (no effect). Fills a
    /// [`Stat2`](super::Stat2).
    pub const FSTATAT: usize = 43;
    /// `getdents(fd, buffer, length) -> bytes`: [`Dirent`](super::Dirent) records, as many
    /// whole ones as fit, from a snapshot the first call takes; zero at the end. `SEEK`
    /// on the descriptor works in the records' cookies.
    pub const GETDENTS: usize = 44;
    /// `mkdirat(dirfd, path, path_length, mode) -> 0`.
    pub const MKDIRAT: usize = 45;
    /// `unlinkat(dirfd, path, path_length, flags) -> 0`: a non-directory, or with
    /// [`at::REMOVEDIR`](super::at::REMOVEDIR) an empty directory. A file still in use
    /// keeps working until it's let go.
    pub const UNLINKAT: usize = 46;
    /// `renameat(from: *const PathAt, to: *const PathAt, flags) -> 0`, flags zero or
    /// [`rename::NOREPLACE`](super::rename::NOREPLACE) (`EEXIST` if `to` is there).
    /// Neither last name is followed. What `to` named is replaced in one step, crash or
    /// no crash, and goes as `UNLINKAT` takes it, so a file in use keeps working: a file
    /// by a file, a directory by an empty directory. `EXDEV` across mounts, `EINVAL` for
    /// a directory under itself, `EBUSY` for a mount point.
    pub const RENAMEAT: usize = 47;
    /// `linkat(existing: *const PathAt, new: *const PathAt, flags) -> 0`, flags from
    /// [`at`](super::at): `SYMLINK_FOLLOW` (else a link is linked itself), `EMPTY_PATH`
    /// (`existing` is its `dirfd`'s file). `EPERM` for a directory, `EXDEV` across mounts,
    /// `EOPNOTSUPP` where the filesystem has no hard links.
    pub const LINKAT: usize = 48;
    /// `symlinkat(target, target_length, dirfd, path, path_length) -> 0`: `target` is a
    /// path by the path rule, not checked to exist.
    pub const SYMLINKAT: usize = 49;
    /// `readlinkat(dirfd, path, path_length, buffer, length) -> bytes`: the link's target,
    /// no NUL, cut short at `length`. `EINVAL` if it isn't a link or `length` is zero.
    /// `path_length` zero reads the link an `O_PATH | O_NOFOLLOW` descriptor refers to.
    pub const READLINKAT: usize = 50;
    /// `fchmodat(dirfd, path, path_length, mode, flags) -> 0`, flags `EMPTY_PATH`
    /// (`fchmod`); `SYMLINK_NOFOLLOW` is `EOPNOTSUPP`. The owner or root.
    pub const FCHMODAT: usize = 51;
    /// `fchownat(dirfd, path, path_length, uid, gid, flags) -> 0`: `uid` and `gid` are
    /// 32-bit, and `-1` ([`KEEP`](crate::KEEP) either width) leaves one alone. Flags
    /// `SYMLINK_NOFOLLOW` and `EMPTY_PATH`. Root only, unless both are left alone.
    pub const FCHOWNAT: usize = 52;
    /// `utimensat(dirfd, path, path_length, times: *const [Timespec; 2], flags) -> 0`:
    /// access then modification, zero for both now; a `nsec` of
    /// [`utime::NOW`](super::utime::NOW) or [`utime::OMIT`](super::utime::OMIT). Flags
    /// `SYMLINK_NOFOLLOW` and `EMPTY_PATH`. Kept to what the filesystem keeps.
    pub const UTIMENSAT: usize = 53;
    /// `faccessat(dirfd, path, path_length, mode, flags) -> 0`: `mode` from
    /// [`access`](super::access), flags `EACCESS`, `SYMLINK_NOFOLLOW`, `EMPTY_PATH`.
    pub const FACCESSAT: usize = 54;
    /// `fstatvfs(fd, out: *mut StatVfs) -> 0`: the filesystem `fd`'s file is on. A pipe or
    /// the console is `EINVAL`.
    pub const FSTATVFS: usize = 67;
    /// `sync() -> 0`: every change so far made durable.
    pub const SYNC: usize = 68;
    /// `flock(fd, operation) -> 0`: [`lock::SH`](super::lock::SH),
    /// [`lock::EX`](super::lock::EX) or [`lock::UN`](super::lock::UN), optionally with
    /// [`lock::NB`](super::lock::NB) (`EAGAIN` on a conflict instead of waiting). The lock
    /// is the open file description's, shared by its descriptors, and goes with its last
    /// one. A regular file or a directory, any access mode (`EINVAL` for anything else);
    /// `ENOLCK` past the lock table's caps.
    pub const FLOCK: usize = 69;
    /// `ftruncate(fd, length) -> 0`: a regular file open for writing.
    pub const FTRUNCATE: usize = 55;
    /// `fsync(fd, flags) -> 0`: flags zero, or [`FSYNC_DATA`](super::FSYNC_DATA) for
    /// `fdatasync`. A regular file or a directory.
    pub const FSYNC: usize = 56;
    /// `pread(fd, buffer, length, offset) -> count`: [`READ`] at `offset`, the
    /// description's own offset untouched.
    pub const PREAD: usize = 57;
    /// `pwrite(fd, buffer, length, offset) -> count`: [`WRITE`] at `offset`, even on an
    /// `O_APPEND` descriptor (POSIX, not Linux).
    pub const PWRITE: usize = 58;
    /// `fchdir(fd) -> 0`: the directory `fd` refers to becomes the working directory.
    pub const FCHDIR: usize = 62;
    /// `getcwd(buffer, length) -> bytes`: the working directory, canonical, with no NUL.
    /// `ERANGE` if it doesn't fit.
    pub const GETCWD: usize = 63;
    /// `umask(mask) -> old`: `mask & 0o777`; other bits are `EINVAL`.
    pub const UMASK: usize = 64;
    /// `fd_path(fd, buffer, length) -> bytes`: the canonical path `fd`'s file or directory
    /// was opened by, with no NUL. `ERANGE` if it doesn't fit; `ENOENT` for a pipe.
    pub const FD_PATH: usize = 65;
    /// `current_exe(buffer, length) -> bytes`: the canonical path of the program this
    /// process runs, with no NUL. `ERANGE` if it doesn't fit.
    pub const CURRENT_EXE: usize = 66;
    /// `dup3(old, new, flags) -> new`: `new` refers to `old`'s open file description,
    /// closed first if it was open. `flags` is zero or [`o::CLOEXEC`](super::o::CLOEXEC).
    /// `old == new` is `EINVAL`, `new` not below [`MAX_FDS`](super::MAX_FDS) `EBADF`.
    pub const DUP3: usize = 59;
    /// `fcntl(fd, command, arg) -> value`, commands from [`f`](super::f): duplicating,
    /// the close-on-exec flag, and the status flags (`O_APPEND` and `O_NONBLOCK` change,
    /// the rest of what's defined is ignored, anything else is `EINVAL`), and POSIX
    /// record locks: `F_GETLK`, `F_SETLK` and `F_SETLKW`, whose `arg` points at a
    /// [`Flock`](super::Flock). A process's record locks on a file go when it closes any
    /// descriptor for that file, or ends; `EAGAIN` on a conflict (`F_SETLK`), `EBADF` for a
    /// read lock on a descriptor not open for reading or a write lock on one not open for
    /// writing, `ENOLCK` past [`MAX_LOCKS`](super::MAX_LOCKS) machine-wide or
    /// [`MAX_PROCESS_LOCKS`](super::MAX_PROCESS_LOCKS) held by one process.
    pub const FCNTL: usize = 60;
    /// `pipe2(fds: *mut [i32; 2], flags) -> 0`: as [`PIPE`], with `flags` any of
    /// [`o::CLOEXEC`](super::o::CLOEXEC) and [`o::NONBLOCK`](super::o::NONBLOCK).
    pub const PIPE2: usize = 61;
    /// `close_range(first, last, flags) -> 0`: closes every open descriptor from `first`
    /// to `last` (32-bit; `last` may be `u32::MAX`), or with
    /// [`close_range::CLOEXEC`](super::close_range::CLOEXEC) marks them close-on-exec.
    /// `first > last` and any other flag are `EINVAL`.
    pub const CLOSE_RANGE: usize = 107;
    /// `spawn2(request: *const SpawnRequest) -> pid`: starts a program in a new process,
    /// in `posix_spawn`'s shape (see [`SpawnRequest`](super::SpawnRequest)). Everything
    /// that can fail (the request, the actions, the program) fails in the call, before
    /// the child exists. Supersedes [`SPAWN`].
    pub const SPAWN2: usize = 72;
    /// `exec(request: *const ExecRequest) -> !`: replaces the calling process's image with
    /// the program [`ExecRequest`](super::ExecRequest) names, a `#!` script's interpreter
    /// included. It returns only with an error, from checking the request, resolving the
    /// program or building its image; past that, every other thread has gone, close-on-exec
    /// descriptors are closed, and the caller runs the new program with its tid, pid,
    /// parent, group, session, working directory, umask and credentials.
    pub const EXEC: usize = 73;
    /// `wait4(pid, status: *mut i32, options, rusage: *mut Rusage) -> pid`: `pid` (32-bit)
    /// positive for that child, `-1` for any, `0` for any in the caller's group, below
    /// `-1` for any in group `-pid`. `options` are [`wait`](super::wait)'s bits (others
    /// `EINVAL`). Writes the POSIX status ([`wait::exited`](super::wait::exited) and the
    /// rest) and the child's resource use where the pointers aren't zero. An end reaps the
    /// child; a stop (with `WUNTRACED`) or continue (with `WCONTINUED`) is reported once
    /// and doesn't. `0` with `WNOHANG` if nothing has changed; `ECHILD` if no child matches.
    pub const WAIT4: usize = 74;
    /// `getppid() -> pid`: the parent's pid, or 1 once the parent has ended.
    pub const GETPPID: usize = 75;
    /// `getpgid(pid) -> pgid`, `pid` (32-bit) 0 for the caller; `ESRCH` for no process.
    pub const GETPGID: usize = 76;
    /// `setpgid(pid, pgid) -> 0`, both 32-bit: `pid` 0 is the caller, `pgid` 0 is `pid`.
    /// The target is the caller or a child of it (`ESRCH`) in its session (`EPERM`), not a
    /// session leader (`EPERM`); `pgid` is `pid` or a group in the session (`EPERM`).
    pub const SETPGID: usize = 77;
    /// `getsid(pid) -> sid`, `pid` (32-bit) 0 for the caller; `ESRCH` for no process.
    pub const GETSID: usize = 78;
    /// `setsid() -> sid`: a new session and group led by the caller, `EPERM` if it leads
    /// a group already.
    pub const SETSID: usize = 79;
    /// `getrusage(who, out: *mut Rusage) -> 0`, `who` (32-bit) one of [`rusage`](super::rusage).
    pub const GETRUSAGE: usize = 80;
    /// `sysinfo(out: *mut SysInfo, size) -> 0`: copies `min(size, size_of::<SysInfo>())`
    /// bytes of a [`SysInfo`](super::SysInfo).
    pub const SYSINFO: usize = 81;
    /// `kill(pid, signal) -> 0`: both 32-bit. `pid` positive for a process, `0` for the
    /// caller's group, `-1` for every process the caller may signal but itself, below `-1`
    /// for group `-pid`; `signal` (`1..=64`, or 0 to check only) runs the target's handler
    /// for it, or is ignored, or takes its default action ([`sig`](super::sig)). Root, or
    /// the target's own user, may (`EPERM`); kernel tasks can't be signalled (`EPERM`). An ended child not yet reaped exists, and does
    /// nothing; a thread id that isn't a process id is `ESRCH`, as is nothing matching.
    pub const KILL: usize = 82;
    /// `thread_kill(tid, signal) -> 0`: both 32-bit. `signal` (`1..=64`, or 0 to check
    /// only) for the thread `tid` of the caller's own process, pending on that thread
    /// alone (`ESRCH` for any other); otherwise as [`KILL`].
    pub const THREAD_KILL: usize = 83;
    /// `sigaction(signal, new: *const SigAction, old: *mut SigAction) -> 0`: either pointer
    /// may be zero. `signal` (32-bit) `1..=64`; `SIGKILL` and `SIGSTOP` can't be given a
    /// new one (`EINVAL`). A handler needs [`sa::RESTORER`](super::sa::RESTORER)
    /// (`EINVAL`), and it and the restorer must be in the user window (`EFAULT`); unknown
    /// flags are `EINVAL`. `SIG_IGN` (or a default that ignores) discards the signal if
    /// it's pending.
    pub const SIGACTION: usize = 84;
    /// `sigprocmask(how, set: *const u64, old: *mut u64) -> 0`: the calling thread's
    /// mask. `how` (32-bit) is one of [`how`](super::how)'s (`EINVAL`), ignored when
    /// `set` is zero; either pointer may be zero. `SIGKILL` and `SIGSTOP` are never
    /// blocked. A pending signal it unblocks is delivered before the call returns.
    pub const SIGPROCMASK: usize = 85;
    /// `sigaltstack(new: *const SigStack, old: *mut SigStack) -> 0`: the calling thread's
    /// alternate stack. `new.flags` 0 or [`ss::DISABLE`](super::ss::DISABLE) (`EINVAL`),
    /// `size` at least [`ss::MINSIGSTKSZ`](super::ss::MINSIGSTKSZ) (`ENOMEM`), the range
    /// in the window (`EFAULT`); `EPERM` while on it.
    pub const SIGALTSTACK: usize = 86;
    /// `sigpending(out: *mut u64) -> 0`: the signals pending for the thread or its
    /// process that the thread blocks.
    pub const SIGPENDING: usize = 87;
    /// `sigsuspend(mask: *const u64) -> EINTR`: waits with `*mask` as the thread's mask
    /// until a handler has run, then puts the mask back. Always `EINTR`.
    pub const SIGSUSPEND: usize = 88;
    /// `sigreturn(context: *const UContext) -> !`: what a handler's restorer calls, with
    /// the stack pointer the handler had at entry. Resumes with the context's registers
    /// and mask, rebuilt from a whitelist; a context that fails a check ends the process
    /// with `SIGSEGV`.
    pub const SIGRETURN: usize = 89;
    /// `setitimer(which, new: *const ITimerVal, old: *mut ITimerVal) -> 0`: `which`
    /// (32-bit) [`itimer::REAL`](super::itimer::REAL) only (`EINVAL`), a per-process
    /// deadline on the monotonic clock that raises `SIGALRM`, re-armed by `interval`. A
    /// zero `new` only reads; a zero `value` disarms; `usec` outside `0..1_000_000` (or a
    /// negative `sec`) is `EINVAL`. Kept across `EXEC`, not inherited by `SPAWN2`.
    pub const SETITIMER: usize = 108;
    /// The first number of the calls `docs/feos-abi-v2.md` adds. Every number from here
    /// to [`LAST_RESERVED`] is taken, and is `ENOSYS` until the stage that brings it.
    pub const FIRST_V2: usize = 31;
    /// The last number `docs/feos-abi-v2.md` gives out.
    pub const LAST_RESERVED: usize = 108;
    /// The first reserved number with no call behind it yet.
    pub const FIRST_UNIMPLEMENTED: usize = 70;
    /// The calls past [`FIRST_UNIMPLEMENTED`] that have landed anyway, a stage having
    /// brought them ahead of the numbers before them.
    pub const IMPLEMENTED_LATER: [usize; 20] = [
        CLOSE_RANGE, SPAWN2, EXEC, WAIT4, GETPPID, GETPGID, SETPGID, GETSID, SETSID, GETRUSAGE, SYSINFO, KILL, THREAD_KILL,
        SIGACTION, SIGPROCMASK, SIGALTSTACK, SIGPENDING, SIGSUSPEND, SIGRETURN, SETITIMER,
    ];

    /// Whether `number` is one the design reserves with no call behind it yet.
    pub const fn is_unimplemented(number: usize) -> bool {
        if number < FIRST_UNIMPLEMENTED || number > LAST_RESERVED {
            return false;
        }
        let mut index = 0;
        while index < IMPLEMENTED_LATER.len() {
            if IMPLEMENTED_LATER[index] == number {
                return false;
            }
            index += 1;
        }
        true
    }
}

/// Leaves the owner or group as it is, in [`nr::CHOWN`], and in [`nr::FCHOWNAT`], whose
/// owner and group are 32-bit arguments: there this is `-1`, as is `u32::MAX`, of which
/// it's the sign extension.
pub const KEEP: usize = usize::MAX;

/// Flags for [`nr::RENAMEAT`]. Linux's values.
pub mod rename {
    /// `RENAME_NOREPLACE`: `EEXIST` if the new name is there.
    pub const NOREPLACE: usize = 1;
    /// `RENAME_EXCHANGE`: not offered (`EINVAL`).
    pub const EXCHANGE: usize = 2;
    /// `RENAME_WHITEOUT`: not offered (`EINVAL`).
    pub const WHITEOUT: usize = 4;
}

/// Special `nsec` values for [`nr::UTIMENSAT`]. Linux's.
pub mod utime {
    /// `UTIME_NOW`: the time now.
    pub const NOW: i64 = 0x3fff_ffff;
    /// `UTIME_OMIT`: leave it as it is.
    pub const OMIT: i64 = 0x3fff_fffe;
}

/// Modes for [`nr::FACCESSAT`]. Linux's values.
pub mod access {
    /// `F_OK`: it's there.
    pub const F_OK: usize = 0;
    /// `X_OK`: it may be run, or gone through.
    pub const X_OK: usize = 1;
    /// `W_OK`: it may be written.
    pub const W_OK: usize = 2;
    /// `R_OK`: it may be read.
    pub const R_OK: usize = 4;
    /// Every bit a mode may have.
    pub const ALL: usize = X_OK | W_OK | R_OK;
}

/// Flags of [`StatVfs::flags`]. Linux's values.
pub mod st {
    /// `ST_RDONLY`: the mount refuses writes.
    pub const RDONLY: u64 = 1;
    /// `ST_NOSUID`: always, as nothing runs set-user-ID.
    pub const NOSUID: u64 = 2;
}

/// What [`nr::FSTATVFS`] fills in: Linux's `struct statvfs` on 64-bit, every field a
/// `u64`. Block counts are in `frsize` units. An output struct.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct StatVfs {
    pub bsize: u64,
    pub frsize: u64,
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub favail: u64,
    /// The mount's number, as [`Stat2::dev`].
    pub fsid: u64,
    /// [`st`].
    pub flags: u64,
    /// The longest name, [`NAME_MAX`].
    pub namemax: u64,
}

const _: () = assert!(core::mem::size_of::<StatVfs>() == 88);
const _: () = assert!(core::mem::offset_of!(StatVfs, bsize) == 0);
const _: () = assert!(core::mem::offset_of!(StatVfs, frsize) == 8);
const _: () = assert!(core::mem::offset_of!(StatVfs, blocks) == 16);
const _: () = assert!(core::mem::offset_of!(StatVfs, bfree) == 24);
const _: () = assert!(core::mem::offset_of!(StatVfs, bavail) == 32);
const _: () = assert!(core::mem::offset_of!(StatVfs, files) == 40);
const _: () = assert!(core::mem::offset_of!(StatVfs, ffree) == 48);
const _: () = assert!(core::mem::offset_of!(StatVfs, favail) == 56);
const _: () = assert!(core::mem::offset_of!(StatVfs, fsid) == 64);
const _: () = assert!(core::mem::offset_of!(StatVfs, flags) == 72);
const _: () = assert!(core::mem::offset_of!(StatVfs, namemax) == 80);

impl StatVfs {
    /// Bytes of one, in user memory.
    pub const SIZE: usize = 88;

    /// As it is in memory, on both ports.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        let fields = [
            self.bsize, self.frsize, self.blocks, self.bfree, self.bavail, self.files, self.ffree, self.favail,
            self.fsid, self.flags, self.namemax,
        ];
        for (index, field) in fields.iter().enumerate() {
            out[index * 8..index * 8 + 8].copy_from_slice(&field.to_ne_bytes());
        }
        out
    }
}

impl PathAt {
    /// Bytes of one, in user memory.
    pub const SIZE: usize = 24;

    /// From its bytes in memory, on both ports.
    pub fn from_bytes(bytes: [u8; Self::SIZE]) -> Self {
        let word = |at: usize| {
            let mut out = [0u8; 8];
            out.copy_from_slice(&bytes[at..at + 8]);
            u64::from_ne_bytes(out)
        };
        let half = |at: usize| u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        Self { dirfd: half(0) as i32, reserved: half(4), pointer: word(8), length: word(16) }
    }
}

/// What [`nr::STAT`] fills in.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Stat {
    /// Type and permission bits. On a filesystem with no owners, made up.
    pub mode: u32,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Names this has.
    pub nlink: u32,
    /// Size in bytes.
    pub size: u64,
    /// A number for this file, on its filesystem.
    pub ino: u64,
    /// One when the filesystem keeps owners and modes, and they're enforced.
    pub posix: u32,
    /// Always zero.
    pub reserved: u32,
}

/// Error codes, returned negated.
pub mod errno {
    /// Only the owner or root may do that.
    pub const EPERM: isize = 1;
    /// No such file or directory.
    pub const ENOENT: isize = 2;
    /// No such device: `/dev/tty` for a process with no terminal.
    pub const ENXIO: isize = 6;
    /// The file's permissions say no.
    pub const EACCES: isize = 13;
    /// This filesystem can't do that.
    pub const ENOTSUP: isize = 95;
    /// Input or output failed.
    pub const EIO: isize = 5;
    /// A bad file descriptor.
    pub const EBADF: isize = 9;
    /// No such child.
    pub const ECHILD: isize = 10;
    /// The word didn't hold the expected value.
    pub const EAGAIN: isize = 11;
    /// Out of memory, or over this process's limit.
    pub const ENOMEM: isize = 12;
    /// A pointer that isn't this process's to pass.
    pub const EFAULT: isize = 14;
    /// Something's already there, or still in use.
    pub const EEXIST: isize = 17;
    /// It's a directory.
    pub const EISDIR: isize = 21;
    /// An argument makes no sense.
    pub const EINVAL: isize = 22;
    /// Too many open files in the system: as many pipes as this process may have, or the
    /// machine's pool full.
    pub const ENFILE: isize = 23;
    /// Too many open files.
    pub const EMFILE: isize = 24;
    /// No space left.
    pub const ENOSPC: isize = 28;
    /// Nobody's reading.
    pub const EPIPE: isize = 32;
    /// A path or argument list is too long.
    pub const ENAMETOOLONG: isize = 36;
    /// No such system call.
    pub const ENOSYS: isize = 38;
    /// The wait ran out of time.
    pub const ETIMEDOUT: isize = 110;
    /// Not a program this machine can run.
    pub const ENOEXEC: isize = 8;
    /// No process slot free.
    pub const EBUSY: isize = 16;
    /// That kind of file can't be mapped.
    pub const ENODEV: isize = 19;
    /// The file is mapped to run, so it can't be written; or it's open for writing, so it
    /// can't be mapped to run.
    pub const ETXTBSY: isize = 26;
    /// An offset and length that run past what a file offset can hold.
    pub const EOVERFLOW: isize = 75;
    /// A signal handler ran while the call waited.
    pub const EINTR: isize = 4;
    /// Not supported: Linux's name for [`ENOTSUP`], the same number, which the v2 calls
    /// use.
    pub const EOPNOTSUPP: isize = ENOTSUP;
    /// A seek on something that has no position: a pipe or the console.
    pub const ESPIPE: isize = 29;
    /// A write that would take a file past the largest offset there can be.
    pub const EFBIG: isize = 27;
    /// `EWOULDBLOCK`: Linux's other name for [`EAGAIN`], for a call on a descriptor set
    /// `O_NONBLOCK` that would have had to wait.
    pub const EWOULDBLOCK: isize = EAGAIN;
    /// A component of a path, or a `dirfd`, that isn't a directory.
    pub const ENOTDIR: isize = 20;
    /// The directory isn't empty.
    pub const ENOTEMPTY: isize = 39;
    /// Too many symbolic links in one resolution, or `O_NOFOLLOW` met one.
    pub const ELOOP: isize = 40;
    /// A path's bytes aren't UTF-8.
    pub const EILSEQ: isize = 84;
    /// The mount, or the file, is read-only.
    pub const EROFS: isize = 30;
    /// The answer doesn't fit in the buffer given.
    pub const ERANGE: isize = 34;
    /// Across mounts.
    pub const EXDEV: isize = 18;
    /// No lock to be had: the lock table's caps, machine-wide or the process's.
    pub const ENOLCK: isize = 37;
    /// No such process (stage 1c.6).
    pub const ESRCH: isize = 3;
    /// The arguments and environment are too big (stage 1c.6).
    pub const E2BIG: isize = 7;
}

/// `O_*` flags of the v2 calls, and of `F_GETFL` and `F_SETFL`: Linux's values (the
/// x86_64 ones, on both ports). v1's [`OPEN`](nr::OPEN) takes [`open`] flags instead.
pub mod o {
    /// Every flag `OPENAT` knows, [`TMPFILE`] apart (`EOPNOTSUPP`): [`ASYNC`] and anything
    /// else is `EINVAL`.
    pub const OPENAT_KNOWN: usize = ACCMODE | SETTABLE | FIXED_AT_OPEN;
    /// `O_RDONLY`.
    pub const RDONLY: usize = 0;
    /// `O_WRONLY`.
    pub const WRONLY: usize = 1;
    /// `O_RDWR`.
    pub const RDWR: usize = 2;
    /// `O_ACCMODE`: where the access mode is.
    pub const ACCMODE: usize = 3;
    pub const CREAT: usize = 0o100;
    pub const EXCL: usize = 0o200;
    pub const NOCTTY: usize = 0o400;
    pub const TRUNC: usize = 0o1000;
    pub const APPEND: usize = 0o2000;
    pub const NONBLOCK: usize = 0o4000;
    pub const DSYNC: usize = 0o10000;
    /// `O_ASYNC`: refused, by `F_SETFL` too.
    pub const ASYNC: usize = 0o20000;
    pub const DIRECT: usize = 0o40000;
    pub const LARGEFILE: usize = 0o100000;
    pub const DIRECTORY: usize = 0o200000;
    pub const NOFOLLOW: usize = 0o400000;
    pub const NOATIME: usize = 0o1000000;
    pub const CLOEXEC: usize = 0o2000000;
    /// `O_SYNC`, which has [`DSYNC`]'s bit in it.
    pub const SYNC: usize = 0o4010000;
    pub const PATH: usize = 0o10000000;
    /// `O_TMPFILE`, which has [`DIRECTORY`]'s bit in it.
    pub const TMPFILE: usize = 0o20200000;
    /// What `F_SETFL` changes.
    pub const SETTABLE: usize = APPEND | NONBLOCK;
    /// What `F_SETFL` accepts and ignores, as POSIX says: every other flag that means
    /// something at open. Any bit outside these and [`SETTABLE`] is `EINVAL`.
    pub const FIXED_AT_OPEN: usize =
        ACCMODE | CREAT | EXCL | NOCTTY | TRUNC | DSYNC | SYNC | DIRECT | LARGEFILE | DIRECTORY | NOFOLLOW | NOATIME | CLOEXEC | PATH;
}

/// Commands and flags for [`nr::FCNTL`]. Linux's values.
pub mod f {
    /// `F_DUPFD`: the lowest free descriptor at or above `arg`.
    pub const DUPFD: usize = 0;
    /// `F_GETFD`: [`FD_CLOEXEC`] or zero.
    pub const GETFD: usize = 1;
    /// `F_SETFD`: `arg` is [`FD_CLOEXEC`] or zero.
    pub const SETFD: usize = 2;
    /// `F_GETFL`: the access mode and status flags.
    pub const GETFL: usize = 3;
    /// `F_SETFL`: `O_APPEND` and `O_NONBLOCK` from `arg`.
    pub const SETFL: usize = 4;
    /// `F_GETLK`: the first lock that would conflict with the [`Flock`](super::Flock)
    /// given, written over it with its owner's pid, or its `kind` set to [`UNLCK`].
    pub const GETLK: usize = 5;
    /// `F_SETLK`: sets or clears the [`Flock`](super::Flock)'s range; `EAGAIN` on a
    /// conflict.
    pub const SETLK: usize = 6;
    /// `F_SETLKW`: [`SETLK`], waiting out a conflict instead. No deadlock detection.
    pub const SETLKW: usize = 7;
    /// `F_RDLCK`: a read (shared) lock, as [`Flock::kind`](super::Flock::kind).
    pub const RDLCK: i16 = 0;
    /// `F_WRLCK`: a write (exclusive) lock.
    pub const WRLCK: i16 = 1;
    /// `F_UNLCK`: no lock; clearing one.
    pub const UNLCK: i16 = 2;
    /// `F_DUPFD_CLOEXEC`: [`DUPFD`], the new descriptor close-on-exec.
    pub const DUPFD_CLOEXEC: usize = 1030;
    /// `FD_CLOEXEC`: the descriptor closes on `EXEC`.
    pub const FD_CLOEXEC: usize = 1;
}

/// Operations for [`nr::FLOCK`]. Linux's values.
pub mod lock {
    /// `LOCK_SH`: a shared lock.
    pub const SH: usize = 1;
    /// `LOCK_EX`: an exclusive lock.
    pub const EX: usize = 2;
    /// `LOCK_NB`: with [`SH`] or [`EX`], `EAGAIN` instead of waiting.
    pub const NB: usize = 4;
    /// `LOCK_UN`: lets go of the lock.
    pub const UN: usize = 8;
}

/// The most record and `flock` locks the machine keeps at once: past this, `ENOLCK`.
pub const MAX_LOCKS: usize = 4096;
/// The most locks one process may hold (its record locks, and the `flock` locks it
/// placed): past this, `ENOLCK`.
pub const MAX_PROCESS_LOCKS: usize = 256;

/// Linux's `struct flock` on 64-bit, for [`f::GETLK`], [`f::SETLK`] and [`f::SETLKW`]. A
/// Linux-layout struct: the reserved fields are padding, ignored on input and written as
/// zero.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Flock {
    /// [`f::RDLCK`], [`f::WRLCK`] or [`f::UNLCK`].
    pub kind: i16,
    /// Where `start` counts from: [`seek`]'s `SET`, `CURRENT` or `END`.
    pub whence: i16,
    pub reserved0: u32,
    pub start: i64,
    /// Bytes from `start`, zero for all of the file from there on; negative for the bytes
    /// before `start`.
    pub length: i64,
    /// The owner of the conflicting lock, from [`f::GETLK`].
    pub pid: i32,
    pub reserved1: u32,
}

const _: () = assert!(core::mem::size_of::<Flock>() == 32);
const _: () = assert!(core::mem::offset_of!(Flock, kind) == 0);
const _: () = assert!(core::mem::offset_of!(Flock, whence) == 2);
const _: () = assert!(core::mem::offset_of!(Flock, reserved0) == 4);
const _: () = assert!(core::mem::offset_of!(Flock, start) == 8);
const _: () = assert!(core::mem::offset_of!(Flock, length) == 16);
const _: () = assert!(core::mem::offset_of!(Flock, pid) == 24);
const _: () = assert!(core::mem::offset_of!(Flock, reserved1) == 28);

impl Flock {
    /// Bytes of one, in user memory.
    pub const SIZE: usize = 32;

    /// From its bytes in memory, on both ports, its padding dropped.
    pub fn from_bytes(bytes: [u8; Self::SIZE]) -> Self {
        let word = |at: usize| {
            let mut out = [0u8; 8];
            out.copy_from_slice(&bytes[at..at + 8]);
            i64::from_ne_bytes(out)
        };
        let half = |at: usize| i16::from_ne_bytes([bytes[at], bytes[at + 1]]);
        let pid = i32::from_ne_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
        Self { kind: half(0), whence: half(2), reserved0: 0, start: word(8), length: word(16), pid, reserved1: 0 }
    }

    /// As it is in memory, on both ports, the padding zero.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..2].copy_from_slice(&self.kind.to_ne_bytes());
        out[2..4].copy_from_slice(&self.whence.to_ne_bytes());
        out[8..16].copy_from_slice(&self.start.to_ne_bytes());
        out[16..24].copy_from_slice(&self.length.to_ne_bytes());
        out[24..28].copy_from_slice(&self.pid.to_ne_bytes());
        out
    }
}

/// Flags for [`nr::CLOSE_RANGE`]. Linux's values.
pub mod close_range {
    /// `CLOSE_RANGE_UNSHARE`: refused, since a descriptor table is never shared between
    /// processes.
    pub const UNSHARE: usize = 2;
    /// `CLOSE_RANGE_CLOEXEC`: mark them close-on-exec instead of closing them.
    pub const CLOEXEC: usize = 4;
}

/// `AT_*` values of the path calls. Linux's.
pub mod at {
    /// `AT_FDCWD`, as a `dirfd`: relative to the working directory. A 32-bit argument, so
    /// `-100` in any width.
    pub const FDCWD: i32 = -100;
    /// `AT_SYMLINK_NOFOLLOW`: the link itself, not what it leads to.
    pub const SYMLINK_NOFOLLOW: usize = 0x100;
    /// `AT_REMOVEDIR`: `UNLINKAT` of an empty directory.
    pub const REMOVEDIR: usize = 0x200;
    /// `AT_EACCESS`: `FACCESSAT` only (stage 1c.4).
    pub const EACCESS: usize = 0x200;
    /// `AT_SYMLINK_FOLLOW`: `LINKAT` only (stage 1c.4).
    pub const SYMLINK_FOLLOW: usize = 0x400;
    /// `AT_NO_AUTOMOUNT`: accepted, and means nothing.
    pub const NO_AUTOMOUNT: usize = 0x800;
    /// `AT_EMPTY_PATH`: with a path of length zero, the file `dirfd` itself.
    pub const EMPTY_PATH: usize = 0x1000;
}

/// File type bits of a mode, `S_IF*`. Linux's.
pub mod s_if {
    /// Where the type is.
    pub const MASK: u32 = 0o170000;
    pub const SOCK: u32 = 0o140000;
    pub const LNK: u32 = 0o120000;
    pub const REG: u32 = 0o100000;
    pub const BLK: u32 = 0o60000;
    pub const DIR: u32 = 0o40000;
    pub const CHR: u32 = 0o20000;
    pub const FIFO: u32 = 0o10000;
}

/// Kinds of [`Dirent`], `DT_*`. Linux's.
pub mod dt {
    pub const UNKNOWN: u8 = 0;
    pub const FIFO: u8 = 1;
    pub const CHR: u8 = 2;
    pub const DIR: u8 = 4;
    pub const REG: u8 = 8;
    pub const LNK: u8 = 10;
    pub const SOCK: u8 = 12;
}

/// Bits of [`Stat2::flags`].
pub mod stat_flags {
    /// The filesystem keeps owners and modes, and they're enforced.
    pub const POSIX: u32 = 1;
    /// `btime` holds the file's creation time.
    pub const BTIME: u32 = 2;
    /// The mount refuses writes.
    pub const READ_ONLY: u32 = 4;
}

/// `FSYNC_DATA`: [`nr::FSYNC`] as `fdatasync`.
pub const FSYNC_DATA: usize = 1;

/// The device number of the console, as [`Stat2::rdev`] gives it: `5 << 8 | 1`.
pub const CONSOLE_RDEV: u64 = 5 << 8 | 1;

/// A path relative to a directory, for calls with two paths and for requests. A request
/// struct: `reserved` must be zero.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct PathAt {
    /// [`at::FDCWD`] or a descriptor.
    pub dirfd: i32,
    pub reserved: u32,
    pub pointer: u64,
    pub length: u64,
}

const _: () = assert!(core::mem::size_of::<PathAt>() == 24);
const _: () = assert!(core::mem::offset_of!(PathAt, dirfd) == 0);
const _: () = assert!(core::mem::offset_of!(PathAt, reserved) == 4);
const _: () = assert!(core::mem::offset_of!(PathAt, pointer) == 8);
const _: () = assert!(core::mem::offset_of!(PathAt, length) == 16);

/// What [`nr::FSTATAT`] fills in. An output struct: the kernel writes the reserved fields
/// as zero.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Stat2 {
    /// The mount's number.
    pub dev: u64,
    /// Stable for the file's life, and not another's while anything refers to it.
    pub ino: u64,
    /// Type and permission bits, [`s_if`].
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    /// Zero, or a device node's number: the console's is [`CONSOLE_RDEV`].
    pub rdev: u64,
    /// Bytes; a link's target length for a link.
    pub size: i64,
    /// The filesystem's cluster or block size, 4096 at least.
    pub blksize: i64,
    /// 512-byte units allocated.
    pub blocks: i64,
    pub atime: Timespec,
    pub mtime: Timespec,
    /// Status change: `mtime` where the filesystem keeps no other (FAT and exFAT).
    pub ctime: Timespec,
    /// Creation, if `flags` has [`stat_flags::BTIME`].
    pub btime: Timespec,
    /// [`stat_flags`].
    pub flags: u32,
    pub reserved0: u32,
    pub reserved1: u64,
}

const _: () = assert!(core::mem::size_of::<Stat2>() == 144);
const _: () = assert!(core::mem::offset_of!(Stat2, dev) == 0);
const _: () = assert!(core::mem::offset_of!(Stat2, ino) == 8);
const _: () = assert!(core::mem::offset_of!(Stat2, mode) == 16);
const _: () = assert!(core::mem::offset_of!(Stat2, nlink) == 20);
const _: () = assert!(core::mem::offset_of!(Stat2, uid) == 24);
const _: () = assert!(core::mem::offset_of!(Stat2, gid) == 28);
const _: () = assert!(core::mem::offset_of!(Stat2, rdev) == 32);
const _: () = assert!(core::mem::offset_of!(Stat2, size) == 40);
const _: () = assert!(core::mem::offset_of!(Stat2, blksize) == 48);
const _: () = assert!(core::mem::offset_of!(Stat2, blocks) == 56);
const _: () = assert!(core::mem::offset_of!(Stat2, atime) == 64);
const _: () = assert!(core::mem::offset_of!(Stat2, mtime) == 80);
const _: () = assert!(core::mem::offset_of!(Stat2, ctime) == 96);
const _: () = assert!(core::mem::offset_of!(Stat2, btime) == 112);
const _: () = assert!(core::mem::offset_of!(Stat2, flags) == 128);
const _: () = assert!(core::mem::offset_of!(Stat2, reserved0) == 132);
const _: () = assert!(core::mem::offset_of!(Stat2, reserved1) == 136);

impl Stat2 {
    /// Bytes of one, in user memory.
    pub const SIZE: usize = 144;

    /// As it is in memory, on both ports: every field in order, no padding.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        let mut at = 0;
        let mut put = |bytes: &[u8]| {
            out[at..at + bytes.len()].copy_from_slice(bytes);
            at += bytes.len();
        };
        put(&self.dev.to_ne_bytes());
        put(&self.ino.to_ne_bytes());
        put(&self.mode.to_ne_bytes());
        put(&self.nlink.to_ne_bytes());
        put(&self.uid.to_ne_bytes());
        put(&self.gid.to_ne_bytes());
        put(&self.rdev.to_ne_bytes());
        put(&self.size.to_ne_bytes());
        put(&self.blksize.to_ne_bytes());
        put(&self.blocks.to_ne_bytes());
        for time in [self.atime, self.mtime, self.ctime, self.btime] {
            put(&time.to_bytes());
        }
        put(&self.flags.to_ne_bytes());
        put(&self.reserved0.to_ne_bytes());
        put(&self.reserved1.to_ne_bytes());
        out
    }
}

/// One record of [`nr::GETDENTS`]'s buffer: Linux's `linux_dirent64`, a header of
/// [`Dirent::HEADER`] bytes then the name, NUL-terminated and zero-padded to `reclen`.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Dirent {
    pub ino: u64,
    /// The cookie of the entry after this one, for `SEEK`.
    pub off: i64,
    /// This record's length, a multiple of 8.
    pub reclen: u16,
    /// [`dt`].
    pub kind: u8,
}

const _: () = assert!(core::mem::offset_of!(Dirent, ino) == 0);
const _: () = assert!(core::mem::offset_of!(Dirent, off) == 8);
const _: () = assert!(core::mem::offset_of!(Dirent, reclen) == 16);
const _: () = assert!(core::mem::offset_of!(Dirent, kind) == 18);

impl Dirent {
    /// Where the name starts.
    pub const HEADER: usize = 19;

    /// The length of a record for a name of `name_length` bytes: header, name, NUL,
    /// rounded up to 8.
    pub const fn record_length(name_length: usize) -> usize {
        (Self::HEADER + name_length + 1).next_multiple_of(8)
    }

    /// Writes the record for `name` at the start of `out`, and says how long it is;
    /// `None` if it doesn't fit, or the name is longer than [`NAME_MAX`].
    pub fn encode(ino: u64, off: i64, kind: u8, name: &[u8], out: &mut [u8]) -> Option<usize> {
        if name.len() > NAME_MAX {
            return None;
        }
        let length = Self::record_length(name.len());
        let record = out.get_mut(..length)?;
        record[..8].copy_from_slice(&ino.to_ne_bytes());
        record[8..16].copy_from_slice(&off.to_ne_bytes());
        record[16..18].copy_from_slice(&(length as u16).to_ne_bytes());
        record[18] = kind;
        record[Self::HEADER..Self::HEADER + name.len()].copy_from_slice(name);
        record[Self::HEADER + name.len()..].fill(0);
        Some(length)
    }

    /// Reads the record at the start of `bytes`: the header and the name, if it's whole.
    pub fn decode(bytes: &[u8]) -> Option<(Self, &[u8])> {
        if bytes.len() < Self::HEADER {
            return None;
        }
        let word = |at: usize| {
            let mut out = [0u8; 8];
            out.copy_from_slice(&bytes[at..at + 8]);
            out
        };
        let reclen = u16::from_ne_bytes([bytes[16], bytes[17]]);
        let record = bytes.get(..usize::from(reclen))?;
        let tail = record.get(Self::HEADER..)?;
        let name_length = tail.iter().position(|&byte| byte == 0)?;
        let header = Self { ino: u64::from_ne_bytes(word(0)), off: i64::from_ne_bytes(word(8)), reclen, kind: bytes[18] };
        Some((header, &tail[..name_length]))
    }
}

/// Clocks, for [`nr::CLOCK_GETTIME`], [`nr::CLOCK_GETRES`], [`nr::NANOSLEEP`] and
/// [`nr::FUTEX_WAIT2`]. Linux's values.
pub mod clock {
    /// `CLOCK_REALTIME`: the wall clock, from the RTC at boot counted forward.
    pub const REALTIME: usize = 0;
    /// `CLOCK_MONOTONIC`: nanoseconds since boot, never going back.
    pub const MONOTONIC: usize = 1;
    /// `CLOCK_PROCESS_CPUTIME_ID`: every thread of the process, live and ended.
    pub const PROCESS_CPUTIME_ID: usize = 2;
    /// `CLOCK_THREAD_CPUTIME_ID`: the calling thread.
    pub const THREAD_CPUTIME_ID: usize = 3;
    /// `CLOCK_MONOTONIC_RAW`: [`MONOTONIC`]; nothing slews it.
    pub const MONOTONIC_RAW: usize = 4;
    /// `CLOCK_REALTIME_COARSE`: [`REALTIME`].
    pub const REALTIME_COARSE: usize = 5;
    /// `CLOCK_MONOTONIC_COARSE`: [`MONOTONIC`].
    pub const MONOTONIC_COARSE: usize = 6;
    /// `CLOCK_BOOTTIME`: [`MONOTONIC`], since the machine never suspends.
    pub const BOOTTIME: usize = 7;
}

/// Flags for [`nr::NANOSLEEP`].
pub mod timer {
    /// `TIMER_ABSTIME`: the request is a time on the clock, not an interval.
    pub const ABSTIME: usize = 1;
}

/// Nanoseconds in a second.
pub const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// A time or an interval: Linux's `struct timespec`. A Linux-layout struct, though with
/// no padding to ignore: `nsec` is a whole `i64`, checked as one.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Timespec {
    pub sec: i64,
    /// `0..1_000_000_000`.
    pub nsec: i64,
}

const _: () = assert!(core::mem::size_of::<Timespec>() == 16);
const _: () = assert!(core::mem::offset_of!(Timespec, sec) == 0);
const _: () = assert!(core::mem::offset_of!(Timespec, nsec) == 8);

impl Timespec {
    /// Bytes of one, in user memory.
    pub const SIZE: usize = 16;

    /// `ns` nanoseconds.
    pub const fn from_nanos(ns: u64) -> Self {
        Self { sec: (ns / NANOS_PER_SECOND) as i64, nsec: (ns % NANOS_PER_SECOND) as i64 }
    }

    /// The nanoseconds this is, if it's a valid time: `sec` not negative, `nsec` in
    /// `0..1e9`. One too far in the future for 64 bits of nanoseconds (past the year 2554)
    /// is `u64::MAX`, which no clock reaches.
    pub const fn to_nanos(&self) -> Option<u64> {
        if self.sec < 0 || self.nsec < 0 || self.nsec >= NANOS_PER_SECOND as i64 {
            return None;
        }
        let whole = (self.sec as u128) * NANOS_PER_SECOND as u128 + self.nsec as u128;
        Some(if whole > u64::MAX as u128 { u64::MAX } else { whole as u64 })
    }

    /// As it is in memory, on both ports.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0; Self::SIZE];
        out[..8].copy_from_slice(&self.sec.to_ne_bytes());
        out[8..].copy_from_slice(&self.nsec.to_ne_bytes());
        out
    }

    /// From its bytes in memory.
    pub fn from_bytes(bytes: [u8; Self::SIZE]) -> Self {
        let mut sec = [0; 8];
        let mut nsec = [0; 8];
        sec.copy_from_slice(&bytes[..8]);
        nsec.copy_from_slice(&bytes[8..]);
        Self { sec: i64::from_ne_bytes(sec), nsec: i64::from_ne_bytes(nsec) }
    }
}

/// A register argument whose C type is 32 bits, as section 1 of the design reads one: its
/// low 32 bits, if the upper 32 are zero or the sign extension of bit 31, so that a C
/// caller's `-1` and a Rust caller's `u32::MAX` both arrive as `-1`.
pub const fn int_arg(value: usize) -> Option<i32> {
    let value = value as u64;
    let low = value as u32;
    let upper = (value >> 32) as u32;
    let extension = if low & 0x8000_0000 != 0 { u32::MAX } else { 0 };
    if upper == 0 || upper == extension {
        Some(low as i32)
    } else {
        None
    }
}

/// Flags for [`nr::OPEN`].
pub mod open {
    pub const READ: usize = 1 << 0;
    pub const WRITE: usize = 1 << 1;
    pub const CREATE: usize = 1 << 2;
    pub const TRUNCATE: usize = 1 << 3;
    pub const APPEND: usize = 1 << 4;
    /// Every flag there is.
    pub const ALL: usize = READ | WRITE | CREATE | TRUNCATE | APPEND;
}

/// Values of `whence` for [`nr::SEEK`].
pub mod seek {
    pub const SET: usize = 0;
    pub const CURRENT: usize = 1;
    pub const END: usize = 2;
}

/// Access `mmap` is asked for. Memory is never both writable and executable, and
/// anonymous memory is never executable at all.
pub mod prot {
    pub const NONE: usize = 0;
    pub const READ: usize = 1 << 0;
    pub const WRITE: usize = 1 << 1;
    /// Only for [`nr::MPROTECT`](super::nr::MPROTECT) and [`nr::MMAP2`](super::nr::MMAP2),
    /// and only for a program's own code: v1's `MMAP` refuses it with `EINVAL`.
    pub const EXEC: usize = 1 << 2;
}

/// Flags for [`nr::MMAP2`]: exactly one of [`SHARED`](map::SHARED), [`PRIVATE`](map::PRIVATE)
/// and [`SHARED_VALIDATE`](map::SHARED_VALIDATE), and any of the rest. Linux's values.
pub mod map {
    pub const SHARED: usize = 0x01;
    pub const PRIVATE: usize = 0x02;
    /// The same as [`SHARED`]: every flag is checked anyway.
    pub const SHARED_VALIDATE: usize = 0x03;
    /// Where [`SHARED`], [`PRIVATE`] and [`SHARED_VALIDATE`] go.
    pub const TYPE: usize = 0x0f;
    /// At exactly `hint`, replacing whatever was there.
    pub const FIXED: usize = 0x10;
    pub const ANONYMOUS: usize = 0x20;
    /// No promise: a page that can't get a frame when it's touched ends the process
    /// with [`EXIT_BUS`](super::EXIT_BUS).
    pub const NORESERVE: usize = 0x4000;
    /// Every page given its frame before the call returns.
    pub const POPULATE: usize = 0x8000;
    /// Accepted, and means nothing.
    pub const STACK: usize = 0x20000;
    /// At exactly `hint`, or `EEXIST` if anything's there.
    pub const FIXED_NOREPLACE: usize = 0x10_0000;
    /// Every flag there is.
    pub const ALL: usize = SHARED_VALIDATE | FIXED | ANONYMOUS | NORESERVE | POPULATE | STACK | FIXED_NOREPLACE;
}

/// Advice for [`nr::MADVISE`]. Linux's values.
pub mod madv {
    pub const NORMAL: usize = 0;
    pub const RANDOM: usize = 1;
    pub const SEQUENTIAL: usize = 2;
    pub const WILLNEED: usize = 3;
    /// Private pages lose their frames, and read as zeroes next time.
    pub const DONTNEED: usize = 4;
    /// As [`DONTNEED`].
    pub const FREE: usize = 8;
    pub const DONTFORK: usize = 10;
    pub const DOFORK: usize = 11;
    pub const HUGEPAGE: usize = 14;
    pub const NOHUGEPAGE: usize = 15;
    pub const DONTDUMP: usize = 16;
    pub const DODUMP: usize = 17;
}

/// Flags for [`nr::MSYNC`]: exactly one of [`ASYNC`](ms::ASYNC) and [`SYNC`](ms::SYNC),
/// and maybe [`INVALIDATE`](ms::INVALIDATE). Linux's values.
pub mod ms {
    pub const ASYNC: usize = 1;
    pub const INVALIDATE: usize = 2;
    pub const SYNC: usize = 4;
}

/// "Use this process's own" for a stream passed to [`nr::SPAWN`].
pub const INHERIT: usize = usize::MAX;

/// A string the caller owns: bytes of UTF-8, not terminated.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UserStr {
    pub pointer: usize,
    pub length: usize,
}

/// Kinds of [`Event`].
pub mod event {
    pub const NONE: u32 = 0;
    /// `a`, `b`: pointer position in the surface. `c`: buttons held, bit 0 the left.
    pub const POINTER: u32 = 1;
    /// `a`: the key's character, or zero. `b`: 1 pressed, 0 released.
    pub const KEY: u32 = 2;
    /// The window was closed.
    pub const CLOSE: u32 = 3;
}

/// One input event from a window.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Event {
    pub kind: u32,
    pub a: i32,
    pub b: i32,
    pub c: i32,
}

/// Whether a raw result is an error, and which.
pub const fn error_of(result: isize) -> Option<isize> {
    if result < 0 && result >= -4095 {
        Some(-result)
    } else {
        None
    }
}

/// `posix_spawn`'s attribute flags for [`SpawnRequest::flags`]: glibc's and musl's values
/// (not relibc's, which number them in sequence and so can't be ORed).
pub mod spawn {
    /// `POSIX_SPAWN_RESETIDS`: no effect, since nothing is set-user-ID.
    pub const RESETIDS: u64 = 1;
    /// `POSIX_SPAWN_SETPGROUP`: the child joins [`SpawnRequest::pgroup`](super::SpawnRequest::pgroup), or a group
    /// of its own if that's 0.
    pub const SETPGROUP: u64 = 2;
    /// `POSIX_SPAWN_SETSIGDEF`: the signals in `sigdefault` start at their defaults.
    pub const SETSIGDEF: u64 = 4;
    /// `POSIX_SPAWN_SETSIGMASK`: the child's signal mask is `sigmask`.
    pub const SETSIGMASK: u64 = 8;
    /// glibc's `POSIX_SPAWN_SETSCHEDPARAM`: refused, `EINVAL`, as there are no policies.
    pub const SETSCHEDPARAM: u64 = 0x10;
    /// glibc's `POSIX_SPAWN_SETSCHEDULER`: refused, `EINVAL`.
    pub const SETSCHEDULER: u64 = 0x20;
    /// `POSIX_SPAWN_USEVFORK`: no effect.
    pub const USEVFORK: u64 = 0x40;
    /// `POSIX_SPAWN_SETSID`: the child leads a new session and group.
    pub const SETSID: u64 = 0x80;
    /// The program is `path.dirfd`'s file itself, `fexecve`-like; `path.length` is 0.
    pub const EMPTY_PATH: u64 = 0x1000;
    /// Every flag `SPAWN2` takes.
    pub const ALL: u64 = RESETIDS | SETPGROUP | SETSIGDEF | SETSIGMASK | USEVFORK | SETSID | EMPTY_PATH;
    /// Most bytes of argument and environment strings, each with its terminating NUL, and
    /// their pointers (8 bytes each) together: past this, `E2BIG`. What `sysconf(_SC_ARG_MAX)`
    /// says.
    pub const ARG_MAX: usize = 1024 * 1024;
    /// Most argument and environment strings together: `E2BIG`.
    pub const MAX_STRINGS: usize = 32768;
    /// Longest one string may be, without its NUL: `E2BIG`.
    pub const STRING_MAX: usize = 128 * 1024;
    /// Most actions one request may have: `EINVAL`.
    pub const MAX_ACTIONS: usize = 256;
}

/// [`SpawnAction::kind`]s, run against the child's table in order.
pub mod spawn_action {
    /// Close `fd`; one that isn't open is ignored, as glibc and musl do.
    pub const CLOSE: u32 = 1;
    /// `dup2(fd, to)`; `fd == to` clears `to`'s close-on-exec flag.
    pub const DUP2: u32 = 2;
    /// `openat(path, open_flags, mode)` placed at `fd`.
    pub const OPEN: u32 = 3;
    /// `chdir(path)`.
    pub const CHDIR: u32 = 4;
    /// `fchdir(fd)`.
    pub const FCHDIR: u32 = 5;
    /// Close every descriptor from `fd` up.
    pub const CLOSEFROM: u32 = 6;
    /// Make the child's group the foreground group of the terminal `fd`: `EINVAL` until
    /// there are terminals (stage 1c.11).
    pub const TCSETPGRP: u32 = 7;
}

/// What [`nr::SPAWN2`] starts, and how. A request struct: `reserved`, and the fields a
/// flag doesn't ask for, are zero.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct SpawnRequest {
    /// The program, relative to `dirfd` in the child's table, or to the child's working
    /// directory, after the actions.
    pub path: PathAt,
    /// `argc` [`UserStr`]s, at least one.
    pub argv: u64,
    pub argc: u64,
    /// `envc` [`UserStr`]s, each `KEY=VALUE`.
    pub envp: u64,
    pub envc: u64,
    /// `action_count` [`SpawnAction`]s.
    pub actions: u64,
    pub action_count: u64,
    /// [`spawn`] flags.
    pub flags: u64,
    /// With [`spawn::SETPGROUP`].
    pub pgroup: i64,
    /// With [`spawn::SETSIGMASK`]: bit `n - 1` is signal `n`.
    pub sigmask: u64,
    /// With [`spawn::SETSIGDEF`].
    pub sigdefault: u64,
    pub reserved: [u64; 3],
}

const _: () = assert!(core::mem::size_of::<SpawnRequest>() == 128);
const _: () = assert!(core::mem::offset_of!(SpawnRequest, path) == 0);
const _: () = assert!(core::mem::offset_of!(SpawnRequest, argv) == 24);
const _: () = assert!(core::mem::offset_of!(SpawnRequest, argc) == 32);
const _: () = assert!(core::mem::offset_of!(SpawnRequest, envp) == 40);
const _: () = assert!(core::mem::offset_of!(SpawnRequest, envc) == 48);
const _: () = assert!(core::mem::offset_of!(SpawnRequest, actions) == 56);
const _: () = assert!(core::mem::offset_of!(SpawnRequest, action_count) == 64);
const _: () = assert!(core::mem::offset_of!(SpawnRequest, flags) == 72);
const _: () = assert!(core::mem::offset_of!(SpawnRequest, pgroup) == 80);
const _: () = assert!(core::mem::offset_of!(SpawnRequest, sigmask) == 88);
const _: () = assert!(core::mem::offset_of!(SpawnRequest, sigdefault) == 96);
const _: () = assert!(core::mem::offset_of!(SpawnRequest, reserved) == 104);

/// The `n`th little word of a struct's bytes, as `u64`, `u32` and `i32` readers want them.
fn word_at(bytes: &[u8], at: usize) -> u64 {
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[at..at + 8]);
    u64::from_ne_bytes(out)
}

fn half_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

impl PathAt {
    /// As it is in memory.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..4].copy_from_slice(&self.dirfd.to_ne_bytes());
        out[4..8].copy_from_slice(&self.reserved.to_ne_bytes());
        out[8..16].copy_from_slice(&self.pointer.to_ne_bytes());
        out[16..24].copy_from_slice(&self.length.to_ne_bytes());
        out
    }
}

impl SpawnRequest {
    /// Bytes of one, in user memory.
    pub const SIZE: usize = 128;

    /// From its bytes in memory, on both ports: read once, so every check and every use
    /// see the same values.
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        let mut path = [0u8; 24];
        path.copy_from_slice(&bytes[..24]);
        Self {
            path: PathAt::from_bytes(path),
            argv: word_at(bytes, 24),
            argc: word_at(bytes, 32),
            envp: word_at(bytes, 40),
            envc: word_at(bytes, 48),
            actions: word_at(bytes, 56),
            action_count: word_at(bytes, 64),
            flags: word_at(bytes, 72),
            pgroup: word_at(bytes, 80) as i64,
            sigmask: word_at(bytes, 88),
            sigdefault: word_at(bytes, 96),
            reserved: [word_at(bytes, 104), word_at(bytes, 112), word_at(bytes, 120)],
        }
    }

    /// As it is in memory.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[..24].copy_from_slice(&self.path.to_bytes());
        let words = [
            self.argv, self.argc, self.envp, self.envc, self.actions, self.action_count, self.flags, self.pgroup as u64,
            self.sigmask, self.sigdefault, self.reserved[0], self.reserved[1], self.reserved[2],
        ];
        for (index, word) in words.iter().enumerate() {
            out[24 + index * 8..32 + index * 8].copy_from_slice(&word.to_ne_bytes());
        }
        out
    }
}

/// What [`nr::EXEC`] runs in place of the caller's image: a [`SpawnRequest`] without
/// actions or attributes. A request struct: `reserved` is zero.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct ExecRequest {
    /// The program, relative to `dirfd` or the working directory.
    pub path: PathAt,
    /// `argc` [`UserStr`]s, at least one.
    pub argv: u64,
    pub argc: u64,
    /// `envc` [`UserStr`]s, each `KEY=VALUE`.
    pub envp: u64,
    pub envc: u64,
    /// [`spawn::EMPTY_PATH`] or nothing.
    pub flags: u64,
    pub reserved: [u64; 2],
}

const _: () = assert!(core::mem::size_of::<ExecRequest>() == 80);
const _: () = assert!(core::mem::offset_of!(ExecRequest, path) == 0);
const _: () = assert!(core::mem::offset_of!(ExecRequest, argv) == 24);
const _: () = assert!(core::mem::offset_of!(ExecRequest, argc) == 32);
const _: () = assert!(core::mem::offset_of!(ExecRequest, envp) == 40);
const _: () = assert!(core::mem::offset_of!(ExecRequest, envc) == 48);
const _: () = assert!(core::mem::offset_of!(ExecRequest, flags) == 56);
const _: () = assert!(core::mem::offset_of!(ExecRequest, reserved) == 64);

impl ExecRequest {
    /// Bytes of one, in user memory.
    pub const SIZE: usize = 80;

    /// From its bytes in memory, on both ports: read once, as a [`SpawnRequest`] is.
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        let mut path = [0u8; 24];
        path.copy_from_slice(&bytes[..24]);
        Self {
            path: PathAt::from_bytes(path),
            argv: word_at(bytes, 24),
            argc: word_at(bytes, 32),
            envp: word_at(bytes, 40),
            envc: word_at(bytes, 48),
            flags: word_at(bytes, 56),
            reserved: [word_at(bytes, 64), word_at(bytes, 72)],
        }
    }

    /// As it is in memory.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[..24].copy_from_slice(&self.path.to_bytes());
        let words = [self.argv, self.argc, self.envp, self.envc, self.flags, self.reserved[0], self.reserved[1]];
        for (index, word) in words.iter().enumerate() {
            out[24 + index * 8..32 + index * 8].copy_from_slice(&word.to_ne_bytes());
        }
        out
    }
}

/// One step of building a [`nr::SPAWN2`] child's table. A request struct: the fields its
/// kind doesn't use, and `reserved`, are zero.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct SpawnAction {
    /// One of [`spawn_action`].
    pub kind: u32,
    /// `CLOSE`, `FCHDIR`, `TCSETPGRP`: the fd; `CLOSEFROM`: the lowest to close; `DUP2`:
    /// from; `OPEN`: where to put it.
    pub fd: i32,
    /// `DUP2`: to.
    pub to: i32,
    /// `OPEN`: `O_*` flags.
    pub open_flags: u32,
    /// `OPEN`: the mode of a new file.
    pub mode: u32,
    pub reserved: u32,
    /// `OPEN`, `CHDIR`.
    pub path: PathAt,
}

const _: () = assert!(core::mem::size_of::<SpawnAction>() == 48);
const _: () = assert!(core::mem::offset_of!(SpawnAction, kind) == 0);
const _: () = assert!(core::mem::offset_of!(SpawnAction, fd) == 4);
const _: () = assert!(core::mem::offset_of!(SpawnAction, to) == 8);
const _: () = assert!(core::mem::offset_of!(SpawnAction, open_flags) == 12);
const _: () = assert!(core::mem::offset_of!(SpawnAction, mode) == 16);
const _: () = assert!(core::mem::offset_of!(SpawnAction, reserved) == 20);
const _: () = assert!(core::mem::offset_of!(SpawnAction, path) == 24);

impl SpawnAction {
    /// Bytes of one, in user memory.
    pub const SIZE: usize = 48;

    /// From its bytes in memory, on both ports.
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        let mut path = [0u8; 24];
        path.copy_from_slice(&bytes[24..]);
        Self {
            kind: half_at(bytes, 0),
            fd: half_at(bytes, 4) as i32,
            to: half_at(bytes, 8) as i32,
            open_flags: half_at(bytes, 12),
            mode: half_at(bytes, 16),
            reserved: half_at(bytes, 20),
            path: PathAt::from_bytes(path),
        }
    }

    /// As it is in memory.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        let halves = [self.kind, self.fd as u32, self.to as u32, self.open_flags, self.mode, self.reserved];
        for (index, half) in halves.iter().enumerate() {
            out[index * 4..index * 4 + 4].copy_from_slice(&half.to_ne_bytes());
        }
        out[24..].copy_from_slice(&self.path.to_bytes());
        out
    }
}

/// Linux's `struct timeval` on 64-bit.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Timeval {
    pub sec: i64,
    pub usec: i64,
}

const _: () = assert!(core::mem::size_of::<Timeval>() == 16);

impl Timeval {
    /// `ns` nanoseconds, to the microsecond below.
    pub const fn from_nanos(ns: u64) -> Self {
        Self { sec: (ns / NANOS_PER_SECOND) as i64, usec: (ns % NANOS_PER_SECOND / 1000) as i64 }
    }
}

/// Signal numbers: Linux's, the same on both ports. A set of them is a `u64` with bit
/// `n - 1` for signal `n`.
pub mod sig {
    pub const HUP: u32 = 1;
    pub const INT: u32 = 2;
    pub const QUIT: u32 = 3;
    pub const ILL: u32 = 4;
    pub const TRAP: u32 = 5;
    pub const ABRT: u32 = 6;
    pub const BUS: u32 = 7;
    pub const FPE: u32 = 8;
    pub const KILL: u32 = 9;
    pub const USR1: u32 = 10;
    pub const SEGV: u32 = 11;
    pub const USR2: u32 = 12;
    pub const PIPE: u32 = 13;
    pub const ALRM: u32 = 14;
    pub const TERM: u32 = 15;
    pub const STKFLT: u32 = 16;
    pub const CHLD: u32 = 17;
    pub const CONT: u32 = 18;
    pub const STOP: u32 = 19;
    pub const TSTP: u32 = 20;
    pub const TTIN: u32 = 21;
    pub const TTOU: u32 = 22;
    pub const URG: u32 = 23;
    pub const XCPU: u32 = 24;
    pub const XFSZ: u32 = 25;
    pub const VTALRM: u32 = 26;
    pub const PROF: u32 = 27;
    pub const WINCH: u32 = 28;
    pub const IO: u32 = 29;
    pub const PWR: u32 = 30;
    pub const SYS: u32 = 31;
    /// The first real-time signal.
    pub const RTMIN: u32 = 32;
    /// The highest signal number.
    pub const MAX: u32 = 64;
}

/// `flags` of a [`SigAction`], and the dispositions that aren't handlers: Linux's
/// values. `SA_SIGINFO` is accepted and always in effect: a handler always gets three
/// arguments.
pub mod sa {
    /// `SIG_DFL`: the signal's default action.
    pub const DFL: u64 = 0;
    /// `SIG_IGN`: the signal is discarded.
    pub const IGN: u64 = 1;
    /// `SA_NOCLDSTOP`: no `SIGCHLD` for a child's stop or continue.
    pub const NOCLDSTOP: u32 = 1;
    /// `SA_NOCLDWAIT`: children are reaped as they end, never left for `WAIT4`.
    pub const NOCLDWAIT: u32 = 2;
    /// `SA_SIGINFO`.
    pub const SIGINFO: u32 = 4;
    /// `SA_RESTORER`: `restorer` is what the handler returns to. Required with a handler.
    pub const RESTORER: u32 = 0x0400_0000;
    /// `SA_ONSTACK`: on the alternate stack, once there is one (stage 1c.10).
    pub const ONSTACK: u32 = 0x0800_0000;
    /// `SA_RESTART`: an interrupted call is restarted (stage 1c.10).
    pub const RESTART: u32 = 0x1000_0000;
    /// `SA_NODEFER`: the signal isn't blocked while its handler runs.
    pub const NODEFER: u32 = 0x4000_0000;
    /// `SA_RESETHAND`: the disposition goes back to `SIG_DFL` as the handler is entered.
    pub const RESETHAND: u32 = 0x8000_0000;
    /// Every flag there is.
    pub const ALL: u32 = NOCLDSTOP | NOCLDWAIT | SIGINFO | RESTORER | ONSTACK | RESTART | NODEFER | RESETHAND;
}

/// `code` of a [`SigInfo`]: Linux's values.
pub mod si {
    /// Sent by `KILL`.
    pub const USER: i32 = 0;
    /// Raised by the kernel: `SIGPIPE`, the console's `SIGINT`.
    pub const KERNEL: i32 = 0x80;
    /// Sent by `THREAD_KILL`.
    pub const TKILL: i32 = -6;
    pub const CLD_EXITED: i32 = 1;
    pub const CLD_KILLED: i32 = 2;
    pub const CLD_STOPPED: i32 = 5;
    pub const CLD_CONTINUED: i32 = 6;
    pub const SEGV_MAPERR: i32 = 1;
    pub const SEGV_ACCERR: i32 = 2;
    pub const BUS_ADRALN: i32 = 1;
    pub const BUS_ADRERR: i32 = 2;
    pub const ILL_ILLOPC: i32 = 1;
    pub const FPE_INTDIV: i32 = 1;
    pub const TRAP_BRKPT: i32 = 1;
}

/// `how` of [`nr::SIGPROCMASK`]: Linux's values.
pub mod how {
    /// Adds the set to the mask.
    pub const BLOCK: i32 = 0;
    /// Takes the set from the mask.
    pub const UNBLOCK: i32 = 1;
    /// The set is the mask.
    pub const SETMASK: i32 = 2;
}

/// `which` of [`nr::SETITIMER`]: Linux's values. Only [`REAL`](itimer::REAL) is offered.
pub mod itimer {
    pub const REAL: i32 = 0;
    pub const VIRTUAL: i32 = 1;
    pub const PROF: i32 = 2;
}

/// An interval timer's setting, Linux's `struct itimerval`: a request struct (no
/// reserved fields) for [`nr::SETITIMER`], and what it reports.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct ITimerVal {
    /// What it's re-armed by each time it goes off; zero for once.
    pub interval: Timeval,
    /// How long until it goes off; zero for disarmed.
    pub value: Timeval,
}

const _: () = assert!(core::mem::size_of::<ITimerVal>() == 32);
const _: () = assert!(core::mem::offset_of!(ITimerVal, interval) == 0);
const _: () = assert!(core::mem::offset_of!(ITimerVal, value) == 16);

impl ITimerVal {
    pub const SIZE: usize = 32;

    pub fn from_bytes(bytes: [u8; Self::SIZE]) -> Self {
        let word = |at: usize| {
            let mut out = [0u8; 8];
            out.copy_from_slice(&bytes[at..at + 8]);
            i64::from_ne_bytes(out)
        };
        Self { interval: Timeval { sec: word(0), usec: word(8) }, value: Timeval { sec: word(16), usec: word(24) } }
    }

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        for (index, word) in [self.interval.sec, self.interval.usec, self.value.sec, self.value.usec].into_iter().enumerate() {
            out[index * 8..index * 8 + 8].copy_from_slice(&word.to_ne_bytes());
        }
        out
    }
}

/// `flags` of a [`SigStack`], and the alternate stack's sizes: Linux's values.
pub mod ss {
    pub const ONSTACK: i32 = 1;
    pub const DISABLE: i32 = 2;
    pub const MINSIGSTKSZ: usize = 2048;
    pub const SIGSTKSZ: usize = 8192;
}

/// A signal's disposition, for [`nr::SIGACTION`]: Linux's `struct sigaction` as its
/// system call takes it. A Linux-layout struct: `reserved` is the padding after a C
/// `sa_flags`, ignored on input and written as zero.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct SigAction {
    /// [`sa::DFL`], [`sa::IGN`], or the handler's address.
    pub handler: u64,
    /// [`sa`]'s flags.
    pub flags: u32,
    pub reserved: u32,
    /// What the handler returns to, which calls [`nr::SIGRETURN`].
    pub restorer: u64,
    /// Signals blocked while the handler runs, besides the signal itself.
    pub mask: u64,
}

const _: () = assert!(core::mem::size_of::<SigAction>() == 32);
const _: () = assert!(core::mem::offset_of!(SigAction, handler) == 0);
const _: () = assert!(core::mem::offset_of!(SigAction, flags) == 8);
const _: () = assert!(core::mem::offset_of!(SigAction, reserved) == 12);
const _: () = assert!(core::mem::offset_of!(SigAction, restorer) == 16);
const _: () = assert!(core::mem::offset_of!(SigAction, mask) == 24);

impl SigAction {
    pub const SIZE: usize = 32;

    /// From its bytes in memory, the padding dropped.
    pub fn from_bytes(bytes: [u8; Self::SIZE]) -> Self {
        let word = |at: usize| {
            let mut out = [0u8; 8];
            out.copy_from_slice(&bytes[at..at + 8]);
            u64::from_ne_bytes(out)
        };
        let flags = u32::from_ne_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        Self { handler: word(0), flags, reserved: 0, restorer: word(16), mask: word(24) }
    }

    /// As it is in memory, the padding zero.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..8].copy_from_slice(&self.handler.to_ne_bytes());
        out[8..12].copy_from_slice(&self.flags.to_ne_bytes());
        out[16..24].copy_from_slice(&self.restorer.to_ne_bytes());
        out[24..32].copy_from_slice(&self.mask.to_ne_bytes());
        out
    }
}

/// An alternate signal stack, Linux's `stack_t`. A Linux-layout struct.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct SigStack {
    pub sp: u64,
    /// [`ss`]'s flags.
    pub flags: i32,
    pub reserved: u32,
    pub size: u64,
}

const _: () = assert!(core::mem::size_of::<SigStack>() == 24);
const _: () = assert!(core::mem::offset_of!(SigStack, sp) == 0);
const _: () = assert!(core::mem::offset_of!(SigStack, flags) == 8);
const _: () = assert!(core::mem::offset_of!(SigStack, reserved) == 12);
const _: () = assert!(core::mem::offset_of!(SigStack, size) == 16);

impl SigStack {
    pub const SIZE: usize = 24;

    /// From its bytes in memory, the padding dropped.
    pub fn from_bytes(bytes: [u8; Self::SIZE]) -> Self {
        let word = |at: usize| {
            let mut out = [0u8; 8];
            out.copy_from_slice(&bytes[at..at + 8]);
            u64::from_ne_bytes(out)
        };
        let flags = i32::from_ne_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        Self { sp: word(0), flags, reserved: 0, size: word(16) }
    }

    /// As it is in memory, the padding zero.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..8].copy_from_slice(&self.sp.to_ne_bytes());
        out[8..12].copy_from_slice(&self.flags.to_ne_bytes());
        out[16..24].copy_from_slice(&self.size.to_ne_bytes());
        out
    }
}

/// What a handler is told about its signal, Linux's `siginfo_t` on 64-bit: an output.
/// What `fields` holds depends on the signal: [`SigInfo::sender`], [`SigInfo::child`] and
/// [`SigInfo::fault`] make each kind, and the rest is zero.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SigInfo {
    pub signo: i32,
    /// Always 0.
    pub errno: i32,
    /// [`si`]'s codes.
    pub code: i32,
    pub reserved: i32,
    /// From offset 16: `pid: i32`, `uid: u32` for a sent signal; then `status: i32`, a
    /// reserved `i32`, `utime: i64` and `stime: i64` for `SIGCHLD`; `address: u64` for a
    /// fault.
    pub fields: [u8; 112],
}

const _: () = assert!(core::mem::size_of::<SigInfo>() == 128);
const _: () = assert!(core::mem::offset_of!(SigInfo, signo) == 0);
const _: () = assert!(core::mem::offset_of!(SigInfo, errno) == 4);
const _: () = assert!(core::mem::offset_of!(SigInfo, code) == 8);
const _: () = assert!(core::mem::offset_of!(SigInfo, fields) == 16);

impl Default for SigInfo {
    fn default() -> Self {
        Self { signo: 0, errno: 0, code: 0, reserved: 0, fields: [0; 112] }
    }
}

impl SigInfo {
    pub const SIZE: usize = 128;

    fn put(&mut self, at: usize, bytes: &[u8]) {
        self.fields[at - 16..at - 16 + bytes.len()].copy_from_slice(bytes);
    }

    fn get<const N: usize>(&self, at: usize) -> [u8; N] {
        let mut out = [0u8; N];
        out.copy_from_slice(&self.fields[at - 16..at - 16 + N]);
        out
    }

    /// A signal sent by a process `pid` of user `uid`, or raised by the kernel (with
    /// [`si::KERNEL`], `pid` and `uid` 0).
    pub fn sender(signo: u32, code: i32, pid: u32, uid: u32) -> Self {
        let mut info = Self { signo: signo as i32, code, ..Self::default() };
        info.put(16, &(pid as i32).to_ne_bytes());
        info.put(20, &uid.to_ne_bytes());
        info
    }

    /// `SIGCHLD` for the child `pid` of user `uid`: `code` one of `si::CLD_*`, `status`
    /// its exit code or signal, and its CPU time in clock ticks of 10 ms.
    pub fn child(code: i32, pid: u32, uid: u32, status: i32, utime: i64, stime: i64) -> Self {
        let mut info = Self::sender(sig::CHLD, code, pid, uid);
        info.put(24, &status.to_ne_bytes());
        info.put(32, &utime.to_ne_bytes());
        info.put(40, &stime.to_ne_bytes());
        info
    }

    /// A fault's signal, at `address`.
    pub fn fault(signo: u32, code: i32, address: u64) -> Self {
        let mut info = Self { signo: signo as i32, code, ..Self::default() };
        info.put(16, &address.to_ne_bytes());
        info
    }

    /// The sender's pid, or a child's.
    pub fn pid(&self) -> i32 {
        i32::from_ne_bytes(self.get(16))
    }

    /// The sender's uid, or a child's.
    pub fn uid(&self) -> u32 {
        u32::from_ne_bytes(self.get(20))
    }

    /// A child's status, for `SIGCHLD`.
    pub fn status(&self) -> i32 {
        i32::from_ne_bytes(self.get(24))
    }

    /// A fault's address.
    pub fn address(&self) -> u64 {
        u64::from_ne_bytes(self.get(16))
    }

    /// As it is in memory.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..4].copy_from_slice(&self.signo.to_ne_bytes());
        out[4..8].copy_from_slice(&self.errno.to_ne_bytes());
        out[8..12].copy_from_slice(&self.code.to_ne_bytes());
        out[12..16].copy_from_slice(&self.reserved.to_ne_bytes());
        out[16..].copy_from_slice(&self.fields);
        out
    }

    /// From its bytes in memory.
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        let int = |at: usize| i32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        let mut fields = [0u8; 112];
        fields.copy_from_slice(&bytes[16..]);
        Self { signo: int(0), errno: int(4), code: int(8), reserved: int(12), fields }
    }
}

/// The register contexts a signal handler is given, one per port (section 8.4 of
/// `docs/feos-abi-v2.md`), both defined on every host so either's layout can be checked
/// anywhere. [`UContext`] is this port's.
pub mod ucontext {
    use super::SigStack;

    /// `flags` on x86_64: the FP area is a plain `FXSAVE64` image.
    pub const UC_FEOS_FXSAVE: u64 = 1;

    /// x86_64's: Linux's `gregs` order. `rsp + 8` at a handler's entry points at it.
    #[repr(C, align(16))]
    #[derive(Clone, Copy)]
    pub struct X86_64 {
        /// [`UC_FEOS_FXSAVE`].
        pub flags: u64,
        /// 0.
        pub link: u64,
        /// The alternate stack as it was.
        pub stack: SigStack,
        /// The mask to restore.
        pub mask: u64,
        /// `r8`..`r15`, `rdi`, `rsi`, `rbp`, `rbx`, `rdx`, `rax`, `rcx`, `rsp`, `rip`,
        /// `rflags`, `cs_gs_fs` (`cs` in the low 16 bits), `err`, `trapno`, `oldmask` (0),
        /// `cr2`: see [`greg`].
        pub gregs: [u64; 23],
        /// The address of `fxsave`, for the handler; ignored by `SIGRETURN`.
        pub fpregs: u64,
        pub reserved: [u64; 8],
        /// The `FXSAVE64` image.
        pub fxsave: [u8; 512],
    }

    /// Indexes of [`X86_64::gregs`]: Linux's `REG_*`.
    pub mod greg {
        pub const R8: usize = 0;
        pub const R9: usize = 1;
        pub const R10: usize = 2;
        pub const R11: usize = 3;
        pub const R12: usize = 4;
        pub const R13: usize = 5;
        pub const R14: usize = 6;
        pub const R15: usize = 7;
        pub const RDI: usize = 8;
        pub const RSI: usize = 9;
        pub const RBP: usize = 10;
        pub const RBX: usize = 11;
        pub const RDX: usize = 12;
        pub const RAX: usize = 13;
        pub const RCX: usize = 14;
        pub const RSP: usize = 15;
        pub const RIP: usize = 16;
        pub const EFL: usize = 17;
        pub const CSGSFS: usize = 18;
        pub const ERR: usize = 19;
        pub const TRAPNO: usize = 20;
        pub const OLDMASK: usize = 21;
        pub const CR2: usize = 22;
    }

    const _: () = assert!(core::mem::size_of::<X86_64>() == 816);
    const _: () = assert!(core::mem::offset_of!(X86_64, flags) == 0);
    const _: () = assert!(core::mem::offset_of!(X86_64, link) == 8);
    const _: () = assert!(core::mem::offset_of!(X86_64, stack) == 16);
    const _: () = assert!(core::mem::offset_of!(X86_64, mask) == 40);
    const _: () = assert!(core::mem::offset_of!(X86_64, gregs) == 48);
    const _: () = assert!(core::mem::offset_of!(X86_64, fpregs) == 232);
    const _: () = assert!(core::mem::offset_of!(X86_64, reserved) == 240);
    const _: () = assert!(core::mem::offset_of!(X86_64, fxsave) == 304);

    /// AArch64's. `sp` at a handler's entry points at it.
    #[repr(C, align(16))]
    #[derive(Clone, Copy)]
    pub struct Aarch64 {
        /// 0.
        pub flags: u64,
        /// 0.
        pub link: u64,
        pub stack: SigStack,
        pub mask: u64,
        pub fault_address: u64,
        /// `x0`..`x30`.
        pub regs: [u64; 31],
        pub sp: u64,
        pub pc: u64,
        /// Only `NZCV` (bits 31 to 28) is taken back.
        pub pstate: u64,
        pub fpsr: u32,
        pub fpcr: u32,
        /// `q0`..`q31`.
        pub v: [u128; 32],
        pub reserved: [u64; 4],
    }

    const _: () = assert!(core::mem::size_of::<Aarch64>() == 880);
    const _: () = assert!(core::mem::offset_of!(Aarch64, flags) == 0);
    const _: () = assert!(core::mem::offset_of!(Aarch64, link) == 8);
    const _: () = assert!(core::mem::offset_of!(Aarch64, stack) == 16);
    const _: () = assert!(core::mem::offset_of!(Aarch64, mask) == 40);
    const _: () = assert!(core::mem::offset_of!(Aarch64, fault_address) == 48);
    const _: () = assert!(core::mem::offset_of!(Aarch64, regs) == 56);
    const _: () = assert!(core::mem::offset_of!(Aarch64, sp) == 304);
    const _: () = assert!(core::mem::offset_of!(Aarch64, pc) == 312);
    const _: () = assert!(core::mem::offset_of!(Aarch64, pstate) == 320);
    const _: () = assert!(core::mem::offset_of!(Aarch64, fpsr) == 328);
    const _: () = assert!(core::mem::offset_of!(Aarch64, fpcr) == 332);
    const _: () = assert!(core::mem::offset_of!(Aarch64, v) == 336);
    const _: () = assert!(core::mem::offset_of!(Aarch64, reserved) == 848);
}

/// This port's signal context.
#[cfg(target_arch = "x86_64")]
pub type UContext = ucontext::X86_64;
/// This port's signal context.
#[cfg(target_arch = "aarch64")]
pub type UContext = ucontext::Aarch64;

/// `options` of [`nr::WAIT4`], and the POSIX status it writes: Linux's.
pub mod wait {
    /// `WNOHANG`: return 0 rather than wait.
    pub const NOHANG: usize = 1;
    /// `WUNTRACED`: report a child that stopped too.
    pub const UNTRACED: usize = 2;
    /// `WCONTINUED`: report a child that continued too.
    pub const CONTINUED: usize = 8;
    /// Every option there is.
    pub const ALL: usize = NOHANG | UNTRACED | CONTINUED;

    /// `WIFEXITED`.
    pub const fn exited(status: i32) -> bool {
        status & 0x7f == 0
    }
    /// `WEXITSTATUS`.
    pub const fn exit_code(status: i32) -> i32 {
        (status >> 8) & 0xff
    }
    /// `WIFSIGNALED`.
    pub const fn signaled(status: i32) -> bool {
        let signal = status & 0x7f;
        signal != 0 && signal != 0x7f
    }
    /// `WTERMSIG`.
    pub const fn term_signal(status: i32) -> i32 {
        status & 0x7f
    }
    /// `WIFSTOPPED`.
    pub const fn stopped(status: i32) -> bool {
        status & 0xff == 0x7f
    }
    /// `WSTOPSIG`.
    pub const fn stop_signal(status: i32) -> i32 {
        (status >> 8) & 0xff
    }
    /// `WIFCONTINUED`.
    pub const fn continued(status: i32) -> bool {
        status == 0xffff
    }
}

/// `who` of [`nr::GETRUSAGE`]: Linux's.
pub mod rusage {
    /// The calling process, every thread of it.
    pub const SELF: i32 = 0;
    /// Its reaped children, and theirs.
    pub const CHILDREN: i32 = -1;
    /// The calling thread.
    pub const THREAD: i32 = 1;
}

/// What [`nr::GETRUSAGE`] fills in: Linux's `struct rusage` on 64-bit. An output struct.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Rusage {
    /// Time in user mode.
    pub utime: Timeval,
    /// Time in calls and faults.
    pub stime: Timeval,
    /// The most pages it has owned, in KiB.
    pub maxrss: i64,
    pub ixrss: i64,
    pub idrss: i64,
    pub isrss: i64,
    /// Faults resolved without reading a file.
    pub minflt: i64,
    /// Faults that read a file (page-cache misses).
    pub majflt: i64,
    pub nswap: i64,
    pub inblock: i64,
    pub oublock: i64,
    pub msgsnd: i64,
    pub msgrcv: i64,
    pub nsignals: i64,
    /// Switches away because it waited.
    pub nvcsw: i64,
    /// Switches away because its time was up.
    pub nivcsw: i64,
}

const _: () = assert!(core::mem::size_of::<Rusage>() == 144);
const _: () = assert!(core::mem::offset_of!(Rusage, stime) == 16);
const _: () = assert!(core::mem::offset_of!(Rusage, maxrss) == 32);
const _: () = assert!(core::mem::offset_of!(Rusage, minflt) == 64);
const _: () = assert!(core::mem::offset_of!(Rusage, majflt) == 72);
const _: () = assert!(core::mem::offset_of!(Rusage, nvcsw) == 128);
const _: () = assert!(core::mem::offset_of!(Rusage, nivcsw) == 136);

impl Rusage {
    /// Bytes of one, in user memory.
    pub const SIZE: usize = 144;

    /// As it is in memory.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let words = [
            self.utime.sec, self.utime.usec, self.stime.sec, self.stime.usec, self.maxrss, self.ixrss, self.idrss,
            self.isrss, self.minflt, self.majflt, self.nswap, self.inblock, self.oublock, self.msgsnd, self.msgrcv,
            self.nsignals, self.nvcsw, self.nivcsw,
        ];
        let mut out = [0u8; Self::SIZE];
        for (index, word) in words.iter().enumerate() {
            out[index * 8..index * 8 + 8].copy_from_slice(&word.to_ne_bytes());
        }
        out
    }
}

/// [`SysInfo::flags`].
pub mod sysinfo {
    /// The wall clock started at the epoch: there was no RTC to set it from.
    pub const NO_RTC: u64 = 1;
    /// The random number generator has been seeded.
    pub const RNG_SEEDED: u64 = 2;
    /// A trusted source of randomness exists, so seeding will finish.
    pub const RNG_TRUSTED: u64 = 4;
}

/// What [`nr::SYSINFO`] fills in. An output struct, which may grow at its end: a caller
/// says how much of it it has room for.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysInfo {
    /// [`ABI_VERSION`].
    pub abi_version: u32,
    pub page_size: u32,
    pub cpus_online: u32,
    pub cpus_configured: u32,
    /// RAM, in pages.
    pub total_pages: u64,
    /// Pages free and not promised.
    pub free_pages: u64,
    pub uptime_ns: u64,
    /// [`spawn::ARG_MAX`].
    pub arg_max: u64,
    /// [`MAX_FDS`].
    pub fd_max: u32,
    /// Task slots, threads included.
    pub task_max: u32,
    /// [`PATH_MAX`].
    pub path_max: u32,
    /// [`NAME_MAX`].
    pub name_max: u32,
    /// The per-process memory limit, in bytes.
    pub memory_limit: u64,
    /// The kernel's version, NUL-padded.
    pub release: [u8; 64],
    /// `x86_64` or `aarch64`, NUL-padded.
    pub machine: [u8; 16],
    /// [`sysinfo`] flags.
    pub flags: u64,
    /// [`FEATURES`].
    pub features: u64,
    pub reserved: [u64; 4],
}

impl Default for SysInfo {
    fn default() -> Self {
        Self {
            abi_version: 0,
            page_size: 0,
            cpus_online: 0,
            cpus_configured: 0,
            total_pages: 0,
            free_pages: 0,
            uptime_ns: 0,
            arg_max: 0,
            fd_max: 0,
            task_max: 0,
            path_max: 0,
            name_max: 0,
            memory_limit: 0,
            release: [0; 64],
            machine: [0; 16],
            flags: 0,
            features: 0,
            reserved: [0; 4],
        }
    }
}

const _: () = assert!(core::mem::size_of::<SysInfo>() == 200);
const _: () = assert!(core::mem::offset_of!(SysInfo, page_size) == 4);
const _: () = assert!(core::mem::offset_of!(SysInfo, cpus_online) == 8);
const _: () = assert!(core::mem::offset_of!(SysInfo, cpus_configured) == 12);
const _: () = assert!(core::mem::offset_of!(SysInfo, total_pages) == 16);
const _: () = assert!(core::mem::offset_of!(SysInfo, free_pages) == 24);
const _: () = assert!(core::mem::offset_of!(SysInfo, uptime_ns) == 32);
const _: () = assert!(core::mem::offset_of!(SysInfo, arg_max) == 40);
const _: () = assert!(core::mem::offset_of!(SysInfo, fd_max) == 48);
const _: () = assert!(core::mem::offset_of!(SysInfo, task_max) == 52);
const _: () = assert!(core::mem::offset_of!(SysInfo, path_max) == 56);
const _: () = assert!(core::mem::offset_of!(SysInfo, name_max) == 60);
const _: () = assert!(core::mem::offset_of!(SysInfo, memory_limit) == 64);
const _: () = assert!(core::mem::offset_of!(SysInfo, release) == 72);
const _: () = assert!(core::mem::offset_of!(SysInfo, machine) == 136);
const _: () = assert!(core::mem::offset_of!(SysInfo, flags) == 152);
const _: () = assert!(core::mem::offset_of!(SysInfo, features) == 160);
const _: () = assert!(core::mem::offset_of!(SysInfo, reserved) == 168);

impl SysInfo {
    /// Bytes of one, in user memory.
    pub const SIZE: usize = 200;

    /// As it is in memory.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        let halves = [self.abi_version, self.page_size, self.cpus_online, self.cpus_configured];
        for (index, half) in halves.iter().enumerate() {
            out[index * 4..index * 4 + 4].copy_from_slice(&half.to_ne_bytes());
        }
        for (index, word) in [self.total_pages, self.free_pages, self.uptime_ns, self.arg_max].iter().enumerate() {
            out[16 + index * 8..24 + index * 8].copy_from_slice(&word.to_ne_bytes());
        }
        for (index, half) in [self.fd_max, self.task_max, self.path_max, self.name_max].iter().enumerate() {
            out[48 + index * 4..52 + index * 4].copy_from_slice(&half.to_ne_bytes());
        }
        out[64..72].copy_from_slice(&self.memory_limit.to_ne_bytes());
        out[72..136].copy_from_slice(&self.release);
        out[136..152].copy_from_slice(&self.machine);
        out[152..160].copy_from_slice(&self.flags.to_ne_bytes());
        out[160..168].copy_from_slice(&self.features.to_ne_bytes());
        for (index, word) in self.reserved.iter().enumerate() {
            out[168 + index * 8..176 + index * 8].copy_from_slice(&word.to_ne_bytes());
        }
        out
    }

    /// From its bytes in memory, as much of it as there is: what a program reads back.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut full = [0u8; Self::SIZE];
        let length = bytes.len().min(Self::SIZE);
        full[..length].copy_from_slice(&bytes[..length]);
        let mut release = [0u8; 64];
        release.copy_from_slice(&full[72..136]);
        let mut machine = [0u8; 16];
        machine.copy_from_slice(&full[136..152]);
        Self {
            abi_version: half_at(&full, 0),
            page_size: half_at(&full, 4),
            cpus_online: half_at(&full, 8),
            cpus_configured: half_at(&full, 12),
            total_pages: word_at(&full, 16),
            free_pages: word_at(&full, 24),
            uptime_ns: word_at(&full, 32),
            arg_max: word_at(&full, 40),
            fd_max: half_at(&full, 48),
            task_max: half_at(&full, 52),
            path_max: half_at(&full, 56),
            name_max: half_at(&full, 60),
            memory_limit: word_at(&full, 64),
            release,
            machine,
            flags: word_at(&full, 152),
            features: word_at(&full, 160),
            reserved: [word_at(&full, 168), word_at(&full, 176), word_at(&full, 184), word_at(&full, 192)],
        }
    }
}

/// The aux vector's types (section 7.2 of the design): Linux's, and feos's own from
/// `0x6665_0000`.
pub mod aux {
    pub const NULL: u64 = 0;
    pub const PHDR: u64 = 3;
    pub const PHENT: u64 = 4;
    pub const PHNUM: u64 = 5;
    pub const PAGESZ: u64 = 6;
    pub const BASE: u64 = 7;
    pub const FLAGS: u64 = 8;
    pub const ENTRY: u64 = 9;
    pub const UID: u64 = 11;
    pub const EUID: u64 = 12;
    pub const GID: u64 = 13;
    pub const EGID: u64 = 14;
    pub const PLATFORM: u64 = 15;
    pub const HWCAP: u64 = 16;
    pub const CLKTCK: u64 = 17;
    pub const SECURE: u64 = 23;
    pub const RANDOM: u64 = 25;
    pub const HWCAP2: u64 = 26;
    pub const EXECFN: u64 = 31;
    pub const MINSIGSTKSZ: u64 = 51;
    /// [`ABI_VERSION`](super::ABI_VERSION).
    pub const FEOS_ABI: u64 = 0x6665_0001;
    /// Bit 0: `AT_RANDOM`'s bytes came from the CSPRNG (clear before stage 1e.1).
    pub const FEOS_FLAGS: u64 = 0x6665_0002;
    /// [`FEATURES`](super::FEATURES), as [`SysInfo::features`](super::SysInfo::features).
    pub const FEOS_FEATURES: u64 = 0x6665_0003;
    /// The program's canonical absolute path, NUL-terminated: what `CURRENT_EXE` says.
    pub const FEOS_EXE: u64 = 0x6665_0004;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_fits_together() {
        assert!(USER_BASE < USER_IMAGE_END);
        assert_eq!(USER_PIE_BASE, USER_BASE);
        // The interpreter's gigabyte sits between the image and the mmap area.
        assert_eq!(USER_INTERP_BASE, USER_IMAGE_END);
        assert_eq!(USER_INTERP_BASE + (1 << 30), USER_MMAP_BASE);
        assert!(USER_MMAP_BASE < USER_MMAP_END);
        assert!(USER_MMAP_END + USER_STACK_GUARD + USER_STACK_SIZE + USER_ARGS_SIZE == USER_STACK_TOP);
        assert!(USER_STACK_TOP < USER_END);
        assert_eq!(USER_END - USER_STACK_TOP, PAGE_SIZE);
        for address in [
            USER_BASE, USER_IMAGE_END, USER_PIE_BASE, USER_INTERP_BASE, USER_MMAP_BASE, USER_MMAP_END, USER_STACK_TOP, USER_END,
            USER_ARGS_SIZE, USER_STACK_SIZE, USER_STACK_GUARD, USER_MEMORY_LIMIT,
        ] {
            assert_eq!(address % PAGE_SIZE, 0);
        }
        // The values section 6 of the design gives.
        assert_eq!(USER_MMAP_END, 0x7f_bf5f_f000);
        assert_eq!(USER_END, 512 << 30);
        assert_eq!(USER_MEMORY_LIMIT, 64 << 30);
        assert_eq!((USER_ARGS_SIZE, USER_STACK_SIZE, USER_STACK_GUARD), (2 << 20, 8 << 20, 1 << 30));
        // A process may have charged all it's allowed and still have room to reserve.
        assert!(USER_MEMORY_LIMIT < USER_MMAP_END - USER_MMAP_BASE);
    }

    #[test]
    fn flock_padding_is_ignored_and_written_zero() {
        let given = Flock { kind: f::WRLCK, whence: 1, reserved0: 0, start: -5, length: 1 << 40, pid: 77, reserved1: 0 };
        let mut bytes = given.to_bytes();
        bytes[4..8].fill(0xaa);
        bytes[28..32].fill(0x55);
        let read = Flock::from_bytes(bytes);
        assert_eq!(read, given);
        assert_eq!(read.to_bytes()[4..8], [0; 4]);
        assert_eq!(read.to_bytes()[28..32], [0; 4]);
    }

    #[test]
    fn abi_version_is_2() {
        assert_eq!(ABI_VERSION, 2);
    }

    #[test]
    fn error_numbers_are_linux() {
        let linux = [
            (errno::EPERM, 1), (errno::ENOENT, 2), (errno::EIO, 5), (errno::ENOEXEC, 8),
            (errno::EBADF, 9), (errno::ECHILD, 10), (errno::EAGAIN, 11), (errno::ENOMEM, 12),
            (errno::EACCES, 13), (errno::EFAULT, 14), (errno::EBUSY, 16), (errno::EEXIST, 17),
            (errno::ENODEV, 19), (errno::EISDIR, 21), (errno::EINVAL, 22), (errno::EMFILE, 24),
            (errno::ENOSPC, 28), (errno::EPIPE, 32), (errno::ENAMETOOLONG, 36),
            (errno::ENOSYS, 38), (errno::ENOTSUP, 95), (errno::ETIMEDOUT, 110),
            (errno::ETXTBSY, 26), (errno::EOVERFLOW, 75), (errno::EINTR, 4), (errno::EOPNOTSUPP, 95),
            (errno::ENFILE, 23), (errno::ESPIPE, 29), (errno::EFBIG, 27), (errno::EWOULDBLOCK, 11),
            (errno::ENOTDIR, 20), (errno::ENOTEMPTY, 39), (errno::ELOOP, 40), (errno::EILSEQ, 84),
            (errno::EROFS, 30), (errno::ERANGE, 34), (errno::EXDEV, 18), (errno::ENOLCK, 37), (errno::ESRCH, 3), (errno::E2BIG, 7),
        ];
        for (ours, theirs) in linux {
            assert_eq!(ours, theirs);
        }
    }

    #[test]
    fn memory_constants_are_linux() {
        assert_eq!((prot::NONE, prot::READ, prot::WRITE, prot::EXEC), (0, 1, 2, 4));
        assert_eq!((map::SHARED, map::PRIVATE, map::SHARED_VALIDATE), (1, 2, 3));
        assert_eq!((map::FIXED, map::ANONYMOUS, map::NORESERVE, map::POPULATE), (0x10, 0x20, 0x4000, 0x8000));
        assert_eq!((map::STACK, map::FIXED_NOREPLACE), (0x20000, 0x100000));
        assert_eq!(map::ALL, 0x12_c033);
        assert_eq!((madv::NORMAL, madv::RANDOM, madv::SEQUENTIAL, madv::WILLNEED, madv::DONTNEED), (0, 1, 2, 3, 4));
        assert_eq!((madv::FREE, madv::DONTFORK, madv::DOFORK), (8, 10, 11));
        assert_eq!((madv::HUGEPAGE, madv::NOHUGEPAGE, madv::DONTDUMP, madv::DODUMP), (14, 15, 16, 17));
        assert_eq!((ms::ASYNC, ms::INVALIDATE, ms::SYNC), (1, 2, 4));
        assert_eq!(EXIT_BUS, 128 + 7);
    }

    #[test]
    fn the_new_numbers_are_where_the_design_puts_them() {
        assert_eq!((nr::MMAP2, nr::MADVISE, nr::MSYNC), (31, 32, 33));
        assert_eq!((nr::THREAD_CREATE, nr::THREAD_EXIT, nr::GETTID, nr::SET_TLS), (34, 35, 36, 37));
        assert_eq!(nr::FIRST_V2, nr::MMAP2);
        assert_eq!((nr::FUTEX_WAIT2, nr::CLOCK_GETTIME, nr::CLOCK_GETRES, nr::NANOSLEEP), (38, 39, 40, 41));
        assert_eq!((nr::RENAMEAT, nr::LINKAT, nr::SYMLINKAT, nr::READLINKAT), (47, 48, 49, 50));
        assert_eq!((nr::FCHMODAT, nr::FCHOWNAT, nr::UTIMENSAT, nr::FACCESSAT), (51, 52, 53, 54));
        assert_eq!((nr::FSTATVFS, nr::SYNC), (67, 68));
        assert_eq!(nr::FLOCK, 69);
        assert_eq!(nr::FIRST_UNIMPLEMENTED, nr::FLOCK + 1);
        assert!(nr::FIRST_UNIMPLEMENTED <= nr::LAST_RESERVED);
        assert_eq!((nr::DUP3, nr::FCNTL, nr::PIPE2, nr::CLOSE_RANGE), (59, 60, 61, 107));
        assert_eq!((nr::OPENAT, nr::FSTATAT, nr::GETDENTS, nr::MKDIRAT, nr::UNLINKAT), (42, 43, 44, 45, 46));
        assert_eq!((nr::FTRUNCATE, nr::FSYNC, nr::PREAD, nr::PWRITE), (55, 56, 57, 58));
        assert_eq!((nr::FCHDIR, nr::GETCWD, nr::UMASK, nr::FD_PATH, nr::CURRENT_EXE), (62, 63, 64, 65, 66));
        assert!(!nr::is_unimplemented(nr::NANOSLEEP) && !nr::is_unimplemented(nr::UNLINKAT));
        assert!(nr::is_unimplemented(nr::FIRST_UNIMPLEMENTED));
        assert!(!nr::is_unimplemented(54) && !nr::is_unimplemented(67) && !nr::is_unimplemented(68));
        assert!(!nr::is_unimplemented(69) && nr::is_unimplemented(70) && nr::is_unimplemented(106) && !nr::is_unimplemented(nr::LAST_RESERVED));
        assert!(!nr::is_unimplemented(58) && !nr::is_unimplemented(62) && !nr::is_unimplemented(66));
        assert_eq!((nr::SPAWN2, nr::GETPPID, nr::GETPGID, nr::SETPGID, nr::GETSID, nr::SETSID), (72, 75, 76, 77, 78, 79));
        assert_eq!((nr::GETRUSAGE, nr::SYSINFO), (80, 81));
        assert_eq!((nr::EXEC, nr::WAIT4, nr::KILL), (73, 74, 82));
        assert!(!nr::is_unimplemented(nr::EXEC) && !nr::is_unimplemented(nr::WAIT4) && !nr::is_unimplemented(nr::SYSINFO));
        assert!(!nr::is_unimplemented(nr::KILL) && !nr::is_unimplemented(83) && !nr::is_unimplemented(84));
        assert_eq!((nr::SIGACTION, nr::SIGRETURN), (84, 89));
        assert_eq!((nr::THREAD_KILL, nr::SIGPROCMASK, nr::SIGALTSTACK, nr::SIGPENDING, nr::SIGSUSPEND, nr::SETITIMER), (83, 85, 86, 87, 88, 108));
        assert!((83..=89).all(|number| !nr::is_unimplemented(number)) && nr::is_unimplemented(90) && !nr::is_unimplemented(108));
        for number in nr::IMPLEMENTED_LATER {
            assert!(number > nr::FIRST_UNIMPLEMENTED && !nr::is_unimplemented(number));
        }
        assert!(!nr::is_unimplemented(nr::LAST_RESERVED + 1));
    }

    #[test]
    fn file_constants_are_linux() {
        assert_eq!((o::RDONLY, o::WRONLY, o::RDWR, o::ACCMODE), (0, 1, 2, 3));
        assert_eq!((o::CREAT, o::EXCL, o::NOCTTY, o::TRUNC, o::APPEND, o::NONBLOCK), (0o100, 0o200, 0o400, 0o1000, 0o2000, 0o4000));
        assert_eq!((o::DSYNC, o::ASYNC, o::DIRECT, o::LARGEFILE, o::DIRECTORY), (0o10000, 0o20000, 0o40000, 0o100000, 0o200000));
        assert_eq!((o::NOFOLLOW, o::NOATIME, o::CLOEXEC, o::SYNC, o::PATH, o::TMPFILE), (0o400000, 0o1000000, 0o2000000, 0o4010000, 0o10000000, 0o20200000));
        assert_eq!((f::DUPFD, f::GETFD, f::SETFD, f::GETFL, f::SETFL), (0, 1, 2, 3, 4));
        assert_eq!((f::GETLK, f::SETLK, f::SETLKW, f::DUPFD_CLOEXEC, f::FD_CLOEXEC), (5, 6, 7, 1030, 1));
        assert_eq!((f::RDLCK, f::WRLCK, f::UNLCK), (0, 1, 2));
        assert_eq!((lock::SH, lock::EX, lock::NB, lock::UN), (1, 2, 4, 8));
        assert_eq!((MAX_LOCKS, MAX_PROCESS_LOCKS), (4096, 256));
        assert_eq!((close_range::UNSHARE, close_range::CLOEXEC), (2, 4));
        assert_eq!((MAX_FDS, PATH_MAX, NAME_MAX, PIPE_BUF), (1024, 4096, 255, 4096));
        // `O_ASYNC` and `O_TMPFILE`'s own bit are neither settable nor ignored.
        assert_eq!(o::ASYNC & (o::SETTABLE | o::FIXED_AT_OPEN), 0);
        assert_eq!(o::TMPFILE & !o::DIRECTORY & (o::SETTABLE | o::FIXED_AT_OPEN), 0);
        assert_eq!(o::SETTABLE & o::FIXED_AT_OPEN, 0);
    }

    #[test]
    fn path_constants_are_linux() {
        assert_eq!(at::FDCWD, -100);
        assert_eq!((at::SYMLINK_NOFOLLOW, at::REMOVEDIR, at::EACCESS, at::SYMLINK_FOLLOW), (0x100, 0x200, 0x200, 0x400));
        assert_eq!((at::NO_AUTOMOUNT, at::EMPTY_PATH), (0x800, 0x1000));
        assert_eq!((s_if::SOCK, s_if::LNK, s_if::REG, s_if::BLK), (0o140000, 0o120000, 0o100000, 0o60000));
        assert_eq!((s_if::DIR, s_if::CHR, s_if::FIFO, s_if::MASK), (0o40000, 0o20000, 0o10000, 0o170000));
        assert_eq!((dt::UNKNOWN, dt::FIFO, dt::CHR, dt::DIR, dt::REG, dt::LNK, dt::SOCK), (0, 1, 2, 4, 8, 10, 12));
        assert_eq!((stat_flags::POSIX, stat_flags::BTIME, stat_flags::READ_ONLY), (1, 2, 4));
        assert_eq!(FSYNC_DATA, 1);
        assert_eq!(CONSOLE_RDEV, 0x501);
        assert_eq!((rename::NOREPLACE, rename::EXCHANGE, rename::WHITEOUT), (1, 2, 4));
        assert_eq!((utime::NOW, utime::OMIT), (0x3fff_ffff, 0x3fff_fffe));
        assert_eq!((access::F_OK, access::X_OK, access::W_OK, access::R_OK), (0, 1, 2, 4));
        assert_eq!((st::RDONLY, st::NOSUID), (1, 2));
        // `-1` as a 32-bit owner, whichever width the caller wrote it in.
        assert_eq!((int_arg(KEEP), int_arg(u32::MAX as usize)), (Some(-1), Some(-1)));
        // `O_TMPFILE`'s own bit isn't known, `O_ASYNC` isn't either.
        assert_eq!(o::ASYNC & o::OPENAT_KNOWN, 0);
        assert_ne!(o::TMPFILE & !o::OPENAT_KNOWN, 0);
    }

    #[test]
    fn stat2_layout() {
        assert_eq!(core::mem::size_of::<Stat2>(), 144);
        assert_eq!(core::mem::align_of::<Stat2>(), 8);
        let stat = Stat2 {
            dev: 1,
            ino: 2,
            mode: s_if::REG | 0o644,
            nlink: 3,
            uid: 4,
            gid: 5,
            rdev: 6,
            size: 7,
            blksize: 4096,
            blocks: 8,
            atime: Timespec { sec: 9, nsec: 10 },
            mtime: Timespec { sec: 11, nsec: 12 },
            ctime: Timespec { sec: 13, nsec: 14 },
            btime: Timespec { sec: 15, nsec: 16 },
            flags: stat_flags::POSIX,
            reserved0: 0,
            reserved1: 0,
        };
        // The bytes are the struct's own, field for field.
        // SAFETY: a `repr(C)` struct of integers with no padding, read as bytes.
        let raw = unsafe { core::slice::from_raw_parts(&stat as *const Stat2 as *const u8, Stat2::SIZE) };
        assert_eq!(&stat.to_bytes()[..], raw);
        assert_eq!(&stat.to_bytes()[80..88], &11i64.to_ne_bytes());
    }

    #[test]
    fn dirent_layout() {
        assert_eq!(Dirent::HEADER, 19);
        assert_eq!(Dirent::record_length(0), 24);
        assert_eq!(Dirent::record_length(4), 24);
        assert_eq!(Dirent::record_length(5), 32);
        assert_eq!(Dirent::record_length(NAME_MAX) % 8, 0);
        let mut out = [0xffu8; 64];
        let length = Dirent::encode(7, 3, dt::REG, b"hello", &mut out).unwrap();
        assert_eq!(length, 32);
        assert_eq!(&out[..8], &7u64.to_ne_bytes());
        assert_eq!(&out[8..16], &3i64.to_ne_bytes());
        assert_eq!(u16::from_ne_bytes([out[16], out[17]]), 32);
        assert_eq!(out[18], dt::REG);
        assert_eq!(&out[19..24], b"hello");
        assert!(out[24..32].iter().all(|&byte| byte == 0));
        assert_eq!(out[32], 0xff);
        let (header, name) = Dirent::decode(&out).unwrap();
        assert_eq!((header.ino, header.off, header.reclen, header.kind, name), (7, 3, 32, dt::REG, &b"hello"[..]));
        assert_eq!(Dirent::encode(1, 1, dt::DIR, b"hello", &mut out[..31]), None);
        assert_eq!(Dirent::encode(1, 1, dt::DIR, &[b'x'; NAME_MAX + 1], &mut [0u8; 512]), None);
    }

    #[test]
    fn path_at_layout() {
        assert_eq!(core::mem::size_of::<PathAt>(), 24);
        assert_eq!(core::mem::offset_of!(PathAt, dirfd), 0);
        assert_eq!(core::mem::offset_of!(PathAt, reserved), 4);
        assert_eq!(core::mem::offset_of!(PathAt, pointer), 8);
        assert_eq!(core::mem::offset_of!(PathAt, length), 16);
        let path = PathAt { dirfd: at::FDCWD, reserved: 7, pointer: 0x1234_5678_9abc, length: 42 };
        // SAFETY: a plain `repr(C)` struct of integers with no padding, read as bytes.
        let bytes: [u8; PathAt::SIZE] = unsafe { core::mem::transmute(path) };
        assert_eq!(PathAt::from_bytes(bytes), path);
    }

    #[test]
    fn statvfs_layout() {
        assert_eq!(core::mem::size_of::<StatVfs>(), StatVfs::SIZE);
        let stat = StatVfs { bsize: 1, frsize: 2, blocks: 3, bfree: 4, bavail: 5, files: 6, ffree: 7, favail: 8, fsid: 9, flags: 10, namemax: 11 };
        // SAFETY: as above.
        let bytes: [u8; StatVfs::SIZE] = unsafe { core::mem::transmute(stat) };
        assert_eq!(stat.to_bytes(), bytes);
    }

    #[test]
    fn time_constants_are_linux() {
        assert_eq!((clock::REALTIME, clock::MONOTONIC, clock::PROCESS_CPUTIME_ID, clock::THREAD_CPUTIME_ID), (0, 1, 2, 3));
        assert_eq!((clock::MONOTONIC_RAW, clock::REALTIME_COARSE, clock::MONOTONIC_COARSE, clock::BOOTTIME), (4, 5, 6, 7));
        assert_eq!(timer::ABSTIME, 1);
    }

    #[test]
    fn timespec_is_linux_layout() {
        assert_eq!(core::mem::size_of::<Timespec>(), 16);
        assert_eq!(core::mem::align_of::<Timespec>(), 8);
        assert_eq!(core::mem::offset_of!(Timespec, sec), 0);
        assert_eq!(core::mem::offset_of!(Timespec, nsec), 8);
        let time = Timespec { sec: 0x0102_0304_0506_0708, nsec: 999_999_999 };
        assert_eq!(Timespec::from_bytes(time.to_bytes()), time);
        assert_eq!(&time.to_bytes()[..8], &0x0102_0304_0506_0708i64.to_ne_bytes());
    }

    #[test]
    fn timespec_is_checked_whole() {
        assert_eq!(Timespec { sec: 2, nsec: 5 }.to_nanos(), Some(2_000_000_005));
        assert_eq!(Timespec { sec: 0, nsec: 999_999_999 }.to_nanos(), Some(999_999_999));
        assert_eq!(Timespec { sec: 0, nsec: 1_000_000_000 }.to_nanos(), None);
        assert_eq!(Timespec { sec: 0, nsec: -1 }.to_nanos(), None);
        assert_eq!(Timespec { sec: -1, nsec: 0 }.to_nanos(), None);
        // The upper half of `nsec` counts: no 32-bit `long` with padding above it.
        assert_eq!(Timespec { sec: 0, nsec: 1 << 32 }.to_nanos(), None);
        assert_eq!(Timespec { sec: i64::MAX, nsec: 0 }.to_nanos(), Some(u64::MAX));
        assert_eq!(Timespec::from_nanos(3_000_000_007), Timespec { sec: 3, nsec: 7 });
    }

    #[test]
    fn int_args_are_their_low_half_sign_extended() {
        assert_eq!(int_arg(5), Some(5));
        assert_eq!(int_arg(u32::MAX as usize), Some(-1));
        assert_eq!(int_arg(usize::MAX), Some(-1));
        assert_eq!(int_arg(0xffff_ffff_0000_0005), None);
        assert_eq!(int_arg(0x1_0000_0000), None);
        assert_eq!(int_arg(0xffff_ffff_7fff_ffff), None);
        assert_eq!(int_arg(0x8000_0000), Some(i32::MIN));
    }

    #[test]
    fn errors_are_told_from_addresses() {
        assert_eq!(error_of(-14), Some(14));
        assert_eq!(error_of(0), None);
        assert_eq!(error_of(USER_BASE as isize), None);
        assert_eq!(error_of(-4096), None);
    }

    #[test]
    fn spawn_structs_are_laid_out_as_the_design_says() {
        assert_eq!(core::mem::size_of::<SpawnRequest>(), SpawnRequest::SIZE);
        assert_eq!(core::mem::size_of::<SpawnAction>(), SpawnAction::SIZE);
        assert_eq!(core::mem::size_of::<SysInfo>(), SysInfo::SIZE);
        assert_eq!(core::mem::size_of::<Rusage>(), Rusage::SIZE);
        let request = SpawnRequest {
            path: PathAt { dirfd: at::FDCWD, reserved: 0, pointer: 0x1234, length: 9 },
            argv: 1, argc: 2, envp: 3, envc: 4, actions: 5, action_count: 6, flags: 7, pgroup: -8, sigmask: 9,
            sigdefault: 10, reserved: [11, 12, 13],
        };
        // SAFETY: a `repr(C)` struct of integers with no padding, as its bytes.
        let bytes: [u8; SpawnRequest::SIZE] = unsafe { core::mem::transmute(request) };
        assert_eq!(request.to_bytes(), bytes);
        assert_eq!(SpawnRequest::from_bytes(&bytes), request);
        let action = SpawnAction {
            kind: spawn_action::OPEN, fd: 3, to: -1, open_flags: 0o101, mode: 0o644, reserved: 0,
            path: PathAt { dirfd: 5, reserved: 0, pointer: 77, length: 3 },
        };
        // SAFETY: as above.
        let bytes: [u8; SpawnAction::SIZE] = unsafe { core::mem::transmute(action) };
        assert_eq!(action.to_bytes(), bytes);
        assert_eq!(SpawnAction::from_bytes(&bytes), action);
        assert_eq!(core::mem::size_of::<ExecRequest>(), ExecRequest::SIZE);
        let exec = ExecRequest {
            path: PathAt { dirfd: 7, reserved: 0, pointer: 0x55, length: 4 },
            argv: 1, argc: 2, envp: 3, envc: 4, flags: spawn::EMPTY_PATH, reserved: [5, 6],
        };
        // SAFETY: as above.
        let bytes: [u8; ExecRequest::SIZE] = unsafe { core::mem::transmute(exec) };
        assert_eq!(exec.to_bytes(), bytes);
        assert_eq!(ExecRequest::from_bytes(&bytes), exec);
    }

    #[test]
    fn sysinfo_and_rusage_bytes_are_their_layout() {
        let mut info = SysInfo { abi_version: 2, page_size: 4096, cpus_online: 3, cpus_configured: 4, ..SysInfo::default() };
        info.total_pages = 5;
        info.features = FEATURES;
        info.release[..4].copy_from_slice(b"0.1\0");
        info.machine[..6].copy_from_slice(b"x86_64");
        info.memory_limit = USER_MEMORY_LIMIT as u64;
        // SAFETY: a `repr(C)` struct of integers and byte arrays with no padding.
        let bytes: [u8; SysInfo::SIZE] = unsafe { core::mem::transmute(info) };
        assert_eq!(info.to_bytes(), bytes);
        assert_eq!(SysInfo::from_bytes(&bytes), info);
        // A shorter copy reads as zeroes past its end.
        assert_eq!(SysInfo::from_bytes(&bytes[..8]).cpus_online, 0);
        let usage = Rusage { utime: Timeval { sec: 1, usec: 2 }, maxrss: 3, nivcsw: 4, ..Rusage::default() };
        // SAFETY: as above.
        let bytes: [u8; Rusage::SIZE] = unsafe { core::mem::transmute(usage) };
        assert_eq!(usage.to_bytes(), bytes);
        assert_eq!(Timeval::from_nanos(2_000_003_999), Timeval { sec: 2, usec: 3 });
    }

    #[test]
    fn spawn_values_are_glibcs() {
        assert_eq!((spawn::RESETIDS, spawn::SETPGROUP, spawn::SETSIGDEF, spawn::SETSIGMASK), (1, 2, 4, 8));
        assert_eq!((spawn::SETSCHEDPARAM, spawn::SETSCHEDULER, spawn::USEVFORK, spawn::SETSID), (0x10, 0x20, 0x40, 0x80));
        assert_eq!(spawn::EMPTY_PATH, 0x1000);
        assert_eq!(spawn::ALL & (spawn::SETSCHEDPARAM | spawn::SETSCHEDULER), 0);
        assert_eq!(spawn::ARG_MAX, 1 << 20);
        let kinds = [
            spawn_action::CLOSE, spawn_action::DUP2, spawn_action::OPEN, spawn_action::CHDIR, spawn_action::FCHDIR,
            spawn_action::CLOSEFROM, spawn_action::TCSETPGRP,
        ];
        assert_eq!(kinds, [1, 2, 3, 4, 5, 6, 7]);
        assert_eq!((rusage::SELF, rusage::CHILDREN, rusage::THREAD), (0, -1, 1));
        assert_eq!((aux::PHDR, aux::PAGESZ, aux::RANDOM, aux::EXECFN, aux::MINSIGSTKSZ), (3, 6, 25, 31, 51));
        assert_eq!((aux::FEOS_ABI, aux::FEOS_FEATURES, aux::FEOS_EXE), (0x6665_0001, 0x6665_0003, 0x6665_0004));
    }

    #[test]
    fn signals_and_wait_statuses_are_linuxs() {
        assert_eq!((sig::HUP, sig::INT, sig::KILL, sig::SEGV, sig::TERM, sig::CHLD), (1, 2, 9, 11, 15, 17));
        assert_eq!((sig::CONT, sig::STOP, sig::TSTP, sig::TTIN, sig::TTOU, sig::WINCH), (18, 19, 20, 21, 22, 28));
        assert_eq!((sig::SYS, sig::RTMIN, sig::MAX), (31, 32, 64));
        assert_eq!((wait::NOHANG, wait::UNTRACED, wait::CONTINUED), (1, 2, 8));
        // glibc's macros on the statuses a kernel writes.
        assert!(wait::exited(42 << 8) && wait::exit_code(42 << 8) == 42 && !wait::signaled(42 << 8));
        assert!(wait::signaled(15) && wait::term_signal(15) == 15 && !wait::exited(15) && !wait::stopped(15));
        assert!(wait::stopped(0x137f) && wait::stop_signal(0x137f) == 19 && !wait::signaled(0x137f));
        assert!(wait::continued(0xffff) && !wait::exited(0xffff) && !wait::stopped(0xffff));
    }

    #[test]
    fn features_cover_every_stage_so_far() {
        // 1a.1 to 1c.10: bits 0 to 20, and nothing beyond.
        assert_eq!(FEATURES.count_ones(), 21);
        assert_eq!(FEATURES >> 21, 0);
    }

    #[test]
    fn signal_structs_are_linuxs() {
        assert_eq!((sa::NOCLDSTOP, sa::NOCLDWAIT, sa::SIGINFO, sa::RESTORER), (1, 2, 4, 0x0400_0000));
        assert_eq!((sa::ONSTACK, sa::RESTART, sa::NODEFER, sa::RESETHAND), (0x0800_0000, 0x1000_0000, 0x4000_0000, 0x8000_0000));
        assert_eq!((sa::DFL, sa::IGN), (0, 1));
        assert_eq!((si::USER, si::KERNEL, si::TKILL), (0, 0x80, -6));
        assert_eq!((si::CLD_EXITED, si::CLD_KILLED, si::CLD_STOPPED, si::CLD_CONTINUED), (1, 2, 5, 6));
        assert_eq!((si::SEGV_MAPERR, si::SEGV_ACCERR, si::BUS_ADRALN, si::BUS_ADRERR), (1, 2, 1, 2));
        assert_eq!((si::ILL_ILLOPC, si::FPE_INTDIV, si::TRAP_BRKPT), (1, 1, 1));
        assert_eq!((ss::ONSTACK, ss::DISABLE, ss::MINSIGSTKSZ, ss::SIGSTKSZ), (1, 2, 2048, 8192));
        assert_eq!((how::BLOCK, how::UNBLOCK, how::SETMASK), (0, 1, 2));
        assert_eq!((itimer::REAL, itimer::VIRTUAL, itimer::PROF), (0, 1, 2));
        let timer = ITimerVal { interval: Timeval { sec: 1, usec: 2 }, value: Timeval { sec: -3, usec: 999_999 } };
        // SAFETY: a `repr(C)` struct of integers with no padding.
        let bytes: [u8; ITimerVal::SIZE] = unsafe { core::mem::transmute(timer) };
        assert_eq!(timer.to_bytes(), bytes);
        assert_eq!(ITimerVal::from_bytes(bytes), timer);
        let stack = SigStack { sp: 0x7f_0000_0000, flags: ss::ONSTACK, reserved: 0, size: 8192 };
        let mut bytes = stack.to_bytes();
        // SAFETY: as above; the padding is zero here.
        assert_eq!(bytes, unsafe { core::mem::transmute::<SigStack, [u8; SigStack::SIZE]>(stack) });
        bytes[13] = 0xee;
        assert_eq!(SigStack::from_bytes(bytes), stack, "the padding after a C ss_flags is ignored");
        let action = SigAction { handler: 0x10_0000_1000, flags: sa::RESTORER | sa::RESETHAND, reserved: 0, restorer: 0x10_0000_2000, mask: 1 << 9 };
        // SAFETY: a `repr(C)` struct of integers whose padding is zero here.
        let bytes: [u8; SigAction::SIZE] = unsafe { core::mem::transmute(action) };
        assert_eq!(action.to_bytes(), bytes);
        let mut dirty = bytes;
        dirty[12] = 0xff;
        assert_eq!(SigAction::from_bytes(dirty), action, "the padding after a C sa_flags is ignored");
        let info = SigInfo::child(si::CLD_EXITED, 42, 1000, 3, 5, 6);
        let bytes = info.to_bytes();
        assert_eq!(SigInfo::from_bytes(&bytes), info);
        assert_eq!((info.signo, info.code, info.pid(), info.uid(), info.status()), (17, 1, 42, 1000, 3));
        assert_eq!(i64::from_ne_bytes(bytes[32..40].try_into().unwrap()), 5);
        assert_eq!(SigInfo::fault(sig::SEGV, si::SEGV_MAPERR, 0x1234).address(), 0x1234);
    }

    #[test]
    fn ucontext_layouts_on_any_host() {
        // Both ports' layouts, wherever the tests run: section 8.4 of the design.
        assert_eq!(core::mem::size_of::<ucontext::X86_64>(), 816);
        assert_eq!(core::mem::align_of::<ucontext::X86_64>(), 16);
        assert_eq!(core::mem::offset_of!(ucontext::X86_64, fxsave) % 16, 0);
        assert_eq!(core::mem::size_of::<ucontext::Aarch64>(), 880);
        assert_eq!(core::mem::align_of::<ucontext::Aarch64>(), 16);
        assert_eq!(core::mem::offset_of!(ucontext::Aarch64, v), 336);
        assert_eq!(ucontext::greg::CR2, 22);
        assert_eq!((ucontext::greg::RSP, ucontext::greg::RIP, ucontext::greg::EFL), (15, 16, 17));
        assert_eq!(ucontext::UC_FEOS_FXSAVE, 1);
        assert_eq!(core::mem::size_of::<UContext>() % 16, 0);
    }
}
