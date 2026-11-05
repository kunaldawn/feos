//! FAT12 and FAT16, read-only.
//!
//! DOS-era images use these. The cluster count is what makes a volume one or the
//! other, not what the boot sector calls itself. The directory format is shared
//! with FAT32; the differences are the entry width and the fixed-size root.
//!
//! As with FAT32, metadata is untrusted: what's damaged is skipped and flagged.

use alloc::vec::Vec;

use crate::block::BlockDevice;
use crate::fat32::{entry_times, short_name, LfnState};
use crate::tree::{try_push, try_zeroed, ClusterSet, NodeTable};
use crate::volume::Volume;
use crate::{attr, EntryKind, FileSystem, FsError, NodeId, NodeTimes, VolumeStats, MAX_PATH_DEPTH};

/// Below this many clusters a FAT holds 12-bit entries.
const FAT12_CLUSTERS: u32 = 4085;
/// At or above this many, a volume has to be FAT32.
const FAT16_CLUSTERS: u32 = 65_525;
/// Most entries in one directory, the same limit FAT32 has.
const MAX_DIRECTORY_ENTRIES: usize = 65_536;

/// A mounted FAT12 or FAT16 volume.
pub struct Fat16<D: BlockDevice> {
    volume: Volume<D>,
    nodes: NodeTable,
    wide: bool,
    fat_start: u64,
    data_start: u64,
    root_start: u64,
    root_entries: usize,
    cluster_size: usize,
    cluster_count: u32,
    damaged: bool,
}

struct Pending {
    node: u32,
    cluster: u32,
    depth: usize,
}

impl<D: BlockDevice> Fat16<D> {
    /// Mounts the volume on `device`.
    ///
    /// Returns [`FsError::UnsupportedVolume`] if it's really FAT32.
    pub fn mount(device: D) -> Result<Self, FsError> {
        let volume = Volume::cached(device)?;
        let boot = volume.read_array::<512>(0)?;
        if boot[510] != 0x55 || boot[511] != 0xaa {
            return Err(FsError::InvalidBootSector);
        }
        let word = |at: usize| u16::from_le_bytes([boot[at], boot[at + 1]]) as u64;
        let sector = word(11);
        if !(512..=4096).contains(&sector) || !sector.is_power_of_two() {
            return Err(FsError::InvalidBootSector);
        }
        let per_cluster = boot[13] as u64;
        if per_cluster == 0 || !per_cluster.is_power_of_two() {
            return Err(FsError::InvalidBootSector);
        }
        let reserved = word(14);
        let fats = boot[16] as u64;
        let root_entries = word(17);
        if reserved == 0 || fats == 0 || root_entries == 0 {
            return Err(FsError::InvalidBootSector);
        }
        let total = match word(19) {
            0 => u32::from_le_bytes([boot[32], boot[33], boot[34], boot[35]]) as u64,
            short => short,
        };
        let fat_sectors = word(22);
        if total == 0 || fat_sectors == 0 {
            return Err(FsError::InvalidBootSector);
        }
        if total * sector > volume.len() {
            return Err(FsError::Corrupt);
        }

        let root_sectors = (root_entries * 32).div_ceil(sector);
        let first_data = reserved + fats * fat_sectors + root_sectors;
        if first_data >= total {
            return Err(FsError::Corrupt);
        }
        let clusters = (total - first_data) / per_cluster;
        if clusters >= FAT16_CLUSTERS as u64 {
            return Err(FsError::UnsupportedVolume);
        }
        let cluster_count = clusters as u32;
        let wide = cluster_count >= FAT12_CLUSTERS;
        let entry_bytes = if wide {
            (cluster_count as u64 + 2) * 2
        } else {
            (cluster_count as u64 + 2) * 3 / 2 + 1
        };
        if entry_bytes > fat_sectors * sector {
            return Err(FsError::Corrupt);
        }

        let mut fs = Self {
            volume,
            nodes: NodeTable::new(0),
            wide,
            fat_start: reserved * sector,
            data_start: first_data * sector,
            root_start: (reserved + fats * fat_sectors) * sector,
            root_entries: root_entries as usize,
            cluster_size: (per_cluster * sector) as usize,
            cluster_count,
            damaged: false,
        };
        fs.scan_all()?;
        Ok(fs)
    }

    fn scan_all(&mut self) -> Result<(), FsError> {
        let mut queue: Vec<Pending> = Vec::new();
        let mut visited = ClusterSet::new(self.cluster_count)?;

        let mut root = try_zeroed(self.root_entries * 32)?;
        self.volume.read_bytes(self.root_start, &mut root)?;
        let root_start = self.root_start;
        let mut lfn = LfnState::new();
        for (index, entry) in root.chunks_exact(32).enumerate() {
            let offset = root_start + index as u64 * 32;
            if !self.scan_entry(entry, offset, 0, 0, &mut lfn, &mut queue, &visited)? {
                break;
            }
        }

        let mut buffer = try_zeroed(self.cluster_size)?;
        while let Some(pending) = queue.pop() {
            match self.scan_directory(pending, &mut buffer, &mut queue, &mut visited) {
                Ok(()) => {}
                Err(error @ (FsError::TooManyNodes | FsError::OutOfMemory)) => return Err(error),
                Err(_) => self.damaged = true,
            }
        }
        Ok(())
    }

    fn scan_directory(
        &mut self,
        pending: Pending,
        buffer: &mut [u8],
        queue: &mut Vec<Pending>,
        visited: &mut ClusterSet,
    ) -> Result<(), FsError> {
        let mut lfn = LfnState::new();
        let mut cluster = pending.cluster;
        let mut entries = 0usize;
        loop {
            if !visited.insert(cluster) {
                self.damaged = true;
                return Ok(());
            }
            let base = self.cluster_offset(cluster)?;
            self.volume.read_bytes(base, buffer)?;
            for (index, entry) in buffer.chunks_exact(32).enumerate() {
                entries += 1;
                if entries > MAX_DIRECTORY_ENTRIES {
                    self.damaged = true;
                    return Ok(());
                }
                let offset = base + index as u64 * 32;
                if !self.scan_entry(
                    entry,
                    offset,
                    pending.node,
                    pending.depth,
                    &mut lfn,
                    queue,
                    visited,
                )? {
                    return Ok(());
                }
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => return Ok(()),
            }
        }
    }

    /// Handles one directory entry. Returns `false` at the end of the directory.
    #[allow(clippy::too_many_arguments)]
    fn scan_entry(
        &mut self,
        entry: &[u8],
        offset: u64,
        parent: u32,
        depth: usize,
        lfn: &mut LfnState,
        queue: &mut Vec<Pending>,
        visited: &ClusterSet,
    ) -> Result<bool, FsError> {
        if entry[0] == 0 {
            return Ok(false);
        }
        if entry[0] == 0xe5 {
            lfn.reset();
            return Ok(true);
        }
        let attributes = entry[11];
        if attributes & 0x3f == 0x0f {
            lfn.push(entry, offset);
            return Ok(true);
        }
        if attributes & 0x08 != 0 && attributes & 0x10 == 0 {
            lfn.reset();
            return Ok(true);
        }
        let short = short_name(entry);
        let found = lfn.finish(entry, &short, offset);
        lfn.reset();
        let Some((name, location)) = found else {
            return Ok(true);
        };
        if name == "." || name == ".." {
            return Ok(true);
        }

        let kind = if attributes & 0x10 != 0 {
            EntryKind::Directory
        } else {
            EntryKind::File
        };
        let start = u16::from_le_bytes([entry[26], entry[27]]) as u32;
        let size = u32::from_le_bytes([entry[28], entry[29], entry[30], entry[31]]) as u64;
        let in_range = start >= 2 && start < self.cluster_count + 2;
        let usable = match kind {
            EntryKind::Directory => in_range,
            EntryKind::File => {
                size == 0
                    || (in_range && size <= self.cluster_count as u64 * self.cluster_size as u64)
            }
        };
        if !usable {
            self.damaged = true;
            return Ok(true);
        }
        let node = self.nodes.add(
            parent,
            name,
            kind,
            start,
            if kind == EntryKind::File { size } else { 0 },
            size,
            false,
            Some(location),
        )?;
        if let Some(added) = self.nodes.get_mut(NodeId(node)) {
            added.attributes = attributes & attr::SETTABLE;
            added.times = entry_times(entry);
        }
        if kind == EntryKind::Directory {
            if depth + 1 >= MAX_PATH_DEPTH || visited.contains(start) {
                self.damaged = true;
            } else {
                try_push(
                    queue,
                    Pending {
                        node,
                        cluster: start,
                        depth: depth + 1,
                    },
                )?;
            }
        }
        Ok(true)
    }

    fn cluster_offset(&self, cluster: u32) -> Result<u64, FsError> {
        if cluster < 2 || cluster >= self.cluster_count + 2 {
            return Err(FsError::Corrupt);
        }
        Ok(self.data_start + (cluster as u64 - 2) * self.cluster_size as u64)
    }

    fn next_cluster(&self, cluster: u32) -> Result<Option<u32>, FsError> {
        let (value, end, bad) = if self.wide {
            let value = self.volume.read_u16_le(self.fat_start + cluster as u64 * 2)? as u32;
            (value, 0xfff8, 0xfff7)
        } else {
            let at = self.fat_start + cluster as u64 + cluster as u64 / 2;
            let pair = self.volume.read_u16_le(at)? as u32;
            let value = if cluster & 1 != 0 { pair >> 4 } else { pair & 0x0fff };
            (value, 0xff8, 0xff7)
        };
        if value == 0 || value >= end {
            Ok(None)
        } else if value == bad || value < 2 || value >= self.cluster_count + 2 {
            Err(FsError::Corrupt)
        } else {
            Ok(Some(value))
        }
    }

    fn read_file(&self, node: NodeId, offset: usize, out: &mut [u8]) -> Result<usize, FsError> {
        let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
        if entry.kind != EntryKind::File {
            return Err(FsError::NotFile);
        }
        let offset = offset as u64;
        if out.is_empty() || offset >= entry.size {
            return Ok(0);
        }
        let cluster_size = self.cluster_size as u64;
        entry.check_chain(entry.size.div_ceil(cluster_size), |cluster| {
            self.next_cluster(cluster)
        })?;
        let wanted = (out.len() as u64).min(entry.size - offset) as usize;
        let target = (offset / cluster_size) as u32;
        let mut within = (offset % cluster_size) as usize;
        let (mut index, mut cluster) = entry.position_before(target);
        while index < target {
            cluster = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
            index += 1;
        }
        let mut done = 0usize;
        while done < wanted {
            let base = self.cluster_offset(cluster)?;
            let take = (self.cluster_size - within).min(wanted - done);
            self.volume
                .read_bytes(base + within as u64, &mut out[done..done + take])?;
            done += take;
            within = 0;
            if done < wanted {
                cluster = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
                index += 1;
            }
        }
        entry.remember_position(index, cluster);
        Ok(done)
    }
}

impl<D: BlockDevice> FileSystem for Fat16<D> {
    fn node_count(&self) -> usize {
        self.nodes.len()
    }

    fn node_id_limit(&self) -> usize {
        self.nodes.id_limit()
    }

    fn is_damaged(&self) -> bool {
        self.damaged
    }

    fn kind(&self, node: NodeId) -> Option<EntryKind> {
        self.nodes.kind(node)
    }

    fn name(&self, node: NodeId) -> Option<&str> {
        self.nodes.name(node)
    }

    fn parent(&self, node: NodeId) -> Option<NodeId> {
        self.nodes.parent(node)
    }

    fn size(&self, node: NodeId) -> usize {
        self.nodes.size(node)
    }

    fn read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> usize {
        self.read_file(node, offset, out).unwrap_or(0)
    }

    fn try_read(&self, node: NodeId, offset: usize, out: &mut [u8]) -> Result<usize, FsError> {
        self.read_file(node, offset, out)
    }

    fn child_count(&self, node: NodeId) -> usize {
        self.nodes.child_count(node)
    }

    fn child_at(&self, node: NodeId, index: usize) -> Option<NodeId> {
        self.nodes.child_at(node, index)
    }

    fn lookup(&self, dir: NodeId, name: &str) -> Option<NodeId> {
        self.nodes.lookup(dir, name)
    }

    fn attributes(&self, node: NodeId) -> u8 {
        self.nodes.get(node).map_or(0, |entry| entry.attributes)
    }

    fn times(&self, node: NodeId) -> NodeTimes {
        self.nodes
            .get(node)
            .map_or(NodeTimes::NONE, |entry| entry.times)
    }

    fn stats(&self) -> VolumeStats {
        VolumeStats {
            block_size: self.cluster_size as u64,
            total_blocks: self.cluster_count as u64,
            free_blocks: 0,
            read_only: true,
            kind: if self.wide { "fat16" } else { "fat12" },
        }
    }
}
