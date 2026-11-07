//! A writable in-memory overlay on top of the board's read-only filesystem.
//!
//! The board compiles a small read-only image into the kernel (`feos-fs`).
//! A real terminal needs `mkdir`, `cp`, `mv`, `rm`, output redirection and
//! friends, so this module keeps every mutation in a `BTreeMap` overlay and
//! leaves the immutable base image untouched. Reads fall through to the base,
//! writes are captured in RAM, and deletes are recorded as tombstones.
//!
//! The implementation is deliberately lexical: there are no symlinks, hard
//! links, permissions or timestamps yet. Paths are normalised relative to a
//! current working directory and never escape the root.

use alloc::collections::BTreeMap;
use alloc::format;
use core::ops::Bound;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use feos_fs::{FileKind, FileSystem, VfsTree};

/// What a filesystem entry is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
}

/// Metadata for one path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Meta {
    /// File or directory.
    pub kind: Kind,
    /// Size in bytes (zero for directories).
    pub size: usize,
}

/// One directory entry produced by [`Vfs::read_dir`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DirEntry {
    /// Final path component.
    pub name: String,
    /// File or directory.
    pub kind: Kind,
    /// Size in bytes (zero for directories).
    pub size: usize,
}

/// Errors shared by the virtual filesystem and the utilities.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsError {
    /// The path does not exist.
    NotFound,
    /// A path component is not a directory.
    NotDir,
    /// The path is a directory where a file was expected.
    IsDir,
    /// The path already exists.
    Exists,
    /// A directory is not empty.
    NotEmpty,
    /// The path is malformed or invalid for the operation.
    InvalidPath,
    /// The operation is not supported by this filesystem.
    NotSupported,
    /// The file's owner and mode say no.
    PermissionDenied,
    /// The file does not fit the overlay quota or the heap.
    NoSpace,
    /// The path is nested too deeply.
    TooDeep,
    /// The filesystem or the file refuses writes.
    ReadOnly,
    /// The disk failed, or what's on it makes no sense.
    Io,
    /// Something is using it.
    Busy,
    /// Symbolic links went round too many times.
    Loop,
}

impl FsError {
    /// A short human-readable description.
    pub const fn text(self) -> &'static str {
        match self {
            Self::NotFound => "No such file or directory",
            Self::NotDir => "Not a directory",
            Self::IsDir => "Is a directory",
            Self::Exists => "File exists",
            Self::NotEmpty => "Directory not empty",
            Self::InvalidPath => "Invalid path",
            Self::NotSupported => "Operation not supported",
            Self::NoSpace => "No space left on device",
            Self::TooDeep => "Path is nested too deeply",
            Self::ReadOnly => "Read-only file system",
            Self::Io => "Input/output error",
            Self::Busy => "Device or resource busy",
            Self::Loop => "Too many levels of symbolic links",
            Self::PermissionDenied => "Permission denied",
        }
    }
}

/// A writable filesystem view used by the core utilities.
pub trait Vfs {
    /// The current working directory, always absolute.
    fn cwd(&self) -> &str;

    /// Changes the working directory.
    fn chdir(&mut self, path: &str) -> Result<(), FsError>;

    /// Returns metadata for `path`.
    fn meta(&self, path: &str) -> Result<Meta, FsError>;

    /// Facts about `path` itself, with its owner and mode: what `ls -l` and `stat` show.
    fn stat_full(&self, _path: &str) -> Option<feos_fs::Stat> {
        None
    }

    /// Sets permission bits.
    fn chmod(&mut self, _path: &str, _mode: u16) -> Result<(), FsError> {
        Err(FsError::NotSupported)
    }

    /// Sets the owner, the group, or both.
    fn chown(&mut self, _path: &str, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), FsError> {
        Err(FsError::NotSupported)
    }

    /// Gives a file another name.
    fn hard_link(&mut self, _existing: &str, _path: &str) -> Result<(), FsError> {
        Err(FsError::NotSupported)
    }

    /// Checks the filesystem holding `path`.
    fn fsck(&mut self, _path: &str, _repair: bool) -> Result<feos_fs::FsckReport, FsError> {
        Err(FsError::NotSupported)
    }

    /// Reads a whole file into memory.
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, FsError>;

    /// Writes a whole file. When `append` is true the existing bytes are kept.
    fn write_file(&mut self, path: &str, data: &[u8], append: bool) -> Result<(), FsError>;

    /// Creates a single directory.
    fn create_dir(&mut self, path: &str) -> Result<(), FsError>;

    /// Removes a regular file.
    fn remove_file(&mut self, path: &str) -> Result<(), FsError>;

    /// Removes an empty directory.
    fn remove_dir(&mut self, path: &str) -> Result<(), FsError>;

    /// Renames `from` to `to`. The destination must not exist.
    fn rename(&mut self, from: &str, to: &str) -> Result<(), FsError>;

    /// Lists a directory, sorted by name.
    fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>, FsError>;

    /// Reads up to `out.len()` bytes at `offset`, returning the count. Reading
    /// at or past the end of the file returns zero.
    fn read_at(&mut self, path: &str, offset: usize, out: &mut [u8]) -> Result<usize, FsError> {
        let data = self.read_file(path)?;
        let tail = data.get(offset..).unwrap_or(&[]);
        let count = tail.len().min(out.len());
        out[..count].copy_from_slice(&tail[..count]);
        Ok(count)
    }

    /// Resolves `path` against the current working directory.
    fn resolve(&self, path: &str) -> String {
        normalize(self.cwd(), path)
    }

    /// Returns `true` when `path` exists.
    fn exists(&self, path: &str) -> bool {
        self.meta(path).is_ok()
    }

    /// Returns `true` when `path` is a directory.
    fn is_dir(&self, path: &str) -> bool {
        self.meta(path)
            .map(|meta| meta.kind == Kind::Dir)
            .unwrap_or(false)
    }

    /// Timestamps of `path`, where the filesystem keeps them.
    fn times(&self, _path: &str) -> Option<feos_fs::NodeTimes> {
        None
    }

    /// Sets whichever timestamps are `Some`.
    fn set_times(&mut self, _path: &str, _times: feos_fs::NodeTimes) -> Result<(), FsError> {
        Err(FsError::NotSupported)
    }

    /// Size and free space of the filesystem holding `path`.
    fn statfs(&self, _path: &str) -> Result<feos_fs::VolumeStats, FsError> {
        Err(FsError::NotSupported)
    }

    /// Creates a symbolic link at `path` pointing at `target`.
    fn symlink(&mut self, _target: &str, _path: &str) -> Result<(), FsError> {
        Err(FsError::NotSupported)
    }

    /// The target of the symbolic link at `path`.
    fn read_link(&self, _path: &str) -> Result<String, FsError> {
        Err(FsError::NotSupported)
    }

    /// Makes every change so far durable.
    fn sync(&mut self) -> Result<(), FsError> {
        Ok(())
    }

    /// What's mounted, root first. Empty when there's no shared tree.
    fn mounts(&self) -> Vec<feos_fs::vfs::MountInfo> {
        Vec::new()
    }
}

/// Normalises `path` against `cwd` into an absolute, slash-separated path.
///
/// `..` never walks above `/`; duplicate slashes and `.` components vanish.
pub fn normalize(cwd: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();

    if !path.starts_with('/') {
        for part in cwd.split('/') {
            if !part.is_empty() {
                parts.push(part);
            }
        }
    }

    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }

    let mut result = String::new();
    for part in parts {
        result.push('/');
        result.push_str(part);
    }
    if result.is_empty() {
        result.push('/');
    }
    result
}

/// Returns the parent directory of an absolute, normalised path.
pub fn parent(path: &str) -> &str {
    if path == "/" {
        return "/";
    }
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(index) => &path[..index],
    }
}

/// Returns the final component of an absolute, normalised path.
pub fn base_name(path: &str) -> &str {
    if path == "/" {
        return "/";
    }
    path.rsplit('/').next().unwrap_or(path)
}

/// Joins a directory and a child name.
pub fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        let mut result = String::from("/");
        result.push_str(name);
        result
    } else {
        let mut result = String::from(dir);
        result.push('/');
        result.push_str(name);
        result
    }
}

#[derive(Clone, Debug)]
enum Node {
    File(Vec<u8>),
    Dir,
    /// A character device that reads as empty and discards writes.
    Null,
    /// A character device that reads as endless zeros and discards writes.
    Zero,
    Tombstone,
}

/// Bytes the overlay may hold in RAM by default. The kernel heap is a 64 MiB
/// arena, and pipeline buffers and variables need their share of it.
pub const DEFAULT_QUOTA: usize = 24 * 1024 * 1024;

/// Largest file [`Vfs::read_file`] brings into memory in one piece.
pub const MAX_READ_BYTES: usize = 32 * 1024 * 1024;

/// Deepest path the overlay will create, which bounds every recursive walker.
pub const MAX_PATH_DEPTH: usize = 48;

/// True when `path` insists on a directory, as in `file/` or `file/.`.
fn wants_dir(path: &str) -> bool {
    path.len() > 1 && (path.ends_with('/') || path.ends_with("/."))
}

fn path_depth(path: &str) -> usize {
    path.split('/').filter(|part| !part.is_empty()).count()
}

fn copy_bytes(data: &[u8]) -> Result<Vec<u8>, FsError> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(data.len())
        .map_err(|_| FsError::NoSpace)?;
    copy.extend_from_slice(data);
    Ok(copy)
}

/// An overlay filesystem with an optional read-only base image.
///
/// When `base` is the system's shared tree, the overlay holds no files of its
/// own: every operation goes straight to the tree, and only the working
/// directory stays per-shell. That's how every shell and Explorer see one set of
/// files. See [`feos_fs::VfsTree`].
pub struct OverlayFs {
    shared: Option<&'static dyn VfsTree>,
    base: Option<&'static dyn FileSystem>,
    nodes: BTreeMap<String, Node>,
    cwd: String,
    used: usize,
    quota: usize,
}

impl OverlayFs {
    /// Creates a filesystem backed by `base` when one is available.
    pub fn new(base: Option<&'static dyn FileSystem>) -> Self {
        Self {
            shared: base.and_then(|base| base.tree()),
            base,
            nodes: BTreeMap::new(),
            cwd: String::from("/"),
            used: 0,
            quota: DEFAULT_QUOTA,
        }
    }

    /// A view for another task, starting in the same directory. Over the shared
    /// tree it sees the same files. An overlay of its own can only be copied, so
    /// what the copy writes stays with the copy.
    pub fn fork(&self) -> Self {
        Self {
            shared: self.shared,
            base: self.base,
            nodes: self.nodes.clone(),
            cwd: self.cwd.clone(),
            used: self.used,
            quota: self.quota,
        }
    }

    /// Sets how many bytes of file data the overlay may keep in RAM.
    pub fn set_quota(&mut self, bytes: usize) {
        self.quota = bytes;
    }

    /// Bytes of file data currently held in RAM.
    pub fn used_bytes(&self) -> usize {
        self.used
    }

    /// Creates a device node that reads as empty and discards writes, which is
    /// how `/dev/null` exists on a filesystem with no device layer.
    pub fn add_null_device(&mut self, path: &str) -> Result<(), FsError> {
        let path = normalize(&self.cwd, path);
        if let Some(tree) = self.shared {
            return shared_device(tree, &path, feos_fs::vfs::DeviceKind::Null);
        }
        if self.effective_meta(parent(&path))?.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }
        self.drop_file_bytes(&path);
        self.nodes.insert(path, Node::Null);
        Ok(())
    }

    /// Creates a device node that reads as endless zeros and discards writes.
    pub fn add_zero_device(&mut self, path: &str) -> Result<(), FsError> {
        let path = normalize(&self.cwd, path);
        if let Some(tree) = self.shared {
            return shared_device(tree, &path, feos_fs::vfs::DeviceKind::Zero);
        }
        if self.effective_meta(parent(&path))?.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }
        self.drop_file_bytes(&path);
        self.nodes.insert(path, Node::Zero);
        Ok(())
    }

    fn drop_file_bytes(&mut self, path: &str) {
        if let Some(Node::File(data)) = self.nodes.get(path) {
            self.used = self.used.saturating_sub(data.len());
        }
    }

    fn check_quota(&self, extra: usize) -> Result<(), FsError> {
        match self.used.checked_add(extra) {
            Some(total) if total <= self.quota => Ok(()),
            _ => Err(FsError::NoSpace),
        }
    }

    /// Moves the working directory off a directory that was just removed.
    fn leave_removed(&mut self, removed: &str) {
        let inside = join(removed, "");
        if self.cwd == removed || self.cwd.starts_with(&inside) {
            self.cwd = parent(removed).to_string();
        }
    }

    /// Creates a purely in-memory filesystem with an empty root.
    pub fn memory() -> Self {
        Self::new(None)
    }

    /// The read-only base image, when present.
    pub fn base(&self) -> Option<&'static dyn FileSystem> {
        self.base
    }

    fn base_meta(&self, path: &str) -> Option<Meta> {
        let base = self.base?;
        let node = base.resolve(path)?;
        if base.is_dir(node) {
            Some(Meta {
                kind: Kind::Dir,
                size: 0,
            })
        } else if base.is_file(node) {
            Some(Meta {
                kind: Kind::File,
                size: base.size(node),
            })
        } else {
            None
        }
    }

    fn is_tombstone(&self, path: &str) -> bool {
        matches!(self.nodes.get(path), Some(Node::Tombstone))
    }

    /// True when any path component, including `path`, is deleted.
    fn hidden(&self, path: &str) -> bool {
        let mut prefix = String::new();
        for part in path.split('/').filter(|part| !part.is_empty()) {
            prefix.push('/');
            prefix.push_str(part);
            if self.is_tombstone(&prefix) {
                return true;
            }
        }
        false
    }

    /// True when an overlay entry at a proper ancestor hides the base image.
    fn overlay_ancestor(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        let mut prefix = String::new();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            prefix.push('/');
            prefix.push_str(part);
            if self.nodes.contains_key(&prefix) {
                return true;
            }
        }
        false
    }

    fn effective_meta(&self, path: &str) -> Result<Meta, FsError> {
        if self.hidden(path) {
            return Err(FsError::NotFound);
        }
        if path == "/" {
            return Ok(Meta {
                kind: Kind::Dir,
                size: 0,
            });
        }
        if let Some(node) = self.nodes.get(path) {
            return match node {
                Node::File(data) => Ok(Meta {
                    kind: Kind::File,
                    size: data.len(),
                }),
                Node::Dir => Ok(Meta {
                    kind: Kind::Dir,
                    size: 0,
                }),
                Node::Null | Node::Zero => Ok(Meta {
                    kind: Kind::File,
                    size: 0,
                }),
                Node::Tombstone => Err(FsError::NotFound),
            };
        }
        if self.overlay_ancestor(path) {
            return Err(FsError::NotFound);
        }
        self.base_meta(path).ok_or(FsError::NotFound)
    }

    fn remove_tree(&mut self, path: &str) -> Result<(), FsError> {
        let meta = self.effective_meta(path)?;
        match meta.kind {
            Kind::File => self.remove_file(path),
            Kind::Dir => {
                if path == "/" {
                    return Err(FsError::InvalidPath);
                }
                if path_depth(path) > MAX_PATH_DEPTH * 2 {
                    return Err(FsError::TooDeep);
                }
                let entries = self.read_dir(path)?;
                for entry in entries {
                    let child = join(path, &entry.name);
                    self.remove_tree(&child)?;
                }
                self.remove_dir(path)
            }
        }
    }

    fn copy_tree(&mut self, from: &str, to: &str) -> Result<(), FsError> {
        let meta = self.effective_meta(from)?;
        match meta.kind {
            Kind::File => {
                let data = self.read_file(from)?;
                self.write_file(to, &data, false)
            }
            Kind::Dir => {
                if path_depth(from) > MAX_PATH_DEPTH * 2 {
                    return Err(FsError::TooDeep);
                }
                self.create_dir(to)?;
                let entries = self.read_dir(from)?;
                for entry in entries {
                    let source = join(from, &entry.name);
                    let target = join(to, &entry.name);
                    self.copy_tree(&source, &target)?;
                }
                Ok(())
            }
        }
    }
}

/// A node's listing facts, copied out so a directory walk borrows nothing.
#[derive(Clone, Copy)]
enum NodeKind {
    File(usize),
    Dir,
    Tombstone,
}

impl Node {
    fn clone_kind(&self) -> NodeKind {
        match self {
            Node::File(data) => NodeKind::File(data.len()),
            Node::Null | Node::Zero => NodeKind::File(0),
            Node::Dir => NodeKind::Dir,
            Node::Tombstone => NodeKind::Tombstone,
        }
    }
}

impl Vfs for OverlayFs {
    fn cwd(&self) -> &str {
        &self.cwd
    }

    fn chdir(&mut self, path: &str) -> Result<(), FsError> {
        let path = normalize(&self.cwd, path);
        let meta = match self.shared {
            Some(tree) => shared_meta(tree, &path)?,
            None => self.effective_meta(&path)?,
        };
        if meta.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }
        self.cwd = path;
        Ok(())
    }

    fn meta(&self, path: &str) -> Result<Meta, FsError> {
        let insists_on_dir = wants_dir(path);
        let path = normalize(&self.cwd, path);
        let meta = match self.shared {
            Some(tree) => shared_meta(tree, &path)?,
            None => self.effective_meta(&path)?,
        };
        if insists_on_dir && meta.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }
        Ok(meta)
    }

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, FsError> {
        let insists_on_dir = wants_dir(path);
        let path = normalize(&self.cwd, path);
        if let Some(tree) = self.shared {
            if insists_on_dir && shared_meta(tree, &path)?.kind != Kind::Dir {
                return Err(FsError::NotDir);
            }
            return shared_read_file(tree, &path);
        }
        let meta = self.effective_meta(&path)?;
        if meta.kind != Kind::File {
            return Err(FsError::IsDir);
        }
        if insists_on_dir {
            return Err(FsError::NotDir);
        }
        if meta.size > MAX_READ_BYTES {
            return Err(FsError::NoSpace);
        }

        match self.nodes.get(&path) {
            Some(Node::File(data)) => return copy_bytes(data),
            Some(Node::Null) => return Ok(Vec::new()),
            // Reading all of an endless file can only end one way.
            Some(Node::Zero) => return Err(FsError::NoSpace),
            _ => {}
        }

        // The size comes from the medium, so it is only a hint: allocate
        // fallibly and never read more than the budget.
        let base = self.base.ok_or(FsError::NotFound)?;
        let node = base.resolve(&path).ok_or(FsError::NotFound)?;
        let size = base.size(node);
        let mut data = Vec::new();
        data.try_reserve_exact(size).map_err(|_| FsError::NoSpace)?;
        let mut chunk = [0u8; 4096];
        let mut offset = 0usize;
        loop {
            let read = base.read(node, offset, &mut chunk);
            if read == 0 {
                break;
            }
            if data.len() + read > MAX_READ_BYTES || data.try_reserve(read).is_err() {
                return Err(FsError::NoSpace);
            }
            data.extend_from_slice(&chunk[..read]);
            offset += read;
        }
        Ok(data)
    }

    fn read_at(&mut self, path: &str, offset: usize, out: &mut [u8]) -> Result<usize, FsError> {
        let insists_on_dir = wants_dir(path);
        let path = normalize(&self.cwd, path);
        if let Some(tree) = self.shared {
            if insists_on_dir && shared_meta(tree, &path)?.kind != Kind::Dir {
                return Err(FsError::NotDir);
            }
            return tree.read_at(&path, offset as u64, out).map_err(shared_error);
        }
        let meta = self.effective_meta(&path)?;
        if meta.kind != Kind::File {
            return Err(FsError::IsDir);
        }
        if insists_on_dir {
            return Err(FsError::NotDir);
        }
        match self.nodes.get(&path) {
            Some(Node::File(data)) => {
                let tail = data.get(offset..).unwrap_or(&[]);
                let count = tail.len().min(out.len());
                out[..count].copy_from_slice(&tail[..count]);
                return Ok(count);
            }
            Some(Node::Null) => return Ok(0),
            Some(Node::Zero) => {
                out.fill(0);
                return Ok(out.len());
            }
            _ => {}
        }
        let base = self.base.ok_or(FsError::NotFound)?;
        let node = base.resolve(&path).ok_or(FsError::NotFound)?;
        Ok(base.read(node, offset, out))
    }

    fn write_file(&mut self, path: &str, data: &[u8], append: bool) -> Result<(), FsError> {
        let insists_on_dir = wants_dir(path);
        let path = normalize(&self.cwd, path);
        if path == "/" {
            return Err(FsError::IsDir);
        }
        if let Some(tree) = self.shared {
            if insists_on_dir {
                return match shared_meta(tree, &path) {
                    Ok(meta) if meta.kind == Kind::Dir => Err(FsError::IsDir),
                    Ok(_) => Err(FsError::NotDir),
                    Err(_) => Err(FsError::IsDir),
                };
            }
            return tree.write_file(&path, data, append).map_err(shared_error);
        }
        let parent = parent(&path);
        if self.effective_meta(parent)?.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }
        let existing = self.effective_meta(&path).ok();
        if let Some(meta) = existing {
            if meta.kind == Kind::Dir {
                return Err(FsError::IsDir);
            }
            if insists_on_dir {
                return Err(FsError::NotDir);
            }
        } else if insists_on_dir {
            return Err(FsError::IsDir);
        } else if path_depth(&path) > MAX_PATH_DEPTH {
            return Err(FsError::TooDeep);
        }

        match self.nodes.get_mut(&path) {
            Some(Node::Null | Node::Zero) => return Ok(()),
            // Appending grows the file in place, so a loop of `>>` is linear.
            Some(Node::File(contents)) if append => {
                match self.used.checked_add(data.len()) {
                    Some(total) if total <= self.quota => {}
                    _ => return Err(FsError::NoSpace),
                }
                contents
                    .try_reserve(data.len())
                    .map_err(|_| FsError::NoSpace)?;
                contents.extend_from_slice(data);
                self.used += data.len();
                return Ok(());
            }
            _ => {}
        }

        let previous = match self.nodes.get(&path) {
            Some(Node::File(contents)) => contents.len(),
            _ => 0,
        };
        let prefix = if append && existing.is_some() {
            self.read_file(&path)?
        } else {
            Vec::new()
        };
        let total = prefix.len().saturating_add(data.len());
        self.check_quota(total.saturating_sub(previous))?;
        let mut contents = prefix;
        contents.try_reserve_exact(data.len()).map_err(|_| FsError::NoSpace)?;
        contents.extend_from_slice(data);
        self.used = self.used.saturating_sub(previous) + contents.len();
        self.nodes.insert(path, Node::File(contents));
        Ok(())
    }

    fn create_dir(&mut self, path: &str) -> Result<(), FsError> {
        let path = normalize(&self.cwd, path);
        if path == "/" {
            return Err(FsError::Exists);
        }
        if let Some(tree) = self.shared {
            return tree.create_dir(&path).map_err(shared_error);
        }
        let parent = parent(&path);
        if self.effective_meta(parent)?.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }
        if self.effective_meta(&path).is_ok() {
            return Err(FsError::Exists);
        }
        if path_depth(&path) > MAX_PATH_DEPTH {
            return Err(FsError::TooDeep);
        }
        self.nodes.insert(path, Node::Dir);
        Ok(())
    }

    fn remove_file(&mut self, path: &str) -> Result<(), FsError> {
        let insists_on_dir = wants_dir(path);
        let path = normalize(&self.cwd, path);
        if let Some(tree) = self.shared {
            if insists_on_dir {
                let stat = tree.lstat(&path).map_err(shared_error)?;
                return Err(if stat.kind == FileKind::Directory {
                    FsError::IsDir
                } else {
                    FsError::NotDir
                });
            }
            return tree.remove_file(&path).map_err(shared_error);
        }
        let meta = self.effective_meta(&path)?;
        if meta.kind == Kind::Dir {
            return Err(FsError::IsDir);
        }
        if insists_on_dir {
            return Err(FsError::NotDir);
        }
        self.drop_file_bytes(&path);
        if self.base_meta(&path).is_some() {
            self.nodes.insert(path, Node::Tombstone);
        } else {
            self.nodes.remove(&path);
        }
        Ok(())
    }

    fn remove_dir(&mut self, path: &str) -> Result<(), FsError> {
        let path = normalize(&self.cwd, path);
        if path == "/" {
            return Err(FsError::InvalidPath);
        }
        if let Some(tree) = self.shared {
            tree.remove_dir(&path).map_err(shared_error)?;
            self.leave_removed(&path);
            return Ok(());
        }
        let meta = self.effective_meta(&path)?;
        if meta.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }
        if !self.read_dir(&path)?.is_empty() {
            return Err(FsError::NotEmpty);
        }

        let prefix = join(&path, "");
        let nested: Vec<String> = self
            .nodes
            .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
            .take_while(|(key, _)| key.starts_with(&prefix))
            .map(|(key, _)| key.clone())
            .collect();
        for key in nested {
            self.nodes.remove(&key);
        }

        self.leave_removed(&path);
        if self.base_meta(&path).is_some() {
            self.nodes.insert(path, Node::Tombstone);
        } else {
            self.nodes.remove(&path);
        }
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), FsError> {
        let from = normalize(&self.cwd, from);
        let to = normalize(&self.cwd, to);
        if from == "/" || to == "/" || from == to {
            return Err(FsError::InvalidPath);
        }

        let inside = format!("{from}/");
        if to.starts_with(&inside) {
            return Err(FsError::InvalidPath);
        }

        if let Some(tree) = self.shared {
            return tree.rename(&from, &to).map_err(shared_error);
        }

        self.effective_meta(&from)?;
        let parent = parent(&to);
        if self.effective_meta(parent)?.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }
        if self.effective_meta(&to).is_ok() {
            return Err(FsError::Exists);
        }

        self.copy_tree(&from, &to)?;
        self.remove_tree(&from)?;
        Ok(())
    }

    fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        let path = normalize(&self.cwd, path);
        if let Some(tree) = self.shared {
            return shared_read_dir(tree, &path);
        }
        let meta = self.effective_meta(&path)?;
        if meta.kind != Kind::Dir {
            return Err(FsError::NotDir);
        }

        let mut entries: BTreeMap<String, DirEntry> = BTreeMap::new();
        let shadowed =
            self.overlay_ancestor(&path) || matches!(self.nodes.get(&path), Some(Node::Dir));

        if !shadowed {
            if let Some(base) = self.base {
                if let Some(dir) = base.resolve(&path) {
                    for index in 0..base.child_count(dir) {
                        let Some(child) = base.child_at(dir, index) else {
                            continue;
                        };
                        let Some(name) = base.name(child) else {
                            continue;
                        };
                        let child_path = join(&path, name);
                        if self.nodes.contains_key(&child_path) || self.hidden(&child_path) {
                            continue;
                        }
                        let kind = if base.is_dir(child) {
                            Kind::Dir
                        } else {
                            Kind::File
                        };
                        let size = if kind == Kind::File {
                            base.size(child)
                        } else {
                            0
                        };
                        entries.insert(
                            name.to_string(),
                            DirEntry {
                                name: name.to_string(),
                                kind,
                                size,
                            },
                        );
                    }
                }
            }
        }

        // Keys sort by path, so this directory's children are one contiguous
        // run, and a child's own subtree can be skipped by jumping past it.
        let prefix = join(&path, "");
        let mut start = prefix.clone();
        let mut inclusive = true;
        loop {
            let lower = if inclusive {
                Bound::Included(start.as_str())
            } else {
                Bound::Excluded(start.as_str())
            };
            let found = self
                .nodes
                .range::<str, _>((lower, Bound::Unbounded))
                .next()
                .map(|(key, node)| (key.clone(), node.clone_kind()));
            let Some((key, node)) = found else {
                break;
            };
            if !key.starts_with(&prefix) {
                break;
            }
            let rest = &key[prefix.len()..];
            let skip_to = match rest.find('/') {
                Some(slash) => {
                    // `/` + 1 is `0`, the first key after this child's subtree.
                    let mut next = String::from(&key[..prefix.len() + slash]);
                    next.push('0');
                    Some(next)
                }
                None => {
                    if !matches!(node, NodeKind::Tombstone) && !self.hidden(&key) {
                        let name = rest.to_string();
                        let (kind, size) = match node {
                            NodeKind::File(size) => (Kind::File, size),
                            NodeKind::Dir => (Kind::Dir, 0),
                            NodeKind::Tombstone => (Kind::File, 0),
                        };
                        entries.insert(name.clone(), DirEntry { name, kind, size });
                    }
                    None
                }
            };
            match skip_to {
                Some(next) => {
                    start = next;
                    inclusive = true;
                }
                None => {
                    start = key;
                    inclusive = false;
                }
            }
        }

        Ok(entries.into_iter().map(|(_, entry)| entry).collect())
    }

    fn times(&self, path: &str) -> Option<feos_fs::NodeTimes> {
        let path = normalize(&self.cwd, path);
        self.shared?.stat(&path).ok().map(|stat| stat.times)
    }

    fn set_times(&mut self, path: &str, times: feos_fs::NodeTimes) -> Result<(), FsError> {
        let path = normalize(&self.cwd, path);
        let tree = self.shared.ok_or(FsError::NotSupported)?;
        tree.set_times(&path, times).map_err(shared_error)
    }

    fn statfs(&self, path: &str) -> Result<feos_fs::VolumeStats, FsError> {
        let path = normalize(&self.cwd, path);
        let tree = self.shared.ok_or(FsError::NotSupported)?;
        tree.statfs(&path).map_err(shared_error)
    }

    fn symlink(&mut self, target: &str, path: &str) -> Result<(), FsError> {
        let path = normalize(&self.cwd, path);
        let tree = self.shared.ok_or(FsError::NotSupported)?;
        tree.symlink(target, &path).map_err(shared_error)
    }

    fn read_link(&self, path: &str) -> Result<String, FsError> {
        let path = normalize(&self.cwd, path);
        let tree = self.shared.ok_or(FsError::NotSupported)?;
        tree.read_link(&path).map_err(shared_error)
    }

    fn stat_full(&self, path: &str) -> Option<feos_fs::Stat> {
        let path = normalize(&self.cwd, path);
        self.shared?.lstat(&path).ok()
    }

    fn chmod(&mut self, path: &str, mode: u16) -> Result<(), FsError> {
        let path = normalize(&self.cwd, path);
        let tree = self.shared.ok_or(FsError::NotSupported)?;
        tree.chmod(&path, mode).map_err(shared_error)
    }

    fn chown(&mut self, path: &str, uid: Option<u32>, gid: Option<u32>) -> Result<(), FsError> {
        let path = normalize(&self.cwd, path);
        let tree = self.shared.ok_or(FsError::NotSupported)?;
        tree.chown(&path, uid, gid).map_err(shared_error)
    }

    fn hard_link(&mut self, existing: &str, path: &str) -> Result<(), FsError> {
        let (existing, path) = (normalize(&self.cwd, existing), normalize(&self.cwd, path));
        let tree = self.shared.ok_or(FsError::NotSupported)?;
        tree.link(&existing, &path).map_err(shared_error)
    }

    fn fsck(&mut self, path: &str, repair: bool) -> Result<feos_fs::FsckReport, FsError> {
        let path = normalize(&self.cwd, path);
        let tree = self.shared.ok_or(FsError::NotSupported)?;
        tree.fsck(&path, repair).map_err(shared_error)
    }

    fn sync(&mut self) -> Result<(), FsError> {
        match self.shared {
            Some(tree) => tree.sync().map_err(shared_error),
            None => Ok(()),
        }
    }

    fn mounts(&self) -> Vec<feos_fs::vfs::MountInfo> {
        self.shared.map_or_else(Vec::new, |tree| tree.mounts())
    }
}

/// Maps the shared tree's errors onto the ones utilities report.
fn shared_error(error: feos_fs::FsError) -> FsError {
    use feos_fs::FsError as Tree;
    match error {
        Tree::NotFound | Tree::StaleHandle => FsError::NotFound,
        Tree::NotDirectory => FsError::NotDir,
        Tree::IsDirectory | Tree::NotFile => FsError::IsDir,
        Tree::NameExists => FsError::Exists,
        Tree::NotEmpty => FsError::NotEmpty,
        Tree::InvalidName => FsError::InvalidPath,
        Tree::Unsupported => FsError::NotSupported,
        Tree::NoSpace | Tree::OutOfMemory | Tree::TooManyNodes => FsError::NoSpace,
        Tree::TooDeep => FsError::TooDeep,
        Tree::ReadOnly => FsError::ReadOnly,
        Tree::Busy => FsError::Busy,
        Tree::SymlinkLoop => FsError::Loop,
        Tree::PermissionDenied => FsError::PermissionDenied,
        _ => FsError::Io,
    }
}

fn shared_meta(tree: &dyn VfsTree, path: &str) -> Result<Meta, FsError> {
    let stat = tree.stat(path).map_err(shared_error)?;
    Ok(match stat.kind {
        FileKind::Directory => Meta {
            kind: Kind::Dir,
            size: 0,
        },
        _ => Meta {
            kind: Kind::File,
            size: stat.size as usize,
        },
    })
}

fn shared_device(
    tree: &dyn VfsTree,
    path: &str,
    kind: feos_fs::vfs::DeviceKind,
) -> Result<(), FsError> {
    if tree.stat(path).is_ok() {
        return Ok(());
    }
    tree.make_device(path, kind).map_err(shared_error)
}

fn shared_read_file(tree: &dyn VfsTree, path: &str) -> Result<Vec<u8>, FsError> {
    let meta = shared_meta(tree, path)?;
    if meta.kind != Kind::File {
        return Err(FsError::IsDir);
    }
    if meta.size > MAX_READ_BYTES {
        return Err(FsError::NoSpace);
    }
    // Reading all of an endless file can only end one way.
    if path == "/dev/zero" {
        return Err(FsError::NoSpace);
    }
    let mut data = Vec::new();
    data.try_reserve_exact(meta.size).map_err(|_| FsError::NoSpace)?;
    data.resize(meta.size, 0);
    let mut done = 0usize;
    while done < meta.size {
        let count = tree
            .read_at(path, done as u64, &mut data[done..])
            .map_err(shared_error)?;
        if count == 0 {
            // Someone truncated it meanwhile.
            data.truncate(done);
            break;
        }
        done += count;
    }
    Ok(data)
}

fn shared_read_dir(tree: &dyn VfsTree, path: &str) -> Result<Vec<DirEntry>, FsError> {
    let listing = tree.read_dir(path).map_err(shared_error)?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(listing.len())
        .map_err(|_| FsError::NoSpace)?;
    for entry in listing {
        // Utilities treat a link as what it points at, or a file if it's dangling.
        let stat = match entry.stat.kind {
            FileKind::Symlink => tree.stat(&join(path, &entry.name)).unwrap_or(entry.stat),
            _ => entry.stat,
        };
        let (kind, size) = match stat.kind {
            FileKind::Directory => (Kind::Dir, 0),
            _ => (Kind::File, stat.size as usize),
        };
        entries.push(DirEntry {
            name: entry.name,
            kind,
            size,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod shared_tests {
    use super::*;
    use feos_fs::vfs::RamFs;
    use feos_fs::Vfs as Tree;

    fn tree() -> &'static Tree {
        Box::leak(Box::new(Tree::in_memory(1024 * 1024)))
    }

    #[test]
    fn two_shells_and_a_reader_share_one_tree() {
        let tree = tree();
        let mut first = OverlayFs::new(Some(tree));
        let mut second = OverlayFs::new(Some(tree));

        first.write_file("/note", b"from the first", false).unwrap();
        assert_eq!(second.read_file("/note").unwrap(), b"from the first");
        second.write_file("/note", b" and second", true).unwrap();
        assert_eq!(first.read_file("/note").unwrap(), b"from the first and second");

        // What Explorer holds: the read-only face of the same tree.
        let reader: &dyn FileSystem = tree;
        let node = reader.resolve("/note").unwrap();
        assert_eq!(reader.size(node), 25);

        // Working directories stay each shell's own.
        first.create_dir("/d").unwrap();
        first.chdir("/d").unwrap();
        assert_eq!(first.cwd(), "/d");
        assert_eq!(second.cwd(), "/");
        first.write_file("relative", b"x", false).unwrap();
        assert!(second.exists("/d/relative"));

        second.remove_file("/note").unwrap();
        assert!(!first.exists("/note"));
        assert!(reader.resolve("/note").is_none());
    }

    #[test]
    fn behaves_like_the_overlay_for_the_errors_utilities_print() {
        let mut fs = OverlayFs::new(Some(tree()));
        fs.create_dir("/d").unwrap();
        fs.write_file("/f", b"1", false).unwrap();
        assert_eq!(fs.read_file("/missing"), Err(FsError::NotFound));
        assert_eq!(fs.read_file("/d"), Err(FsError::IsDir));
        assert_eq!(fs.read_file("/f/"), Err(FsError::NotDir));
        assert_eq!(fs.meta("/f/").unwrap_err(), FsError::NotDir);
        assert!(fs.meta("/d/").is_ok());
        assert_eq!(fs.write_file("/d", b"x", false), Err(FsError::IsDir));
        assert_eq!(fs.write_file("/new/", b"x", false), Err(FsError::IsDir));
        assert_eq!(fs.write_file("/", b"x", false), Err(FsError::IsDir));
        assert_eq!(fs.create_dir("/d"), Err(FsError::Exists));
        assert_eq!(fs.create_dir("/"), Err(FsError::Exists));
        assert_eq!(fs.remove_file("/d"), Err(FsError::IsDir));
        assert_eq!(fs.remove_dir("/f"), Err(FsError::NotDir));
        assert_eq!(fs.remove_dir("/"), Err(FsError::InvalidPath));
        assert_eq!(fs.rename("/d", "/d/x"), Err(FsError::InvalidPath));
        assert_eq!(fs.rename("/f", "/d"), Err(FsError::Exists));
        assert_eq!(fs.chdir("/f"), Err(FsError::NotDir));
        fs.write_file("/d/in", b"", false).unwrap();
        assert_eq!(fs.remove_dir("/d"), Err(FsError::NotEmpty));

        let names: Vec<String> = fs.read_dir("/").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["d", "f"]);
    }

    #[test]
    fn devices_on_the_tree_act_like_the_overlays() {
        let tree = tree();
        let mut dev = RamFs::new(0);
        dev.add_device(feos_fs::NodeId::ROOT, "null", feos_fs::vfs::DeviceKind::Null)
            .unwrap();
        feos_fs::VfsTree::mount(tree, "/dev", Box::new(dev), false, "dev").unwrap();
        let mut fs = OverlayFs::new(Some(tree));
        fs.add_null_device("/dev/null").unwrap();
        fs.add_zero_device("/dev/zero").unwrap();
        fs.write_file("/dev/null", b"gone", false).unwrap();
        assert_eq!(fs.read_file("/dev/null").unwrap(), b"");
        assert_eq!(fs.read_file("/dev/zero"), Err(FsError::NoSpace));
        let mut out = [1u8; 3];
        assert_eq!(fs.read_at("/dev/zero", 0, &mut out).unwrap(), 3);
        assert_eq!(out, [0; 3]);
    }

    #[test]
    fn a_link_lists_as_what_it_points_at() {
        let mut fs = OverlayFs::new(Some(tree()));
        fs.create_dir("/d").unwrap();
        fs.write_file("/f", b"abc", false).unwrap();
        fs.symlink("/d", "/to-dir").unwrap();
        fs.symlink("/f", "/to-file").unwrap();
        fs.symlink("/gone", "/dangling").unwrap();
        let listing = fs.read_dir("/").unwrap();
        let entry = |name: &str| listing.iter().find(|entry| entry.name == name).unwrap();
        assert_eq!(entry("to-dir").kind, Kind::Dir);
        assert_eq!((entry("to-file").kind, entry("to-file").size), (Kind::File, 3));
        assert_eq!(entry("dangling").kind, Kind::File);
        assert_eq!(fs.read_link("/to-file").unwrap(), "/f");
        fs.remove_file("/dangling").unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static NODES: &[feos_fs::StaticNode] = &[
        feos_fs::StaticNode::dir("/", None, &[1, 2]),
        feos_fs::StaticNode::file("readme.txt", Some(0), b"hello\n"),
        feos_fs::StaticNode::dir("docs", Some(0), &[3]),
        feos_fs::StaticNode::file("guide.md", Some(2), b"# Guide\n"),
    ];
    static BASE: feos_fs::StaticFs = feos_fs::StaticFs::new(NODES);

    #[test]
    fn normalises_paths() {
        assert_eq!(normalize("/", ""), "/");
        assert_eq!(normalize("/a/b", "../c"), "/a/c");
        assert_eq!(normalize("/", "../../x"), "/x");
        assert_eq!(normalize("/a", "b/./c/"), "/a/b/c");
        assert_eq!(parent("/a/b"), "/a");
        assert_eq!(parent("/"), "/");
        assert_eq!(base_name("/a/b"), "b");
        assert_eq!(join("/", "x"), "/x");
    }

    #[test]
    fn reads_through_to_the_base_image() {
        let mut fs = OverlayFs::new(Some(&BASE));
        assert_eq!(fs.cwd(), "/");
        assert!(fs.is_dir("/docs"));
        let data = fs.read_file("/docs/guide.md").unwrap();
        assert_eq!(data, b"# Guide\n");
    }

    #[test]
    fn writes_shadow_the_base_and_deletes_create_tombstones() {
        let mut fs = OverlayFs::new(Some(&BASE));

        fs.write_file("/readme.txt", b"new", false).unwrap();
        assert_eq!(fs.read_file("/readme.txt").unwrap(), b"new");
        fs.write_file("/readme.txt", b"!", true).unwrap();
        assert_eq!(fs.read_file("/readme.txt").unwrap(), b"new!");

        fs.create_dir("/tmp").unwrap();
        fs.write_file("/tmp/a.txt", b"a", false).unwrap();
        assert_eq!(fs.read_dir("/").unwrap().len(), 3);

        fs.remove_file("/docs/guide.md").unwrap();
        assert_eq!(fs.read_dir("/docs").unwrap().len(), 0);
        assert!(!fs.exists("/docs/guide.md"));
        fs.remove_dir("/docs").unwrap();
        assert!(!fs.exists("/docs"));
    }

    #[test]
    fn renames_files_and_directories() {
        let mut fs = OverlayFs::memory();
        fs.create_dir("/a").unwrap();
        fs.write_file("/a/file", b"payload", false).unwrap();
        fs.rename("/a", "/b").unwrap();
        assert!(!fs.exists("/a"));
        assert_eq!(fs.read_file("/b/file").unwrap(), b"payload");
        fs.rename("/b/file", "/b/renamed").unwrap();
        assert_eq!(fs.read_file("/b/renamed").unwrap(), b"payload");
    }

    #[test]
    fn a_trailing_slash_requires_a_directory() {
        let mut fs = OverlayFs::memory();
        fs.write_file("/f", b"x", false).unwrap();
        assert_eq!(fs.meta("/f/"), Err(FsError::NotDir));
        assert_eq!(fs.read_file("/f/"), Err(FsError::NotDir));
        assert_eq!(fs.write_file("/f/", b"y", false), Err(FsError::NotDir));
        assert_eq!(fs.write_file("/new/", b"y", false), Err(FsError::IsDir));
        fs.create_dir("/d").unwrap();
        assert!(fs.is_dir("/d/"));
    }

    #[test]
    fn removing_the_working_directory_moves_up() {
        let mut fs = OverlayFs::memory();
        fs.create_dir("/a").unwrap();
        fs.create_dir("/a/b").unwrap();
        fs.chdir("/a/b").unwrap();
        fs.remove_dir("/a/b").unwrap();
        assert_eq!(fs.cwd(), "/a");
    }

    #[test]
    fn the_quota_turns_writes_into_no_space_errors() {
        let mut fs = OverlayFs::memory();
        fs.set_quota(4);
        fs.write_file("/a", b"abc", false).unwrap();
        assert_eq!(fs.write_file("/b", b"de", false), Err(FsError::NoSpace));
        assert_eq!(fs.write_file("/a", b"de", true), Err(FsError::NoSpace));
        fs.write_file("/a", b"abcd", false).unwrap();
        assert_eq!(fs.used_bytes(), 4);
        fs.remove_file("/a").unwrap();
        assert_eq!(fs.used_bytes(), 0);
    }

    #[test]
    fn appending_to_a_base_file_keeps_its_contents() {
        let mut fs = OverlayFs::new(Some(&BASE));
        fs.write_file("/readme.txt", b"more\n", true).unwrap();
        assert_eq!(fs.read_file("/readme.txt").unwrap(), b"hello\nmore\n");
    }

    #[test]
    fn null_devices_read_empty_and_discard_writes() {
        let mut fs = OverlayFs::memory();
        fs.create_dir("/dev").unwrap();
        fs.add_null_device("/dev/null").unwrap();
        fs.write_file("/dev/null", b"x", false).unwrap();
        fs.write_file("/dev/null", b"x", true).unwrap();
        assert_eq!(fs.read_file("/dev/null").unwrap(), b"");
        assert_eq!(fs.meta("/dev/null").unwrap().kind, Kind::File);
    }

    #[test]
    fn read_at_returns_slices_of_overlay_and_base_files() {
        let mut fs = OverlayFs::new(Some(&BASE));
        let mut out = [0u8; 3];
        assert_eq!(fs.read_at("/readme.txt", 1, &mut out), Ok(3));
        assert_eq!(&out, b"ell");
        fs.write_file("/o", b"abcd", false).unwrap();
        assert_eq!(fs.read_at("/o", 3, &mut out), Ok(1));
        assert_eq!(fs.read_at("/o", 9, &mut out), Ok(0));
    }

    #[test]
    fn listing_skips_nested_subtrees_and_sibling_prefixes() {
        let mut fs = OverlayFs::memory();
        for dir in ["/p", "/p/child", "/p/child/deep", "/p/child-x", "/p/child0", "/q"] {
            fs.create_dir(dir).unwrap();
        }
        fs.write_file("/p/child/deep/f", b"x", false).unwrap();
        fs.write_file("/p/file", b"yy", false).unwrap();
        let names: Vec<String> = fs
            .read_dir("/p")
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, ["child", "child-x", "child0", "file"]);
        assert_eq!(fs.read_dir("/p").unwrap()[3].size, 2);
    }

    #[test]
    fn deep_paths_are_refused_so_walkers_stay_bounded() {
        let mut fs = OverlayFs::memory();
        let mut path = String::new();
        let mut result = Ok(());
        for _ in 0..=MAX_PATH_DEPTH {
            path.push_str("/a");
            result = fs.create_dir(&path);
        }
        assert_eq!(result, Err(FsError::TooDeep));
    }

    #[test]
    fn refuses_to_remove_non_empty_directories() {
        let mut fs = OverlayFs::memory();
        fs.create_dir("/a").unwrap();
        fs.write_file("/a/file", b"x", false).unwrap();
        assert_eq!(fs.remove_dir("/a"), Err(FsError::NotEmpty));
        fs.remove_file("/a/file").unwrap();
        fs.remove_dir("/a").unwrap();
    }
}
