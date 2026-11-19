//! User programs: loading one, the system calls it makes, and what a process owns.
//!
//! A program's task is an ordinary scheduler task that has a page table and a second
//! register frame. Its life is: `sched::app_entry` calls [`start`] on the task's kernel
//! stack, which builds the address space from the ELF file and goes to EL0 or ring 3
//! for good. Each system call comes back in on the task's kernel stack, in
//! [`syscall`], with the program's registers safe in its user frame. So everything
//! here is ordinary kernel code that can block and be preempted, and the arch crates
//! need to know nothing about what a call means.
//!
//! # The boundary
//!
//! A process can have many threads, each a task of its own sharing the process: see
//! [`sys_thread_create`] and `sched`'s threads.
//!
//! Memory: the program's page tables give it its own pages and nothing else; the
//! kernel's mappings in them are for the supervisor only. Pointers: every address and
//! length in a call goes through `feos-vm`, which walks the tables and never touches
//! the address itself, and a copy pins the frames it uses, so another thread's `munmap`
//! can't free one under it (see [`process`]). Privilege: EL0 and ring 3 trap anything
//! else. Failure: a fault ends the process with status 139, which costs nobody else
//! anything, and its frames go back whichever way it ended.
//!
//! What this doesn't do is isolate kernel-linked apps from anything, or the filesystem
//! from programs: the tree is shared and there are no users or permissions.

use core::mem;

use alloc::string::String;
use alloc::vec::Vec;

use feos_abi::{errno, nr, Event};
use feos_fs::vfs::{path, Caller, VfsTree, ACCESS_EXECUTE, ACCESS_WRITE};
use feos_fs::{FileKind, FsError};
use feos_hal::Clock;
use feos_vm::{FrameSource, PAGE_SIZE};

use crate::sched;
use crate::{surface, Frame};

mod children;
mod devfs;
mod files;
mod fs_calls;
mod load;
mod lock_calls;
mod memory_calls;
mod proc;
pub mod process;
pub mod signal;
mod time_calls;
pub mod tlb;
mod tree_calls;

use memory_calls::{KernelFrames, PROMISES};
pub use memory_calls::{forget_fault, promised, resolve_fault, segv_code, FaultOutcome, SPARE_FRAMES};
pub use fs_calls::orphans;
pub use lock_calls::count as locks;
pub use children::{drain as drain_records, held as pid_held, release as release_record, ChildRecord};
pub use process::{take as take_process, Process, TestSpace};

/// Makes the zero frame and takes the port's ASIDs. Before any program runs.
pub fn init() {
    memory_calls::init();
    tlb::init();
    devfs::init();
}

/// The process of the task in `slot`, which is the caller.
fn process(slot: usize) -> Option<&'static Process> {
    process::current(slot)
}

/// Holds off a kill: a frame just allocated but not yet in a table is in no record that
/// a kill could reclaim it from.
use crate::sched::NoKill;

pub use load::UserStart;

/// The umask of a program a kernel app starts (section 3.3 of the ABI design).
pub const DEFAULT_UMASK: u32 = 0o022;

pub(super) fn tree() -> Option<&'static dyn VfsTree> {
    sched::platform().filesystem.and_then(|filesystem| filesystem.tree())
}

pub(super) fn say(message: core::fmt::Arguments<'_>) {
    feos_console::eprintln!("{}", message);
}

/// Loads the program `start` names and runs it. Only returns if it can't, with the
/// status the process should end with.
pub fn start(slot: usize, start: alloc::boxed::Box<UserStart>) -> i32 {
    load::start(slot, start)
}

/// What a filesystem error is called across the boundary.
pub(super) fn fs_errno(error: FsError) -> isize {
    -match error {
        FsError::NotFound => errno::ENOENT,
        FsError::NameExists => errno::EEXIST,
        FsError::NoSpace => errno::ENOSPC,
        FsError::OutOfMemory => errno::ENOMEM,
        FsError::IsDirectory => errno::EISDIR,
        FsError::NotDirectory | FsError::InvalidName => errno::EINVAL,
        FsError::PermissionDenied => errno::EACCES,
        FsError::Unsupported => errno::ENOTSUP,
        FsError::NotEmpty | FsError::Busy => errno::EBUSY,
        FsError::TextBusy => errno::ETXTBSY,
        FsError::NotPermitted => errno::EPERM,
        _ => errno::EIO,
    }
}

/// Copies a path from the program and makes it absolute, on the heap: it may be
/// `PATH_MAX` long, too much for a kernel stack. `PATH_MAX` is the limit on what the
/// program gives, as section 1 of the design says; a relative path joined to the working
/// directory may come to more, which the VFS takes (it's `PATH_MAX` of each, and the
/// slash between).
fn user_path(process: &Process, pointer: usize, length: usize) -> Result<String, isize> {
    if length == 0 {
        return Err(-errno::ENOENT);
    }
    if !process.mm().in_window(pointer, 1) {
        return Err(-errno::EFAULT);
    }
    if length > feos_abi::PATH_MAX {
        return Err(-errno::ENAMETOOLONG);
    }
    let mut given = Vec::new();
    given.try_reserve_exact(length).map_err(|_| -errno::ENOMEM)?;
    given.resize(length, 0);
    process.mm().copy_in(pointer, &mut given)?;
    let given = String::from_utf8(given).map_err(|_| -errno::EINVAL)?;
    if path::has_long_name(&given) {
        return Err(-errno::ENAMETOOLONG);
    }
    // No program reaches the orphans by any call, v1's included, nor makes anything by
    // their name (section 3.3 of the design).
    if path::is_hidden(&given) {
        return Err(-errno::ENOENT);
    }
    if given.starts_with('/') {
        return Ok(given);
    }
    let files = process.files();
    let cwd = files.cwd();
    let total = cwd.len() + 1 + length;
    let mut out = String::new();
    out.try_reserve_exact(total).map_err(|_| -errno::ENOMEM)?;
    out.push_str(cwd);
    out.push('/');
    out.push_str(&given);
    Ok(out)
}

/// Calls `step` with each page's worth of a user buffer, already validated, until the
/// buffer is done, or `step` moves less than it was given or fails. A result of zero
/// bytes with an error returns the error; otherwise, the count moved so far.
///
/// Every page goes through a bounce buffer, and nothing is pinned while `step` runs,
/// which may wait for as long as a pipe or the console takes: a page the program gives
/// is pinned and copied in before it, a page it's given is pinned and copied out after
/// it. Another thread may unmap a page meanwhile; then the call ends there, `EFAULT` if
/// nothing had moved (and what `step` read for it is lost, as on Linux).
fn with_user_buffer(
    process: &Process,
    address: usize,
    length: usize,
    write: bool,
    mut step: impl FnMut(&mut [u8]) -> Result<usize, isize>,
) -> isize {
    if length == 0 {
        return 0;
    }
    let user = process.mm();
    // Checked at its whole length first: a length that runs off the end is a wild pointer
    // even though a call only moves a part of it at a time.
    if !user.in_window(address, length) {
        return -errno::EFAULT;
    }
    let length = length.min(feos_abi::IO_CHUNK);
    // All of it readied now, so a buffer the program couldn't use fails before anything
    // moves.
    if let Err(error) = user.fault_in(address, length, write) {
        return error;
    }
    // Both ways go through a copy: the program's page may be the page cache's own frame
    // (a mapping of a file). What's read from it may be the file being written, which the
    // write updates as it goes, and the source of a copy mustn't be where it's copied to.
    // What's written to it may be from the file whose page it is: a read through the
    // filesystem would put the file's bytes over what a shared mapping dirtied there and
    // hasn't written back, before the read could lay the dirty page over them.
    let mut bounce = [0u8; PAGE_SIZE];
    let mut done = 0;
    let stopped = |done: usize| if done == 0 { -errno::EFAULT } else { done as isize };
    while done < length {
        let here = address + done;
        let count = (PAGE_SIZE - here % PAGE_SIZE).min(length - done);
        if !write && user.read_page(here, &mut bounce[..count]).is_err() {
            return stopped(done);
        }
        match step(&mut bounce[..count]) {
            Ok(moved) => {
                let moved = moved.min(count);
                if write && moved > 0 && user.write_page(here, &bounce[..moved]).is_err() {
                    return stopped(done);
                }
                done += moved;
                if moved < count {
                    break;
                }
            }
            Err(error) if done == 0 => return error,
            Err(_) => break,
        }
    }
    done as isize
}

/// What a call that gave up waiting because its task is being killed returns. Nobody
/// sees it: `sched::syscall_entry` ends the task instead.
const KILLED: isize = -errno::EPIPE;

/// `mmap2`, with what its fd is: the path is shared out of the description, so the
/// table isn't held while the space is.
fn sys_mmap2(process: &Process, args: [usize; 6]) -> isize {
    memory_calls::sys_mmap2(process.mm(), args, |fd| files::map_source(process, fd))
}

/// The directory `path` is in, as given.
fn parent_of(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/",
        Some(at) => &trimmed[..at],
    }
}

/// May this user add to or take from the directory `path` is in?
fn check_parent(tree: &dyn VfsTree, path: &str, uid: u32, gid: u32) -> Result<(), isize> {
    tree.check_access(parent_of(path), uid, gid, ACCESS_WRITE | ACCESS_EXECUTE).map_err(fs_errno)
}

/// May this user reach the link at the canonical path `link`, not followed: search
/// permission on the way to the directory it's in, and nothing of where it leads, which
/// may not be there at all or be somewhere this user can't search.
fn check_reach_link(tree: &dyn VfsTree, link: &str, uid: u32, gid: u32) -> Result<(), FsError> {
    tree.check_access(parent_of(link), uid, gid, ACCESS_EXECUTE)
}

/// `ENOENT` if a v1 call's `path` reaches the orphans through a link: in the directory
/// it's in, or, for a call that follows a last link, in where it leads. [`user_path`]
/// has already refused one that names them.
fn check_visible(tree: &dyn VfsTree, path: &str, follow_last: bool) -> Result<(), isize> {
    let reaches = |path: &str| tree.resolve_path(path, true).is_ok_and(|found| path::is_hidden(&found.canonical));
    if reaches(parent_of(path)) || (follow_last && reaches(path)) {
        return Err(-errno::ENOENT);
    }
    Ok(())
}

fn sys_setuid(slot: usize, uid: usize, gid: usize) -> isize {
    // Only down, and only from root: there's nothing to raise a process's rights again.
    if sched::credentials(slot).0 != 0 {
        return -errno::EPERM;
    }
    let (Ok(uid), Ok(gid)) = (u32::try_from(uid), u32::try_from(gid)) else {
        return -errno::EINVAL;
    };
    sched::set_credentials(slot, uid, gid);
    0
}

fn sys_stat(slot: usize, process: &Process, pointer: usize, length: usize, out: usize) -> isize {
    let path = match user_path(process, pointer, length) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let path = path.as_str();
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    if let Err(error) = check_visible(tree, path, true) {
        return error;
    }
    let (uid, gid) = sched::credentials(slot);
    // Looking takes getting there, and nothing of the file itself.
    if let Err(error) = tree.check_access(path, uid, gid, 0) {
        return fs_errno(error);
    }
    let stat = match tree.stat(path) {
        Ok(stat) => stat,
        Err(error) => return fs_errno(error),
    };
    let result = feos_abi::Stat {
        mode: u32::from(stat.mode),
        uid: stat.uid,
        gid: stat.gid,
        nlink: stat.nlink,
        size: stat.size,
        ino: stat.ino,
        posix: u32::from(stat.posix),
        reserved: 0,
    };
    // SAFETY: a plain `repr(C)` struct of integers with no padding, read as bytes.
    let bytes = unsafe {
        core::slice::from_raw_parts(&result as *const feos_abi::Stat as *const u8, mem::size_of::<feos_abi::Stat>())
    };
    match process.mm().copy_out(out, bytes) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

fn sys_chmod(slot: usize, process: &Process, pointer: usize, length: usize, mode: usize) -> isize {
    let path = match user_path(process, pointer, length) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let path = path.as_str();
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    if let Err(error) = check_visible(tree, path, true) {
        return error;
    }
    let (uid, gid) = sched::credentials(slot);
    if let Err(error) = tree.check_access(path, uid, gid, 0) {
        return fs_errno(error);
    }
    let stat = match tree.stat(path) {
        Ok(stat) => stat,
        Err(error) => return fs_errno(error),
    };
    // Where nothing has an owner, there's no one to say no: only the filesystem to say it can't.
    if stat.posix && uid != 0 && stat.uid != uid {
        return -errno::EPERM;
    }
    match tree.chmod(path, mode as u16) {
        Ok(()) => 0,
        Err(error) => fs_errno(error),
    }
}

fn sys_chown(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    let [pointer, length, owner, group, ..] = args;
    let path = match user_path(process, pointer, length) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let path = path.as_str();
    if sched::credentials(slot).0 != 0 {
        return -errno::EPERM;
    }
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    if let Err(error) = check_visible(tree, path, true) {
        return error;
    }
    let id = |value: usize| (value != feos_abi::KEEP).then_some(value as u32);
    match tree.chown(path, id(owner), id(group)) {
        Ok(()) => 0,
        Err(error) => fs_errno(error),
    }
}

fn sys_make_or_remove(slot: usize, process: &Process, pointer: usize, length: usize, make: bool) -> isize {
    let path = match user_path(process, pointer, length) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let path = path.as_str();
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    if let Err(error) = check_visible(tree, path, false) {
        return error;
    }
    let (uid, gid) = sched::credentials(slot);
    // Taking a name away from a sticky directory (`/tmp`) is for its owner too.
    let allowed = if make { check_parent(tree, path, uid, gid) } else { tree.check_remove(path, uid, gid).map_err(fs_errno) };
    if let Err(error) = allowed {
        return error;
    }
    let result = if make {
        // Theirs as it's made, judged on what the walk finds (see `create_dir_as`).
        tree.create_dir_as(path, None, Caller { uid, gid })
    } else {
        // A file still in use keeps working, as `UNLINKAT` leaves it.
        match tree.resolve_path(path, false) {
            Ok(found) if found.stat.kind == FileKind::Directory => tree.remove_dir(path),
            Ok(found) => fs_calls::remove_entry(tree, &found),
            Err(error) => Err(error),
        }
    };
    match result {
        Ok(()) => 0,
        Err(error) => fs_errno(error),
    }
}

fn sys_link(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    let [from_pointer, from_length, to_pointer, to_length, ..] = args;
    let from = match user_path(process, from_pointer, from_length) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let to = match user_path(process, to_pointer, to_length) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let (from, to) = (from.as_str(), to.as_str());
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    if let Err(error) = check_visible(tree, from, true).and_then(|()| check_visible(tree, to, false)) {
        return error;
    }
    let (uid, gid) = sched::credentials(slot);
    if let Err(error) = check_parent(tree, to, uid, gid) {
        return error;
    }
    if let Err(error) = tree.check_access(from, uid, gid, 0) {
        return fs_errno(error);
    }
    match tree.link(from, to) {
        Ok(()) => 0,
        Err(error) => fs_errno(error),
    }
}

/// The calling task's pid, before it has a process to say so.
pub(super) fn current_pid() -> u32 {
    use feos_app::Processes;
    sched::PROCESSES.current()
}

/// v1's `futex_wait`: a timeout in milliseconds, zero for none, and `expected` cut to
/// its low 32 bits as v1 always did.
fn sys_futex_wait(user: &process::UserMm, address: usize, expected: usize, timeout_ms: usize) -> isize {
    let deadline = (timeout_ms != 0).then(|| sched::deadline_in_ms(timeout_ms as u64));
    time_calls::futex_wait_until(user, address, expected as u32, deadline)
}

fn sys_surface_create(process: &Process, width: usize, height: usize) -> isize {
    let user = process.mm();
    let mut state = user.lock();
    if state.surface.is_some() {
        return -errno::EEXIST;
    }
    if width == 0 || height == 0 || width > feos_abi::SURFACE_MAX_SIDE || height > feos_abi::SURFACE_MAX_SIDE {
        return -errno::EINVAL;
    }
    let bytes = width * height * 4;
    let Some(length) = bytes.checked_next_multiple_of(PAGE_SIZE) else {
        return -errno::EINVAL;
    };
    // The surface's frames are the program's memory like any other: within its limit
    // (which `map_lent` counts them against, and checks again), and promised before
    // they're taken, so they can't take frames other programs were promised.
    let pages = length / PAGE_SIZE;
    if !state.mm.fits(pages) {
        return -errno::ENOMEM;
    }
    let (index, first_frame) = {
        let _no_kill = NoKill::new();
        if !PROMISES.promise(pages, KernelFrames.free_count()) {
            return -errno::ENOMEM;
        }
        let made = surface::create(process.pid(), width, height);
        PROMISES.release(pages);
        match made {
            Some(surface) => surface,
            None => return -errno::ENOMEM,
        }
    };
    let address = match user.change(&mut state, |mm, ctx| mm.map_lent(first_frame, length, ctx)) {
        Ok(address) => address,
        Err(_) => {
            surface::release(index);
            return -errno::ENOMEM;
        }
    };
    state.surface = Some((index, address, length));
    address as isize
}

/// The window surface's index, if the process has one.
fn surface_of(process: &Process) -> Option<usize> {
    process.mm().lock().surface.map(|(index, _, _)| index)
}

fn sys_poll_event(process: &Process, out: usize) -> isize {
    let size = mem::size_of::<Event>();
    // Readied first, so a bad pointer fails before an event is taken.
    if !process.mm().in_window(out, size) || process.mm().fault_in(out, size, true).is_err() {
        return -errno::EFAULT;
    }
    let Some(index) = surface_of(process) else {
        return -errno::EINVAL;
    };
    let Some(event) = surface::pop_event(index) else {
        return 0;
    };
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&event.kind.to_ne_bytes());
    bytes[4..8].copy_from_slice(&event.a.to_ne_bytes());
    bytes[8..12].copy_from_slice(&event.b.to_ne_bytes());
    bytes[12..].copy_from_slice(&event.c.to_ne_bytes());
    if process.mm().copy_out(out, &bytes).is_err() {
        return -errno::EFAULT;
    }
    1
}

/// `thread_create`: every argument checked before anything is made, then a thread of
/// this process in a slot of its own, starting at its program.
fn sys_thread_create(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    let [entry, stack_top, tls, arg, clear_word, _] = args;
    let user = process.mm();
    if !user.in_window(entry, 1) || !user.is_executable(entry) {
        return -errno::EFAULT;
    }
    // The top is one past the stack, so it may be the window's end, never its base.
    let stack_ok = stack_top.is_multiple_of(16) && stack_top.checked_sub(16).is_some_and(|low| user.in_window(low, 16));
    let tls_ok = tls == 0 || user.in_window(tls, 1);
    let clear_ok = clear_word == 0 || (clear_word.is_multiple_of(4) && user.in_window(clear_word, 4));
    if !stack_ok || !tls_ok || !clear_ok {
        return -errno::EINVAL;
    }
    let Some(shared) = process::share(slot) else {
        return -errno::EAGAIN;
    };
    let Some(_creating) = process.creating() else {
        return KILLED;
    };
    // Its registers all zero but these, its floating point as a new program's.
    let frame = Frame::new_user(entry, stack_top, arg, 0);
    match sched::spawn_thread(frame, user.root(), user.tlb(), tls, clear_word, |new| process::adopt(new, shared)) {
        Ok(tid) => tid as isize,
        Err(sched::ThreadError::Again) => -errno::EAGAIN,
    }
}

/// `thread_exit`: only returns if the range is wrong.
fn sys_thread_exit(slot: usize, process: &Process, address: usize, length: usize) -> isize {
    if let Err(error) = memory_calls::thread_exit(process.mm(), sched::clear_word(slot), address, length) {
        return error;
    }
    sched::exit_thread()
}

fn sys_set_tls(slot: usize, process: &Process, value: usize) -> isize {
    // On x86_64 the window is what keeps a non-canonical base out of `wrmsr`.
    if value != 0 && !process.mm().in_window(value, 1) {
        return -errno::EINVAL;
    }
    sched::set_tls(slot, value);
    0
}

/// Carries out a system call for the task in `slot`. Everything in `args` is whatever
/// the program left in its registers.
pub fn syscall(slot: usize, number: usize, args: [usize; 6]) -> isize {
    // Ends the process, every thread of it. Before the process is borrowed: the exit
    // gives it back.
    if number == nr::EXIT {
        sched::exit_user(feos_signal::Status::Exited(args[0] as i32 & 0xff));
    }
    let Some(process) = process(slot) else {
        // A user task always has its process. There's no one to report this to but the
        // kernel's own log, and no way to carry on.
        sched::exit_user(feos_signal::FAULTED);
    };
    memory_calls::forget_fault(slot);
    memory_calls::apply_zaps(process.mm());
    let result = dispatch(slot, process, number, args);
    // A truncation in the call, of a file this space maps, takes effect before it's back.
    memory_calls::apply_zaps(process.mm());
    // The call may have let go of the last hold on a file unlinked while in use: a
    // close, an unmap, a dup over a descriptor.
    fs_calls::sweep_orphans();
    // A write with nobody to read it raises `SIGPIPE` on the writer, as the call has
    // unwound (a kill's own `EPIPE` is never seen: the task ends on its way out).
    if result == -errno::EPIPE && matches!(number, nr::WRITE | nr::PWRITE) && !sched::kill_requested() {
        signal::broken_pipe(slot, process);
    }
    result
}

fn dispatch(slot: usize, process: &Process, number: usize, args: [usize; 6]) -> isize {
    let user = process.mm();
    match number {
        nr::WRITE => files::sys_write(slot, process, args[0], args[1], args[2]),
        nr::READ => files::sys_read(slot, process, args[0], args[1], args[2]),
        nr::OPEN => files::sys_open(slot, process, args[0], args[1], args[2]),
        nr::CLOSE => files::sys_close(process, args[0]),
        nr::SEEK => files::sys_seek(process, args[0], args[1] as isize, args[2]),
        nr::DUP3 => files::sys_dup3(process, args[0], args[1], args[2]),
        nr::FCNTL => files::sys_fcntl(process, args[0], args[1], args[2]),
        nr::PIPE2 => files::sys_pipe2(process, args[0], args[1]),
        nr::CLOSE_RANGE => files::sys_close_range(process, args[0], args[1], args[2]),
        nr::OPENAT => fs_calls::sys_openat(slot, process, args),
        nr::FSTATAT => fs_calls::sys_fstatat(slot, process, args),
        nr::GETDENTS => fs_calls::sys_getdents(process, args[0], args[1], args[2]),
        nr::MKDIRAT => fs_calls::sys_mkdirat(slot, process, args),
        nr::UNLINKAT => fs_calls::sys_unlinkat(slot, process, args),
        nr::FTRUNCATE => fs_calls::sys_ftruncate(process, args[0], args[1]),
        nr::FSYNC => fs_calls::sys_fsync(process, args[0], args[1]),
        nr::PREAD => files::sys_pread_pwrite(process, args[0], args[1], args[2], args[3], false),
        nr::PWRITE => files::sys_pread_pwrite(process, args[0], args[1], args[2], args[3], true),
        nr::FCHDIR => fs_calls::sys_fchdir(slot, process, args[0]),
        nr::GETCWD => fs_calls::sys_getcwd(process, args[0], args[1]),
        nr::UMASK => fs_calls::sys_umask(process, args[0]),
        nr::FD_PATH => fs_calls::sys_fd_path(slot, process, args[0], args[1], args[2]),
        nr::CURRENT_EXE => fs_calls::sys_current_exe(process, args[0], args[1]),
        nr::RENAMEAT => tree_calls::sys_renameat(slot, process, args),
        nr::LINKAT => tree_calls::sys_linkat(slot, process, args),
        nr::SYMLINKAT => tree_calls::sys_symlinkat(slot, process, args),
        nr::READLINKAT => tree_calls::sys_readlinkat(slot, process, args),
        nr::FCHMODAT => tree_calls::sys_fchmodat(slot, process, args),
        nr::FCHOWNAT => tree_calls::sys_fchownat(slot, process, args),
        nr::UTIMENSAT => tree_calls::sys_utimensat(slot, process, args),
        nr::FACCESSAT => tree_calls::sys_faccessat(slot, process, args),
        nr::FSTATVFS => tree_calls::sys_fstatvfs(process, args[0], args[1]),
        nr::SYNC => tree_calls::sys_sync(),
        nr::FLOCK => lock_calls::sys_flock(process, args[0], args[1]),
        nr::SLEEP => {
            sched::sleep_ms(args[0] as u64);
            0
        }
        nr::YIELD => {
            sched::yield_now();
            0
        }
        nr::GETPID => process.pid() as isize,
        nr::GETTID => {
            sched::injected_fault(slot);
            sched::tid(slot) as isize
        }
        nr::SET_TLS => sys_set_tls(slot, process, args[0]),
        nr::THREAD_CREATE => sys_thread_create(slot, process, args),
        nr::THREAD_EXIT => sys_thread_exit(slot, process, args[0], args[1]),
        nr::GETUID => sched::credentials(slot).0 as isize,
        nr::GETGID => sched::credentials(slot).1 as isize,
        nr::SETUID => sys_setuid(slot, args[0], args[1]),
        nr::STAT => sys_stat(slot, process, args[0], args[1], args[2]),
        nr::CHMOD => sys_chmod(slot, process, args[0], args[1], args[2]),
        nr::CHOWN => sys_chown(slot, process, args),
        nr::MKDIR => sys_make_or_remove(slot, process, args[0], args[1], true),
        nr::UNLINK => sys_make_or_remove(slot, process, args[0], args[1], false),
        nr::LINK => sys_link(slot, process, args),
        nr::GETCPU => sched::current_cpu() as isize,
        nr::UPTIME => sched::CLOCK.uptime_ms() as isize,
        nr::MMAP => memory_calls::sys_mmap(user, args[0], args[1]),
        nr::MUNMAP => memory_calls::sys_munmap(user, args[0], args[1]),
        nr::MPROTECT => memory_calls::sys_mprotect(user, args[0], args[1], args[2]),
        nr::MMAP2 => sys_mmap2(process, args),
        nr::MADVISE => memory_calls::sys_madvise(user, args[0], args[1], args[2]),
        nr::MSYNC => memory_calls::sys_msync(user, args[0], args[1], args[2]),
        nr::SPAWN => proc::sys_spawn(process, args),
        nr::SPAWN2 => proc::sys_spawn2(slot, process, args[0]),
        nr::EXEC => proc::sys_exec(slot, process, args[0]),
        nr::WAIT => proc::sys_wait(slot, process, args[0]),
        nr::WAIT4 => proc::sys_wait4(slot, process, args),
        nr::KILL => proc::sys_kill(slot, process, args[0], args[1]),
        nr::SIGACTION => signal::sys_sigaction(slot, process, args[0], args[1], args[2]),
        nr::THREAD_KILL => signal::sys_thread_kill(slot, args[0], args[1]),
        nr::SIGPROCMASK => signal::sys_sigprocmask(slot, process, args[0], args[1], args[2]),
        nr::SIGALTSTACK => signal::sys_sigaltstack(slot, process, args[0], args[1]),
        nr::SIGPENDING => signal::sys_sigpending(slot, process, args[0]),
        nr::SIGSUSPEND => signal::sys_sigsuspend(slot, process, args[0]),
        nr::SETITIMER => signal::sys_setitimer(process, args[0], args[1], args[2]),
        nr::GETPPID => sched::parent_pid(slot) as isize,
        nr::GETPGID => proc::sys_getpgid_getsid(slot, args[0], false),
        nr::SETPGID => proc::sys_setpgid(slot, args[0], args[1]),
        nr::GETSID => proc::sys_getpgid_getsid(slot, args[0], true),
        nr::SETSID => proc::sys_setsid(slot),
        nr::GETRUSAGE => proc::sys_getrusage(slot, process, args[0], args[1]),
        nr::SYSINFO => proc::sys_sysinfo(process, args[0], args[1]),
        nr::PIPE => files::sys_pipe2(process, args[0], 0),
        nr::FUTEX_WAIT => sys_futex_wait(process.mm(), args[0], args[1], args[2]),
        nr::FUTEX_WAIT2 => time_calls::sys_futex_wait2(user, args),
        nr::CLOCK_GETTIME => time_calls::sys_clock_gettime(slot, process, args[0], args[1]),
        nr::CLOCK_GETRES => time_calls::sys_clock_getres(process, args[0], args[1]),
        nr::NANOSLEEP => time_calls::sys_nanosleep(process, args),
        nr::FUTEX_WAKE => match user.futex_key(args[0]) {
            Ok(key) => sched::futex_wake_user(key, args[1]) as isize,
            Err(_) => -errno::EFAULT,
        },
        nr::SURFACE_CREATE => sys_surface_create(process, args[0], args[1]),
        nr::SURFACE_PRESENT => match surface_of(process) {
            Some(index) => {
                surface::present(index);
                0
            }
            None => -errno::EINVAL,
        },
        nr::POLL_EVENT => sys_poll_event(process, args[0]),
        _ => -errno::ENOSYS,
    }
}
