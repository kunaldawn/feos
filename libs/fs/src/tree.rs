//! Shared in-memory node table used by the on-disk filesystems.
//!
//! Mounting walks the directory tree once and records names, parents and the
//! location of each file's data. Lookups and listings after that are cheap and
//! do not touch the disk. File contents are still read on demand, so the table
//! only costs one small allocation per entry.

use alloc::string::String;
use alloc::vec::Vec;

use crate::{EntryKind, FsError, NodeId};

/// Upper bound on directory entries kept in memory for one mounted volume.
const MAX_NODES: usize = 1 << 20;

/// Where a node's directory entries live on disk.
///
/// FAT32 entry sets are one or more long-name entries followed by a short
/// entry. exFAT entry sets are a primary file entry, a stream entry and one or
/// more UTF-16 name entries. Keeping the offsets lets the writable backends
/// update sizes, delete entries and rename in place.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EntryLocation {
    /// First entry of the set (first LFN entry for FAT32, primary for exFAT).
    pub(crate) first: u64,
    /// Short entry (FAT32) or primary file entry (exFAT).
    pub(crate) primary: u64,
    /// Number of 32-byte entries in the set.
    pub(crate) count: u8,
    /// Stream entry offset (exFAT), zero for FAT32.
    pub(crate) stream: u64,
}

#[derive(Debug)]
pub(crate) struct Node {
    pub(crate) name: String,
    pub(crate) kind: EntryKind,
    pub(crate) start_cluster: u32,
    /// Logical size in bytes. Directories report zero through `FileSystem`.
    pub(crate) size: u64,
    /// Allocated stream length in bytes. For exFAT directories this is the
    /// directory's `DataLength`; for files it is normally the file size.
    pub(crate) data_length: u64,
    /// True when the stream is a contiguous run of clusters (exFAT
    /// `NoFatChain`). FAT32 always uses a FAT chain.
    pub(crate) contiguous: bool,
    pub(crate) parent: Option<u32>,
    pub(crate) children: Vec<u32>,
    /// On-disk entry set, or `None` for the root.
    pub(crate) entry: Option<EntryLocation>,
    pub(crate) alive: bool,
}

#[derive(Debug)]
pub(crate) struct NodeTable {
    pub(crate) nodes: Vec<Node>,
    live: usize,
}

impl NodeTable {
    pub(crate) fn new(root_start_cluster: u32) -> Self {
        Self {
            nodes: alloc::vec![Node {
                name: String::from("/"),
                kind: EntryKind::Directory,
                start_cluster: root_start_cluster,
                size: 0,
                data_length: 0,
                contiguous: false,
                parent: None,
                children: Vec::new(),
                entry: None,
                alive: true,
            }],
            live: 1,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.live
    }

    pub(crate) fn get(&self, node: NodeId) -> Option<&Node> {
        self.nodes.get(node.0 as usize).filter(|node| node.alive)
    }

    pub(crate) fn get_mut(&mut self, node: NodeId) -> Option<&mut Node> {
        self.nodes
            .get_mut(node.0 as usize)
            .filter(|node| node.alive)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add(
        &mut self,
        parent: u32,
        name: String,
        kind: EntryKind,
        start_cluster: u32,
        size: u64,
        data_length: u64,
        contiguous: bool,
        entry: Option<EntryLocation>,
    ) -> Result<u32, FsError> {
        if self.live >= MAX_NODES {
            return Err(FsError::TooManyNodes);
        }
        let parent_index = parent as usize;
        if parent_index >= self.nodes.len() || !self.nodes[parent_index].alive {
            return Err(FsError::Corrupt);
        }
        let id = u32::try_from(self.nodes.len()).map_err(|_| FsError::TooManyNodes)?;
        self.nodes.push(Node {
            name,
            kind,
            start_cluster,
            size,
            data_length,
            contiguous,
            parent: Some(parent),
            children: Vec::new(),
            entry,
            alive: true,
        });
        self.nodes[parent_index].children.push(id);
        self.live += 1;
        Ok(id)
    }

    pub(crate) fn remove(&mut self, node: NodeId) -> Result<(), FsError> {
        let index = node.0 as usize;
        let entry = self.nodes.get(index).ok_or(FsError::NotFound)?;
        if !entry.alive {
            return Err(FsError::NotFound);
        }
        if !entry.children.is_empty() {
            return Err(FsError::NotEmpty);
        }
        let parent = entry.parent.ok_or(FsError::Unsupported)?;
        let parent_entry = self
            .nodes
            .get_mut(parent as usize)
            .ok_or(FsError::Corrupt)?;
        parent_entry.children.retain(|&child| child != node.0);
        self.nodes[index].alive = false;
        self.live -= 1;
        Ok(())
    }

    pub(crate) fn reparent(
        &mut self,
        node: NodeId,
        new_parent: NodeId,
        name: String,
        entry: EntryLocation,
    ) -> Result<(), FsError> {
        let index = node.0 as usize;
        let old_parent = self
            .nodes
            .get(index)
            .and_then(|entry| entry.parent)
            .ok_or(FsError::NotFound)?;
        let new_parent_index = new_parent.0 as usize;
        if !self
            .nodes
            .get(new_parent_index)
            .is_some_and(|entry| entry.alive && entry.kind == EntryKind::Directory)
        {
            return Err(FsError::NotDirectory);
        }
        if old_parent as usize >= self.nodes.len() {
            return Err(FsError::Corrupt);
        }
        self.nodes[old_parent as usize]
            .children
            .retain(|&child| child != node.0);
        self.nodes[new_parent_index].children.push(node.0);
        let target = &mut self.nodes[index];
        target.parent = Some(new_parent.0);
        target.name = name;
        target.entry = Some(entry);
        Ok(())
    }

    /// True when `ancestor` is `node` or one of its parents.
    pub(crate) fn is_ancestor(&self, ancestor: NodeId, node: NodeId) -> bool {
        let mut current = Some(node);
        let mut guard = 0usize;
        while let Some(id) = current {
            if id == ancestor {
                return true;
            }
            guard += 1;
            if guard > MAX_NODES {
                break;
            }
            current = self.get(id).and_then(|entry| entry.parent.map(NodeId));
        }
        false
    }

    pub(crate) fn kind(&self, node: NodeId) -> Option<EntryKind> {
        self.get(node).map(|node| node.kind)
    }

    pub(crate) fn name(&self, node: NodeId) -> Option<&str> {
        self.get(node).map(|node| node.name.as_str())
    }

    pub(crate) fn parent(&self, node: NodeId) -> Option<NodeId> {
        self.get(node)?.parent.map(NodeId)
    }

    pub(crate) fn size(&self, node: NodeId) -> usize {
        match self.get(node) {
            Some(node) if node.kind == EntryKind::File => {
                usize::try_from(node.size).unwrap_or(usize::MAX)
            }
            _ => 0,
        }
    }

    pub(crate) fn child_count(&self, node: NodeId) -> usize {
        self.get(node).map_or(0, |node| node.children.len())
    }

    pub(crate) fn child_at(&self, node: NodeId, index: usize) -> Option<NodeId> {
        self.get(node)?.children.get(index).copied().map(NodeId)
    }

    /// Looks up a child using FAT/exFAT's case-insensitive name rules.
    pub(crate) fn lookup(&self, dir: NodeId, name: &str) -> Option<NodeId> {
        let directory = self.get(dir)?;
        if directory.kind != EntryKind::Directory {
            return None;
        }
        for &child in &directory.children {
            let child_name = &self.nodes[child as usize].name;
            if names_equal(child_name, name) {
                return Some(NodeId(child));
            }
        }
        None
    }
}

fn names_equal(left: &str, right: &str) -> bool {
    if left == right || left.eq_ignore_ascii_case(right) {
        return true;
    }
    // exFAT names are UTF-16 and use the volume's up-case table. Unicode
    // lowercase is a good approximation and keeps non-ASCII lookups working
    // without storing the table (which is only needed for writing).
    if left.is_ascii() && right.is_ascii() {
        false
    } else {
        left.to_lowercase() == right.to_lowercase()
    }
}
