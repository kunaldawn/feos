//! The one filesystem tree the whole system shares.
//!
//! Before this, each shell kept private changes in an overlay, Explorer saw only
//! the boot image, and nothing could reach a disk. [`Vfs`] is a mount table of
//! writable filesystems behind one lock. Everything goes through paths, so the
//! serial shell, every Terminal and Explorer see the same files as soon as they
//! exist, and open files survive other people's renames.
//!
//! Apps get it from `Platform::filesystem`, a `&dyn FileSystem`, because most only
//! read. [`Vfs`] implements [`FileSystem`] too, on nodes it hands out by path, and
//! [`FileSystem::tree`] reaches the writable [`VfsTree`] interface.
//!
//! ## Locking
//!
//! One blocking mutex covers the mount table and every filesystem. A block
//! transfer can run under it, so it must never be taken with interrupts masked.
//! Long operations (a copy, a recursive delete) are many short ones, each locking
//! on its own, and other tasks can get in between.
//!
//! ## Durability
//!
//! A successful call is visible at once to everyone, and durable after [`sync`],
//! which a background task runs about once a second. A crash in between loses
//! recent changes but not consistency; see [`crate::Fat32`].
//!
//! [`sync`]: VfsTree::sync

mod boot;
pub mod devfs;
mod listing;
pub mod path;
mod ramfs;

pub use boot::{assemble, BootLog, Embedded};
pub use listing::{Listed, Snapshot};
pub use ramfs::{set_random_source, DeviceKind, RamFs, DEFAULT_QUOTA};

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use feos_sync::{Mutex, SpinLock};

use crate::{
    attr, EntryKind, FileSystem, FsError, NodeId, NodeTimes, TimeSource, VolumeStats,
    WritableFileSystem,
};

/// Symbolic links followed in one resolution: more is `ELOOP`, as on Linux (section 3.3
/// of the ABI design).
pub const MAX_SYMLINK_HOPS: usize = 40;
/// The directory at a mount's root where a file unlinked while it's in use keeps its
/// bytes until it's let go, named `<dev>-<ino>` ([`VfsTree::orphan`]). A listing of the
/// mount's root never shows it, and whatever is left in it goes when the mount is next
/// made.
pub const ORPHANS: &str = ".feos-orphans";
/// A symbolic link is a file with the system attribute that starts with this.
/// FAT has nothing better, and this way a link is just a small odd file to any
/// other OS, and survives a copy to one and back.
const SYMLINK_MAGIC: &[u8] = b"XSym\n";
/// Longest link target: `PATH_MAX`, since `/proc/<pid>/fd/<n>` and `/proc/<pid>/exe`
/// lead to whatever path a program opened or ran.
const MAX_LINK_TARGET: usize = 4096;
/// Open files at once.
const MAX_HANDLES: usize = 256;
/// Longest path this tree accepts: a program's relative path of up to `PATH_MAX` (4096)
/// joined to a working directory of up to as much, and the slash between.
const MAX_PATH_BYTES: usize = 2 * 4096 + 1;
/// Most mounts at once.
const MAX_MOUNTS: usize = 16;
/// How deep a copy across filesystems recurses.
const MAX_COPY_DEPTH: usize = 48;
/// Heap for the paths behind [`FileSystem`] node IDs. They can't be freed, since
/// a caller may hold the `&str`. Past this, new paths resolve as missing.
const MAX_INTERNED_BYTES: usize = 8 * 1024 * 1024;
/// Chunk used to copy across filesystems.
const COPY_CHUNK: usize = 64 * 1024;

/// What a path names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link. Only [`VfsTree::lstat`] and listings report this.
    Symlink,
}

/// What's known about a path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stat {
    /// File, directory or link.
    pub kind: FileKind,
    /// Size in bytes; zero for a directory.
    pub size: u64,
    /// Attribute bits; see [`attr`].
    pub attributes: u8,
    /// Timestamps, where the volume keeps them.
    pub times: NodeTimes,
    /// Writes here would be refused, by the mount or the file's attribute.
    pub read_only: bool,
    /// Type and permission bits. On a filesystem that doesn't keep them, these are made up
    /// from what it does say: see [`posix`](Self::posix).
    pub mode: u16,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Names this has.
    pub nlink: u32,
    /// A number for this file, unique on its filesystem, kept across renames and not
    /// given to another file while anything [holds](VfsTree::hold) it.
    pub ino: u64,
    /// The mount's number, the same for every file on it and for as long as it's
    /// mounted. With `ino`, the file's [`FileId`].
    pub dev: u64,
    /// The filesystem keeps the owner and mode, and they're enforced. FAT, exFAT and RAM
    /// mounts don't, and everyone may do anything there that the mount allows.
    pub posix: bool,
    /// What device it is, for a device node: `kind` says a file, but its bytes aren't
    /// stored anywhere, so it can't be mapped as one.
    pub device: Option<DeviceKind>,
}

/// A file's identity: its mount's number and its inode number, as [`Stat`] reports them.
/// It outlives renames, so the page cache and the kernel's lock table key files by it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

/// How a file's bytes changed, told to the [`FileHook`] after it allowed the change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change<'a> {
    /// `data` was written at `offset`; the file is `size` bytes now.
    Written { offset: u64, data: &'a [u8], size: u64 },
    /// The file was cut or grown to `size` bytes.
    Resized { size: u64 },
    /// A change of `[offset, offset + length)` failed part way, so what's there now isn't
    /// known; the file is `size` bytes.
    Failed { offset: u64, length: u64, size: u64 },
    /// Nothing changed after all.
    Nothing,
}

/// What the tree tells about changes to files' bytes, for a cache of them kept outside
/// it, such as the kernel's page cache. Called with the tree's lock held, so it mustn't
/// call the tree back. Every [`may_change`](Self::may_change) that says yes is followed
/// by one [`changed`](Self::changed).
pub trait FileHook: Sync {
    /// A write or truncation of `file` is about to start: `Err` (as [`FsError::TextBusy`]
    /// for a file mapped to run) refuses it, and nothing changes.
    fn may_change(&self, file: FileId) -> Result<(), FsError>;
    /// What `may_change` allowed is over.
    fn changed(&self, file: FileId, change: Change<'_>);
    /// The file's last name is gone, and its bytes with it.
    fn removed(&self, file: FileId);
    /// The mount `dev` is to be unmounted: whatever holds its files only to cache them
    /// should let them go now.
    fn unmounting(&self, dev: u64);
}

/// The sticky bit of a directory's mode: only an entry's owner, the directory's or root
/// may take an entry away ([`VfsTree::check_remove`]).
pub const STICKY: u16 = 0o1000;
/// Asking for read access, to [`VfsTree::check_access`].
pub const ACCESS_READ: u8 = 4;
/// Asking for write access.
pub const ACCESS_WRITE: u8 = 2;
/// Asking to run a file, or to go through a directory.
pub const ACCESS_EXECUTE: u8 = 1;

/// Whether these permission bits let this user do what's asked. Root may do anything,
/// except run a file with no execute bit at all, as on any Unix.
pub fn permits(mode: u16, is_directory: bool, owner: u32, group: u32, uid: u32, gid: u32, want: u8) -> bool {
    if uid == 0 {
        return want & ACCESS_EXECUTE == 0 || is_directory || mode & 0o111 != 0;
    }
    let bits = if uid == owner {
        mode >> 6
    } else if gid == group {
        mode >> 3
    } else {
        mode
    };
    (bits as u8 & 7) & want == want
}

/// Who a program's call is for, for the calls that check their permission under the
/// tree's lock, against what the tree walked to: a check made beforehand, on a path, can
/// be about a different file by the time the path is walked again, if a directory on the
/// way has been swapped for a link meanwhile. Such a call never reaches [`ORPHANS`]
/// either (`NotFound`), whatever leads there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Caller {
    /// Their user id.
    pub uid: u32,
    /// Their group id.
    pub gid: u32,
}

/// What a by-identity change asks before it's made, under the tree's lock, of the facts
/// of the file it's about to change: `Err` refuses it, and nothing changes.
pub type Allow<'a> = &'a dyn Fn(&Stat) -> Result<(), FsError>;

/// One entry of a directory listing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TreeEntry {
    /// Final path component.
    pub name: String,
    /// Its facts, not following a link.
    pub stat: Stat,
}

/// What a path resolves to, as [`VfsTree::resolve_path`] finds it in one look.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Resolved {
    /// The canonical path to it: absolute, every link on the way resolved (the last too,
    /// if it was followed), no `.` or `..`.
    pub canonical: String,
    /// Its facts; a link's own if the last component is a link that wasn't followed.
    pub stat: Stat,
    /// Its identity.
    pub id: FileId,
}

/// How to open a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct OpenOptions {
    /// Will be read.
    pub read: bool,
    /// Will be written.
    pub write: bool,
    /// Create it if missing.
    pub create: bool,
    /// Fail if it exists. Needs `create`.
    pub exclusive: bool,
    /// Empty it on open.
    pub truncate: bool,
    /// Every write goes to the end.
    pub append: bool,
}

impl OpenOptions {
    /// Open an existing file to read.
    pub const fn reading() -> Self {
        Self {
            read: true,
            write: false,
            create: false,
            exclusive: false,
            truncate: false,
            append: false,
        }
    }

    /// Create or replace a file to write.
    pub const fn replacing() -> Self {
        Self {
            read: false,
            write: true,
            create: true,
            exclusive: false,
            truncate: true,
            append: false,
        }
    }

    /// Open a file to add to its end, creating it if missing.
    pub const fn appending() -> Self {
        Self {
            read: false,
            write: true,
            create: true,
            exclusive: false,
            truncate: false,
            append: true,
        }
    }
}

/// Where a read or write position moves to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Seek {
    /// From the start.
    Start(u64),
    /// From the current position.
    Current(i64),
    /// From the end.
    End(i64),
}

/// An open file. See [`File`] for the wrapper that closes itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Handle {
    slot: u32,
    serial: u32,
}

/// What's mounted where.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MountInfo {
    /// Absolute mount point.
    pub path: String,
    /// The volume's facts.
    pub stats: VolumeStats,
    /// The mount refuses writes, whatever the volume could do.
    pub read_only: bool,
    /// Where it came from, such as `virtio0p1`.
    pub source: String,
}

/// The writable, path-based interface to the shared tree.
pub trait VfsTree: Sync {
    /// Facts about `path`, following a final symbolic link.
    fn stat(&self, path: &str) -> Result<Stat, FsError>;

    /// Facts about `path` itself, which may be a symbolic link.
    fn lstat(&self, path: &str) -> Result<Stat, FsError>;

    /// Size and free space of the filesystem holding `path`.
    fn statfs(&self, path: &str) -> Result<VolumeStats, FsError>;

    /// Lists a directory, sorted by name, with mount points in place.
    fn read_dir(&self, path: &str) -> Result<Vec<TreeEntry>, FsError>;

    /// Reads up to `out.len()` bytes at `offset`. Zero at or past the end.
    fn read_at(&self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, FsError>;

    /// Writes `data` at `offset` of an existing file, growing it if needed.
    fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize, FsError>;

    /// Creates or replaces a file with `data`, or adds to its end.
    fn write_file(&self, path: &str, data: &[u8], append: bool) -> Result<(), FsError>;

    /// Replaces a file so a crash leaves the old contents or the new, not a mix.
    fn write_file_atomic(&self, path: &str, data: &[u8]) -> Result<(), FsError>;

    /// Creates an empty file. Fails if the name is taken.
    fn create_file(&self, path: &str) -> Result<(), FsError>;

    /// Creates one directory.
    fn create_dir(&self, path: &str) -> Result<(), FsError>;

    /// Removes a file or symbolic link.
    fn remove_file(&self, path: &str) -> Result<(), FsError>;

    /// Removes an empty directory.
    fn remove_dir(&self, path: &str) -> Result<(), FsError>;

    /// Moves `from` to `to`, which must not exist. Works across mounts, by
    /// copying.
    fn rename(&self, from: &str, to: &str) -> Result<(), FsError>;

    /// Sets a file's length, zero-filling any growth.
    fn truncate(&self, path: &str, size: u64) -> Result<(), FsError>;

    /// Replaces the settable attribute bits (see [`attr::SETTABLE`]).
    fn set_attributes(&self, path: &str, attributes: u8) -> Result<(), FsError>;

    /// Sets whichever timestamps are `Some`.
    fn set_times(&self, path: &str, times: NodeTimes) -> Result<(), FsError>;

    /// Creates a symbolic link at `path` pointing at `target`.
    fn symlink(&self, target: &str, path: &str) -> Result<(), FsError>;

    /// The target of the symbolic link at `path`.
    fn read_link(&self, path: &str) -> Result<String, FsError>;

    /// Opens a file. Close the handle with [`close`](Self::close).
    fn open(&self, path: &str, options: OpenOptions) -> Result<Handle, FsError>;

    /// Reads from an open file and moves its position.
    fn read_handle(&self, handle: Handle, out: &mut [u8]) -> Result<usize, FsError>;

    /// Writes to an open file and moves its position.
    fn write_handle(&self, handle: Handle, data: &[u8]) -> Result<usize, FsError>;

    /// Moves an open file's position, returning the new one.
    fn seek_handle(&self, handle: Handle, to: Seek) -> Result<u64, FsError>;

    /// Facts about an open file.
    fn stat_handle(&self, handle: Handle) -> Result<Stat, FsError>;

    /// Closes an open file.
    fn close(&self, handle: Handle) -> Result<(), FsError>;

    /// Makes every change so far durable.
    fn sync(&self) -> Result<(), FsError>;

    /// Syncs, if anything changed since the last time. An idle system does no
    /// I/O for this.
    fn sync_if_dirty(&self) -> Result<(), FsError>;

    /// Bumps with each change anywhere in the tree. A listing is stale if this
    /// has moved since it was read.
    fn generation(&self) -> u64;

    /// Sets what runs when the first change arrives after a sync.
    fn set_change_hook(&self, hook: fn());

    /// Attaches a filesystem at `path`, which needn't exist in its parent.
    fn mount(
        &self,
        path: &str,
        fs: Box<dyn WritableFileSystem>,
        read_only: bool,
        source: &str,
    ) -> Result<(), FsError>;

    /// Flushes and detaches the filesystem at `path`.
    fn unmount(&self, path: &str) -> Result<(), FsError>;

    /// What's mounted, root first.
    fn mounts(&self) -> Vec<MountInfo>;

    /// Creates a device node. Only a RAM mount can hold one.
    fn make_device(&self, path: &str, kind: DeviceKind) -> Result<(), FsError>;

    /// The identity of the file `path` names, following links, with its facts.
    fn identify(&self, path: &str) -> Result<(FileId, Stat), FsError>;

    /// Reads from the file `file` is, wherever it is now: `NotFound` once its last
    /// name is gone.
    fn read_file(&self, file: FileId, offset: u64, out: &mut [u8]) -> Result<usize, FsError>;

    /// Facts about the file `file` is.
    fn stat_file(&self, file: FileId) -> Result<Stat, FsError>;

    /// Writes a cache's own copy of part of `file` back to it, wherever it is now, without
    /// telling the [`FileHook`]: the bytes are the hook's already. `fill` puts them in
    /// `buffer` and says how many, under the tree's lock, so no write through the tree can
    /// come between taking them and writing them. Never past the file's end: the file
    /// doesn't grow.
    fn write_back(&self, file: FileId, offset: u64, buffer: &mut [u8], fill: &mut dyn FnMut(&mut [u8]) -> usize) -> Result<(), FsError>;

    /// Keeps `file`'s inode number from going to another file, even once it's removed,
    /// until as many [`release`](Self::release)s. Its mount can't be unmounted meanwhile.
    fn hold(&self, file: FileId) -> Result<(), FsError>;

    /// Lets go of a [`hold`](Self::hold). Never takes the tree's lock, so a
    /// [`FileHook`] may call it.
    fn release(&self, file: FileId);

    /// Installs what's told of every change to a file's bytes.
    fn set_file_hook(&self, hook: &'static dyn FileHook);

    /// Resolves `path`, following a final link if `follow_last`: where it ends up, by
    /// its canonical path, what it is and its identity, all from one look.
    fn resolve_path(&self, _path: &str, _follow_last: bool) -> Result<Resolved, FsError> {
        Err(FsError::Unsupported)
    }

    /// Writes `data` at `offset` of the file `file` is, by identity, so a name it has
    /// lost since doesn't matter; grows it if needed. As [`write_at`](Self::write_at).
    fn write_file_at(&self, _file: FileId, _offset: u64, _data: &[u8]) -> Result<usize, FsError> {
        Err(FsError::Unsupported)
    }

    /// Cuts or grows the file `file` is to `size` bytes, by identity.
    fn truncate_file(&self, _file: FileId, _size: u64) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Takes the last name of a regular file something still uses away without taking
    /// its bytes: `path` (not followed) moves into [`ORPHANS`] at its mount's root, made
    /// if there's none, as `<dev>-<ino>`. Returns the name it has there, which the caller
    /// removes once nothing uses the file.
    fn orphan(&self, _path: &str) -> Result<String, FsError> {
        Err(FsError::Unsupported)
    }

    /// Removes whatever [`orphan`](Self::orphan) left in the mount at `mount_point`: done
    /// as a mount is made, when nothing can be using them. Says how many went.
    fn clean_orphans(&self, _mount_point: &str) -> Result<usize, FsError> {
        Ok(0)
    }

    /// Sets permission bits. `Unsupported` on a filesystem that doesn't keep them.
    fn chmod(&self, _path: &str, _mode: u16) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Sets whichever of the owner and group are `Some`.
    fn chown(&self, _path: &str, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Sets whichever of the owner and group are `Some`, of `path` itself: a link's own,
    /// not what it leads to.
    fn lchown(&self, _path: &str, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Gives the file at `existing` another name, `path`, on the same filesystem.
    fn link(&self, _existing: &str, _path: &str) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// A program's [`link`](Self::link): `existing` (not followed) must still be `source`
    /// (`NotFound` otherwise), the file the caller was found able to reach, and `caller`
    /// must be able to write and search the directory `path` is in, judged under the
    /// tree's lock.
    fn link_as(&self, _existing: &str, _path: &str, _source: FileId, _caller: Caller) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// A program's [`symlink`](Self::symlink): `caller` must be able to write and search
    /// the directory `path` is in, judged under the tree's lock, and the link is theirs,
    /// owned as it's made, where the filesystem keeps owners.
    fn symlink_as(&self, _target: &str, _path: &str, _caller: Caller) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// A program's [`create_file`](Self::create_file): `caller` must be able to write and
    /// search the directory `path` is in, as the tree's walk finds it, under its lock; the
    /// new file is theirs and has `mode` (where the filesystem keeps owners and modes) from
    /// that same step, and what was made is returned, for the caller to open by: never
    /// looked up again by its path, which could lead somewhere else by then.
    fn create_file_as(&self, _path: &str, _mode: u16, _caller: Caller) -> Result<Resolved, FsError> {
        Err(FsError::Unsupported)
    }

    /// A program's [`create_dir`](Self::create_dir), judged and owned in one step as
    /// [`create_file_as`](Self::create_file_as) is; `mode` `None` keeps the filesystem's.
    fn create_dir_as(&self, _path: &str, _mode: Option<u16>, _caller: Caller) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Whether a user may do `want` (`ACCESS_*`) to `path`, having to be able to go through
    /// every directory on the way. Only filesystems with owners say no. It's for the system
    /// call boundary: kernel-linked apps run as root and aren't checked at all.
    fn check_access(&self, _path: &str, _uid: u32, _gid: u32, _want: u8) -> Result<(), FsError> {
        Ok(())
    }

    /// Whether a user may take away the name `path` (a canonical path, not followed):
    /// write and search permission on the directory it's in, and, where that directory has
    /// the sticky bit (`/tmp`'s `0o1777`), owning the entry or the directory, or being
    /// root (`NotPermitted`, `EPERM`, otherwise). For the system call boundary, as
    /// [`check_access`](Self::check_access) is.
    fn check_remove(&self, path: &str, uid: u32, gid: u32) -> Result<(), FsError> {
        self.check_access(parent_of(path.trim_end_matches('/')), uid, gid, ACCESS_WRITE | ACCESS_EXECUTE)
    }

    /// Checks the filesystem holding `path` through and through, if it has a checker.
    fn fsck(&self, _path: &str, _repair: bool) -> Result<crate::native::FsckReport, FsError> {
        Err(FsError::Unsupported)
    }

    /// A program's rename (`RENAMEAT`, section 3.3 of the ABI design): `from` (not
    /// followed) to `to` (its last name not followed either), on one mount
    /// (`CrossDevice` otherwise), never under itself (`InvalidMove`), never a mount point
    /// (`Busy`). What `to` names is replaced in one step, so anyone looking finds the old
    /// file there or the new one, and so does the next mount after a crash: a file by a
    /// file (`IsDirectory` for a directory), a directory by a directory only if that's
    /// empty (`NotDirectory`, `NotEmpty`). With `no_replace`, anything there is
    /// `NameExists`. Two names of one file leave both as they are.
    ///
    /// The replaced file goes as [`remove_file`](Self::remove_file) takes it, unless it's
    /// a regular file's last name and `in_use` says something still holds it: then, in
    /// the same step, it becomes an orphan as [`orphan`](Self::orphan) makes one, and its
    /// identity and where it is are returned. `in_use` is asked with the tree's lock held.
    ///
    /// With a `caller`, theirs is a program's call: [`ORPHANS`] is `NotFound` on either
    /// side, and the old name must be theirs to take away and the new theirs to make or
    /// take, as [`check_remove`](Self::check_remove) and
    /// [`check_access`](Self::check_access) say, judged under the lock.
    fn rename_replacing(
        &self,
        _from: &str,
        _to: &str,
        _no_replace: bool,
        _caller: Option<Caller>,
        _in_use: &dyn Fn(FileId) -> bool,
    ) -> Result<Option<(FileId, String)>, FsError> {
        Err(FsError::Unsupported)
    }

    /// Sets the permission bits of the file `file` is, by identity, wherever it is now,
    /// if `allow` says so of its facts: a directory or a link too.
    fn chmod_file(&self, _file: FileId, _mode: u16, _allow: Allow<'_>) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Sets whichever of the owner and group of `file` are `Some`, by identity.
    fn chown_file(&self, _file: FileId, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Sets whichever timestamps of `file` are `Some`, by identity, if `allow` says so.
    fn set_times_file(&self, _file: FileId, _times: NodeTimes, _allow: Allow<'_>) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Size and free space of the mount `file` is on, by identity: as
    /// [`statfs`](Self::statfs), for a file whose name may have gone.
    fn statfs_file(&self, _file: FileId) -> Result<VolumeStats, FsError> {
        Err(FsError::Unsupported)
    }

    /// Sets the clock used for timestamps, on every mount present and to come: the
    /// kernel's, to the nanosecond, once it has one.
    fn set_time_source(&self, _now: TimeSource) {}
}

/// An open file that closes itself.
pub struct File<'a> {
    tree: &'a dyn VfsTree,
    handle: Handle,
}

impl<'a> File<'a> {
    /// Opens `path` on `tree`.
    pub fn open(tree: &'a dyn VfsTree, path: &str, options: OpenOptions) -> Result<Self, FsError> {
        Ok(Self {
            tree,
            handle: tree.open(path, options)?,
        })
    }

    /// See [`VfsTree::read_handle`].
    pub fn read(&self, out: &mut [u8]) -> Result<usize, FsError> {
        self.tree.read_handle(self.handle, out)
    }

    /// See [`VfsTree::write_handle`].
    pub fn write(&self, data: &[u8]) -> Result<usize, FsError> {
        self.tree.write_handle(self.handle, data)
    }

    /// See [`VfsTree::seek_handle`].
    pub fn seek(&self, to: Seek) -> Result<u64, FsError> {
        self.tree.seek_handle(self.handle, to)
    }

    /// See [`VfsTree::stat_handle`].
    pub fn stat(&self) -> Result<Stat, FsError> {
        self.tree.stat_handle(self.handle)
    }
}

impl Drop for File<'_> {
    fn drop(&mut self) {
        let _ = self.tree.close(self.handle);
    }
}

/// Reads a file written by [`VfsTree::write_file_atomic`].
///
/// A crash at the wrong moment can leave only the new copy, under its temporary
/// name, which is still complete: it's made durable before the old one goes.
pub fn read_atomic(tree: &dyn VfsTree, path: &str) -> Result<Vec<u8>, FsError> {
    let normal = normalize_path(path)?;
    let temporary = join_path(parent_of(&normal), &format!(".{}.new", base_of(&normal)));
    let chosen = match tree.stat(&normal) {
        Ok(_) => normal,
        Err(FsError::NotFound) => temporary,
        Err(error) => return Err(error),
    };
    let size = tree.stat(&chosen)?.size as usize;
    let mut data = Vec::new();
    data.try_reserve_exact(size).map_err(|_| FsError::OutOfMemory)?;
    data.resize(size, 0);
    let mut done = 0;
    while done < size {
        let count = tree.read_at(&chosen, done as u64, &mut data[done..])?;
        if count == 0 {
            return Err(FsError::Corrupt);
        }
        done += count;
    }
    Ok(data)
}

/// Normalises an absolute or relative-to-root path: no `.`, `..`, repeated or
/// trailing slashes. `..` stops at the root.
pub fn normalize_path(path: &str) -> Result<String, FsError> {
    if path.len() > MAX_PATH_BYTES {
        return Err(FsError::InvalidName);
    }
    let mut result = String::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                let keep = result.rfind('/').unwrap_or(0);
                result.truncate(keep);
            }
            other => {
                result.push('/');
                result.push_str(other);
            }
        }
    }
    if result.is_empty() {
        result.push('/');
    }
    Ok(result)
}

fn parent_of(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/",
        Some((parent, _)) => parent,
    }
}

fn base_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// True when `path` is `prefix` or lies under it.
fn is_within(path: &str, prefix: &str) -> bool {
    prefix == "/"
        || path == prefix
        || (path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/'))
}

struct Mount {
    path: String,
    /// Its number, [`Stat::dev`]: never reused for another mount.
    dev: u64,
    fs: Box<dyn WritableFileSystem>,
    read_only: bool,
    source: String,
    /// Changed since the last flush.
    dirty: bool,
}

struct OpenFile {
    serial: u32,
    mount: u32,
    node: NodeId,
    position: u64,
    readable: bool,
    writable: bool,
    append: bool,
    /// The file was removed, or its mount is gone.
    dead: bool,
}

struct State {
    mounts: Vec<Mount>,
    handles: Vec<Option<OpenFile>>,
    next_serial: u32,
    /// The number the next mount gets.
    next_dev: u64,
    now: Option<TimeSource>,
}

/// The part of a [`Stat`] about ownership.
struct PosixFields {
    mode: u16,
    uid: u32,
    gid: u32,
    nlink: u32,
    ino: u64,
    posix: bool,
}

/// A file something [holds](VfsTree::hold): its inode number isn't given to another file
/// until it's let go.
struct Held {
    file: FileId,
    count: u32,
    /// Where it was last found, to save a search on filesystems with inode numbers of
    /// their own.
    node: NodeId,
}

impl PosixFields {
    /// A whole `Stat` to fill the rest of in, with `..`.
    fn into_stat(self) -> Stat {
        Stat {
            kind: FileKind::File,
            size: 0,
            attributes: 0,
            times: NodeTimes::NONE,
            read_only: false,
            mode: self.mode,
            uid: self.uid,
            gid: self.gid,
            nlink: self.nlink,
            ino: self.ino,
            dev: 0,
            posix: self.posix,
            device: None,
        }
    }
}

/// What a file on a filesystem with no such idea reports: owned by root, and as open as
/// the attribute bits allow.
fn synthesised(kind: FileKind, attributes: u8, node: NodeId) -> PosixFields {
    use crate::native::{S_IFDIR, S_IFLNK, S_IFREG};
    let writable = attributes & attr::READ_ONLY == 0;
    let mode = match kind {
        FileKind::Directory => S_IFDIR | if writable { 0o755 } else { 0o555 },
        FileKind::Symlink => S_IFLNK | 0o777,
        FileKind::File => S_IFREG | if writable { 0o644 } else { 0o444 },
    };
    PosixFields {
        mode,
        uid: 0,
        gid: 0,
        nlink: 1,
        ino: u64::from(node.0) + 1,
        posix: false,
    }
}

/// Where a path ended up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Location {
    mount: usize,
    node: NodeId,
}

impl State {
    /// The mount whose point is exactly `path`.
    fn mount_at(&self, path: &str) -> Option<usize> {
        self.mounts.iter().position(|mount| mount.path == path)
    }

    fn is_mount_point(&self, path: &str) -> bool {
        self.mount_at(path).is_some()
    }

    /// Reads the target out of a link, or `None` when `node` isn't one.
    fn link_target(&self, mount: usize, node: NodeId) -> Result<Option<String>, FsError> {
        let fs = &self.mounts[mount].fs;
        if let Some(answer) = fs.synthesised_link(node) {
            return answer;
        }
        if fs.attributes(node) & attr::SYSTEM == 0 || fs.kind(node) != Some(EntryKind::File) {
            return Ok(None);
        }
        let size = fs.size(node);
        if size < SYMLINK_MAGIC.len() || size > SYMLINK_MAGIC.len() + MAX_LINK_TARGET {
            return Ok(None);
        }
        let mut bytes = alloc::vec![0u8; size];
        let mut done = 0;
        while done < size {
            let count = fs.try_read(node, done, &mut bytes[done..])?;
            if count == 0 {
                return Ok(None);
            }
            done += count;
        }
        if !bytes.starts_with(SYMLINK_MAGIC) {
            return Ok(None);
        }
        match core::str::from_utf8(&bytes[SYMLINK_MAGIC.len()..]) {
            Ok(target) if !target.is_empty() => Ok(Some(target.to_string())),
            _ => Ok(None),
        }
    }

    /// Walks `path` from the root, following links as asked. Returns where it
    /// ended up and the canonical path to there.
    fn walk(&self, path: &str, follow_last: bool) -> Result<(Location, String), FsError> {
        if path.len() > MAX_PATH_BYTES {
            return Err(FsError::InvalidName);
        }
        let root = self.mount_at("/").ok_or(FsError::NotFound)?;
        let root_node = self.mounts[root].fs.root();
        let mut here = Location {
            mount: root,
            node: root_node,
        };
        let mut walked = String::from("/");
        // Components still to go, last first, so links can push theirs on.
        let mut pending: Vec<String> = path
            .split('/')
            .rev()
            .filter(|part| !part.is_empty())
            .map(ToString::to_string)
            .collect();
        let mut hops = 0usize;

        while let Some(part) = pending.pop() {
            if part == "." {
                continue;
            }
            if part == ".." {
                walked = parent_of(&walked).to_string();
                // Going up has no shortcut across mounts: restart from the top.
                let (location, canonical) = self.walk(&walked, true)?;
                here = location;
                walked = canonical;
                continue;
            }
            if self.mounts[here.mount].fs.kind(here.node) != Some(EntryKind::Directory) {
                return Err(FsError::NotDirectory);
            }
            let next_path = join_path(&walked, &part);
            if let Some(mount) = self.mount_at(&next_path) {
                here = Location {
                    mount,
                    node: self.mounts[mount].fs.root(),
                };
                walked = next_path;
                continue;
            }
            let fs = &self.mounts[here.mount].fs;
            let node = fs.lookup(here.node, &part).ok_or(FsError::NotFound)?;
            let is_last = pending.is_empty();
            if !is_last || follow_last {
                if let Some(target) = self.link_target(here.mount, node)? {
                    hops += 1;
                    if hops > MAX_SYMLINK_HOPS {
                        return Err(FsError::SymlinkLoop);
                    }
                    for piece in target.rsplit('/').filter(|piece| !piece.is_empty()) {
                        pending.push(piece.to_string());
                    }
                    if target.starts_with('/') {
                        here = Location {
                            mount: root,
                            node: root_node,
                        };
                        walked = String::from("/");
                    }
                    continue;
                }
            }
            // The name as the volume spells it, in case the lookup ignored case.
            let actual = fs.name(node).unwrap_or(&part);
            walked = join_path(&walked, actual);
            here = Location {
                mount: here.mount,
                node,
            };
        }
        Ok((here, walked))
    }

    /// Resolves the directory part of `path`, leaving the last name to the
    /// caller. Returns the parent's location, its canonical path, and the name.
    fn walk_parent<'p>(&self, path: &'p str) -> Result<(Location, String, &'p str), FsError> {
        let name = path.rsplit('/').find(|part| !part.is_empty()).ok_or(FsError::InvalidName)?;
        if name == "." || name == ".." {
            return Err(FsError::InvalidName);
        }
        let trimmed = path.trim_end_matches('/');
        // `name` is the last part of `path`, so `trimmed` always ends with it.
        let parent = trimmed.strip_suffix(name).ok_or(FsError::InvalidName)?;
        let (location, canonical) = self.walk(parent, true)?;
        if self.mounts[location.mount].fs.kind(location.node) != Some(EntryKind::Directory) {
            return Err(FsError::NotDirectory);
        }
        Ok((location, canonical, name))
    }

    fn stat_location(&self, location: Location, is_link: bool) -> Result<Stat, FsError> {
        let mount = &self.mounts[location.mount];
        let fs = &mount.fs;
        let kind = fs.kind(location.node).ok_or(FsError::NotFound)?;
        let attributes = fs.attributes(location.node);
        let file_kind = match (kind, is_link) {
            (_, true) => FileKind::Symlink,
            (EntryKind::File, false) => FileKind::File,
            (EntryKind::Directory, false) => FileKind::Directory,
        };
        Ok(Stat {
            kind: file_kind,
            size: if kind == EntryKind::File {
                fs.size(location.node) as u64
            } else {
                0
            },
            attributes,
            times: fs.times(location.node),
            read_only: mount.read_only
                || fs.is_read_only()
                || attributes & attr::READ_ONLY != 0,
            dev: mount.dev,
            device: if is_link { None } else { fs.device(location.node) },
            ..match fs.posix(location.node) {
                Some(meta) => PosixFields {
                    mode: meta.mode,
                    uid: meta.uid,
                    gid: meta.gid,
                    nlink: meta.nlink,
                    ino: meta.ino,
                    posix: true,
                }
                .into_stat(),
                None => synthesised(file_kind, attributes, location.node).into_stat(),
            }
        })
    }

    /// The identity of what's at `location`.
    fn file_id(&self, location: Location) -> FileId {
        let mount = &self.mounts[location.mount];
        let ino = match mount.fs.posix(location.node) {
            Some(meta) => meta.ino,
            None => u64::from(location.node.0) + 1,
        };
        FileId { dev: mount.dev, ino }
    }

    /// Where the file `file` is now, if it still has a name. `hint` is where it was.
    fn find(&self, file: FileId, hint: Option<NodeId>) -> Result<Location, FsError> {
        let mount = self.mounts.iter().position(|mount| mount.dev == file.dev).ok_or(FsError::NotFound)?;
        let fs = &self.mounts[mount].fs;
        let is_it = |node: NodeId| fs.kind(node).is_some() && self.file_id(Location { mount, node }) == file;
        let node = match hint.filter(|&node| is_it(node)) {
            Some(node) => node,
            None => {
                let guess = u32::try_from(file.ino.wrapping_sub(1)).ok().map(NodeId);
                guess
                    .filter(|&node| is_it(node))
                    .or_else(|| fs.node_with_ino(file.ino).filter(|&node| is_it(node)))
                    .ok_or(FsError::NotFound)?
            }
        };
        Ok(Location { mount, node })
    }

    fn check_writable_mount(&self, mount: usize) -> Result<(), FsError> {
        let mount = &self.mounts[mount];
        if mount.read_only {
            return Err(FsError::ReadOnly);
        }
        Ok(())
    }

    /// Refuses to change a file the read-only attribute protects.
    fn check_writable_file(&self, location: Location) -> Result<(), FsError> {
        self.check_writable_mount(location.mount)?;
        if self.mounts[location.mount].fs.attributes(location.node) & attr::READ_ONLY != 0 {
            return Err(FsError::ReadOnly);
        }
        Ok(())
    }

    fn require_file(&self, location: Location) -> Result<(), FsError> {
        match self.mounts[location.mount].fs.kind(location.node) {
            Some(EntryKind::File) => Ok(()),
            Some(EntryKind::Directory) => Err(FsError::IsDirectory),
            None => Err(FsError::NotFound),
        }
    }

    /// Marks that a mount has changes for the next sync.
    fn touched(&mut self, mount: usize) {
        self.mounts[mount].dirty = true;
    }

    /// Open files on a node stop working once it's gone: its slot may be reused.
    fn kill_handles(&mut self, mount: usize, node: NodeId) {
        for handle in self.handles.iter_mut().flatten() {
            if handle.mount == mount as u32 && handle.node == node {
                handle.dead = true;
            }
        }
    }

    fn handle_mut(&mut self, handle: Handle) -> Result<&mut OpenFile, FsError> {
        let open = self
            .handles
            .get_mut(handle.slot as usize)
            .and_then(Option::as_mut)
            .filter(|open| open.serial == handle.serial)
            .ok_or(FsError::StaleHandle)?;
        if open.dead {
            return Err(FsError::StaleHandle);
        }
        Ok(open)
    }

    fn write_all(
        &mut self,
        location: Location,
        mut offset: usize,
        data: &[u8],
    ) -> Result<(), FsError> {
        let mut done = 0;
        while done < data.len() {
            let count = self.mounts[location.mount].fs.write(
                location.node,
                offset,
                &data[done..],
            )?;
            if count == 0 {
                return Err(FsError::NoSpace);
            }
            done += count;
            offset += count;
        }
        Ok(())
    }

    /// [`VfsTree::check_access`], under the lock already held.
    fn check_access(&self, path: &str, caller: Caller, want: u8) -> Result<(), FsError> {
        let Caller { uid, gid } = caller;
        // A step at a time, each directory judged before anything past it is looked at.
        // Otherwise whether a name exists in a closed directory would show in the answer.
        // A link is judged by what it leads to.
        let mut here = self.walk("/", true)?.0;
        let mut prefix = String::new();
        for part in path.split('/').filter(|part| !part.is_empty() && *part != ".") {
            let fs = &self.mounts[here.mount].fs;
            if let Some(meta) = fs.posix(here.node) {
                if !permits(meta.mode, true, meta.uid, meta.gid, uid, gid, ACCESS_EXECUTE) {
                    return Err(FsError::PermissionDenied);
                }
            }
            prefix.push('/');
            prefix.push_str(part);
            here = self.walk(&prefix, true)?.0;
        }
        let fs = &self.mounts[here.mount].fs;
        if let Some(meta) = fs.posix(here.node) {
            let is_directory = fs.kind(here.node) == Some(EntryKind::Directory);
            if !permits(meta.mode, is_directory, meta.uid, meta.gid, uid, gid, want) {
                return Err(FsError::PermissionDenied);
            }
        }
        Ok(())
    }

    /// [`VfsTree::check_remove`], under the lock already held.
    fn check_remove(&self, path: &str, caller: Caller) -> Result<(), FsError> {
        let trimmed = path.trim_end_matches('/');
        let parent = parent_of(trimmed);
        self.check_access(parent, caller, ACCESS_WRITE | ACCESS_EXECUTE)?;
        if caller.uid == 0 {
            return Ok(());
        }
        let (directory, _) = self.walk(parent, true)?;
        let Some(meta) = self.mounts[directory.mount].fs.posix(directory.node) else {
            return Ok(());
        };
        if meta.mode & STICKY == 0 || meta.uid == caller.uid {
            return Ok(());
        }
        let (entry, _) = self.walk(trimmed, false)?;
        match self.mounts[entry.mount].fs.posix(entry.node) {
            Some(own) if own.uid == caller.uid => Ok(()),
            _ => Err(FsError::NotPermitted),
        }
    }

    /// May `caller` add a name to the directory `parent` (a path to it), or take one away
    /// where there's no sticky bit: write and search permission on it.
    fn check_parent(&self, parent: &str, caller: Caller) -> Result<(), FsError> {
        self.check_access(parent, caller, ACCESS_WRITE | ACCESS_EXECUTE)
    }

    /// The [`ORPHANS`] directory at the root of `mount`, made if there's none.
    fn orphan_directory(&mut self, mount: usize) -> Result<NodeId, FsError> {
        let fs = &mut self.mounts[mount].fs;
        let root = fs.root();
        match fs.lookup(root, ORPHANS) {
            Some(node) if fs.kind(node) == Some(EntryKind::Directory) => Ok(node),
            Some(_) => Err(FsError::NameExists),
            None => {
                let node = fs.create_dir(root, ORPHANS)?;
                // Root's alone, where the filesystem keeps modes at all.
                let _ = fs.set_mode(node, 0o700);
                Ok(node)
            }
        }
    }

    /// Creates a file under the resolved parent.
    fn create_at(&mut self, parent: Location, name: &str) -> Result<Location, FsError> {
        self.check_writable_mount(parent.mount)?;
        let node = self.mounts[parent.mount].fs.create_file(parent.node, name)?;
        self.touched(parent.mount);
        Ok(Location {
            mount: parent.mount,
            node,
        })
    }
}

/// Paths behind the node IDs of the [`FileSystem`] view of a [`Vfs`].
struct Interner {
    ids: BTreeMap<&'static str, u32>,
    paths: Vec<&'static str>,
    bytes: usize,
    /// The last directory listed, so that indexed walks don't relist it per child.
    listing: Option<(u32, u64, Vec<NodeId>)>,
}

impl Interner {
    fn new() -> Self {
        Self {
            ids: BTreeMap::new(),
            paths: Vec::new(),
            bytes: 0,
            listing: None,
        }
    }

    fn intern(&mut self, path: &str) -> Option<NodeId> {
        if let Some(&id) = self.ids.get(path) {
            return Some(NodeId(id));
        }
        if self.bytes + path.len() > MAX_INTERNED_BYTES {
            return None;
        }
        let id = u32::try_from(self.paths.len()).ok()?;
        self.paths.try_reserve(1).ok()?;
        let mut owned = String::new();
        owned.try_reserve_exact(path.len()).ok()?;
        owned.push_str(path);
        let leaked: &'static str = alloc::boxed::Box::leak(owned.into_boxed_str());
        self.bytes += leaked.len();
        self.ids.insert(leaked, id);
        self.paths.push(leaked);
        Some(NodeId(id))
    }

    fn path(&self, node: NodeId) -> Option<&'static str> {
        self.paths.get(node.0 as usize).copied()
    }
}

/// The shared tree. See the module docs.
pub struct Vfs {
    state: Mutex<State>,
    generation: AtomicU64,
    dirty: AtomicBool,
    change_hook: SpinLock<Option<fn()>>,
    interner: Mutex<Interner>,
    file_hook: SpinLock<Option<&'static dyn FileHook>>,
    /// Files held, and how many times.
    held: SpinLock<Vec<Held>>,
    /// Files whose last hold went, to let their filesystems reuse their numbers the next
    /// time the tree's lock is taken: [`VfsTree::release`] may be called with it held.
    let_go: SpinLock<Vec<(FileId, NodeId)>>,
}

impl Vfs {
    /// A tree with `root` mounted at `/`.
    pub fn new(root: Box<dyn WritableFileSystem>, read_only: bool, source: &str) -> Self {
        let mut interner = Interner::new();
        let _ = interner.intern("/");
        Self {
            state: Mutex::new(State {
                mounts: alloc::vec![Mount {
                    path: String::from("/"),
                    dev: 1,
                    fs: root,
                    read_only,
                    source: source.to_string(),
                    dirty: false,
                }],
                handles: Vec::new(),
                next_serial: 1,
                next_dev: 2,
                now: None,
            }),
            generation: AtomicU64::new(1),
            dirty: AtomicBool::new(false),
            change_hook: SpinLock::new(None),
            interner: Mutex::new(interner),
            file_hook: SpinLock::new(None),
            held: SpinLock::new(Vec::new()),
            let_go: SpinLock::new(Vec::new()),
        }
    }

    /// An empty tree in RAM, which is what tests and simple boots want.
    pub fn in_memory(quota: usize) -> Self {
        Self::new(Box::new(RamFs::new(quota)), false, "ram")
    }

    /// Takes the tree's lock, and lets filesystems reuse the numbers of files whose
    /// last hold went since.
    fn lock(&self) -> feos_sync::MutexGuard<'_, State> {
        let mut state = self.state.lock();
        loop {
            let next = self.let_go.lock().pop();
            let Some((file, node)) = next else {
                break;
            };
            // Held again since: the new hold keeps it.
            if self.held.lock().iter().any(|held| held.file == file) {
                continue;
            }
            if let Some(mount) = state.mounts.iter_mut().find(|mount| mount.dev == file.dev) {
                let _ = mount.fs.keep_id(node, file.ino, false);
            }
        }
        state
    }

    fn hook(&self) -> Option<&'static dyn FileHook> {
        *self.file_hook.lock()
    }

    /// Asks the hook whether `location`'s bytes may change, and says which file it is.
    fn may_change(&self, state: &State, location: Location) -> Result<FileId, FsError> {
        let file = state.file_id(location);
        if let Some(hook) = self.hook() {
            hook.may_change(file)?;
        }
        Ok(file)
    }

    /// Tells the hook how the change it allowed went.
    fn changed_file(&self, file: FileId, change: Change<'_>) {
        if let Some(hook) = self.hook() {
            hook.changed(file, change);
        }
    }

    /// Writes `data` at `offset` under the hook: allowed first, told after, whether it
    /// worked or not.
    fn write_hooked(&self, state: &mut State, location: Location, offset: usize, data: &[u8]) -> Result<(), FsError> {
        let file = self.may_change(state, location)?;
        let result = state.write_all(location, offset, data);
        let size = state.mounts[location.mount].fs.size(location.node) as u64;
        let change = match result {
            Ok(()) => Change::Written { offset: offset as u64, data, size },
            Err(_) => Change::Failed { offset: offset as u64, length: data.len() as u64, size },
        };
        self.changed_file(file, change);
        result
    }

    /// Sets `location`'s length under the hook.
    fn resize_hooked(&self, state: &mut State, location: Location, size: usize) -> Result<(), FsError> {
        let file = self.may_change(state, location)?;
        let result = state.mounts[location.mount].fs.set_len(location.node, size);
        let now = state.mounts[location.mount].fs.size(location.node) as u64;
        let change = match result {
            Ok(()) => Change::Resized { size: now },
            Err(_) => Change::Failed { offset: 0, length: u64::MAX, size: now },
        };
        self.changed_file(file, change);
        result
    }

    fn changed(&self) {
        self.generation.fetch_add(1, Ordering::Release);
        if !self.dirty.swap(true, Ordering::AcqRel) {
            let hook = *self.change_hook.lock();
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    fn file_data(&self, state: &State, location: Location, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
        let offset = usize::try_from(offset).map_err(|_| FsError::OutOfBounds)?;
        state.mounts[location.mount].fs.try_read(location.node, offset, out)
    }

    /// Moves or copies `from` to `to` when they're on different mounts.
    fn copy_across(&self, from: &str, to: &str, depth: usize) -> Result<(), FsError> {
        if depth > MAX_COPY_DEPTH {
            return Err(FsError::TooDeep);
        }
        let stat = self.lstat(from)?;
        match stat.kind {
            FileKind::Directory => {
                self.create_dir(to)?;
                for entry in self.read_dir(from)? {
                    self.copy_across(
                        &join_path(from, &entry.name),
                        &join_path(to, &entry.name),
                        depth + 1,
                    )?;
                }
            }
            FileKind::Symlink => {
                let target = self.read_link(from)?;
                self.symlink(&target, to)?;
            }
            FileKind::File => {
                self.create_file(to)?;
                let mut chunk = alloc::vec::Vec::new();
                chunk
                    .try_reserve_exact(COPY_CHUNK)
                    .map_err(|_| FsError::OutOfMemory)?;
                chunk.resize(COPY_CHUNK, 0);
                // Up to its size, not until reads stop: `/dev/zero` never would.
                let mut offset = 0u64;
                while offset < stat.size {
                    let want = (stat.size - offset).min(COPY_CHUNK as u64) as usize;
                    let count = self.read_at(from, offset, &mut chunk[..want])?;
                    if count == 0 {
                        break;
                    }
                    self.write_at(to, offset, &chunk[..count])?;
                    offset += count as u64;
                }
            }
        }
        self.set_attributes(to, stat.attributes).or_else(ignore_unsupported)?;
        self.set_times(to, stat.times).or_else(ignore_unsupported)?;
        Ok(())
    }

    /// [`VfsTree::create_file_as`] and [`VfsTree::create_dir_as`]: the new name's directory
    /// judged, the entry made, and its owner and mode set, all under the one lock, on what
    /// the walk reached. Done by its path in separate steps, a directory on the way swapped
    /// for a link between them could have the check made in one place, the file made in
    /// another, and the owner and mode given to whatever the path named by then (a file of
    /// root's, say).
    fn create_as(&self, path: &str, directory: bool, mode: Option<u16>, caller: Caller) -> Result<Resolved, FsError> {
        let mut state = self.lock();
        let (parent, canonical, name) = state.walk_parent(path)?;
        let made = join_path(&canonical, name);
        if path::is_hidden(&made) {
            return Err(FsError::NotFound);
        }
        if state.is_mount_point(&made) {
            return Err(FsError::NameExists);
        }
        state.check_parent(&canonical, caller)?;
        let location = if directory {
            state.check_writable_mount(parent.mount)?;
            let node = state.mounts[parent.mount].fs.create_dir(parent.node, name)?;
            state.touched(parent.mount);
            Location { mount: parent.mount, node }
        } else {
            state.create_at(parent, name)?
        };
        let fs = &mut state.mounts[location.mount].fs;
        // A filesystem with no owners or modes has nothing to keep, and says so.
        if caller.uid != 0 || caller.gid != 0 {
            let _ = fs.set_owner(location.node, Some(caller.uid), Some(caller.gid));
        }
        if let Some(mode) = mode {
            let _ = fs.set_mode(location.node, mode);
        }
        let stat = state.stat_location(location, false)?;
        let id = state.file_id(location);
        drop(state);
        self.changed();
        Ok(Resolved { canonical: made, stat, id })
    }

    /// [`VfsTree::symlink`], and with a `caller`, [`VfsTree::symlink_as`].
    fn make_symlink(&self, target: &str, path: &str, caller: Option<Caller>) -> Result<(), FsError> {
        if target.is_empty() || target.len() > MAX_LINK_TARGET {
            return Err(FsError::InvalidName);
        }
        let mut state = self.lock();
        let (parent, canonical, name) = state.walk_parent(path)?;
        if let Some(caller) = caller {
            if path::is_hidden(&join_path(&canonical, name)) {
                return Err(FsError::NotFound);
            }
            state.check_parent(&canonical, caller)?;
        }
        let location = state.create_at(parent, name)?;
        let mut contents = Vec::new();
        contents
            .try_reserve_exact(SYMLINK_MAGIC.len() + target.len())
            .map_err(|_| FsError::OutOfMemory)?;
        contents.extend_from_slice(SYMLINK_MAGIC);
        contents.extend_from_slice(target.as_bytes());
        let written = state
            .write_all(location, 0, &contents)
            .and_then(|()| {
                state.mounts[location.mount]
                    .fs
                    .set_attributes(location.node, attr::SYSTEM | attr::ARCHIVE)
            });
        if let Err(error) = written {
            // No half-made link: without the attribute it's just a stray file.
            let _ = state.mounts[location.mount].fs.remove(location.node);
            return Err(error);
        }
        // Theirs from the start: given later, by its path, it could go to whatever that
        // path names by then.
        if let Some(Caller { uid, gid }) = caller.filter(|caller| caller.uid != 0 || caller.gid != 0) {
            let _ = state.mounts[location.mount].fs.set_owner(location.node, Some(uid), Some(gid));
        }
        drop(state);
        self.changed();
        Ok(())
    }

    /// [`VfsTree::link`], and for a program, [`VfsTree::link_as`].
    fn hard_link(&self, existing: &str, path: &str, program: Option<(FileId, Caller)>) -> Result<(), FsError> {
        let mut state = self.lock();
        let (from, canonical_from) = state.walk(existing, false)?;
        let (parent, canonical, name) = state.walk_parent(path)?;
        if let Some((source, caller)) = program {
            if path::is_hidden(&canonical_from) || path::is_hidden(&join_path(&canonical, name)) || state.file_id(from) != source {
                return Err(FsError::NotFound);
            }
            state.check_parent(&canonical, caller)?;
        }
        if from.mount != parent.mount {
            return Err(FsError::Unsupported);
        }
        let mount = &mut state.mounts[from.mount];
        if mount.read_only {
            return Err(FsError::ReadOnly);
        }
        mount.fs.hard_link(from.node, parent.node, name)?;
        mount.dirty = true;
        drop(state);
        self.changed();
        Ok(())
    }

    /// Changes something about the file `file` is, by identity, on a writable mount, if
    /// `allow` says so of its facts as they are under the lock, so of the very file
    /// changed.
    fn change_file(
        &self,
        file: FileId,
        allow: Allow<'_>,
        change: &mut dyn FnMut(&mut dyn WritableFileSystem, NodeId) -> Result<(), FsError>,
    ) -> Result<(), FsError> {
        let mut state = self.lock();
        let hint = self.held.lock().iter().find(|held| held.file == file).map(|held| held.node);
        let location = state.find(file, hint)?;
        let is_link = state.link_target(location.mount, location.node)?.is_some();
        allow(&state.stat_location(location, is_link)?)?;
        state.check_writable_mount(location.mount)?;
        change(&mut *state.mounts[location.mount].fs, location.node)?;
        state.touched(location.mount);
        drop(state);
        self.changed();
        Ok(())
    }

    fn remove_tree(&self, path: &str, depth: usize) -> Result<(), FsError> {
        if depth > MAX_COPY_DEPTH {
            return Err(FsError::TooDeep);
        }
        if self.lstat(path)?.kind == FileKind::Directory {
            for entry in self.read_dir(path)? {
                self.remove_tree(&join_path(path, &entry.name), depth + 1)?;
            }
            self.remove_dir(path)
        } else {
            self.remove_file(path)
        }
    }
}

fn ignore_unsupported(error: FsError) -> Result<(), FsError> {
    if error == FsError::Unsupported {
        Ok(())
    } else {
        Err(error)
    }
}

impl VfsTree for Vfs {
    fn stat(&self, path: &str) -> Result<Stat, FsError> {
        let state = self.lock();
        let (location, _) = state.walk(path, true)?;
        state.stat_location(location, false)
    }

    fn lstat(&self, path: &str) -> Result<Stat, FsError> {
        let state = self.lock();
        let (location, canonical) = state.walk(path, false)?;
        let is_link = canonical != "/"
            && !state.is_mount_point(&canonical)
            && state.link_target(location.mount, location.node)?.is_some();
        state.stat_location(location, is_link)
    }

    fn statfs(&self, path: &str) -> Result<VolumeStats, FsError> {
        let state = self.lock();
        let (location, _) = state.walk(path, true)?;
        let mount = &state.mounts[location.mount];
        let mut stats = mount.fs.stats();
        stats.read_only |= mount.read_only;
        Ok(stats)
    }

    fn read_dir(&self, path: &str) -> Result<Vec<TreeEntry>, FsError> {
        let state = self.lock();
        let (location, canonical) = state.walk(path, true)?;
        let fs = &state.mounts[location.mount].fs;
        if fs.kind(location.node) != Some(EntryKind::Directory) {
            return Err(FsError::NotDirectory);
        }
        let mut entries: Vec<TreeEntry> = Vec::new();
        for index in 0..fs.child_count(location.node) {
            let Some(child) = fs.child_at(location.node, index) else {
                continue;
            };
            let Some(name) = fs.name(child).map(ToString::to_string).or_else(|| fs.dynamic_name(child)) else {
                continue;
            };
            // A made-up node can go between being counted and being looked at (a
            // descriptor another thread closed): it's left out, as Linux's would be.
            let is_link = match state.link_target(location.mount, child) {
                Ok(target) => target.is_some(),
                Err(FsError::NotFound) => continue,
                Err(error) => return Err(error),
            };
            let stat = match state.stat_location(
                Location {
                    mount: location.mount,
                    node: child,
                },
                is_link,
            ) {
                Ok(stat) => stat,
                Err(FsError::NotFound) => continue,
                Err(error) => return Err(error),
            };
            entries.try_reserve(1).map_err(|_| FsError::OutOfMemory)?;
            entries.push(TreeEntry { name, stat });
        }
        // Mount points replace whatever their parent has under that name.
        for (index, mount) in state.mounts.iter().enumerate() {
            if mount.path == "/" || parent_of(&mount.path) != canonical {
                continue;
            }
            let name = base_of(&mount.path);
            entries.retain(|entry| entry.name != name);
            let stat = state.stat_location(
                Location {
                    mount: index,
                    node: mount.fs.root(),
                },
                false,
            )?;
            entries.try_reserve(1).map_err(|_| FsError::OutOfMemory)?;
            entries.push(TreeEntry {
                name: name.to_string(),
                stat,
            });
        }
        // Files unlinked while in use are no one's to see.
        if state.is_mount_point(&canonical) {
            entries.retain(|entry| entry.name != ORPHANS);
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    fn read_at(&self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
        let state = self.lock();
        let (location, _) = state.walk(path, true)?;
        state.require_file(location)?;
        self.file_data(&state, location, offset, out)
    }

    fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize, FsError> {
        let mut state = self.lock();
        let (location, _) = state.walk(path, true)?;
        state.require_file(location)?;
        state.check_writable_file(location)?;
        let offset = usize::try_from(offset).map_err(|_| FsError::NoSpace)?;
        let result = self.write_hooked(&mut state, location, offset, data);
        state.touched(location.mount);
        drop(state);
        self.changed();
        result.map(|()| data.len())
    }

    fn write_file(&self, path: &str, data: &[u8], append: bool) -> Result<(), FsError> {
        let mut state = self.lock();
        let location = match state.walk(path, true) {
            Ok((location, _)) => {
                state.require_file(location)?;
                state.check_writable_file(location)?;
                location
            }
            Err(FsError::NotFound) => {
                let (parent, _, name) = state.walk_parent(path)?;
                state.create_at(parent, name)?
            }
            Err(error) => return Err(error),
        };
        let offset = if append {
            state.mounts[location.mount].fs.size(location.node)
        } else {
            self.resize_hooked(&mut state, location, 0)?;
            0
        };
        let result = self.write_hooked(&mut state, location, offset, data);
        state.touched(location.mount);
        drop(state);
        self.changed();
        result
    }

    fn write_file_atomic(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        let normal = normalize_path(path)?;
        let name = base_of(&normal);
        if name.is_empty() {
            return Err(FsError::InvalidName);
        }
        let temporary = join_path(parent_of(&normal), &format!(".{name}.new"));
        match self.remove_file(&temporary) {
            Ok(()) | Err(FsError::NotFound) => {}
            Err(error) => return Err(error),
        }
        self.write_file(&temporary, data, false)?;
        // The new bytes must be on the disk before any name refers to them.
        self.sync()?;
        // In one step, so the name is never missing, crash or no crash. A kernel app's
        // removals never orphan.
        self.rename_replacing(&temporary, &normal, false, None, &|_| false)?;
        self.sync()
    }

    fn create_file(&self, path: &str) -> Result<(), FsError> {
        let mut state = self.lock();
        let (parent, canonical, name) = state.walk_parent(path)?;
        if state.is_mount_point(&join_path(&canonical, name)) {
            return Err(FsError::NameExists);
        }
        state.create_at(parent, name)?;
        drop(state);
        self.changed();
        Ok(())
    }

    fn create_file_as(&self, path: &str, mode: u16, caller: Caller) -> Result<Resolved, FsError> {
        self.create_as(path, false, Some(mode), caller)
    }

    fn create_dir_as(&self, path: &str, mode: Option<u16>, caller: Caller) -> Result<(), FsError> {
        self.create_as(path, true, mode, caller).map(|_| ())
    }

    fn create_dir(&self, path: &str) -> Result<(), FsError> {
        let mut state = self.lock();
        let (parent, canonical, name) = state.walk_parent(path)?;
        if state.is_mount_point(&join_path(&canonical, name)) {
            return Err(FsError::NameExists);
        }
        state.check_writable_mount(parent.mount)?;
        state.mounts[parent.mount].fs.create_dir(parent.node, name)?;
        state.touched(parent.mount);
        drop(state);
        self.changed();
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<(), FsError> {
        let mut state = self.lock();
        let (location, canonical) = state.walk(path, false)?;
        if state.is_mount_point(&canonical) {
            return Err(FsError::Busy);
        }
        state.require_file(location)?;
        state.check_writable_file(location)?;
        let file = state.file_id(location);
        let links = state.mounts[location.mount].fs.posix(location.node).map_or(1, |meta| meta.nlink);
        state.mounts[location.mount].fs.remove(location.node)?;
        state.kill_handles(location.mount, location.node);
        state.touched(location.mount);
        // Its bytes go with its last name; another name keeps them.
        if links <= 1 {
            if let Some(hook) = self.hook() {
                hook.removed(file);
            }
        }
        drop(state);
        self.changed();
        Ok(())
    }

    fn remove_dir(&self, path: &str) -> Result<(), FsError> {
        let mut state = self.lock();
        let (location, canonical) = state.walk(path, false)?;
        if canonical == "/" || state.is_mount_point(&canonical) {
            return Err(FsError::Busy);
        }
        // A mount point inside still counts as content, though the volume
        // doesn't know of it.
        let prefix = join_path(&canonical, "");
        if state.mounts.iter().any(|mount| mount.path.starts_with(&prefix)) {
            return Err(FsError::Busy);
        }
        match state.mounts[location.mount].fs.kind(location.node) {
            Some(EntryKind::Directory) => {}
            Some(EntryKind::File) => return Err(FsError::NotDirectory),
            None => return Err(FsError::NotFound),
        }
        state.check_writable_mount(location.mount)?;
        state.mounts[location.mount].fs.remove(location.node)?;
        state.touched(location.mount);
        drop(state);
        self.changed();
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), FsError> {
        let same_mount = {
            let mut state = self.lock();
            let (source, canonical_from) = state.walk(from, false)?;
            if canonical_from == "/" || state.is_mount_point(&canonical_from) {
                return Err(FsError::Busy);
            }
            let (parent, canonical_parent, name) = state.walk_parent(to)?;
            let target = join_path(&canonical_parent, name);
            if target == canonical_from {
                return Ok(());
            }
            if is_within(&target, &canonical_from) {
                return Err(FsError::Unsupported);
            }
            if state.is_mount_point(&target) {
                return Err(FsError::NameExists);
            }
            // A mount inside what's moving would lose its path.
            let prefix = join_path(&canonical_from, "");
            if state.mounts.iter().any(|mount| mount.path.starts_with(&prefix)) {
                return Err(FsError::Busy);
            }
            let fs = &state.mounts[parent.mount].fs;
            if let Some(existing) = fs.lookup(parent.node, name) {
                // Only a change of case may land on itself.
                if existing != source.node || parent.mount != source.mount {
                    return Err(FsError::NameExists);
                }
            }
            if source.mount == parent.mount {
                state.check_writable_mount(source.mount)?;
                state.mounts[source.mount]
                    .fs
                    .rename(source.node, parent.node, name)?;
                state.touched(source.mount);
                true
            } else {
                false
            }
        };
        if same_mount {
            self.changed();
            return Ok(());
        }
        // Across filesystems: nothing's removed until the copy is whole.
        let from = normalize_path(from)?;
        let to = normalize_path(to)?;
        if let Err(error) = self.copy_across(&from, &to, 0) {
            let _ = self.remove_tree(&to, 0);
            return Err(error);
        }
        self.remove_tree(&from, 0)
    }

    fn truncate(&self, path: &str, size: u64) -> Result<(), FsError> {
        let mut state = self.lock();
        let (location, _) = state.walk(path, true)?;
        state.require_file(location)?;
        state.check_writable_file(location)?;
        let size = usize::try_from(size).map_err(|_| FsError::NoSpace)?;
        let result = self.resize_hooked(&mut state, location, size);
        state.touched(location.mount);
        result?;
        drop(state);
        self.changed();
        Ok(())
    }

    fn set_attributes(&self, path: &str, attributes: u8) -> Result<(), FsError> {
        let mut state = self.lock();
        let (location, _) = state.walk(path, false)?;
        state.check_writable_mount(location.mount)?;
        // A link's system bit is what makes it a link, whatever the caller asks.
        let keep = state.link_target(location.mount, location.node)?.is_some();
        let attributes = if keep { attributes | attr::SYSTEM } else { attributes };
        state.mounts[location.mount]
            .fs
            .set_attributes(location.node, attributes)?;
        state.touched(location.mount);
        drop(state);
        self.changed();
        Ok(())
    }

    fn set_times(&self, path: &str, times: NodeTimes) -> Result<(), FsError> {
        let mut state = self.lock();
        let (location, _) = state.walk(path, false)?;
        state.check_writable_mount(location.mount)?;
        state.mounts[location.mount].fs.set_times(location.node, times)?;
        state.touched(location.mount);
        drop(state);
        self.changed();
        Ok(())
    }

    fn symlink(&self, target: &str, path: &str) -> Result<(), FsError> {
        self.make_symlink(target, path, None)
    }

    fn symlink_as(&self, target: &str, path: &str, caller: Caller) -> Result<(), FsError> {
        self.make_symlink(target, path, Some(caller))
    }

    fn chmod(&self, path: &str, mode: u16) -> Result<(), FsError> {
        let mut state = self.lock();
        let (location, _) = state.walk(path, true)?;
        let mount = &mut state.mounts[location.mount];
        if mount.read_only {
            return Err(FsError::ReadOnly);
        }
        mount.fs.set_mode(location.node, mode)?;
        mount.dirty = true;
        drop(state);
        self.changed();
        Ok(())
    }

    fn chown(&self, path: &str, uid: Option<u32>, gid: Option<u32>) -> Result<(), FsError> {
        let mut state = self.lock();
        let (location, _) = state.walk(path, true)?;
        let mount = &mut state.mounts[location.mount];
        if mount.read_only {
            return Err(FsError::ReadOnly);
        }
        mount.fs.set_owner(location.node, uid, gid)?;
        mount.dirty = true;
        drop(state);
        self.changed();
        Ok(())
    }

    fn lchown(&self, path: &str, uid: Option<u32>, gid: Option<u32>) -> Result<(), FsError> {
        let mut state = self.lock();
        let (location, _) = state.walk(path, false)?;
        let mount = &mut state.mounts[location.mount];
        if mount.read_only {
            return Err(FsError::ReadOnly);
        }
        mount.fs.set_owner(location.node, uid, gid)?;
        mount.dirty = true;
        drop(state);
        self.changed();
        Ok(())
    }

    fn link(&self, existing: &str, path: &str) -> Result<(), FsError> {
        self.hard_link(existing, path, None)
    }

    fn link_as(&self, existing: &str, path: &str, source: FileId, caller: Caller) -> Result<(), FsError> {
        self.hard_link(existing, path, Some((source, caller)))
    }

    fn check_access(&self, path: &str, uid: u32, gid: u32, want: u8) -> Result<(), FsError> {
        self.lock().check_access(path, Caller { uid, gid }, want)
    }

    fn check_remove(&self, path: &str, uid: u32, gid: u32) -> Result<(), FsError> {
        self.lock().check_remove(path, Caller { uid, gid })
    }

    fn fsck(&self, path: &str, repair: bool) -> Result<crate::native::FsckReport, FsError> {
        let mut state = self.lock();
        let (location, _) = state.walk(path, true)?;
        state.mounts[location.mount].fs.fsck(repair)
    }

    fn read_link(&self, path: &str) -> Result<String, FsError> {
        let state = self.lock();
        let (location, _) = state.walk(path, false)?;
        state
            .link_target(location.mount, location.node)?
            .ok_or(FsError::Unsupported)
    }

    fn open(&self, path: &str, options: OpenOptions) -> Result<Handle, FsError> {
        let mut state = self.lock();
        let mut created = false;
        let location = match state.walk(path, true) {
            Ok(_) if options.create && options.exclusive => return Err(FsError::NameExists),
            Ok((location, _)) => {
                state.require_file(location)?;
                location
            }
            Err(FsError::NotFound) if options.create => {
                let (parent, _, name) = state.walk_parent(path)?;
                created = true;
                state.create_at(parent, name)?
            }
            Err(error) => return Err(error),
        };
        if options.write || options.truncate {
            state.check_writable_file(location)?;
        }
        if options.truncate {
            let result = self.resize_hooked(&mut state, location, 0);
            state.touched(location.mount);
            result?;
        }
        let slot = match state.handles.iter().position(Option::is_none) {
            Some(slot) => slot,
            None => {
                if state.handles.len() >= MAX_HANDLES {
                    return Err(FsError::Busy);
                }
                state
                    .handles
                    .try_reserve(1)
                    .map_err(|_| FsError::OutOfMemory)?;
                state.handles.push(None);
                state.handles.len() - 1
            }
        };
        let serial = state.next_serial;
        state.next_serial = serial.wrapping_add(1);
        state.handles[slot] = Some(OpenFile {
            serial,
            mount: location.mount as u32,
            node: location.node,
            position: 0,
            readable: options.read,
            writable: options.write,
            append: options.append,
            dead: false,
        });
        drop(state);
        if created || options.truncate {
            self.changed();
        }
        Ok(Handle {
            slot: slot as u32,
            serial,
        })
    }

    fn read_handle(&self, handle: Handle, out: &mut [u8]) -> Result<usize, FsError> {
        let mut state = self.lock();
        let open = state.handle_mut(handle)?;
        if !open.readable {
            return Err(FsError::Unsupported);
        }
        let location = Location {
            mount: open.mount as usize,
            node: open.node,
        };
        let position = open.position;
        let count = self.file_data(&state, location, position, out)?;
        state.handle_mut(handle)?.position = position + count as u64;
        Ok(count)
    }

    fn write_handle(&self, handle: Handle, data: &[u8]) -> Result<usize, FsError> {
        let mut state = self.lock();
        let open = state.handle_mut(handle)?;
        if !open.writable {
            return Err(FsError::ReadOnly);
        }
        let location = Location {
            mount: open.mount as usize,
            node: open.node,
        };
        let append = open.append;
        let position = open.position;
        let offset = if append {
            state.mounts[location.mount].fs.size(location.node) as u64
        } else {
            position
        };
        let start = usize::try_from(offset).map_err(|_| FsError::NoSpace)?;
        let result = self.write_hooked(&mut state, location, start, data);
        state.touched(location.mount);
        result?;
        state.handle_mut(handle)?.position = offset + data.len() as u64;
        drop(state);
        self.changed();
        Ok(data.len())
    }

    fn seek_handle(&self, handle: Handle, to: Seek) -> Result<u64, FsError> {
        let mut state = self.lock();
        let open = state.handle_mut(handle)?;
        let (mount, node, position) = (open.mount as usize, open.node, open.position);
        let size = state.mounts[mount].fs.size(node) as u64;
        let target = match to {
            Seek::Start(offset) => Some(offset),
            Seek::Current(delta) => position.checked_add_signed(delta),
            Seek::End(delta) => size.checked_add_signed(delta),
        }
        .ok_or(FsError::OutOfBounds)?;
        state.handle_mut(handle)?.position = target;
        Ok(target)
    }

    fn stat_handle(&self, handle: Handle) -> Result<Stat, FsError> {
        let mut state = self.lock();
        let open = state.handle_mut(handle)?;
        let location = Location {
            mount: open.mount as usize,
            node: open.node,
        };
        state.stat_location(location, false)
    }

    fn close(&self, handle: Handle) -> Result<(), FsError> {
        let mut state = self.lock();
        let slot = state
            .handles
            .get_mut(handle.slot as usize)
            .ok_or(FsError::StaleHandle)?;
        match slot {
            Some(open) if open.serial == handle.serial => {
                *slot = None;
                Ok(())
            }
            _ => Err(FsError::StaleHandle),
        }
    }

    fn sync(&self) -> Result<(), FsError> {
        let mut state = self.lock();
        // Cleared first: a change arriving during the flush must not be lost.
        self.dirty.store(false, Ordering::Release);
        let mut result = Ok(());
        for mount in state.mounts.iter_mut() {
            if mount.dirty {
                match mount.fs.flush() {
                    Ok(()) => mount.dirty = false,
                    Err(error) => {
                        self.dirty.store(true, Ordering::Release);
                        result = Err(error);
                    }
                }
            }
        }
        result
    }

    fn sync_if_dirty(&self) -> Result<(), FsError> {
        if !self.dirty.load(Ordering::Acquire) {
            return Ok(());
        }
        self.sync()
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn set_change_hook(&self, hook: fn()) {
        *self.change_hook.lock() = Some(hook);
    }

    fn mount(
        &self,
        path: &str,
        mut fs: Box<dyn WritableFileSystem>,
        read_only: bool,
        source: &str,
    ) -> Result<(), FsError> {
        let path = normalize_path(path)?;
        if path == "/" {
            return Err(FsError::Busy);
        }
        let mut state = self.lock();
        if state.mounts.len() >= MAX_MOUNTS {
            return Err(FsError::Busy);
        }
        if state.is_mount_point(&path) {
            return Err(FsError::Busy);
        }
        // The parent has to be reachable, or nothing could list the mount.
        state.walk(parent_of(&path), true)?;
        if let Some(now) = state.now {
            fs.set_time_source(now);
        }
        let dev = state.next_dev;
        state.next_dev += 1;
        state.mounts.push(Mount {
            path: path.clone(),
            dev,
            fs,
            read_only,
            source: source.to_string(),
            dirty: false,
        });
        drop(state);
        self.generation.fetch_add(1, Ordering::Release);
        // What a crash left orphaned: nothing can be using it now.
        if !read_only {
            let _ = self.clean_orphans(&path);
        }
        Ok(())
    }

    fn unmount(&self, path: &str) -> Result<(), FsError> {
        let path = normalize_path(path)?;
        let mut state = self.lock();
        if path == "/" {
            return Err(FsError::Busy);
        }
        let index = state.mount_at(&path).ok_or(FsError::NotFound)?;
        let inside = join_path(&path, "");
        if state.mounts.iter().any(|mount| mount.path.starts_with(&inside)) {
            return Err(FsError::Busy);
        }
        if state
            .handles
            .iter()
            .flatten()
            .any(|open| open.mount == index as u32 && !open.dead)
        {
            return Err(FsError::Busy);
        }
        // A file something holds, as the page cache does, keeps its mount.
        let dev = state.mounts[index].dev;
        if let Some(hook) = self.hook() {
            hook.unmounting(dev);
        }
        if self.held.lock().iter().any(|held| held.file.dev == dev) {
            return Err(FsError::Busy);
        }
        state.mounts[index].fs.flush()?;
        state.mounts.remove(index);
        // Handles index mounts: every later mount moved down one.
        for open in state.handles.iter_mut().flatten() {
            if open.mount == index as u32 {
                open.dead = true;
            } else if open.mount > index as u32 {
                open.mount -= 1;
            }
        }
        drop(state);
        self.generation.fetch_add(1, Ordering::Release);
        Ok(())
    }

    fn mounts(&self) -> Vec<MountInfo> {
        let state = self.lock();
        state
            .mounts
            .iter()
            .map(|mount| MountInfo {
                path: mount.path.clone(),
                stats: mount.fs.stats(),
                read_only: mount.read_only,
                source: mount.source.clone(),
            })
            .collect()
    }

    fn make_device(&self, path: &str, kind: DeviceKind) -> Result<(), FsError> {
        let mut state = self.lock();
        let (parent, _, name) = state.walk_parent(path)?;
        // A device is a file whose content the mount generates, so only a mount
        // that can do that may hold one. Nothing is left behind if it can't.
        let fs = &mut state.mounts[parent.mount].fs;
        let node = fs.create_file(parent.node, name)?;
        if let Err(error) = fs.make_device(node, kind) {
            let _ = fs.remove(node);
            return Err(error);
        }
        drop(state);
        self.changed();
        Ok(())
    }

    fn identify(&self, path: &str) -> Result<(FileId, Stat), FsError> {
        let state = self.lock();
        let (location, _) = state.walk(path, true)?;
        Ok((state.file_id(location), state.stat_location(location, false)?))
    }

    fn read_file(&self, file: FileId, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
        let state = self.lock();
        let hint = self.held.lock().iter().find(|held| held.file == file).map(|held| held.node);
        let location = state.find(file, hint)?;
        state.require_file(location)?;
        self.file_data(&state, location, offset, out)
    }

    fn stat_file(&self, file: FileId) -> Result<Stat, FsError> {
        let state = self.lock();
        let hint = self.held.lock().iter().find(|held| held.file == file).map(|held| held.node);
        let location = state.find(file, hint)?;
        state.stat_location(location, false)
    }

    fn write_back(&self, file: FileId, offset: u64, buffer: &mut [u8], fill: &mut dyn FnMut(&mut [u8]) -> usize) -> Result<(), FsError> {
        let mut state = self.lock();
        let hint = self.held.lock().iter().find(|held| held.file == file).map(|held| held.node);
        let location = state.find(file, hint)?;
        state.require_file(location)?;
        state.check_writable_file(location)?;
        let length = fill(buffer).min(buffer.len());
        if length == 0 {
            return Ok(());
        }
        let size = state.mounts[location.mount].fs.size(location.node) as u64;
        let length = length.min(size.saturating_sub(offset) as usize);
        if length == 0 {
            return Ok(());
        }
        let offset = usize::try_from(offset).map_err(|_| FsError::OutOfBounds)?;
        let result = state.write_all(location, offset, &buffer[..length]);
        state.touched(location.mount);
        drop(state);
        self.changed();
        result
    }

    fn hold(&self, file: FileId) -> Result<(), FsError> {
        let mut state = self.lock();
        let mut held = self.held.lock();
        if let Some(entry) = held.iter_mut().find(|entry| entry.file == file) {
            entry.count = entry.count.checked_add(1).ok_or(FsError::Busy)?;
            return Ok(());
        }
        held.try_reserve(1).map_err(|_| FsError::OutOfMemory)?;
        drop(held);
        let location = state.find(file, None)?;
        state.mounts[location.mount].fs.keep_id(location.node, file.ino, true)?;
        let mut held = self.held.lock();
        // Nothing else can have added it: holds are made under the tree's lock.
        if held.try_reserve(1).is_err() {
            drop(held);
            let _ = state.mounts[location.mount].fs.keep_id(location.node, file.ino, false);
            return Err(FsError::OutOfMemory);
        }
        held.push(Held { file, count: 1, node: location.node });
        Ok(())
    }

    fn release(&self, file: FileId) {
        let mut held = self.held.lock();
        let Some(at) = held.iter().position(|entry| entry.file == file) else {
            return;
        };
        held[at].count -= 1;
        if held[at].count > 0 {
            return;
        }
        let node = held.swap_remove(at).node;
        drop(held);
        // If there's no room to say so, the number is just never reused.
        let mut let_go = self.let_go.lock();
        if let_go.try_reserve(1).is_ok() {
            let_go.push((file, node));
        }
    }

    fn set_file_hook(&self, hook: &'static dyn FileHook) {
        *self.file_hook.lock() = Some(hook);
    }

    fn resolve_path(&self, path: &str, follow_last: bool) -> Result<Resolved, FsError> {
        let state = self.lock();
        let (location, canonical) = state.walk(path, follow_last)?;
        let is_link = !follow_last
            && canonical != "/"
            && !state.is_mount_point(&canonical)
            && state.link_target(location.mount, location.node)?.is_some();
        let stat = state.stat_location(location, is_link)?;
        Ok(Resolved { id: state.file_id(location), stat, canonical })
    }

    fn write_file_at(&self, file: FileId, offset: u64, data: &[u8]) -> Result<usize, FsError> {
        let mut state = self.lock();
        let hint = self.held.lock().iter().find(|held| held.file == file).map(|held| held.node);
        let location = state.find(file, hint)?;
        state.require_file(location)?;
        state.check_writable_file(location)?;
        let offset = usize::try_from(offset).map_err(|_| FsError::NoSpace)?;
        let result = self.write_hooked(&mut state, location, offset, data);
        state.touched(location.mount);
        drop(state);
        self.changed();
        result.map(|()| data.len())
    }

    fn truncate_file(&self, file: FileId, size: u64) -> Result<(), FsError> {
        let mut state = self.lock();
        let hint = self.held.lock().iter().find(|held| held.file == file).map(|held| held.node);
        let location = state.find(file, hint)?;
        state.require_file(location)?;
        state.check_writable_file(location)?;
        let size = usize::try_from(size).map_err(|_| FsError::NoSpace)?;
        let result = self.resize_hooked(&mut state, location, size);
        state.touched(location.mount);
        result?;
        drop(state);
        self.changed();
        Ok(())
    }

    fn orphan(&self, path: &str) -> Result<String, FsError> {
        let mut state = self.lock();
        let (location, canonical) = state.walk(path, false)?;
        if canonical == "/" || state.is_mount_point(&canonical) {
            return Err(FsError::Busy);
        }
        state.require_file(location)?;
        state.check_writable_file(location)?;
        let file = state.file_id(location);
        let name = format!("{}-{}", file.dev, file.ino);
        let directory = state.orphan_directory(location.mount)?;
        let fs = &mut state.mounts[location.mount].fs;
        if fs.lookup(directory, &name).is_some() {
            return Err(FsError::NameExists);
        }
        fs.rename(location.node, directory, &name)?;
        state.touched(location.mount);
        let at = join_path(&join_path(&state.mounts[location.mount].path, ORPHANS), &name);
        drop(state);
        self.changed();
        Ok(at)
    }

    fn rename_replacing(
        &self,
        from: &str,
        to: &str,
        no_replace: bool,
        caller: Option<Caller>,
        in_use: &dyn Fn(FileId) -> bool,
    ) -> Result<Option<(FileId, String)>, FsError> {
        let mut state = self.lock();
        let (source, canonical_from) = state.walk(from, false)?;
        // Where the walk got to is what's judged, not what the caller saw before: a
        // directory on the way may have been swapped for a link since.
        if caller.is_some() && path::is_hidden(&canonical_from) {
            return Err(FsError::NotFound);
        }
        if canonical_from == "/" || state.is_mount_point(&canonical_from) {
            return Err(FsError::Busy);
        }
        let (parent, canonical_parent, name) = state.walk_parent(to)?;
        let target = join_path(&canonical_parent, name);
        if caller.is_some() && path::is_hidden(&target) {
            return Err(FsError::NotFound);
        }
        let existing = state.mounts[parent.mount].fs.lookup(parent.node, name);
        // Anything there, itself included, as Linux's `renameat2` has it.
        if no_replace && (existing.is_some() || state.is_mount_point(&target)) {
            return Err(FsError::NameExists);
        }
        if target == canonical_from {
            return Ok(None);
        }
        if let Some(caller) = caller {
            state.check_remove(&canonical_from, caller)?;
            if existing.is_some() {
                state.check_remove(&target, caller)?;
            } else {
                state.check_parent(&canonical_parent, caller)?;
            }
        }
        if state.is_mount_point(&target) {
            return Err(FsError::Busy);
        }
        if parent.mount != source.mount {
            return Err(FsError::CrossDevice);
        }
        if is_within(&target, &canonical_from) {
            return Err(FsError::InvalidMove);
        }
        // A mount inside what's moving would lose its path, as would one in what goes.
        let prefix = join_path(&canonical_from, "");
        let below = join_path(&target, "");
        if state.mounts.iter().any(|mount| mount.path.starts_with(&prefix) || mount.path.starts_with(&below)) {
            return Err(FsError::Busy);
        }
        state.check_writable_mount(source.mount)?;
        let mount = source.mount;
        let fs = &state.mounts[mount].fs;
        let moving_directory = fs.kind(source.node) == Some(EntryKind::Directory);
        let victim = match existing {
            // Only a change of case lands on itself.
            None => None,
            Some(existing) if existing == source.node => None,
            Some(existing) => Some(existing),
        };
        let Some(victim) = victim else {
            state.mounts[mount].fs.rename(source.node, parent.node, name)?;
            state.touched(mount);
            drop(state);
            self.changed();
            return Ok(None);
        };
        let victim_location = Location { mount, node: victim };
        let victim_id = state.file_id(victim_location);
        // Two names of one file: POSIX has nothing happen.
        if victim_id == state.file_id(source) {
            return Ok(None);
        }
        let victim_directory = fs.kind(victim) == Some(EntryKind::Directory);
        match (moving_directory, victim_directory) {
            (true, false) => return Err(FsError::NotDirectory),
            (false, true) => return Err(FsError::IsDirectory),
            (true, true) if fs.child_count(victim) > 0 => return Err(FsError::NotEmpty),
            _ => {}
        }
        if !victim_directory {
            state.check_writable_file(victim_location)?;
        }
        let is_link = state.link_target(mount, victim)?.is_some();
        let fs = &state.mounts[mount].fs;
        let last_link = fs.posix(victim).map_or(1, |meta| meta.nlink) <= 1;
        let regular = !victim_directory && !is_link && fs.device(victim).is_none();
        let keep = regular && last_link && in_use(victim_id);
        let orphan_name = format!("{}-{}", victim_id.dev, victim_id.ino);
        let orphans = if keep { Some(state.orphan_directory(mount)?) } else { None };
        let fs = &mut state.mounts[mount].fs;
        if let Some(directory) = orphans {
            if fs.lookup(directory, &orphan_name).is_some() {
                return Err(FsError::NameExists);
            }
        }
        fs.rename_over(source.node, parent.node, name, victim, orphans.map(|directory| (directory, orphan_name.as_str())))?;
        state.touched(mount);
        let kept = if keep {
            Some((victim_id, join_path(&join_path(&state.mounts[mount].path, ORPHANS), &orphan_name)))
        } else {
            state.kill_handles(mount, victim);
            // Its bytes go with its last name; another name keeps them.
            if !victim_directory && last_link {
                if let Some(hook) = self.hook() {
                    hook.removed(victim_id);
                }
            }
            None
        };
        drop(state);
        self.changed();
        Ok(kept)
    }

    fn chmod_file(&self, file: FileId, mode: u16, allow: Allow<'_>) -> Result<(), FsError> {
        self.change_file(file, allow, &mut |fs, node| fs.set_mode(node, mode))
    }

    fn chown_file(&self, file: FileId, uid: Option<u32>, gid: Option<u32>) -> Result<(), FsError> {
        self.change_file(file, &|_| Ok(()), &mut |fs, node| fs.set_owner(node, uid, gid))
    }

    fn set_times_file(&self, file: FileId, times: NodeTimes, allow: Allow<'_>) -> Result<(), FsError> {
        self.change_file(file, allow, &mut |fs, node| fs.set_times(node, times))
    }

    fn statfs_file(&self, file: FileId) -> Result<VolumeStats, FsError> {
        let state = self.lock();
        let mount = state.mounts.iter().find(|mount| mount.dev == file.dev).ok_or(FsError::NotFound)?;
        let mut stats = mount.fs.stats();
        stats.read_only |= mount.read_only;
        Ok(stats)
    }

    fn set_time_source(&self, now: TimeSource) {
        let mut state = self.lock();
        state.now = Some(now);
        for mount in state.mounts.iter_mut() {
            mount.fs.set_time_source(now);
        }
    }

    fn clean_orphans(&self, mount_point: &str) -> Result<usize, FsError> {
        let directory = join_path(&normalize_path(mount_point)?, ORPHANS);
        let entries = match self.read_dir(&directory) {
            Ok(entries) => entries,
            Err(FsError::NotFound) => return Ok(0),
            Err(error) => return Err(error),
        };
        let mut removed = 0;
        for entry in entries.iter().filter(|entry| entry.stat.kind != FileKind::Directory) {
            if self.remove_file(&join_path(&directory, &entry.name)).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// The read-only, node-based view, so existing readers see the live tree.
impl FileSystem for Vfs {
    fn node_count(&self) -> usize {
        self.interner.lock().paths.len()
    }

    fn kind(&self, node: NodeId) -> Option<EntryKind> {
        let path = self.interner.lock().path(node)?;
        match self.stat(path).ok()?.kind {
            FileKind::Directory => Some(EntryKind::Directory),
            _ => Some(EntryKind::File),
        }
    }

    fn name(&self, node: NodeId) -> Option<&str> {
        let path = self.interner.lock().path(node)?;
        Some(if path == "/" { "/" } else { base_of(path) })
    }

    fn parent(&self, node: NodeId) -> Option<NodeId> {
        let mut interner = self.interner.lock();
        let path = interner.path(node)?;
        if path == "/" {
            return None;
        }
        interner.intern(parent_of(path))
    }

    fn size(&self, node: NodeId) -> usize {
        let Some(path) = self.interner.lock().path(node) else {
            return 0;
        };
        self.stat(path).map_or(0, |stat| stat.size as usize)
    }

    fn read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> usize {
        self.try_read(node, offset, out).unwrap_or(0)
    }

    fn try_read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> Result<usize, FsError> {
        let path = self.interner.lock().path(node).ok_or(FsError::NotFound)?;
        self.read_at(path, offset as u64, out).map_err(|error| match error {
            FsError::IsDirectory => FsError::NotFile,
            other => other,
        })
    }

    fn child_count(&self, node: NodeId) -> usize {
        self.with_listing(node, |children| children.len()).unwrap_or(0)
    }

    fn child_at(&self, node: NodeId, index: usize) -> Option<NodeId> {
        self.with_listing(node, |children| children.get(index).copied())?
    }

    fn lookup(&self, dir: NodeId, name: &str) -> Option<NodeId> {
        if name.is_empty() || name.contains('/') {
            return None;
        }
        let path = self.interner.lock().path(dir)?;
        let child = join_path(path, name);
        self.stat(&child).ok()?;
        self.interner.lock().intern(&child)
    }

    fn resolve(&self, path: &str) -> Option<NodeId> {
        // Going above the root fails here, as it does for other filesystems.
        let mut depth = 0i32;
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    depth -= 1;
                    if depth < 0 {
                        return None;
                    }
                }
                _ => depth += 1,
            }
        }
        let normal = normalize_path(path).ok()?;
        self.stat(&normal).ok()?;
        self.interner.lock().intern(&normal)
    }

    fn path<'a>(&self, node: NodeId, out: &'a mut [u8]) -> Option<&'a str> {
        let path = self.interner.lock().path(node)?;
        let target = out.get_mut(..path.len())?;
        target.copy_from_slice(path.as_bytes());
        core::str::from_utf8(target).ok()
    }

    fn attributes(&self, node: NodeId) -> u8 {
        let Some(path) = self.interner.lock().path(node) else {
            return 0;
        };
        self.stat(path).map_or(0, |stat| stat.attributes)
    }

    fn times(&self, node: NodeId) -> NodeTimes {
        let Some(path) = self.interner.lock().path(node) else {
            return NodeTimes::NONE;
        };
        self.stat(path).map_or(NodeTimes::NONE, |stat| stat.times)
    }

    fn stats(&self) -> VolumeStats {
        self.statfs("/").unwrap_or_default()
    }

    fn is_damaged(&self) -> bool {
        self.lock().mounts.iter().any(|mount| mount.fs.is_damaged())
    }

    fn tree(&self) -> Option<&dyn VfsTree> {
        Some(self)
    }
}

impl Vfs {
    /// Runs `action` on the children of `node`, listing them if the last listing
    /// was of something else or the tree has changed since.
    fn with_listing<R>(&self, node: NodeId, action: impl FnOnce(&[NodeId]) -> R) -> Option<R> {
        let generation = self.generation();
        let interner = self.interner.lock();
        if let Some((id, seen, children)) = interner.listing.as_ref() {
            if *id == node.0 && *seen == generation {
                return Some(action(children));
            }
        }
        let path = interner.path(node)?;
        drop(interner);
        // The tree's lock is taken with the interner's released: `kind` and
        // friends hold them in the same order, one after the other.
        let entries = self.read_dir(path).ok()?;
        let mut interner = self.interner.lock();
        let mut children = Vec::new();
        children.try_reserve_exact(entries.len()).ok()?;
        for entry in entries {
            if let Some(id) = interner.intern(&join_path(path, &entry.name)) {
                children.push(id);
            }
        }
        let result = action(&children);
        interner.listing = Some((node.0, generation, children));
        Some(result)
    }
}

#[cfg(test)]
mod tests;
