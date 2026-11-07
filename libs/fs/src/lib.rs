//! Read-only and writable filesystems for feos.
//!
//! The abstraction is deliberately storage-agnostic: an app walks nodes
//! through [`FileSystem`] and never sees whether the bytes came from an
//! embedded image, a disk, or RAM. Three backends are provided:
//!
//! * [`StaticFs`] — a compiled-in `StaticNode` table. It is allocation-free
//!   and is what the QEMU board uses by default.
//! * [`Fat32`] — FAT32 long-file-name volumes on any [`BlockDevice`].
//! * [`Exfat`] — exFAT volumes, including UTF-16 names, contiguous streams,
//!   allocation bitmaps and both main and backup boot regions.
//!
//! [`WritableFileSystem`] adds `create_file`, `create_dir`, `write`, `set_len`,
//! `rename`, `remove` and `flush` for devices implementing
//! [`WritableBlockDevice`]. The read-only [`FileSystem`] view stays available
//! through a shared borrow.
//!
//! Disk backends walk the directory tree once at mount time into a small
//! in-memory index (one allocation per entry) and read file data on demand
//! through the [`BlockDevice`]. Reads check the file's cluster chain the first
//! time and keep a cursor, so sequential reads don't walk it again. A RAM-backed
//! device is included for embedding and for tests:
//!
//! ```no_run
//! use feos_fs::{mount_image, EntryKind, FileSystem};
//!
//! static DISK: &[u8] = &[];
//! let fs = mount_image(DISK).unwrap();
//! let node = fs.resolve("/hello.txt").unwrap();
//! assert_eq!(fs.kind(node), Some(EntryKind::File));
//! ```
//!
//! [`StaticFs`] example:
//!
//! ```no_run
//! use feos_fs::{EntryKind, FileSystem, StaticFs};
//!
//! static NODES: &[feos_fs::StaticNode] = &[
//!     feos_fs::StaticNode::dir("/", None, &[1]),
//!     feos_fs::StaticNode::file("hello.txt", Some(0), b"hello\n"),
//! ];
//! let fs = StaticFs::new(NODES);
//! let node = fs.resolve("/hello.txt").unwrap();
//! assert_eq!(fs.kind(node), Some(EntryKind::File));
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

mod block;
mod cache;
mod error;
mod exfat;
mod fat16;
mod fat32;
mod format;
mod mount;
pub mod native;
mod partition;
mod tree;
pub mod vfs;
mod volume;

pub use block::{
    BlockDevice, DriverDevice, MemoryBlockDevice, MemoryBlockDeviceMut, RamDisk,
    WritableBlockDevice, WriteClass, MAX_BLOCK_SIZE,
};
pub use cache::{BufferCache, CacheStats};
pub use partition::{read_partitions, Partition, PartitionHint, PartitionInfo, MAX_PARTITIONS};
pub use error::FsError;
pub use exfat::Exfat;
pub use fat16::Fat16;
pub use fat32::Fat32;
pub use format::{format, format_exfat, format_fat32};
pub use native::{FsckReport, Native, PosixMeta};
pub use vfs::{File, FileKind, OpenOptions, Seek, Stat, TreeEntry, Vfs, VfsTree};
pub use mount::MountTable;
pub use volume::{detect, VolumeKind};

use alloc::boxed::Box;
use alloc::vec::Vec;

#[cfg(test)]
mod tests;

/// Probes `device` and mounts the first supported filesystem.
///
/// The returned trait object borrows from `device` only when the device itself
/// does: `mount_image` can therefore return a box tied to the image slice's
/// lifetime.
pub fn mount<'a, D: BlockDevice + 'a>(device: D) -> Result<Box<dyn FileSystem + 'a>, FsError> {
    match detect(&device)? {
        VolumeKind::Fat32 => Ok(Box::new(Fat32::mount(device)?)),
        VolumeKind::Fat16 => Ok(Box::new(Fat16::mount(device)?)),
        VolumeKind::Exfat => Ok(Box::new(Exfat::mount(device)?)),
        // Mounting it can mean replaying its journal, which takes a writable device.
        VolumeKind::Native => Err(FsError::UnsupportedVolume),
    }
}

/// Mounts a filesystem from an in-memory disk image.
pub fn mount_image(image: &[u8]) -> Result<Box<dyn FileSystem + '_>, FsError> {
    mount(MemoryBlockDevice::new(image))
}

/// Probes `device` and mounts the first supported writable filesystem.
pub fn mount_writable<'a, D: WritableBlockDevice + 'a>(
    device: D,
) -> Result<Box<dyn WritableFileSystem + 'a>, FsError> {
    match detect(&device)? {
        VolumeKind::Fat32 => Ok(Box::new(Fat32::mount(device)?)),
        VolumeKind::Fat16 => Err(FsError::ReadOnly),
        VolumeKind::Exfat => Ok(Box::new(Exfat::mount(device)?)),
        VolumeKind::Native => Ok(Box::new(native::Native::mount(device)?)),
    }
}

/// Mounts a writable filesystem from a mutable in-memory disk image.
pub fn mount_image_mut(image: &mut [u8]) -> Result<Box<dyn WritableFileSystem + '_>, FsError> {
    mount_writable(MemoryBlockDeviceMut::new(image))
}

/// Stable identifier of a node inside one filesystem.
///
/// IDs are opaque to callers and only meaningful for the [`FileSystem`] that
/// produced them. The root node is always [`NodeId::ROOT`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct NodeId(pub u32);

impl NodeId {
    /// The root directory.
    pub const ROOT: Self = Self(0);
}

/// What a node is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
}

impl EntryKind {
    /// A short human-readable name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }

    /// A one-character indicator suitable for a text listing.
    pub const fn short(self) -> char {
        match self {
            Self::File => '-',
            Self::Directory => 'd',
        }
    }
}

/// Maximum number of path components [`FileSystem::path`] can walk.
///
/// The disk backends also stop here when walking a volume, and refuse changes
/// that would nest deeper, so recursive walkers have a known bound. A deeper
/// tree on an untrusted volume marks it [damaged](FileSystem::is_damaged).
pub const MAX_PATH_DEPTH: usize = 32;

/// File attribute bits, the ones FAT and exFAT agree on.
pub mod attr {
    /// Writes are refused.
    pub const READ_ONLY: u8 = 0x01;
    /// Left out of ordinary listings.
    pub const HIDDEN: u8 = 0x02;
    /// Belongs to the system.
    pub const SYSTEM: u8 = 0x04;
    /// Changed since the last backup.
    pub const ARCHIVE: u8 = 0x20;
    /// Every bit a caller may set.
    pub const SETTABLE: u8 = READ_ONLY | HIDDEN | SYSTEM | ARCHIVE;
}

/// When a node was last touched, in seconds since the Unix epoch.
///
/// `None` means the volume doesn't say. That's usual on one written by something
/// with no clock, which leaves the date fields zero.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NodeTimes {
    /// Created.
    pub created: Option<u64>,
    /// Contents last changed.
    pub modified: Option<u64>,
    /// Last read. FAT keeps only the date.
    pub accessed: Option<u64>,
}

impl NodeTimes {
    /// No timestamps at all.
    pub const NONE: Self = Self {
        created: None,
        modified: None,
        accessed: None,
    };
}

/// How much room a volume has.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct VolumeStats {
    /// Allocation unit in bytes.
    pub block_size: u64,
    /// Total allocation units.
    pub total_blocks: u64,
    /// Free allocation units.
    pub free_blocks: u64,
    /// The volume refuses writes.
    pub read_only: bool,
    /// Name of the on-disk format.
    pub kind: &'static str,
}

/// What a mount-time check found and repaired.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CheckReport {
    /// The volume wasn't cleanly unmounted, so the check ran.
    pub was_dirty: bool,
    /// Allocated clusters nothing referred to, now free.
    pub lost_clusters: u32,
    /// Files whose size or chain had to change to agree.
    pub truncated_files: u32,
    /// Directory entries removed because they duplicate another's data.
    pub removed_entries: u32,
    /// Long-name fragments with no entry to belong to, now cleared.
    pub orphan_names: u32,
    /// Entry sets kept, as their parts agree, with their checksum put right.
    pub repaired_sets: u32,
}

impl CheckReport {
    /// True when the check changed nothing but the dirty flag.
    pub const fn found_nothing(&self) -> bool {
        self.repaired_sets == 0
            && self.lost_clusters == 0
            && self.truncated_files == 0
            && self.removed_entries == 0
            && self.orphan_names == 0
    }
}

/// Where a writable filesystem gets the time from.
pub type TimeSource = fn() -> Option<u64>;

/// A read-only filesystem view.
///
/// All methods are cheap lookups; file data is copied by
/// [`read`](FileSystem::read), so callers control the buffer size and no
/// allocation is required after mounting.
pub trait FileSystem: Sync {
    /// Number of live nodes in the filesystem.
    ///
    /// Node IDs can exceed this after a removal: use
    /// [`node_id_limit`](FileSystem::node_id_limit) to bound them.
    fn node_count(&self) -> usize;

    /// One past the highest node ID that is in use.
    fn node_id_limit(&self) -> usize {
        self.node_count()
    }

    /// Whether mounting found damage and skipped part of the volume.
    ///
    /// A damaged volume mounts so that what's intact can be read, but it can
    /// list fewer files than it holds, and writes to it fail with
    /// [`FsError::Corrupt`].
    fn is_damaged(&self) -> bool {
        false
    }

    /// Returns the kind of `node`.
    fn kind(&self, node: NodeId) -> Option<EntryKind>;

    /// Returns the final component of `node`'s name.
    ///
    /// The root has the name `/`.
    fn name(&self, node: NodeId) -> Option<&str>;

    /// Returns `node`'s parent, or `None` for the root.
    fn parent(&self, node: NodeId) -> Option<NodeId>;

    /// File size in bytes, or zero for a directory.
    fn size(&self, node: NodeId) -> usize;

    /// Copies bytes from `node`, starting at `offset`, into `out`.
    ///
    /// Returns the number of bytes copied; zero means end-of-file or that
    /// `node` is not a file. It also means the read failed, because there's no
    /// way to say so: code that handles untrusted volumes should prefer
    /// [`try_read`](FileSystem::try_read).
    fn read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> usize;

    /// Like [`read`](FileSystem::read), but reports failure.
    ///
    /// `Ok(0)` only ever means end-of-file. A truncated or cyclic cluster chain
    /// is [`FsError::Corrupt`] rather than short or repeated data, and a device
    /// failure is passed on. `out` may have been partly filled on error.
    fn try_read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> Result<usize, FsError> {
        match self.kind(node) {
            None => Err(FsError::NotFound),
            Some(EntryKind::Directory) => Err(FsError::NotFile),
            Some(EntryKind::File) => Ok(self.read(node, offset, out)),
        }
    }

    /// Reads a whole file into a new buffer.
    ///
    /// The size comes from the disk, and it may be larger than the heap, so
    /// allocation failure is [`FsError::OutOfMemory`] rather than a panic.
    fn read_to_vec(&self, node: NodeId) -> Result<Vec<u8>, FsError> {
        if self.kind(node).is_none() {
            return Err(FsError::NotFound);
        }
        let size = self.size(node);
        let mut data = Vec::new();
        data.try_reserve_exact(size)
            .map_err(|_| FsError::OutOfMemory)?;
        data.resize(size, 0);
        let mut done = 0usize;
        while done < size {
            let count = self.try_read(node, done, &mut data[done..])?;
            if count == 0 {
                return Err(FsError::Corrupt);
            }
            done += count;
        }
        Ok(data)
    }

    /// Number of children of `node`.
    fn child_count(&self, node: NodeId) -> usize;

    /// Returns the child at `index`, in filesystem order.
    fn child_at(&self, node: NodeId, index: usize) -> Option<NodeId>;

    /// Attribute bits of a node; see [`attr`].
    fn attributes(&self, _node: NodeId) -> u8 {
        0
    }

    /// Timestamps of a node.
    fn times(&self, _node: NodeId) -> NodeTimes {
        NodeTimes::NONE
    }

    /// True when the backend refuses changes. Cheaper than [`stats`](Self::stats),
    /// which may have to count free space.
    fn is_read_only(&self) -> bool {
        false
    }

    /// Size and free space, as far as the backend knows.
    fn stats(&self) -> VolumeStats {
        VolumeStats {
            block_size: 512,
            total_blocks: 0,
            free_blocks: 0,
            read_only: true,
            kind: "unknown",
        }
    }

    /// The shared writable tree behind this filesystem, if it has one.
    ///
    /// This is how a holder of [`Platform::filesystem`] finds out it can write,
    /// without the type having to change for every caller that only reads.
    fn tree(&self) -> Option<&dyn VfsTree> {
        None
    }

    /// The root directory.
    fn root(&self) -> NodeId {
        NodeId::ROOT
    }

    /// Returns `true` when `node` is a file.
    fn is_file(&self, node: NodeId) -> bool {
        self.kind(node) == Some(EntryKind::File)
    }

    /// Returns `true` when `node` is a directory.
    fn is_dir(&self, node: NodeId) -> bool {
        self.kind(node) == Some(EntryKind::Directory)
    }

    /// Looks up a direct child of `dir` by name.
    fn lookup(&self, dir: NodeId, name: &str) -> Option<NodeId> {
        for index in 0..self.child_count(dir) {
            let child = self.child_at(dir, index)?;
            if self.name(child) == Some(name) {
                return Some(child);
            }
        }
        None
    }

    /// Resolves `path` from the root.
    ///
    /// A leading `/` is optional. `.` and `..` components are honoured, and
    /// trailing slashes are ignored. Returns `None` for a missing path or a
    /// path that walks above the root.
    fn resolve(&self, path: &str) -> Option<NodeId> {
        let mut node = self.root();
        for component in path.split('/') {
            if component.is_empty() || component == "." {
                continue;
            }
            if component == ".." {
                node = self.parent(node)?;
                continue;
            }
            node = self.lookup(node, component)?;
        }
        Some(node)
    }

    /// Writes the absolute path of `node` into `out` and returns it as a string.
    ///
    /// Truncation is not performed: the buffer must be large enough or `None`
    /// is returned.
    fn path<'a>(&self, node: NodeId, out: &'a mut [u8]) -> Option<&'a str> {
        if node == self.root() {
            if out.is_empty() {
                return None;
            }
            out[0] = b'/';
            return core::str::from_utf8(&out[..1]).ok();
        }

        let mut chain = [NodeId::ROOT; MAX_PATH_DEPTH];
        let mut depth = 0usize;
        let mut current = node;
        while current != self.root() {
            if depth == MAX_PATH_DEPTH {
                return None;
            }
            chain[depth] = current;
            depth += 1;
            current = self.parent(current)?;
        }

        let mut len = 0usize;
        for &part in chain[..depth].iter().rev() {
            let name = self.name(part)?;
            if len + name.len() + 1 > out.len() {
                return None;
            }
            out[len] = b'/';
            len += 1;
            out[len..len + name.len()].copy_from_slice(name.as_bytes());
            len += name.len();
        }
        core::str::from_utf8(&out[..len]).ok()
    }
}

/// A filesystem that can be modified.
///
/// The read-only [`FileSystem`] view is still available through a mutable
/// borrow. Mutations take `&mut self` so a backend can update its block device
/// without interior locking; callers that need to share a mounted volume should
/// protect it with a lock.
pub trait WritableFileSystem: FileSystem + Send {
    /// Creates an empty file in `parent` and returns its node.
    fn create_file(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError>;

    /// Creates an empty directory in `parent` and returns its node.
    fn create_dir(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError>;

    /// Removes a file or an empty directory.
    fn remove(&mut self, node: NodeId) -> Result<(), FsError>;

    /// Moves `node` into `new_parent` and gives it `new_name`.
    fn rename(&mut self, node: NodeId, new_parent: NodeId, new_name: &str) -> Result<(), FsError>;

    /// Writes `data` at `offset`, extending the file when necessary.
    ///
    /// Returns the number of bytes written. A gap between the old end of file
    /// and `offset` is zero-filled.
    fn write(&mut self, node: NodeId, offset: usize, data: &[u8]) -> Result<usize, FsError>;

    /// Grows or shrinks a file to exactly `size` bytes.
    ///
    /// Growing zero-fills the new range. Shrinking releases clusters that are
    /// no longer needed.
    fn set_len(&mut self, node: NodeId, size: usize) -> Result<(), FsError>;

    /// Flushes filesystem metadata and device caches.
    fn flush(&mut self) -> Result<(), FsError>;

    /// Replaces a node's attribute bits (see [`attr::SETTABLE`]).
    fn set_attributes(&mut self, _node: NodeId, _attributes: u8) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Sets whichever timestamps are `Some`.
    fn set_times(&mut self, _node: NodeId, _times: NodeTimes) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Tells the filesystem what time it is, for the timestamps it stores.
    fn set_time_source(&mut self, _now: TimeSource) {}

    /// Checks a volume that wasn't cleanly unmounted, and repairs what a crash
    /// can leave behind. A clean volume returns at once.
    fn check(&mut self) -> Result<CheckReport, FsError> {
        Ok(CheckReport::default())
    }

    /// Turns an empty file into a device node, on a filesystem that can.
    fn make_device(&mut self, _node: NodeId, _kind: vfs::DeviceKind) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Owner, group, mode and links, on a filesystem that keeps them. `None` means it
    /// doesn't, as FAT doesn't, and that nothing about a file's owner should be enforced.
    fn posix(&self, _node: NodeId) -> Option<native::PosixMeta> {
        None
    }

    /// Sets the permission bits.
    fn set_mode(&mut self, _node: NodeId, _mode: u16) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Sets whichever of the owner and group are `Some`.
    fn set_owner(&mut self, _node: NodeId, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }

    /// Gives a file another name in `new_parent`.
    fn hard_link(&mut self, _node: NodeId, _new_parent: NodeId, _name: &str) -> Result<NodeId, FsError> {
        Err(FsError::Unsupported)
    }

    /// Checks the volume through and through, on a filesystem with a checker of its own.
    fn fsck(&mut self, _repair: bool) -> Result<native::FsckReport, FsError> {
        Err(FsError::Unsupported)
    }
}

/// One node in a compiled-in filesystem image.
///
/// `parent` and `children` are indices into the enclosing node table.
#[derive(Clone, Copy, Debug)]
pub struct StaticNode {
    /// Final path component; `/` for the root.
    pub name: &'static str,
    /// File or directory.
    pub kind: EntryKind,
    /// File contents (empty for directories).
    pub data: &'static [u8],
    /// Parent index, or `None` for the root.
    pub parent: Option<u32>,
    /// Indices of this directory's children.
    pub children: &'static [u32],
}

impl StaticNode {
    /// Creates a directory node.
    pub const fn dir(name: &'static str, parent: Option<u32>, children: &'static [u32]) -> Self {
        Self {
            name,
            kind: EntryKind::Directory,
            data: &[],
            parent,
            children,
        }
    }

    /// Creates a file node.
    pub const fn file(name: &'static str, parent: Option<u32>, data: &'static [u8]) -> Self {
        Self {
            name,
            kind: EntryKind::File,
            data,
            parent,
            children: &[],
        }
    }
}

/// A filesystem backed by a static node table.
#[derive(Clone, Copy, Debug)]
pub struct StaticFs {
    nodes: &'static [StaticNode],
}

impl StaticFs {
    /// Creates a filesystem over `nodes`.
    pub const fn new(nodes: &'static [StaticNode]) -> Self {
        Self { nodes }
    }

    fn node(&self, node: NodeId) -> Option<&'static StaticNode> {
        self.nodes.get(node.0 as usize)
    }
}

impl FileSystem for StaticFs {
    fn node_count(&self) -> usize {
        self.nodes.len()
    }

    fn kind(&self, node: NodeId) -> Option<EntryKind> {
        self.node(node).map(|node| node.kind)
    }

    fn name(&self, node: NodeId) -> Option<&str> {
        self.node(node).map(|node| node.name)
    }

    fn parent(&self, node: NodeId) -> Option<NodeId> {
        self.node(node)?.parent.map(NodeId)
    }

    fn size(&self, node: NodeId) -> usize {
        self.node(node).map(|node| node.data.len()).unwrap_or(0)
    }

    fn read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> usize {
        let Some(entry) = self.node(node) else {
            return 0;
        };
        if offset >= entry.data.len() {
            return 0;
        }
        let available = &entry.data[offset..];
        let len = available.len().min(out.len());
        out[..len].copy_from_slice(&available[..len]);
        len
    }

    fn child_count(&self, node: NodeId) -> usize {
        self.node(node).map(|node| node.children.len()).unwrap_or(0)
    }

    fn child_at(&self, node: NodeId, index: usize) -> Option<NodeId> {
        self.node(node)?.children.get(index).copied().map(NodeId)
    }
}

#[cfg(test)]
mod static_tests {
    use super::*;

    static NODES: &[StaticNode] = &[
        StaticNode::dir("/", None, &[1, 2, 3]),
        StaticNode::file("readme.txt", Some(0), b"hello filesystem\n"),
        StaticNode::dir("docs", Some(0), &[4]),
        StaticNode::dir("empty", Some(0), &[]),
        StaticNode::file("guide.md", Some(2), b"# Guide\n"),
    ];

    fn fs() -> StaticFs {
        StaticFs::new(NODES)
    }

    #[test]
    fn resolves_paths_and_navigates_parents() {
        let fs = fs();
        let guide = fs.resolve("/docs/guide.md").unwrap();
        assert_eq!(fs.name(guide), Some("guide.md"));
        assert_eq!(fs.kind(guide), Some(EntryKind::File));
        assert_eq!(fs.parent(guide), fs.resolve("/docs"));
        assert_eq!(fs.parent(fs.root()), None);
        assert_eq!(fs.resolve("docs/../readme.txt"), fs.resolve("/readme.txt"));
        assert_eq!(fs.resolve("/missing"), None);
        assert_eq!(fs.resolve("/../"), None);
    }

    #[test]
    fn builds_absolute_paths() {
        let fs = fs();
        let guide = fs.resolve("/docs/guide.md").unwrap();
        let mut buffer = [0u8; 64];
        assert_eq!(fs.path(guide, &mut buffer), Some("/docs/guide.md"));
        assert_eq!(fs.path(fs.root(), &mut buffer), Some("/"));

        let mut tiny = [0u8; 4];
        assert_eq!(fs.path(guide, &mut tiny), None);
    }

    #[test]
    fn reads_file_data_in_chunks() {
        let fs = fs();
        let readme = fs.resolve("/readme.txt").unwrap();
        assert_eq!(fs.size(readme), 17);

        let mut buffer = [0u8; 5];
        assert_eq!(fs.read(readme, 0, &mut buffer), 5);
        assert_eq!(&buffer, b"hello");
        assert_eq!(fs.read(readme, 6, &mut buffer), 5);
        assert_eq!(&buffer, b"files");
        assert_eq!(fs.read(readme, 17, &mut buffer), 0);
        assert_eq!(fs.read(fs.resolve("/docs").unwrap(), 0, &mut buffer), 0);
    }

    #[test]
    fn lists_children_in_image_order() {
        let fs = fs();
        let names: std::vec::Vec<_> = (0..fs.child_count(fs.root()))
            .map(|index| fs.name(fs.child_at(fs.root(), index).unwrap()).unwrap())
            .collect();
        assert_eq!(names, ["readme.txt", "docs", "empty"]);
        assert_eq!(fs.child_count(fs.resolve("/empty").unwrap()), 0);
    }
}
