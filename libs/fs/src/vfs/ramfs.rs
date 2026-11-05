//! A writable filesystem in RAM, optionally seeded from a read-only one.
//!
//! This is what makes an embedded image writable when no disk is attached, and
//! what backs `/tmp` and `/dev`. A seeded file stays on the image until someone
//! writes to it, when it's copied up, so seeding costs a node per entry and no
//! file data.

use alloc::string::String;
use alloc::vec::Vec;

use crate::{
    attr, EntryKind, FileSystem, FsError, NodeId, NodeTimes, TimeSource, VolumeStats,
    WritableFileSystem,
};

/// File data a [`RamFs`] may hold by default. The kernel heap is a 64 MiB arena,
/// and pipeline buffers and variables need their share of it.
pub const DEFAULT_QUOTA: usize = 24 * 1024 * 1024;

/// Deepest a tree can go here; more than a FAT volume, as paths are in memory.
const MAX_DEPTH: usize = 48;

/// Special behaviour of a device node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceKind {
    /// Reads as empty, discards writes.
    Null,
    /// Reads as endless zeros, discards writes.
    Zero,
}

enum Data {
    Owned(Vec<u8>),
    /// Still on the image it was seeded from.
    Lower(NodeId, usize),
    Device(DeviceKind),
}

struct RamNode {
    name: String,
    parent: Option<u32>,
    kind: EntryKind,
    children: Vec<u32>,
    data: Data,
    attributes: u8,
    times: NodeTimes,
    alive: bool,
}

/// A writable tree in RAM.
pub struct RamFs {
    nodes: Vec<RamNode>,
    free: Vec<u32>,
    live: usize,
    used: usize,
    quota: usize,
    lower: Option<&'static dyn FileSystem>,
    now: Option<TimeSource>,
}

impl RamFs {
    /// An empty tree holding at most `quota` bytes of file data.
    pub fn new(quota: usize) -> Self {
        Self {
            nodes: alloc::vec![RamNode {
                name: String::from("/"),
                parent: None,
                kind: EntryKind::Directory,
                children: Vec::new(),
                data: Data::Owned(Vec::new()),
                attributes: 0,
                times: NodeTimes::NONE,
                alive: true,
            }],
            free: Vec::new(),
            live: 1,
            used: 0,
            quota,
            lower: None,
            now: None,
        }
    }

    /// A tree that starts as a copy of `lower`, whose file data stays there
    /// until it's written to.
    pub fn seeded(lower: &'static dyn FileSystem, quota: usize) -> Result<Self, FsError> {
        let mut fs = Self::new(quota);
        fs.lower = Some(lower);
        fs.copy_children(lower, lower.root(), NodeId::ROOT, 0)?;
        Ok(fs)
    }

    fn copy_children(
        &mut self,
        lower: &'static dyn FileSystem,
        from: NodeId,
        to: NodeId,
        depth: usize,
    ) -> Result<(), FsError> {
        if depth >= MAX_DEPTH {
            return Err(FsError::TooDeep);
        }
        for index in 0..lower.child_count(from) {
            let Some(child) = lower.child_at(from, index) else {
                continue;
            };
            let Some(name) = lower.name(child) else {
                continue;
            };
            match lower.kind(child) {
                Some(EntryKind::Directory) => {
                    let dir = self.add_node(to, name, EntryKind::Directory, Data::Owned(Vec::new()))?;
                    self.copy_metadata(lower, child, dir);
                    self.copy_children(lower, child, dir, depth + 1)?;
                }
                Some(EntryKind::File) => {
                    let data = Data::Lower(child, lower.size(child));
                    let file = self.add_node(to, name, EntryKind::File, data)?;
                    self.copy_metadata(lower, child, file);
                }
                None => {}
            }
        }
        Ok(())
    }

    fn copy_metadata(&mut self, lower: &dyn FileSystem, from: NodeId, to: NodeId) {
        let attributes = lower.attributes(from);
        let times = lower.times(from);
        if let Some(node) = self.node_mut(to) {
            node.attributes = attributes;
            node.times = times;
        }
    }

    /// Adds a device node under `parent`.
    pub fn add_device(
        &mut self,
        parent: NodeId,
        name: &str,
        kind: DeviceKind,
    ) -> Result<NodeId, FsError> {
        self.check_new_name(parent, name)?;
        self.add_node(parent, name, EntryKind::File, Data::Device(kind))
    }

    /// Bytes of file data currently held.
    pub fn used_bytes(&self) -> usize {
        self.used
    }

    fn node(&self, node: NodeId) -> Option<&RamNode> {
        self.nodes.get(node.0 as usize).filter(|node| node.alive)
    }

    fn node_mut(&mut self, node: NodeId) -> Option<&mut RamNode> {
        self.nodes.get_mut(node.0 as usize).filter(|node| node.alive)
    }

    fn depth(&self, node: NodeId) -> usize {
        let mut depth = 0;
        let mut current = self.node(node).and_then(|node| node.parent);
        while let Some(parent) = current {
            depth += 1;
            current = self.nodes[parent as usize].parent;
        }
        depth
    }

    fn check_new_name(&self, parent: NodeId, name: &str) -> Result<(), FsError> {
        let directory = self.node(parent).ok_or(FsError::NotFound)?;
        if directory.kind != EntryKind::Directory {
            return Err(FsError::NotDirectory);
        }
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(FsError::InvalidName);
        }
        if self.lookup(parent, name).is_some() {
            return Err(FsError::NameExists);
        }
        if self.depth(parent) + 1 >= MAX_DEPTH {
            return Err(FsError::TooDeep);
        }
        Ok(())
    }

    fn add_node(
        &mut self,
        parent: NodeId,
        name: &str,
        kind: EntryKind,
        data: Data,
    ) -> Result<NodeId, FsError> {
        let mut owned = String::new();
        owned
            .try_reserve_exact(name.len())
            .map_err(|_| FsError::OutOfMemory)?;
        owned.push_str(name);
        let now = self.now.and_then(|now| now());
        let node = RamNode {
            name: owned,
            parent: Some(parent.0),
            kind,
            children: Vec::new(),
            data,
            attributes: if kind == EntryKind::File { attr::ARCHIVE } else { 0 },
            times: NodeTimes {
                created: now,
                modified: now,
                accessed: now,
            },
            alive: true,
        };
        self.nodes
            .get_mut(parent.0 as usize)
            .ok_or(FsError::NotFound)?
            .children
            .try_reserve(1)
            .map_err(|_| FsError::OutOfMemory)?;
        let id = match self.free.pop() {
            Some(id) => {
                self.nodes[id as usize] = node;
                id
            }
            None => {
                self.nodes.try_reserve(1).map_err(|_| FsError::OutOfMemory)?;
                let id = u32::try_from(self.nodes.len()).map_err(|_| FsError::TooManyNodes)?;
                self.nodes.push(node);
                id
            }
        };
        self.nodes[parent.0 as usize].children.push(id);
        self.live += 1;
        Ok(NodeId(id))
    }

    /// Moves a seeded file's bytes into RAM, ready to change.
    fn copy_up(&mut self, node: NodeId) -> Result<(), FsError> {
        let current = self.node(node).ok_or(FsError::NotFound)?;
        let Data::Lower(source, size) = current.data else {
            return Ok(());
        };
        let lower = self.lower.ok_or(FsError::Corrupt)?;
        let total = self.used.checked_add(size).ok_or(FsError::NoSpace)?;
        if total > self.quota {
            return Err(FsError::NoSpace);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| FsError::OutOfMemory)?;
        bytes.resize(size, 0);
        let mut done = 0usize;
        while done < size {
            let count = lower.try_read(source, done, &mut bytes[done..])?;
            if count == 0 {
                return Err(FsError::Corrupt);
            }
            done += count;
        }
        self.used = total;
        if let Some(node) = self.node_mut(node) {
            node.data = Data::Owned(bytes);
        }
        Ok(())
    }

    fn touch(&mut self, node: NodeId) {
        let now = self.now.and_then(|now| now());
        if let Some(entry) = self.node_mut(node) {
            entry.attributes |= attr::ARCHIVE;
            if now.is_some() {
                entry.times.modified = now;
                entry.times.accessed = now;
            }
        }
    }

    fn resize(&mut self, node: NodeId, size: usize) -> Result<(), FsError> {
        self.copy_up(node)?;
        let used = self.used;
        let quota = self.quota;
        let entry = self.node_mut(node).ok_or(FsError::NotFound)?;
        if let Data::Owned(bytes) = &mut entry.data {
            if size > bytes.len() {
                let extra = size - bytes.len();
                let total = used.checked_add(extra).ok_or(FsError::NoSpace)?;
                if total > quota {
                    return Err(FsError::NoSpace);
                }
                bytes.try_reserve(extra).map_err(|_| FsError::NoSpace)?;
                bytes.resize(size, 0);
                self.used = total;
            } else {
                let removed = bytes.len() - size;
                bytes.truncate(size);
                self.used = used - removed;
            }
        }
        Ok(())
    }
}

impl FileSystem for RamFs {
    fn node_count(&self) -> usize {
        self.live
    }

    fn node_id_limit(&self) -> usize {
        self.nodes.len()
    }

    fn kind(&self, node: NodeId) -> Option<EntryKind> {
        self.node(node).map(|node| node.kind)
    }

    fn name(&self, node: NodeId) -> Option<&str> {
        self.node(node).map(|node| node.name.as_str())
    }

    fn parent(&self, node: NodeId) -> Option<NodeId> {
        self.node(node)?.parent.map(NodeId)
    }

    fn size(&self, node: NodeId) -> usize {
        match self.node(node).map(|node| &node.data) {
            Some(Data::Owned(bytes)) => bytes.len(),
            Some(Data::Lower(_, size)) => *size,
            _ => 0,
        }
    }

    fn read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> usize {
        self.try_read(node, offset, out).unwrap_or(0)
    }

    fn try_read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> Result<usize, FsError> {
        let entry = self.node(node).ok_or(FsError::NotFound)?;
        if entry.kind != EntryKind::File {
            return Err(FsError::NotFile);
        }
        match &entry.data {
            Data::Owned(bytes) => {
                let tail = bytes.get(offset..).unwrap_or(&[]);
                let count = tail.len().min(out.len());
                out[..count].copy_from_slice(&tail[..count]);
                Ok(count)
            }
            Data::Lower(source, _) => self.lower.ok_or(FsError::Corrupt)?.try_read(*source, offset, out),
            Data::Device(DeviceKind::Null) => Ok(0),
            Data::Device(DeviceKind::Zero) => {
                out.fill(0);
                Ok(out.len())
            }
        }
    }

    fn child_count(&self, node: NodeId) -> usize {
        self.node(node).map_or(0, |node| node.children.len())
    }

    fn child_at(&self, node: NodeId, index: usize) -> Option<NodeId> {
        self.node(node)?.children.get(index).copied().map(NodeId)
    }

    fn attributes(&self, node: NodeId) -> u8 {
        self.node(node).map_or(0, |node| node.attributes)
    }

    fn times(&self, node: NodeId) -> NodeTimes {
        self.node(node).map_or(NodeTimes::NONE, |node| node.times)
    }

    fn stats(&self) -> VolumeStats {
        const UNIT: usize = 4096;
        VolumeStats {
            block_size: UNIT as u64,
            total_blocks: (self.quota / UNIT) as u64,
            free_blocks: (self.quota.saturating_sub(self.used) / UNIT) as u64,
            read_only: false,
            kind: "ramfs",
        }
    }
}

impl WritableFileSystem for RamFs {
    fn create_file(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        self.check_new_name(parent, name)?;
        self.add_node(parent, name, EntryKind::File, Data::Owned(Vec::new()))
    }

    fn create_dir(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        self.check_new_name(parent, name)?;
        self.add_node(parent, name, EntryKind::Directory, Data::Owned(Vec::new()))
    }

    fn remove(&mut self, node: NodeId) -> Result<(), FsError> {
        let entry = self.node(node).ok_or(FsError::NotFound)?;
        let parent = entry.parent.ok_or(FsError::Unsupported)?;
        if !entry.children.is_empty() {
            return Err(FsError::NotEmpty);
        }
        let released = match &entry.data {
            Data::Owned(bytes) => bytes.len(),
            _ => 0,
        };
        self.nodes[parent as usize]
            .children
            .retain(|&child| child != node.0);
        let slot = &mut self.nodes[node.0 as usize];
        slot.alive = false;
        slot.name = String::new();
        slot.data = Data::Owned(Vec::new());
        slot.children = Vec::new();
        self.used -= released;
        self.live -= 1;
        // If the free list can't grow, the slot is just not reused.
        if self.free.try_reserve(1).is_ok() {
            self.free.push(node.0);
        }
        Ok(())
    }

    fn rename(&mut self, node: NodeId, new_parent: NodeId, new_name: &str) -> Result<(), FsError> {
        let entry = self.node(node).ok_or(FsError::NotFound)?;
        let old_parent = entry.parent.ok_or(FsError::Unsupported)?;
        // Moving a directory under itself would cut it off from the root.
        let mut ancestor = Some(new_parent.0);
        while let Some(current) = ancestor {
            if current == node.0 {
                return Err(FsError::Unsupported);
            }
            ancestor = self.nodes.get(current as usize).and_then(|node| node.parent);
        }
        if self.node(new_parent).ok_or(FsError::NotFound)?.kind != EntryKind::Directory {
            return Err(FsError::NotDirectory);
        }
        match self.lookup(new_parent, new_name) {
            Some(found) if found == node => return Ok(()),
            Some(_) => return Err(FsError::NameExists),
            None => {}
        }
        if new_name.is_empty() || new_name == "." || new_name == ".." || new_name.contains('/') {
            return Err(FsError::InvalidName);
        }
        let mut name = String::new();
        name.try_reserve_exact(new_name.len())
            .map_err(|_| FsError::OutOfMemory)?;
        name.push_str(new_name);
        self.nodes[new_parent.0 as usize]
            .children
            .try_reserve(1)
            .map_err(|_| FsError::OutOfMemory)?;

        self.nodes[old_parent as usize]
            .children
            .retain(|&child| child != node.0);
        self.nodes[new_parent.0 as usize].children.push(node.0);
        let entry = &mut self.nodes[node.0 as usize];
        entry.parent = Some(new_parent.0);
        entry.name = name;
        Ok(())
    }

    fn write(&mut self, node: NodeId, offset: usize, data: &[u8]) -> Result<usize, FsError> {
        let entry = self.node(node).ok_or(FsError::NotFound)?;
        if entry.kind != EntryKind::File {
            return Err(FsError::NotFile);
        }
        if matches!(entry.data, Data::Device(_)) {
            return Ok(data.len());
        }
        let end = offset.checked_add(data.len()).ok_or(FsError::NoSpace)?;
        self.copy_up(node)?;
        if end > self.size(node) {
            self.resize(node, end)?;
        }
        if let Some(RamNode {
            data: Data::Owned(bytes),
            ..
        }) = self.node_mut(node)
        {
            bytes[offset..end].copy_from_slice(data);
        }
        self.touch(node);
        Ok(data.len())
    }

    fn set_len(&mut self, node: NodeId, size: usize) -> Result<(), FsError> {
        let entry = self.node(node).ok_or(FsError::NotFound)?;
        if entry.kind != EntryKind::File {
            return Err(FsError::NotFile);
        }
        if matches!(entry.data, Data::Device(_)) {
            return Ok(());
        }
        self.resize(node, size)?;
        self.touch(node);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), FsError> {
        Ok(())
    }

    fn set_attributes(&mut self, node: NodeId, attributes: u8) -> Result<(), FsError> {
        self.node_mut(node).ok_or(FsError::NotFound)?.attributes = attributes & attr::SETTABLE;
        Ok(())
    }

    fn set_times(&mut self, node: NodeId, times: NodeTimes) -> Result<(), FsError> {
        let entry = self.node_mut(node).ok_or(FsError::NotFound)?;
        if times.created.is_some() {
            entry.times.created = times.created;
        }
        if times.modified.is_some() {
            entry.times.modified = times.modified;
        }
        if times.accessed.is_some() {
            entry.times.accessed = times.accessed;
        }
        Ok(())
    }

    fn set_time_source(&mut self, now: TimeSource) {
        self.now = Some(now);
    }

    fn make_device(&mut self, node: NodeId, kind: DeviceKind) -> Result<(), FsError> {
        self.resize(node, 0)?;
        self.node_mut(node).ok_or(FsError::NotFound)?.data = Data::Device(kind);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StaticFs, StaticNode};

    static NODES: &[StaticNode] = &[
        StaticNode::dir("/", None, &[1, 2]),
        StaticNode::file("a.txt", Some(0), b"hello"),
        StaticNode::dir("docs", Some(0), &[3]),
        StaticNode::file("b.txt", Some(2), b"nested"),
    ];
    static LOWER: StaticFs = StaticFs::new(NODES);

    #[test]
    fn a_seeded_file_copies_up_only_when_written() {
        let mut fs = RamFs::seeded(&LOWER, 1024).unwrap();
        let file = fs.resolve("/docs/b.txt").unwrap();
        assert_eq!(fs.size(file), 6);
        assert_eq!(fs.used_bytes(), 0);
        let mut out = [0u8; 6];
        assert_eq!(fs.read(file, 0, &mut out), 6);
        assert_eq!(&out, b"nested");

        fs.write(file, 6, b"!").unwrap();
        assert_eq!(fs.used_bytes(), 7);
        assert_eq!(fs.read_to_vec(file).unwrap(), b"nested!");
    }

    #[test]
    fn the_quota_is_enforced_and_given_back() {
        let mut fs = RamFs::new(10);
        let file = fs.create_file(NodeId::ROOT, "f").unwrap();
        assert_eq!(fs.write(file, 0, &[1; 11]), Err(FsError::NoSpace));
        assert_eq!(fs.size(file), 0);
        fs.write(file, 0, &[1; 10]).unwrap();
        assert_eq!(fs.write(file, 10, &[1]), Err(FsError::NoSpace));
        fs.remove(file).unwrap();
        assert_eq!(fs.used_bytes(), 0);
        let file = fs.create_file(NodeId::ROOT, "g").unwrap();
        fs.write(file, 0, &[1; 10]).unwrap();
        fs.set_len(file, 2).unwrap();
        assert_eq!(fs.used_bytes(), 2);
    }

    #[test]
    fn slots_are_reused_so_a_loop_does_not_grow() {
        let mut fs = RamFs::new(1024);
        for _ in 0..1000 {
            let file = fs.create_file(NodeId::ROOT, "x").unwrap();
            fs.remove(file).unwrap();
        }
        assert!(fs.node_id_limit() <= 2);
    }

    #[test]
    fn renames_move_nodes_and_refuse_cycles_and_collisions() {
        let mut fs = RamFs::new(1024);
        let a = fs.create_dir(NodeId::ROOT, "a").unwrap();
        let b = fs.create_dir(a, "b").unwrap();
        let file = fs.create_file(NodeId::ROOT, "f").unwrap();
        assert_eq!(fs.rename(a, b, "loop"), Err(FsError::Unsupported));
        fs.rename(file, b, "g").unwrap();
        assert_eq!(fs.resolve("/a/b/g"), Some(file));
        assert_eq!(fs.resolve("/f"), None);
        let other = fs.create_file(b, "h").unwrap();
        assert_eq!(fs.rename(other, b, "g"), Err(FsError::NameExists));
    }

    #[test]
    fn devices_discard_writes() {
        let mut fs = RamFs::new(0);
        let null = fs.add_device(NodeId::ROOT, "null", DeviceKind::Null).unwrap();
        let zero = fs.add_device(NodeId::ROOT, "zero", DeviceKind::Zero).unwrap();
        assert_eq!(fs.write(null, 0, b"gone"), Ok(4));
        let mut out = [7u8; 4];
        assert_eq!(fs.read(null, 0, &mut out), 0);
        assert_eq!(fs.read(zero, 123, &mut out), 4);
        assert_eq!(out, [0; 4]);
        assert_eq!(fs.size(zero), 0);
    }

    #[test]
    fn a_tree_too_deep_is_refused() {
        let mut fs = RamFs::new(0);
        let mut dir = NodeId::ROOT;
        for _ in 0..MAX_DEPTH - 1 {
            dir = fs.create_dir(dir, "d").unwrap();
        }
        assert_eq!(fs.create_dir(dir, "d"), Err(FsError::TooDeep));
    }
}
