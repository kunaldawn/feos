//! The calls of stage 1c.4 that change the tree and its times: `RENAMEAT`, `LINKAT`,
//! `SYMLINKAT`, `READLINKAT`, `FCHMODAT`, `FCHOWNAT`, `UTIMENSAT`, `FACCESSAT`,
//! `FSTATVFS` and `SYNC`.
//!
//! Paths go as `fs_calls`' do: copied once, checked, joined to the directory they're
//! relative to, resolved with POSIX's trailing slashes, the orphans hidden. The two
//! `PathAt`s of `RENAMEAT` and `LINKAT` are each copied once, so another thread changing
//! one mid-call changes nothing the kernel looks at. A file named by a descriptor (the
//! `AT_EMPTY_PATH` forms) is changed by its identity, so one renamed or unlinked since it
//! was opened is still the one changed.
//!
//! Whatever a call changes it changes as the caller judged under the tree's lock, on what
//! the tree's own walk found: the permission to change a name (`RENAMEAT`, `LINKAT`,
//! `SYMLINKAT`), and the owner of a file whose mode or times change (`FCHMODAT`,
//! `UTIMENSAT`, done by the identity the path resolved to). Checked on a path beforehand,
//! then walked again, the answer could be about another file by then, a directory on the
//! way having been swapped for a link meanwhile. The checks made here first are for the
//! order of the errors POSIX gives.
//!
//! A rename's replaced file goes as `UNLINKAT` takes one: if something still holds it
//! (a description, a mapping, a running program's text), it's made an orphan in the same
//! step as the rename, so the name never names nothing, and swept once let go.

use alloc::string::String;

use feos_abi::{access, at, errno, int_arg, rename, st, utime, PathAt, StatVfs, Timespec, NAME_MAX};
use feos_fs::vfs::{path, permits, Caller, FileId, Resolved, Stat, VfsTree, ACCESS_WRITE};
use feos_fs::{FileKind, FsError, NodeTimes, Timestamp};

use super::files::{self, Object};
use super::fs_calls::{check_parent, errno_of, note_orphan, path_at, user_path};
use super::process::Process;
use super::{check_reach_link, tree};
use crate::{pagecache, sched, time};

/// A [`PathAt`] from the program, read once, its reserved word zero (`EINVAL`).
fn read_path_at(process: &Process, address: usize) -> Result<PathAt, isize> {
    let mut bytes = [0u8; PathAt::SIZE];
    process.mm().copy_in(address, &mut bytes)?;
    let given = PathAt::from_bytes(bytes);
    if given.reserved != 0 {
        return Err(-errno::EINVAL);
    }
    Ok(given)
}

/// A `PathAt`'s path, copied, checked and made absolute: [`path_at`] with its fields.
fn path_of(process: &Process, given: &PathAt) -> Result<String, isize> {
    let length = usize::try_from(given.length).map_err(|_| -errno::ENAMETOOLONG)?;
    path_at(process, given.dirfd as isize as usize, given.pointer as usize, length)
}

/// `path`'s directory and its last name: the directory resolved as a program's call
/// resolves one (links followed, `ENOTDIR`, the orphans hidden), and the name as given.
/// `EBUSY` for the root, `.` or `..`, which no call may take away or put anything at.
fn split<'a>(tree: &dyn VfsTree, path: &'a str) -> Result<(Resolved, &'a str), isize> {
    if path::is_hidden(path) {
        return Err(-errno::ENOENT);
    }
    let name = match path::last_component(path) {
        Some(name) if name != "." && name != ".." => name,
        _ => return Err(-errno::EBUSY),
    };
    let trimmed = path.trim_end_matches('/');
    let parent = match &trimmed[..trimmed.len() - name.len()] {
        "" => "/",
        parent => parent,
    };
    match path::lookup(tree, parent, true) {
        Ok(found) if found.stat.kind == FileKind::Directory => Ok((found, name)),
        Ok(_) => Err(-errno::ENOTDIR),
        Err(error) => Err(errno_of(error)),
    }
}

/// The canonical path of `name` in the directory `parent` resolved to.
fn within(parent: &Resolved, name: &str) -> Result<String, isize> {
    path::join(&parent.canonical, name).map_err(errno_of)
}

/// What a call that takes a path or, with `AT_EMPTY_PATH` and no path, a descriptor,
/// is to work on.
enum Target {
    /// A regular file or device by identity: a descriptor's.
    File(FileId),
    /// Anything else by its canonical path: a path's, or a descriptor's directory or link;
    /// and what that was found to be, which is what's changed.
    Path { canonical: String, link: bool, id: FileId },
}

impl Target {
    /// Its identity, by which it's changed.
    fn id(&self) -> FileId {
        match self {
            Self::File(id) | Self::Path { id, .. } => *id,
        }
    }

    /// The facts about it.
    fn stat(&self, tree: &dyn VfsTree) -> Result<Stat, isize> {
        match self {
            Self::File(id) => tree.stat_file(*id),
            Self::Path { canonical, link: true, .. } => tree.lstat(canonical),
            Self::Path { canonical, link: false, .. } => tree.stat(canonical),
        }
        .map_err(errno_of)
    }
}

/// The file `dirfd` refers to, for an `AT_EMPTY_PATH` call. Pipes and the console have
/// nothing in the tree to change: `not_file`.
fn target_of_fd(process: &Process, tree: &dyn VfsTree, dirfd: usize, not_file: isize) -> Result<Target, isize> {
    let by_path = |canonical: String, link: bool| {
        let id = tree.resolve_path(&canonical, !link).map_err(errno_of)?.id;
        Ok(Target::Path { canonical, link, id })
    };
    if int_arg(dirfd) == Some(at::FDCWD) {
        return by_path(path::join("/", process.files().cwd()).map_err(errno_of)?, false);
    }
    let ofd = files::lookup(process, dirfd)?;
    match &ofd.object {
        Object::File { id, .. } => Ok(Target::File(*id)),
        Object::Directory { path, .. } => by_path(String::from(path.as_str()), false),
        Object::Link { path } => by_path(String::from(path.as_str()), true),
        _ => Err(not_file),
    }
}

/// A path argument's target: resolved, following the last link if `follow`, and the way
/// there searchable by the caller (a link not followed needs only the way to it).
fn target_of_path(slot: usize, tree: &dyn VfsTree, joined: &str, follow: bool) -> Result<Target, isize> {
    let found = path::lookup(tree, joined, follow).map_err(errno_of)?;
    let (uid, gid) = sched::credentials(slot);
    let link = found.stat.kind == FileKind::Symlink;
    if link {
        check_reach_link(tree, &found.canonical, uid, gid).map_err(errno_of)?;
    } else {
        tree.check_access(joined, uid, gid, 0).map_err(errno_of)?;
    }
    Ok(Target::Path { canonical: found.canonical, link, id: found.id })
}

/// The target of a `(dirfd, path, length, flags)` call: the descriptor's own file with
/// `AT_EMPTY_PATH` and a length of zero, else the path, followed unless `nofollow`.
fn target(
    slot: usize,
    process: &Process,
    tree: &dyn VfsTree,
    [dirfd, pointer, length]: [usize; 3],
    flags: usize,
    not_file: isize,
) -> Result<Target, isize> {
    if flags & at::EMPTY_PATH != 0 && length == 0 {
        return target_of_fd(process, tree, dirfd, not_file);
    }
    let joined = path_at(process, dirfd, pointer, length)?;
    target_of_path(slot, tree, &joined, flags & at::SYMLINK_NOFOLLOW == 0)
}

/// A flags argument: 32 bits, with nothing but `known` set.
fn flags_arg(flags: usize, known: usize) -> Result<usize, isize> {
    match int_arg(flags).map(|flags| flags as u32 as usize) {
        Some(flags) if flags & !known == 0 => Ok(flags),
        _ => Err(-errno::EINVAL),
    }
}

/// `renameat`.
pub fn sys_renameat(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    match renameat(slot, process, args) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

fn renameat(slot: usize, process: &Process, [from, to, flags, ..]: [usize; 6]) -> Result<(), isize> {
    // `RENAME_EXCHANGE` and `RENAME_WHITEOUT` aren't offered.
    let no_replace = flags_arg(flags, rename::NOREPLACE)? != 0;
    let (from, to) = (read_path_at(process, from)?, read_path_at(process, to)?);
    let (from, to) = (path_of(process, &from)?, path_of(process, &to)?);
    let tree = tree().ok_or(-errno::EIO)?;
    let (from_parent, from_name) = split(tree, &from)?;
    let (to_parent, to_name) = split(tree, &to)?;
    let (from_at, to_at) = (within(&from_parent, from_name)?, within(&to_parent, to_name)?);
    // The moving name itself, never followed: a slash after either says it must be a
    // directory, which a link isn't.
    let moving = tree.resolve_path(&from_at, false).map_err(errno_of)?;
    if (path::has_trailing_slash(&from) || path::has_trailing_slash(&to)) && moving.stat.kind != FileKind::Directory {
        return Err(-errno::ENOTDIR);
    }
    // Taking the old name away and putting one in the new directory, each with the
    // sticky bit's say, and what's replaced taken away too: all judged by the tree, under
    // its lock, on the directories its walk finds.
    let (uid, gid) = sched::credentials(slot);
    let caller = Caller { uid, gid };
    // As for `UNLINKAT`: no hold can be made between asking whether the replaced file is
    // in use and its name going, which the tree does under its lock, in one step.
    let _removing = pagecache::removing();
    match tree.rename_replacing(&from_at, &to_at, no_replace, Some(caller), &pagecache::held) {
        Ok(Some((id, at))) => {
            note_orphan(id, at);
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(error) => Err(errno_of(error)),
    }
}

/// `linkat`.
pub fn sys_linkat(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    match linkat(slot, process, args) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

fn linkat(slot: usize, process: &Process, [existing, new, flags, ..]: [usize; 6]) -> Result<(), isize> {
    let flags = flags_arg(flags, at::SYMLINK_FOLLOW | at::EMPTY_PATH)?;
    let (existing, new) = (read_path_at(process, existing)?, read_path_at(process, new)?);
    let tree = tree().ok_or(-errno::EIO)?;
    let (uid, gid) = sched::credentials(slot);
    // What gets the new name, by the path it has now: a descriptor's file has to still
    // have one (`ENOENT`), as on Linux.
    let source = if flags & at::EMPTY_PATH != 0 && existing.length == 0 {
        match target_of_fd(process, tree, existing.dirfd as isize as usize, -errno::ENOENT)? {
            Target::Path { canonical, link: false, .. } => {
                let found = tree.resolve_path(&canonical, true).map_err(errno_of)?;
                if found.stat.kind == FileKind::Directory {
                    return Err(-errno::EPERM);
                }
                found
            }
            Target::Path { canonical, link: true, .. } => tree.resolve_path(&canonical, false).map_err(errno_of)?,
            Target::File(id) => {
                let path = match &files::lookup(process, existing.dirfd as isize as usize)?.object {
                    Object::File { path, .. } => String::from(path.as_str()),
                    _ => return Err(-errno::ENOENT),
                };
                match tree.resolve_path(&path, false) {
                    Ok(found) if found.id == id => found,
                    _ => return Err(-errno::ENOENT),
                }
            }
        }
    } else {
        let joined = path_of(process, &existing)?;
        let found = path::lookup(tree, &joined, flags & at::SYMLINK_FOLLOW != 0).map_err(errno_of)?;
        if found.stat.kind == FileKind::Symlink {
            check_reach_link(tree, &found.canonical, uid, gid).map_err(errno_of)?;
        } else {
            tree.check_access(&joined, uid, gid, 0).map_err(errno_of)?;
        }
        found
    };
    if source.stat.kind == FileKind::Directory {
        return Err(-errno::EPERM);
    }
    let new = path_of(process, &new)?;
    let (parent, name) = split(tree, &new).map_err(|error| if error == -errno::EBUSY { -errno::EEXIST } else { error })?;
    let made = within(&parent, name)?;
    if tree.resolve_path(&made, false).is_ok() {
        return Err(-errno::EEXIST);
    }
    if path::has_trailing_slash(&new) {
        return Err(-errno::ENOENT);
    }
    if parent.stat.dev != source.stat.dev {
        return Err(-errno::EXDEV);
    }
    check_parent(tree, &made, uid, gid)?;
    // What was found reachable gets the name, and only if it's still the caller's to give
    // there: the tree judges both again, under its lock.
    tree.link_as(&source.canonical, &made, source.id, Caller { uid, gid }).map_err(errno_of)
}

/// `symlinkat`.
pub fn sys_symlinkat(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    match symlinkat(slot, process, args) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

fn symlinkat(slot: usize, process: &Process, [target, target_length, dirfd, pointer, length, _]: [usize; 6]) -> Result<(), isize> {
    // By the path rule, so that every link can be followed: not checked to exist.
    let target = user_path(process, target, target_length)?;
    let joined = path_at(process, dirfd, pointer, length)?;
    let tree = tree().ok_or(-errno::EIO)?;
    let (parent, name) = split(tree, &joined).map_err(|error| if error == -errno::EBUSY { -errno::EEXIST } else { error })?;
    let made = within(&parent, name)?;
    if tree.resolve_path(&made, false).is_ok() {
        return Err(-errno::EEXIST);
    }
    if path::has_trailing_slash(&joined) {
        return Err(-errno::ENOENT);
    }
    let (uid, gid) = sched::credentials(slot);
    check_parent(tree, &made, uid, gid)?;
    // Judged again under the tree's lock, and the link made the caller's there too.
    tree.symlink_as(&target, &made, Caller { uid, gid }).map_err(errno_of)
}

/// `readlinkat`.
pub fn sys_readlinkat(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    let [dirfd, pointer, length, buffer, size, _] = args;
    if size == 0 {
        return -errno::EINVAL;
    }
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    let link = if length == 0 && int_arg(dirfd) != Some(at::FDCWD) {
        // The link an `O_PATH | O_NOFOLLOW` descriptor is; any other isn't one.
        match files::lookup(process, dirfd) {
            Ok(ofd) => match &ofd.object {
                Object::Link { path } => String::from(path.as_str()),
                _ => return -errno::EINVAL,
            },
            Err(error) => return error,
        }
    } else {
        let joined = match path_at(process, dirfd, pointer, length) {
            Ok(joined) => joined,
            Err(error) => return error,
        };
        match target_of_path(slot, tree, &joined, false) {
            Ok(Target::Path { canonical, link: true, .. }) => canonical,
            Ok(_) => return -errno::EINVAL,
            Err(error) => return error,
        }
    };
    let target = match tree.read_link(&link) {
        Ok(target) => target,
        Err(FsError::Unsupported) => return -errno::EINVAL,
        Err(error) => return errno_of(error),
    };
    // Cut short where the buffer ends, silently, as POSIX's `readlink`.
    let count = target.len().min(size);
    match process.mm().copy_out(buffer, &target.as_bytes()[..count]) {
        Ok(()) => count as isize,
        Err(error) => error,
    }
}

/// `fchmodat`.
pub fn sys_fchmodat(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    match fchmodat(slot, process, args) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

fn fchmodat(slot: usize, process: &Process, [dirfd, pointer, length, mode, flags, _]: [usize; 6]) -> Result<(), isize> {
    let mode = match int_arg(mode).map(|mode| mode as u32) {
        Some(mode) if mode & !0o7777 == 0 => mode as u16,
        _ => return Err(-errno::EINVAL),
    };
    let flags = flags_arg(flags, at::EMPTY_PATH | at::SYMLINK_NOFOLLOW)?;
    // A link's mode means nothing, and Linux won't set one.
    if flags & at::SYMLINK_NOFOLLOW != 0 {
        return Err(-errno::EOPNOTSUPP);
    }
    let tree = tree().ok_or(-errno::EIO)?;
    let target = target(slot, process, tree, [dirfd, pointer, length], flags, -errno::EOPNOTSUPP)?;
    let (uid, _) = sched::credentials(slot);
    // Where nothing has an owner, there's no one to say no: only the filesystem to say
    // it can't. Asked of the file changed, under the tree's lock.
    let allow = |stat: &Stat| {
        if stat.posix && uid != 0 && stat.uid != uid {
            return Err(FsError::NotPermitted);
        }
        Ok(())
    };
    tree.chmod_file(target.id(), mode, &allow).map_err(errno_of)
}

/// `fchownat`.
pub fn sys_fchownat(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    match fchownat(slot, process, args) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

fn fchownat(slot: usize, process: &Process, [dirfd, pointer, length, uid, gid, flags]: [usize; 6]) -> Result<(), isize> {
    // 32-bit arguments, `-1` for "leave it": v1's `KEEP` is its sign extension.
    let (Some(owner), Some(group)) = (int_arg(uid), int_arg(gid)) else {
        return Err(-errno::EINVAL);
    };
    let flags = flags_arg(flags, at::EMPTY_PATH | at::SYMLINK_NOFOLLOW)?;
    let id = |value: i32| (value != -1).then_some(value as u32);
    let (owner, group) = (id(owner), id(group));
    let tree = tree().ok_or(-errno::EIO)?;
    let target = target(slot, process, tree, [dirfd, pointer, length], flags, -errno::EOPNOTSUPP)?;
    // Leaving both alone changes nothing, which anyone may do.
    if owner.is_none() && group.is_none() {
        return Ok(());
    }
    if sched::credentials(slot).0 != 0 {
        return Err(-errno::EPERM);
    }
    match &target {
        Target::File(id) => tree.chown_file(*id, owner, group),
        Target::Path { canonical, link: true, .. } => tree.lchown(canonical, owner, group),
        Target::Path { canonical, link: false, .. } => tree.chown(canonical, owner, group),
    }
    .map_err(errno_of)
}

/// One of `UTIMENSAT`'s times: `Some` to set, `None` to leave (`UTIME_OMIT`).
fn time_arg(given: Timespec, now: Timestamp) -> Result<Option<Timestamp>, isize> {
    match given.nsec {
        utime::NOW => Ok(Some(now)),
        utime::OMIT => Ok(None),
        // A time before the epoch has no place in what the tree keeps.
        _ => given.to_nanos().map(|nanos| Some(Timestamp(nanos))).ok_or(-errno::EINVAL),
    }
}

/// `utimensat`.
pub fn sys_utimensat(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    match utimensat(slot, process, args) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

fn utimensat(slot: usize, process: &Process, [dirfd, pointer, length, times, flags, _]: [usize; 6]) -> Result<(), isize> {
    let flags = flags_arg(flags, at::EMPTY_PATH | at::SYMLINK_NOFOLLOW)?;
    let now = Timestamp(time::realtime_ns());
    // Both read once, before anything else is looked at.
    let (accessed, modified, both_now) = if times == 0 {
        (Some(now), Some(now), true)
    } else {
        let mut bytes = [0u8; 2 * Timespec::SIZE];
        process.mm().copy_in(times, &mut bytes)?;
        let mut first = [0u8; Timespec::SIZE];
        let mut second = [0u8; Timespec::SIZE];
        first.copy_from_slice(&bytes[..Timespec::SIZE]);
        second.copy_from_slice(&bytes[Timespec::SIZE..]);
        let (first, second) = (Timespec::from_bytes(first), Timespec::from_bytes(second));
        let both_now = first.nsec == utime::NOW && second.nsec == utime::NOW;
        (time_arg(first, now)?, time_arg(second, now)?, both_now)
    };
    let tree = tree().ok_or(-errno::EIO)?;
    let target = target(slot, process, tree, [dirfd, pointer, length], flags, -errno::EOPNOTSUPP)?;
    if accessed.is_none() && modified.is_none() {
        return Ok(());
    }
    let (uid, gid) = sched::credentials(slot);
    // Its owner or root may set any time; anyone who may write it may set both to now.
    // Asked of the file changed, under the tree's lock.
    let allow = |stat: &Stat| {
        if stat.posix && uid != 0 && stat.uid != uid {
            let is_directory = stat.kind == FileKind::Directory;
            if !both_now {
                return Err(FsError::NotPermitted);
            }
            if !permits(stat.mode, is_directory, stat.uid, stat.gid, uid, gid, ACCESS_WRITE) {
                return Err(FsError::PermissionDenied);
            }
        }
        Ok(())
    };
    let times = NodeTimes { accessed, modified, ..NodeTimes::NONE };
    tree.set_times_file(target.id(), times, &allow).map_err(errno_of)
}

/// `faccessat`.
pub fn sys_faccessat(slot: usize, process: &Process, args: [usize; 6]) -> isize {
    match faccessat(slot, process, args) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

fn faccessat(slot: usize, process: &Process, [dirfd, pointer, length, mode, flags, _]: [usize; 6]) -> Result<(), isize> {
    let want = match int_arg(mode).map(|mode| mode as u32 as usize) {
        Some(mode) if mode & !access::ALL == 0 => mode as u8,
        _ => return Err(-errno::EINVAL),
    };
    // `AT_EACCESS` is the same as without: real and effective ids are always equal.
    let flags = flags_arg(flags, at::EACCESS | at::SYMLINK_NOFOLLOW | at::EMPTY_PATH)?;
    let tree = tree().ok_or(-errno::EIO)?;
    let (uid, gid) = sched::credentials(slot);
    let target = target(slot, process, tree, [dirfd, pointer, length], flags, -errno::EINVAL)?;
    let stat = target.stat(tree)?;
    if want & access::W_OK as u8 != 0 {
        let mount_read_only = match &target {
            Target::File(id) => tree.statfs_file(*id),
            Target::Path { canonical, .. } => tree.statfs(canonical),
        }
        .is_ok_and(|stats| stats.read_only);
        if mount_read_only {
            return Err(-errno::EROFS);
        }
    }
    if want == 0 {
        return Ok(());
    }
    match &target {
        // A link itself may be done anything to: what it leads to says.
        Target::Path { link: true, .. } => Ok(()),
        Target::Path { canonical, .. } => tree.check_access(canonical, uid, gid, want).map_err(errno_of),
        Target::File(_) if !stat.posix => Ok(()),
        Target::File(_) => {
            let is_directory = stat.kind == FileKind::Directory;
            if permits(stat.mode, is_directory, stat.uid, stat.gid, uid, gid, want) {
                Ok(())
            } else {
                Err(-errno::EACCES)
            }
        }
    }
}

/// `fstatvfs`.
pub fn sys_fstatvfs(process: &Process, fd: usize, out: usize) -> isize {
    let ofd = match files::lookup(process, fd) {
        Ok(ofd) => ofd,
        Err(error) => return error,
    };
    let Some(tree) = tree() else {
        return -errno::EIO;
    };
    let found = match &ofd.object {
        Object::File { id, .. } => tree.statfs_file(*id).and_then(|stats| Ok((stats, tree.stat_file(*id)?.dev))),
        Object::Directory { path, .. } => tree.statfs(path).and_then(|stats| Ok((stats, tree.stat(path)?.dev))),
        // A link's own filesystem: the one its directory is on.
        Object::Link { path } => {
            let directory = match path.rsplit_once('/') {
                Some(("", _)) | None => "/",
                Some((directory, _)) => directory,
            };
            tree.statfs(directory).and_then(|stats| Ok((stats, tree.lstat(path)?.dev)))
        }
        _ => return -errno::EINVAL,
    };
    let (stats, dev) = match found {
        Ok(found) => found,
        Err(error) => return errno_of(error),
    };
    let mut flags = st::NOSUID;
    if stats.read_only {
        flags |= st::RDONLY;
    }
    let block = stats.block_size.max(1);
    // Files aren't counted by most of these filesystems (FAT has no table of them), so
    // as Linux says for FAT: none, and none free.
    let result = StatVfs {
        bsize: block,
        frsize: block,
        blocks: stats.total_blocks,
        bfree: stats.free_blocks,
        bavail: stats.free_blocks,
        files: 0,
        ffree: 0,
        favail: 0,
        fsid: dev,
        flags,
        namemax: NAME_MAX as u64,
    };
    match process.mm().copy_out(out, &result.to_bytes()) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

/// `sync`: can't fail. What a mount couldn't flush stays dirty for the next one.
pub fn sys_sync() -> isize {
    if let Some(tree) = tree() {
        let _ = tree.sync();
    }
    0
}
