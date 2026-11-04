//! A read-only view that exposes several filesystems under one root.
//!
//! This is useful on boards that embed more than one disk image: each volume
//! appears as a directory such as `/fat32` or `/exfat`, while apps keep using
//! the normal [`FileSystem`] API.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::{EntryKind, FileSystem, FsError, NodeId};

struct Mount<'a> {
    name: &'static str,
    fs: Box<dyn FileSystem + 'a>,
    /// Global ID assigned to the mounted filesystem's root node.
    first: u32,
    /// Number of node IDs reported by the mounted filesystem.
    count: u32,
}

/// A filesystem that presents each mounted volume as a top-level directory.
pub struct MountTable<'a> {
    mounts: Vec<Mount<'a>>,
    children: u32,
}

enum Location {
    Root,
    MountDir(usize),
    Child(usize, NodeId),
}

impl<'a> MountTable<'a> {
    /// Creates an empty mount table.
    pub fn new() -> Self {
        Self {
            mounts: Vec::new(),
            children: 0,
        }
    }

    /// Adds `fs` under a top-level directory named `name`.
    ///
    /// Names are matched case-insensitively and must be unique.
    pub fn mount(
        &mut self,
        name: &'static str,
        fs: Box<dyn FileSystem + 'a>,
    ) -> Result<(), FsError> {
        if name.is_empty()
            || name.contains('/')
            || self
                .mounts
                .iter()
                .any(|mount| mount.name.eq_ignore_ascii_case(name))
        {
            return Err(FsError::InvalidName);
        }
        // IDs index slots, which can outnumber live nodes after a removal.
        let count = u32::try_from(fs.node_id_limit()).map_err(|_| FsError::TooManyNodes)?;
        if count == 0 {
            return Err(FsError::Corrupt);
        }
        let children = self
            .children
            .checked_add(count)
            .ok_or(FsError::TooManyNodes)?;
        self.mounts.push(Mount {
            name,
            fs,
            first: self.children + 1,
            count,
        });
        self.children = children;
        Ok(())
    }

    /// Returns `true` when no volumes are mounted.
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }

    fn mount_dir_id(&self, index: usize) -> NodeId {
        NodeId(1 + self.children + index as u32)
    }

    fn locate(&self, node: NodeId) -> Option<Location> {
        if node == NodeId::ROOT {
            return Some(Location::Root);
        }
        let mount_base = 1 + self.children;
        if node.0 >= mount_base {
            let index = (node.0 - mount_base) as usize;
            if index < self.mounts.len() {
                return Some(Location::MountDir(index));
            }
            return None;
        }
        for (index, mount) in self.mounts.iter().enumerate() {
            if node.0 >= mount.first && node.0 < mount.first + mount.count {
                return Some(Location::Child(index, NodeId(node.0 - mount.first)));
            }
        }
        None
    }

    fn map_child(&self, index: usize, child: NodeId) -> Option<NodeId> {
        let mount = self.mounts.get(index)?;
        if child.0 >= mount.count {
            return None;
        }
        Some(NodeId(mount.first + child.0))
    }
}

impl Default for MountTable<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystem for MountTable<'_> {
    fn node_count(&self) -> usize {
        let live: usize = self.mounts.iter().map(|mount| mount.fs.node_count()).sum();
        1 + live + self.mounts.len()
    }

    fn node_id_limit(&self) -> usize {
        1 + self.children as usize + self.mounts.len()
    }

    fn is_damaged(&self) -> bool {
        self.mounts.iter().any(|mount| mount.fs.is_damaged())
    }

    fn kind(&self, node: NodeId) -> Option<EntryKind> {
        match self.locate(node)? {
            Location::Root | Location::MountDir(_) => Some(EntryKind::Directory),
            Location::Child(index, child) => self.mounts.get(index)?.fs.kind(child),
        }
    }

    fn name(&self, node: NodeId) -> Option<&str> {
        match self.locate(node)? {
            Location::Root => Some("/"),
            Location::MountDir(index) => self.mounts.get(index).map(|mount| mount.name),
            Location::Child(index, child) => self.mounts.get(index)?.fs.name(child),
        }
    }

    fn parent(&self, node: NodeId) -> Option<NodeId> {
        match self.locate(node)? {
            Location::Root => None,
            Location::MountDir(_) => Some(NodeId::ROOT),
            Location::Child(index, child) => {
                let mount = self.mounts.get(index)?;
                match mount.fs.parent(child) {
                    Some(parent) if parent != mount.fs.root() => {
                        Some(NodeId(mount.first + parent.0))
                    }
                    _ => Some(self.mount_dir_id(index)),
                }
            }
        }
    }

    fn size(&self, node: NodeId) -> usize {
        match self.locate(node) {
            Some(Location::Child(index, child)) => self
                .mounts
                .get(index)
                .map_or(0, |mount| mount.fs.size(child)),
            _ => 0,
        }
    }

    fn read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> usize {
        match self.locate(node) {
            Some(Location::Child(index, child)) => self
                .mounts
                .get(index)
                .map_or(0, |mount| mount.fs.read(child, offset, out)),
            _ => 0,
        }
    }

    fn try_read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> Result<usize, FsError> {
        match self.locate(node) {
            Some(Location::Child(index, child)) => self
                .mounts
                .get(index)
                .ok_or(FsError::NotFound)?
                .fs
                .try_read(child, offset, out),
            Some(_) => Err(FsError::NotFile),
            None => Err(FsError::NotFound),
        }
    }

    fn child_count(&self, node: NodeId) -> usize {
        match self.locate(node) {
            Some(Location::Root) => self.mounts.len(),
            Some(Location::MountDir(index)) => self
                .mounts
                .get(index)
                .map_or(0, |mount| mount.fs.child_count(mount.fs.root())),
            Some(Location::Child(index, child)) => self
                .mounts
                .get(index)
                .map_or(0, |mount| mount.fs.child_count(child)),
            None => 0,
        }
    }

    fn child_at(&self, node: NodeId, index: usize) -> Option<NodeId> {
        match self.locate(node)? {
            Location::Root => {
                if index < self.mounts.len() {
                    Some(self.mount_dir_id(index))
                } else {
                    None
                }
            }
            Location::MountDir(mount_index) => {
                let mount = self.mounts.get(mount_index)?;
                let child = mount.fs.child_at(mount.fs.root(), index)?;
                self.map_child(mount_index, child)
            }
            Location::Child(mount_index, child) => {
                let mount = self.mounts.get(mount_index)?;
                let next = mount.fs.child_at(child, index)?;
                self.map_child(mount_index, next)
            }
        }
    }

    fn lookup(&self, dir: NodeId, name: &str) -> Option<NodeId> {
        match self.locate(dir)? {
            Location::Root => self
                .mounts
                .iter()
                .position(|mount| mount.name.eq_ignore_ascii_case(name))
                .map(|index| self.mount_dir_id(index)),
            Location::MountDir(mount_index) => {
                let mount = self.mounts.get(mount_index)?;
                let child = mount.fs.lookup(mount.fs.root(), name)?;
                self.map_child(mount_index, child)
            }
            Location::Child(mount_index, child) => {
                let mount = self.mounts.get(mount_index)?;
                let next = mount.fs.lookup(child, name)?;
                self.map_child(mount_index, next)
            }
        }
    }
}
