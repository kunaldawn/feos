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

// The kernel starts a program with `argc` and its `UserStr`s in the first two argument
// registers, and the stack pointer at the System V initial stack: passed on as the third,
// for the environment and the aux vector.
#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    "_start:",
    "    mov x2, sp",
    "    bl feos_main",
    "    brk #0",
);

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    "_start:",
    "    mov rdx, rsp",
    "    call feos_main",
    "    ud2",
);

/// Where the stack pointer started: the `argc` word of the initial stack.
static START: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Notes where the initial stack is. The entry point's business.
#[doc(hidden)]
pub fn set_start(sp: usize) {
    START.store(sp, core::sync::atomic::Ordering::Relaxed);
}

/// The initial stack's word at `index`, from the stack pointer the program started with.
fn start_word(index: usize) -> usize {
    let sp = START.load(core::sync::atomic::Ordering::Relaxed);
    if sp == 0 {
        return 0;
    }
    // SAFETY: the kernel laid the initial stack out there, and nothing writes it.
    unsafe { core::ptr::read((sp as *const usize).add(index)) }
}

/// A NUL-terminated string the kernel left on the initial stack, without its NUL.
fn c_string(pointer: usize) -> &'static [u8] {
    if pointer == 0 {
        return &[];
    }
    let mut length = 0;
    // SAFETY: as the kernel promises: a string it copied, NUL-terminated.
    while unsafe { core::ptr::read((pointer as *const u8).add(length)) } != 0 {
        length += 1;
    }
    // SAFETY: as above, `length` bytes before the NUL.
    unsafe { core::slice::from_raw_parts(pointer as *const u8, length) }
}

/// The initial stack's `argv[index]`, as C sees it.
pub fn c_arg(index: usize) -> Option<&'static [u8]> {
    let argc = start_word(0);
    (index < argc).then(|| c_string(start_word(1 + index)))
}

/// Where the environment's pointers start on the initial stack, as a word index.
fn envp_index() -> usize {
    start_word(0) + 2
}

/// The program's environment, `KEY=VALUE` strings.
pub fn environment() -> impl Iterator<Item = &'static [u8]> {
    let first = envp_index();
    (first..).map(start_word).take_while(|&pointer| pointer != 0).map(c_string)
}

/// The value of the environment variable `name`.
pub fn getenv(name: &str) -> Option<&'static [u8]> {
    environment().find_map(|entry| {
        let rest = entry.strip_prefix(name.as_bytes())?;
        rest.strip_prefix(b"=")
    })
}

/// The aux vector entry of type `kind` (one of [`abi::aux`]).
pub fn auxv(kind: u64) -> Option<u64> {
    if START.load(core::sync::atomic::Ordering::Relaxed) == 0 {
        return None;
    }
    let mut index = envp_index();
    while start_word(index) != 0 {
        index += 1;
    }
    index += 1;
    loop {
        let (key, value) = (start_word(index) as u64, start_word(index + 1) as u64);
        if key == abi::aux::NULL {
            return None;
        }
        if key == kind {
            return Some(value);
        }
        index += 2;
    }
}

/// The NUL-terminated string an aux vector entry points at, such as `AT_FEOS_EXE`'s.
pub fn auxv_string(kind: u64) -> Option<&'static [u8]> {
    auxv(kind).map(|pointer| c_string(pointer as usize))
}

/// The stack pointer, as it is in the caller.
#[inline(always)]
pub fn stack_pointer() -> usize {
    let sp: usize;
    // SAFETY: reading the stack pointer has no effect.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags))
    };
    // SAFETY: as above.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags))
    };
    sp
}

/// Declares a program's `fn main(args: Args) -> i32`.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[no_mangle]
        extern "C" fn feos_main(argc: usize, argv: *const $crate::abi::UserStr, sp: usize) -> ! {
            $crate::set_start(sp);
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

    /// Where the kernel put the [`UserStr`]s, which is also where the stack pointer
    /// started.
    pub fn address(&self) -> usize {
        self.strings.as_ptr() as usize
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

/// `mmap2`, for anonymous memory: no file, so `fd` is `-1` and `offset` zero.
pub fn mmap2(hint: usize, length: usize, prot: usize, flags: usize) -> isize {
    syscall(nr::MMAP2, [hint, length, prot, flags | abi::map::ANONYMOUS, usize::MAX, 0])
}

/// `mmap2` of a file: `fd` open for reading, `offset` a whole number of pages.
pub fn mmap2_file(hint: usize, length: usize, prot: usize, flags: usize, fd: usize, offset: usize) -> isize {
    syscall(nr::MMAP2, [hint, length, prot, flags, fd, offset])
}

pub fn madvise(address: usize, length: usize, advice: usize) -> isize {
    syscall(nr::MADVISE, [address, length, advice, 0, 0, 0])
}

pub fn msync(address: usize, length: usize, flags: usize) -> isize {
    syscall(nr::MSYNC, [address, length, flags, 0, 0, 0])
}

/// Whether a kernel test is driving this program, and will answer [`free_frames`].
static DRIVEN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Says a kernel test is driving this program: `ktest drive` passes `--frames`.
pub fn set_driven(driven: bool) {
    DRIVEN.store(driven, core::sync::atomic::Ordering::Relaxed);
}

/// The machine's free frames right now, if a kernel test is driving this program: it asks
/// with a line `?frames` on standard output and reads the answer from standard input.
/// There's no call for it until `SYSINFO`.
pub fn free_frames() -> Option<usize> {
    if !DRIVEN.load(core::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    if write(1, b"?frames\n") != 8 {
        return None;
    }
    let mut line = [0u8; 24];
    let mut length = 0;
    while length < line.len() {
        let mut byte = [0u8; 1];
        if read(0, &mut byte) != 1 {
            return None;
        }
        if byte[0] == b'\n' {
            break;
        }
        line[length] = byte[0];
        length += 1;
    }
    parse_number(core::str::from_utf8(&line[..length]).ok()?)
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

/// `UserStr`s for `strings`, in `out`: the part used.
pub fn user_strs<'a>(strings: &[&str], out: &'a mut [UserStr]) -> &'a [UserStr] {
    let count = strings.len().min(out.len());
    for (slot, text) in out.iter_mut().zip(strings.iter()) {
        *slot = UserStr { pointer: text.as_ptr() as usize, length: text.len() };
    }
    &out[..count]
}

/// `spawn2`, with the request as given.
pub fn spawn2(request: &abi::SpawnRequest) -> isize {
    syscall(nr::SPAWN2, [request as *const abi::SpawnRequest as usize, 0, 0, 0, 0, 0])
}

/// A `SPAWN2` request for the program at `path` (relative to the working directory),
/// with `argv`, `envp` and `actions`; its flags and attributes are the caller's to add.
pub fn spawn_request(path: &str, argv: &[UserStr], envp: &[UserStr], actions: &[abi::SpawnAction]) -> abi::SpawnRequest {
    abi::SpawnRequest {
        path: path_at(abi::at::FDCWD, path),
        argv: argv.as_ptr() as u64,
        argc: argv.len() as u64,
        envp: envp.as_ptr() as u64,
        envc: envp.len() as u64,
        actions: actions.as_ptr() as u64,
        action_count: actions.len() as u64,
        ..abi::SpawnRequest::default()
    }
}

pub fn getppid() -> isize {
    syscall(nr::GETPPID, [0; 6])
}

pub fn getpgid(pid: usize) -> isize {
    syscall(nr::GETPGID, [pid, 0, 0, 0, 0, 0])
}

pub fn setpgid(pid: usize, pgid: usize) -> isize {
    syscall(nr::SETPGID, [pid, pgid, 0, 0, 0, 0])
}

pub fn getsid(pid: usize) -> isize {
    syscall(nr::GETSID, [pid, 0, 0, 0, 0, 0])
}

pub fn setsid() -> isize {
    syscall(nr::SETSID, [0; 6])
}

pub fn getrusage(who: i32) -> Result<abi::Rusage, isize> {
    let mut out = abi::Rusage::default();
    match syscall(nr::GETRUSAGE, [who as usize, &mut out as *mut abi::Rusage as usize, 0, 0, 0, 0]) {
        0 => Ok(out),
        error => Err(error),
    }
}

pub fn sysinfo() -> Result<abi::SysInfo, isize> {
    let mut out = [0u8; abi::SysInfo::SIZE];
    match syscall(nr::SYSINFO, [out.as_mut_ptr() as usize, out.len(), 0, 0, 0, 0]) {
        0 => Ok(abi::SysInfo::from_bytes(&out)),
        error => Err(error),
    }
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

/// `pipe2`: read end, then write end, with `flags` from [`abi::o`].
pub fn pipe2(flags: usize) -> Result<(usize, usize), isize> {
    let mut fds = [0i32; 2];
    match syscall(nr::PIPE2, [fds.as_mut_ptr() as usize, flags, 0, 0, 0, 0]) {
        0 => Ok((fds[0] as usize, fds[1] as usize)),
        error => Err(error),
    }
}

/// `dup3`: `flags` zero or [`abi::o::CLOEXEC`].
pub fn dup3(old: usize, new: usize, flags: usize) -> isize {
    syscall(nr::DUP3, [old, new, flags, 0, 0, 0])
}

/// `fcntl`, commands from [`abi::f`].
pub fn fcntl(fd: usize, command: usize, arg: usize) -> isize {
    syscall(nr::FCNTL, [fd, command, arg, 0, 0, 0])
}

/// `fcntl`'s record lock commands: `F_GETLK`, `F_SETLK` and `F_SETLKW` with `lock`,
/// which `F_GETLK` writes its answer over.
pub fn fcntl_lock(fd: usize, command: usize, lock: &mut abi::Flock) -> isize {
    fcntl(fd, command, lock as *mut abi::Flock as usize)
}

/// A record lock of `kind` (`F_RDLCK`, `F_WRLCK`, `F_UNLCK`) on `length` bytes from
/// `start`, from the start of the file, for [`fcntl_lock`].
pub fn record(kind: i16, start: i64, length: i64) -> abi::Flock {
    abi::Flock { kind, whence: abi::seek::SET as i16, start, length, ..abi::Flock::default() }
}

/// `flock(fd, operation)`, `operation` from [`abi::lock`].
pub fn flock(fd: usize, operation: usize) -> isize {
    syscall(nr::FLOCK, [fd, operation, 0, 0, 0, 0])
}

/// `dup`, as relibc makes it: the lowest free descriptor.
pub fn dup(fd: usize) -> isize {
    fcntl(fd, abi::f::DUPFD, 0)
}

/// `close_range`: `flags` zero or [`abi::close_range::CLOEXEC`].
pub fn close_range(first: usize, last: usize, flags: usize) -> isize {
    syscall(nr::CLOSE_RANGE, [first, last, flags, 0, 0, 0])
}

pub fn futex_wait(word: &core::sync::atomic::AtomicU32, expected: u32, timeout_ms: usize) -> isize {
    syscall(nr::FUTEX_WAIT, [word as *const _ as usize, expected as usize, timeout_ms, 0, 0, 0])
}

pub fn futex_wake(word: &core::sync::atomic::AtomicU32, count: usize) -> isize {
    syscall(nr::FUTEX_WAKE, [word as *const _ as usize, count, 0, 0, 0, 0])
}

/// `futex_wait2`: waits while `word` holds `expected`, until the absolute time `deadline`
/// on `clock` if there is one.
pub fn futex_wait2(word: &core::sync::atomic::AtomicU32, expected: u32, deadline: Option<&abi::Timespec>, clock: usize) -> isize {
    let deadline = deadline.map_or(0, |time| time as *const abi::Timespec as usize);
    syscall(nr::FUTEX_WAIT2, [word as *const _ as usize, expected as usize, deadline, clock, 0, 0])
}

pub fn clock_gettime(clock: usize) -> Result<abi::Timespec, isize> {
    let mut out = abi::Timespec::default();
    match syscall(nr::CLOCK_GETTIME, [clock, &mut out as *mut abi::Timespec as usize, 0, 0, 0, 0]) {
        0 => Ok(out),
        error => Err(error),
    }
}

pub fn clock_getres(clock: usize) -> Result<abi::Timespec, isize> {
    let mut out = abi::Timespec::default();
    match syscall(nr::CLOCK_GETRES, [clock, &mut out as *mut abi::Timespec as usize, 0, 0, 0, 0]) {
        0 => Ok(out),
        error => Err(error),
    }
}

/// A clock's reading in nanoseconds, or zero if it can't be read.
pub fn now_ns(clock: usize) -> u64 {
    clock_gettime(clock).ok().and_then(|time| time.to_nanos()).unwrap_or(0)
}

/// `clock_nanosleep`: `flags` zero or [`abi::timer::ABSTIME`].
pub fn nanosleep(clock: usize, flags: usize, request: &abi::Timespec, remain: Option<&mut abi::Timespec>) -> isize {
    let remain = remain.map_or(0, |time| time as *mut abi::Timespec as usize);
    syscall(nr::NANOSLEEP, [clock, flags, request as *const abi::Timespec as usize, remain, 0, 0])
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

pub fn gettid() -> isize {
    syscall(nr::GETTID, [0; 6])
}

/// Sets this thread's thread pointer: `TPIDR_EL0`, or the FS base.
pub fn set_tls(value: usize) -> isize {
    syscall(nr::SET_TLS, [value, 0, 0, 0, 0, 0])
}

/// This thread's thread pointer, as the hardware has it. On x86_64 the FS base can't be
/// read at ring 3 (`CR4.FSGSBASE` is off), so it's the word at `fs:0`: by the usual
/// convention, a thread control block's first word is its own address, which this is
/// then.
#[inline(always)]
pub fn tls() -> usize {
    let value: usize;
    // SAFETY: reading the thread pointer has no effect.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) value, options(nomem, nostack, preserves_flags))
    };
    // SAFETY: the caller has set the FS base to a block whose first word it can read.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("mov {}, fs:[0]", out(reg) value, options(nostack, readonly, preserves_flags))
    };
    value
}

/// `thread_create`, raw: see [`abi::nr::THREAD_CREATE`].
pub fn thread_create(entry: usize, stack_top: usize, tls: usize, arg: usize, clear_word: usize) -> isize {
    syscall(nr::THREAD_CREATE, [entry, stack_top, tls, arg, clear_word, 0])
}

/// `thread_exit`, raw: only comes back if the range is refused.
pub fn thread_exit_raw(unmap_address: usize, unmap_length: usize) -> isize {
    syscall(nr::THREAD_EXIT, [unmap_address, unmap_length, 0, 0, 0, 0])
}

/// Ends this thread, unmapping the range after its clear word is written.
pub fn thread_exit(unmap_address: usize, unmap_length: usize) -> ! {
    thread_exit_raw(unmap_address, unmap_length);
    exit(134)
}

/// Threads with a stack of their own and a join: what a program here needs of
/// `pthread_create` and `pthread_join`, with no heap.
pub mod thread {
    use core::sync::atomic::{AtomicU32, Ordering};

    use super::abi::{map, prot, PAGE_SIZE};

    /// Bytes of a thread's stack, its start block at the top.
    pub const STACK: usize = 64 * 1024;

    /// What a thread starts from, at the top of its stack: what to run, and where its
    /// result and its end go. `done` is its clear word: one until the kernel writes zero.
    #[repr(C, align(16))]
    pub struct Start {
        pub done: AtomicU32,
        run: fn(usize) -> usize,
        arg: usize,
        result: usize,
    }

    #[cfg(target_arch = "aarch64")]
    core::arch::global_asm!(
        ".section .text.feos_thread_start, \"ax\"",
        ".global feos_thread_start",
        "feos_thread_start:",
        "    bl feos_thread_main",
        "    brk #0",
    );

    // The stack pointer is 16-byte aligned here, so a call leaves it as a function
    // expects to find it.
    #[cfg(target_arch = "x86_64")]
    core::arch::global_asm!(
        ".section .text.feos_thread_start, \"ax\"",
        ".global feos_thread_start",
        "feos_thread_start:",
        "    call feos_thread_main",
        "    ud2",
    );

    extern "C" {
        fn feos_thread_start();
    }

    /// The address a new thread starts at, for programs that make threads by hand.
    pub fn entry() -> usize {
        feos_thread_start as *const () as usize
    }

    #[no_mangle]
    extern "C" fn feos_thread_main(start: *mut Start) -> ! {
        // SAFETY: `spawn` made the block, and it lives until the join after this ends.
        let start = unsafe { &mut *start };
        start.result = (start.run)(start.arg);
        super::thread_exit(0, 0)
    }

    /// A thread that has been started, to be joined.
    pub struct Thread {
        pub tid: usize,
        stack: usize,
        start: *mut Start,
    }

    /// Starts `run(arg)` in a new thread with a stack of its own, and the thread pointer
    /// `tls`.
    pub fn spawn_tls(run: fn(usize) -> usize, arg: usize, tls: usize) -> Result<Thread, isize> {
        let stack = super::mmap2(0, STACK, prot::READ | prot::WRITE, map::PRIVATE);
        if stack < 0 {
            return Err(stack);
        }
        let stack = stack as usize;
        let start = (stack + STACK - core::mem::size_of::<Start>()) as *mut Start;
        // SAFETY: the top of the stack just mapped, aligned, and nothing else has it.
        unsafe { start.write(Start { done: AtomicU32::new(1), run, arg, result: 0 }) };
        // SAFETY: as above.
        let done = unsafe { &(*start).done } as *const AtomicU32 as usize;
        let tid = super::thread_create(entry(), start as usize, tls, start as usize, done);
        if tid < 0 {
            super::munmap(stack, STACK);
            return Err(tid);
        }
        Ok(Thread { tid: tid as usize, stack, start })
    }

    /// Starts `run(arg)` in a new thread, with no thread pointer.
    pub fn spawn(run: fn(usize) -> usize, arg: usize) -> Result<Thread, isize> {
        spawn_tls(run, arg, 0)
    }

    impl Thread {
        /// Whether the thread has ended.
        pub fn finished(&self) -> bool {
            // SAFETY: the block lives until the join.
            unsafe { (*self.start).done.load(Ordering::Acquire) == 0 }
        }

        /// Waits for the thread to end, on its clear word, and gives back its stack.
        /// Returns what it returned.
        pub fn join(self) -> usize {
            // SAFETY: the block lives until the stack goes, below.
            let start = unsafe { &*self.start };
            loop {
                let done = start.done.load(Ordering::Acquire);
                if done == 0 {
                    break;
                }
                super::futex_wait(&start.done, done, 0);
            }
            let result = start.result;
            super::munmap(self.stack, STACK);
            result
        }
    }

    /// Starts a thread as [`spawn_tls`] does, and says where its stack is: for a thread
    /// that ends by unmapping it, which [`Thread::join`] mustn't be used for.
    pub fn spawn_with_stack(run: fn(usize) -> usize, arg: usize) -> Result<(Thread, usize, usize), isize> {
        let thread = spawn_tls(run, arg, 0)?;
        let (stack, done) = (thread.stack, thread.start as usize);
        Ok((thread, stack, done))
    }

    /// Bytes of stack below a thread's start block: what [`spawn_with_stack`]'s stack is.
    pub const fn stack_pages() -> usize {
        STACK / PAGE_SIZE
    }
}

/// `openat(dirfd, path, flags, mode)`: `dirfd` is a descriptor or
/// [`abi::at::FDCWD`] (passed as `i32`, sign extended, as a C caller's would be).
pub fn openat(dirfd: i32, path: &str, flags: usize, mode: usize) -> isize {
    syscall(nr::OPENAT, [dirfd as isize as usize, path.as_ptr() as usize, path.len(), flags, mode, 0])
}

/// `fstatat(dirfd, path, flags)`.
pub fn fstatat(dirfd: i32, path: &str, flags: usize) -> Result<abi::Stat2, isize> {
    let mut out = abi::Stat2::default();
    let out_at = &mut out as *mut abi::Stat2 as usize;
    match syscall(nr::FSTATAT, [dirfd as isize as usize, path.as_ptr() as usize, path.len(), out_at, flags, 0]) {
        0 => Ok(out),
        error => Err(error),
    }
}

/// `fstat(fd)`: `fstatat(fd, "", AT_EMPTY_PATH)`.
pub fn fstat(fd: usize) -> Result<abi::Stat2, isize> {
    fstatat(fd as i32, "", abi::at::EMPTY_PATH)
}

pub fn getdents(fd: usize, out: &mut [u8]) -> isize {
    syscall(nr::GETDENTS, [fd, out.as_mut_ptr() as usize, out.len(), 0, 0, 0])
}

pub fn mkdirat(dirfd: i32, path: &str, mode: usize) -> isize {
    syscall(nr::MKDIRAT, [dirfd as isize as usize, path.as_ptr() as usize, path.len(), mode, 0, 0])
}

pub fn unlinkat(dirfd: i32, path: &str, flags: usize) -> isize {
    syscall(nr::UNLINKAT, [dirfd as isize as usize, path.as_ptr() as usize, path.len(), flags, 0, 0])
}

pub fn ftruncate(fd: usize, length: usize) -> isize {
    syscall(nr::FTRUNCATE, [fd, length, 0, 0, 0, 0])
}

pub fn fsync(fd: usize, flags: usize) -> isize {
    syscall(nr::FSYNC, [fd, flags, 0, 0, 0, 0])
}

pub fn pread(fd: usize, out: &mut [u8], offset: usize) -> isize {
    syscall(nr::PREAD, [fd, out.as_mut_ptr() as usize, out.len(), offset, 0, 0])
}

pub fn pwrite(fd: usize, bytes: &[u8], offset: usize) -> isize {
    syscall(nr::PWRITE, [fd, bytes.as_ptr() as usize, bytes.len(), offset, 0, 0])
}

pub fn fchdir(fd: usize) -> isize {
    syscall(nr::FCHDIR, [fd, 0, 0, 0, 0, 0])
}

/// `getcwd` into `out`: the length, the path with no NUL.
pub fn getcwd(out: &mut [u8]) -> isize {
    syscall(nr::GETCWD, [out.as_mut_ptr() as usize, out.len(), 0, 0, 0, 0])
}

pub fn umask(mask: usize) -> isize {
    syscall(nr::UMASK, [mask, 0, 0, 0, 0, 0])
}

pub fn fd_path(fd: usize, out: &mut [u8]) -> isize {
    syscall(nr::FD_PATH, [fd, out.as_mut_ptr() as usize, out.len(), 0, 0, 0])
}

pub fn current_exe(out: &mut [u8]) -> isize {
    syscall(nr::CURRENT_EXE, [out.as_mut_ptr() as usize, out.len(), 0, 0, 0, 0])
}

/// A [`abi::PathAt`] for `path` relative to `dirfd`.
pub fn path_at(dirfd: i32, path: &str) -> abi::PathAt {
    abi::PathAt { dirfd, reserved: 0, pointer: path.as_ptr() as u64, length: path.len() as u64 }
}

/// `renameat(from relative to from_dirfd, to relative to to_dirfd, flags)`.
pub fn renameat(from_dirfd: i32, from: &str, to_dirfd: i32, to: &str, flags: usize) -> isize {
    let (from, to) = (path_at(from_dirfd, from), path_at(to_dirfd, to));
    syscall(nr::RENAMEAT, [&from as *const abi::PathAt as usize, &to as *const abi::PathAt as usize, flags, 0, 0, 0])
}

/// `linkat(existing relative to existing_dirfd, new relative to new_dirfd, flags)`.
pub fn linkat(existing_dirfd: i32, existing: &str, new_dirfd: i32, new: &str, flags: usize) -> isize {
    let (existing, new) = (path_at(existing_dirfd, existing), path_at(new_dirfd, new));
    syscall(nr::LINKAT, [&existing as *const abi::PathAt as usize, &new as *const abi::PathAt as usize, flags, 0, 0, 0])
}

/// `symlinkat(target, dirfd, path)`: a link at `path` leading to `target`.
pub fn symlinkat(target: &str, dirfd: i32, path: &str) -> isize {
    syscall(nr::SYMLINKAT, [target.as_ptr() as usize, target.len(), dirfd as isize as usize, path.as_ptr() as usize, path.len(), 0])
}

/// `readlinkat(dirfd, path)` into `out`: how many bytes of the target it holds.
pub fn readlinkat(dirfd: i32, path: &str, out: &mut [u8]) -> isize {
    syscall(nr::READLINKAT, [dirfd as isize as usize, path.as_ptr() as usize, path.len(), out.as_mut_ptr() as usize, out.len(), 0])
}

pub fn fchmodat(dirfd: i32, path: &str, mode: usize, flags: usize) -> isize {
    syscall(nr::FCHMODAT, [dirfd as isize as usize, path.as_ptr() as usize, path.len(), mode, flags, 0])
}

/// `fchownat`: `uid` and `gid` as the call takes them, [`abi::KEEP`] (or `u32::MAX`) for
/// "leave it".
pub fn fchownat(dirfd: i32, path: &str, uid: usize, gid: usize, flags: usize) -> isize {
    syscall(nr::FCHOWNAT, [dirfd as isize as usize, path.as_ptr() as usize, path.len(), uid, gid, flags])
}

/// `utimensat`: access then modification time, or `None` for both now.
pub fn utimensat(dirfd: i32, path: &str, times: Option<&[abi::Timespec; 2]>, flags: usize) -> isize {
    let times = times.map_or(0, |times| times.as_ptr() as usize);
    syscall(nr::UTIMENSAT, [dirfd as isize as usize, path.as_ptr() as usize, path.len(), times, flags, 0])
}

pub fn faccessat(dirfd: i32, path: &str, mode: usize, flags: usize) -> isize {
    syscall(nr::FACCESSAT, [dirfd as isize as usize, path.as_ptr() as usize, path.len(), mode, flags, 0])
}

pub fn fstatvfs(fd: usize) -> Result<abi::StatVfs, isize> {
    let mut out = abi::StatVfs::default();
    match syscall(nr::FSTATVFS, [fd, &mut out as *mut abi::StatVfs as usize, 0, 0, 0, 0]) {
        0 => Ok(out),
        error => Err(error),
    }
}

pub fn sync() -> isize {
    syscall(nr::SYNC, [0; 6])
}

/// A negated error code as a word, for messages.
pub fn error_name(result: isize) -> &'static str {
    match abi::error_of(result) {
        Some(errno::EPERM) => "EPERM",
        Some(errno::ENOENT) => "ENOENT",
        Some(errno::EACCES) => "EACCES",
        Some(errno::ENODEV) => "ENODEV",
        Some(errno::ETXTBSY) => "ETXTBSY",
        Some(errno::EOVERFLOW) => "EOVERFLOW",
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
        Some(errno::ENFILE) => "ENFILE",
        Some(errno::ENOSPC) => "ENOSPC",
        Some(errno::EPIPE) => "EPIPE",
        Some(errno::ENAMETOOLONG) => "ENAMETOOLONG",
        Some(errno::ENOSYS) => "ENOSYS",
        Some(errno::ETIMEDOUT) => "ETIMEDOUT",
        Some(errno::EINTR) => "EINTR",
        Some(errno::EOPNOTSUPP) => "EOPNOTSUPP",
        Some(errno::ESPIPE) => "ESPIPE",
        Some(errno::EFBIG) => "EFBIG",
        Some(errno::ENOTDIR) => "ENOTDIR",
        Some(errno::ENOTEMPTY) => "ENOTEMPTY",
        Some(errno::ELOOP) => "ELOOP",
        Some(errno::EILSEQ) => "EILSEQ",
        Some(errno::EROFS) => "EROFS",
        Some(errno::ERANGE) => "ERANGE",
        Some(errno::EXDEV) => "EXDEV",
        Some(errno::ENOLCK) => "ENOLCK",
        Some(errno::ESRCH) => "ESRCH",
        Some(errno::E2BIG) => "E2BIG",
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
