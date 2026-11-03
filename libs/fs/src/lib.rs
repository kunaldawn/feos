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
//! through the [`BlockDevice`]. A RAM-backed device is included for embedding
//! and for tests:
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
mod error;
mod exfat;
mod fat32;
mod mount;
mod tree;
mod volume;

pub use block::{
    BlockDevice, MemoryBlockDevice, MemoryBlockDeviceMut, RamDisk, WritableBlockDevice,
    MAX_BLOCK_SIZE,
};
pub use error::FsError;
pub use exfat::Exfat;
pub use fat32::Fat32;
pub use mount::MountTable;
pub use volume::{detect, VolumeKind};

use alloc::boxed::Box;

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
        VolumeKind::Exfat => Ok(Box::new(Exfat::mount(device)?)),
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
        VolumeKind::Exfat => Ok(Box::new(Exfat::mount(device)?)),
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
pub const MAX_PATH_DEPTH: usize = 32;

/// A read-only filesystem view.
///
/// All methods are cheap lookups; file data is copied by
/// [`read`](FileSystem::read), so callers control the buffer size and no
/// allocation is required after mounting.
pub trait FileSystem: Sync {
    /// Number of nodes in the filesystem.
    fn node_count(&self) -> usize;

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
    /// `node` is not a file.
    fn read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> usize;

    /// Number of children of `node`.
    fn child_count(&self, node: NodeId) -> usize;

    /// Returns the child at `index`, in filesystem order.
    fn child_at(&self, node: NodeId, index: usize) -> Option<NodeId>;

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
pub trait WritableFileSystem: FileSystem {
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
