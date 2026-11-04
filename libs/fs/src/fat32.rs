//! FAT32, read and write.
//!
//! The driver understands the FAT32 BPB, long file names (LFN), the FAT
//! cluster chain, and arbitrary directory trees. It intentionally rejects
//! FAT12/FAT16 volumes: those use a fixed root-directory area and a different
//! cluster-count threshold, and this crate targets the format that removable
//! media uses today.
//!
//! Metadata is untrusted. A directory that loops, is cross-linked, runs past the
//! specification's size limit or nests too deeply is skipped, and the volume is
//! marked [damaged](FileSystem::is_damaged): what's intact stays readable, and
//! writes are refused.
//!
//! Layout references: Microsoft's FAT specification, sections "Boot Sector and
//! BPB" and "FAT 32", and the Linux `fs/fat` implementation for the LFN
//! checksum and entry ordering rules.

use alloc::string::String;
use alloc::vec::Vec;

use crate::block::{BlockDevice, WritableBlockDevice};
use crate::tree::{sanitize_name, try_push, try_zeroed, ClusterSet, EntryLocation, NodeTable};
use crate::volume::Volume;
use crate::{EntryKind, FileSystem, FsError, NodeId, WritableFileSystem, MAX_PATH_DEPTH};

const FAT_MASK: u32 = 0x0fff_ffff;
const FAT_EOC_MIN: u32 = 0x0fff_fff8;
const FAT_BAD: u32 = 0x0fff_fff7;
const FAT_MIN_CLUSTERS: u64 = 65_525;
const MAX_PATH_NAME_UNITS: usize = 260;
const MAX_FILE_SIZE: u64 = u32::MAX as u64;
/// The specification's limit on a directory: 2 MiB of 32-byte entries.
const MAX_DIRECTORY_ENTRIES: usize = 65_536;

/// A mounted FAT32 volume.
pub struct Fat32<D: BlockDevice> {
    volume: Volume<D>,
    nodes: NodeTable,
    fat_start: u64,
    fat_size_bytes: u64,
    fat_count: u32,
    /// The only FAT in use when `BPB_ExtFlags` turns mirroring off.
    active_fat: Option<u32>,
    data_start: u64,
    cluster_size: usize,
    cluster_count: u32,
    fsinfo: [Option<u64>; 2],
    free_count: Option<u32>,
    next_free: u32,
    damaged: bool,
}

impl<D: BlockDevice> Fat32<D> {
    /// Mounts a FAT32 image from `device`.
    ///
    /// Returns [`FsError::UnsupportedVolume`] for FAT12/FAT16 volumes and
    /// [`FsError::Corrupt`] for malformed metadata in the boot sector or root.
    /// Damage deeper in the tree is skipped; see [`FileSystem::is_damaged`].
    pub fn mount(device: D) -> Result<Self, FsError> {
        let volume = Volume::cached(device)?;
        let boot = volume.read_array::<512>(0)?;

        if boot[510] != 0x55 || boot[511] != 0xaa {
            return Err(FsError::InvalidBootSector);
        }

        let bytes_per_sector = u16::from_le_bytes([boot[11], boot[12]]) as usize;
        if !(512..=4096).contains(&bytes_per_sector) || !bytes_per_sector.is_power_of_two() {
            return Err(FsError::InvalidBootSector);
        }

        let sectors_per_cluster = boot[13] as usize;
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
            return Err(FsError::InvalidBootSector);
        }
        let cluster_size = bytes_per_sector
            .checked_mul(sectors_per_cluster)
            .ok_or(FsError::InvalidBootSector)?;
        if cluster_size > 32 * 1024 * 1024 {
            return Err(FsError::InvalidBootSector);
        }

        let reserved_sectors = u16::from_le_bytes([boot[14], boot[15]]) as u64;
        if reserved_sectors == 0 {
            return Err(FsError::InvalidBootSector);
        }
        let fat_count = boot[16] as u64;
        if fat_count == 0 {
            return Err(FsError::InvalidBootSector);
        }

        let total_sectors = u32::from_le_bytes([boot[32], boot[33], boot[34], boot[35]]) as u64;
        let total_sectors = if total_sectors == 0 {
            u16::from_le_bytes([boot[19], boot[20]]) as u64
        } else {
            total_sectors
        };
        if total_sectors == 0 {
            return Err(FsError::InvalidBootSector);
        }

        let fat_size = u32::from_le_bytes([boot[36], boot[37], boot[38], boot[39]]) as u64;
        if fat_size == 0 {
            // A zero FAT32 size field means this is FAT12/FAT16.
            return Err(FsError::UnsupportedVolume);
        }

        let root_entries = u16::from_le_bytes([boot[17], boot[18]]) as u64;
        if root_entries != 0 {
            return Err(FsError::UnsupportedVolume);
        }

        // Bit 7 clear: every FAT is a mirror. Set: only the numbered one is live.
        let ext_flags = u16::from_le_bytes([boot[40], boot[41]]);
        let active_fat = if ext_flags & 0x80 != 0 {
            let active = (ext_flags & 0x0f) as u64;
            if active >= fat_count {
                return Err(FsError::InvalidBootSector);
            }
            Some(active as u32)
        } else {
            None
        };

        let root_dir_sectors = 0u64;
        let metadata_sectors = reserved_sectors
            .checked_add(fat_count.checked_mul(fat_size).ok_or(FsError::Corrupt)?)
            .and_then(|value| value.checked_add(root_dir_sectors))
            .ok_or(FsError::Corrupt)?;
        if metadata_sectors >= total_sectors {
            return Err(FsError::Corrupt);
        }

        let data_sectors = total_sectors - metadata_sectors;
        let cluster_count = data_sectors / sectors_per_cluster as u64;
        if cluster_count < FAT_MIN_CLUSTERS {
            return Err(FsError::UnsupportedVolume);
        }
        if cluster_count > FAT_MASK as u64 {
            return Err(FsError::Corrupt);
        }

        let fat_bytes = fat_size
            .checked_mul(bytes_per_sector as u64)
            .ok_or(FsError::Corrupt)?;
        if fat_bytes < (cluster_count + 2) * 4 {
            return Err(FsError::Corrupt);
        }

        let total_bytes = total_sectors
            .checked_mul(bytes_per_sector as u64)
            .ok_or(FsError::Corrupt)?;
        if total_bytes > volume.len() {
            return Err(FsError::Corrupt);
        }

        let root_cluster = u32::from_le_bytes([boot[44], boot[45], boot[46], boot[47]]);
        let max_cluster = cluster_count as u32 + 2;
        if root_cluster < 2 || root_cluster >= max_cluster {
            return Err(FsError::Corrupt);
        }

        let fat_start = reserved_sectors
            .checked_mul(bytes_per_sector as u64)
            .ok_or(FsError::Corrupt)?;
        let data_start = metadata_sectors
            .checked_mul(bytes_per_sector as u64)
            .ok_or(FsError::Corrupt)?;

        let mut fsinfo = [None, None];
        let primary = u16::from_le_bytes([boot[48], boot[49]]) as u64;
        if primary != 0 && primary != 0xffff {
            let offset = primary
                .checked_mul(bytes_per_sector as u64)
                .ok_or(FsError::Corrupt)?;
            if valid_fsinfo(&volume, offset) {
                fsinfo[0] = Some(offset);
            }
        }
        let backup = u16::from_le_bytes([boot[50], boot[51]]) as u64;
        if backup != 0 && backup != 0xffff {
            let offset = backup
                .checked_add(1)
                .and_then(|sector| sector.checked_mul(bytes_per_sector as u64))
                .ok_or(FsError::Corrupt)?;
            if valid_fsinfo(&volume, offset) {
                fsinfo[1] = Some(offset);
            }
        }

        let mut free_count = None;
        let mut next_free = 2u32;
        if let Some(offset) = fsinfo[0] {
            let free = volume.read_u32_le(offset + 488)?;
            if free != u32::MAX && free <= cluster_count as u32 {
                free_count = Some(free);
            }
            let next = volume.read_u32_le(offset + 492)?;
            if next >= 2 && next < max_cluster {
                next_free = next;
            }
        }

        let mut fs = Self {
            volume,
            nodes: NodeTable::new(root_cluster),
            fat_start,
            fat_size_bytes: fat_bytes,
            fat_count: fat_count as u32,
            active_fat,
            data_start,
            cluster_size,
            cluster_count: cluster_count as u32,
            fsinfo,
            free_count,
            next_free,
            damaged: false,
        };

        let mut queue: Vec<DirTask> = Vec::new();
        try_push(
            &mut queue,
            DirTask {
                node: 0,
                cluster: root_cluster,
                depth: 0,
            },
        )?;
        let mut visited = ClusterSet::new(fs.cluster_count)?;
        let mut buffer = try_zeroed(fs.cluster_size)?;

        while let Some(task) = queue.pop() {
            let is_root = task.node == 0;
            match fs.scan_directory(task, &mut queue, &mut visited, &mut buffer) {
                Ok(()) => {}
                // Running out of memory says nothing about the volume.
                Err(error @ (FsError::TooManyNodes | FsError::OutOfMemory)) => return Err(error),
                Err(error) if is_root && fs.nodes.child_count(NodeId::ROOT) == 0 => {
                    return Err(error)
                }
                Err(_) => fs.damaged = true,
            }
        }

        Ok(fs)
    }

    /// Offset of `cluster`'s entry in the FAT that's being used.
    fn fat_entry_offset(&self, cluster: u32) -> Result<u64, FsError> {
        let live = self.active_fat.unwrap_or(0) as u64 * self.fat_size_bytes;
        self.fat_start
            .checked_add(live)
            .and_then(|start| start.checked_add(cluster as u64 * 4))
            .ok_or(FsError::Corrupt)
    }

    fn max_cluster(&self) -> u32 {
        self.cluster_count + 2
    }

    fn cluster_offset(&self, cluster: u32) -> Result<u64, FsError> {
        if cluster < 2 || cluster >= self.max_cluster() {
            return Err(FsError::Corrupt);
        }
        let index = (cluster as u64 - 2)
            .checked_mul(self.cluster_size as u64)
            .ok_or(FsError::Corrupt)?;
        self.data_start.checked_add(index).ok_or(FsError::Corrupt)
    }

    fn read_cluster(&self, cluster: u32, buffer: &mut [u8]) -> Result<(), FsError> {
        debug_assert_eq!(buffer.len(), self.cluster_size);
        let offset = self.cluster_offset(cluster)?;
        self.volume.read_bytes(offset, buffer)
    }

    fn next_cluster(&self, cluster: u32) -> Result<Option<u32>, FsError> {
        let offset = self.fat_entry_offset(cluster)?;
        let value = self.volume.read_u32_le(offset)? & FAT_MASK;
        if value >= FAT_EOC_MIN || value == 0 {
            Ok(None)
        } else if value == FAT_BAD || value < 2 || value >= self.max_cluster() {
            Err(FsError::Corrupt)
        } else {
            Ok(Some(value))
        }
    }

    fn scan_directory(
        &mut self,
        task: DirTask,
        queue: &mut Vec<DirTask>,
        visited: &mut ClusterSet,
        buffer: &mut [u8],
    ) -> Result<(), FsError> {
        let mut lfn = LfnState::new();
        let mut cluster = task.cluster;
        let mut entries = 0usize;

        loop {
            if !visited.insert(cluster) {
                // A loop in this chain, or a cluster shared with a directory
                // already walked. Either way, there's nothing new here.
                self.damaged = true;
                return Ok(());
            }
            self.read_cluster(cluster, buffer)?;
            let cluster_start = self.cluster_offset(cluster)?;

            for (index, entry) in buffer.chunks_exact(32).enumerate() {
                let offset = cluster_start + index as u64 * 32;
                if entry[0] == 0x00 {
                    return Ok(());
                }
                entries += 1;
                if entries > MAX_DIRECTORY_ENTRIES {
                    self.damaged = true;
                    return Ok(());
                }
                if entry[0] == 0xe5 {
                    lfn.reset();
                    continue;
                }

                let attributes = entry[11];
                if attributes & 0x3f == 0x0f {
                    lfn.push(entry, offset);
                    continue;
                }
                if attributes & 0x08 != 0 && attributes & 0x10 == 0 {
                    // Volume label, not a file system object.
                    lfn.reset();
                    continue;
                }

                let short = short_name(entry);
                let (name, location) = match lfn.finish(entry, &short, offset) {
                    Some(value) => value,
                    None => {
                        lfn.reset();
                        continue;
                    }
                };
                lfn.reset();

                if name == "." || name == ".." {
                    continue;
                }

                let kind = if attributes & 0x10 != 0 {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                };
                let high = u16::from_le_bytes([entry[20], entry[21]]) as u32;
                let low = u16::from_le_bytes([entry[26], entry[27]]) as u32;
                let start = (high << 16) | low;
                let size = u32::from_le_bytes([entry[28], entry[29], entry[30], entry[31]]) as u64;

                let usable = match kind {
                    EntryKind::Directory => start >= 2 && start < self.max_cluster(),
                    // Callers size buffers from this, so it has to be possible:
                    // no file can be larger than the volume.
                    EntryKind::File => {
                        size == 0
                            || (start >= 2
                                && start < self.max_cluster()
                                && size <= self.cluster_count as u64 * self.cluster_size as u64)
                    }
                };
                if !usable {
                    self.damaged = true;
                    continue;
                }

                let node = self.nodes.add(
                    task.node,
                    name,
                    kind,
                    start,
                    if kind == EntryKind::File { size } else { 0 },
                    size,
                    false,
                    Some(location),
                )?;

                if kind == EntryKind::Directory {
                    let depth = task.depth + 1;
                    if depth >= MAX_PATH_DEPTH || visited.contains(start) {
                        self.damaged = true;
                    } else {
                        try_push(
                            queue,
                            DirTask {
                                node,
                                cluster: start,
                                depth,
                            },
                        )?;
                    }
                }
            }

            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => return Ok(()),
            }
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
            let cluster_offset = self.cluster_offset(cluster)?;
            let take = (self.cluster_size - within).min(wanted - done);
            self.volume
                .read_bytes(cluster_offset + within as u64, &mut out[done..done + take])?;
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

impl<D: WritableBlockDevice> Fat32<D> {
    /// Fails when mounting found damage. Writing to a tree that's only partly
    /// understood could reuse clusters that hold skipped files.
    fn ensure_writable(&self) -> Result<(), FsError> {
        if self.damaged {
            return Err(FsError::Corrupt);
        }
        Ok(())
    }

    fn read_fat_entry(&self, cluster: u32) -> Result<u32, FsError> {
        let offset = self.fat_entry_offset(cluster)?;
        Ok(self.volume.read_u32_le(offset)? & FAT_MASK)
    }

    fn write_fat_entry(&mut self, cluster: u32, value: u32) -> Result<(), FsError> {
        let offset = self
            .fat_start
            .checked_add(cluster as u64 * 4)
            .ok_or(FsError::Corrupt)?;
        for copy in 0..self.fat_count {
            if self.active_fat.is_some_and(|active| active != copy) {
                continue;
            }
            let target = offset + copy as u64 * self.fat_size_bytes;
            // The top four bits of an entry are reserved and must be preserved.
            let reserved = self.volume.read_u32_le(target)? & !FAT_MASK;
            self.volume
                .write_u32_le(target, reserved | (value & FAT_MASK))?;
        }
        Ok(())
    }

    fn update_fsinfo(&mut self) -> Result<(), FsError> {
        let free = self.free_count;
        for offset in self.fsinfo.iter().copied().flatten() {
            if let Some(free) = free {
                self.volume.write_u32_le(offset + 488, free)?;
            }
            self.volume.write_u32_le(offset + 492, self.next_free)?;
        }
        Ok(())
    }

    fn alloc_cluster(&mut self) -> Result<u32, FsError> {
        let max_cluster = self.max_cluster();
        let start = self.next_free.clamp(2, max_cluster.saturating_sub(1));
        for step in 0..self.cluster_count {
            let cluster = 2 + ((start - 2 + step) % self.cluster_count);
            if self.read_fat_entry(cluster)? == 0 {
                self.write_fat_entry(cluster, FAT_EOC_MIN)?;
                let offset = self.cluster_offset(cluster)?;
                let zeros = try_zeroed(self.cluster_size)?;
                self.volume.write_bytes(offset, &zeros)?;
                self.next_free = if cluster + 1 < max_cluster {
                    cluster + 1
                } else {
                    2
                };
                if let Some(free) = self.free_count.as_mut() {
                    *free = free.saturating_sub(1);
                }
                self.update_fsinfo()?;
                return Ok(cluster);
            }
        }
        Err(FsError::NoSpace)
    }

    fn free_cluster(&mut self, cluster: u32) -> Result<(), FsError> {
        self.write_fat_entry(cluster, 0)?;
        if let Some(free) = self.free_count.as_mut() {
            *free = free.saturating_add(1).min(self.cluster_count);
        }
        if cluster >= 2 && cluster < self.next_free {
            self.next_free = cluster;
        }
        Ok(())
    }

    fn free_chain(&mut self, start: u32) -> Result<(), FsError> {
        if start < 2 {
            return Ok(());
        }
        let max_cluster = self.max_cluster();
        let mut cluster = start;
        let mut walked = 0usize;
        loop {
            if cluster < 2 || cluster >= max_cluster {
                return Err(FsError::Corrupt);
            }
            let next = self.read_fat_entry(cluster)?;
            self.free_cluster(cluster)?;
            walked += 1;
            if walked > self.cluster_count as usize {
                return Err(FsError::Corrupt);
            }
            if next == 0 || next >= FAT_EOC_MIN {
                break;
            }
            if next == FAT_BAD || next < 2 || next >= max_cluster {
                break;
            }
            cluster = next;
        }
        self.update_fsinfo()
    }

    fn chain_length(&self, start: u32) -> Result<usize, FsError> {
        if start < 2 {
            return Ok(0);
        }
        let mut cluster = start;
        let mut count = 0usize;
        loop {
            count += 1;
            if count > self.cluster_count as usize {
                return Err(FsError::Corrupt);
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => return Ok(count),
            }
        }
    }

    fn chain_last(&self, start: u32) -> Result<u32, FsError> {
        let mut cluster = start;
        let mut walked = 0usize;
        loop {
            walked += 1;
            if walked > self.cluster_count as usize {
                return Err(FsError::Corrupt);
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => return Ok(cluster),
            }
        }
    }

    fn ensure_chain(&mut self, start: u32, needed: usize) -> Result<u32, FsError> {
        if needed == 0 {
            return Ok(start);
        }
        if start < 2 {
            let first = self.alloc_cluster()?;
            if let Err(error) = self.extend_chain(first, needed - 1) {
                // Nothing refers to the partial chain yet, so it would leak.
                self.free_chain(first)?;
                return Err(error);
            }
            self.update_fsinfo()?;
            return Ok(first);
        }

        let length = self.chain_length(start)?;
        if length >= needed {
            return Ok(start);
        }
        let last = self.chain_last(start)?;
        if let Err(error) = self.extend_chain(last, needed - length) {
            // Give back what was added: the file's size still ends at `last`.
            if let Some(tail) = self.next_cluster(last)? {
                self.write_fat_entry(last, FAT_EOC_MIN)?;
                self.free_chain(tail)?;
            }
            return Err(error);
        }
        self.update_fsinfo()?;
        Ok(start)
    }

    /// Appends `count` clusters after `last`, linking each as it's allocated.
    fn extend_chain(&mut self, mut last: u32, count: usize) -> Result<(), FsError> {
        for _ in 0..count {
            let next = self.alloc_cluster()?;
            self.write_fat_entry(last, next)?;
            last = next;
        }
        Ok(())
    }

    fn truncate_chain(&mut self, start: u32, needed: usize) -> Result<u32, FsError> {
        if start < 2 {
            return Ok(0);
        }
        if needed == 0 {
            self.free_chain(start)?;
            return Ok(0);
        }
        let length = self.chain_length(start)?;
        if length <= needed {
            return Ok(start);
        }

        let mut cluster = start;
        for _ in 1..needed {
            cluster = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
        }
        let tail = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
        self.write_fat_entry(cluster, FAT_EOC_MIN)?;
        self.free_chain(tail)?;
        self.update_fsinfo()?;
        Ok(start)
    }

    fn write_stream_at(&mut self, start: u32, offset: u64, data: &[u8]) -> Result<(), FsError> {
        if data.is_empty() {
            return Ok(());
        }
        let cluster_size = self.cluster_size as u64;
        let mut cluster = start;
        for _ in 0..(offset / cluster_size) {
            cluster = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
        }
        let mut within = (offset % cluster_size) as usize;
        let mut done = 0usize;
        while done < data.len() {
            let take = (self.cluster_size - within).min(data.len() - done);
            let offset = self.cluster_offset(cluster)?;
            self.volume
                .write_bytes(offset + within as u64, &data[done..done + take])?;
            done += take;
            within = 0;
            if done < data.len() {
                cluster = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
            }
        }
        Ok(())
    }

    fn zero_stream_range(&mut self, start: u32, from: u64, to: u64) -> Result<(), FsError> {
        if from >= to {
            return Ok(());
        }
        let cluster_size = self.cluster_size as u64;
        let mut cluster = start;
        for _ in 0..(from / cluster_size) {
            cluster = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
        }
        let mut within = (from % cluster_size) as usize;
        let mut position = from;
        let zeros = [0u8; 4096];
        while position < to {
            let take = (self.cluster_size - within)
                .min((to - position) as usize)
                .min(zeros.len());
            let offset = self.cluster_offset(cluster)?;
            self.volume
                .write_bytes(offset + within as u64, &zeros[..take])?;
            position += take as u64;
            within += take;
            if within == self.cluster_size && position < to {
                within = 0;
                cluster = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
            }
        }
        Ok(())
    }

    fn update_file_meta(&mut self, node: NodeId, start: u32, size: u64) -> Result<(), FsError> {
        let location = self
            .nodes
            .get(node)
            .and_then(|entry| entry.entry)
            .ok_or(FsError::Corrupt)?;
        self.volume
            .write_u16_le(location.primary + 20, (start >> 16) as u16)?;
        self.volume
            .write_u16_le(location.primary + 26, (start & 0xffff) as u16)?;
        self.volume
            .write_u32_le(location.primary + 28, size as u32)?;
        if let Some(entry) = self.nodes.get_mut(node) {
            entry.start_cluster = start;
            entry.size = size;
            entry.valid_size = size;
            entry.data_length = size;
            entry.chain_changed();
        }
        Ok(())
    }

    fn write_file(&mut self, node: NodeId, offset: usize, data: &[u8]) -> Result<usize, FsError> {
        let (old_size, start_cluster, kind) = {
            let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
            entry.chain_changed();
            (entry.size, entry.start_cluster, entry.kind)
        };
        if kind != EntryKind::File {
            return Err(FsError::NotFile);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let end = (offset as u64)
            .checked_add(data.len() as u64)
            .ok_or(FsError::NoSpace)?;
        if end > MAX_FILE_SIZE {
            return Err(FsError::NoSpace);
        }
        let needed = ((end + self.cluster_size as u64 - 1) / self.cluster_size as u64) as usize;
        let start = self.ensure_chain(start_cluster, needed)?;
        if end > old_size {
            self.zero_stream_range(start, old_size, end)?;
        }
        self.write_stream_at(start, offset as u64, data)?;
        let size = old_size.max(end);
        self.update_file_meta(node, start, size)?;
        Ok(data.len())
    }

    fn set_file_len(&mut self, node: NodeId, size: usize) -> Result<(), FsError> {
        let (old_size, start_cluster, kind) = {
            let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
            entry.chain_changed();
            (entry.size, entry.start_cluster, entry.kind)
        };
        if kind != EntryKind::File {
            return Err(FsError::NotFile);
        }
        let new_size = size as u64;
        if new_size > MAX_FILE_SIZE {
            return Err(FsError::NoSpace);
        }
        if new_size == old_size {
            return Ok(());
        }
        if new_size == 0 {
            self.free_chain(start_cluster)?;
            self.update_file_meta(node, 0, 0)?;
            return Ok(());
        }
        let needed =
            ((new_size + self.cluster_size as u64 - 1) / self.cluster_size as u64) as usize;
        if new_size < old_size {
            let start = self.truncate_chain(start_cluster, needed)?;
            self.update_file_meta(node, start, new_size)?;
        } else {
            let start = self.ensure_chain(start_cluster, needed)?;
            self.zero_stream_range(start, old_size, new_size)?;
            self.update_file_meta(node, start, new_size)?;
        }
        Ok(())
    }

    /// Checks everything about a new name except whether it's taken.
    fn validate_name_syntax(&self, parent: NodeId, name: &str) -> Result<(), FsError> {
        let directory = self.nodes.get(parent).ok_or(FsError::NotFound)?;
        if directory.kind != EntryKind::Directory {
            return Err(FsError::NotDirectory);
        }
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.starts_with(' ')
            || name.ends_with('.')
            || name.ends_with(' ')
            || name.chars().any(|character| {
                character.is_control()
                    || matches!(
                        character,
                        '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                    )
            })
            || name.encode_utf16().count() > 255
        {
            return Err(FsError::InvalidName);
        }
        Ok(())
    }

    /// `levels` is how many levels the new node may need below `parent`.
    fn validate_new_name(&self, parent: NodeId, name: &str, levels: usize) -> Result<(), FsError> {
        self.validate_name_syntax(parent, name)?;
        if self.nodes.lookup(parent, name).is_some() {
            return Err(FsError::NameExists);
        }
        self.nodes.ensure_room(name)?;
        self.nodes.check_depth(parent, levels)
    }

    fn make_short_name(&self, parent: NodeId, name: &str) -> Result<[u8; 11], FsError> {
        let (base, extension) = split_short_name(name);
        let base_clean = clean_short_part(&base, 8);
        let extension_clean = clean_short_part(&extension, 3);
        let base_upper: String = base
            .chars()
            .map(|character| character.to_ascii_uppercase())
            .collect();
        let extension_upper: String = extension
            .chars()
            .map(|character| character.to_ascii_uppercase())
            .collect();

        if !base_clean.is_empty()
            && base_clean == base_upper
            && extension_clean == extension_upper
            && base_clean.len() <= 8
            && extension_clean.len() <= 3
        {
            let candidate = short_bytes(&base_clean, &extension_clean);
            if !self.short_exists(parent, &candidate)? {
                return Ok(candidate);
            }
        }

        let base = if base_clean.is_empty() {
            String::from("FILE")
        } else {
            base_clean
        };
        for number in 1..=99_999u32 {
            let suffix = alloc::format!("~{number}");
            if suffix.len() >= 8 {
                break;
            }
            let keep = 8 - suffix.len();
            let mut alias = String::new();
            for character in base.chars().take(keep) {
                alias.push(character);
            }
            alias.push_str(&suffix);
            let candidate = short_bytes(&alias, &extension_clean);
            if !self.short_exists(parent, &candidate)? {
                return Ok(candidate);
            }
        }
        Err(FsError::NameExists)
    }

    fn short_exists(&self, parent: NodeId, candidate: &[u8; 11]) -> Result<bool, FsError> {
        let start = self
            .nodes
            .get(parent)
            .ok_or(FsError::NotFound)?
            .start_cluster;
        if start < 2 {
            return Ok(false);
        }
        let mut buffer = try_zeroed(self.cluster_size)?;
        let mut cluster = start;
        let mut walked = 0usize;
        loop {
            self.read_cluster(cluster, &mut buffer)?;
            for entry in buffer.chunks_exact(32) {
                if entry[0] == 0x00 {
                    return Ok(false);
                }
                if entry[0] == 0xe5 || entry[11] & 0x3f == 0x0f {
                    continue;
                }
                if &entry[..11] == candidate {
                    return Ok(true);
                }
            }
            walked += 1;
            if walked > self.cluster_count as usize {
                return Err(FsError::Corrupt);
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => return Ok(false),
            }
        }
    }

    fn find_free_entries(&mut self, parent: NodeId, needed: usize) -> Result<Vec<u64>, FsError> {
        let start = self
            .nodes
            .get(parent)
            .ok_or(FsError::NotFound)?
            .start_cluster;
        if start < 2 {
            return Err(FsError::Corrupt);
        }

        let mut buffer = try_zeroed(self.cluster_size)?;
        let mut cluster = start;
        let mut offsets: Vec<u64> = Vec::with_capacity(needed);
        let mut walked = 0usize;

        loop {
            self.read_cluster(cluster, &mut buffer)?;
            let base = self.cluster_offset(cluster)?;
            for (index, entry) in buffer.chunks_exact(32).enumerate() {
                if entry[0] == 0x00 || entry[0] == 0xe5 {
                    offsets.push(base + index as u64 * 32);
                    if offsets.len() == needed {
                        return Ok(offsets);
                    }
                } else {
                    offsets.clear();
                }
            }

            walked += 1;
            if walked > self.cluster_count as usize {
                return Err(FsError::NoSpace);
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => {
                    let next = self.alloc_cluster()?;
                    self.write_fat_entry(cluster, next)?;
                    self.update_fsinfo()?;
                    cluster = next;
                }
            }
        }
    }

    fn write_entry_set(
        &mut self,
        parent: NodeId,
        name: &str,
        attributes: u8,
        start_cluster: u32,
        size: u64,
        template: Option<[u8; 32]>,
    ) -> Result<EntryLocation, FsError> {
        let short = self.make_short_name(parent, name)?;
        let units: Vec<u16> = name.encode_utf16().collect();
        if units.is_empty() || units.len() > 255 {
            return Err(FsError::InvalidName);
        }
        let lfn_count = (units.len() + 1 + 12) / 13;
        if lfn_count > 20 {
            return Err(FsError::InvalidName);
        }
        let needed = lfn_count + 1;
        let offsets = self.find_free_entries(parent, needed)?;

        let checksum = short_checksum_bytes(&short);
        for (offset_index, sequence) in (1..=lfn_count).rev().enumerate() {
            let entry = build_lfn_entry(
                sequence as u8,
                lfn_count,
                &units,
                checksum,
                offset_index == 0,
            );
            self.volume.write_bytes(offsets[offset_index], &entry)?;
        }

        let short_offset = offsets[lfn_count];
        let mut entry = build_short_entry(&short, attributes, start_cluster, size);
        if let Some(template) = template {
            // A rename changes the name and nothing else: attributes and
            // timestamps survive. The case flags described the old short name.
            entry[11..].copy_from_slice(&template[11..]);
            entry[12] = 0;
        }
        self.volume.write_bytes(short_offset, &entry)?;

        Ok(EntryLocation {
            first: offsets[0],
            primary: short_offset,
            count: (lfn_count + 1) as u8,
            stream: 0,
        })
    }

    fn delete_entry_set(&mut self, parent: NodeId, location: EntryLocation) -> Result<(), FsError> {
        let start = self
            .nodes
            .get(parent)
            .ok_or(FsError::NotFound)?
            .start_cluster;
        let mut cluster = start;
        let mut walked = 0usize;
        loop {
            let base = self.cluster_offset(cluster)?;
            if location.first >= base && location.first < base + self.cluster_size as u64 {
                let mut offset = location.first;
                for remaining in (0..location.count).rev() {
                    self.volume.write_bytes(offset, &[0xe5])?;
                    offset += 32;
                    let base = self.cluster_offset(cluster)?;
                    // A set can end in the last slot of the last cluster, with
                    // nothing to advance to.
                    if remaining > 0 && offset >= base + self.cluster_size as u64 {
                        cluster = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
                        offset = self.cluster_offset(cluster)?;
                    }
                }
                return Ok(());
            }
            walked += 1;
            if walked > self.cluster_count as usize {
                return Err(FsError::Corrupt);
            }
            cluster = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
        }
    }

    fn create_file_node(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        self.validate_new_name(parent, name, 0)?;
        let location = self.write_entry_set(parent, name, 0x20, 0, 0, None)?;
        self.nodes
            .add(
                parent.0,
                String::from(name),
                EntryKind::File,
                0,
                0,
                0,
                false,
                Some(location),
            )
            .map(NodeId)
    }

    fn create_dir_node(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        // A directory must leave room for its own children: one at the limit
        // could not be walked on the next mount.
        self.validate_new_name(parent, name, 1)?;
        let cluster = self.alloc_cluster()?;
        let parent_cluster = if parent == NodeId::ROOT {
            0
        } else {
            self.nodes
                .get(parent)
                .ok_or(FsError::NotFound)?
                .start_cluster
        };

        let base = self.cluster_offset(cluster)?;
        let dot = build_short_entry(&dot_name(true), 0x10, cluster, 0);
        let dotdot = build_short_entry(&dot_name(false), 0x10, parent_cluster, 0);
        self.volume.write_bytes(base, &dot)?;
        self.volume.write_bytes(base + 32, &dotdot)?;

        let location = match self.write_entry_set(parent, name, 0x10, cluster, 0, None) {
            Ok(location) => location,
            Err(error) => {
                self.free_chain(cluster)?;
                return Err(error);
            }
        };

        self.nodes
            .add(
                parent.0,
                String::from(name),
                EntryKind::Directory,
                cluster,
                0,
                self.cluster_size as u64,
                false,
                Some(location),
            )
            .map(NodeId)
    }

    fn update_dotdot(&mut self, node: NodeId, parent_cluster: u32) -> Result<(), FsError> {
        let start = self.nodes.get(node).ok_or(FsError::NotFound)?.start_cluster;
        if start < 2 {
            return Ok(());
        }
        let base = self.cluster_offset(start)?;
        let mut buffer = try_zeroed(self.cluster_size)?;
        self.read_cluster(start, &mut buffer)?;
        for (index, entry) in buffer.chunks_exact(32).enumerate() {
            if entry[0] == b'.' && entry[1] == b'.' {
                let offset = base + index as u64 * 32;
                self.volume
                    .write_u16_le(offset + 20, (parent_cluster >> 16) as u16)?;
                self.volume
                    .write_u16_le(offset + 26, (parent_cluster & 0xffff) as u16)?;
                return Ok(());
            }
        }
        Ok(())
    }

    fn remove_node(&mut self, node: NodeId) -> Result<(), FsError> {
        if node == NodeId::ROOT {
            return Err(FsError::Unsupported);
        }
        let (kind, start_cluster, location, parent) = {
            let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
            (
                entry.kind,
                entry.start_cluster,
                entry.entry,
                entry.parent.ok_or(FsError::Unsupported)?,
            )
        };
        if kind == EntryKind::Directory && self.nodes.child_count(node) > 0 {
            return Err(FsError::NotEmpty);
        }
        // Entry first: if this stops partway, the worst left behind is a lost
        // chain, not a name that points at freed clusters.
        self.delete_entry_set(NodeId(parent), location.ok_or(FsError::Corrupt)?)?;
        self.nodes.remove(node)?;
        if start_cluster >= 2 {
            self.free_chain(start_cluster)?;
        }
        self.update_fsinfo()
    }

    fn rename_node(
        &mut self,
        node: NodeId,
        new_parent: NodeId,
        new_name: &str,
    ) -> Result<(), FsError> {
        if node == NodeId::ROOT {
            return Err(FsError::Unsupported);
        }
        let (kind, start_cluster, size, old_parent) = {
            let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
            (
                entry.kind,
                entry.start_cluster,
                entry.size,
                entry.parent.ok_or(FsError::Unsupported)?,
            )
        };
        if kind == EntryKind::Directory && self.nodes.is_ancestor(node, new_parent) {
            return Err(FsError::Unsupported);
        }
        match self.nodes.lookup(new_parent, new_name) {
            // The same name exactly. A case change still has to be written.
            Some(found) if found == node && self.nodes.name(node) == Some(new_name) => {
                return Ok(())
            }
            Some(found) if found == node => {}
            Some(_) => return Err(FsError::NameExists),
            None => {}
        }
        self.validate_name_syntax(new_parent, new_name)?;
        let directory_level = usize::from(kind == EntryKind::Directory);
        self.nodes
            .check_depth(new_parent, self.nodes.height(node) + directory_level)?;

        let old_location = self
            .nodes
            .get(node)
            .and_then(|entry| entry.entry)
            .ok_or(FsError::Corrupt)?;
        let attributes = if kind == EntryKind::Directory {
            0x10
        } else {
            0x20
        };
        let size_field = if kind == EntryKind::File { size } else { 0 };
        let template = self.volume.read_array::<32>(old_location.primary)?;
        let new_location = self.write_entry_set(
            new_parent,
            new_name,
            attributes,
            start_cluster,
            size_field,
            Some(template),
        )?;
        self.delete_entry_set(NodeId(old_parent), old_location)?;

        if kind == EntryKind::Directory {
            let parent_cluster = if new_parent == NodeId::ROOT {
                0
            } else {
                self.nodes
                    .get(new_parent)
                    .ok_or(FsError::NotFound)?
                    .start_cluster
            };
            self.update_dotdot(node, parent_cluster)?;
        }

        self.nodes
            .reparent(node, new_parent, String::from(new_name), new_location)
    }
}

impl<D: WritableBlockDevice> WritableFileSystem for Fat32<D> {
    fn create_file(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        self.ensure_writable()?;
        self.create_file_node(parent, name)
    }

    fn create_dir(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        self.ensure_writable()?;
        self.create_dir_node(parent, name)
    }

    fn remove(&mut self, node: NodeId) -> Result<(), FsError> {
        self.ensure_writable()?;
        self.remove_node(node)
    }

    fn rename(&mut self, node: NodeId, new_parent: NodeId, new_name: &str) -> Result<(), FsError> {
        self.ensure_writable()?;
        self.rename_node(node, new_parent, new_name)
    }

    fn write(&mut self, node: NodeId, offset: usize, data: &[u8]) -> Result<usize, FsError> {
        self.ensure_writable()?;
        self.write_file(node, offset, data)
    }

    fn set_len(&mut self, node: NodeId, size: usize) -> Result<(), FsError> {
        self.ensure_writable()?;
        self.set_file_len(node, size)
    }

    fn flush(&mut self) -> Result<(), FsError> {
        self.volume.flush()
    }
}

impl<D: BlockDevice> FileSystem for Fat32<D> {
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
}

struct DirTask {
    node: u32,
    cluster: u32,
    /// Parents between this directory and the root.
    depth: usize,
}

struct LfnState {
    units: [u16; MAX_PATH_NAME_UNITS],
    len: usize,
    checksum: Option<u8>,
    active: bool,
    first: Option<u64>,
    entries: u8,
    /// Sequence number the next entry must carry. Entries count down to one.
    expected: u8,
}

impl LfnState {
    fn new() -> Self {
        Self {
            units: [0xffff; MAX_PATH_NAME_UNITS],
            len: 0,
            checksum: None,
            active: false,
            first: None,
            entries: 0,
            expected: 0,
        }
    }

    fn reset(&mut self) {
        self.len = 0;
        self.checksum = None;
        self.active = false;
        self.first = None;
        self.entries = 0;
        self.expected = 0;
    }

    fn push(&mut self, entry: &[u8], offset: u64) {
        let order = (entry[0] & 0x1f) as usize;
        if order == 0 {
            self.active = false;
            return;
        }

        if entry[0] & 0x40 != 0 {
            // The first entry in on-disk order carries the highest sequence
            // number. Reset the buffer and size it from that number.
            self.units.fill(0xffff);
            self.len = order * 13;
            if self.len > self.units.len() {
                self.active = false;
                return;
            }
            self.checksum = Some(entry[13]);
            self.active = true;
            self.first = Some(offset);
            self.entries = 1;
            self.expected = order as u8 - 1;
        } else if !self.active
            || self.checksum != Some(entry[13])
            || order != self.expected as usize
        {
            self.active = false;
            return;
        } else {
            self.entries = self.entries.saturating_add(1);
            self.expected -= 1;
        }

        let offset = (order - 1) * 13;
        if offset + 13 > self.units.len() {
            self.active = false;
            return;
        }

        for index in 0..5 {
            self.units[offset + index] =
                u16::from_le_bytes([entry[1 + index * 2], entry[2 + index * 2]]);
        }
        for index in 0..6 {
            self.units[offset + 5 + index] =
                u16::from_le_bytes([entry[14 + index * 2], entry[15 + index * 2]]);
        }
        for index in 0..2 {
            self.units[offset + 11 + index] =
                u16::from_le_bytes([entry[28 + index * 2], entry[29 + index * 2]]);
        }
    }

    fn finish(
        &self,
        short_entry: &[u8],
        short: &Option<String>,
        short_offset: u64,
    ) -> Option<(String, EntryLocation)> {
        // An incomplete run of sequence numbers is an orphan, not this file's
        // name, even if the checksum matches.
        let valid_lfn =
            self.active && self.expected == 0 && self.checksum == Some(short_checksum(short_entry));
        let location = EntryLocation {
            first: if valid_lfn {
                self.first.unwrap_or(short_offset)
            } else {
                short_offset
            },
            primary: short_offset,
            count: if valid_lfn {
                self.entries.saturating_add(1)
            } else {
                1
            },
            stream: 0,
        };

        if valid_lfn {
            let mut end = self.len.min(self.units.len());
            while end > 0 && (self.units[end - 1] == 0 || self.units[end - 1] == 0xffff) {
                end -= 1;
            }
            if end > 0 {
                let name = String::from_utf16_lossy(&self.units[..end]);
                if !name.is_empty() {
                    return Some((sanitize_name(name), location));
                }
            }
        }

        short
            .as_ref()
            .map(|name| (sanitize_name(name.clone()), location))
            .filter(|(name, _)| !name.is_empty())
    }
}

fn short_checksum(entry: &[u8]) -> u8 {
    let mut sum = 0u8;
    for &byte in &entry[..11] {
        sum = sum.rotate_right(1).wrapping_add(byte);
    }
    sum
}

fn short_name(entry: &[u8]) -> Option<String> {
    let mut base = [0u8; 8];
    base.copy_from_slice(&entry[..8]);
    if base[0] == 0x05 {
        base[0] = 0xe5;
    }
    let mut extension = [0u8; 3];
    extension.copy_from_slice(&entry[8..11]);

    let base_end = trimmed_end(&base);
    if base_end == 0 {
        return None;
    }
    let extension_end = trimmed_end(&extension);
    let flags = entry[12];
    let lower_base = flags & 0x08 != 0;
    let lower_extension = flags & 0x10 != 0;

    let mut name = String::new();
    for &byte in &base[..base_end] {
        let character = char::from(byte);
        name.push(if lower_base {
            character.to_ascii_lowercase()
        } else {
            character
        });
    }
    if extension_end > 0 {
        name.push('.');
        for &byte in &extension[..extension_end] {
            let character = char::from(byte);
            name.push(if lower_extension {
                character.to_ascii_lowercase()
            } else {
                character
            });
        }
    }
    Some(name)
}

fn trimmed_end(bytes: &[u8]) -> usize {
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == 0) {
        end -= 1;
    }
    // A NUL terminates the field; anything after it is padding.
    if let Some(position) = bytes[..end].iter().position(|&byte| byte == 0) {
        end = position;
    }
    end
}

fn valid_fsinfo<D: BlockDevice>(volume: &Volume<D>, offset: u64) -> bool {
    matches!(
        (
            volume.read_u32_le(offset),
            volume.read_u32_le(offset + 484),
            volume.read_u32_le(offset + 508),
        ),
        (Ok(0x4161_5252), Ok(0x6141_7272), Ok(0xaa55_0000))
    )
}

fn short_checksum_bytes(name: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    for &byte in name {
        sum = sum.rotate_right(1).wrapping_add(byte);
    }
    sum
}

fn build_short_entry(name: &[u8; 11], attributes: u8, start: u32, size: u64) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[..11].copy_from_slice(name);
    entry[11] = attributes;
    entry[20..22].copy_from_slice(&((start >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&((start & 0xffff) as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&(size as u32).to_le_bytes());
    entry
}

fn build_lfn_entry(
    sequence: u8,
    total: usize,
    units: &[u16],
    checksum: u8,
    first: bool,
) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[0] = sequence;
    if first {
        entry[0] |= 0x40;
    }
    debug_assert!(total >= sequence as usize);
    entry[11] = 0x0f;
    entry[13] = checksum;

    for index in 0..13 {
        let unit_index = (sequence as usize - 1) * 13 + index;
        let value = if unit_index < units.len() {
            units[unit_index]
        } else if unit_index == units.len() {
            0
        } else {
            0xffff
        };
        let offset = match index {
            0..=4 => 1 + index * 2,
            5..=10 => 14 + (index - 5) * 2,
            _ => 28 + (index - 11) * 2,
        };
        entry[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    entry
}

fn split_short_name(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(position) if position > 0 && position + 1 < name.len() => (
            String::from(&name[..position]),
            String::from(&name[position + 1..]),
        ),
        _ => (String::from(name), String::new()),
    }
}

fn clean_short_part(source: &str, max: usize) -> String {
    let mut out = String::new();
    for character in source.chars() {
        let character = character.to_ascii_uppercase();
        let allowed = character.is_ascii_alphanumeric()
            || matches!(
                character,
                '$' | '%'
                    | '\''
                    | '-'
                    | '_'
                    | '@'
                    | '~'
                    | '`'
                    | '!'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '^'
                    | '#'
                    | '&'
            );
        out.push(if allowed { character } else { '_' });
        if out.len() >= max {
            break;
        }
    }
    out
}

fn short_bytes(base: &str, extension: &str) -> [u8; 11] {
    let mut name = [b' '; 11];
    for (index, byte) in base.bytes().take(8).enumerate() {
        name[index] = byte;
    }
    for (index, byte) in extension.bytes().take(3).enumerate() {
        name[8 + index] = byte;
    }
    name
}

fn dot_name(single: bool) -> [u8; 11] {
    let mut name = [b' '; 11];
    name[0] = b'.';
    if !single {
        name[1] = b'.';
    }
    name
}
