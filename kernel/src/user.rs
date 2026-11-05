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
//! Memory: the program's page tables give it its own pages and nothing else; the
//! kernel's mappings in them are for the supervisor only. Pointers: every address and
//! length in a call goes through `feos-vm`, which walks the tables and never touches
//! the address itself. Privilege: EL0 and ring 3 trap anything else. Failure: a fault
//! ends the process with status 139, which costs nobody else anything, and its
//! frames go back whichever way it ended.
//!
//! What this doesn't do is isolate kernel-linked apps from anything, or the filesystem
//! from programs: the tree is shared and there are no users or permissions.

use core::cell::UnsafeCell;
use core::mem;

use feos_abi::{errno, nr, open as open_flags, seek as seek_whence, Event, UserStr};
use feos_app::pipe::{self, PipeId};
use feos_app::{ArgBlock, SpawnIo, MAX_PROCESSES, PROCESS_ARGS_MAX};
use feos_elf::Limits;
use feos_fs::vfs::VfsTree;
use feos_fs::{FileKind, FsError};
use feos_hal::{Arch, Clock, PageAccess};
use feos_vm::{AddressSpace, FrameSource, MapError, PAGE_SIZE};

use crate::sched::{self, RawStreams};
use crate::{memory, surface, Frame, Port};

#[cfg(target_arch = "aarch64")]
const MACHINE: u16 = feos_elf::EM_AARCH64;
#[cfg(target_arch = "x86_64")]
const MACHINE: u16 = feos_elf::EM_X86_64;

/// Frames for user memory and page tables come straight from the kernel's allocator.
struct KernelFrames;

impl FrameSource for KernelFrames {
    fn alloc(&mut self) -> Option<usize> {
        memory::alloc_frames(1, 1)
    }

    fn free(&mut self, frame: usize) {
        memory::free_frames(frame, 1);
    }
}

/// Holds off a kill for as long as it's alive: a frame just allocated but not yet in a
/// table is in no record that a kill could reclaim it from.
struct NoKill;

impl NoKill {
    fn new() -> Self {
        sched::lock_enter();
        Self
    }
}

impl Drop for NoKill {
    fn drop(&mut self) {
        sched::lock_exit();
    }
}

/// What a program's address space is made of.
pub struct Process {
    space: AddressSpace,
    /// What to load: the same as `space`'s root, or a table above it.
    root: usize,
    /// Where `mmap` looks next.
    mmap_next: usize,
    /// The window surface, as its index, address and length.
    surface: Option<(usize, usize, usize)>,
}

impl Process {
    /// Gives everything back. No CPU has these tables loaded any more.
    pub fn destroy(mut self) {
        if let Some((index, address, length)) = self.surface.take() {
            self.space.unmap_shared(address, length);
            surface::release(index);
        }
        let tables = self.space.root();
        let root = self.root;
        self.space.destroy(&mut KernelFrames);
        if root != tables && root != 0 {
            memory::free_frames(root, 1);
        }
    }
}

struct ProcessCell(UnsafeCell<Option<Process>>);

// SAFETY: a cell belongs to the task in its slot. The scheduler takes one only when that
// task has gone, under its lock, before the slot can be reused.
unsafe impl Sync for ProcessCell {}

static PROCESSES: [ProcessCell; MAX_PROCESSES] =
    [const { ProcessCell(UnsafeCell::new(None)) }; MAX_PROCESSES];

/// Takes the address space of a task that has gone.
pub fn take_process(slot: usize) -> Option<Process> {
    // SAFETY: see `ProcessCell`.
    unsafe { (*PROCESSES[slot].0.get()).take() }
}

/// The address space of the calling task, which is in `slot`.
fn process(slot: usize) -> Option<&'static mut Process> {
    // SAFETY: see `ProcessCell`. The task is the only user, and is single threaded.
    unsafe { (*PROCESSES[slot].0.get()).as_mut() }
}

#[derive(Clone, Copy)]
struct OpenFile {
    path: [u8; feos_abi::PATH_MAX],
    length: usize,
    offset: u64,
    readable: bool,
    writable: bool,
    append: bool,
}

impl OpenFile {
    fn path(&self) -> &str {
        core::str::from_utf8(&self.path[..self.length]).unwrap_or("")
    }
}

/// An open file. No heap and no handle in the VFS: a file is its path and position, so
/// a killed process leaves nothing behind for anyone to clean up, and there's no call
/// that has to run with interrupts on when a task goes.
#[derive(Clone, Copy)]
enum Fd {
    Closed,
    Stdin,
    Stdout,
    Stderr,
    PipeRead(u32),
    PipeWrite(u32),
    File(OpenFile),
}

struct Files {
    fds: [Fd; feos_abi::MAX_FDS],
    cwd: [u8; feos_abi::PATH_MAX],
    cwd_length: usize,
}

struct FilesCell(UnsafeCell<Files>);

// SAFETY: as `ProcessCell`, with `reset_files` called while the slot is reserved.
unsafe impl Sync for FilesCell {}

static FILES: [FilesCell; MAX_PROCESSES] = [const {
    FilesCell(UnsafeCell::new(Files {
        fds: [Fd::Closed; feos_abi::MAX_FDS],
        cwd: [0; feos_abi::PATH_MAX],
        cwd_length: 0,
    }))
}; MAX_PROCESSES];

fn files(slot: usize) -> &'static mut Files {
    // SAFETY: see `FilesCell`.
    unsafe { &mut *FILES[slot].0.get() }
}

/// Sets up the files of a new program, whose slot is reserved.
pub fn reset_files(slot: usize, cwd: &str) {
    let files = files(slot);
    files.fds = [Fd::Closed; feos_abi::MAX_FDS];
    files.fds[0] = Fd::Stdin;
    files.fds[1] = Fd::Stdout;
    files.fds[2] = Fd::Stderr;
    let length = cwd.len().min(files.cwd.len());
    files.cwd[..length].copy_from_slice(&cwd.as_bytes()[..length]);
    files.cwd_length = length;
}

fn tree() -> Option<&'static dyn VfsTree> {
    sched::platform().filesystem.and_then(|filesystem| filesystem.tree())
}

fn say(message: core::fmt::Arguments<'_>) {
    feos_console::eprintln!("{}", message);
}

/// Loads the program at `path` and runs it. Only returns if it can't, with the status
/// the process should end with. The address space is the process's from the moment
/// it exists, so a failure or a kill anywhere in here still gets its frames back.
pub fn start(slot: usize, path: &str, args: &ArgBlock) -> i32 {
    match load(slot, path, args) {
        Ok((root, frame)) => sched::enter_user(slot, root, frame),
        Err(status) => status,
    }
}

fn load(slot: usize, path: &str, args: &ArgBlock) -> Result<(usize, Frame), i32> {
    let Some(tree) = tree() else {
        say(format_args!("{path}: no filesystem"));
        return Err(feos_abi::EXIT_NOT_FOUND);
    };
    let size = match tree.stat(path) {
        Ok(stat) if stat.kind == FileKind::File => stat.size,
        Ok(_) => {
            say(format_args!("{path}: not a file"));
            return Err(feos_abi::EXIT_NOT_EXECUTABLE);
        }
        Err(_) => {
            say(format_args!("{path}: No such file or directory"));
            return Err(feos_abi::EXIT_NOT_FOUND);
        }
    };

    let mut head = [0u8; feos_elf::HEADER_BYTES];
    let got = match tree.read_at(path, 0, &mut head) {
        Ok(got) => got,
        Err(_) => {
            say(format_args!("{path}: can't be read"));
            return Err(feos_abi::EXIT_NOT_EXECUTABLE);
        }
    };
    let limits = Limits {
        machine: MACHINE,
        base: feos_abi::USER_BASE as u64,
        end: feos_abi::USER_IMAGE_END as u64,
        max_memory: feos_abi::USER_MEMORY_LIMIT as u64,
    };
    let program = match feos_elf::parse(&head[..got], size, &limits) {
        Ok(program) => program,
        Err(error) => {
            say(format_args!("{path}: {}", error.text()));
            return Err(feos_abi::EXIT_NOT_EXECUTABLE);
        }
    };

    let root = {
        let _no_kill = NoKill::new();
        let Some(space) = AddressSpace::new(
            &mut KernelFrames,
            Port::user_page_bits(),
            (feos_abi::USER_BASE, feos_abi::USER_END),
            feos_abi::USER_MEMORY_LIMIT / PAGE_SIZE,
        ) else {
            say(format_args!("{path}: out of memory"));
            return Err(feos_abi::EXIT_NOT_EXECUTABLE);
        };
        let mut alloc = || memory::alloc_frames(1, 1);
        // SAFETY: a table just made for this, on a machine past boot.
        let root = unsafe { Port::prepare_user_tables(space.root(), &mut alloc) };
        let Some(root) = root else {
            space.destroy(&mut KernelFrames);
            say(format_args!("{path}: out of memory"));
            return Err(feos_abi::EXIT_NOT_EXECUTABLE);
        };
        // SAFETY: see `ProcessCell`.
        unsafe {
            *PROCESSES[slot].0.get() = Some(Process {
                space,
                root,
                mmap_next: feos_abi::USER_IMAGE_END,
                surface: None,
            });
        }
        root
    };
    let Some(process) = process(slot) else {
        return Err(feos_abi::EXIT_NOT_EXECUTABLE);
    };

    for segment in program.segments() {
        let access = PageAccess {
            write: segment.writable,
            execute: segment.executable,
        };
        let start = segment.page_start() as usize;
        let length = (segment.page_end() - segment.page_start()) as usize;
        {
            let _no_kill = NoKill::new();
            if process.space.map_new(start, length, access, &mut KernelFrames).is_err() {
                say(format_args!("{path}: out of memory, or overlapping segments"));
                return Err(feos_abi::EXIT_NOT_EXECUTABLE);
            }
        }
        let mut done = 0u64;
        while done < segment.file_size {
            let address = segment.address as usize + done as usize;
            let Ok(target) = process.space.translate(address, false) else {
                return Err(feos_abi::EXIT_NOT_EXECUTABLE);
            };
            let count = ((PAGE_SIZE - address % PAGE_SIZE) as u64).min(segment.file_size - done) as usize;
            // SAFETY: `target` is a frame of this new space, which nothing can run yet,
            // and `count` stops at its end.
            let out = unsafe { core::slice::from_raw_parts_mut(target as *mut u8, count) };
            match tree.read_at(path, segment.offset + done, out) {
                Ok(got) if got > 0 => done += got as u64,
                _ => {
                    say(format_args!("{path}: truncated"));
                    return Err(feos_abi::EXIT_NOT_EXECUTABLE);
                }
            }
        }
        if segment.executable {
            let mut page = start;
            while page < start + length {
                if let Ok(frame) = process.space.translate(page, false) {
                    Port::sync_user_code(frame, PAGE_SIZE);
                }
                page += PAGE_SIZE;
            }
        }
    }

    let stack_low = feos_abi::USER_STACK_TOP - feos_abi::USER_STACK_SIZE;
    let writable = PageAccess { write: true, execute: false };
    {
        let _no_kill = NoKill::new();
        if process
            .space
            .map_new(stack_low, feos_abi::USER_STACK_SIZE, writable, &mut KernelFrames)
            .is_err()
        {
            say(format_args!("{path}: out of memory"));
            return Err(feos_abi::EXIT_NOT_EXECUTABLE);
        }
    }

    let (argc, argv, stack) = match push_args(process, args) {
        Ok(place) => place,
        Err(()) => return Err(feos_abi::EXIT_NOT_EXECUTABLE),
    };
    Port::sync_user_tables();
    Ok((root, Frame::new_user(program.entry as usize, stack, argc, argv)))
}

/// Copies the arguments to the top of the new stack, and says where they are.
fn push_args(process: &Process, args: &ArgBlock) -> Result<(usize, usize, usize), ()> {
    let mut top = feos_abi::USER_STACK_TOP;
    let mut strings = [UserStr::default(); feos_abi::MAX_ARGS];
    let mut count = 0;
    for arg in args.iter().take(feos_abi::MAX_ARGS) {
        top -= arg.len();
        process.space.copy_out(top, arg.as_bytes()).map_err(|_| ())?;
        strings[count] = UserStr { pointer: top, length: arg.len() };
        count += 1;
    }
    let array = (top - count * mem::size_of::<UserStr>()) & !0xf;
    let mut at = array;
    for string in &strings[..count] {
        process.space.copy_out(at, &string.pointer.to_ne_bytes()).map_err(|_| ())?;
        process.space.copy_out(at + 8, &string.length.to_ne_bytes()).map_err(|_| ())?;
        at += mem::size_of::<UserStr>();
    }
    Ok((count, array, array))
}

/// What a filesystem error is called across the boundary.
fn fs_errno(error: FsError) -> isize {
    -match error {
        FsError::NotFound => errno::ENOENT,
        FsError::NameExists => errno::EEXIST,
        FsError::NoSpace => errno::ENOSPC,
        FsError::OutOfMemory => errno::ENOMEM,
        FsError::IsDirectory => errno::EISDIR,
        FsError::NotDirectory | FsError::InvalidName => errno::EINVAL,
        _ => errno::EIO,
    }
}

/// Copies a path from the program and makes it absolute.
fn user_path<'a>(
    slot: usize,
    process: &Process,
    pointer: usize,
    length: usize,
    out: &'a mut [u8; feos_abi::PATH_MAX],
) -> Result<&'a str, isize> {
    if length == 0 {
        return Err(-errno::ENOENT);
    }
    if !process.space.in_window(pointer, 1) {
        return Err(-errno::EFAULT);
    }
    if length > feos_abi::PATH_MAX {
        return Err(-errno::ENAMETOOLONG);
    }
    let mut given = [0u8; feos_abi::PATH_MAX];
    process
        .space
        .copy_in(pointer, &mut given[..length])
        .map_err(|_| -errno::EFAULT)?;
    let given = core::str::from_utf8(&given[..length]).map_err(|_| -errno::EINVAL)?;
    if given.starts_with('/') {
        out[..length].copy_from_slice(given.as_bytes());
        return core::str::from_utf8(&out[..length]).map_err(|_| -errno::EINVAL);
    }
    let files = files(slot);
    let cwd = core::str::from_utf8(&files.cwd[..files.cwd_length]).unwrap_or("/");
    let total = cwd.len() + 1 + length;
    if total > feos_abi::PATH_MAX {
        return Err(-errno::ENAMETOOLONG);
    }
    out[..cwd.len()].copy_from_slice(cwd.as_bytes());
    out[cwd.len()] = b'/';
    out[cwd.len() + 1..total].copy_from_slice(given.as_bytes());
    core::str::from_utf8(&out[..total]).map_err(|_| -errno::EINVAL)
}

/// Calls `step` with each page's worth of a user buffer, already validated, until the
/// buffer is done, or `step` moves less than it was given or fails. A result of zero
/// bytes with an error returns the error; otherwise, the count moved so far.
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
    // Checked at its whole length first: a length that runs off the end is a wild pointer
    // even though a call only moves a part of it at a time.
    if !process.space.in_window(address, length) {
        return -errno::EFAULT;
    }
    let length = length.min(feos_abi::IO_CHUNK);
    if process.space.validate(address, length, write).is_err() {
        return -errno::EFAULT;
    }
    let mut done = 0;
    while done < length {
        let here = address + done;
        let Ok(frame) = process.space.translate(here, write) else {
            return -errno::EFAULT;
        };
        let count = (PAGE_SIZE - here % PAGE_SIZE).min(length - done);
        // SAFETY: the page is this process's, it can't change during this call since a
        // process has one thread, and the kernel's mapping of the frame is its address.
        let piece = unsafe { core::slice::from_raw_parts_mut(frame as *mut u8, count) };
        match step(piece) {
            Ok(moved) => {
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

fn sys_write(slot: usize, process: &Process, fd: usize, address: usize, length: usize) -> isize {
    let table = files(slot);
    let Some(entry) = table.fds.get_mut(fd) else {
        return -errno::EBADF;
    };
    let stream = match entry {
        Fd::Stdout => Some(false),
        Fd::Stderr => Some(true),
        _ => None,
    };
    if let Some(error) = stream {
        return with_user_buffer(process, address, length, false, |piece| {
            if sched::write_stream(slot, error, piece) {
                Ok(piece.len())
            } else {
                Err(-errno::EPIPE)
            }
        });
    }
    match entry {
        Fd::PipeWrite(id) => {
            let Some(id) = PipeId::from_raw(*id) else {
                return -errno::EBADF;
            };
            with_user_buffer(process, address, length, false, |piece| {
                if pipe::write_id(id, piece) {
                    Ok(piece.len())
                } else {
                    Err(-errno::EPIPE)
                }
            })
        }
        Fd::File(file) if file.writable => {
            let Some(tree) = tree() else {
                return -errno::EIO;
            };
            if file.append {
                match tree.stat(file.path()) {
                    Ok(stat) => file.offset = stat.size,
                    Err(error) => return fs_errno(error),
                }
            }
            let result = with_user_buffer(process, address, length, false, |piece| {
                tree.write_at(file.path(), file.offset, piece)
                    .map(|moved| {
                        file.offset += moved as u64;
                        moved
                    })
                    .map_err(fs_errno)
            });
            result
        }
        _ => -errno::EBADF,
    }
}

fn sys_read(slot: usize, process: &Process, fd: usize, address: usize, length: usize) -> isize {
    let table = files(slot);
    let Some(entry) = table.fds.get_mut(fd) else {
        return -errno::EBADF;
    };
    match entry {
        // A pipe read returns what's there, not what was asked for, as soon as it has
        // something: with a short count, `with_user_buffer` stops.
        Fd::Stdin => with_user_buffer(process, address, length, true, |piece| {
            Ok(sched::read_stdin(slot, piece))
        }),
        Fd::PipeRead(id) => {
            let Some(id) = PipeId::from_raw(*id) else {
                return -errno::EBADF;
            };
            with_user_buffer(process, address, length, true, |piece| Ok(pipe::read_id(id, piece)))
        }
        Fd::File(file) if file.readable => {
            let Some(tree) = tree() else {
                return -errno::EIO;
            };
            with_user_buffer(process, address, length, true, |piece| {
                tree.read_at(file.path(), file.offset, piece)
                    .map(|moved| {
                        file.offset += moved as u64;
                        moved
                    })
                    .map_err(fs_errno)
            })
        }
        _ => -errno::EBADF,
    }
}

fn free_fd(table: &Files) -> Option<usize> {
    table.fds.iter().position(|fd| matches!(fd, Fd::Closed))
}

fn sys_open(slot: usize, process: &Process, pointer: usize, length: usize, flags: usize) -> isize {
    if flags & !open_flags::ALL != 0 || flags & (open_flags::READ | open_flags::WRITE) == 0 {
        return -errno::EINVAL;
    }
    let mut buffer = [0u8; feos_abi::PATH_MAX];
    let path = match user_path(slot, process, pointer, length, &mut buffer) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    let table = files(slot);
    let Some(fd) = free_fd(table) else {
        return -errno::EMFILE;
    };
    let writable = flags & open_flags::WRITE != 0;
    match tree.stat(path) {
        Ok(stat) if stat.kind == FileKind::Directory => return -errno::EISDIR,
        Ok(_) => {}
        Err(FsError::NotFound) if flags & open_flags::CREATE != 0 && writable => {
            if let Err(error) = tree.create_file(path) {
                return fs_errno(error);
            }
        }
        Err(error) => return fs_errno(error),
    }
    if flags & open_flags::TRUNCATE != 0 && writable {
        if let Err(error) = tree.truncate(path, 0) {
            return fs_errno(error);
        }
    }
    let mut file = OpenFile {
        path: [0; feos_abi::PATH_MAX],
        length: path.len(),
        offset: 0,
        readable: flags & open_flags::READ != 0,
        writable,
        append: flags & open_flags::APPEND != 0,
    };
    file.path[..path.len()].copy_from_slice(path.as_bytes());
    table.fds[fd] = Fd::File(file);
    fd as isize
}

fn current_pid() -> u32 {
    use feos_app::Processes;
    sched::PROCESSES.current()
}

fn sys_close(slot: usize, fd: usize) -> isize {
    let table = files(slot);
    let Some(entry) = table.fds.get_mut(fd) else {
        return -errno::EBADF;
    };
    match *entry {
        Fd::Closed => return -errno::EBADF,
        Fd::PipeRead(id) => {
            if let Some(id) = PipeId::from_raw(id) {
                pipe::close_reader_id(id, current_pid());
            }
        }
        Fd::PipeWrite(id) => {
            if let Some(id) = PipeId::from_raw(id) {
                pipe::close_writer_id(id, current_pid());
            }
        }
        _ => {}
    }
    *entry = Fd::Closed;
    0
}

fn sys_seek(slot: usize, fd: usize, offset: isize, whence: usize) -> isize {
    let table = files(slot);
    let Some(Fd::File(file)) = table.fds.get_mut(fd) else {
        return -errno::EBADF;
    };
    let base = match whence {
        seek_whence::SET => 0,
        seek_whence::CURRENT => file.offset,
        seek_whence::END => {
            let Some(tree) = tree() else {
                return -errno::EIO;
            };
            match tree.stat(file.path()) {
                Ok(stat) => stat.size,
                Err(error) => return fs_errno(error),
            }
        }
        _ => return -errno::EINVAL,
    };
    match base.checked_add_signed(offset as i64) {
        Some(position) if position <= isize::MAX as u64 => {
            file.offset = position;
            position as isize
        }
        _ => -errno::EINVAL,
    }
}

fn sys_mmap(slot: usize, process: &mut Process, length: usize, prot: usize) -> isize {
    if prot & !(feos_abi::prot::READ | feos_abi::prot::WRITE) != 0 || prot == 0 {
        return -errno::EINVAL;
    }
    if length == 0 || length > feos_abi::USER_MEMORY_LIMIT {
        return if length == 0 { -errno::EINVAL } else { -errno::ENOMEM };
    }
    let Some(length) = length.checked_next_multiple_of(PAGE_SIZE) else {
        return -errno::ENOMEM;
    };
    let address = process.mmap_next;
    // An unmapped page between mappings, so that running off the end of one faults.
    let Some(next) = address.checked_add(length + PAGE_SIZE) else {
        return -errno::ENOMEM;
    };
    if next > feos_abi::USER_MMAP_END {
        return -errno::ENOMEM;
    }
    let access = PageAccess {
        write: prot & feos_abi::prot::WRITE != 0,
        execute: false,
    };
    let _ = slot;
    let result = {
        let _no_kill = NoKill::new();
        process.space.map_new(address, length, access, &mut KernelFrames)
    };
    Port::sync_user_tables();
    match result {
        Ok(()) => {
            process.mmap_next = next;
            address as isize
        }
        Err(MapError::BadRange) => -errno::EINVAL,
        Err(_) => -errno::ENOMEM,
    }
}

fn sys_munmap(slot: usize, process: &mut Process, address: usize, length: usize) -> isize {
    if length == 0 || address % PAGE_SIZE != 0 {
        return -errno::EINVAL;
    }
    let Some(length) = length.checked_next_multiple_of(PAGE_SIZE) else {
        return -errno::EINVAL;
    };
    // Only what `mmap` handed out: not the program, not the stack.
    let Some(end) = address.checked_add(length) else {
        return -errno::EINVAL;
    };
    if address < feos_abi::USER_IMAGE_END || end > process.mmap_next {
        return -errno::EINVAL;
    }
    if let Some((_, surface_at, surface_length)) = process.surface {
        if address < surface_at + surface_length && surface_at < end {
            return -errno::EINVAL;
        }
    }
    let asid = slot as u16 + 1;
    let result = {
        let _no_kill = NoKill::new();
        process.space.unmap_each(address, length, &mut KernelFrames, &mut |page| {
            Port::flush_user_page(page, asid);
        })
    };
    match result {
        Ok(_) => {
            // The last mapping can be reused, which keeps a program that keeps
            // allocating and freeing from running out of addresses.
            if end + PAGE_SIZE == process.mmap_next {
                process.mmap_next = address;
            }
            0
        }
        Err(_) => -errno::EINVAL,
    }
}

/// `spawn`: copies everything from the program before anything happens.
fn sys_spawn(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    let [path_pointer, path_length, argv, argc, stdin_fd, stdout_fd] = args;
    let mut buffer = [0u8; feos_abi::PATH_MAX];
    let path = match user_path(slot, process, path_pointer, path_length, &mut buffer) {
        Ok(path) => path,
        Err(error) => return error,
    };
    if argc > feos_abi::MAX_ARGS {
        return -errno::ENAMETOOLONG;
    }
    let mut block = ArgBlock::new();
    let mut scratch = [0u8; PROCESS_ARGS_MAX];
    for index in 0..argc {
        let Some(at) = index
            .checked_mul(mem::size_of::<UserStr>())
            .and_then(|offset| argv.checked_add(offset))
        else {
            return -errno::EFAULT;
        };
        let mut raw = [0u8; 16];
        if process.space.copy_in(at, &mut raw).is_err() {
            return -errno::EFAULT;
        }
        let mut word = [0u8; 8];
        word.copy_from_slice(&raw[..8]);
        let pointer = usize::from_ne_bytes(word);
        word.copy_from_slice(&raw[8..]);
        let length = usize::from_ne_bytes(word);
        if length != 0 && !process.space.in_window(pointer, length) {
            return -errno::EFAULT;
        }
        if length > scratch.len() {
            return -errno::ENAMETOOLONG;
        }
        if process.space.copy_in(pointer, &mut scratch[..length]).is_err() {
            return -errno::EFAULT;
        }
        let Ok(text) = core::str::from_utf8(&scratch[..length]) else {
            return -errno::EINVAL;
        };
        if !block.push(text) {
            return -errno::ENAMETOOLONG;
        }
    }

    let table = files(slot);
    let stdin = match stdin_fd {
        feos_abi::INHERIT => 0,
        fd => match table.fds.get(fd) {
            Some(Fd::PipeRead(id)) => *id,
            Some(Fd::Stdin) => 0,
            _ => return -errno::EBADF,
        },
    };
    let stdout = match stdout_fd {
        feos_abi::INHERIT => 0,
        fd => match table.fds.get(fd) {
            Some(Fd::PipeWrite(id)) => *id,
            Some(Fd::Stdout) => 0,
            _ => return -errno::EBADF,
        },
    };
    let cwd = core::str::from_utf8(&table.cwd[..table.cwd_length]).unwrap_or("/");
    // The program's stream of its own, if it inherits, is the scheduler's business.
    let raw = RawStreams { stdin, stdout };
    match sched::spawn_user(path, block, cwd, SpawnIo::default(), raw) {
        Ok(pid) => pid as isize,
        Err(feos_app::SpawnError::NoFreeSlot) => -errno::EBUSY,
        Err(feos_app::SpawnError::ArgsTooLong) => -errno::ENAMETOOLONG,
        Err(_) => -errno::EINVAL,
    }
}

fn sys_pipe(slot: usize, process: &Process, out: usize) -> isize {
    if process.space.validate(out, 8, true).is_err() {
        return -errno::EFAULT;
    }
    let table = files(slot);
    let Some(read_fd) = free_fd(table) else {
        return -errno::EMFILE;
    };
    let write_fd = table
        .fds
        .iter()
        .enumerate()
        .position(|(fd, entry)| fd != read_fd && matches!(entry, Fd::Closed));
    let Some(write_fd) = write_fd else {
        return -errno::EMFILE;
    };
    let Some((reader, writer)) = pipe::create() else {
        return -errno::EMFILE;
    };
    table.fds[read_fd] = Fd::PipeRead(reader.id().raw());
    table.fds[write_fd] = Fd::PipeWrite(writer.id().raw());
    // The pool records this task as holding both ends, and closes them if it goes.
    mem::forget(reader);
    mem::forget(writer);
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&(read_fd as u32).to_ne_bytes());
    bytes[4..].copy_from_slice(&(write_fd as u32).to_ne_bytes());
    if process.space.copy_out(out, &bytes).is_err() {
        return -errno::EFAULT;
    }
    0
}

fn sys_futex_wait(process: &Process, address: usize, expected: usize, timeout_ms: usize) -> isize {
    let Ok(key) = process.space.translate_word(address) else {
        return -errno::EFAULT;
    };
    // SAFETY: the word is aligned, in this process's own page, and the kernel's address
    // for it. The page can't go during this call.
    let word = unsafe { &*(key as *const core::sync::atomic::AtomicU32) };
    if word.load(core::sync::atomic::Ordering::Acquire) != expected as u32 {
        return -errno::EAGAIN;
    }
    if sched::futex_wait_user(key, expected as u32, timeout_ms as u64) {
        -errno::ETIMEDOUT
    } else {
        0
    }
}

fn sys_surface_create(process: &mut Process, width: usize, height: usize) -> isize {
    if process.surface.is_some() {
        return -errno::EEXIST;
    }
    if width == 0 || height == 0 || width > feos_abi::SURFACE_MAX_SIDE || height > feos_abi::SURFACE_MAX_SIDE {
        return -errno::EINVAL;
    }
    let bytes = width * height * 4;
    let Some(length) = bytes.checked_next_multiple_of(PAGE_SIZE) else {
        return -errno::EINVAL;
    };
    let address = process.mmap_next;
    let Some(next) = address.checked_add(length + PAGE_SIZE) else {
        return -errno::ENOMEM;
    };
    if next > feos_abi::USER_MMAP_END {
        return -errno::ENOMEM;
    }
    let _no_kill = NoKill::new();
    let Some((index, first_frame)) = surface::create(current_pid(), width, height) else {
        return -errno::ENOMEM;
    };
    let access = PageAccess { write: true, execute: false };
    if process
        .space
        .map_shared(address, first_frame, length, access, &mut KernelFrames)
        .is_err()
    {
        process.space.unmap_shared(address, length);
        surface::release(index);
        return -errno::ENOMEM;
    }
    Port::sync_user_tables();
    process.mmap_next = next;
    process.surface = Some((index, address, length));
    address as isize
}

fn sys_poll_event(process: &Process, out: usize) -> isize {
    if process.space.validate(out, mem::size_of::<Event>(), true).is_err() {
        return -errno::EFAULT;
    }
    let Some((index, _, _)) = process.surface else {
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
    if process.space.copy_out(out, &bytes).is_err() {
        return -errno::EFAULT;
    }
    1
}

/// Carries out a system call for the task in `slot`. Everything in `args` is whatever
/// the program left in its registers.
pub fn syscall(slot: usize, number: usize, args: [usize; 6]) -> isize {
    let Some(process) = process(slot) else {
        // A user task always has its space. There's no one to report this to but the
        // kernel's own log, and no way to carry on.
        sched::exit_user(feos_abi::EXIT_FAULTED);
    };
    match number {
        nr::EXIT => sched::exit_user(args[0] as i32 & 0xff),
        nr::WRITE => sys_write(slot, process, args[0], args[1], args[2]),
        nr::READ => sys_read(slot, process, args[0], args[1], args[2]),
        nr::OPEN => sys_open(slot, process, args[0], args[1], args[2]),
        nr::CLOSE => sys_close(slot, args[0]),
        nr::SEEK => sys_seek(slot, args[0], args[1] as isize, args[2]),
        nr::SLEEP => {
            sched::sleep_ms(args[0] as u64);
            0
        }
        nr::YIELD => {
            sched::yield_now();
            0
        }
        nr::GETPID => current_pid() as isize,
        nr::GETCPU => sched::current_cpu() as isize,
        nr::UPTIME => sched::CLOCK.uptime_ms() as isize,
        nr::MMAP => sys_mmap(slot, process, args[0], args[1]),
        nr::MUNMAP => sys_munmap(slot, process, args[0], args[1]),
        nr::SPAWN => sys_spawn(slot, process, args),
        nr::WAIT => match sched::wait_child(args[0] as u32, current_pid()) {
            Some(status) => status as isize,
            None => -errno::ECHILD,
        },
        nr::PIPE => sys_pipe(slot, process, args[0]),
        nr::FUTEX_WAIT => sys_futex_wait(process, args[0], args[1], args[2]),
        nr::FUTEX_WAKE => match process.space.translate_word(args[0]) {
            Ok(key) => sched::futex_wake_user(key, args[1]) as isize,
            Err(_) => -errno::EFAULT,
        },
        nr::SURFACE_CREATE => sys_surface_create(process, args[0], args[1]),
        nr::SURFACE_PRESENT => match process.surface {
            Some((index, _, _)) => {
                surface::present(index);
                0
            }
            None => -errno::EINVAL,
        },
        nr::POLL_EVENT => sys_poll_event(process, args[0]),
        _ => -errno::ENOSYS,
    }
}
