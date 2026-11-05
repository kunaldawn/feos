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
/// Bytes of the initial stack, all mapped from the start.
pub const USER_STACK_SIZE: usize = 128 * 1024;
/// Unmapped space below the stack, so that an overflow faults instead of reaching
/// anything `mmap` handed out.
pub const USER_STACK_GUARD: usize = 16 * 1024 * 1024;
/// One past the addresses `mmap` hands out.
pub const USER_MMAP_END: usize = USER_STACK_TOP - USER_STACK_SIZE - USER_STACK_GUARD;

/// Bytes of a page: the granularity of `mmap` and of an ELF segment's placement.
pub const PAGE_SIZE: usize = 4096;
/// Most memory one process can have mapped, in bytes.
pub const USER_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
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
    /// from [`prot`](super::prot).
    pub const MMAP: usize = 8;
    /// `munmap(address, length) -> 0`: only what `mmap` handed out.
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
}

/// Error codes, returned negated.
pub mod errno {
    /// No such file or directory.
    pub const ENOENT: isize = 2;
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
    pub const READ: usize = 1 << 0;
    pub const WRITE: usize = 1 << 1;
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
    fn errors_are_told_from_addresses() {
        assert_eq!(error_of(-14), Some(14));
        assert_eq!(error_of(0), None);
        assert_eq!(error_of(USER_BASE as isize), None);
        assert_eq!(error_of(-4096), None);
    }
}
