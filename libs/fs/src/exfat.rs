//! exFAT, read and write.
//!
//! The driver parses the volume boot record (including its checksum, falling
//! back to the backup region), the FAT and directory entry sets. File names are
//! UTF-16, and directory streams may be contiguous (`NoFatChain`) or
//! FAT-chained.
//!
//! Metadata is untrusted. Each entry set's `SetChecksum` and `NameHash` and the
//! up-case table's `TableChecksum` are verified. An entry set or directory that
//! fails, or that names impossible clusters, is skipped and the volume is marked
//! [damaged](FileSystem::is_damaged): what's intact stays readable, and writes
//! are refused.
//!
//! Bytes past a file's `ValidDataLength` have never been written, so reads
//! return zeros there rather than whatever the clusters last held.
//!
//! Layout references: Microsoft's exFAT specification, sections 3.1 (boot
//! region), 4.1 (FAT) and 6 (directory entries), plus the Linux `fs/exfat`
//! implementation for entry-set ordering.

use alloc::string::String;
use alloc::vec::Vec;

use crate::block::{BlockDevice, WritableBlockDevice, WriteClass};
use crate::fat32::{fat_time_to_unix, fat_timestamp};
use crate::tree::{sanitize_name, try_push, try_zeroed, ClusterSet, EntryLocation, NodeTable};
use crate::volume::Volume;
use crate::{
    attr, CheckReport, EntryKind, FileSystem, FsError, NodeId, NodeTimes, TimeSource, VolumeStats,
    WritableFileSystem, MAX_PATH_DEPTH,
};

const EXFAT_SIGNATURE: &[u8; 8] = b"EXFAT   ";
// exFAT FAT entries are a full 32 bits, unlike FAT32's 28.
const FAT_BAD: u32 = 0xffff_fff7;
const FAT_EOC_MIN: u32 = 0xffff_fff8;
const EXFAT_EOC: u32 = 0xffff_ffff;
/// The specification's maximum `ClusterCount`.
const MAX_CLUSTER_COUNT: u32 = 0xffff_fff5;
const MAX_NAME_UNITS: usize = 255;
/// The specification's limit on a directory's size.
const MAX_DIRECTORY_BYTES: u64 = 256 * 1024 * 1024;

/// `VolumeFlags` bit: the volume may be inconsistent.
const VOLUME_DIRTY: u16 = 0x0002;

const ENTRY_END: u8 = 0x00;
const ENTRY_BITMAP: u8 = 0x81;
const ENTRY_UPCASE: u8 = 0x82;
const ENTRY_FILE: u8 = 0x85;
const ENTRY_STREAM: u8 = 0xc0;
const ENTRY_NAME: u8 = 0xc1;

/// An up-case table covers 64 K units at two bytes each, and may not exceed that
/// even uncompressed.
const UPCASE_MAX_BYTES: u64 = 2 * 0x1_0000;

/// A mounted exFAT volume.
pub struct Exfat<D: BlockDevice> {
    volume: Volume<D>,
    nodes: NodeTable,
    fat_start: u64,
    fat_size_bytes: u64,
    fat_count: u32,
    /// The FAT that reads use: `ActiveFat` can select the second of two.
    active_fat_start: u64,
    heap_start: u64,
    cluster_size: usize,
    cluster_count: u32,
    root_cluster: u32,
    backup_base: Option<u64>,
    bitmap: Option<Bitmap>,
    upcase: Option<Vec<u16>>,
    next_free: u32,
    dirty: bool,
    damaged: bool,
    /// Directories that share a start with another, seen while the volume was
    /// dirty: the two names of an interrupted rename.
    suspects: Vec<u32>,
    /// `(start, length)` of the bitmap and up-case table, which no node owns.
    system_streams: Vec<(u32, u64)>,
    now: Option<TimeSource>,
}

impl<D: BlockDevice> Exfat<D> {
    /// Mounts an exFAT image from `device`.
    ///
    /// A damaged main boot region falls back to the backup. Damage deeper in the
    /// tree is skipped; see [`FileSystem::is_damaged`].
    pub fn mount(device: D) -> Result<Self, FsError> {
        let volume = Volume::cached(device)?;
        let selected = select_boot(&volume)?;
        let boot = selected.boot;
        let sector_size = 1usize << selected.sector_shift;
        let cluster_shift = boot[109] as u32;
        let cluster_size = sector_size << cluster_shift;

        let volume_length = u64::from_le_bytes([
            boot[72], boot[73], boot[74], boot[75], boot[76], boot[77], boot[78], boot[79],
        ]);
        if volume_length == 0 {
            return Err(FsError::InvalidBootSector);
        }
        let volume_bytes = volume_length
            .checked_mul(sector_size as u64)
            .ok_or(FsError::InvalidBootSector)?;
        if volume_bytes > volume.len() {
            return Err(FsError::Corrupt);
        }

        let fat_offset = u32::from_le_bytes([boot[80], boot[81], boot[82], boot[83]]) as u64;
        let fat_length = u32::from_le_bytes([boot[84], boot[85], boot[86], boot[87]]) as u64;
        let heap_offset = u32::from_le_bytes([boot[88], boot[89], boot[90], boot[91]]) as u64;
        let cluster_count = u32::from_le_bytes([boot[92], boot[93], boot[94], boot[95]]);
        let root_cluster = u32::from_le_bytes([boot[96], boot[97], boot[98], boot[99]]);
        let fat_count = boot[110] as u64;

        if fat_offset == 0 || fat_length == 0 || heap_offset == 0 || cluster_count == 0 {
            return Err(FsError::InvalidBootSector);
        }
        if !(1..=2).contains(&fat_count) {
            return Err(FsError::InvalidBootSector);
        }
        if cluster_count > MAX_CLUSTER_COUNT {
            return Err(FsError::Corrupt);
        }
        let fats_end = fat_length
            .checked_mul(fat_count)
            .and_then(|length| length.checked_add(fat_offset))
            .ok_or(FsError::Corrupt)?;
        if fats_end > heap_offset {
            return Err(FsError::Corrupt);
        }
        let heap_end = heap_offset
            .checked_add(cluster_count as u64 * (1u64 << cluster_shift))
            .ok_or(FsError::Corrupt)?;
        if heap_end > volume_length {
            return Err(FsError::Corrupt);
        }

        let fat_start = fat_offset
            .checked_mul(sector_size as u64)
            .ok_or(FsError::Corrupt)?;
        let fat_bytes = fat_length
            .checked_mul(sector_size as u64)
            .ok_or(FsError::Corrupt)?;
        if fat_bytes < (cluster_count as u64 + 2) * 4 {
            return Err(FsError::Corrupt);
        }
        let heap_start = heap_offset
            .checked_mul(sector_size as u64)
            .ok_or(FsError::Corrupt)?;
        let max_cluster = cluster_count + 2;
        if root_cluster < 2 || root_cluster >= max_cluster {
            return Err(FsError::Corrupt);
        }

        let volume_flags = u16::from_le_bytes([boot[106], boot[107]]);
        let active_fat_start = if fat_count == 2 && volume_flags & 1 != 0 {
            fat_start + fat_bytes
        } else {
            fat_start
        };

        let mut fs = Self {
            volume,
            nodes: NodeTable::new(root_cluster),
            fat_start,
            fat_size_bytes: fat_bytes,
            fat_count: fat_count as u32,
            active_fat_start,
            heap_start,
            cluster_size,
            cluster_count,
            root_cluster,
            backup_base: if selected.has_backup {
                Some(12 * sector_size as u64)
            } else {
                None
            },
            bitmap: None,
            upcase: None,
            next_free: 2,
            dirty: volume_flags & VOLUME_DIRTY != 0,
            damaged: selected.used_backup,
            suspects: Vec::new(),
            system_streams: Vec::new(),
            now: None,
        };

        let mut buffer = try_zeroed(fs.cluster_size)?;
        fs.load_system_entries(&mut buffer)?;

        let mut queue: Vec<DirTask> = Vec::new();
        try_push(
            &mut queue,
            DirTask {
                node: 0,
                cluster: root_cluster,
                data_length: 0,
                contiguous: false,
                depth: 0,
            },
        )?;
        let mut visited = ClusterSet::new(fs.cluster_count)?;

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
        self.heap_start.checked_add(index).ok_or(FsError::Corrupt)
    }

    fn read_cluster(&self, cluster: u32, buffer: &mut [u8]) -> Result<(), FsError> {
        debug_assert_eq!(buffer.len(), self.cluster_size);
        let offset = self.cluster_offset(cluster)?;
        self.volume.read_bytes(offset, buffer)
    }

    fn next_cluster(&self, cluster: u32) -> Result<Option<u32>, FsError> {
        let offset = self
            .active_fat_start
            .checked_add(cluster as u64 * 4)
            .ok_or(FsError::Corrupt)?;
        let value = self.volume.read_u32_le(offset)?;
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
        let cluster_size = self.cluster_size as u64;
        let contiguous_clusters = if task.contiguous && task.data_length > 0 {
            task.data_length
                .div_ceil(cluster_size)
                .min(self.cluster_count as u64) as usize
        } else {
            0
        };
        let max_clusters = MAX_DIRECTORY_BYTES.div_ceil(cluster_size);

        let mut pending: Option<FileSet> = None;
        let mut cluster = task.cluster;
        let mut index = 0usize;

        if visited.contains(cluster) && self.dirty && task.node != 0 {
            try_push(&mut self.suspects, task.node)?;
            return Ok(());
        }

        loop {
            if !visited.insert(cluster) || index as u64 >= max_clusters {
                // A loop in this chain, a cluster shared with a directory
                // already walked, or more directory than the format allows.
                self.damaged = true;
                return Ok(());
            }
            self.read_cluster(cluster, buffer)?;
            let cluster_start = self.cluster_offset(cluster)?;

            for (entry_index, entry) in buffer.chunks_exact(32).enumerate() {
                let offset = cluster_start + entry_index as u64 * 32;
                let entry_type = entry[0];
                if entry_type == ENTRY_END {
                    self.damaged |= pending.is_some();
                    return Ok(());
                }
                if entry_type == ENTRY_FILE {
                    self.damaged |= pending.is_some();
                    let set = FileSet::new(entry, offset);
                    if set.remaining == 0 {
                        self.damaged = true;
                    } else {
                        pending = Some(set);
                    }
                    continue;
                }
                if entry_type & 0xc0 != 0xc0 {
                    // Not a secondary entry, so an open set was cut short. This
                    // covers deleted entries and other primaries like the label.
                    self.damaged |= pending.is_some();
                    pending = None;
                    continue;
                }

                let Some(set) = pending.as_mut() else {
                    continue;
                };
                if set.push(entry, offset) {
                    if let Some(set) = pending.take() {
                        self.add_entry(set, &task, queue, visited)?;
                    }
                }
            }

            index += 1;

            if task.contiguous && contiguous_clusters > 0 {
                if index >= contiguous_clusters {
                    self.damaged |= pending.is_some();
                    return Ok(());
                }
                let Some(next) = task.cluster.checked_add(index as u32) else {
                    return Err(FsError::Corrupt);
                };
                cluster = next;
            } else {
                match self.next_cluster(cluster)? {
                    Some(next) => cluster = next,
                    None => {
                        self.damaged |= pending.is_some();
                        return Ok(());
                    }
                }
            }
        }
    }

    fn add_entry(
        &mut self,
        set: FileSet,
        parent: &DirTask,
        queue: &mut Vec<DirTask>,
        visited: &mut ClusterSet,
    ) -> Result<(), FsError> {
        let Some(info) = set.finish() else {
            self.damaged = true;
            return Ok(());
        };
        if self.name_hash(&info.name_units) != info.name_hash || !self.stream_is_plausible(&info) {
            self.damaged = true;
            return Ok(());
        }
        let name = String::from_utf16_lossy(&info.name_units);
        if name == "." || name == ".." {
            self.damaged = true;
            return Ok(());
        }
        // Stored raw above, because the hash covers what's on disk.
        let name = sanitize_name(name);

        let node = self.nodes.add(
            parent.node,
            name,
            info.kind,
            info.start_cluster,
            if info.kind == EntryKind::File {
                info.data_length
            } else {
                0
            },
            info.data_length,
            info.contiguous,
            Some(info.location),
        )?;
        if info.valid_length > info.data_length {
            self.damaged = true;
        }
        if let Some(entry) = self.nodes.get_mut(NodeId(node)) {
            entry.valid_size = info.valid_length.min(info.data_length);
            entry.attributes = info.attributes as u8 & attr::SETTABLE;
            entry.times = info.times;
        }

        if info.kind == EntryKind::Directory && info.data_length > 0 {
            let depth = parent.depth + 1;
            if visited.contains(info.start_cluster) && self.dirty {
                try_push(&mut self.suspects, node)?;
            } else if depth >= MAX_PATH_DEPTH || visited.contains(info.start_cluster) {
                self.damaged = true;
            } else {
                try_push(
                    queue,
                    DirTask {
                        node,
                        cluster: info.start_cluster,
                        data_length: info.data_length,
                        contiguous: info.contiguous,
                        depth,
                    },
                )?;
            }
        }
        Ok(())
    }

    /// Checks that a stream could exist on this volume. Callers size buffers from
    /// its length, so a claim of more data than the volume holds must not pass.
    fn stream_is_plausible(&self, info: &PendingEntry) -> bool {
        if info.data_length == 0 {
            return true;
        }
        let cluster_size = self.cluster_size as u64;
        if info.data_length > self.cluster_count as u64 * cluster_size {
            return false;
        }
        if info.start_cluster < 2 || info.start_cluster >= self.max_cluster() {
            return false;
        }
        let end = info.start_cluster as u64 + info.data_length.div_ceil(cluster_size);
        !info.contiguous || end <= self.max_cluster() as u64
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
        if !entry.contiguous {
            entry.check_chain(entry.data_length.div_ceil(cluster_size), |cluster| {
                self.next_cluster(cluster)
            })?;
        }

        let wanted = (out.len() as u64).min(entry.size - offset) as usize;
        // Never-written bytes read as zero, without touching the disk.
        let stored = wanted.min(entry.valid_size.saturating_sub(offset) as usize);
        out[stored..wanted].fill(0);
        if stored == 0 {
            return Ok(wanted);
        }

        let target = (offset / cluster_size) as u32;
        let mut within = (offset % cluster_size) as usize;
        let (mut index, mut cluster) = if entry.contiguous {
            let cluster = entry
                .start_cluster
                .checked_add(target)
                .ok_or(FsError::Corrupt)?;
            (target, cluster)
        } else {
            entry.position_before(target)
        };
        while index < target {
            cluster = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
            index += 1;
        }

        let mut done = 0usize;
        while done < stored {
            let cluster_offset = self.cluster_offset(cluster)?;
            let take = (self.cluster_size - within).min(stored - done);
            self.volume
                .read_bytes(cluster_offset + within as u64, &mut out[done..done + take])?;
            done += take;
            within = 0;

            if done < stored {
                cluster = self
                    .advance_cluster(cluster, entry.contiguous)?
                    .ok_or(FsError::Corrupt)?;
                index += 1;
            }
        }
        if !entry.contiguous {
            entry.remember_position(index, cluster);
        }
        Ok(wanted)
    }
}

struct Bitmap {
    start_cluster: u32,
    bytes: Vec<u8>,
}

impl Bitmap {
    fn is_used(&self, cluster: u32) -> bool {
        let index = cluster.saturating_sub(2);
        let byte = (index / 8) as usize;
        self.bytes
            .get(byte)
            .is_none_or(|value| value & (1 << (index % 8)) != 0)
    }

    fn set(&mut self, cluster: u32, used: bool) -> Option<(usize, u8)> {
        let index = cluster.saturating_sub(2);
        let byte = (index / 8) as usize;
        let mask = 1u8 << (index % 8);
        let value = self.bytes.get_mut(byte)?;
        let old = *value;
        *value = if used { old | mask } else { old & !mask };
        if *value != old {
            Some((byte, *value))
        } else {
            None
        }
    }
}

impl<D: BlockDevice> Exfat<D> {
    fn advance_cluster(&self, cluster: u32, contiguous: bool) -> Result<Option<u32>, FsError> {
        if contiguous {
            let next = cluster.checked_add(1).ok_or(FsError::Corrupt)?;
            if next < 2 || next >= self.max_cluster() {
                Ok(None)
            } else {
                Ok(Some(next))
            }
        } else {
            self.next_cluster(cluster)
        }
    }

    fn cluster_at(&self, start: u32, contiguous: bool, index: u32) -> Result<u32, FsError> {
        let mut cluster = start;
        for _ in 0..index {
            cluster = self
                .advance_cluster(cluster, contiguous)?
                .ok_or(FsError::Corrupt)?;
        }
        if cluster < 2 || cluster >= self.max_cluster() {
            return Err(FsError::Corrupt);
        }
        Ok(cluster)
    }

    fn read_stream_bytes(
        &self,
        start: u32,
        contiguous: bool,
        offset: u64,
        out: &mut [u8],
        data_length: u64,
    ) -> Result<(), FsError> {
        if offset
            .checked_add(out.len() as u64)
            .ok_or(FsError::OutOfBounds)?
            > data_length
        {
            return Err(FsError::OutOfBounds);
        }
        if out.is_empty() {
            return Ok(());
        }
        let cluster_size = self.cluster_size as u64;
        let index = u32::try_from(offset / cluster_size).map_err(|_| FsError::OutOfBounds)?;
        let mut cluster = self.cluster_at(start, contiguous, index)?;
        let mut within = (offset % cluster_size) as usize;
        let mut done = 0usize;
        while done < out.len() {
            let take = (self.cluster_size - within).min(out.len() - done);
            let offset = self.cluster_offset(cluster)?;
            self.volume
                .read_bytes(offset + within as u64, &mut out[done..done + take])?;
            done += take;
            within = 0;
            if done < out.len() {
                cluster = self
                    .advance_cluster(cluster, contiguous)?
                    .ok_or(FsError::Corrupt)?;
            }
        }
        Ok(())
    }

    /// Reads the allocation bitmap and up-case table named by the root.
    ///
    /// Neither is needed to read files, so trouble with them doesn't stop the
    /// mount: it costs writing, and marks the volume damaged.
    fn load_system_entries(&mut self, buffer: &mut [u8]) -> Result<(), FsError> {
        let mut bitmap_info = None;
        let mut upcase_info = None;
        let mut cluster = self.root_cluster;
        let max_clusters = MAX_DIRECTORY_BYTES
            .div_ceil(self.cluster_size as u64)
            .min(self.cluster_count as u64);
        let mut walked = 0u64;

        loop {
            self.read_cluster(cluster, buffer)?;
            let mut end = false;
            for entry in buffer.chunks_exact(32) {
                if entry[0] == ENTRY_END {
                    end = true;
                    break;
                }
                if entry[0] != ENTRY_BITMAP && entry[0] != ENTRY_UPCASE {
                    continue;
                }
                let start = u32::from_le_bytes([entry[20], entry[21], entry[22], entry[23]]);
                let length = u64::from_le_bytes([
                    entry[24], entry[25], entry[26], entry[27], entry[28], entry[29], entry[30],
                    entry[31],
                ]);
                try_push(&mut self.system_streams, (start, length))?;
                if start < 2 || start >= self.max_cluster() || length == 0 {
                    self.damaged = true;
                } else if entry[0] == ENTRY_BITMAP {
                    bitmap_info = Some((start, length));
                } else {
                    let checksum = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]);
                    upcase_info = Some((start, length, checksum));
                }
            }
            if end {
                break;
            }
            walked += 1;
            if walked >= max_clusters {
                self.damaged = true;
                break;
            }
            match self.next_cluster(cluster) {
                Ok(Some(next)) => cluster = next,
                Ok(None) => break,
                Err(_) => {
                    self.damaged = true;
                    break;
                }
            }
        }

        if let Some((start, length)) = bitmap_info {
            let needed = (self.cluster_count as u64).div_ceil(8);
            if length >= needed && length <= 16 * 1024 * 1024 {
                match self.read_bitmap(start, length) {
                    Ok(bitmap) => {
                        for cluster in 2..self.max_cluster() {
                            if !bitmap.is_used(cluster) {
                                self.next_free = cluster;
                                break;
                            }
                        }
                        self.bitmap = Some(bitmap);
                    }
                    Err(FsError::OutOfMemory) => return Err(FsError::OutOfMemory),
                    Err(_) => self.damaged = true,
                }
            } else {
                self.damaged = true;
            }
        }

        if let Some((start, length, checksum)) = upcase_info {
            if length <= UPCASE_MAX_BYTES {
                match self.read_upcase(start, length, checksum) {
                    Ok(table) => self.upcase = Some(table),
                    Err(FsError::OutOfMemory) => return Err(FsError::OutOfMemory),
                    Err(_) => self.damaged = true,
                }
            } else {
                self.damaged = true;
            }
        }

        Ok(())
    }

    fn read_bitmap(&self, start: u32, length: u64) -> Result<Bitmap, FsError> {
        let mut bytes = try_zeroed(length as usize)?;
        self.read_stream_bytes(start, false, 0, &mut bytes, length)?;
        Ok(Bitmap {
            start_cluster: start,
            bytes,
        })
    }

    fn read_upcase(&self, start: u32, length: u64, checksum: u32) -> Result<Vec<u16>, FsError> {
        let mut bytes = try_zeroed(length as usize)?;
        self.read_stream_bytes(start, false, 0, &mut bytes, length)?;

        let actual = bytes.iter().fold(0u32, |sum, &byte| {
            sum.rotate_right(1).wrapping_add(byte as u32)
        });
        if actual != checksum {
            return Err(FsError::Corrupt);
        }

        let mut table: Vec<u16> = Vec::new();
        table
            .try_reserve_exact(0x1_0000)
            .map_err(|_| FsError::OutOfMemory)?;
        table.extend((0..=0xffffu32).map(|value| value as u16));
        let mut index = 0usize;
        let mut skip = false;
        for chunk in bytes.chunks_exact(2) {
            let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
            if skip {
                index = index.saturating_add(unit as usize);
                skip = false;
            } else if index > 0xffff {
                break;
            } else if unit as usize == index {
                index += 1;
            } else if unit == 0xffff {
                skip = true;
            } else {
                table[index] = unit;
                index += 1;
            }
        }
        Ok(table)
    }

    fn upcase_unit(&self, unit: u16) -> u16 {
        self.upcase
            .as_ref()
            .and_then(|table| table.get(unit as usize))
            .copied()
            .unwrap_or_else(|| {
                // No table on this volume. A surrogate has no case, and neither
                // does a character whose capital isn't one BMP character.
                let Some(character) = char::from_u32(unit as u32) else {
                    return unit;
                };
                let mut upper = character.to_uppercase();
                match (upper.next(), upper.next()) {
                    (Some(single), None) if (single as u32) <= 0xffff => single as u32 as u16,
                    _ => unit,
                }
            })
    }

    /// The `NameHash` of a name, over its up-cased UTF-16 units.
    fn name_hash(&self, units: &[u16]) -> u16 {
        let mut hash = 0u16;
        for &unit in units {
            let unit = self.upcase_unit(unit);
            hash = hash.rotate_right(1).wrapping_add(unit & 0xff);
            hash = hash.rotate_right(1).wrapping_add(unit >> 8);
        }
        hash
    }

    fn lookup_name(&self, parent: NodeId, name: &str) -> Option<NodeId> {
        let directory = self.nodes.get(parent)?;
        if directory.kind != EntryKind::Directory {
            return None;
        }
        let query: Vec<u16> = name
            .encode_utf16()
            .map(|unit| self.upcase_unit(unit))
            .collect();
        for &child in &directory.children {
            let child_name = &self.nodes.nodes[child as usize].name;
            let folded: Vec<u16> = child_name
                .encode_utf16()
                .map(|unit| self.upcase_unit(unit))
                .collect();
            if folded == query {
                return Some(NodeId(child));
            }
        }
        None
    }
}

impl<D: WritableBlockDevice> Exfat<D> {
    /// Fails when mounting found damage. Writing to a tree that's only partly
    /// understood could reuse clusters that hold skipped files.
    fn ensure_writable(&mut self) -> Result<(), FsError> {
        if self.damaged {
            return Err(FsError::Corrupt);
        }
        if self.volume.is_read_only() {
            return Err(FsError::ReadOnly);
        }
        if !self.dirty {
            // Durable before anything else changes, or a crash looks clean.
            self.set_dirty(true)?;
            self.volume.commit()?;
        }
        Ok(())
    }

    fn now(&self) -> Option<u64> {
        self.now.and_then(|now| now())
    }

    fn read_fat_entry(&self, cluster: u32) -> Result<u32, FsError> {
        let offset = self
            .active_fat_start
            .checked_add(cluster as u64 * 4)
            .ok_or(FsError::Corrupt)?;
        self.volume.read_u32_le(offset)
    }

    fn write_fat_entry(&mut self, cluster: u32, value: u32) -> Result<(), FsError> {
        self.volume.class = WriteClass::Fat;
        let offset = self
            .fat_start
            .checked_add(cluster as u64 * 4)
            .ok_or(FsError::Corrupt)?;
        for copy in 0..self.fat_count {
            let target = offset + copy as u64 * self.fat_size_bytes;
            self.volume.write_u32_le(target, value)?;
        }
        Ok(())
    }

    fn set_dirty(&mut self, dirty: bool) -> Result<(), FsError> {
        if self.dirty == dirty {
            return Ok(());
        }
        self.volume.class = WriteClass::Data;
        let flags = self.volume.read_u16_le(106)?;
        let flags = if dirty {
            flags | VOLUME_DIRTY
        } else {
            flags & !VOLUME_DIRTY
        };
        self.volume.write_u16_le(106, flags)?;
        if let Some(base) = self.backup_base {
            self.volume.write_u16_le(base + 106, flags)?;
        }
        self.dirty = dirty;
        Ok(())
    }

    fn bitmap_used(&self, cluster: u32) -> bool {
        match &self.bitmap {
            Some(bitmap) => bitmap.is_used(cluster),
            None => self
                .read_fat_entry(cluster)
                .map(|value| value != 0)
                .unwrap_or(true),
        }
    }

    fn set_bitmap_bit(&mut self, cluster: u32, used: bool) -> Result<(), FsError> {
        let Some(bitmap) = self.bitmap.as_mut() else {
            return Ok(());
        };
        let Some((byte, value)) = bitmap.set(cluster, used) else {
            return Ok(());
        };
        let start = bitmap.start_cluster;
        self.volume.class = WriteClass::Fat;
        self.write_stream_raw(start, false, byte as u64, &[value])
    }

    fn alloc_cluster(&mut self) -> Result<u32, FsError> {
        if self.bitmap.is_none() {
            return Err(FsError::Unsupported);
        }
        let max_cluster = self.max_cluster();
        let start = self.next_free.clamp(2, max_cluster.saturating_sub(1));
        for step in 0..self.cluster_count {
            let cluster = 2 + ((start as u64 - 2 + step as u64) % self.cluster_count as u64) as u32;
            let free = match &self.bitmap {
                Some(bitmap) => !bitmap.is_used(cluster),
                None => self.read_fat_entry(cluster)? == 0,
            };
            if free {
                self.set_bitmap_bit(cluster, true)?;
                self.write_fat_entry(cluster, EXFAT_EOC)?;
                let offset = self.cluster_offset(cluster)?;
                let zeros = try_zeroed(self.cluster_size)?;
                self.volume.class = WriteClass::Data;
                self.volume.write_bytes(offset, &zeros)?;
                self.next_free = if cluster + 1 < max_cluster {
                    cluster + 1
                } else {
                    2
                };
                return Ok(cluster);
            }
        }
        Err(FsError::NoSpace)
    }

    fn free_cluster(&mut self, cluster: u32) -> Result<(), FsError> {
        if self.bitmap.is_none() {
            return Err(FsError::Unsupported);
        }
        self.set_bitmap_bit(cluster, false)?;
        self.write_fat_entry(cluster, 0)?;
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
        Ok(())
    }

    fn free_stream(
        &mut self,
        start: u32,
        contiguous: bool,
        data_length: u64,
    ) -> Result<(), FsError> {
        if start < 2 {
            return Ok(());
        }
        if contiguous {
            let count = data_length.div_ceil(self.cluster_size as u64);
            for index in 0..count {
                let cluster = start as u64 + index;
                if cluster < self.max_cluster() as u64 {
                    self.free_cluster(cluster as u32)?;
                }
            }
            Ok(())
        } else {
            self.free_chain(start)
        }
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

    fn ensure_chain(
        &mut self,
        start: u32,
        contiguous: bool,
        data_length: u64,
        needed: usize,
    ) -> Result<(u32, bool), FsError> {
        if needed == 0 {
            return Ok((start, contiguous));
        }
        if start < 2 {
            let first = self.alloc_cluster()?;
            if let Err(error) = self.extend_chain(first, needed - 1) {
                // Nothing refers to the partial chain yet, so it would leak.
                self.free_chain(first)?;
                return Err(error);
            }
            return Ok((first, false));
        }

        let current = if contiguous {
            data_length.div_ceil(self.cluster_size as u64) as usize
        } else {
            self.chain_length(start)?
        };
        if current >= needed {
            return Ok((start, contiguous));
        }
        if current == 0 {
            return Err(FsError::Corrupt);
        }

        if contiguous {
            let mut can_extend = true;
            for index in current..needed {
                let cluster = start as u64 + index as u64;
                if cluster >= self.max_cluster() as u64 || self.bitmap_used(cluster as u32) {
                    can_extend = false;
                    break;
                }
            }
            if can_extend {
                for index in current..needed {
                    let cluster = start + index as u32;
                    self.set_bitmap_bit(cluster, true)?;
                    self.write_fat_entry(cluster, EXFAT_EOC)?;
                    let offset = self.cluster_offset(cluster)?;
                    let zeros = try_zeroed(self.cluster_size)?;
                    self.volume.write_bytes(offset, &zeros)?;
                }
                self.set_dirty(true)?;
                return Ok((start, true));
            }

            // Allocate everything before the FAT changes, so that running out of
            // space leaves the contiguous stream as it was.
            let mut new_clusters = Vec::new();
            for _ in current..needed {
                let allocated = self
                    .alloc_cluster()
                    .and_then(|cluster| try_push(&mut new_clusters, cluster));
                if let Err(error) = allocated {
                    for &cluster in &new_clusters {
                        self.free_cluster(cluster)?;
                    }
                    return Err(error);
                }
            }
            for index in 0..current.saturating_sub(1) {
                let cluster = start + index as u32;
                self.write_fat_entry(cluster, cluster + 1)?;
            }
            let mut last = start + current as u32 - 1;
            for cluster in new_clusters {
                self.write_fat_entry(last, cluster)?;
                last = cluster;
            }
            return Ok((start, false));
        }

        let last = self.chain_last(start)?;
        if let Err(error) = self.extend_chain(last, needed - current) {
            // Give back what was added: the file's size still ends at `last`.
            if let Some(tail) = self.next_cluster(last)? {
                self.write_fat_entry(last, EXFAT_EOC)?;
                self.free_chain(tail)?;
            }
            return Err(error);
        }
        Ok((start, false))
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

    fn truncate_chain(
        &mut self,
        start: u32,
        contiguous: bool,
        data_length: u64,
        needed: usize,
    ) -> Result<(u32, bool), FsError> {
        if needed == 0 {
            self.free_stream(start, contiguous, data_length)?;
            return Ok((0, contiguous));
        }
        if start < 2 {
            return Ok((0, contiguous));
        }
        if contiguous {
            let current = data_length.div_ceil(self.cluster_size as u64) as usize;
            if current <= needed {
                return Ok((start, true));
            }
            for index in needed..current {
                self.free_cluster(start + index as u32)?;
            }
            return Ok((start, true));
        }

        let current = self.chain_length(start)?;
        if current <= needed {
            return Ok((start, false));
        }
        let mut cluster = start;
        for _ in 1..needed {
            cluster = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
        }
        let tail = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
        self.write_fat_entry(cluster, EXFAT_EOC)?;
        self.free_chain(tail)?;
        Ok((start, false))
    }

    fn write_stream_at(
        &mut self,
        start: u32,
        contiguous: bool,
        offset: u64,
        data: &[u8],
    ) -> Result<(), FsError> {
        self.volume.class = WriteClass::Data;
        self.write_stream_raw(start, contiguous, offset, data)
    }

    /// Writes to a stream in whatever class is current.
    fn write_stream_raw(
        &mut self,
        start: u32,
        contiguous: bool,
        offset: u64,
        data: &[u8],
    ) -> Result<(), FsError> {
        if data.is_empty() {
            return Ok(());
        }
        let cluster_size = self.cluster_size as u64;
        let index = u32::try_from(offset / cluster_size).map_err(|_| FsError::OutOfBounds)?;
        let mut cluster = self.cluster_at(start, contiguous, index)?;
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
                cluster = self
                    .advance_cluster(cluster, contiguous)?
                    .ok_or(FsError::Corrupt)?;
            }
        }
        Ok(())
    }

    fn zero_stream_range(
        &mut self,
        start: u32,
        contiguous: bool,
        from: u64,
        to: u64,
    ) -> Result<(), FsError> {
        if from >= to {
            return Ok(());
        }
        self.volume.class = WriteClass::Data;
        let cluster_size = self.cluster_size as u64;
        let index = u32::try_from(from / cluster_size).map_err(|_| FsError::OutOfBounds)?;
        let mut cluster = self.cluster_at(start, contiguous, index)?;
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
                cluster = self
                    .advance_cluster(cluster, contiguous)?
                    .ok_or(FsError::Corrupt)?;
            }
        }
        Ok(())
    }

    fn next_directory_cluster(
        &self,
        parent: NodeId,
        cluster: u32,
        index: usize,
    ) -> Result<Option<u32>, FsError> {
        let directory = self.nodes.get(parent).ok_or(FsError::NotFound)?;
        if directory.contiguous && directory.data_length > 0 {
            let total = directory.data_length.div_ceil(self.cluster_size as u64) as usize;
            if index + 1 >= total {
                Ok(None)
            } else {
                let next = cluster.checked_add(1).ok_or(FsError::Corrupt)?;
                if next < 2 || next >= self.max_cluster() {
                    Ok(None)
                } else {
                    Ok(Some(next))
                }
            }
        } else {
            self.next_cluster(cluster)
        }
    }

    fn entry_offsets(&self, parent: NodeId, location: EntryLocation) -> Result<Vec<u64>, FsError> {
        let start = self
            .nodes
            .get(parent)
            .ok_or(FsError::NotFound)?
            .start_cluster;
        let mut cluster = start;
        let mut index = 0usize;
        let mut walked = 0usize;
        loop {
            let base = self.cluster_offset(cluster)?;
            if location.first >= base && location.first < base + self.cluster_size as u64 {
                let mut offsets = Vec::with_capacity(location.count as usize);
                let mut offset = location.first;
                let mut current = cluster;
                let mut current_index = index;
                for remaining in (0..location.count).rev() {
                    offsets.push(offset);
                    offset += 32;
                    let base = self.cluster_offset(current)?;
                    // A set can end in the last slot of the last cluster, with
                    // nothing to advance to.
                    if remaining > 0 && offset >= base + self.cluster_size as u64 {
                        current = self
                            .next_directory_cluster(parent, current, current_index)?
                            .ok_or(FsError::Corrupt)?;
                        current_index += 1;
                        offset = self.cluster_offset(current)?;
                    }
                }
                return Ok(offsets);
            }
            walked += 1;
            if walked > self.cluster_count as usize {
                return Err(FsError::Corrupt);
            }
            cluster = self
                .next_directory_cluster(parent, cluster, index)?
                .ok_or(FsError::Corrupt)?;
            index += 1;
        }
    }

    fn update_stream_entry(
        &mut self,
        node: NodeId,
        start: u32,
        data_length: u64,
        contiguous: bool,
        valid_size: u64,
    ) -> Result<(), FsError> {
        let (location, parent) = self
            .nodes
            .get(node)
            .map(|entry| (entry.entry, entry.parent))
            .ok_or(FsError::NotFound)?;
        let location = location.ok_or(FsError::Corrupt)?;
        let parent = NodeId(parent.ok_or(FsError::Unsupported)?);
        if location.stream == 0 {
            return Err(FsError::Corrupt);
        }
        self.volume.class = WriteClass::Dir;
        self.volume
            .write_bytes(location.stream + 1, &[if contiguous { 0x03 } else { 0x01 }])?;
        self.volume.write_u64_le(location.stream + 8, valid_size)?;
        self.volume.write_u32_le(location.stream + 20, start)?;
        self.volume
            .write_u64_le(location.stream + 24, data_length)?;
        let now = self.now();
        let stamp = now.and_then(fat_timestamp);
        if let Some((day, time)) = stamp {
            let value = (day as u32) << 16 | time as u32;
            self.volume.write_u32_le(location.primary + 12, value)?;
            self.volume.write_u32_le(location.primary + 16, value)?;
        }
        if let Some(entry) = self.nodes.get_mut(node) {
            entry.start_cluster = start;
            entry.data_length = data_length;
            entry.contiguous = contiguous;
            entry.chain_changed();
            if entry.kind == EntryKind::File {
                entry.size = data_length;
                entry.valid_size = valid_size;
            }
            if stamp.is_some() {
                entry.times.modified = now;
                entry.times.accessed = now;
            }
        }
        self.update_set_checksum(parent, location)
    }

    fn update_set_checksum(
        &mut self,
        parent: NodeId,
        location: EntryLocation,
    ) -> Result<(), FsError> {
        self.volume.class = WriteClass::Dir;
        let offsets = self.entry_offsets(parent, location)?;
        let mut checksum = 0u16;
        let mut entry = [0u8; 32];
        for (index, offset) in offsets.iter().enumerate() {
            self.volume.read_bytes(*offset, &mut entry)?;
            for (byte_index, &byte) in entry.iter().enumerate() {
                if index == 0 && (byte_index == 2 || byte_index == 3) {
                    continue;
                }
                checksum = checksum.rotate_right(1).wrapping_add(byte as u16);
            }
        }
        self.volume.write_u16_le(location.primary + 2, checksum)
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
            || name.encode_utf16().count() > MAX_NAME_UNITS
        {
            return Err(FsError::InvalidName);
        }
        Ok(())
    }

    /// `levels` is how many levels the new node may need below `parent`.
    fn validate_new_name(&self, parent: NodeId, name: &str, levels: usize) -> Result<(), FsError> {
        self.validate_name_syntax(parent, name)?;
        if self.lookup_name(parent, name).is_some() {
            return Err(FsError::NameExists);
        }
        self.nodes.ensure_room(name)?;
        self.nodes.check_depth(parent, levels)
    }

    fn extend_directory(
        &mut self,
        parent: NodeId,
        start: u32,
        contiguous: bool,
        clusters: usize,
        data_length: u64,
    ) -> Result<(u32, bool, u64), FsError> {
        let new_cluster = self.alloc_cluster()?;
        let new_length = data_length.saturating_add(self.cluster_size as u64);
        // Durably clear before the chain reaches it. Entries written into it next
        // are a later class, and would take the clearing with them, leaving the
        // old bytes to read as a directory.
        self.volume.commit()?;
        if contiguous {
            if clusters == 0 {
                return Err(FsError::Corrupt);
            }
            for index in 0..clusters.saturating_sub(1) {
                let cluster = start + index as u32;
                self.write_fat_entry(cluster, cluster + 1)?;
            }
            self.write_fat_entry(start + clusters as u32 - 1, new_cluster)?;
            if parent != NodeId::ROOT {
                self.update_stream_entry(parent, start, new_length, false, new_length)?;
            }
            Ok((new_cluster, false, new_length))
        } else {
            let last = self.chain_last(start)?;
            self.write_fat_entry(last, new_cluster)?;
            if parent != NodeId::ROOT {
                self.update_stream_entry(parent, start, new_length, false, new_length)?;
            }
            Ok((new_cluster, false, new_length))
        }
    }

    fn find_free_entries(&mut self, parent: NodeId, needed: usize) -> Result<Vec<u64>, FsError> {
        let (start, mut contiguous, mut data_length) = {
            let directory = self.nodes.get(parent).ok_or(FsError::NotFound)?;
            (
                directory.start_cluster,
                directory.contiguous,
                directory.data_length,
            )
        };
        if start < 2 {
            return Err(FsError::Corrupt);
        }

        let mut buffer = try_zeroed(self.cluster_size)?;
        let mut cluster = start;
        let mut index = 0usize;
        let mut offsets: Vec<u64> = Vec::with_capacity(needed);
        let mut walked = 0usize;

        loop {
            self.read_cluster(cluster, &mut buffer)?;
            let base = self.cluster_offset(cluster)?;
            for (entry_index, entry) in buffer.chunks_exact(32).enumerate() {
                if entry[0] < 0x80 {
                    offsets.push(base + entry_index as u64 * 32);
                    if offsets.len() == needed {
                        return Ok(offsets);
                    }
                } else {
                    offsets.clear();
                }
            }

            index += 1;
            walked += 1;
            if walked > self.cluster_count as usize {
                return Err(FsError::NoSpace);
            }

            let next = if contiguous {
                let total = data_length.div_ceil(self.cluster_size as u64) as usize;
                if index < total {
                    let cluster = start as u64 + index as u64;
                    if cluster < self.max_cluster() as u64 {
                        Some(cluster as u32)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                self.next_cluster(cluster)?
            };

            match next {
                Some(next) => cluster = next,
                None => {
                    let (next, new_contiguous, new_length) =
                        self.extend_directory(parent, start, contiguous, index, data_length)?;
                    contiguous = new_contiguous;
                    data_length = new_length;
                    cluster = next;
                }
            }
        }
    }

    fn write_entry_set(
        &mut self,
        parent: NodeId,
        name: &str,
        attributes: u16,
        start_cluster: u32,
        data_length: u64,
        contiguous: bool,
        template: Option<[[u8; 32]; 2]>,
    ) -> Result<EntryLocation, FsError> {
        let units: Vec<u16> = name.encode_utf16().collect();
        if units.is_empty() || units.len() > MAX_NAME_UNITS {
            return Err(FsError::InvalidName);
        }
        let name_entries = units.len().div_ceil(15);
        let secondary = 1 + name_entries;
        let needed = 1 + secondary;
        let offsets = self.find_free_entries(parent, needed)?;
        self.volume.class = WriteClass::Dir;

        let mut entries: Vec<[u8; 32]> = Vec::with_capacity(needed);
        let mut primary = [0u8; 32];
        primary[0] = ENTRY_FILE;
        primary[1] = secondary as u8;
        primary[4..6].copy_from_slice(&attributes.to_le_bytes());
        if let Some((day, time)) = self.now().and_then(fat_timestamp) {
            let value = ((day as u32) << 16 | time as u32).to_le_bytes();
            primary[8..12].copy_from_slice(&value);
            primary[12..16].copy_from_slice(&value);
            primary[16..20].copy_from_slice(&value);
        }
        if let Some([old_primary, _]) = &template {
            // A rename changes the name and nothing else: attributes and
            // timestamps survive.
            primary[4..].copy_from_slice(&old_primary[4..]);
        }
        entries.push(primary);

        let mut stream = [0u8; 32];
        stream[0] = ENTRY_STREAM;
        stream[1] = if contiguous { 0x03 } else { 0x01 };
        stream[3] = units.len() as u8;
        stream[4..6].copy_from_slice(&self.name_hash(&units).to_le_bytes());
        stream[8..16].copy_from_slice(&data_length.to_le_bytes());
        stream[20..24].copy_from_slice(&start_cluster.to_le_bytes());
        stream[24..32].copy_from_slice(&data_length.to_le_bytes());
        if let Some([_, old_stream]) = &template {
            // This includes ValidDataLength, whose loss would expose whatever
            // the unwritten clusters hold.
            stream[1] = old_stream[1];
            stream[8..16].copy_from_slice(&old_stream[8..16]);
            stream[20..32].copy_from_slice(&old_stream[20..32]);
        }
        entries.push(stream);

        for chunk in units.chunks(15) {
            let mut entry = [0u8; 32];
            entry[0] = ENTRY_NAME;
            for (index, unit) in chunk.iter().enumerate() {
                entry[2 + index * 2..4 + index * 2].copy_from_slice(&unit.to_le_bytes());
            }
            entries.push(entry);
        }

        let mut checksum = 0u16;
        for (entry_index, entry) in entries.iter().enumerate() {
            for (byte_index, &byte) in entry.iter().enumerate() {
                if entry_index == 0 && (byte_index == 2 || byte_index == 3) {
                    continue;
                }
                checksum = checksum.rotate_right(1).wrapping_add(byte as u16);
            }
        }
        entries[0][2..4].copy_from_slice(&checksum.to_le_bytes());

        for (offset, entry) in offsets.iter().zip(entries.iter()) {
            self.volume.write_bytes(*offset, entry)?;
        }

        Ok(EntryLocation {
            first: offsets[0],
            primary: offsets[0],
            count: needed as u8,
            stream: offsets[1],
        })
    }

    fn delete_entry_set(&mut self, parent: NodeId, location: EntryLocation) -> Result<(), FsError> {
        self.volume.class = WriteClass::Dir;
        let offsets = self.entry_offsets(parent, location)?;
        let mut entry_type = [0u8; 1];
        for offset in offsets {
            self.volume.read_bytes(offset, &mut entry_type)?;
            self.volume.write_bytes(offset, &[entry_type[0] & 0x7f])?;
        }
        Ok(())
    }

    /// Fills in a node's facts to match the entry just written.
    fn stamp_new_node(&mut self, node: NodeId, attributes: u8) {
        let now = self.now().filter(|&now| fat_timestamp(now).is_some());
        if let Some(entry) = self.nodes.get_mut(node) {
            entry.attributes = attributes;
            entry.times = NodeTimes {
                created: now,
                modified: now,
                accessed: now,
            };
        }
    }

    fn create_file_node(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        self.validate_new_name(parent, name, 0)?;
        let location = self.write_entry_set(parent, name, 0x20, 0, 0, false, None)?;
        let node = self.nodes
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
            .map(NodeId)?;
        self.stamp_new_node(node, attr::ARCHIVE);
        Ok(node)
    }

    fn create_dir_node(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        // A directory must leave room for its own children: one at the limit
        // could not be walked on the next mount.
        self.validate_new_name(parent, name, 1)?;
        let cluster = self.alloc_cluster()?;
        // Durable before an entry can refer to it.
        self.volume.commit()?;
        let location = match self.write_entry_set(
            parent,
            name,
            0x10,
            cluster,
            self.cluster_size as u64,
            false,
            None,
        ) {
            Ok(location) => location,
            Err(error) => {
                self.free_cluster(cluster)?;
                return Err(error);
            }
        };
        let node = self
            .nodes
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
            .map(NodeId)?;
        self.stamp_new_node(node, 0);
        Ok(node)
    }

    fn remove_node(&mut self, node: NodeId) -> Result<(), FsError> {
        if node == NodeId::ROOT {
            return Err(FsError::Unsupported);
        }
        let (kind, start_cluster, contiguous, data_length, location, parent) = {
            let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
            (
                entry.kind,
                entry.start_cluster,
                entry.contiguous,
                entry.data_length,
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
        // Durably gone before its clusters can be handed to anything else.
        self.volume.commit()?;
        self.free_stream(start_cluster, contiguous, data_length)
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
        let (kind, start_cluster, contiguous, data_length, size, old_parent) = {
            let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
            (
                entry.kind,
                entry.start_cluster,
                entry.contiguous,
                entry.data_length,
                entry.size,
                entry.parent.ok_or(FsError::Unsupported)?,
            )
        };
        if kind == EntryKind::Directory && self.nodes.is_ancestor(node, new_parent) {
            return Err(FsError::Unsupported);
        }
        match self.lookup_name(new_parent, new_name) {
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
        let length = if kind == EntryKind::File {
            size
        } else {
            data_length
        };
        let template = [
            self.volume.read_array::<32>(old_location.primary)?,
            self.volume.read_array::<32>(old_location.stream)?,
        ];
        let new_location = self.write_entry_set(
            new_parent,
            new_name,
            attributes,
            start_cluster,
            length,
            contiguous,
            Some(template),
        )?;
        // The new name durably exists before the old one goes. A crash in
        // between leaves two names, which `check` resolves, and never none.
        self.volume.commit()?;
        self.delete_entry_set(NodeId(old_parent), old_location)?;
        self.nodes
            .reparent(node, new_parent, String::from(new_name), new_location)
    }

    fn write_file(&mut self, node: NodeId, offset: usize, data: &[u8]) -> Result<usize, FsError> {
        let (old_size, old_valid, start_cluster, contiguous, data_length, kind) = {
            let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
            entry.chain_changed();
            (
                entry.size,
                entry.valid_size,
                entry.start_cluster,
                entry.contiguous,
                entry.data_length,
                entry.kind,
            )
        };
        if kind != EntryKind::File {
            return Err(FsError::NotFile);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let offset = offset as u64;
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(FsError::NoSpace)?;
        let needed = end.div_ceil(self.cluster_size as u64) as usize;
        let (start, contiguous) =
            self.ensure_chain(start_cluster, contiguous, data_length, needed)?;
        // Moving ValidDataLength past a gap exposes the gap, so it has to hold
        // zeros on disk first.
        self.zero_stream_range(start, contiguous, old_valid, offset)?;
        self.write_stream_at(start, contiguous, offset, data)?;
        self.update_stream_entry(
            node,
            start,
            old_size.max(end),
            contiguous,
            old_valid.max(end),
        )?;
        Ok(data.len())
    }

    fn set_file_len(&mut self, node: NodeId, size: usize) -> Result<(), FsError> {
        let (old_size, old_valid, start_cluster, contiguous, data_length, kind) = {
            let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
            entry.chain_changed();
            (
                entry.size,
                entry.valid_size,
                entry.start_cluster,
                entry.contiguous,
                entry.data_length,
                entry.kind,
            )
        };
        if kind != EntryKind::File {
            return Err(FsError::NotFile);
        }
        let new_size = size as u64;
        if new_size == old_size {
            return Ok(());
        }
        // Shrinking: the entry first and durably, so that it never promises
        // clusters that have been freed.
        if new_size == 0 {
            self.update_stream_entry(node, 0, 0, false, 0)?;
            self.volume.commit()?;
            self.free_stream(start_cluster, contiguous, data_length)?;
            return Ok(());
        }
        let needed = new_size.div_ceil(self.cluster_size as u64) as usize;
        if new_size < old_size {
            self.update_stream_entry(
                node,
                start_cluster,
                new_size,
                contiguous,
                old_valid.min(new_size),
            )?;
            self.volume.commit()?;
            self.truncate_chain(start_cluster, contiguous, data_length, needed)?;
            return Ok(());
        }
        // No zeroing: bytes past ValidDataLength read as zero already.
        let (start, contiguous) = self.ensure_chain(start_cluster, contiguous, data_length, needed)?;
        self.update_stream_entry(node, start, new_size, contiguous, old_valid.min(new_size))?;
        Ok(())
    }
}

impl<D: WritableBlockDevice> WritableFileSystem for Exfat<D> {
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
        if self.volume.is_read_only() {
            return Ok(());
        }
        if self.dirty && !self.damaged {
            // Everything lands first: only then is the volume clean.
            self.volume.commit()?;
            self.set_dirty(false)?;
        }
        self.volume.commit()
    }

    fn set_attributes(&mut self, node: NodeId, attributes: u8) -> Result<(), FsError> {
        if node == NodeId::ROOT {
            return Err(FsError::Unsupported);
        }
        self.ensure_writable()?;
        let attributes = attributes & attr::SETTABLE;
        let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
        let location = entry.entry.ok_or(FsError::Corrupt)?;
        let parent = NodeId(entry.parent.ok_or(FsError::Unsupported)?);
        self.volume.class = WriteClass::Dir;
        // The directory bit is the entry's type, and stays.
        let current = self.volume.read_u16_le(location.primary + 4)?;
        let value = (current & !(attr::SETTABLE as u16)) | attributes as u16;
        self.volume.write_u16_le(location.primary + 4, value)?;
        self.update_set_checksum(parent, location)?;
        if let Some(entry) = self.nodes.get_mut(node) {
            entry.attributes = attributes;
        }
        Ok(())
    }

    fn set_times(&mut self, node: NodeId, times: NodeTimes) -> Result<(), FsError> {
        if node == NodeId::ROOT {
            return Err(FsError::Unsupported);
        }
        self.ensure_writable()?;
        let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
        let location = entry.entry.ok_or(FsError::Corrupt)?;
        let parent = NodeId(entry.parent.ok_or(FsError::Unsupported)?);
        let mut stored = entry.times;
        self.volume.class = WriteClass::Dir;
        let fields = [
            (times.created, 8u64),
            (times.modified, 12),
            (times.accessed, 16),
        ];
        for (value, offset) in fields {
            if let Some((day, time)) = value.and_then(fat_timestamp) {
                self.volume
                    .write_u32_le(location.primary + offset, (day as u32) << 16 | time as u32)?;
                match offset {
                    8 => stored.created = value,
                    12 => stored.modified = value,
                    _ => stored.accessed = value,
                }
            }
        }
        self.update_set_checksum(parent, location)?;
        if let Some(entry) = self.nodes.get_mut(node) {
            entry.times = stored;
        }
        Ok(())
    }

    fn set_time_source(&mut self, now: TimeSource) {
        self.now = Some(now);
    }

    fn check(&mut self) -> Result<CheckReport, FsError> {
        if !self.dirty || self.volume.is_read_only() {
            return Ok(CheckReport::default());
        }
        self.repair()
    }
}

/// The mount-time check, for what ordered updates can leave after a crash: lost
/// clusters, a second name from an interrupted rename, and a file whose chain is
/// short. A torn entry set fails its checksum at mount, flagging the volume
/// damaged and so read-only, because guessing what it held could lose more.
impl<D: WritableBlockDevice> Exfat<D> {
    fn repair(&mut self) -> Result<CheckReport, FsError> {
        let mut report = CheckReport {
            was_dirty: true,
            ..CheckReport::default()
        };
        if self.damaged {
            return Err(FsError::Corrupt);
        }
        for node in core::mem::take(&mut self.suspects) {
            self.drop_entry(NodeId(node), &mut report)?;
        }

        let mut used = ClusterSet::new(self.cluster_count)?;
        for (start, length) in self.system_streams.clone() {
            let clusters = length.div_ceil(self.cluster_size as u64);
            self.claim_stream(start, false, clusters, &mut used)?;
        }
        self.claim_stream(self.root_cluster, false, u64::MAX, &mut used)?;

        let limit = self.nodes.id_limit() as u32;
        for id in 0..limit {
            let node = NodeId(id);
            if self.nodes.kind(node) == Some(EntryKind::Directory) {
                let entry = self.nodes.get(node).ok_or(FsError::Corrupt)?;
                if id != NodeId::ROOT.0 && entry.start_cluster >= 2 {
                    let clusters = entry.data_length.div_ceil(self.cluster_size as u64);
                    let (start, contiguous) = (entry.start_cluster, entry.contiguous);
                    self.claim_stream(start, contiguous, clusters, &mut used)?;
                }
            }
        }
        for id in 0..limit {
            let node = NodeId(id);
            if self.nodes.kind(node) != Some(EntryKind::File) {
                continue;
            }
            let entry = self.nodes.get(node).ok_or(FsError::Corrupt)?;
            let (start, contiguous, length) =
                (entry.start_cluster, entry.contiguous, entry.data_length);
            if start < 2 || length == 0 {
                continue;
            }
            if used.contains(start) {
                self.drop_entry(node, &mut report)?;
                continue;
            }
            let needed = length.div_ceil(self.cluster_size as u64);
            let found = self.claim_stream(start, contiguous, needed, &mut used)?;
            if found < needed {
                let size = found * self.cluster_size as u64;
                let valid = entry_valid(&self.nodes, node).min(size);
                self.update_stream_entry(node, start, size, contiguous, valid)?;
                report.truncated_files += 1;
            }
        }

        // The bitmap says what's allocated, whatever the FAT holds.
        for cluster in 2..self.max_cluster() {
            let claimed = used.contains(cluster);
            if self.bitmap_used(cluster) != claimed {
                if !claimed {
                    report.lost_clusters += 1;
                }
                self.set_bitmap_bit(cluster, claimed)?;
            }
        }
        self.next_free = 2;
        self.volume.commit()?;
        self.set_dirty(false)?;
        self.volume.commit()?;
        Ok(report)
    }

    fn drop_entry(&mut self, node: NodeId, report: &mut CheckReport) -> Result<(), FsError> {
        let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
        let parent = entry.parent.ok_or(FsError::Corrupt)?;
        let location = entry.entry.ok_or(FsError::Corrupt)?;
        self.delete_entry_set(NodeId(parent), location)?;
        self.nodes.remove(node)?;
        report.removed_entries += 1;
        Ok(())
    }

    /// Marks up to `wanted` clusters of a stream as used. Returns how many there
    /// were, stopping at a break or at a cluster already claimed.
    fn claim_stream(
        &self,
        start: u32,
        contiguous: bool,
        wanted: u64,
        used: &mut ClusterSet,
    ) -> Result<u64, FsError> {
        let mut cluster = start;
        let mut found = 0u64;
        while found < wanted {
            if cluster < 2 || cluster >= self.max_cluster() || !used.insert(cluster) {
                break;
            }
            found += 1;
            if contiguous {
                cluster += 1;
            } else {
                match self.next_cluster(cluster) {
                    Ok(Some(next)) => cluster = next,
                    _ => break,
                }
            }
        }
        Ok(found)
    }
}

fn entry_valid(nodes: &NodeTable, node: NodeId) -> u64 {
    nodes.get(node).map_or(0, |entry| entry.valid_size)
}

impl<D: BlockDevice> FileSystem for Exfat<D> {
    fn node_count(&self) -> usize {
        self.nodes.len()
    }

    fn node_id_limit(&self) -> usize {
        self.nodes.id_limit()
    }

    fn is_damaged(&self) -> bool {
        self.damaged
    }

    fn attributes(&self, node: NodeId) -> u8 {
        self.nodes.get(node).map_or(0, |entry| entry.attributes)
    }

    fn times(&self, node: NodeId) -> NodeTimes {
        self.nodes.get(node).map_or(NodeTimes::NONE, |entry| entry.times)
    }

    fn is_read_only(&self) -> bool {
        self.damaged || self.volume.is_read_only()
    }

    fn stats(&self) -> VolumeStats {
        let free = match &self.bitmap {
            Some(bitmap) => (2..self.max_cluster())
                .filter(|&cluster| !bitmap.is_used(cluster))
                .count() as u64,
            None => 0,
        };
        VolumeStats {
            block_size: self.cluster_size as u64,
            total_blocks: self.cluster_count as u64,
            free_blocks: free,
            read_only: self.damaged || self.volume.is_read_only(),
            kind: "exfat",
        }
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
        self.lookup_name(dir, name)
    }
}

struct DirTask {
    node: u32,
    cluster: u32,
    data_length: u64,
    contiguous: bool,
    /// Parents between this directory and the root.
    depth: usize,
}

struct PendingEntry {
    name_units: Vec<u16>,
    name_hash: u16,
    kind: EntryKind,
    start_cluster: u32,
    valid_length: u64,
    data_length: u64,
    contiguous: bool,
    location: EntryLocation,
    attributes: u16,
    times: NodeTimes,
}

/// One directory entry's contribution to a `SetChecksum`, which skips the
/// checksum field itself in the primary entry.
fn checksum_entry(mut checksum: u16, entry: &[u8], primary: bool) -> u16 {
    for (index, &byte) in entry.iter().enumerate() {
        if primary && (index == 2 || index == 3) {
            continue;
        }
        checksum = checksum.rotate_right(1).wrapping_add(byte as u16);
    }
    checksum
}

/// Reads a file entry's timestamps: each a FAT time in the low half of a word,
/// and a FAT date in the high half.
fn entry_times(entry: &[u8]) -> NodeTimes {
    let stamp = |at: usize| {
        let word = u32::from_le_bytes([entry[at], entry[at + 1], entry[at + 2], entry[at + 3]]);
        fat_time_to_unix((word >> 16) as u16, word as u16)
    };
    NodeTimes {
        created: stamp(8),
        modified: stamp(12),
        accessed: stamp(16),
    }
}

struct FileSet {
    remaining: u8,
    attributes: u16,
    times: NodeTimes,
    flags: u8,
    name_len: usize,
    name_hash: u16,
    name_units: [u16; MAX_NAME_UNITS],
    name_count: usize,
    start_cluster: u32,
    valid_length: u64,
    data_length: u64,
    has_stream: bool,
    /// The first secondary wasn't a stream entry, as the format requires.
    misordered: bool,
    primary: u64,
    stream: u64,
    count: u8,
    stored_checksum: u16,
    checksum: u16,
}

impl FileSet {
    fn new(entry: &[u8], offset: u64) -> Self {
        Self {
            remaining: entry[1],
            attributes: u16::from_le_bytes([entry[4], entry[5]]),
            times: entry_times(entry),
            flags: 0,
            name_len: 0,
            name_hash: 0,
            name_units: [0; MAX_NAME_UNITS],
            name_count: 0,
            start_cluster: 0,
            valid_length: 0,
            data_length: 0,
            has_stream: false,
            misordered: false,
            primary: offset,
            stream: 0,
            count: entry[1].saturating_add(1),
            stored_checksum: u16::from_le_bytes([entry[2], entry[3]]),
            checksum: checksum_entry(0, entry, true),
        }
    }

    /// Consumes one secondary entry and reports whether the set is complete.
    fn push(&mut self, entry: &[u8], offset: u64) -> bool {
        self.checksum = checksum_entry(self.checksum, entry, false);
        if !self.has_stream && entry[0] != ENTRY_STREAM {
            self.misordered = true;
        }
        match entry[0] {
            ENTRY_STREAM => {
                self.flags = entry[1];
                self.name_len = entry[3] as usize;
                self.name_hash = u16::from_le_bytes([entry[4], entry[5]]);
                self.valid_length = u64::from_le_bytes([
                    entry[8], entry[9], entry[10], entry[11], entry[12], entry[13], entry[14],
                    entry[15],
                ]);
                self.start_cluster =
                    u32::from_le_bytes([entry[20], entry[21], entry[22], entry[23]]);
                self.data_length = u64::from_le_bytes([
                    entry[24], entry[25], entry[26], entry[27], entry[28], entry[29], entry[30],
                    entry[31],
                ]);
                self.has_stream = true;
                self.stream = offset;
            }
            ENTRY_NAME => {
                for index in 0..15 {
                    if self.name_count >= MAX_NAME_UNITS {
                        break;
                    }
                    let offset = 2 + index * 2;
                    self.name_units[self.name_count] =
                        u16::from_le_bytes([entry[offset], entry[offset + 1]]);
                    self.name_count += 1;
                }
            }
            _ => {}
        }

        self.remaining = self.remaining.saturating_sub(1);
        self.remaining == 0
    }

    /// Returns the entry, or `None` if the set is inconsistent or fails its
    /// checksum.
    fn finish(self) -> Option<PendingEntry> {
        if !self.has_stream || self.misordered || self.name_len == 0 {
            return None;
        }
        if self.name_len > self.name_count || self.name_len > MAX_NAME_UNITS {
            return None;
        }
        if self.checksum != self.stored_checksum {
            return None;
        }

        let kind = if self.attributes & 0x10 != 0 {
            EntryKind::Directory
        } else {
            EntryKind::File
        };
        let contiguous = self.flags & 0x03 == 0x03;

        let mut name_units = Vec::new();
        name_units.try_reserve_exact(self.name_len).ok()?;
        name_units.extend_from_slice(&self.name_units[..self.name_len]);

        Some(PendingEntry {
            name_units,
            name_hash: self.name_hash,
            kind,
            start_cluster: self.start_cluster,
            valid_length: self.valid_length,
            data_length: self.data_length,
            contiguous,
            location: EntryLocation {
                first: self.primary,
                primary: self.primary,
                count: self.count,
                stream: self.stream,
            },
            attributes: self.attributes,
            times: self.times,
        })
    }
}

/// The boot record a mount will trust.
struct SelectedBoot {
    boot: [u8; 512],
    sector_shift: u32,
    /// A valid backup region exists, so flag changes must be mirrored there.
    has_backup: bool,
    /// The main region was damaged. The file tree may still be fine, but the
    /// volume shouldn't be written.
    used_backup: bool,
}

/// Checks the boot region at `base_sector` and returns its sector shift.
fn check_boot_region<D: BlockDevice>(
    volume: &Volume<D>,
    boot: &[u8; 512],
    base_sector: u64,
) -> Result<u32, FsError> {
    if &boot[3..11] != EXFAT_SIGNATURE {
        return Err(FsError::UnsupportedVolume);
    }
    if boot[510] != 0x55 || boot[511] != 0xaa {
        return Err(FsError::InvalidBootSector);
    }
    let sector_shift = boot[108] as u32;
    let cluster_shift = boot[109] as u32;
    if !(9..=12).contains(&sector_shift) || sector_shift + cluster_shift > 25 {
        return Err(FsError::InvalidBootSector);
    }
    verify_boot_checksum(volume, 1 << sector_shift, base_sector)?;
    Ok(sector_shift)
}

/// Chooses the main boot region, or the backup if the main one is damaged.
fn select_boot<D: BlockDevice>(volume: &Volume<D>) -> Result<SelectedBoot, FsError> {
    let main = volume.read_array::<512>(0)?;
    let main_error = match check_boot_region(volume, &main, 0) {
        Ok(sector_shift) => {
            let offset = 12u64 << sector_shift;
            let has_backup = volume
                .read_array::<512>(offset)
                .is_ok_and(|backup| check_boot_region(volume, &backup, 12) == Ok(sector_shift));
            return Ok(SelectedBoot {
                boot: main,
                sector_shift,
                has_backup,
                used_backup: false,
            });
        }
        Err(error) => error,
    };

    // The sector size is in the damaged record, so try each legal one. A backup
    // only counts if it agrees about the sector size it was found with.
    for shift in 9..=12u32 {
        let Ok(backup) = volume.read_array::<512>(12u64 << shift) else {
            continue;
        };
        if check_boot_region(volume, &backup, 12) == Ok(shift) {
            return Ok(SelectedBoot {
                boot: backup,
                sector_shift: shift,
                has_backup: true,
                used_backup: true,
            });
        }
    }
    Err(main_error)
}

fn verify_boot_checksum<D: BlockDevice>(
    volume: &Volume<D>,
    sector_size: usize,
    base_sector: u64,
) -> Result<(), FsError> {
    let region_len = sector_size
        .checked_mul(11)
        .ok_or(FsError::InvalidBootSector)?;
    let base = base_sector
        .checked_mul(sector_size as u64)
        .ok_or(FsError::Corrupt)?;
    let mut region = try_zeroed(region_len)?;
    volume.read_bytes(base, &mut region)?;
    let expected = boot_checksum(&region);
    let stored = volume.read_u32_le(base + (11 * sector_size) as u64)?;
    if expected != stored {
        return Err(FsError::Corrupt);
    }
    Ok(())
}

fn boot_checksum(region: &[u8]) -> u32 {
    let mut checksum = 0u32;
    for (index, &byte) in region.iter().enumerate() {
        // VolumeFlags (106-107) and PercentInUse (112) are excluded.
        if index == 106 || index == 107 || index == 112 {
            continue;
        }
        checksum = checksum.rotate_right(1).wrapping_add(byte as u32);
    }
    checksum
}
