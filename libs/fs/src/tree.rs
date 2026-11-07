//! Shared in-memory node table used by the on-disk filesystems.
//!
//! Mounting walks the directory tree once and records names, parents and the
//! location of each file's data. Lookups and listings after that are cheap and
//! do not touch the disk. File contents are still read on demand, so the table
//! only costs one small allocation per entry.
//!
//! The table's size is limited by the heap it uses rather than by an entry
//! count, because the kernel arena is small and untrusted volumes pick the
//! count: see [`MAX_TABLE_BYTES`].

use alloc::string::String;
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use crate::{EntryKind, FsError, NodeId, NodeTimes, MAX_PATH_DEPTH};

/// Heap budget for one mounted volume's node table: names, nodes and child
/// lists. It leaves room in the 64 MiB kernel arena for the table's own growth
/// (old and new arrays coexist while it reallocates), and for everything else.
/// At typical name lengths this is about 100,000 entries.
const MAX_TABLE_BYTES: usize = 16 * 1024 * 1024;

/// The chain has not been checked since it last changed.
const CHAIN_UNCHECKED: u8 = 0;
/// The chain is long enough for the file's size and doesn't loop.
const CHAIN_GOOD: u8 = 1;
/// The chain is truncated or cyclic.
const CHAIN_BAD: u8 = 2;

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
    /// Bytes that have ever been written (exFAT `ValidDataLength`). Reads past
    /// this return zeros, whatever the clusters hold. FAT32 keeps it at `size`.
    pub(crate) valid_size: u64,
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
    /// Where the last read ended, as `cluster index << 32 | cluster`, so that
    /// sequential reads don't walk the chain from the start each time. Zero
    /// means no hint. Reads share `&self`, hence the atomic: one word keeps the
    /// two halves consistent, and a stale hint from another reader is still a
    /// true position in the chain.
    cursor: AtomicU64,
    /// Result of the first read's check of the chain.
    chain: AtomicU8,
    /// Attribute bits; see [`attr`](crate::attr).
    pub(crate) attributes: u8,
    /// Seconds since the Unix epoch, or `None` where the disk has nothing.
    pub(crate) times: NodeTimes,
    /// The chain's last cluster and its index, once a write has found them.
    /// Writers hold `&mut`, so this needs no atomics. Without it, appending in
    /// chunks walks the whole chain per chunk.
    pub(crate) tail: Option<(u32, u32)>,
}

impl Node {
    fn new(
        name: String,
        kind: EntryKind,
        start_cluster: u32,
        size: u64,
        data_length: u64,
        contiguous: bool,
        parent: Option<u32>,
        entry: Option<EntryLocation>,
    ) -> Self {
        Self {
            name,
            kind,
            start_cluster,
            size,
            valid_size: size,
            data_length,
            contiguous,
            parent,
            children: Vec::new(),
            entry,
            alive: true,
            cursor: AtomicU64::new(0),
            chain: AtomicU8::new(CHAIN_UNCHECKED),
            attributes: 0,
            times: NodeTimes::NONE,
            tail: None,
        }
    }

    /// Forgets the read cursor and chain check after the chain changes.
    pub(crate) fn chain_changed(&self) {
        self.cursor.store(0, Ordering::Relaxed);
        self.chain.store(CHAIN_UNCHECKED, Ordering::Relaxed);
    }

    /// The best known position at or before cluster `index`.
    pub(crate) fn position_before(&self, index: u32) -> (u32, u32) {
        let hint = self.cursor.load(Ordering::Relaxed);
        let cluster = hint as u32;
        let hint_index = (hint >> 32) as u32;
        if cluster >= 2 && hint_index <= index {
            (hint_index, cluster)
        } else {
            (0, self.start_cluster)
        }
    }

    pub(crate) fn remember_position(&self, index: u32, cluster: u32) {
        self.cursor
            .store(((index as u64) << 32) | cluster as u64, Ordering::Relaxed);
    }

    /// Checks once that the chain holds `needed` clusters and doesn't loop.
    ///
    /// A cycle within the file's length would otherwise read as repeated data,
    /// and a short chain as a short file. The whole chain is walked, because a
    /// loop only shows once the walk has gone around it. The verdict is kept, so
    /// later reads cost one load. I/O errors aren't kept, since they may pass.
    pub(crate) fn check_chain(
        &self,
        needed: u64,
        mut next: impl FnMut(u32) -> Result<Option<u32>, FsError>,
    ) -> Result<(), FsError> {
        match self.chain.load(Ordering::Relaxed) {
            CHAIN_GOOD => return Ok(()),
            CHAIN_BAD => return Err(FsError::Corrupt),
            _ => {}
        }
        // Brent's algorithm: compare against a marker moved at powers of two.
        // It needs no memory, and stops within three times the chain's length.
        let mut cluster = self.start_cluster;
        let mut length = 1u64;
        let mut marker = cluster;
        let mut since_marker = 0u64;
        let mut stride = 1u64;
        let good = loop {
            let following = match next(cluster) {
                Ok(following) => following,
                Err(FsError::Corrupt) => None,
                Err(error) => return Err(error),
            };
            let Some(value) = following else {
                break length >= needed;
            };
            if value == marker {
                break false;
            }
            cluster = value;
            length += 1;
            since_marker += 1;
            if since_marker == stride {
                marker = cluster;
                since_marker = 0;
                stride *= 2;
            }
        };
        let verdict = if good { CHAIN_GOOD } else { CHAIN_BAD };
        self.chain.store(verdict, Ordering::Relaxed);
        if good {
            Ok(())
        } else {
            Err(FsError::Corrupt)
        }
    }
}

/// Tracks which clusters a directory walk has seen, one bit per cluster.
///
/// A directory chain that revisits a cluster is a loop or a cross-link, and this
/// finds either in constant time per cluster.
pub(crate) struct ClusterSet {
    words: Vec<u64>,
}

impl ClusterSet {
    pub(crate) fn new(cluster_count: u32) -> Result<Self, FsError> {
        let len = (cluster_count as usize).div_ceil(64);
        let mut words = Vec::new();
        words
            .try_reserve_exact(len)
            .map_err(|_| FsError::OutOfMemory)?;
        words.resize(len, 0);
        Ok(Self { words })
    }

    /// True when `cluster` was seen. Out-of-range clusters count as seen.
    pub(crate) fn contains(&self, cluster: u32) -> bool {
        let index = cluster.wrapping_sub(2) as usize;
        self.words
            .get(index / 64)
            .is_none_or(|word| word & (1 << (index % 64)) != 0)
    }

    /// Records `cluster`, returning `false` if it was already seen.
    pub(crate) fn insert(&mut self, cluster: u32) -> bool {
        if self.contains(cluster) {
            return false;
        }
        let index = cluster.wrapping_sub(2) as usize;
        self.words[index / 64] |= 1 << (index % 64);
        true
    }
}

/// Makes a name found on disk usable as a path component, and nothing else: a name is
/// kept as it is, control characters and all, and escaped by whatever shows it (see
/// `feos_text::display_name`). A separator or NUL would mean something else in a path.
pub(crate) fn path_component(name: String) -> String {
    let separator = |character: char| matches!(character, '/' | '\0');
    if name.contains(separator) {
        name.replace(separator, "_")
    } else {
        name
    }
}

/// Allocates a zeroed buffer, reporting failure rather than aborting.
pub(crate) fn try_zeroed(len: usize) -> Result<Vec<u8>, FsError> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(len)
        .map_err(|_| FsError::OutOfMemory)?;
    buffer.resize(len, 0);
    Ok(buffer)
}

/// Pushes without the abort of an infallible allocation.
pub(crate) fn try_push<T>(list: &mut Vec<T>, value: T) -> Result<(), FsError> {
    list.try_reserve(1).map_err(|_| FsError::OutOfMemory)?;
    list.push(value);
    Ok(())
}

#[derive(Debug)]
pub(crate) struct NodeTable {
    pub(crate) nodes: Vec<Node>,
    /// Slots of removed nodes, reused by later additions.
    free: Vec<u32>,
    live: usize,
    /// Heap charged against [`MAX_TABLE_BYTES`] for names and child links.
    name_bytes: usize,
}

impl NodeTable {
    pub(crate) fn new(root_start_cluster: u32) -> Self {
        Self {
            nodes: alloc::vec![Node::new(
                String::from("/"),
                EntryKind::Directory,
                root_start_cluster,
                0,
                0,
                false,
                None,
                None,
            )],
            free: Vec::new(),
            live: 1,
            name_bytes: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.live
    }

    /// One past the highest node ID in use. IDs index slots, so this can exceed
    /// [`len`](Self::len) after a removal.
    pub(crate) fn id_limit(&self) -> usize {
        self.nodes.len()
    }

    /// Fails if another entry called `name` would not fit, so a backend can ask
    /// before it changes the disk.
    pub(crate) fn ensure_room(&self, name: &str) -> Result<(), FsError> {
        let slots = self.nodes.len() + usize::from(self.free.is_empty());
        let used = slots
            .saturating_mul(size_of::<Node>())
            .saturating_add(self.name_bytes)
            .saturating_add(name.len() + size_of::<u32>());
        if used > MAX_TABLE_BYTES {
            return Err(FsError::TooManyNodes);
        }
        Ok(())
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
        self.ensure_room(&name)?;
        let parent_index = parent as usize;
        if !self.nodes.get(parent_index).is_some_and(|node| node.alive) {
            return Err(FsError::Corrupt);
        }
        self.nodes[parent_index]
            .children
            .try_reserve(1)
            .map_err(|_| FsError::OutOfMemory)?;

        let name_bytes = name.len() + size_of::<u32>();
        let node = Node::new(
            name,
            kind,
            start_cluster,
            size,
            data_length,
            contiguous,
            Some(parent),
            entry,
        );
        let id = if let Some(id) = self.free.pop() {
            self.nodes[id as usize] = node;
            id
        } else {
            let id = u32::try_from(self.nodes.len()).map_err(|_| FsError::TooManyNodes)?;
            if self.nodes.len() == self.nodes.capacity() {
                // Grow gently: doubling a large table would need three times its
                // size while both arrays exist.
                let more = (self.nodes.len() / 4).max(16);
                self.nodes
                    .try_reserve_exact(more)
                    .map_err(|_| FsError::OutOfMemory)?;
            }
            self.nodes.push(node);
            id
        };
        self.nodes[parent_index].children.push(id);
        self.live += 1;
        self.name_bytes += name_bytes;
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
        let released = entry.name.len() + size_of::<u32>();
        let parent_entry = self
            .nodes
            .get_mut(parent as usize)
            .ok_or(FsError::Corrupt)?;
        parent_entry.children.retain(|&child| child != node.0);
        let slot = &mut self.nodes[index];
        slot.alive = false;
        slot.name = String::new();
        slot.children = Vec::new();
        self.live -= 1;
        self.name_bytes = self.name_bytes.saturating_sub(released);
        // If the free list can't grow, the slot is just not reused.
        let _ = try_push(&mut self.free, node.0);
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
        self.name_bytes = (self.name_bytes + name.len()).saturating_sub(target.name.len());
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
            if guard > self.nodes.len() {
                break;
            }
            current = self.get(id).and_then(|entry| entry.parent.map(NodeId));
        }
        false
    }

    /// Number of parents between `node` and the root.
    pub(crate) fn depth(&self, node: NodeId) -> usize {
        let mut depth = 0usize;
        let mut current = self.parent(node);
        while let Some(id) = current {
            depth += 1;
            if depth > self.nodes.len() {
                break;
            }
            current = self.parent(id);
        }
        depth
    }

    /// Levels below `node`, which is zero for a file or an empty directory.
    /// The recursion is bounded because depth is capped at mount and on change.
    pub(crate) fn height(&self, node: NodeId) -> usize {
        self.get(node).map_or(0, |entry| {
            entry
                .children
                .iter()
                .map(|&child| self.height(NodeId(child)) + 1)
                .max()
                .unwrap_or(0)
        })
    }

    /// Fails if a node with `height` levels below it can't live under `parent`.
    pub(crate) fn check_depth(&self, parent: NodeId, height: usize) -> Result<(), FsError> {
        if self.depth(parent) + 1 + height > MAX_PATH_DEPTH {
            return Err(FsError::TooDeep);
        }
        Ok(())
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
