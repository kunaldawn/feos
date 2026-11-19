//! What a program's descriptors refer to, and the calls on them.
//!
//! A descriptor names an [`Ofd`], an open file description, through the process's
//! table ([`Files`], a `feos-fdtable` table of `Arc<Ofd>`). `dup3`, `F_DUPFD` and a pipe's
//! two descriptors share one, so they share its offset and status flags, as POSIX says;
//! each descriptor has its own close-on-exec flag. An open file description goes with
//! the last reference to it, whoever holds that: a descriptor's close, a `dup3` over it,
//! or the process's end, whose teardown drops the table in task context with interrupts
//! on (see `sched`'s teardown). Its `Drop` is what closes a pipe end, gives up the right
//! to write a file that can't be mapped to run meanwhile, and writes back what shared
//! mappings of the file dirtied. Nothing is dropped with the table's lock held: what
//! comes out of the table is let go after.
//!
//! A call takes a reference to the description and lets go of the table before it does
//! anything that can wait, so another thread can close the descriptor meanwhile; the
//! description, and the transfer, carry on until the call is done with it.
//!
//! Transfers. On a regular file a read or write is whole: `IO_CHUNK` at a time through
//! [`with_user_buffer`], each page bounced, until the request is done, the end of the
//! file is reached or the volume is full, with the description's offset held for all of
//! it, so two threads sharing one don't interleave. On a pipe or the console, a read
//! returns what's there as soon as there's anything; a write of at most `PIPE_BUF` bytes
//! is bounced whole and goes in one piece, and a larger one a page at a time until it's
//! all in (blocking) or the pipe is full (`O_NONBLOCK`). Nothing is pinned while a pipe
//! waits: the program's page is copied in before, or out after.
//!
//! A file's description holds it by identity (`FileId`, which the tree keeps stable
//! across renames and doesn't give another file while anything holds it), and holds its
//! entry in the lock table, the page cache's per-file record: every description of a
//! file, every mapping of it and a running program's text each hold one, which is the
//! hold count that lets `UNLINKAT` of a file in use keep it as an orphan (see
//! [`fs_calls`](super::fs_calls)). It remembers the canonical path it was opened by too,
//! for `FD_PATH` and a directory's `*at` calls, which join strings to it (section 3.3 of
//! the ABI design): a directory renamed after it was opened is found by its old name.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use feos_abi::{close_range, errno, f, int_arg, o, open as open_flags, seek as seek_whence, IO_CHUNK, MAX_FDS, PIPE_BUF};
use feos_app::pipe::{self, PipeId, Transfer};
use feos_fdtable::{FdError, FdTable};
use feos_fs::vfs::{path, Caller, DeviceKind, FileId, Snapshot, VfsTree, ACCESS_READ, ACCESS_WRITE};
use feos_fs::{FileKind, FsError};
use feos_sync::{Mutex, TryArc};
use feos_vm::{FileRef, PAGE_SIZE};

use super::fs_calls::{self, Fail};
use super::memory_calls::MapFd;
use super::process::{HeapCharge, Process};
use super::{check_parent, check_reach_link, fs_errno, lock_calls, tree, user_path, with_user_buffer, KILLED};
use crate::{pagecache, sched};

/// What an open file description refers to.
pub enum Object {
    /// One of the calling thread's standard streams, 0 to 2: the pipe its task was
    /// given, or the console (see `sched::stream_pipe`).
    Stream(usize),
    /// The read end of a pipe the program made, which this description owns.
    PipeRead(PipeId),
    /// The write end of a pipe the program made, which this description owns.
    PipeWrite(PipeId),
    /// A regular file (or a device node), by identity, with its lock-table entry (`None`
    /// only for a device the cache won't keep), counted there as a writer if `writer`: a
    /// file mapped to run can't be opened for writing, nor mapped to run while it is.
    File { path: Path, id: FileId, entry: Option<FileRef>, writer: bool },
    /// A directory, read with `GETDENTS` from the snapshot it takes; `READ` says `EISDIR`.
    /// Its identity is what a lock on it is keyed by.
    Directory { path: Path, id: FileId, listing: Mutex<Option<Listing>> },
    /// A symbolic link itself, opened `O_PATH | O_NOFOLLOW`.
    Link { path: Path },
    /// `/dev/console` opened: the console itself, whatever the thread's streams are.
    Console,
}

/// A directory's listing as `GETDENTS` took it, and the heap it's charged as.
pub struct Listing {
    pub snapshot: Snapshot,
    _charge: HeapCharge,
}

impl Listing {
    /// A snapshot of `directory`, charged to `process`.
    pub fn take(process: &Process, tree: &dyn VfsTree, directory: &str) -> Result<Self, isize> {
        let snapshot = Snapshot::take(tree, directory).map_err(fs_calls::errno_of)?;
        let charge = process.charge_heap(snapshot.heap_bytes())?;
        Ok(Self { snapshot, _charge: charge })
    }
}

/// The path a description remembers, shared with `mmap2` so the table isn't held while
/// the space is.
pub type Path = TryArc<String>;

/// A reference to an open file description, as the table and calls hold it.
pub type OfdRef = TryArc<Ofd>;

/// What the allocator adds to each block, about: its header and rounding.
const BLOCK_OVERHEAD: usize = 32;

/// What a description takes of the kernel heap, with a remembered path of `path` bytes.
const fn ofd_heap(path: Option<usize>) -> usize {
    let own = OfdRef::heap_size() + BLOCK_OVERHEAD;
    match path {
        Some(length) => own + Path::heap_size() + length + 2 * BLOCK_OVERHEAD,
        None => own,
    }
}

/// An open file description: what's open, how, and where in it.
pub struct Ofd {
    pub(super) object: Object,
    /// The access mode it was opened with: `O_RDONLY`, `O_WRONLY` or `O_RDWR`.
    access: usize,
    /// Its status flags: `O_APPEND` and `O_NONBLOCK`, the ones `F_SETFL` changes.
    status: AtomicUsize,
    /// Opened `O_PATH`: a reference to a place, for `*at` calls, `FSTATAT`, `FCHDIR`,
    /// `FD_PATH` and the table's own calls, and `EBADF` to anything else.
    pub(super) path_only: bool,
    /// Opened `O_DSYNC` or `O_SYNC`: every write is followed by an `FSYNC`.
    sync: bool,
    /// The position, held for a whole transfer; a directory's is the cookie of the next
    /// entry `GETDENTS` gives.
    pub(super) offset: Mutex<u64>,
    /// The kernel heap it and its path take, charged to the process that opened it
    /// (none for the standard streams a process starts with).
    _charge: Option<HeapCharge>,
    /// It may hold a `flock` lock, which its last close lets go of.
    flocked: AtomicBool,
}

impl Ofd {
    fn value(object: Object, access: usize, status: usize, charge: Option<HeapCharge>) -> Self {
        Self {
            object,
            access,
            status: AtomicUsize::new(status & o::SETTABLE),
            path_only: false,
            sync: false,
            offset: Mutex::new(0),
            _charge: charge,
            flocked: AtomicBool::new(false),
        }
    }

    /// It's about to take a `flock` lock.
    pub(super) fn mark_flocked(&self) {
        self.flocked.store(true, Ordering::Release);
    }

    pub(super) fn readable(&self) -> bool {
        self.access != o::WRONLY && !self.path_only
    }

    pub(super) fn writable(&self) -> bool {
        self.access != o::RDONLY && !self.path_only
    }

    /// The canonical path it was opened by, for a file, directory or link.
    pub(super) fn path(&self) -> Option<&str> {
        match &self.object {
            Object::File { path, .. } | Object::Directory { path, .. } | Object::Link { path } => Some(path),
            _ => None,
        }
    }

    fn nonblocking(&self) -> bool {
        self.status.load(Ordering::Relaxed) & o::NONBLOCK != 0
    }

    fn append(&self) -> bool {
        self.status.load(Ordering::Relaxed) & o::APPEND != 0
    }

    /// What `F_GETFL` says: the access mode and status flags as opened or since set.
    fn flags(&self) -> usize {
        let directory = if matches!(self.object, Object::Directory { .. }) { o::DIRECTORY } else { 0 };
        let path_only = if self.path_only { o::PATH } else { 0 };
        self.access | self.status.load(Ordering::Relaxed) | directory | path_only
    }
}

impl Drop for Ofd {
    /// The last reference has gone, in task context: whatever was open closes.
    fn drop(&mut self) {
        if *self.flocked.get_mut() {
            lock_calls::description_closed(self);
        }
        match &mut self.object {
            Object::PipeRead(id) => pipe::close_owned(*id, true),
            Object::PipeWrite(id) => pipe::close_owned(*id, false),
            Object::File { id, entry, writer, .. } => {
                if let Some(entry) = entry.take() {
                    if *writer {
                        entry.close_writer();
                    }
                    // A file's last close writes back what its shared mappings dirtied;
                    // nothing can be told of a failure.
                    if entry.dirty_pages() > 0 {
                        let _ = pagecache::write_back_id(*id);
                    }
                }
                // This may have been the last hold on a file unlinked while in use.
                fs_calls::sweep_orphans();
            }
            Object::Stream(_) | Object::Console | Object::Directory { .. } | Object::Link { .. } => {}
        }
    }
}

/// A process's descriptors, its working directory and its umask.
pub struct Files {
    pub table: FdTable<OfdRef>,
    /// Canonical, as `GETCWD` gives it.
    cwd: String,
    /// What `OPENAT` and `MKDIRAT` take out of a new file's mode.
    pub umask: u32,
}

impl Files {
    /// A table with the standard streams as descriptors 0 to 2, `cwd` and `umask`. `None`
    /// if there's no heap for it.
    pub fn new(cwd: &str, umask: u32) -> Option<Self> {
        let mut path = String::new();
        path.try_reserve_exact(cwd.len()).ok()?;
        path.push_str(cwd);
        let mut table = FdTable::new(MAX_FDS);
        for (stream, access) in [(0, o::RDONLY), (1, o::WRONLY), (2, o::WRONLY)] {
            let ofd = TryArc::new(Ofd::value(Object::Stream(stream), access, 0, None)).ok()?;
            table.insert(ofd, false, 0).ok()?;
        }
        Some(Self { table, cwd: path, umask })
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// A copy for a child `SPAWN2` makes: the same descriptions, close-on-exec flags,
    /// working directory and umask. `ENOMEM` if there's no heap for it.
    pub fn try_clone(&self) -> Result<Self, isize> {
        let table = self.table.try_clone().map_err(|_| -errno::ENOMEM)?;
        let mut cwd = String::new();
        cwd.try_reserve_exact(self.cwd.len()).map_err(|_| -errno::ENOMEM)?;
        cwd.push_str(&self.cwd);
        Ok(Self { table, cwd, umask: self.umask })
    }

    /// Makes `cwd` the working directory, a canonical path the caller made.
    pub fn set_cwd(&mut self, cwd: String) -> String {
        core::mem::replace(&mut self.cwd, cwd)
    }
}

/// A descriptor argument: a 32-bit value, as section 1 of the design reads one, and not
/// negative. `None` is `EBADF`.
pub(super) fn fd_arg(raw: usize) -> Option<usize> {
    int_arg(raw).and_then(|fd| usize::try_from(fd).ok())
}

/// The description `fd` refers to, the table let go again.
pub(super) fn lookup(process: &Process, fd: usize) -> Result<OfdRef, isize> {
    let fd = fd_arg(fd).ok_or(-errno::EBADF)?;
    process.files().table.get(fd).cloned().ok_or(-errno::EBADF)
}

/// What a table error is called across the boundary, for a call that adds descriptors:
/// a table that can't grow is as full as one that's at its limit.
const fn table_errno(error: FdError) -> isize {
    -match error {
        FdError::BadFd => errno::EBADF,
        FdError::Invalid => errno::EINVAL,
        FdError::Full | FdError::NoMemory => errno::EMFILE,
    }
}

/// Readies a user buffer the kernel is about to copy a pipe's or the console's bytes
/// into, before anything is taken from it: a bad buffer fails with nothing lost.
fn ready(process: &Process, address: usize, length: usize, write: bool) -> Result<(), isize> {
    let user = process.mm();
    if !user.in_window(address, length) {
        return Err(-errno::EFAULT);
    }
    user.fault_in(address, length.min(IO_CHUNK), write)
}

/// `read`, of a pipe's read end: what's there, as soon as there's anything.
fn pipe_read(process: &Process, id: PipeId, address: usize, length: usize, nonblocking: bool) -> isize {
    if length == 0 {
        return 0;
    }
    if let Err(error) = ready(process, address, length, true) {
        return error;
    }
    let length = length.min(IO_CHUNK);
    let mut bounce = [0u8; PAGE_SIZE];
    let mut done = 0;
    // The first piece may wait; the rest is only what's there already.
    while done < length {
        let count = (length - done).min(PAGE_SIZE);
        let now_only = nonblocking || done > 0;
        let moved = match pipe::read_some(id, &mut bounce[..count], now_only) {
            // A kill, or a signal: only the first piece waits, so nothing has moved.
            Err(pipe::Killed) => return sched::interrupted_result(),
            Ok(Transfer::Moved(moved)) => moved,
            Ok(Transfer::WouldBlock) if done == 0 => return -errno::EAGAIN,
            Ok(_) => break,
        };
        // Copied out with nothing held: a page another thread unmapped since loses what
        // was read for it, as on Linux.
        if let Err(error) = process.mm().copy_out(address + done, &bounce[..moved]) {
            return if done == 0 { error } else { done as isize };
        }
        done += moved;
        if moved < count {
            break;
        }
    }
    done as isize
}

// A write that goes in one piece is bounced whole, in one page.
const _: () = assert!(PIPE_BUF <= PAGE_SIZE);

/// `write`, to a pipe's write end: a write of at most `PIPE_BUF` in one piece; a larger
/// one a page at a time, until it's all in or, without waiting, the pipe is full.
fn pipe_write(process: &Process, id: PipeId, address: usize, length: usize, nonblocking: bool) -> isize {
    if length == 0 {
        return 0;
    }
    if !process.mm().in_window(address, length) {
        return -errno::EFAULT;
    }
    let mut bounce = [0u8; PAGE_SIZE];
    let atomic = length <= PIPE_BUF;
    let mut done = 0;
    while done < length {
        // A reader that keeps up never makes this wait, where a kill would be seen: it's
        // looked for between pages instead, as a file's transfer does between chunks.
        if done > 0 && sched::kill_requested() {
            return KILLED;
        }
        let count = if atomic { length } else { (length - done).min(PAGE_SIZE) };
        if let Err(error) = process.mm().copy_in(address + done, &mut bounce[..count]) {
            return if done == 0 { error } else { done as isize };
        }
        match pipe::write_some(id, &bounce[..count], atomic, nonblocking) {
            // A kill, or a signal: what had gone in is the result, as `write`'s partial
            // count.
            Err(pipe::Killed) if done == 0 => return sched::interrupted_result(),
            Err(pipe::Killed) => break,
            Ok(Transfer::Moved(moved)) => {
                done += moved;
                if moved < count {
                    break;
                }
            }
            // A broken pipe or a full one: what went before this piece is the result.
            Ok(Transfer::Closed) if done == 0 => return -errno::EPIPE,
            Ok(Transfer::WouldBlock) if done == 0 => return -errno::EAGAIN,
            Ok(_) => break,
        }
    }
    done as isize
}

/// `write` to the console itself, a page at a time, as a kernel-linked app's print.
fn console_write(process: &Process, address: usize, length: usize) -> isize {
    with_user_buffer(process, address, length, false, |piece| {
        feos_console::write_bytes(piece);
        Ok(piece.len())
    })
}

/// Moves a whole request to or from a file, `IO_CHUNK` at a time, each piece through
/// `step` with the offset; stops at a short piece (the end of the file, a full volume) or
/// a kill. A failure part way returns what had moved.
fn whole(
    process: &Process,
    address: usize,
    length: usize,
    into_user: bool,
    mut step: impl FnMut(&mut [u8]) -> Result<usize, isize>,
) -> isize {
    if length == 0 {
        return 0;
    }
    if !process.mm().in_window(address, length) {
        return -errno::EFAULT;
    }
    let mut done = 0;
    while done < length {
        // A long transfer is the call a kill waits for: it ends here instead, and the
        // program never sees the count.
        if done > 0 && sched::kill_requested() {
            return KILLED;
        }
        let count = (length - done).min(IO_CHUNK);
        let moved = with_user_buffer(process, address + done, count, into_user, &mut step);
        if moved < 0 {
            return if done == 0 { moved } else { done as isize };
        }
        done += moved as usize;
        if (moved as usize) < count {
            break;
        }
    }
    done as isize
}

/// `read`.
pub fn sys_read(slot: usize, process: &Process, fd: usize, address: usize, length: usize) -> isize {
    let ofd = match lookup(process, fd) {
        Ok(ofd) => ofd,
        Err(error) => return error,
    };
    if !ofd.readable() {
        return -errno::EBADF;
    }
    let nonblocking = ofd.nonblocking();
    match &ofd.object {
        Object::Stream(stream) => match sched::stream_pipe(slot, *stream) {
            Some(id) => pipe_read(process, id, address, length, nonblocking),
            // No pipe is the console's own input, which programs aren't given: at end of
            // file, once the buffer is seen to be good.
            None => ready(process, address, length, true).map_or_else(|error| error, |()| 0),
        },
        Object::PipeRead(id) => pipe_read(process, *id, address, length, nonblocking),
        // The console's input isn't a program's to read: at end of file, as above.
        Object::Console => ready(process, address, length, true).map_or_else(|error| error, |()| 0),
        Object::PipeWrite(_) | Object::Link { .. } => -errno::EBADF,
        Object::Directory { .. } => -errno::EISDIR,
        Object::File { id, .. } => {
            let mut offset = ofd.offset.lock();
            let at = *offset;
            let moved = file_read(process, *id, address, length, at);
            if moved > 0 {
                *offset = at + moved as u64;
            }
            moved
        }
    }
}

/// Reads the file `id` at `at` into the program's buffer, as much as there is of the
/// request: what `READ` and `PREAD` do to a file.
fn file_read(process: &Process, id: FileId, address: usize, length: usize, at: u64) -> isize {
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    let mut offset = at;
    whole(process, address, length, true, |piece| {
        let moved = tree.read_file(id, offset, piece).map_err(fs_errno)?;
        // What shared mappings have written and not yet written back is the file as far
        // as a program's read is concerned.
        if pagecache::any_dirty() {
            pagecache::read_dirty(id, offset, &mut piece[..moved]);
        }
        offset += moved as u64;
        Ok(moved)
    })
}

/// Writes the program's buffer to the file `id` at `at`, or at its end if `append`, with
/// the file's write lock held throughout: an `O_APPEND` write's end is still the end when
/// it writes there, whoever else is writing the file. The offset after, or the error.
fn file_write(
    process: &Process,
    id: FileId,
    entry: Option<&FileRef>,
    address: usize,
    length: usize,
    at: u64,
    append: bool,
) -> (isize, u64) {
    let Some(tree) = tree() else {
        return (-errno::EIO, at);
    };
    let _writing = entry.map(FileRef::write_lock);
    let mut offset = at;
    if append {
        match tree.stat_file(id) {
            Ok(stat) => offset = stat.size,
            Err(error) => return (fs_errno(error), at),
        }
    }
    // As far as a file can go, and no further: `EFBIG` from there.
    if offset >= MAX_OFFSET && length > 0 {
        return (-errno::EFBIG, offset);
    }
    let length = length.min((MAX_OFFSET - offset).min(usize::MAX as u64) as usize);
    // A write past the end leaves a gap the filesystem fills with zeroes.
    let moved = whole(process, address, length, false, |piece| {
        let moved = tree.write_file_at(id, offset, piece).map_err(fs_errno)?;
        offset += moved as u64;
        Ok(moved)
    });
    (moved, offset)
}

/// An `O_DSYNC` or `O_SYNC` description's write is followed by an `FSYNC` of the file.
fn sync_after(ofd: &Ofd, id: FileId, moved: isize) -> isize {
    if ofd.sync && moved > 0 && fs_calls::fsync_file(id).is_err() {
        return -errno::EIO;
    }
    moved
}

/// The largest offset a file can have.
const MAX_OFFSET: u64 = i64::MAX as u64;

/// `write`.
pub fn sys_write(slot: usize, process: &Process, fd: usize, address: usize, length: usize) -> isize {
    let ofd = match lookup(process, fd) {
        Ok(ofd) => ofd,
        Err(error) => return error,
    };
    if !ofd.writable() {
        return -errno::EBADF;
    }
    let nonblocking = ofd.nonblocking();
    match &ofd.object {
        Object::Stream(stream) => match sched::stream_pipe(slot, *stream) {
            Some(id) => pipe_write(process, id, address, length, nonblocking),
            None => console_write(process, address, length),
        },
        Object::PipeWrite(id) => pipe_write(process, *id, address, length, nonblocking),
        Object::Console => console_write(process, address, length),
        Object::PipeRead(_) | Object::Directory { .. } | Object::Link { .. } => -errno::EBADF,
        Object::File { id, entry, .. } => {
            let mut offset = ofd.offset.lock();
            let (moved, after) = file_write(process, *id, entry.as_ref(), address, length, *offset, ofd.append());
            // An append that failed still moved the offset to the end, as Linux's does.
            *offset = after;
            drop(offset);
            sync_after(&ofd, *id, moved)
        }
    }
}

/// `pread` and `pwrite`: at `offset`, the description's own untouched. `pwrite` on an
/// `O_APPEND` description writes at `offset` too, as POSIX says (Linux writes at the end).
pub fn sys_pread_pwrite(process: &Process, fd: usize, address: usize, length: usize, offset: usize, write: bool) -> isize {
    let ofd = match lookup(process, fd) {
        Ok(ofd) => ofd,
        Err(error) => return error,
    };
    if ofd.path_only {
        return -errno::EBADF;
    }
    let Ok(offset) = u64::try_from(offset as i64) else {
        return -errno::EINVAL;
    };
    match &ofd.object {
        Object::Directory { .. } => -errno::EISDIR,
        Object::File { id, entry, .. } => {
            if write {
                if !ofd.writable() {
                    return -errno::EBADF;
                }
                let (moved, _) = file_write(process, *id, entry.as_ref(), address, length, offset, false);
                sync_after(&ofd, *id, moved)
            } else {
                if !ofd.readable() {
                    return -errno::EBADF;
                }
                file_read(process, *id, address, length, offset)
            }
        }
        Object::Link { .. } => -errno::EBADF,
        Object::Stream(_) | Object::Console | Object::PipeRead(_) | Object::PipeWrite(_) => -errno::ESPIPE,
    }
}

/// v1's `open`: `openat(AT_FDCWD, path, length, its flags translated, 0o666)`, with v1's
/// error numbers. A trailing slash is ignored, as it always was for v1.
pub fn sys_open(slot: usize, process: &Process, pointer: usize, length: usize, flags: usize) -> isize {
    if flags & !open_flags::ALL != 0 || flags & (open_flags::READ | open_flags::WRITE) == 0 {
        return -errno::EINVAL;
    }
    let path = match user_path(process, pointer, length) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let readable = flags & open_flags::READ != 0;
    let writable = flags & open_flags::WRITE != 0;
    let request = Request {
        access: match (readable, writable) {
            (true, true) => o::RDWR,
            (false, true) => o::WRONLY,
            _ => o::RDONLY,
        },
        // v1 made a file only to write it.
        create: flags & open_flags::CREATE != 0 && writable,
        exclusive: false,
        truncate: flags & open_flags::TRUNCATE != 0 && writable,
        status: if flags & open_flags::APPEND != 0 { o::APPEND } else { 0 },
        directory: false,
        nofollow: false,
        path_only: false,
        sync: false,
        cloexec: false,
        mode: 0o666,
    };
    let trimmed = if path::has_trailing_slash(&path) { path.trim_end_matches('/') } else { &path };
    match open(slot, process, trimmed, &request) {
        Ok(fd) => fd as isize,
        Err(Fail::Errno(error)) => error,
        Err(Fail::Fs(error)) => fs_errno(error),
    }
}

/// What an open asks for, from `OPENAT`'s flags or v1's.
pub struct Request {
    /// `O_RDONLY`, `O_WRONLY` or `O_RDWR`.
    pub access: usize,
    pub create: bool,
    pub exclusive: bool,
    pub truncate: bool,
    /// `O_APPEND` and `O_NONBLOCK`.
    pub status: usize,
    /// `O_DIRECTORY`.
    pub directory: bool,
    pub nofollow: bool,
    pub path_only: bool,
    pub sync: bool,
    pub cloexec: bool,
    /// For a new file, before the umask.
    pub mode: u32,
}

/// Opens the absolute path `path` (its trailing slash, if any, meaning what POSIX says)
/// onto a new description at the lowest free descriptor.
///
/// Everything the description needs (its heap, charged; the canonical path it will
/// remember) is had before anything is made, so nothing below has to be undone for want
/// of it but a writer's count.
pub fn open(slot: usize, process: &Process, joined: &str, request: &Request) -> Result<usize, Fail> {
    // Looked for now, so a full table fails before anything is made, and taken at the
    // end: the table isn't held through the filesystem's work.
    if !process.files().table.has_room(0, 1) {
        return Err(Fail::Errno(-errno::EMFILE));
    }
    let umask = process.files().umask;
    let ofd = open_description(slot, process, joined, request, umask)?;
    let inserted = process.files().table.insert(ofd, request.cloexec, 0);
    match inserted {
        Ok(fd) => Ok(fd),
        // Let go of, and so closed, with the table let go.
        Err((error, ofd)) => {
            drop(ofd);
            Err(Fail::Errno(table_errno(error)))
        }
    }
}

/// Opens the absolute path `path` as [`open`] does, onto a new description (or, for a
/// descriptor's link that leads nowhere a path goes, the description it names) for the
/// caller to put in a table: the process's own, or a child's being built by `SPAWN2`,
/// whose umask is `umask`. The heap it takes is charged to `process`.
pub fn open_description(slot: usize, process: &Process, joined: &str, request: &Request, umask: u32) -> Result<OfdRef, Fail> {
    let tree = tree().ok_or(Fail::Errno(-errno::EIO))?;
    // No program reaches the orphans, nor makes anything by their name.
    if path::is_hidden(joined) {
        return Err(Fail::Fs(FsError::NotFound));
    }
    let (uid, gid) = sched::credentials(slot);
    let trailing = path::has_trailing_slash(joined);
    let writable = request.access != o::RDONLY && !request.path_only;
    // `O_CREAT | O_EXCL` never follows a last link: anything there, a dangling link too,
    // is `EEXIST`.
    let follow = !(request.nofollow || (request.create && request.exclusive));
    let found = match path::lookup(tree, joined, follow) {
        Ok(found) => Some(found),
        // A descriptor's link that leads nowhere a path can go (a pipe's): a new
        // descriptor for the same description, as Linux opens a pipe's.
        Err(FsError::NotFound) if follow && !trailing => match super::devfs::descriptor_behind(process, tree, joined) {
            Some(fd) => return open_descriptor(process, fd, request),
            None if request.create && !request.path_only => None,
            None => return Err(Fail::Fs(FsError::NotFound)),
        },
        Err(FsError::NotFound) if request.create && !request.path_only => None,
        Err(error) => return Err(Fail::Fs(error)),
    };
    if let Some(found) = &found {
        if request.create && request.exclusive && !request.path_only {
            return Err(Fail::Fs(FsError::NameExists));
        }
        match found.stat.kind {
            FileKind::Symlink if !request.path_only => return Err(Fail::Errno(-errno::ELOOP)),
            FileKind::Directory if writable || (request.create && !request.path_only) => {
                return Err(Fail::Fs(FsError::IsDirectory));
            }
            FileKind::File | FileKind::Symlink if request.directory => return Err(Fail::Fs(FsError::NotDirectory)),
            _ => {}
        }
        // A descriptor's link that leads to the console (`/dev/stdout`, `/dev/fd/1`, a link
        // to one of those) reaches the descriptor, not the node: a new descriptor for the
        // same description, as Linux's do. The caller holds that description already, so
        // the node's own bits (`0o600`, root's) aren't what decides; the description's
        // access is. `/dev/console` itself, opened by name, is judged by the node.
        if follow && !request.path_only && found.stat.device == Some(DeviceKind::Console) {
            if let Some(fd) = super::devfs::descriptor_behind(process, tree, joined) {
                return open_descriptor(process, fd, request);
            }
        }
        // Getting there takes search permission on the way; using it, the file's bits.
        let mut want = 0;
        if !request.path_only {
            if request.access != o::WRONLY {
                want |= ACCESS_READ;
            }
            if writable {
                want |= ACCESS_WRITE;
            }
        }
        // An `O_PATH | O_NOFOLLOW` of a link refers to the link: getting to it is all.
        if found.stat.kind == FileKind::Symlink {
            check_reach_link(tree, &found.canonical, uid, gid).map_err(Fail::Fs)?;
        } else {
            tree.check_access(joined, uid, gid, want).map_err(Fail::Fs)?;
        }
        // A process's terminal is stage 1c.11's: until then none has one.
        if found.stat.device == Some(DeviceKind::Tty) && !request.path_only {
            return Err(Fail::Errno(-errno::ENXIO));
        }
    }
    // The canonical path, which a new file's parent says.
    let canonical = match &found {
        Some(found) => copy(&found.canonical)?,
        None => {
            // `open("new/", O_CREAT)` can only be a directory, which this doesn't make.
            if trailing {
                return Err(Fail::Fs(FsError::IsDirectory));
            }
            let name = path::last_component(joined).ok_or(Fail::Fs(FsError::IsDirectory))?;
            let parent = &joined[..joined.len() - name.len()];
            let parent = path::lookup(tree, if parent.is_empty() { "/" } else { parent }, true).map_err(Fail::Fs)?;
            if parent.stat.kind != FileKind::Directory {
                return Err(Fail::Fs(FsError::NotDirectory));
            }
            path::join(&parent.canonical, name).map_err(Fail::Fs)?
        }
    };
    // The description's heap is charged and taken now, so nothing made below (a new file,
    // a writer's count) has to be undone for want of it.
    let charge = process.charge_heap(ofd_heap(Some(canonical.len()))).map_err(Fail::Errno)?;
    let room = OfdRef::reserve().ok_or(Fail::Errno(-errno::ENOMEM))?;
    let remembered = Path::new(canonical).map_err(|_| Fail::Errno(-errno::ENOMEM))?;
    let found = match found {
        Some(found) => found,
        None => {
            check_parent(tree, &remembered, uid, gid).map_err(Fail::Errno)?;
            // Judged, made, owned and given its mode in one step, and opened as what was
            // made: by its path again, a parent swapped for a link in between would have
            // the owner, the mode and the open land on another file.
            let mode = (request.mode & 0o7777 & !umask) as u16;
            tree.create_file_as(&remembered, mode, Caller { uid, gid }).map_err(Fail::Fs)?
        }
    };
    let object = match found.stat.kind {
        FileKind::Directory => Object::Directory { path: remembered, id: found.id, listing: Mutex::new(None) },
        FileKind::Symlink => Object::Link { path: remembered },
        FileKind::File if found.stat.device == Some(DeviceKind::Console) && !request.path_only => Object::Console,
        FileKind::File => {
            // Every description holds the file's entry: what makes a file in use an
            // orphan when it's unlinked. A device the cache won't keep needs none.
            let entry = match pagecache::file_for(found.id, found.stat.size) {
                Ok(entry) => Some(entry),
                Err(_) if found.stat.device.is_some() => None,
                Err(error) => return Err(Fail::Fs(error)),
            };
            // A writer is counted in the entry, which refuses it while any of the file is
            // mapped to run, and refuses mapping it to run while it's open.
            if writable {
                if let Some(entry) = &entry {
                    entry.open_writer().map_err(|_| Fail::Errno(-errno::ETXTBSY))?;
                }
            }
            if request.truncate && writable {
                // Under the file's write lock, as a write is: not in the middle of another's.
                let writing = entry.as_ref().map(FileRef::write_lock);
                let truncated = tree.truncate_file(found.id, 0);
                drop(writing);
                if let Err(error) = truncated {
                    if let Some(entry) = &entry {
                        entry.close_writer();
                    }
                    return Err(Fail::Fs(error));
                }
            }
            Object::File { path: remembered, id: found.id, entry, writer: writable }
        }
    };
    // `O_PATH` keeps only `O_CLOEXEC`, `O_DIRECTORY` and `O_NOFOLLOW`, as on Linux.
    let (access, status) = if request.path_only { (o::RDONLY, 0) } else { (request.access, request.status) };
    let mut ofd = Ofd::value(object, access, status, Some(charge));
    ofd.path_only = request.path_only;
    ofd.sync = request.sync && !request.path_only;
    Ok(room.fill(ofd))
}

/// Descriptor `fd`'s description again, for a new descriptor: what opening
/// `/proc/self/fd/<fd>` does when it leads nowhere a path goes. The description has to
/// allow what the open asks for (`EACCES`), and be a directory if it must.
fn open_descriptor(process: &Process, fd: usize, request: &Request) -> Result<OfdRef, Fail> {
    let files = process.files();
    let ofd = files.table.get(fd).ok_or(Fail::Fs(FsError::NotFound))?;
    if request.directory && !matches!(ofd.object, Object::Directory { .. }) {
        return Err(Fail::Fs(FsError::NotDirectory));
    }
    if !request.path_only {
        let wants_read = request.access != o::WRONLY;
        let wants_write = request.access != o::RDONLY;
        if (wants_read && !ofd.readable()) || (wants_write && !ofd.writable()) {
            return Err(Fail::Errno(-errno::EACCES));
        }
    }
    Ok(ofd.clone())
}

/// A copy of `text` on the heap, fallibly.
fn copy(text: &str) -> Result<String, Fail> {
    let mut out = String::new();
    out.try_reserve_exact(text.len()).map_err(|_| Fail::Errno(-errno::ENOMEM))?;
    out.push_str(text);
    Ok(out)
}

/// `close`.
pub fn sys_close(process: &Process, fd: usize) -> isize {
    let Some(fd) = fd_arg(fd) else {
        return -errno::EBADF;
    };
    // Out of the table first; what closing needs done is done with the table let go.
    let removed = process.files().table.remove(fd);
    match removed {
        Ok(ofd) => {
            lock_calls::descriptor_closed(process, &ofd);
            drop(ofd);
            0
        }
        Err(_) => -errno::EBADF,
    }
}

/// `seek`: a file's offset, or a directory's cookie. A pipe or the console has none.
pub fn sys_seek(process: &Process, fd: usize, offset: isize, whence: usize) -> isize {
    let ofd = match lookup(process, fd) {
        Ok(ofd) => ofd,
        Err(error) => return error,
    };
    if ofd.path_only {
        return -errno::EBADF;
    }
    let id = match &ofd.object {
        Object::File { id, .. } => *id,
        Object::Directory { path, listing, .. } => return seek_directory(process, &ofd, path, listing, offset, whence),
        Object::Link { .. } => return -errno::EBADF,
        _ => return -errno::ESPIPE,
    };
    let mut position = ofd.offset.lock();
    let base = match whence {
        seek_whence::SET => 0,
        seek_whence::CURRENT => *position,
        seek_whence::END => {
            let Some(tree) = tree() else {
                return -errno::EIO;
            };
            match tree.stat_file(id) {
                Ok(stat) => stat.size,
                Err(error) => return fs_errno(error),
            }
        }
        _ => return -errno::EINVAL,
    };
    match base.checked_add_signed(offset as i64) {
        Some(target) if target <= isize::MAX as u64 => {
            *position = target;
            target as isize
        }
        _ => -errno::EINVAL,
    }
}

/// `seek` on a directory, in `GETDENTS` cookies (section 3.3 of the ABI design, the way
/// relibc's `telldir`, `seekdir` and `posix_getdents` use it): `SEEK_CUR` 0 says where the
/// next entry is; `SEEK_SET` 0 rewinds and drops the snapshot, and to a cookie places
/// the next read there, taking a snapshot first if there's none. A cookie past the
/// snapshot's end, a negative one, any other `SEEK_CUR` and `SEEK_END` are `EINVAL`.
fn seek_directory(process: &Process, ofd: &Ofd, path: &str, listing: &Mutex<Option<Listing>>, offset: isize, whence: usize) -> isize {
    let mut position = ofd.offset.lock();
    match whence {
        seek_whence::CURRENT if offset == 0 => *position as isize,
        seek_whence::SET if offset == 0 => {
            *position = 0;
            let dropped = listing.lock().take();
            drop(dropped);
            0
        }
        seek_whence::SET if offset > 0 => {
            let mut listing = listing.lock();
            if listing.is_none() {
                let Some(tree) = tree() else {
                    return -errno::EIO;
                };
                match Listing::take(process, tree, path) {
                    Ok(taken) => *listing = Some(taken),
                    Err(error) => return error,
                }
            }
            let entries = listing.as_ref().map_or(0, |listing| listing.snapshot.len());
            if offset as usize > entries {
                return -errno::EINVAL;
            }
            *position = offset as u64;
            offset
        }
        _ => -errno::EINVAL,
    }
}

/// `pipe2`, and v1's `pipe` as `pipe2(fds, 0)`: a pipe, its ends owned by two new
/// descriptions, at the two lowest free descriptors.
pub fn sys_pipe2(process: &Process, out: usize, flags: usize) -> isize {
    if flags & !(o::CLOEXEC | o::NONBLOCK) != 0 {
        return -errno::EINVAL;
    }
    // Readied for writing first, so a bad pointer fails before there's a pipe to undo.
    if !process.mm().in_window(out, 8) || process.mm().fault_in(out, 8, true).is_err() {
        return -errno::EFAULT;
    }
    if !process.files().table.has_room(0, 2) {
        return -errno::EMFILE;
    }
    // Both descriptions' heap first: a pipe made has nothing but them to close it.
    let (Ok(read_charge), Ok(write_charge)) = (process.charge_heap(ofd_heap(None)), process.charge_heap(ofd_heap(None))) else {
        return -errno::ENOMEM;
    };
    let (Some(read_room), Some(write_room)) = (OfdRef::reserve(), OfdRef::reserve()) else {
        return -errno::ENOMEM;
    };
    let id = match pipe::create_owned_for_program() {
        Ok(id) => id,
        Err(pipe::CreateError::ProcessCap | pipe::CreateError::PoolFull) => return -errno::ENFILE,
        Err(pipe::CreateError::NoMemory) => return -errno::ENOMEM,
    };
    let cloexec = flags & o::CLOEXEC != 0;
    let status = flags & o::NONBLOCK;
    let reader = read_room.fill(Ofd::value(Object::PipeRead(id), o::RDONLY, status, Some(read_charge)));
    let writer = write_room.fill(Ofd::value(Object::PipeWrite(id), o::WRONLY, status, Some(write_charge)));
    // Kept to tell them by, should the descriptors have to go again.
    let (ours_read, ours_write) = (reader.clone(), writer.clone());
    let mut files = process.files();
    let read_fd = match files.table.insert(reader, cloexec, 0) {
        Ok(fd) => fd,
        Err((error, reader)) => {
            drop(files);
            drop((reader, writer));
            return table_errno(error);
        }
    };
    let write_fd = match files.table.insert(writer, cloexec, 0) {
        Ok(fd) => fd,
        Err((error, writer)) => {
            let reader = files.table.remove(read_fd);
            drop(files);
            drop((reader, writer));
            return table_errno(error);
        }
    };
    drop(files);
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&(read_fd as u32).to_ne_bytes());
    bytes[4..].copy_from_slice(&(write_fd as u32).to_ne_bytes());
    if process.mm().copy_out(out, &bytes).is_err() {
        // Another thread unmapped it since: the descriptors go again, as if never made.
        // Another thread may have closed one meanwhile and been given its number for
        // something else, so a number goes only if it still names this call's end.
        let ends = {
            let mut files = process.files();
            let mut take = |fd: usize, ours: &OfdRef| match files.table.get(fd) {
                Some(now) if TryArc::ptr_eq(now, ours) => files.table.remove(fd).ok(),
                _ => None,
            };
            (take(read_fd, &ours_read), take(write_fd, &ours_write))
        };
        drop(ends);
        return -errno::EFAULT;
    }
    0
}

/// `dup3`.
pub fn sys_dup3(process: &Process, old: usize, new: usize, flags: usize) -> isize {
    if flags & !o::CLOEXEC != 0 {
        return -errno::EINVAL;
    }
    let (Some(old), Some(new)) = (fd_arg(old), fd_arg(new)) else {
        return -errno::EBADF;
    };
    let replaced = process.files().table.dup3(old, new, flags & o::CLOEXEC != 0);
    match replaced {
        // What `new` referred to closes now, with the table let go; its errors are
        // nobody's, as POSIX says.
        Ok(closed) => {
            if let Some(closed) = &closed {
                lock_calls::descriptor_closed(process, closed);
            }
            drop(closed);
            new as isize
        }
        Err(error) => table_errno(error),
    }
}

/// `fcntl`; its record locks are [`lock_calls`]'.
pub fn sys_fcntl(process: &Process, fd: usize, command: usize, arg: usize) -> isize {
    let Some(fd) = fd_arg(fd) else {
        return -errno::EBADF;
    };
    let Some(command) = int_arg(command).and_then(|command| usize::try_from(command).ok()) else {
        return -errno::EINVAL;
    };
    let mut files = process.files();
    let Some(ofd) = files.table.get(fd).cloned() else {
        return -errno::EBADF;
    };
    // Record locks may wait, and copy a `Flock`: with the table let go.
    if matches!(command, f::GETLK | f::SETLK | f::SETLKW) {
        drop(files);
        let result = lock_calls::sys_fcntl_lock(process, fd, &ofd, command, arg);
        drop(ofd);
        return result;
    }
    let result = match command {
        f::DUPFD | f::DUPFD_CLOEXEC => {
            let Some(from) = int_arg(arg).and_then(|from| usize::try_from(from).ok()).filter(|&from| from < MAX_FDS) else {
                return -errno::EINVAL;
            };
            match files.table.dup_from(fd, from, command == f::DUPFD_CLOEXEC) {
                Ok(new) => new as isize,
                Err(error) => table_errno(error),
            }
        }
        f::GETFD => isize::from(files.table.cloexec(fd) == Some(true)),
        f::SETFD => match int_arg(arg) {
            Some(value) if value as u32 as usize & !f::FD_CLOEXEC == 0 => {
                let _ = files.table.set_cloexec(fd, value as u32 as usize & f::FD_CLOEXEC != 0);
                0
            }
            _ => -errno::EINVAL,
        },
        f::GETFL => ofd.flags() as isize,
        f::SETFL if ofd.path_only => -errno::EBADF,
        f::SETFL => match int_arg(arg) {
            // Only what can change does; what's fixed at open is ignored, and anything
            // else (`O_ASYNC`, undefined bits) refused.
            Some(value) if value as u32 as usize & !(o::SETTABLE | o::FIXED_AT_OPEN) == 0 => {
                ofd.status.store(value as u32 as usize & o::SETTABLE, Ordering::Relaxed);
                0
            }
            _ => -errno::EINVAL,
        },
        _ => -errno::EINVAL,
    };
    drop(files);
    drop(ofd);
    result
}

/// `close_range`.
pub fn sys_close_range(process: &Process, first: usize, last: usize, flags: usize) -> isize {
    // Both unsigned 32-bit values: `u32::MAX` (C's `~0U`) is the largest there is.
    let (Some(first), Some(last)) = (int_arg(first), int_arg(last)) else {
        return -errno::EINVAL;
    };
    let (first, last) = (first as u32 as usize, last as u32 as usize);
    let cloexec = match flags {
        0 => false,
        close_range::CLOEXEC => true,
        // `CLOSE_RANGE_UNSHARE` too: a table is never shared between processes.
        _ => return -errno::EINVAL,
    };
    let mut closed = Vec::new();
    let result = {
        let mut files = process.files();
        // Room for all of them first, so nothing is allocated, or fails, part way.
        if closed.try_reserve_exact(files.table.open_count()).is_err() {
            return -errno::ENOMEM;
        }
        files.table.close_range(first, last, cloexec, |ofd| closed.push(ofd))
    };
    for ofd in &closed {
        lock_calls::descriptor_closed(process, ofd);
    }
    drop(closed);
    match result {
        Ok(()) => 0,
        Err(error) => table_errno(error),
    }
}

/// What `fd` is, for `mmap2`: the file's identity, so the table isn't held while the
/// space is.
pub fn map_source(process: &Process, fd: usize) -> MapFd {
    let files = process.files();
    match files.table.get(fd) {
        None => MapFd::Closed,
        // Not a descriptor anything can be done with but look.
        Some(ofd) if ofd.path_only => MapFd::Closed,
        Some(ofd) => match (&ofd.object, ofd.readable()) {
            (Object::File { .. }, false) => MapFd::Unreadable,
            (Object::File { id, .. }, true) => MapFd::File { id: *id, writable: ofd.writable() },
            _ => MapFd::NotMappable,
        },
    }
}

/// What v1's `spawn` gives a child as a stream from descriptor `fd`: a pipe's raw id, or
/// zero to inherit this process's own. `None` is `EBADF`: nothing a stream can be.
pub fn spawn_stream(process: &Process, fd: usize, read: bool) -> Option<u32> {
    let files = process.files();
    let ofd = files.table.get(fd)?;
    if ofd.path_only {
        return None;
    }
    match (&ofd.object, read) {
        (Object::PipeRead(id), true) | (Object::PipeWrite(id), false) => Some(id.raw()),
        (Object::Stream(0), true) | (Object::Stream(1), false) => Some(0),
        _ => None,
    }
}
