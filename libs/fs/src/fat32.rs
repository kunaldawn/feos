//! FAT32, read and write.
//!
//! The driver understands the FAT32 BPB, long file names (LFN), the FAT
//! cluster chain, and arbitrary directory trees. It rejects FAT12/FAT16
//! volumes, which use a fixed root-directory area and a different
//! cluster-count threshold: those are [`crate::Fat16`]'s, read-only.
//!
//! Metadata is untrusted. A directory that loops, is cross-linked, runs past the
//! specification's size limit or nests too deeply is skipped, and the volume is
//! marked [damaged](FileSystem::is_damaged): what's intact stays readable, and
//! writes are refused.
//!
//! ## Surviving a crash
//!
//! A write-back device may reorder writes, so each one is
//! [classified](WriteClass): contents, then the FAT, then directory entries.
//! Whatever a crash cuts off, a directory entry only ever refers to a chain and
//! contents that exist. What has to go the other way (deleting, truncating) is
//! split by a [commit](WritableBlockDevice::commit): the entry changes first, and
//! the clusters are freed after. Renaming writes the new entry and commits before
//! erasing the old one.
//!
//! So a crash leaves only lost clusters, a file shorter than its chain, or one
//! name too many. The dirty bits (`FAT[1]` and the boot sector, set before the
//! first change and cleared by [`flush`](WritableFileSystem::flush)) say when to
//! look, and [`check`](WritableFileSystem::check) repairs all three.
//!
//! Layout references: Microsoft's FAT specification, sections "Boot Sector and
//! BPB" and "FAT 32", and the Linux `fs/fat` implementation for the LFN
//! checksum and entry ordering rules.

use alloc::string::String;
use alloc::vec::Vec;

use feos_hal::time::DateTime;

use crate::block::{BlockDevice, WritableBlockDevice, WriteClass};
use crate::tree::{path_component, try_push, try_zeroed, ClusterSet, EntryLocation, NodeTable};
use crate::volume::Volume;
use crate::{
    attr, CheckReport, EntryKind, FileSystem, FsError, NodeId, NodeTimes, TimeSource, VolumeStats,
    WritableFileSystem, MAX_PATH_DEPTH,
};

const FAT_MASK: u32 = 0x0fff_ffff;
const FAT_EOC_MIN: u32 = 0x0fff_fff8;
const FAT_BAD: u32 = 0x0fff_fff7;
const FAT_MIN_CLUSTERS: u64 = 65_525;
const MAX_PATH_NAME_UNITS: usize = 260;
const MAX_FILE_SIZE: u64 = u32::MAX as u64;
/// The specification's limit on a directory: 2 MiB of 32-byte entries.
const MAX_DIRECTORY_ENTRIES: usize = 65_536;
/// `FAT[1]` bit: set when the volume was unmounted cleanly.
const FAT1_CLEAN: u32 = 0x0800_0000;
/// Boot sector byte holding the dirty flag Linux and `fsck.vfat` use.
const BOOT_FLAGS: u64 = 65;
const BOOT_FLAG_DIRTY: u8 = 0x01;
/// Attribute bits kept on a node. Directory and volume bits are the entry's type.
const NODE_ATTRIBUTES: u8 = attr::SETTABLE;
/// FAT dates start here.
const FAT_EPOCH_YEAR: u32 = 1980;
/// Bytes of the FAT handled at a time by the mount-time check.
const CHECK_CHUNK: usize = 64 * 1024;

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
    damage_reason: Option<&'static str>,
    /// The on-disk flags say a change is, or may be, in progress.
    dirty: bool,
    /// Directories that share a start with another, seen while the volume was
    /// dirty: the two names of an interrupted rename.
    suspects: Vec<u32>,
    now: Option<TimeSource>,
}

/// Encodes a time as FAT's `(date, time)` words, or `None` before 1980 and past
/// what the seven-bit year holds.
pub(crate) fn fat_timestamp(seconds: u64) -> Option<(u16, u16)> {
    let date = DateTime::from_unix(seconds);
    if !(FAT_EPOCH_YEAR..FAT_EPOCH_YEAR + 128).contains(&date.year) {
        return None;
    }
    let day = ((date.year - FAT_EPOCH_YEAR) as u16) << 9
        | (date.month as u16) << 5
        | date.day as u16;
    let time = (date.hour as u16) << 11 | (date.minute as u16) << 5 | ((date.second as u16) / 2);
    Some((day, time))
}

/// Decodes FAT's date and time words. Zero, which a clockless writer leaves, and
/// anything impossible decode to `None`.
pub(crate) fn fat_time_to_unix(day: u16, time: u16) -> Option<u64> {
    if day == 0 {
        return None;
    }
    DateTime {
        year: FAT_EPOCH_YEAR + (day >> 9) as u32,
        month: ((day >> 5) & 0x0f) as u8,
        day: (day & 0x1f) as u8,
        hour: (time >> 11) as u8,
        minute: ((time >> 5) & 0x3f) as u8,
        second: ((time & 0x1f) * 2) as u8,
    }
    .to_unix()
}

fn entry_u16(entry: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([entry[at], entry[at + 1]])
}

/// Reads the timestamps out of a short entry.
pub(crate) fn entry_times(entry: &[u8]) -> NodeTimes {
    NodeTimes {
        created: fat_time_to_unix(entry_u16(entry, 16), entry_u16(entry, 14)),
        modified: fat_time_to_unix(entry_u16(entry, 24), entry_u16(entry, 22)),
        accessed: fat_time_to_unix(entry_u16(entry, 18), 0),
    }
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

        // A dirty volume's tree can legitimately hold an interrupted rename.
        let fat1 = volume.read_u32_le(fat_start + 4)?;
        let dirty = boot[BOOT_FLAGS as usize] & BOOT_FLAG_DIRTY != 0 || fat1 & FAT1_CLEAN == 0;

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
            damage_reason: None,
            dirty,
            suspects: Vec::new(),
            now: None,
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
                Err(error) => {
                    let _ = error;
                    fs.mark_damaged("unreadable directory");
                }
            }
        }

        Ok(fs)
    }

    fn mark_damaged(&mut self, reason: &'static str) {
        self.damaged = true;
        if self.damage_reason.is_none() {
            self.damage_reason = Some(reason);
        }
    }

    /// What mounting found wrong first, if anything.
    pub fn damage_reason(&self) -> Option<&'static str> {
        self.damage_reason
    }

    /// Gives the device back, without writing anything further.
    pub fn into_device(self) -> D {
        self.volume.into_device()
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

        if visited.contains(cluster) && self.dirty && task.node != 0 {
            try_push(&mut self.suspects, task.node)?;
            return Ok(());
        }

        loop {
            if !visited.insert(cluster) {
                // A loop in this chain, or a cluster shared with a directory
                // already walked. Either way, there's nothing new here.
                self.mark_damaged("directory loop or shared cluster");
                return Ok(());
            }
            self.read_cluster(cluster, buffer)?;
            let cluster_start = self.cluster_offset(cluster)?;

            for (index, entry) in buffer.as_chunks::<32>().0.iter().enumerate() {
                let offset = cluster_start + index as u64 * 32;
                if entry[0] == 0x00 {
                    return Ok(());
                }
                entries += 1;
                if entries > MAX_DIRECTORY_ENTRIES {
                    self.mark_damaged("directory too large");
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
                    self.mark_damaged("unusable entry");
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
                if let Some(added) = self.nodes.get_mut(NodeId(node)) {
                    added.attributes = attributes & NODE_ATTRIBUTES;
                    added.times = entry_times(entry);
                }

                if kind == EntryKind::Directory {
                    let depth = task.depth + 1;
                    if visited.contains(start) && self.dirty {
                        try_push(&mut self.suspects, node)?;
                    } else if depth >= MAX_PATH_DEPTH || visited.contains(start) {
                        self.mark_damaged("directory nested too deep or shared");
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
            let mut take = (self.cluster_size - within).min(wanted - done);
            // Consecutive clusters go to the device as one transfer.
            let mut last = cluster;
            let mut last_index = index;
            while take < wanted - done {
                match self.next_cluster(last)? {
                    Some(next) if next == last + 1 => {
                        last = next;
                        last_index += 1;
                        take = (take + self.cluster_size).min(wanted - done);
                    }
                    _ => break,
                }
            }
            self.volume
                .read_bytes(cluster_offset + within as u64, &mut out[done..done + take])?;
            done += take;
            within = 0;
            cluster = last;
            index = last_index;

            if done < wanted {
                cluster = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
                index += 1;
            }
        }
        entry.remember_position(index, cluster);
        Ok(done)
    }

    /// Counts free FAT entries, for a volume whose FSInfo doesn't say.
    fn count_free(&self) -> Result<u32, FsError> {
        let mut chunk = try_zeroed(CHECK_CHUNK)?;
        let total = self.cluster_count as u64 + 2;
        let start = self.fat_entry_offset(0)?;
        let mut free = 0u32;
        let mut done = 0u64;
        while done < total {
            let entries = (total - done).min((CHECK_CHUNK / 4) as u64) as usize;
            self.volume.read_bytes(start + done * 4, &mut chunk[..entries * 4])?;
            for (index, entry) in chunk[..entries * 4].as_chunks::<4>().0.iter().enumerate() {
                let value = u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
                if done + index as u64 >= 2 && value & FAT_MASK == 0 {
                    free += 1;
                }
            }
            done += entries as u64;
        }
        Ok(free)
    }
}

impl<D: WritableBlockDevice> Fat32<D> {
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
            self.write_dirty_flags(true)?;
            self.volume.commit()?;
            self.dirty = true;
        }
        Ok(())
    }

    /// Writes both dirty flags. `FAT[1]`'s bit is in every copy, because tools
    /// compare the copies.
    fn write_dirty_flags(&mut self, dirty: bool) -> Result<(), FsError> {
        self.volume.class = WriteClass::Data;
        let flags = self.volume.read_array::<1>(BOOT_FLAGS)?[0];
        let flags = if dirty {
            flags | BOOT_FLAG_DIRTY
        } else {
            flags & !BOOT_FLAG_DIRTY
        };
        self.volume.write_bytes(BOOT_FLAGS, &[flags])?;
        for copy in 0..self.fat_count {
            let offset = self.fat_start + 4 + copy as u64 * self.fat_size_bytes;
            let value = self.volume.read_u32_le(offset)?;
            let value = if dirty {
                value & !FAT1_CLEAN
            } else {
                value | FAT1_CLEAN
            };
            self.volume.write_u32_le(offset, value)?;
        }
        Ok(())
    }

    fn now(&self) -> Option<u64> {
        self.now.and_then(|now| now())
    }

    fn read_fat_entry(&self, cluster: u32) -> Result<u32, FsError> {
        let offset = self.fat_entry_offset(cluster)?;
        Ok(self.volume.read_u32_le(offset)? & FAT_MASK)
    }

    fn write_fat_entry(&mut self, cluster: u32, value: u32) -> Result<(), FsError> {
        self.volume.class = WriteClass::Fat;
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
        self.volume.class = WriteClass::Fat;
        let free = self.free_count;
        for offset in self.fsinfo.iter().copied().flatten() {
            if let Some(free) = free {
                self.volume.write_u32_le(offset + 488, free)?;
            }
            self.volume.write_u32_le(offset + 492, self.next_free)?;
        }
        Ok(())
    }

    /// Allocates a cluster. Only a directory's needs clearing: a file's bytes
    /// past its size are never read, and growing a file zeroes what it exposes.
    fn alloc_cluster(&mut self, clear: bool) -> Result<u32, FsError> {
        let max_cluster = self.max_cluster();
        let start = self.next_free.clamp(2, max_cluster.saturating_sub(1));
        for step in 0..self.cluster_count {
            let cluster = 2 + ((start - 2 + step) % self.cluster_count);
            if self.read_fat_entry(cluster)? == 0 {
                if clear {
                    let offset = self.cluster_offset(cluster)?;
                    let zeros = try_zeroed(self.cluster_size)?;
                    self.volume.class = WriteClass::Data;
                    self.volume.write_bytes(offset, &zeros)?;
                }
                self.write_fat_entry(cluster, FAT_EOC_MIN)?;
                self.next_free = if cluster + 1 < max_cluster {
                    cluster + 1
                } else {
                    2
                };
                if let Some(free) = self.free_count.as_mut() {
                    *free = free.saturating_sub(1);
                }
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
        Ok(())
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

    /// Grows `node`'s chain to at least `needed` clusters and returns its start.
    fn ensure_chain(&mut self, node: NodeId, start: u32, needed: usize) -> Result<u32, FsError> {
        if needed == 0 {
            return Ok(start);
        }
        if start < 2 {
            let first = self.alloc_cluster(false)?;
            let last = match self.extend_chain(first, needed - 1) {
                Ok(last) => last,
                Err(error) => {
                    // Nothing refers to the partial chain yet, so it would leak.
                    self.free_chain(first)?;
                    return Err(error);
                }
            };
            self.remember_tail(node, needed - 1, last);
            return Ok(first);
        }

        let tail = self.nodes.get(node).and_then(|entry| entry.tail);
        let (length, last) = match tail {
            Some((index, cluster)) => (index as usize + 1, cluster),
            None => (self.chain_length(start)?, self.chain_last(start)?),
        };
        if length >= needed {
            self.remember_tail(node, length - 1, last);
            return Ok(start);
        }
        match self.extend_chain(last, needed - length) {
            Ok(tail) => {
                self.remember_tail(node, needed - 1, tail);
                Ok(start)
            }
            Err(error) => {
                // Give back what was added: the file's size still ends at `last`.
                if let Some(tail) = self.next_cluster(last)? {
                    self.write_fat_entry(last, FAT_EOC_MIN)?;
                    self.free_chain(tail)?;
                }
                Err(error)
            }
        }
    }

    fn remember_tail(&mut self, node: NodeId, index: usize, cluster: u32) {
        if let Some(entry) = self.nodes.get_mut(node) {
            entry.tail = Some((index as u32, cluster));
        }
    }

    /// Appends `count` clusters after `last`, linking each as it's allocated,
    /// and returns the new last cluster. Linking one at a time means the walk
    /// back on failure finds them all.
    fn extend_chain(&mut self, mut last: u32, count: usize) -> Result<u32, FsError> {
        for _ in 0..count {
            let next = self.alloc_cluster(false)?;
            self.write_fat_entry(last, next)?;
            last = next;
        }
        Ok(last)
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
        Ok(start)
    }

    fn write_stream_at(&mut self, start: u32, offset: u64, data: &[u8]) -> Result<(), FsError> {
        if data.is_empty() {
            return Ok(());
        }
        self.volume.class = WriteClass::Data;
        let cluster_size = self.cluster_size as u64;
        let mut cluster = start;
        for _ in 0..(offset / cluster_size) {
            cluster = self.next_cluster(cluster)?.ok_or(FsError::Corrupt)?;
        }
        let mut within = (offset % cluster_size) as usize;
        let mut done = 0usize;
        while done < data.len() {
            let mut take = (self.cluster_size - within).min(data.len() - done);
            let offset = self.cluster_offset(cluster)?;
            // Consecutive clusters go to the device as one transfer.
            while take < data.len() - done {
                match self.next_cluster(cluster)? {
                    Some(next) if next == cluster + 1 => {
                        cluster = next;
                        take = (take + self.cluster_size).min(data.len() - done);
                    }
                    _ => break,
                }
            }
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
        self.volume.class = WriteClass::Data;
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
        self.volume.class = WriteClass::Dir;
        self.volume
            .write_u16_le(location.primary + 20, (start >> 16) as u16)?;
        self.volume
            .write_u16_le(location.primary + 26, (start & 0xffff) as u16)?;
        self.volume
            .write_u32_le(location.primary + 28, size as u32)?;
        let now = self.now();
        let stamp = now.and_then(fat_timestamp);
        if let Some((day, time)) = stamp {
            self.volume.write_u16_le(location.primary + 22, time)?;
            self.volume.write_u16_le(location.primary + 24, day)?;
            self.volume.write_u16_le(location.primary + 18, day)?;
        }
        // Whatever backs up this volume should know it changed.
        let flags = self.volume.read_array::<1>(location.primary + 11)?[0];
        if flags & attr::ARCHIVE == 0 {
            self.volume
                .write_bytes(location.primary + 11, &[flags | attr::ARCHIVE])?;
        }
        if let Some(entry) = self.nodes.get_mut(node) {
            entry.start_cluster = start;
            entry.size = size;
            entry.valid_size = size;
            entry.data_length = size;
            entry.attributes |= attr::ARCHIVE;
            if stamp.is_some() {
                entry.times.modified = now;
                entry.times.accessed = now;
            }
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
        let needed = end.div_ceil(self.cluster_size as u64) as usize;
        let start = self.ensure_chain(node, start_cluster, needed)?;
        // A gap reads as zeros. What the new data covers needs no clearing.
        if offset as u64 > old_size {
            self.zero_stream_range(start, old_size, offset as u64)?;
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
        let needed =
            new_size.div_ceil(self.cluster_size as u64) as usize;
        if new_size < old_size {
            // The entry first and durably, so that it never promises clusters
            // that have been freed.
            let start = if new_size == 0 { 0 } else { start_cluster };
            self.update_file_meta(node, start, new_size)?;
            self.volume.commit()?;
            if let Some(entry) = self.nodes.get_mut(node) {
                entry.tail = None;
            }
            self.truncate_chain(start_cluster, needed)?;
        } else {
            let start = self.ensure_chain(node, start_cluster, needed)?;
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
            for entry in buffer.as_chunks::<32>().0 {
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
            for (index, entry) in buffer.as_chunks::<32>().0.iter().enumerate() {
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
                    let next = self.alloc_cluster(true)?;
                    // Durably clear before the chain reaches it. Entries written
                    // into it next are a later class, and would take the clearing
                    // with them, leaving the old bytes to read as a directory.
                    self.volume.commit()?;
                    self.write_fat_entry(cluster, next)?;
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
        let lfn_count = (units.len() + 1).div_ceil(13);
        if lfn_count > 20 {
            return Err(FsError::InvalidName);
        }
        let needed = lfn_count + 1;
        let offsets = self.find_free_entries(parent, needed)?;

        self.volume.class = WriteClass::Dir;
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
        } else if let Some((day, time)) = self.now().and_then(fat_timestamp) {
            entry[14..16].copy_from_slice(&time.to_le_bytes());
            entry[16..18].copy_from_slice(&day.to_le_bytes());
            entry[18..20].copy_from_slice(&day.to_le_bytes());
            entry[22..24].copy_from_slice(&time.to_le_bytes());
            entry[24..26].copy_from_slice(&day.to_le_bytes());
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
        self.volume.class = WriteClass::Dir;
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
        let node = self
            .nodes
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

    fn create_dir_node(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        // A directory must leave room for its own children: one at the limit
        // could not be walked on the next mount.
        self.validate_new_name(parent, name, 1)?;
        let cluster = self.alloc_cluster(true)?;
        let parent_cluster = if parent == NodeId::ROOT {
            0
        } else {
            self.nodes
                .get(parent)
                .ok_or(FsError::NotFound)?
                .start_cluster
        };

        let base = self.cluster_offset(cluster)?;
        let mut dot = build_short_entry(&dot_name(true), 0x10, cluster, 0);
        let mut dotdot = build_short_entry(&dot_name(false), 0x10, parent_cluster, 0);
        if let Some((day, time)) = self.now().and_then(fat_timestamp) {
            for entry in [&mut dot, &mut dotdot] {
                entry[14..16].copy_from_slice(&time.to_le_bytes());
                entry[16..18].copy_from_slice(&day.to_le_bytes());
                entry[18..20].copy_from_slice(&day.to_le_bytes());
                entry[22..24].copy_from_slice(&time.to_le_bytes());
                entry[24..26].copy_from_slice(&day.to_le_bytes());
            }
        }
        // The new directory is unreachable until its entry lands.
        self.volume.class = WriteClass::Data;
        self.volume.write_bytes(base, &dot)?;
        self.volume.write_bytes(base + 32, &dotdot)?;
        // Durable before an entry can refer to it, for the same reason as when a
        // directory grows.
        self.volume.commit()?;

        let location = match self.write_entry_set(parent, name, 0x10, cluster, 0, None) {
            Ok(location) => location,
            Err(error) => {
                self.free_chain(cluster)?;
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

    fn update_dotdot(&mut self, node: NodeId, parent_cluster: u32) -> Result<(), FsError> {
        let start = self.nodes.get(node).ok_or(FsError::NotFound)?.start_cluster;
        if start < 2 {
            return Ok(());
        }
        let base = self.cluster_offset(start)?;
        let mut buffer = try_zeroed(self.cluster_size)?;
        self.read_cluster(start, &mut buffer)?;
        self.volume.class = WriteClass::Dir;
        for (index, entry) in buffer.as_chunks::<32>().0.iter().enumerate() {
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
            // Durably gone before its clusters can be handed to anything else.
            self.volume.commit()?;
            self.free_chain(start_cluster)?;
        }
        Ok(())
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
        // The new name durably exists before the old one goes. A crash in
        // between leaves two names, which `check` resolves, and never none.
        self.volume.commit()?;
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
        if self.volume.is_read_only() {
            return Ok(());
        }
        if self.dirty && !self.damaged {
            self.update_fsinfo()?;
            // Everything lands first: only then is the volume clean.
            self.volume.commit()?;
            self.write_dirty_flags(false)?;
            self.volume.commit()?;
            self.dirty = false;
            return Ok(());
        }
        self.volume.commit()
    }

    fn set_attributes(&mut self, node: NodeId, attributes: u8) -> Result<(), FsError> {
        if node == NodeId::ROOT {
            return Err(FsError::Unsupported);
        }
        self.ensure_writable()?;
        let attributes = attributes & NODE_ATTRIBUTES;
        let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
        let location = entry.entry.ok_or(FsError::Corrupt)?;
        self.volume.class = WriteClass::Dir;
        // The directory bit is the entry's type, and stays.
        let current = self.volume.read_array::<1>(location.primary + 11)?[0];
        self.volume
            .write_bytes(location.primary + 11, &[(current & !NODE_ATTRIBUTES) | attributes])?;
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
        let location = self
            .nodes
            .get(node)
            .ok_or(FsError::NotFound)?
            .entry
            .ok_or(FsError::Corrupt)?;
        self.volume.class = WriteClass::Dir;
        let mut stored = self.nodes.get(node).ok_or(FsError::NotFound)?.times;
        if let Some((day, time)) = times.created.and_then(fat_timestamp) {
            self.volume.write_u16_le(location.primary + 14, time)?;
            self.volume.write_u16_le(location.primary + 16, day)?;
            stored.created = times.created;
        }
        if let Some((day, time)) = times.modified.and_then(fat_timestamp) {
            self.volume.write_u16_le(location.primary + 22, time)?;
            self.volume.write_u16_le(location.primary + 24, day)?;
            stored.modified = times.modified;
        }
        if let Some((day, _)) = times.accessed.and_then(fat_timestamp) {
            self.volume.write_u16_le(location.primary + 18, day)?;
            stored.accessed = times.accessed;
        }
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

/// The mount-time check. See the module docs for what a crash can leave.
impl<D: WritableBlockDevice> Fat32<D> {
    fn repair(&mut self) -> Result<CheckReport, FsError> {
        let mut report = CheckReport {
            was_dirty: true,
            ..CheckReport::default()
        };
        // Damage found at mount isn't a crash's doing, and repairing around what
        // isn't understood could destroy what can still be read.
        if self.damaged {
            return Err(FsError::Corrupt);
        }

        self.copy_primary_fat()?;
        for node in core::mem::take(&mut self.suspects) {
            self.drop_entry(NodeId(node), &mut report)?;
        }

        let mut used = ClusterSet::new(self.cluster_count)?;
        // Directories claim their clusters first: their chains were walked at
        // mount, and a file sharing one with them is the one that has to give.
        let limit = self.nodes.id_limit() as u32;
        for id in 0..limit {
            let node = NodeId(id);
            if self.nodes.kind(node) == Some(EntryKind::Directory) {
                let start = self.nodes.get(node).ok_or(FsError::Corrupt)?.start_cluster;
                self.claim_directory(start, &mut used)?;
                self.fix_directory(node, &mut report)?;
            }
        }
        for id in 0..limit {
            let node = NodeId(id);
            if self.nodes.kind(node) == Some(EntryKind::File) {
                self.fix_file(node, &mut used, &mut report)?;
            }
        }

        report.lost_clusters = self.free_unclaimed(&used)?;
        self.free_count = Some(self.count_free()?);
        self.next_free = 2;
        self.update_fsinfo()?;
        self.volume.commit()?;
        self.write_dirty_flags(false)?;
        self.volume.commit()?;
        self.dirty = false;
        Ok(report)
    }

    /// Makes every mirror match the first, which a crash can leave differing.
    fn copy_primary_fat(&mut self) -> Result<(), FsError> {
        if self.active_fat.is_some() || self.fat_count < 2 {
            return Ok(());
        }
        self.volume.class = WriteClass::Fat;
        let mut primary = try_zeroed(CHECK_CHUNK)?;
        let mut mirror = try_zeroed(CHECK_CHUNK)?;
        let mut offset = 0u64;
        while offset < self.fat_size_bytes {
            let take = (self.fat_size_bytes - offset).min(CHECK_CHUNK as u64) as usize;
            self.volume
                .read_bytes(self.fat_start + offset, &mut primary[..take])?;
            for copy in 1..self.fat_count as u64 {
                let at = self.fat_start + copy * self.fat_size_bytes + offset;
                self.volume.read_bytes(at, &mut mirror[..take])?;
                if primary[..take] != mirror[..take] {
                    self.volume.write_bytes(at, &primary[..take])?;
                }
            }
            offset += take as u64;
        }
        Ok(())
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

    fn claim_directory(&self, start: u32, used: &mut ClusterSet) -> Result<(), FsError> {
        let mut cluster = start;
        let mut walked = 0usize;
        loop {
            used.insert(cluster);
            walked += 1;
            if walked > self.cluster_count as usize {
                return Err(FsError::Corrupt);
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => return Ok(()),
            }
        }
    }

    /// Clears orphaned long-name entries, and points `..` at the real parent,
    /// which an interrupted move of a directory can leave wrong.
    fn fix_directory(&mut self, node: NodeId, report: &mut CheckReport) -> Result<(), FsError> {
        let entry = self.nodes.get(node).ok_or(FsError::Corrupt)?;
        let start = entry.start_cluster;
        if let Some(parent) = entry.parent {
            let parent_cluster = if parent == NodeId::ROOT.0 {
                0
            } else {
                self.nodes
                    .get(NodeId(parent))
                    .ok_or(FsError::Corrupt)?
                    .start_cluster
            };
            self.fix_dotdot(start, parent_cluster)?;
        }

        let mut buffer = try_zeroed(self.cluster_size)?;
        let mut cluster = start;
        let mut pending: Vec<u64> = Vec::new();
        let mut checksum = 0u8;
        // Sequence number the next fragment must carry. Fragments count down.
        let mut expected = 0u8;
        let mut walked = 0usize;
        'walk: loop {
            self.read_cluster(cluster, &mut buffer)?;
            let base = self.cluster_offset(cluster)?;
            for (index, slot) in buffer.as_chunks::<32>().0.iter().enumerate() {
                let offset = base + index as u64 * 32;
                if slot[0] == 0x00 {
                    self.clear_past_end(cluster, offset)?;
                    break 'walk;
                }
                if slot[0] == 0xe5 {
                    self.clear_orphans(&mut pending, report)?;
                    continue;
                }
                if slot[11] & 0x3f == 0x0f {
                    let order = slot[0] & 0x1f;
                    if slot[0] & 0x40 != 0 {
                        // A new name starts: anything pending had no owner.
                        self.clear_orphans(&mut pending, report)?;
                        checksum = slot[13];
                        expected = order.saturating_sub(1);
                        try_push(&mut pending, offset)?;
                    } else if !pending.is_empty()
                        && slot[13] == checksum
                        && order == expected
                        && expected > 0
                    {
                        expected -= 1;
                        try_push(&mut pending, offset)?;
                    } else {
                        // Out of sequence: it can't be part of any name.
                        self.clear_orphans(&mut pending, report)?;
                        try_push(&mut pending, offset)?;
                        self.clear_orphans(&mut pending, report)?;
                    }
                    continue;
                }
                if !pending.is_empty() && (expected != 0 || short_checksum(slot) != checksum) {
                    self.clear_orphans(&mut pending, report)?;
                }
                pending.clear();
            }
            walked += 1;
            if walked > self.cluster_count as usize {
                return Err(FsError::Corrupt);
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => break,
            }
        }
        self.clear_orphans(&mut pending, report)
    }

    /// Zeroes a directory past its end marker. An earlier block that didn't land
    /// reads as the end, while later ones may hold entries. Nothing sees those
    /// today, but they'd come alive when the directory next grew over the marker,
    /// pointing at clusters long since reused.
    fn clear_past_end(&mut self, cluster: u32, from: u64) -> Result<(), FsError> {
        let mut buffer = try_zeroed(self.cluster_size)?;
        let mut cluster = cluster;
        let mut first = from;
        let mut walked = 0usize;
        loop {
            self.read_cluster(cluster, &mut buffer)?;
            let base = self.cluster_offset(cluster)?;
            let within = (first - base) as usize;
            if buffer[within..].iter().any(|&byte| byte != 0) {
                buffer[within..].fill(0);
                self.volume.class = WriteClass::Dir;
                self.volume.write_bytes(first, &buffer[within..])?;
            }
            walked += 1;
            if walked > self.cluster_count as usize {
                return Err(FsError::Corrupt);
            }
            match self.next_cluster(cluster)? {
                Some(next) => {
                    cluster = next;
                    first = self.cluster_offset(next)?;
                }
                None => return Ok(()),
            }
        }
    }

    fn clear_orphans(
        &mut self,
        pending: &mut Vec<u64>,
        report: &mut CheckReport,
    ) -> Result<(), FsError> {
        if pending.is_empty() {
            return Ok(());
        }
        self.volume.class = WriteClass::Dir;
        for &offset in pending.iter() {
            self.volume.write_bytes(offset, &[0xe5])?;
        }
        report.orphan_names += 1;
        pending.clear();
        Ok(())
    }

    fn fix_dotdot(&mut self, start: u32, parent_cluster: u32) -> Result<(), FsError> {
        let base = self.cluster_offset(start)?;
        let entry = self.volume.read_array::<32>(base + 32)?;
        if entry[0] != b'.' || entry[1] != b'.' {
            return Ok(());
        }
        let stored = (entry_u16(&entry, 20) as u32) << 16 | entry_u16(&entry, 26) as u32;
        if stored != parent_cluster {
            self.volume.class = WriteClass::Dir;
            self.volume
                .write_u16_le(base + 32 + 20, (parent_cluster >> 16) as u16)?;
            self.volume
                .write_u16_le(base + 32 + 26, (parent_cluster & 0xffff) as u16)?;
        }
        Ok(())
    }

    /// Makes one file's entry and chain agree, and claims its clusters.
    fn fix_file(
        &mut self,
        node: NodeId,
        used: &mut ClusterSet,
        report: &mut CheckReport,
    ) -> Result<(), FsError> {
        let entry = self.nodes.get(node).ok_or(FsError::Corrupt)?;
        let start = entry.start_cluster;
        let size = entry.size;
        if start < 2 {
            if size != 0 {
                self.set_recovered(node, 0, 0)?;
                report.truncated_files += 1;
            }
            return Ok(());
        }
        // Already somebody's: the second name of an interrupted rename. Both
        // names hold the same bytes, so dropping this one loses nothing.
        if used.contains(start) {
            return self.drop_entry(node, report);
        }
        // A chain that starts on a free cluster has nothing behind it.
        if self.read_fat_entry(start)? == 0 {
            self.set_recovered(node, 0, 0)?;
            report.truncated_files += 1;
            return Ok(());
        }

        let cluster_size = self.cluster_size as u64;
        let needed = size.div_ceil(cluster_size);
        let mut length = 0u64;
        let mut cluster = start;
        let mut last;
        // Walk as far as the size needs, stopping at anything that isn't a link
        // or isn't this file's to use.
        loop {
            used.insert(cluster);
            last = cluster;
            length += 1;
            if length == needed.max(1) {
                break;
            }
            let value = self.read_fat_entry(cluster)?;
            if value == 0 || value >= FAT_EOC_MIN || value == FAT_BAD || value < 2 {
                break;
            }
            if value >= self.max_cluster() || used.contains(value) {
                break;
            }
            cluster = value;
        }

        if length < needed {
            // Keep what's there; the file ends where its chain does.
            self.set_recovered(node, start, length * cluster_size)?;
            report.truncated_files += 1;
        }
        // Whatever followed `last` is not this file's, and the sweep frees it.
        if self.read_fat_entry(last)? < FAT_EOC_MIN {
            self.write_fat_entry(last, FAT_EOC_MIN)?;
            if length >= needed {
                report.truncated_files += 1;
            }
        }
        Ok(())
    }

    fn set_recovered(&mut self, node: NodeId, start: u32, size: u64) -> Result<(), FsError> {
        let location = self
            .nodes
            .get(node)
            .and_then(|entry| entry.entry)
            .ok_or(FsError::Corrupt)?;
        self.volume.class = WriteClass::Dir;
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
            entry.tail = None;
            entry.chain_changed();
        }
        Ok(())
    }

    /// Frees every allocated cluster nothing claimed. Bad clusters stay marked.
    fn free_unclaimed(&mut self, used: &ClusterSet) -> Result<u32, FsError> {
        let mut lost = 0u32;
        for cluster in 2..self.max_cluster() {
            if used.contains(cluster) {
                continue;
            }
            let value = self.read_fat_entry(cluster)?;
            if value != 0 && value != FAT_BAD {
                self.write_fat_entry(cluster, 0)?;
                lost += 1;
            }
        }
        Ok(lost)
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
        let free = match self.free_count {
            Some(free) => free,
            None => self.count_free().unwrap_or(0),
        };
        VolumeStats {
            block_size: self.cluster_size as u64,
            total_blocks: self.cluster_count as u64,
            free_blocks: free as u64,
            read_only: self.damaged || self.volume.is_read_only(),
            kind: "fat32",
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
        self.nodes.lookup(dir, name)
    }
}

struct DirTask {
    node: u32,
    cluster: u32,
    /// Parents between this directory and the root.
    depth: usize,
}

pub(crate) struct LfnState {
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
    pub(crate) fn new() -> Self {
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

    pub(crate) fn reset(&mut self) {
        self.len = 0;
        self.checksum = None;
        self.active = false;
        self.first = None;
        self.entries = 0;
        self.expected = 0;
    }

    pub(crate) fn push(&mut self, entry: &[u8], offset: u64) {
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

    pub(crate) fn finish(
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
                    return Some((path_component(name), location));
                }
            }
        }

        short
            .as_ref()
            .map(|name| (path_component(name.clone()), location))
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

pub(crate) fn short_name(entry: &[u8]) -> Option<String> {
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
    match name.rsplit_once('.') {
        Some((base, extension)) if !base.is_empty() && !extension.is_empty() => {
            (String::from(base), String::from(extension))
        }
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
