//! Read-only exFAT.
//!
//! The driver parses the volume boot record (including its checksum), the FAT
//! and directory entry sets. File names are UTF-16, directory streams may be
//! contiguous (`NoFatChain`) or FAT-chained, and directory entry checksums are
//! validated in the same way the Linux exFAT driver does when walking a
//! directory.
//!
//! Layout references: Microsoft's exFAT specification, sections 3.1 (boot
//! region), 4.1 (FAT) and 6 (directory entries), plus the Linux `fs/exfat`
//! implementation for entry-set ordering.

use alloc::string::String;
use alloc::vec::Vec;

use crate::block::{BlockDevice, WritableBlockDevice};
use crate::tree::{EntryLocation, NodeTable};
use crate::volume::Volume;
use crate::{EntryKind, FileSystem, FsError, NodeId, WritableFileSystem};

const EXFAT_SIGNATURE: &[u8; 8] = b"EXFAT   ";
const FAT_MASK: u32 = 0x0fff_ffff;
const FAT_EOC_MIN: u32 = 0x0fff_fff8;
const FAT_BAD: u32 = 0x0fff_fff7;
const EXFAT_EOC: u32 = 0xffff_ffff;
const MAX_NAME_UNITS: usize = 255;

const ENTRY_END: u8 = 0x00;
const ENTRY_BITMAP: u8 = 0x81;
const ENTRY_UPCASE: u8 = 0x82;
const ENTRY_FILE: u8 = 0x85;
const ENTRY_STREAM: u8 = 0xc0;
const ENTRY_NAME: u8 = 0xc1;

/// A mounted exFAT volume.
pub struct Exfat<D: BlockDevice> {
    volume: Volume<D>,
    nodes: NodeTable,
    fat_start: u64,
    fat_size_bytes: u64,
    fat_count: u32,
    heap_start: u64,
    cluster_size: usize,
    cluster_count: u32,
    root_cluster: u32,
    backup_base: Option<u64>,
    bitmap: Option<Bitmap>,
    upcase: Option<Vec<u16>>,
    next_free: u32,
    dirty: bool,
}

impl<D: BlockDevice> Exfat<D> {
    /// Mounts an exFAT image from `device`.
    pub fn mount(device: D) -> Result<Self, FsError> {
        let volume = Volume::new(device)?;
        let boot = volume.read_array::<512>(0)?;
        if &boot[3..11] != EXFAT_SIGNATURE {
            return Err(FsError::UnsupportedVolume);
        }
        if boot[510] != 0x55 || boot[511] != 0xaa {
            return Err(FsError::InvalidBootSector);
        }

        let sector_shift = boot[108];
        let cluster_shift = boot[109];
        if !(9..=12).contains(&sector_shift) || sector_shift + cluster_shift > 25 {
            return Err(FsError::InvalidBootSector);
        }
        let sector_size = 1usize << sector_shift;
        let cluster_size = sector_size
            .checked_shl(cluster_shift as u32)
            .ok_or(FsError::InvalidBootSector)?;
        if cluster_size > 32 * 1024 * 1024 {
            return Err(FsError::InvalidBootSector);
        }

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

        verify_boot_checksum(&volume, sector_size, 0)?;
        if backup_boot_present(&volume, sector_size)? {
            verify_boot_checksum(&volume, sector_size, 12)?;
        }

        let fat_offset = u32::from_le_bytes([boot[80], boot[81], boot[82], boot[83]]) as u64;
        let fat_length = u32::from_le_bytes([boot[84], boot[85], boot[86], boot[87]]) as u64;
        let heap_offset = u32::from_le_bytes([boot[88], boot[89], boot[90], boot[91]]) as u64;
        let cluster_count = u32::from_le_bytes([boot[92], boot[93], boot[94], boot[95]]);
        let root_cluster = u32::from_le_bytes([boot[96], boot[97], boot[98], boot[99]]);

        if fat_offset == 0 || fat_length == 0 || heap_offset == 0 || cluster_count == 0 {
            return Err(FsError::InvalidBootSector);
        }
        if fat_offset.checked_add(fat_length).ok_or(FsError::Corrupt)? > heap_offset {
            return Err(FsError::Corrupt);
        }
        let heap_end = heap_offset
            .checked_add(cluster_count as u64 * (1u64 << cluster_shift))
            .ok_or(FsError::Corrupt)?;
        if heap_end > volume_length {
            return Err(FsError::Corrupt);
        }
        if cluster_count > FAT_MASK {
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

        let has_backup = backup_boot_present(&volume, sector_size)?;

        let mut fs = Self {
            volume,
            nodes: NodeTable::new(root_cluster),
            fat_start,
            fat_size_bytes: fat_bytes,
            fat_count: boot[110].max(1) as u32,
            heap_start,
            cluster_size,
            cluster_count,
            root_cluster,
            backup_base: if has_backup {
                Some(12 * sector_size as u64)
            } else {
                None
            },
            bitmap: None,
            upcase: None,
            next_free: 2,
            dirty: false,
        };

        fs.load_system_entries()?;

        let mut queue: Vec<DirTask> = Vec::new();
        queue.push(DirTask {
            node: 0,
            cluster: root_cluster,
            data_length: 0,
            contiguous: false,
        });
        let mut visited: Vec<u32> = Vec::new();
        visited.push(root_cluster);

        while let Some(task) = queue.pop() {
            fs.scan_directory(task, &mut queue, &mut visited)?;
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
            .fat_start
            .checked_add(cluster as u64 * 4)
            .ok_or(FsError::Corrupt)?;
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
        visited: &mut Vec<u32>,
    ) -> Result<(), FsError> {
        let contiguous_clusters = if task.contiguous && task.data_length > 0 {
            task.data_length
                .div_ceil(self.cluster_size as u64)
                .min(self.cluster_count as u64) as usize
        } else {
            0
        };

        let mut buffer = alloc::vec![0u8; self.cluster_size];
        let mut pending: Option<FileSet> = None;
        let mut cluster = task.cluster;
        let mut index = 0usize;
        let mut walked = 0usize;

        loop {
            self.read_cluster(cluster, &mut buffer)?;
            let cluster_start = self.cluster_offset(cluster)?;

            for (entry_index, entry) in buffer.chunks_exact(32).enumerate() {
                let offset = cluster_start + entry_index as u64 * 32;
                let entry_type = entry[0];
                if entry_type == ENTRY_END {
                    return Ok(());
                }
                if entry_type < 0x80 {
                    // Deleted or unused. It also terminates an incomplete set.
                    pending = None;
                    continue;
                }

                if entry_type == ENTRY_FILE {
                    pending = Some(FileSet::new(entry, offset));
                    if pending.as_ref().is_some_and(|set| set.remaining == 0) {
                        if let Some(set) = pending.take() {
                            self.add_entry(set, task.node, queue, visited)?;
                        }
                    }
                    continue;
                }

                let Some(set) = pending.as_mut() else {
                    continue;
                };
                if set.remaining == 0 {
                    continue;
                }
                let complete = set.push(entry, offset);
                if complete {
                    if let Some(set) = pending.take() {
                        self.add_entry(set, task.node, queue, visited)?;
                    }
                }
            }

            index += 1;
            walked += 1;
            if walked > self.cluster_count as usize {
                return Err(FsError::Corrupt);
            }

            if task.contiguous && contiguous_clusters > 0 {
                if index >= contiguous_clusters {
                    return Ok(());
                }
                let Some(next) = task.cluster.checked_add(index as u32) else {
                    return Err(FsError::Corrupt);
                };
                cluster = next;
            } else {
                match self.next_cluster(cluster)? {
                    Some(next) => cluster = next,
                    None => return Ok(()),
                }
            }
        }
    }

    fn add_entry(
        &mut self,
        set: FileSet,
        parent: u32,
        queue: &mut Vec<DirTask>,
        visited: &mut Vec<u32>,
    ) -> Result<(), FsError> {
        let Some(info) = set.finish() else {
            return Ok(());
        };

        let node = self.nodes.add(
            parent,
            info.name,
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

        if info.kind == EntryKind::Directory
            && info.start_cluster >= 2
            && info.start_cluster < self.max_cluster()
            && info.data_length > 0
            && !visited.contains(&info.start_cluster)
        {
            visited.push(info.start_cluster);
            queue.push(DirTask {
                node,
                cluster: info.start_cluster,
                data_length: info.data_length,
                contiguous: info.contiguous,
            });
        }
        Ok(())
    }

    fn read_file(&self, node: NodeId, offset: usize, out: &mut [u8]) -> usize {
        let Some(entry) = self.nodes.get(node) else {
            return 0;
        };
        if entry.kind != EntryKind::File || out.is_empty() {
            return 0;
        }

        let offset = offset as u64;
        if offset >= entry.size || entry.start_cluster < 2 {
            return 0;
        }
        let wanted = (out.len() as u64).min(entry.size - offset) as usize;
        if wanted == 0 {
            return 0;
        }

        let cluster_size = self.cluster_size as u64;
        let mut cluster_index = offset / cluster_size;
        let mut within = (offset % cluster_size) as usize;
        let mut cluster = entry.start_cluster;

        if entry.contiguous {
            let Some(target) = entry.start_cluster.checked_add(cluster_index as u32) else {
                return 0;
            };
            if target < 2 || target >= self.max_cluster() {
                return 0;
            }
            cluster = target;
            cluster_index = 0;
        }
        while cluster_index > 0 {
            let Some(next) = self.next_cluster(cluster).ok().flatten() else {
                return 0;
            };
            cluster = next;
            cluster_index -= 1;
        }

        let mut done = 0usize;
        let mut walked = 0usize;
        while done < wanted {
            let Some(cluster_offset) = self.cluster_offset(cluster).ok() else {
                return done;
            };
            let take = (self.cluster_size - within).min(wanted - done);
            if self
                .volume
                .read_bytes(cluster_offset + within as u64, &mut out[done..done + take])
                .is_err()
            {
                return done;
            }
            done += take;
            within = 0;

            if done < wanted {
                walked += 1;
                if walked > self.cluster_count as usize {
                    return done;
                }
                if entry.contiguous {
                    match cluster.checked_add(1) {
                        Some(next) if next >= 2 && next < self.max_cluster() => cluster = next,
                        _ => return done,
                    }
                } else {
                    match self.next_cluster(cluster) {
                        Ok(Some(next)) => cluster = next,
                        _ => return done,
                    }
                }
            }
        }
        done
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

    fn load_system_entries(&mut self) -> Result<(), FsError> {
        let mut bitmap_info = None;
        let mut upcase_info = None;
        let mut buffer = alloc::vec![0u8; self.cluster_size];
        let mut cluster = self.root_cluster;
        let mut walked = 0usize;

        loop {
            self.read_cluster(cluster, &mut buffer)?;
            let mut end = false;
            for entry in buffer.chunks_exact(32) {
                if entry[0] == ENTRY_END {
                    end = true;
                    break;
                }
                match entry[0] {
                    ENTRY_BITMAP => {
                        let start =
                            u32::from_le_bytes([entry[20], entry[21], entry[22], entry[23]]);
                        let length = u64::from_le_bytes([
                            entry[24], entry[25], entry[26], entry[27], entry[28], entry[29],
                            entry[30], entry[31],
                        ]);
                        if start >= 2 && length > 0 {
                            bitmap_info = Some((start, length));
                        }
                    }
                    ENTRY_UPCASE => {
                        let start =
                            u32::from_le_bytes([entry[20], entry[21], entry[22], entry[23]]);
                        let length = u64::from_le_bytes([
                            entry[24], entry[25], entry[26], entry[27], entry[28], entry[29],
                            entry[30], entry[31],
                        ]);
                        if start >= 2 && length > 0 {
                            upcase_info = Some((start, length));
                        }
                    }
                    _ => {}
                }
            }
            if end {
                break;
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

        if let Some((start, length)) = bitmap_info {
            let needed = (self.cluster_count as u64).div_ceil(8);
            if length >= needed && length <= 16 * 1024 * 1024 {
                let mut bytes = alloc::vec![0u8; length as usize];
                self.read_stream_bytes(start, false, 0, &mut bytes, length)?;
                let bitmap = Bitmap {
                    start_cluster: start,
                    bytes,
                };
                for cluster in 2..self.max_cluster() {
                    if !bitmap.is_used(cluster) {
                        self.next_free = cluster;
                        break;
                    }
                }
                self.bitmap = Some(bitmap);
            }
        }

        if let Some((start, length)) = upcase_info {
            if length <= 2 * 1024 * 1024 {
                if let Ok(table) = self.read_upcase(start, length) {
                    self.upcase = Some(table);
                }
            }
        }

        Ok(())
    }

    fn read_upcase(&self, start: u32, length: u64) -> Result<Vec<u16>, FsError> {
        let mut bytes = alloc::vec![0u8; length as usize];
        self.read_stream_bytes(start, false, 0, &mut bytes, length)?;

        let mut table: Vec<u16> = (0..=0xffffu32).map(|value| value as u16).collect();
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
                let character = char::from_u32(unit as u32).unwrap_or('\u{fffd}');
                let mut upper = character.to_uppercase();
                upper.next().unwrap_or(character) as u32 as u16
            })
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
    fn read_fat_entry(&self, cluster: u32) -> Result<u32, FsError> {
        let offset = self
            .fat_start
            .checked_add(cluster as u64 * 4)
            .ok_or(FsError::Corrupt)?;
        self.volume.read_u32_le(offset)
    }

    fn write_fat_entry(&mut self, cluster: u32, value: u32) -> Result<(), FsError> {
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
        let flags = self.volume.read_u16_le(106)?;
        let flags = if dirty {
            flags | 0x0002
        } else {
            flags & !0x0002
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
        self.write_stream_at(start, false, byte as u64, &[value])
    }

    fn alloc_cluster(&mut self) -> Result<u32, FsError> {
        if self.bitmap.is_none() {
            return Err(FsError::Unsupported);
        }
        let max_cluster = self.max_cluster();
        let start = self.next_free.clamp(2, max_cluster.saturating_sub(1));
        for step in 0..self.cluster_count {
            let cluster = 2 + ((start - 2 + step) % self.cluster_count);
            let free = match &self.bitmap {
                Some(bitmap) => !bitmap.is_used(cluster),
                None => self.read_fat_entry(cluster)? == 0,
            };
            if free {
                self.set_bitmap_bit(cluster, true)?;
                self.write_fat_entry(cluster, EXFAT_EOC)?;
                let offset = self.cluster_offset(cluster)?;
                let zeros = alloc::vec![0u8; self.cluster_size];
                self.volume.write_bytes(offset, &zeros)?;
                self.next_free = if cluster + 1 < max_cluster {
                    cluster + 1
                } else {
                    2
                };
                self.set_dirty(true)?;
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
            if next == 0 || next >= 0xffff_fff8 {
                break;
            }
            if next == 0xffff_fff7 || next < 2 || next >= max_cluster {
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
            let mut last = first;
            for _ in 1..needed {
                let next = self.alloc_cluster()?;
                self.write_fat_entry(last, next)?;
                last = next;
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
                    let zeros = alloc::vec![0u8; self.cluster_size];
                    self.volume.write_bytes(offset, &zeros)?;
                }
                self.set_dirty(true)?;
                return Ok((start, true));
            }

            let mut new_clusters = Vec::new();
            for _ in current..needed {
                new_clusters.push(self.alloc_cluster()?);
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

        let mut last = self.chain_last(start)?;
        for _ in current..needed {
            let next = self.alloc_cluster()?;
            self.write_fat_entry(last, next)?;
            last = next;
        }
        Ok((start, false))
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
                for _ in 0..location.count {
                    offsets.push(offset);
                    offset += 32;
                    let base = self.cluster_offset(current)?;
                    if offset >= base + self.cluster_size as u64 {
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
        self.volume
            .write_bytes(location.stream + 1, &[if contiguous { 0x03 } else { 0x01 }])?;
        self.volume.write_u64_le(location.stream + 8, valid_size)?;
        self.volume.write_u32_le(location.stream + 20, start)?;
        self.volume
            .write_u64_le(location.stream + 24, data_length)?;
        if let Some(entry) = self.nodes.get_mut(node) {
            entry.start_cluster = start;
            entry.data_length = data_length;
            entry.contiguous = contiguous;
            if entry.kind == EntryKind::File {
                entry.size = valid_size;
            }
        }
        self.update_set_checksum(parent, location)?;
        self.set_dirty(true)
    }

    fn update_set_checksum(
        &mut self,
        parent: NodeId,
        location: EntryLocation,
    ) -> Result<(), FsError> {
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

    fn validate_new_name(&self, parent: NodeId, name: &str) -> Result<(), FsError> {
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
        if self.lookup_name(parent, name).is_some() {
            return Err(FsError::NameExists);
        }
        Ok(())
    }

    fn name_hash(&self, name: &str) -> u16 {
        let mut hash = 0u16;
        for unit in name.encode_utf16() {
            let unit = self.upcase_unit(unit);
            hash = hash.rotate_right(1).wrapping_add(unit & 0xff);
            hash = hash.rotate_right(1).wrapping_add(unit >> 8);
        }
        hash
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

        let mut buffer = alloc::vec![0u8; self.cluster_size];
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
    ) -> Result<EntryLocation, FsError> {
        let units: Vec<u16> = name.encode_utf16().collect();
        if units.is_empty() || units.len() > MAX_NAME_UNITS {
            return Err(FsError::InvalidName);
        }
        let name_entries = units.len().div_ceil(15);
        let secondary = 1 + name_entries;
        let needed = 1 + secondary;
        let offsets = self.find_free_entries(parent, needed)?;

        let mut entries: Vec<[u8; 32]> = Vec::with_capacity(needed);
        let mut primary = [0u8; 32];
        primary[0] = ENTRY_FILE;
        primary[1] = secondary as u8;
        primary[4..6].copy_from_slice(&attributes.to_le_bytes());
        entries.push(primary);

        let mut stream = [0u8; 32];
        stream[0] = ENTRY_STREAM;
        stream[1] = if contiguous { 0x03 } else { 0x01 };
        stream[3] = units.len() as u8;
        stream[4..6].copy_from_slice(&self.name_hash(name).to_le_bytes());
        stream[8..16].copy_from_slice(&data_length.to_le_bytes());
        stream[20..24].copy_from_slice(&start_cluster.to_le_bytes());
        stream[24..32].copy_from_slice(&data_length.to_le_bytes());
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
        self.set_dirty(true)?;

        Ok(EntryLocation {
            first: offsets[0],
            primary: offsets[0],
            count: needed as u8,
            stream: offsets[1],
        })
    }

    fn delete_entry_set(&mut self, parent: NodeId, location: EntryLocation) -> Result<(), FsError> {
        let offsets = self.entry_offsets(parent, location)?;
        let mut entry_type = [0u8; 1];
        for offset in offsets {
            self.volume.read_bytes(offset, &mut entry_type)?;
            self.volume.write_bytes(offset, &[entry_type[0] & 0x7f])?;
        }
        self.set_dirty(true)
    }

    fn create_file_node(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        self.validate_new_name(parent, name)?;
        let location = self.write_entry_set(parent, name, 0x20, 0, 0, false)?;
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
        self.validate_new_name(parent, name)?;
        let cluster = self.alloc_cluster()?;
        let location = match self.write_entry_set(
            parent,
            name,
            0x10,
            cluster,
            self.cluster_size as u64,
            false,
        ) {
            Ok(location) => location,
            Err(error) => {
                self.free_cluster(cluster)?;
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
        self.free_stream(start_cluster, contiguous, data_length)?;
        self.delete_entry_set(NodeId(parent), location.ok_or(FsError::Corrupt)?)?;
        self.nodes.remove(node)?;
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
        if self.lookup_name(new_parent, new_name) == Some(node) {
            return Ok(());
        }
        self.validate_new_name(new_parent, new_name)?;

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
        let new_location = self.write_entry_set(
            new_parent,
            new_name,
            attributes,
            start_cluster,
            length,
            contiguous,
        )?;
        self.delete_entry_set(NodeId(old_parent), old_location)?;
        self.nodes
            .reparent(node, new_parent, String::from(new_name), new_location)
    }

    fn write_file(&mut self, node: NodeId, offset: usize, data: &[u8]) -> Result<usize, FsError> {
        let (old_size, start_cluster, contiguous, data_length, kind) = {
            let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
            (
                entry.size,
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
        let end = (offset as u64)
            .checked_add(data.len() as u64)
            .ok_or(FsError::NoSpace)?;
        let needed = ((end + self.cluster_size as u64 - 1) / self.cluster_size as u64) as usize;
        let (start, contiguous) =
            self.ensure_chain(start_cluster, contiguous, data_length, needed)?;
        if end > old_size {
            self.zero_stream_range(start, contiguous, old_size, end)?;
        }
        self.write_stream_at(start, contiguous, offset as u64, data)?;
        let size = old_size.max(end);
        self.update_stream_entry(node, start, size, contiguous, size)?;
        Ok(data.len())
    }

    fn set_file_len(&mut self, node: NodeId, size: usize) -> Result<(), FsError> {
        let (old_size, start_cluster, contiguous, data_length, kind) = {
            let entry = self.nodes.get(node).ok_or(FsError::NotFound)?;
            (
                entry.size,
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
        if new_size == 0 {
            self.free_stream(start_cluster, contiguous, data_length)?;
            self.update_stream_entry(node, 0, 0, false, 0)?;
            return Ok(());
        }
        let needed =
            ((new_size + self.cluster_size as u64 - 1) / self.cluster_size as u64) as usize;
        if new_size < old_size {
            let (start, contiguous) =
                self.truncate_chain(start_cluster, contiguous, data_length, needed)?;
            self.update_stream_entry(node, start, new_size, contiguous, new_size)?;
        } else {
            let (start, contiguous) =
                self.ensure_chain(start_cluster, contiguous, data_length, needed)?;
            self.zero_stream_range(start, contiguous, old_size, new_size)?;
            self.update_stream_entry(node, start, new_size, contiguous, new_size)?;
        }
        Ok(())
    }
}

impl<D: WritableBlockDevice> WritableFileSystem for Exfat<D> {
    fn create_file(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        self.create_file_node(parent, name)
    }

    fn create_dir(&mut self, parent: NodeId, name: &str) -> Result<NodeId, FsError> {
        self.create_dir_node(parent, name)
    }

    fn remove(&mut self, node: NodeId) -> Result<(), FsError> {
        self.remove_node(node)
    }

    fn rename(&mut self, node: NodeId, new_parent: NodeId, new_name: &str) -> Result<(), FsError> {
        self.rename_node(node, new_parent, new_name)
    }

    fn write(&mut self, node: NodeId, offset: usize, data: &[u8]) -> Result<usize, FsError> {
        self.write_file(node, offset, data)
    }

    fn set_len(&mut self, node: NodeId, size: usize) -> Result<(), FsError> {
        self.set_file_len(node, size)
    }

    fn flush(&mut self) -> Result<(), FsError> {
        self.set_dirty(false)?;
        self.volume.flush()
    }
}

impl<D: BlockDevice> FileSystem for Exfat<D> {
    fn node_count(&self) -> usize {
        self.nodes.len()
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
}

struct PendingEntry {
    name: String,
    kind: EntryKind,
    start_cluster: u32,
    data_length: u64,
    contiguous: bool,
    location: EntryLocation,
}

struct FileSet {
    remaining: u8,
    attributes: u16,
    flags: u8,
    name_len: usize,
    name_units: [u16; MAX_NAME_UNITS],
    name_count: usize,
    start_cluster: u32,
    data_length: u64,
    has_stream: bool,
    primary: u64,
    stream: u64,
    count: u8,
}

impl FileSet {
    fn new(entry: &[u8], offset: u64) -> Self {
        Self {
            remaining: entry[1],
            attributes: u16::from_le_bytes([entry[4], entry[5]]),
            flags: 0,
            name_len: 0,
            name_units: [0; MAX_NAME_UNITS],
            name_count: 0,
            start_cluster: 0,
            data_length: 0,
            has_stream: false,
            primary: offset,
            stream: 0,
            count: entry[1].saturating_add(1),
        }
    }

    /// Consumes one secondary entry and reports whether the set is complete.
    fn push(&mut self, entry: &[u8], offset: u64) -> bool {
        match entry[0] {
            ENTRY_STREAM => {
                self.flags = entry[1];
                self.name_len = entry[3] as usize;
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

    fn finish(self) -> Option<PendingEntry> {
        if !self.has_stream || self.name_len == 0 {
            return None;
        }
        if self.name_len > self.name_count || self.name_len > MAX_NAME_UNITS {
            return None;
        }

        let name = String::from_utf16_lossy(&self.name_units[..self.name_len]);
        if name.is_empty() || name == "." || name == ".." {
            return None;
        }
        let name = if name.contains('/') || name.contains('\\') {
            name.replace(['/', '\\'], "_")
        } else {
            name
        };

        let kind = if self.attributes & 0x10 != 0 {
            EntryKind::Directory
        } else {
            EntryKind::File
        };
        let contiguous = self.flags & 0x03 == 0x03;

        Some(PendingEntry {
            name,
            kind,
            start_cluster: self.start_cluster,
            data_length: self.data_length,
            contiguous,
            location: EntryLocation {
                first: self.primary,
                primary: self.primary,
                count: self.count,
                stream: self.stream,
            },
        })
    }
}

fn backup_boot_present<D: BlockDevice>(
    volume: &Volume<D>,
    sector_size: usize,
) -> Result<bool, FsError> {
    let offset = (12 * sector_size as u64)
        .checked_add(3)
        .ok_or(FsError::Corrupt)?;
    let mut signature = [0u8; 8];
    if volume.read_bytes(offset, &mut signature).is_err() {
        return Ok(false);
    }
    Ok(&signature == EXFAT_SIGNATURE)
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
    let mut region = alloc::vec![0u8; region_len];
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
