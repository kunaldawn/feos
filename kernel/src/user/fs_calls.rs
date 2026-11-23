//! The path calls of stage 1c.2: `OPENAT`, `FSTATAT`, `GETDENTS`, `MKDIRAT`, `UNLINKAT`,
//! `FTRUNCATE`, `FSYNC`, `FCHDIR`, `GETCWD`, `UMASK`, `FD_PATH`, `CURRENT_EXE`, and the
//! orphans a file unlinked while in use becomes.
//!
//! Paths. Each is `(pointer, length)`, copied once and checked as section 1 of the ABI
//! design says (empty `ENOENT`, past `PATH_MAX` `ENAMETOOLONG`, a NUL `EINVAL`, not UTF-8
//! `EILSEQ`), then joined to the directory it's relative to: the working directory or a
//! `dirfd`, both remembered as canonical paths, so `..` is physical. Resolution is the
//! tree's walk with POSIX's trailing slashes on top (`feos_fs::vfs::path`), and errors
//! take the v2 mapping ([`errno_of`]), where v1's calls keep theirs.
//!
//! Orphans. A program's last name for a regular file goes while the file is still held
//! (its lock-table entry referenced by a description, a mapping or a running program's
//! text, beyond the page cache's own reference) by renaming it into the mount's
//! `.feos-orphans` instead of removing it (`VfsTree::orphan`), and noting it here. Every
//! place a hold can go ends with [`sweep_orphans`] (each system call, each description's
//! last close, each address space's end), which removes the orphans nothing holds any
//! more. One a crash leaves goes when its mount is next made.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use feos_abi::{at, dt, errno, int_arg, o, s_if, stat_flags, Dirent, Stat2, Timespec, CONSOLE_RDEV, FSYNC_DATA, PATH_MAX};
use feos_fs::vfs::{path, Caller, DeviceKind, FileId, Resolved, Stat, VfsTree, ACCESS_EXECUTE, ACCESS_WRITE};
use feos_fs::{FileKind, FsError, Timestamp};
use feos_sync::Mutex;

use super::devfs;
use super::files::{self, Listing, Object, Request};
use super::process::Process;
use super::{check_reach_link, tree};
use crate::{pagecache, sched};

/// Why an open failed: a filesystem error, mapped by the caller's version of the ABI, or
/// an error number already.
pub enum Fail {
    Fs(FsError),
    Errno(isize),
}

/// What a filesystem error is called across the boundary, for the v2 calls (section 3.3
/// of the ABI design).
pub fn errno_of(error: FsError) -> isize {
    -match error {
        FsError::NotFound => errno::ENOENT,
        FsError::NameExists => errno::EEXIST,
        FsError::NotDirectory => errno::ENOTDIR,
        FsError::IsDirectory => errno::EISDIR,
        FsError::NotEmpty => errno::ENOTEMPTY,
        FsError::PermissionDenied => errno::EACCES,
        FsError::ReadOnly => errno::EROFS,
        FsError::NoSpace => errno::ENOSPC,
        FsError::OutOfMemory => errno::ENOMEM,
        FsError::Unsupported => errno::EOPNOTSUPP,
        FsError::Busy => errno::EBUSY,
        FsError::InvalidName => errno::EINVAL,
        FsError::SymlinkLoop => errno::ELOOP,
        FsError::TextBusy => errno::ETXTBSY,
        FsError::NotPermitted => errno::EPERM,
        FsError::CrossDevice => errno::EXDEV,
        FsError::InvalidMove => errno::EINVAL,
        _ => errno::EIO,
    }
}

pub(super) fn failed(error: Fail) -> isize {
    match error {
        Fail::Fs(error) => errno_of(error),
        Fail::Errno(error) => error,
    }
}

/// A v2 call's path, copied from the program once and checked, as given (not joined).
pub(super) fn user_path(process: &Process, pointer: usize, length: usize) -> Result<String, isize> {
    if length == 0 {
        return Err(-errno::ENOENT);
    }
    if length > PATH_MAX {
        return Err(-errno::ENAMETOOLONG);
    }
    if !process.mm().in_window(pointer, length) {
        return Err(-errno::EFAULT);
    }
    let mut given = Vec::new();
    given.try_reserve_exact(length).map_err(|_| -errno::ENOMEM)?;
    given.resize(length, 0);
    process.mm().copy_in(pointer, &mut given)?;
    if given.contains(&0) {
        return Err(-errno::EINVAL);
    }
    let given = String::from_utf8(given).map_err(|_| -errno::EILSEQ)?;
    // Nothing by such a name can be made, so none can be found either.
    if path::has_long_name(&given) {
        return Err(-errno::ENAMETOOLONG);
    }
    Ok(given)
}

/// `given` made absolute: relative to the working directory for `AT_FDCWD`, or to the
/// directory `dirfd` refers to (`EBADF`, `ENOTDIR`). An absolute path ignores `dirfd`.
pub(super) fn joined(process: &Process, dirfd: usize, given: &str) -> Result<String, isize> {
    if given.starts_with('/') {
        return path::join("/", given).map_err(errno_of);
    }
    match int_arg(dirfd) {
        Some(at::FDCWD) => path::join(process.files().cwd(), given).map_err(errno_of),
        Some(_) => {
            let ofd = files::lookup(process, dirfd)?;
            match &ofd.object {
                Object::Directory { path, .. } => path::join(path, given).map_err(errno_of),
                _ => Err(-errno::ENOTDIR),
            }
        }
        None => Err(-errno::EBADF),
    }
}

/// A path argument, copied, checked and made absolute.
pub(super) fn path_at(process: &Process, dirfd: usize, pointer: usize, length: usize) -> Result<String, isize> {
    let given = user_path(process, pointer, length)?;
    joined(process, dirfd, &given)
}

/// May this user add to or take from the directory `path` is in?
pub(super) fn check_parent(tree: &dyn VfsTree, path: &str, uid: u32, gid: u32) -> Result<(), isize> {
    let trimmed = path.trim_end_matches('/');
    let parent = match trimmed.rfind('/') {
        Some(0) | None => "/",
        Some(at) => &trimmed[..at],
    };
    tree.check_access(parent, uid, gid, ACCESS_WRITE | ACCESS_EXECUTE).map_err(errno_of)
}

/// Copies `data` to the program at `address`: a path-returning call's answer, which
/// must fit in `length` (`ERANGE`, `length` zero included). Its length, or the error.
fn give_path(process: &Process, address: usize, length: usize, data: &str) -> isize {
    if length == 0 || data.len() > length {
        return -errno::ERANGE;
    }
    match process.mm().copy_out(address, data.as_bytes()) {
        Ok(()) => data.len() as isize,
        Err(error) => error,
    }
}

/// `openat`.
pub fn sys_openat(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    let [dirfd, pointer, length, flags, mode, _] = args;
    let Some(flags) = int_arg(flags).map(|flags| flags as u32 as usize) else {
        return -errno::EINVAL;
    };
    if flags & o::TMPFILE == o::TMPFILE {
        return -errno::EOPNOTSUPP;
    }
    if flags & !o::OPENAT_KNOWN != 0 || flags & o::ACCMODE == o::ACCMODE {
        return -errno::EINVAL;
    }
    // A file made to be a directory can't be either; Linux refuses the pair outright, as
    // of 6.4, whatever is there.
    if flags & (o::CREAT | o::DIRECTORY) == o::CREAT | o::DIRECTORY {
        return -errno::EINVAL;
    }
    let Some(mode) = int_arg(mode) else {
        return -errno::EINVAL;
    };
    let joined = match path_at(process, dirfd, pointer, length) {
        Ok(joined) => joined,
        Err(error) => return error,
    };
    let path_only = flags & o::PATH != 0;
    let request = Request {
        access: flags & o::ACCMODE,
        create: flags & o::CREAT != 0 && !path_only,
        exclusive: flags & o::EXCL != 0,
        truncate: flags & o::TRUNC != 0 && !path_only,
        status: flags & o::SETTABLE,
        directory: flags & o::DIRECTORY != 0,
        nofollow: flags & o::NOFOLLOW != 0,
        path_only,
        sync: flags & o::DSYNC != 0,
        cloexec: flags & o::CLOEXEC != 0,
        mode: mode as u32,
    };
    match files::open(slot, process, &joined, &request) {
        Ok(fd) => fd as isize,
        Err(error) => failed(error),
    }
}

/// A [`Stat2`] from the tree's facts.
pub(super) fn stat2(stat: &Stat, link_length: Option<u64>) -> Stat2 {
    let mut mode = u32::from(stat.mode);
    if stat.device.is_some() {
        mode = s_if::CHR | (mode & 0o7777);
    }
    let size = link_length.unwrap_or(stat.size);
    // Stored as nanoseconds since the epoch, which 64 bits hold to 2554: the seconds fit.
    let time = |at: Option<Timestamp>| at.map_or(Timespec::default(), |at| Timespec { sec: at.secs() as i64, nsec: i64::from(at.subsec_nanos()) });
    let mut flags = 0;
    if stat.posix {
        flags |= stat_flags::POSIX;
    }
    if stat.read_only {
        flags |= stat_flags::READ_ONLY;
    }
    if stat.times.created.is_some() {
        flags |= stat_flags::BTIME;
    }
    Stat2 {
        dev: stat.dev,
        ino: stat.ino,
        mode,
        nlink: stat.nlink,
        uid: stat.uid,
        gid: stat.gid,
        rdev: stat.device.map_or(0, DeviceKind::rdev),
        size: size.min(i64::MAX as u64) as i64,
        blksize: 4096,
        blocks: (size.div_ceil(4096) * 8).min(i64::MAX as u64) as i64,
        atime: time(stat.times.accessed.or(stat.times.modified)),
        mtime: time(stat.times.modified),
        // FAT and exFAT keep no status change time: the modification time stands in.
        ctime: time(stat.times.changed.or(stat.times.modified)),
        btime: time(stat.times.created),
        flags,
        reserved0: 0,
        reserved1: 0,
    }
}

/// What a pipe says of itself.
fn pipe_stat(slot: usize, id: u32) -> Stat2 {
    let (uid, gid) = sched::credentials(slot);
    Stat2 { mode: s_if::FIFO | 0o600, ino: u64::from(id), nlink: 1, uid, gid, blksize: 4096, ..Stat2::default() }
}

/// What the console says of itself: the `/dev/console` node's facts, so a descriptor of
/// it answers as `FSTATAT` of the path does (its mode, owner, `st_dev` and `st_ino`).
/// Without a `/dev` in the tree, the same mode and number, owned by root.
fn console_stat(tree: &dyn VfsTree) -> Stat2 {
    match tree.stat("/dev/console") {
        Ok(stat) if stat.device == Some(DeviceKind::Console) => stat2(&stat, None),
        _ => Stat2 { mode: s_if::CHR | 0o600, ino: 1, rdev: CONSOLE_RDEV, nlink: 1, blksize: 4096, ..Stat2::default() },
    }
}

/// What a terminal's descriptor is: `/dev/tty`'s node.
fn terminal_stat(tree: &dyn VfsTree) -> Stat2 {
    match tree.stat("/dev/tty") {
        Ok(stat) if stat.device == Some(DeviceKind::Tty) => stat2(&stat, None),
        _ => Stat2 { mode: s_if::CHR | 0o666, ino: 1, rdev: 5 << 8, nlink: 1, blksize: 4096, ..Stat2::default() },
    }
}

/// The link's own facts, its size the length of what it leads to.
pub(super) fn link_stat(tree: &dyn VfsTree, path: &str, stat: &Stat) -> Stat2 {
    let length = tree.read_link(path).map(|target| target.len() as u64).ok();
    stat2(stat, length)
}

/// What the descriptor `fd` refers to, for `fstatat(fd, "", AT_EMPTY_PATH)`.
pub(super) fn stat_of_fd(slot: usize, process: &Process, tree: &dyn VfsTree, dirfd: usize) -> Result<Stat2, isize> {
    if int_arg(dirfd) == Some(at::FDCWD) {
        let cwd = path::join("/", process.files().cwd()).map_err(errno_of)?;
        return tree.stat(&cwd).map(|stat| stat2(&stat, None)).map_err(errno_of);
    }
    let ofd = files::lookup(process, dirfd)?;
    match &ofd.object {
        Object::File { id, .. } => tree.stat_file(*id).map(|stat| stat2(&stat, None)).map_err(errno_of),
        Object::Directory { path, .. } => tree.stat(path).map(|stat| stat2(&stat, None)).map_err(errno_of),
        Object::Link { path } => tree.lstat(path).map(|stat| link_stat(tree, path, &stat)).map_err(errno_of),
        Object::PipeRead(id) | Object::PipeWrite(id) => Ok(pipe_stat(slot, id.raw())),
        Object::Stream(stream) => match sched::stream_pipe(slot, *stream) {
            Some(id) => Ok(pipe_stat(slot, id.raw())),
            None => Ok(console_stat(tree)),
        },
        Object::Console => Ok(console_stat(tree)),
        Object::Terminal { .. } => Ok(terminal_stat(tree)),
    }
}

/// `fstatat`.
pub fn sys_fstatat(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    let [dirfd, pointer, length, out, flags, _] = args;
    let Some(flags) = int_arg(flags).map(|flags| flags as u32 as usize) else {
        return -errno::EINVAL;
    };
    if flags & !(at::SYMLINK_NOFOLLOW | at::EMPTY_PATH | at::NO_AUTOMOUNT) != 0 {
        return -errno::EINVAL;
    }
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    let result = if flags & at::EMPTY_PATH != 0 && length == 0 {
        stat_of_fd(slot, process, tree, dirfd)
    } else {
        path_at(process, dirfd, pointer, length).and_then(|joined| {
            let follow = flags & at::SYMLINK_NOFOLLOW == 0;
            let found = match path::lookup(tree, &joined, follow) {
                Ok(found) => found,
                // A descriptor's link that leads nowhere a path goes: the descriptor's.
                Err(FsError::NotFound) if follow && !path::has_trailing_slash(&joined) => {
                    match devfs::descriptor_behind(process, tree, &joined) {
                        Some(fd) => return stat_of_fd(slot, process, tree, fd),
                        None => return Err(errno_of(FsError::NotFound)),
                    }
                }
                Err(error) => return Err(errno_of(error)),
            };
            // Looking takes getting there, and nothing of the file itself: of a link not
            // followed, getting to the link.
            let (uid, gid) = sched::credentials(slot);
            if found.stat.kind == FileKind::Symlink {
                check_reach_link(tree, &found.canonical, uid, gid).map_err(errno_of)?;
            } else {
                tree.check_access(&joined, uid, gid, 0).map_err(errno_of)?;
            }
            Ok(if found.stat.kind == FileKind::Symlink {
                link_stat(tree, &found.canonical, &found.stat)
            } else {
                stat2(&found.stat, None)
            })
        })
    };
    match result {
        Ok(stat) => match process.mm().copy_out(out, &stat.to_bytes()) {
            Ok(()) => 0,
            Err(error) => error,
        },
        Err(error) => error,
    }
}

/// Most of the buffer one `GETDENTS` fills: about 2000 short names. A program reads on
/// for the rest.
const GETDENTS_CHUNK: usize = 64 * 1024;

/// `getdents`.
pub fn sys_getdents(process: &Process, fd: usize, buffer: usize, length: usize) -> isize {
    let ofd = match files::lookup(process, fd) {
        Ok(ofd) => ofd,
        Err(error) => return error,
    };
    if ofd.path_only {
        return -errno::EBADF;
    }
    let Object::Directory { path, listing, .. } = &ofd.object else {
        return -errno::ENOTDIR;
    };
    let length = length.min(GETDENTS_CHUNK);
    if length > 0 && !process.mm().in_window(buffer, length) {
        return -errno::EFAULT;
    }
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    let mut position = ofd.offset.lock();
    let mut listing = listing.lock();
    if listing.is_none() {
        match Listing::take(process, tree, path) {
            Ok(taken) => *listing = Some(taken),
            Err(error) => return error,
        }
    }
    let Some(snapshot) = listing.as_ref().map(|listing| &listing.snapshot) else {
        return -errno::EIO;
    };
    let mut out = Vec::new();
    if out.try_reserve_exact(length).is_err() {
        return -errno::ENOMEM;
    }
    out.resize(length, 0u8);
    let mut used = 0;
    let mut next = *position as usize;
    while let Some(entry) = snapshot.get(next) {
        let kind = match entry.kind {
            FileKind::Directory => dt::DIR,
            FileKind::Symlink => dt::LNK,
            FileKind::File if entry.device => dt::CHR,
            FileKind::File => dt::REG,
        };
        // A name no record can carry, which a filesystem made outside feos may have (a
        // RAM mount refuses them): passed over, not the end of every listing here.
        if entry.name.len() > feos_abi::NAME_MAX {
            next += 1;
            continue;
        }
        // Each record's cookie is the position of the entry after it.
        match Dirent::encode(entry.ino, (next + 1) as i64, kind, entry.name.as_bytes(), &mut out[used..]) {
            Some(record) => used += record,
            None => break,
        }
        next += 1;
    }
    if used == 0 && next < snapshot.len() {
        return -errno::EINVAL;
    }
    // Copied before the position moves: a buffer that's gone loses nothing.
    if let Err(error) = process.mm().copy_out(buffer, &out[..used]) {
        return error;
    }
    *position = next as u64;
    used as isize
}

/// `mkdirat`.
pub fn sys_mkdirat(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    let [dirfd, pointer, length, mode, ..] = args;
    let Some(mode) = int_arg(mode) else {
        return -errno::EINVAL;
    };
    let joined = match path_at(process, dirfd, pointer, length) {
        Ok(joined) => joined,
        Err(error) => return error,
    };
    if path::is_hidden(&joined) {
        return -errno::ENOENT;
    }
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    // The root, `.` and `..` are all there already.
    let name = match path::last_component(&joined) {
        Some(name) if name != "." && name != ".." => name,
        _ => return -errno::EEXIST,
    };
    // Made in the directory the parent leads to, every link followed, so one into the
    // orphans is `ENOENT` here as it is to `OPENAT`'s `O_CREAT`.
    let trimmed = joined.trim_end_matches('/');
    let parent = &trimmed[..trimmed.len() - name.len()];
    let parent = match path::lookup(tree, parent, true) {
        Ok(parent) if parent.stat.kind == FileKind::Directory => parent,
        Ok(_) => return -errno::ENOTDIR,
        Err(error) => return errno_of(error),
    };
    let made = match path::join(&parent.canonical, name) {
        Ok(made) => made,
        Err(error) => return errno_of(error),
    };
    let (uid, gid) = sched::credentials(slot);
    if let Err(error) = check_parent(tree, &made, uid, gid) {
        return error;
    }
    // Judged, made, and given its owner and mode in one step under the tree's lock, on the
    // directory its walk reaches: done by the path afterwards, a parent swapped for a link
    // meanwhile would have them land on whatever the path named by then.
    let umask = process.files().umask;
    let mode = (mode as u32 & 0o7777 & !umask) as u16;
    match tree.create_dir_as(&made, Some(mode), Caller { uid, gid }) {
        Ok(()) => 0,
        Err(error) => errno_of(error),
    }
}

/// `unlinkat`.
pub fn sys_unlinkat(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    let [dirfd, pointer, length, flags, ..] = args;
    let remove_dir = match int_arg(flags).map(|flags| flags as u32 as usize) {
        Some(0) => false,
        Some(at::REMOVEDIR) => true,
        _ => return -errno::EINVAL,
    };
    let joined = match path_at(process, dirfd, pointer, length) {
        Ok(joined) => joined,
        Err(error) => return error,
    };
    // What the last name says, before what it names.
    match (path::last_component(&joined), remove_dir) {
        (None, true) => return -errno::EBUSY,
        (Some("."), true) => return -errno::EINVAL,
        (Some(".."), true) => return -errno::ENOTEMPTY,
        (None | Some(".") | Some(".."), false) => return -errno::EISDIR,
        _ => {}
    }
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    // Never the last link followed, a slash after it or not: a slash says the name must
    // be a directory itself, so a link there is `ENOTDIR`, as on Linux, rather than the
    // directory it leads to removed.
    let trailing = path::has_trailing_slash(&joined);
    let found = match path::lookup(tree, joined.trim_end_matches('/'), false) {
        Ok(found) if trailing && found.stat.kind != FileKind::Directory => return -errno::ENOTDIR,
        Ok(found) => found,
        Err(error) => return errno_of(error),
    };
    if found.canonical == "/" {
        return -errno::EBUSY;
    }
    let (uid, gid) = sched::credentials(slot);
    // Write and search permission on the directory, and its sticky bit's say.
    if let Err(error) = tree.check_remove(&found.canonical, uid, gid) {
        return errno_of(error);
    }
    let result = match (remove_dir, found.stat.kind) {
        (true, FileKind::Directory) => tree.remove_dir(&found.canonical),
        (true, _) => Err(FsError::NotDirectory),
        (false, FileKind::Directory) => Err(FsError::IsDirectory),
        (false, _) => remove_entry(tree, &found),
    };
    match result {
        Ok(()) => 0,
        Err(error) => errno_of(error),
    }
}

/// Orphans noted, waiting for nothing to hold them: their identity and where they are.
static ORPHANS: Mutex<Vec<(FileId, String)>> = Mutex::new(Vec::new());
/// How many, so a sweep with none costs a load.
static ORPHAN_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Takes a name away: a regular file's last, while anything holds the file, by making it
/// an orphan; anything else, at once. `found` is the name, not followed.
pub fn remove_entry(tree: &dyn VfsTree, found: &Resolved) -> Result<(), FsError> {
    // No new hold between asking and removing: an open as the name goes either comes
    // first, and the file is orphaned, or after, and finds nothing. Looked at again
    // under it, for the file the name is now.
    let _removing = pagecache::removing();
    let found = &tree.resolve_path(&found.canonical, false)?;
    let regular = found.stat.kind == FileKind::File && found.stat.device.is_none();
    if !(regular && found.stat.nlink <= 1 && pagecache::held(found.id)) {
        return tree.remove_file(&found.canonical);
    }
    let at = tree.orphan(&found.canonical)?;
    note_orphan(found.id, at);
    Ok(())
}

/// Notes an orphan the tree just made of `id` at `at`, for [`sweep_orphans`] to remove
/// once nothing holds it. With no room to note it, it stays until its mount is next made,
/// which is where a crash leaves orphans too.
pub fn note_orphan(id: FileId, at: String) {
    let mut orphans = ORPHANS.lock();
    if orphans.try_reserve(1).is_ok() {
        orphans.push((id, at));
        ORPHAN_COUNT.fetch_add(1, Ordering::AcqRel);
    }
}

/// Removes every orphan nothing holds any more. Called with no lock held, wherever a hold
/// may have gone: at the end of each system call, a description's last close and an
/// address space's end.
pub fn sweep_orphans() {
    if ORPHAN_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    let Some(tree) = tree() else {
        return;
    };
    // As in `remove_entry`: nothing can take a hold between asking and removing.
    let _removing = pagecache::removing();
    let mut orphans = ORPHANS.lock();
    let mut index = 0;
    while index < orphans.len() {
        if pagecache::held(orphans[index].0) {
            index += 1;
            continue;
        }
        let (_, at) = orphans.swap_remove(index);
        ORPHAN_COUNT.fetch_sub(1, Ordering::AcqRel);
        let _ = tree.remove_file(&at);
    }
}

/// Orphans waiting, for `ktest orphans`.
pub fn orphans() -> usize {
    ORPHAN_COUNT.load(Ordering::Acquire)
}

/// What shared mappings of the file `id` dirtied, written back, then its mount synced.
pub fn fsync_file(id: FileId) -> Result<(), ()> {
    pagecache::write_back_id(id)?;
    tree().ok_or(())?.sync().map_err(|_| ())
}

/// `ftruncate`.
pub fn sys_ftruncate(process: &Process, fd: usize, length: usize) -> isize {
    let ofd = match files::lookup(process, fd) {
        Ok(ofd) => ofd,
        Err(error) => return error,
    };
    if ofd.path_only {
        return -errno::EBADF;
    }
    let Object::File { id, entry, .. } = &ofd.object else {
        return -errno::EINVAL;
    };
    if !ofd.writable() {
        return -errno::EBADF;
    }
    let Ok(length) = u64::try_from(length as i64) else {
        return -errno::EINVAL;
    };
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    // Under the file's write lock, as a write is.
    let _writing = entry.as_ref().map(feos_vm::FileRef::write_lock);
    match tree.truncate_file(*id, length) {
        Ok(()) => 0,
        Err(FsError::NoSpace) if length > u64::from(u32::MAX) => -errno::EFBIG,
        Err(error) => errno_of(error),
    }
}

/// `fsync`.
pub fn sys_fsync(process: &Process, fd: usize, flags: usize) -> isize {
    if flags != 0 && flags != FSYNC_DATA {
        return -errno::EINVAL;
    }
    let ofd = match files::lookup(process, fd) {
        Ok(ofd) => ofd,
        Err(error) => return error,
    };
    if ofd.path_only {
        return -errno::EBADF;
    }
    let synced = match &ofd.object {
        Object::File { id, .. } => fsync_file(*id),
        Object::Directory { .. } => tree().ok_or(()).and_then(|tree| tree.sync().map_err(|_| ())),
        _ => return -errno::EINVAL,
    };
    match synced {
        Ok(()) => 0,
        Err(()) => -errno::EIO,
    }
}

/// `fchdir`.
pub fn sys_fchdir(slot: usize, process: &Process, fd: usize) -> isize {
    let ofd = match files::lookup(process, fd) {
        Ok(ofd) => ofd,
        Err(error) => return error,
    };
    let Object::Directory { path, .. } = &ofd.object else {
        return -errno::ENOTDIR;
    };
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    let (uid, gid) = sched::credentials(slot);
    if let Err(error) = tree.check_access(path, uid, gid, ACCESS_EXECUTE) {
        return errno_of(error);
    }
    let cwd = match path::join("/", path) {
        Ok(cwd) => cwd,
        Err(error) => return errno_of(error),
    };
    let old = process.files().set_cwd(cwd);
    drop(old);
    0
}

/// `getcwd`.
pub fn sys_getcwd(process: &Process, address: usize, length: usize) -> isize {
    let cwd = match path::join("/", process.files().cwd()) {
        Ok(cwd) => cwd,
        Err(error) => return errno_of(error),
    };
    give_path(process, address, length, &cwd)
}

/// `umask`.
pub fn sys_umask(process: &Process, mask: usize) -> isize {
    match int_arg(mask) {
        Some(mask) if mask as u32 & !0o777 == 0 => {
            let mut files = process.files();
            let old = files.umask;
            files.umask = mask as u32;
            old as isize
        }
        _ => -errno::EINVAL,
    }
}

/// `fd_path`.
pub fn sys_fd_path(slot: usize, process: &Process, fd: usize, address: usize, length: usize) -> isize {
    let ofd = match files::lookup(process, fd) {
        Ok(ofd) => ofd,
        Err(error) => return error,
    };
    match (&ofd.object, ofd.path()) {
        (_, Some(path)) => give_path(process, address, length, path),
        (Object::Stream(stream), None) if sched::stream_pipe(slot, *stream).is_none() => {
            give_path(process, address, length, "/dev/console")
        }
        (Object::Console, None) => give_path(process, address, length, "/dev/console"),
        (Object::Terminal { .. }, None) => give_path(process, address, length, "/dev/tty"),
        _ => -errno::ENOENT,
    }
}

/// `current_exe`.
pub fn sys_current_exe(process: &Process, address: usize, length: usize) -> isize {
    give_path(process, address, length, process.exe())
}
