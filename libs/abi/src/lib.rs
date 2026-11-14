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
//! aligned stack.

#![cfg_attr(not(test), no_std)]

/// Lowest address a user program can have mapped.
pub const USER_BASE: usize = 0x10_0000_0000;
/// One past the highest address a user program can have mapped.
pub const USER_END: usize = 0x14_0000_0000;
/// Where programs are linked, and where `mmap` starts handing out memory above.
pub const USER_IMAGE_END: usize = 0x11_0000_0000;
/// One past the top of the initial stack. There's an unmapped page above it.
pub const USER_STACK_TOP: usize = USER_END - 0x1000;
/// Bytes of the initial stack, promised from the start and given frames on first touch.
pub const USER_STACK_SIZE: usize = 128 * 1024;
/// Unmapped space below the stack, so that an overflow faults instead of reaching
/// anything `mmap` handed out.
pub const USER_STACK_GUARD: usize = 16 * 1024 * 1024;
/// One past the addresses `mmap` hands out.
pub const USER_MMAP_END: usize = USER_STACK_TOP - USER_STACK_SIZE - USER_STACK_GUARD;

/// Bytes of a page: the granularity of `mmap` and of an ELF segment's placement.
pub const PAGE_SIZE: usize = 4096;
/// Most memory one process can have charged or owned, in bytes: pages promised to it,
/// frames it has that no promise covers, and its page tables. A reservation with no
/// access, or one made with `MAP_NORESERVE`, isn't charged, so it can be larger. A
/// machine with less to spare says `ENOMEM` sooner.
pub const USER_MEMORY_LIMIT: usize = 4 * 1024 * 1024 * 1024;
/// Most mappings one process can have: each `mmap` adds one, unless it joins a
/// neighbour, and a `munmap` or `mprotect` in the middle of one splits it.
pub const MAX_VMAS: usize = 16384;
/// Open files one process can have, standard streams included.
pub const MAX_FDS: usize = 16;
/// Longest path a call accepts.
pub const PATH_MAX: usize = 192;
/// Most arguments `spawn` accepts.
pub const MAX_ARGS: usize = 32;
/// Most bytes one `read` or `write` moves. A longer one returns a short count.
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
    /// `exit(status: i32) -> !`
    pub const EXIT: usize = 0;
    /// `write(fd, buffer, length) -> count`
    pub const WRITE: usize = 1;
    /// `read(fd, buffer, length) -> count`, where zero is end of file.
    pub const READ: usize = 2;
    /// `open(path, path_length, flags) -> fd`, flags from [`open`](super::open).
    pub const OPEN: usize = 3;
    /// `close(fd) -> 0`
    pub const CLOSE: usize = 4;
    /// `sleep(milliseconds) -> 0`
    pub const SLEEP: usize = 5;
    /// `yield() -> 0`
    pub const YIELD: usize = 6;
    /// `getpid() -> pid`
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
    /// [`INHERIT`](super::INHERIT) inherits this process's.
    pub const SPAWN: usize = 10;
    /// `wait(pid) -> status`: this process's children only.
    pub const WAIT: usize = 11;
    /// `futex_wait(address, expected, timeout_ms) -> 0`, or `EAGAIN` if the word at
    /// `address` isn't `expected`, or `ETIMEDOUT`. A timeout of zero is none.
    pub const FUTEX_WAIT: usize = 12;
    /// `futex_wake(address, count) -> woken`
    pub const FUTEX_WAKE: usize = 13;
    /// `pipe(fds: *mut [u32; 2]) -> 0`: read end, then write end.
    pub const PIPE: usize = 14;
    /// `seek(fd, offset, whence) -> position`, `whence` from [`seek`](super::seek).
    pub const SEEK: usize = 15;
    /// `getcpu() -> cpu`
    pub const GETCPU: usize = 16;
    /// `uptime() -> milliseconds`
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
    pub const STAT: usize = 24;
    /// `chmod(path, length, mode) -> 0`: the owner or root.
    pub const CHMOD: usize = 25;
    /// `chown(path, length, uid, gid) -> 0`: root, and [`KEEP`](crate::KEEP) leaves one as it is.
    pub const CHOWN: usize = 26;
    /// `mkdir(path, length) -> 0`
    pub const MKDIR: usize = 27;
    /// `unlink(path, length) -> 0`: a file, link or empty directory.
    pub const UNLINK: usize = 28;
    /// `link(existing, length, path, length) -> 0`: another name for a file.
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
    /// The first number of the calls `docs/feos-abi-v2.md` adds. Every number from here
    /// to [`LAST_RESERVED`] is taken, and is `ENOSYS` until the stage that brings it.
    pub const FIRST_V2: usize = 31;
    /// The last number `docs/feos-abi-v2.md` gives out.
    pub const LAST_RESERVED: usize = 108;
    /// The first reserved number with no call behind it yet.
    pub const FIRST_UNIMPLEMENTED: usize = 34;
}

/// Leaves the owner or group as it is, in [`nr::CHOWN`].
pub const KEEP: usize = usize::MAX;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_fits_together() {
        assert!(USER_BASE < USER_IMAGE_END);
        assert!(USER_IMAGE_END < USER_MMAP_END);
        assert!(USER_MMAP_END + USER_STACK_GUARD + USER_STACK_SIZE == USER_STACK_TOP);
        assert!(USER_STACK_TOP < USER_END);
        for address in [USER_BASE, USER_IMAGE_END, USER_MMAP_END, USER_STACK_TOP, USER_END] {
            assert_eq!(address % PAGE_SIZE, 0);
        }
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
        assert_eq!(nr::FIRST_V2, nr::MMAP2);
        assert!(nr::FIRST_UNIMPLEMENTED > nr::MSYNC && nr::FIRST_UNIMPLEMENTED <= nr::LAST_RESERVED);
    }

    #[test]
    fn errors_are_told_from_addresses() {
        assert_eq!(error_of(-14), Some(14));
        assert_eq!(error_of(0), None);
        assert_eq!(error_of(USER_BASE as isize), None);
        assert_eq!(error_of(-4096), None);
    }
}
