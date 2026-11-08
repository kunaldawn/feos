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
mod ramfs;

pub use boot::{assemble, BootLog, Embedded};
pub use ramfs::{DeviceKind, RamFs, DEFAULT_QUOTA};

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

/// Symbolic links followed in one resolution.
const MAX_SYMLINK_HOPS: usize = 8;
/// A symbolic link is a file with the system attribute that starts with this.
/// FAT has nothing better, and this way a link is just a small odd file to any
/// other OS, and survives a copy to one and back.
const SYMLINK_MAGIC: &[u8] = b"XSym\n";
/// Longest link target.
const MAX_LINK_TARGET: usize = 1024;
/// Open files at once.
const MAX_HANDLES: usize = 256;
/// Longest path this tree accepts.
const MAX_PATH_BYTES: usize = 4096;
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
    /// A number for this file, unique on its filesystem.
    pub ino: u64,
    /// The filesystem keeps the owner and mode, and they're enforced. FAT, exFAT and RAM
    /// mounts don't, and everyone may do anything there that the mount allows.
    pub posix: bool,
}

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

/// One entry of a directory listing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TreeEntry {
    /// Final path component.
    pub name: String,
    /// Its facts, not following a link.
    pub stat: Stat,
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

    /// Sets permission bits. `Unsupported` on a filesystem that doesn't keep them.
    fn chmod(&self, _path: &str, _mode: u16) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Sets whichever of the owner and group are `Some`.
    fn chown(&self, _path: &str, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Gives the file at `existing` another name, `path`, on the same filesystem.
    fn link(&self, _existing: &str, _path: &str) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Whether a user may do `want` (`ACCESS_*`) to `path`, having to be able to go through
    /// every directory on the way. Only filesystems with owners say no. It's for the system
    /// call boundary: kernel-linked apps run as root and aren't checked at all.
    fn check_access(&self, _path: &str, _uid: u32, _gid: u32, _want: u8) -> Result<(), FsError> {
        Ok(())
    }

    /// Checks the filesystem holding `path` through and through, if it has a checker.
    fn fsck(&self, _path: &str, _repair: bool) -> Result<crate::native::FsckReport, FsError> {
        Err(FsError::Unsupported)
    }
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
            posix: self.posix,
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
                    fs: root,
                    read_only,
                    source: source.to_string(),
                    dirty: false,
                }],
                handles: Vec::new(),
                next_serial: 1,
                now: None,
            }),
            generation: AtomicU64::new(1),
            dirty: AtomicBool::new(false),
            change_hook: SpinLock::new(None),
            interner: Mutex::new(interner),
        }
    }

    /// An empty tree in RAM, which is what tests and simple boots want.
    pub fn in_memory(quota: usize) -> Self {
        Self::new(Box::new(RamFs::new(quota)), false, "ram")
    }

    /// Sets the clock used for timestamps, on every mount present and to come.
    pub fn set_time_source(&self, now: TimeSource) {
        let mut state = self.state.lock();
        state.now = Some(now);
        for mount in state.mounts.iter_mut() {
            mount.fs.set_time_source(now);
        }
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
        let state = self.state.lock();
        let (location, _) = state.walk(path, true)?;
        state.stat_location(location, false)
    }

    fn lstat(&self, path: &str) -> Result<Stat, FsError> {
        let state = self.state.lock();
        let (location, canonical) = state.walk(path, false)?;
        let is_link = canonical != "/"
            && !state.is_mount_point(&canonical)
            && state.link_target(location.mount, location.node)?.is_some();
        state.stat_location(location, is_link)
    }

    fn statfs(&self, path: &str) -> Result<VolumeStats, FsError> {
        let state = self.state.lock();
        let (location, _) = state.walk(path, true)?;
        let mount = &state.mounts[location.mount];
        let mut stats = mount.fs.stats();
        stats.read_only |= mount.read_only;
        Ok(stats)
    }

    fn read_dir(&self, path: &str) -> Result<Vec<TreeEntry>, FsError> {
        let state = self.state.lock();
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
            let Some(name) = fs.name(child) else {
                continue;
            };
            let is_link = state.link_target(location.mount, child)?.is_some();
            let stat = state.stat_location(
                Location {
                    mount: location.mount,
                    node: child,
                },
                is_link,
            )?;
            entries.try_reserve(1).map_err(|_| FsError::OutOfMemory)?;
            entries.push(TreeEntry {
                name: name.to_string(),
                stat,
            });
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
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    fn read_at(&self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, FsError> {
        let state = self.state.lock();
        let (location, _) = state.walk(path, true)?;
        state.require_file(location)?;
        self.file_data(&state, location, offset, out)
    }

    fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize, FsError> {
        let mut state = self.state.lock();
        let (location, _) = state.walk(path, true)?;
        state.require_file(location)?;
        state.check_writable_file(location)?;
        let offset = usize::try_from(offset).map_err(|_| FsError::NoSpace)?;
        state.write_all(location, offset, data)?;
        state.touched(location.mount);
        drop(state);
        self.changed();
        Ok(data.len())
    }

    fn write_file(&self, path: &str, data: &[u8], append: bool) -> Result<(), FsError> {
        let mut state = self.state.lock();
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
            state.mounts[location.mount].fs.set_len(location.node, 0)?;
            0
        };
        state.write_all(location, offset, data)?;
        state.touched(location.mount);
        drop(state);
        self.changed();
        Ok(())
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
        match self.remove_file(&normal) {
            Ok(()) | Err(FsError::NotFound) => {}
            Err(error) => return Err(error),
        }
        self.rename(&temporary, &normal)?;
        self.sync()
    }

    fn create_file(&self, path: &str) -> Result<(), FsError> {
        let mut state = self.state.lock();
        let (parent, canonical, name) = state.walk_parent(path)?;
        if state.is_mount_point(&join_path(&canonical, name)) {
            return Err(FsError::NameExists);
        }
        state.create_at(parent, name)?;
        drop(state);
        self.changed();
        Ok(())
    }

    fn create_dir(&self, path: &str) -> Result<(), FsError> {
        let mut state = self.state.lock();
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
        let mut state = self.state.lock();
        let (location, canonical) = state.walk(path, false)?;
        if state.is_mount_point(&canonical) {
            return Err(FsError::Busy);
        }
        state.require_file(location)?;
        state.check_writable_file(location)?;
        state.mounts[location.mount].fs.remove(location.node)?;
        state.kill_handles(location.mount, location.node);
        state.touched(location.mount);
        drop(state);
        self.changed();
        Ok(())
    }

    fn remove_dir(&self, path: &str) -> Result<(), FsError> {
        let mut state = self.state.lock();
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
            let mut state = self.state.lock();
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
        let mut state = self.state.lock();
        let (location, _) = state.walk(path, true)?;
        state.require_file(location)?;
        state.check_writable_file(location)?;
        let size = usize::try_from(size).map_err(|_| FsError::NoSpace)?;
        state.mounts[location.mount].fs.set_len(location.node, size)?;
        state.touched(location.mount);
        drop(state);
        self.changed();
        Ok(())
    }

    fn set_attributes(&self, path: &str, attributes: u8) -> Result<(), FsError> {
        let mut state = self.state.lock();
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
        let mut state = self.state.lock();
        let (location, _) = state.walk(path, false)?;
        state.check_writable_mount(location.mount)?;
        state.mounts[location.mount].fs.set_times(location.node, times)?;
        state.touched(location.mount);
        drop(state);
        self.changed();
        Ok(())
    }

    fn symlink(&self, target: &str, path: &str) -> Result<(), FsError> {
        if target.is_empty() || target.len() > MAX_LINK_TARGET {
            return Err(FsError::InvalidName);
        }
        let mut state = self.state.lock();
        let (parent, _, name) = state.walk_parent(path)?;
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
        drop(state);
        self.changed();
        Ok(())
    }

    fn chmod(&self, path: &str, mode: u16) -> Result<(), FsError> {
        let mut state = self.state.lock();
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
        let mut state = self.state.lock();
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

    fn link(&self, existing: &str, path: &str) -> Result<(), FsError> {
        let mut state = self.state.lock();
        let (from, _) = state.walk(existing, false)?;
        let (parent, _, name) = state.walk_parent(path)?;
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

    fn check_access(&self, path: &str, uid: u32, gid: u32, want: u8) -> Result<(), FsError> {
        let state = self.state.lock();
        // A step at a time, each directory judged before anything past it is looked at.
        // Otherwise whether a name exists in a closed directory would show in the answer.
        // A link is judged by what it leads to.
        let mut here = state.walk("/", true)?.0;
        let mut prefix = String::new();
        for part in path.split('/').filter(|part| !part.is_empty() && *part != ".") {
            let fs = &state.mounts[here.mount].fs;
            if let Some(meta) = fs.posix(here.node) {
                if !permits(meta.mode, true, meta.uid, meta.gid, uid, gid, ACCESS_EXECUTE) {
                    return Err(FsError::PermissionDenied);
                }
            }
            prefix.push('/');
            prefix.push_str(part);
            here = state.walk(&prefix, true)?.0;
        }
        let fs = &state.mounts[here.mount].fs;
        if let Some(meta) = fs.posix(here.node) {
            let is_directory = fs.kind(here.node) == Some(EntryKind::Directory);
            if !permits(meta.mode, is_directory, meta.uid, meta.gid, uid, gid, want) {
                return Err(FsError::PermissionDenied);
            }
        }
        Ok(())
    }

    fn fsck(&self, path: &str, repair: bool) -> Result<crate::native::FsckReport, FsError> {
        let mut state = self.state.lock();
        let (location, _) = state.walk(path, true)?;
        state.mounts[location.mount].fs.fsck(repair)
    }

    fn read_link(&self, path: &str) -> Result<String, FsError> {
        let state = self.state.lock();
        let (location, _) = state.walk(path, false)?;
        state
            .link_target(location.mount, location.node)?
            .ok_or(FsError::Unsupported)
    }

    fn open(&self, path: &str, options: OpenOptions) -> Result<Handle, FsError> {
        let mut state = self.state.lock();
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
            state.mounts[location.mount].fs.set_len(location.node, 0)?;
            state.touched(location.mount);
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
        let mut state = self.state.lock();
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
        let mut state = self.state.lock();
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
        state.write_all(location, start, data)?;
        state.touched(location.mount);
        state.handle_mut(handle)?.position = offset + data.len() as u64;
        drop(state);
        self.changed();
        Ok(data.len())
    }

    fn seek_handle(&self, handle: Handle, to: Seek) -> Result<u64, FsError> {
        let mut state = self.state.lock();
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
        let mut state = self.state.lock();
        let open = state.handle_mut(handle)?;
        let location = Location {
            mount: open.mount as usize,
            node: open.node,
        };
        state.stat_location(location, false)
    }

    fn close(&self, handle: Handle) -> Result<(), FsError> {
        let mut state = self.state.lock();
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
        let mut state = self.state.lock();
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
        let mut state = self.state.lock();
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
        state.mounts.push(Mount {
            path,
            fs,
            read_only,
            source: source.to_string(),
            dirty: false,
        });
        drop(state);
        self.generation.fetch_add(1, Ordering::Release);
        Ok(())
    }

    fn unmount(&self, path: &str) -> Result<(), FsError> {
        let path = normalize_path(path)?;
        let mut state = self.state.lock();
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
        let state = self.state.lock();
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
        let mut state = self.state.lock();
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
        self.state.lock().mounts.iter().any(|mount| mount.fs.is_damaged())
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
