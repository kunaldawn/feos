//! The user runtime: the entry point, system calls and a little printing. Everything a
//! program here knows about the kernel goes through this and `feos-abi`.

#![no_std]

use core::fmt;

pub use feos_abi as abi;
use feos_abi::{errno, nr, Event, UserStr, INHERIT};

/// A raw system call. Returns what the kernel returned: see [`abi::error_of`].
#[cfg(target_arch = "aarch64")]
#[inline]
pub fn syscall(number: usize, args: [usize; 6]) -> isize {
    let result: isize;
    // SAFETY: the trap only changes `x0`. Anything wrong with the arguments is for the
    // kernel to find, and that's the point.
    unsafe {
        core::arch::asm!(
            "svc #0",
            inlateout("x0") args[0] => result,
            in("x1") args[1],
            in("x2") args[2],
            in("x3") args[3],
            in("x4") args[4],
            in("x5") args[5],
            in("x8") number,
            options(nostack),
        );
    }
    result
}

/// A raw system call. Returns what the kernel returned: see [`abi::error_of`].
#[cfg(target_arch = "x86_64")]
#[inline]
pub fn syscall(number: usize, args: [usize; 6]) -> isize {
    let result: isize;
    // SAFETY: as on AArch64. The trap only changes `rax`.
    unsafe {
        core::arch::asm!(
            "int 0xf0",
            inlateout("rax") number as isize => result,
            in("rdi") args[0],
            in("rsi") args[1],
            in("rdx") args[2],
            in("r10") args[3],
            in("r8") args[4],
            in("r9") args[5],
            options(nostack),
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    "_start:",
    "    bl feos_main",
    "    brk #0",
);

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    "_start:",
    "    call feos_main",
    "    ud2",
);

/// Declares a program's `fn main(args: Args) -> i32`.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[no_mangle]
        extern "C" fn feos_main(argc: usize, argv: *const $crate::abi::UserStr) -> ! {
            // SAFETY: the kernel starts a program with these describing its arguments.
            let args = unsafe { $crate::Args::new(argc, argv) };
            let status: i32 = $main(args);
            $crate::exit(status)
        }
    };
}

/// A program's arguments, its own name first.
#[derive(Clone, Copy)]
pub struct Args {
    strings: &'static [UserStr],
}

impl Args {
    /// # Safety
    /// As the kernel promises about what it gives a new program.
    pub unsafe fn new(argc: usize, argv: *const UserStr) -> Self {
        Self {
            // SAFETY: as the caller promises.
            strings: unsafe { core::slice::from_raw_parts(argv, argc) },
        }
    }

    pub fn len(&self) -> usize {
        self.strings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&'static str> {
        let string = self.strings.get(index)?;
        // SAFETY: as `new` promises: the kernel put these strings on the stack and copied
        // them from a `&str`.
        let bytes = unsafe { core::slice::from_raw_parts(string.pointer as *const u8, string.length) };
        core::str::from_utf8(bytes).ok()
    }

    /// The value after `flag`.
    pub fn value_of(&self, flag: &str) -> Option<&'static str> {
        (0..self.len()).find(|index| self.get(*index) == Some(flag)).and_then(|index| self.get(index + 1))
    }

    /// Whether `flag` is there.
    pub fn has(&self, flag: &str) -> bool {
        (0..self.len()).any(|index| self.get(index) == Some(flag))
    }
}

pub fn exit(status: i32) -> ! {
    syscall(nr::EXIT, [status as usize, 0, 0, 0, 0, 0]);
    // The kernel doesn't come back from that. If something did, the machine should know.
    loop {
        core::hint::spin_loop();
    }
}

pub fn write(fd: usize, bytes: &[u8]) -> isize {
    syscall(nr::WRITE, [fd, bytes.as_ptr() as usize, bytes.len(), 0, 0, 0])
}

pub fn read(fd: usize, out: &mut [u8]) -> isize {
    syscall(nr::READ, [fd, out.as_mut_ptr() as usize, out.len(), 0, 0, 0])
}

pub fn open(path: &str, flags: usize) -> isize {
    syscall(nr::OPEN, [path.as_ptr() as usize, path.len(), flags, 0, 0, 0])
}

pub fn getuid() -> isize {
    syscall(nr::GETUID, [0; 6])
}

pub fn getgid() -> isize {
    syscall(nr::GETGID, [0; 6])
}

/// Becomes this user, for good. Only root can.
pub fn setuid(uid: usize, gid: usize) -> isize {
    syscall(nr::SETUID, [uid, gid, 0, 0, 0, 0])
}

pub fn stat(path: &str) -> Result<abi::Stat, isize> {
    let mut out = abi::Stat::default();
    match syscall(nr::STAT, [path.as_ptr() as usize, path.len(), &mut out as *mut abi::Stat as usize, 0, 0, 0]) {
        0 => Ok(out),
        error => Err(error),
    }
}

pub fn chmod(path: &str, mode: usize) -> isize {
    syscall(nr::CHMOD, [path.as_ptr() as usize, path.len(), mode, 0, 0, 0])
}

pub fn chown(path: &str, uid: usize, gid: usize) -> isize {
    syscall(nr::CHOWN, [path.as_ptr() as usize, path.len(), uid, gid, 0, 0])
}

pub fn mkdir(path: &str) -> isize {
    syscall(nr::MKDIR, [path.as_ptr() as usize, path.len(), 0, 0, 0, 0])
}

pub fn unlink(path: &str) -> isize {
    syscall(nr::UNLINK, [path.as_ptr() as usize, path.len(), 0, 0, 0, 0])
}

pub fn link(existing: &str, path: &str) -> isize {
    syscall(nr::LINK, [existing.as_ptr() as usize, existing.len(), path.as_ptr() as usize, path.len(), 0, 0])
}

pub fn close(fd: usize) -> isize {
    syscall(nr::CLOSE, [fd, 0, 0, 0, 0, 0])
}

pub fn seek(fd: usize, offset: isize, whence: usize) -> isize {
    syscall(nr::SEEK, [fd, offset as usize, whence, 0, 0, 0])
}

pub fn sleep_ms(ms: usize) {
    syscall(nr::SLEEP, [ms, 0, 0, 0, 0, 0]);
}

pub fn yield_now() {
    syscall(nr::YIELD, [0; 6]);
}

pub fn getpid() -> isize {
    syscall(nr::GETPID, [0; 6])
}

pub fn getcpu() -> isize {
    syscall(nr::GETCPU, [0; 6])
}

pub fn uptime_ms() -> isize {
    syscall(nr::UPTIME, [0; 6])
}

pub fn mmap(length: usize, prot: usize) -> isize {
    syscall(nr::MMAP, [length, prot, 0, 0, 0, 0])
}

pub fn munmap(address: usize, length: usize) -> isize {
    syscall(nr::MUNMAP, [address, length, 0, 0, 0, 0])
}

pub fn mprotect(address: usize, length: usize, prot: usize) -> isize {
    syscall(nr::MPROTECT, [address, length, prot, 0, 0, 0])
}

/// Starts a program. `args` includes its own name.
pub fn spawn(path: &str, args: &[&str]) -> isize {
    spawn_io(path, args, INHERIT, INHERIT)
}

/// Starts a program with some of its streams from this process's files.
pub fn spawn_io(path: &str, args: &[&str], stdin: usize, stdout: usize) -> isize {
    let mut strings = [UserStr::default(); abi::MAX_ARGS];
    let count = args.len().min(strings.len());
    for (string, arg) in strings.iter_mut().zip(args.iter()) {
        *string = UserStr { pointer: arg.as_ptr() as usize, length: arg.len() };
    }
    syscall(
        nr::SPAWN,
        [path.as_ptr() as usize, path.len(), strings.as_ptr() as usize, count, stdin, stdout],
    )
}

pub fn wait(pid: usize) -> isize {
    syscall(nr::WAIT, [pid, 0, 0, 0, 0, 0])
}

pub fn pipe() -> Result<(usize, usize), isize> {
    let mut fds = [0u32; 2];
    match syscall(nr::PIPE, [fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0]) {
        0 => Ok((fds[0] as usize, fds[1] as usize)),
        error => Err(error),
    }
}

pub fn futex_wait(word: &core::sync::atomic::AtomicU32, expected: u32, timeout_ms: usize) -> isize {
    syscall(nr::FUTEX_WAIT, [word as *const _ as usize, expected as usize, timeout_ms, 0, 0, 0])
}

pub fn futex_wake(word: &core::sync::atomic::AtomicU32, count: usize) -> isize {
    syscall(nr::FUTEX_WAKE, [word as *const _ as usize, count, 0, 0, 0, 0])
}

pub fn surface_create(width: usize, height: usize) -> isize {
    syscall(nr::SURFACE_CREATE, [width, height, 0, 0, 0, 0])
}

pub fn surface_present() -> isize {
    syscall(nr::SURFACE_PRESENT, [0; 6])
}

pub fn poll_event(event: &mut Event) -> isize {
    syscall(nr::POLL_EVENT, [event as *mut Event as usize, 0, 0, 0, 0, 0])
}

/// A negated error code as a word, for messages.
pub fn error_name(result: isize) -> &'static str {
    match abi::error_of(result) {
        Some(errno::ENOENT) => "ENOENT",
        Some(errno::EIO) => "EIO",
        Some(errno::ENOEXEC) => "ENOEXEC",
        Some(errno::EBADF) => "EBADF",
        Some(errno::ECHILD) => "ECHILD",
        Some(errno::EAGAIN) => "EAGAIN",
        Some(errno::ENOMEM) => "ENOMEM",
        Some(errno::EFAULT) => "EFAULT",
        Some(errno::EBUSY) => "EBUSY",
        Some(errno::EEXIST) => "EEXIST",
        Some(errno::EISDIR) => "EISDIR",
        Some(errno::EINVAL) => "EINVAL",
        Some(errno::EMFILE) => "EMFILE",
        Some(errno::ENOSPC) => "ENOSPC",
        Some(errno::EPIPE) => "EPIPE",
        Some(errno::ENAMETOOLONG) => "ENAMETOOLONG",
        Some(errno::ENOSYS) => "ENOSYS",
        Some(errno::ETIMEDOUT) => "ETIMEDOUT",
        Some(_) => "error",
        None => "ok",
    }
}

/// Writes to a file descriptor, for `write!`.
pub struct Stream(pub usize);

impl fmt::Write for Stream {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut bytes = text.as_bytes();
        while !bytes.is_empty() {
            let wrote = write(self.0, bytes);
            if wrote <= 0 {
                return Err(fmt::Error);
            }
            bytes = &bytes[wrote as usize..];
        }
        Ok(())
    }
}

/// Collects a line so it reaches the console in one write, not interleaved with
/// another process's.
pub struct Line {
    bytes: [u8; 256],
    length: usize,
    fd: usize,
}

impl Line {
    pub const fn new(fd: usize) -> Self {
        Self { bytes: [0; 256], length: 0, fd }
    }

    pub fn flush(&mut self) {
        let mut sent = 0;
        while sent < self.length {
            let wrote = write(self.fd, &self.bytes[sent..self.length]);
            if wrote <= 0 {
                break;
            }
            sent += wrote as usize;
        }
        self.length = 0;
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            if self.length == self.bytes.len() {
                self.flush();
            }
            self.bytes[self.length] = byte;
            self.length += 1;
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let mut line = $crate::Line::new(1);
        let _ = core::write!(line, $($arg)*);
        line.flush();
    }};
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let mut line = $crate::Line::new(1);
        let _ = core::writeln!(line, $($arg)*);
        line.flush();
    }};
}

#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let mut line = $crate::Line::new(2);
        let _ = core::writeln!(line, $($arg)*);
        line.flush();
    }};
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    eprintln!("panic: {info}");
    exit(134)
}

/// Parses a decimal argument.
pub fn parse_number(text: &str) -> Option<usize> {
    if text.is_empty() {
        return None;
    }
    let mut value = 0usize;
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as usize)?;
    }
    Some(value)
}
