//! In-memory FAT32 and exFAT volumes for tests, with any sector and cluster size.
//!
//! A FAT32 volume needs at least 65,525 clusters, which is 256 MiB at 4 KiB per
//! cluster. [`SparseDisk`] stores only the blocks that were written, so those
//! geometries cost almost nothing and fit under the fuzz tests' heap limit.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use feos_fs::{BlockDevice, FsError, WritableBlockDevice};

/// Shared handles to a disk's counters, which stay usable after a mount takes
/// the disk itself.
#[derive(Clone, Default)]
pub struct DiskProbe {
    /// Number of block reads so far.
    pub reads: Arc<AtomicU64>,
    /// Writes allowed before failing. Negative means no limit.
    pub writes_left: Arc<AtomicI64>,
}

impl DiskProbe {
    pub fn read_count(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    pub fn fail_writes_after(&self, count: i64) {
        self.writes_left.store(count, Ordering::Relaxed);
    }
}

/// A block device that stores only non-default blocks.
pub struct SparseDisk {
    pub block_size: usize,
    pub block_count: u64,
    pub blocks: BTreeMap<u64, Vec<u8>>,
    pub probe: DiskProbe,
}

impl SparseDisk {
    pub fn new(bytes: u64, block_size: usize) -> Self {
        let probe = DiskProbe::default();
        probe.fail_writes_after(-1);
        Self {
            block_size,
            block_count: bytes / block_size as u64,
            blocks: BTreeMap::new(),
            probe,
        }
    }

    pub fn write_at(&mut self, offset: u64, data: &[u8]) {
        let block_size = self.block_size as u64;
        let mut done = 0usize;
        while done < data.len() {
            let position = offset + done as u64;
            let within = (position % block_size) as usize;
            let take = (self.block_size - within).min(data.len() - done);
            let block = self
                .blocks
                .entry(position / block_size)
                .or_insert_with(|| vec![0u8; self.block_size]);
            block[within..within + take].copy_from_slice(&data[done..done + take]);
            done += take;
        }
    }

    pub fn read_at(&self, offset: u64, out: &mut [u8]) {
        let block_size = self.block_size as u64;
        let mut done = 0usize;
        while done < out.len() {
            let position = offset + done as u64;
            let within = (position % block_size) as usize;
            let take = (self.block_size - within).min(out.len() - done);
            match self.blocks.get(&(position / block_size)) {
                Some(block) => {
                    out[done..done + take].copy_from_slice(&block[within..within + take])
                }
                None => out[done..done + take].fill(0),
            }
            done += take;
        }
    }

    pub fn u32_at(&self, offset: u64) -> u32 {
        let mut bytes = [0u8; 4];
        self.read_at(offset, &mut bytes);
        u32::from_le_bytes(bytes)
    }
}

impl BlockDevice for SparseDisk {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), FsError> {
        if out.len() != self.block_size {
            return Err(FsError::InvalidBuffer);
        }
        if block >= self.block_count {
            return Err(FsError::OutOfBounds);
        }
        self.probe.reads.fetch_add(1, Ordering::Relaxed);
        match self.blocks.get(&block) {
            Some(data) => out.copy_from_slice(data),
            None => out.fill(0),
        }
        Ok(())
    }
}

impl WritableBlockDevice for SparseDisk {
    fn write_block(&mut self, block: u64, data: &[u8]) -> Result<(), FsError> {
        if data.len() != self.block_size {
            return Err(FsError::InvalidBuffer);
        }
        if block >= self.block_count {
            return Err(FsError::OutOfBounds);
        }
        let left = self.probe.writes_left.load(Ordering::Relaxed);
        if left == 0 {
            return Err(FsError::Io);
        }
        if left > 0 {
            self.probe.writes_left.store(left - 1, Ordering::Relaxed);
        }
        self.blocks.insert(block, data.to_vec());
        Ok(())
    }
}

/// A short (8.3) directory entry.
pub fn short_entry(name: &[u8; 11], attributes: u8, cluster: u32, size: u32) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[..11].copy_from_slice(name);
    entry[11] = attributes;
    entry[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
    entry
}

/// The checksum a long-name entry stores for its short entry.
pub fn lfn_checksum(short: &[u8]) -> u8 {
    short[..11]
        .iter()
        .fold(0u8, |sum, &byte| sum.rotate_right(1).wrapping_add(byte))
}

/// Long-name entries for `name`, in on-disk order.
pub fn lfn_entries(name: &str, short: &[u8]) -> Vec<[u8; 32]> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let count = (units.len() + 1).div_ceil(13);
    let checksum = lfn_checksum(short);
    let mut entries = Vec::new();
    for sequence in (1..=count).rev() {
        let mut entry = [0u8; 32];
        entry[0] = sequence as u8 | if sequence == count { 0x40 } else { 0 };
        entry[11] = 0x0f;
        entry[13] = checksum;
        for index in 0..13 {
            let at = (sequence - 1) * 13 + index;
            let unit = match at.cmp(&units.len()) {
                std::cmp::Ordering::Less => units[at],
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 0xffff,
            };
            let offset = match index {
                0..=4 => 1 + index * 2,
                5..=10 => 14 + (index - 5) * 2,
                _ => 28 + (index - 11) * 2,
            };
            entry[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
        entries.push(entry);
    }
    entries
}

/// Geometry of a FAT32 volume.
#[derive(Clone, Copy)]
pub struct FatGeometry {
    pub sector: u32,
    pub sectors_per_cluster: u32,
    /// Logical block size of the device under the filesystem.
    pub block: usize,
}

const FAT_RESERVED: u32 = 32;
const FAT_CLUSTERS: u32 = 65_600;
pub const FAT_ROOT: u32 = 2;
pub const FAT_END: u32 = 0x0fff_ffff;

/// A FAT32 volume under construction.
pub struct FatImage {
    pub disk: SparseDisk,
    pub geometry: FatGeometry,
    pub fat_sectors: u32,
    pub fat_copies: u32,
    pub clusters: u32,
    next_cluster: u32,
    next_slot: BTreeMap<u32, u32>,
}

impl FatImage {
    pub fn new(geometry: FatGeometry) -> Self {
        let sector = geometry.sector;
        let fat_sectors = ((FAT_CLUSTERS + 2) * 4).div_ceil(sector);
        let fat_copies = 2;
        let total =
            FAT_RESERVED + fat_copies * fat_sectors + FAT_CLUSTERS * geometry.sectors_per_cluster;
        let mut disk = SparseDisk::new(total as u64 * sector as u64, geometry.block);

        let mut boot = [0u8; 512];
        boot[..3].copy_from_slice(&[0xeb, 0x58, 0x90]);
        boot[3..11].copy_from_slice(b"FEOSTEST");
        boot[11..13].copy_from_slice(&(sector as u16).to_le_bytes());
        boot[13] = geometry.sectors_per_cluster as u8;
        boot[14..16].copy_from_slice(&(FAT_RESERVED as u16).to_le_bytes());
        boot[16] = fat_copies as u8;
        boot[21] = 0xf8;
        boot[32..36].copy_from_slice(&total.to_le_bytes());
        boot[36..40].copy_from_slice(&fat_sectors.to_le_bytes());
        boot[44..48].copy_from_slice(&FAT_ROOT.to_le_bytes());
        boot[48..50].copy_from_slice(&1u16.to_le_bytes());
        boot[50..52].copy_from_slice(&6u16.to_le_bytes());
        boot[82..90].copy_from_slice(b"FAT32   ");
        boot[510] = 0x55;
        boot[511] = 0xaa;
        disk.write_at(0, &boot);
        disk.write_at(6 * sector as u64, &boot);

        let mut fsinfo = [0u8; 512];
        fsinfo[..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
        fsinfo[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
        fsinfo[488..492].copy_from_slice(&u32::MAX.to_le_bytes());
        fsinfo[492..496].copy_from_slice(&u32::MAX.to_le_bytes());
        fsinfo[508..512].copy_from_slice(&0xaa55_0000u32.to_le_bytes());
        disk.write_at(sector as u64, &fsinfo);
        disk.write_at(7 * sector as u64, &fsinfo);

        let mut image = Self {
            disk,
            geometry,
            fat_sectors,
            fat_copies,
            clusters: FAT_CLUSTERS,
            next_cluster: FAT_ROOT + 1,
            next_slot: BTreeMap::new(),
        };
        image.set_fat(0, 0x0fff_fff8);
        image.set_fat(1, FAT_END);
        image.set_fat(FAT_ROOT, FAT_END);
        image
    }

    pub fn cluster_size(&self) -> u64 {
        self.geometry.sector as u64 * self.geometry.sectors_per_cluster as u64
    }

    pub fn fat_offset(&self, copy: u32, cluster: u32) -> u64 {
        (FAT_RESERVED + copy * self.fat_sectors) as u64 * self.geometry.sector as u64
            + cluster as u64 * 4
    }

    pub fn set_fat(&mut self, cluster: u32, value: u32) {
        for copy in 0..self.fat_copies {
            let offset = self.fat_offset(copy, cluster);
            self.disk.write_at(offset, &value.to_le_bytes());
        }
    }

    pub fn cluster_offset(&self, cluster: u32) -> u64 {
        let data = (FAT_RESERVED + self.fat_copies * self.fat_sectors) as u64;
        data * self.geometry.sector as u64 + (cluster as u64 - 2) * self.cluster_size()
    }

    pub fn alloc_cluster(&mut self) -> u32 {
        // Skip one so that no chain is accidentally contiguous.
        let cluster = self.next_cluster;
        self.next_cluster += 2;
        self.set_fat(cluster, FAT_END);
        cluster
    }

    /// Appends raw entries to a directory, growing it as needed.
    pub fn push_entries(&mut self, dir: u32, entries: &[[u8; 32]]) -> u64 {
        let per_cluster = (self.cluster_size() / 32) as u32;
        let mut first = None;
        for entry in entries {
            let slot = *self.next_slot.entry(dir).or_insert(0);
            let mut cluster = dir;
            for _ in 0..slot / per_cluster {
                let next = self.disk.u32_at(self.fat_offset(0, cluster));
                cluster = if next == FAT_END {
                    let grown = self.alloc_cluster();
                    self.set_fat(cluster, grown);
                    grown
                } else {
                    next
                };
            }
            let offset = self.cluster_offset(cluster) + (slot % per_cluster) as u64 * 32;
            self.disk.write_at(offset, entry);
            first.get_or_insert(offset);
            self.next_slot.insert(dir, slot + 1);
        }
        first.unwrap_or(0)
    }

    /// Writes `data` into a new chain, returning its first cluster.
    pub fn write_chain(&mut self, data: &[u8]) -> u32 {
        let cluster_size = self.cluster_size() as usize;
        let count = data.len().div_ceil(cluster_size).max(1);
        let mut first = 0;
        let mut last = 0;
        for index in 0..count {
            let cluster = self.alloc_cluster();
            if index == 0 {
                first = cluster;
            } else {
                self.set_fat(last, cluster);
            }
            let start = index * cluster_size;
            let end = data.len().min(start + cluster_size);
            if start < end {
                let offset = self.cluster_offset(cluster);
                self.disk.write_at(offset, &data[start..end]);
            }
            last = cluster;
        }
        first
    }

    pub fn add_file(&mut self, dir: u32, short: &[u8; 11], data: &[u8]) -> u32 {
        let cluster = if data.is_empty() {
            0
        } else {
            self.write_chain(data)
        };
        self.push_entries(dir, &[short_entry(short, 0x20, cluster, data.len() as u32)]);
        cluster
    }

    pub fn add_long_file(&mut self, dir: u32, name: &str, short: &[u8; 11], data: &[u8]) -> u32 {
        let cluster = self.write_chain(data);
        let mut entries = lfn_entries(name, short);
        entries.push(short_entry(short, 0x20, cluster, data.len() as u32));
        self.push_entries(dir, &entries);
        cluster
    }

    pub fn add_dir(&mut self, dir: u32, short: &[u8; 11]) -> u32 {
        let cluster = self.alloc_cluster();
        self.push_entries(dir, &[short_entry(short, 0x10, cluster, 0)]);
        self.push_entries(
            cluster,
            &[
                short_entry(b".          ", 0x10, cluster, 0),
                short_entry(b"..         ", 0x10, dir, 0),
            ],
        );
        cluster
    }

    /// The entry offset of the next free slot in a directory.
    pub fn next_entry_offset(&self, dir: u32) -> u64 {
        let slot = self.next_slot.get(&dir).copied().unwrap_or(0);
        self.cluster_offset(dir) + slot as u64 * 32
    }

    /// Marks every cluster used except the `free` ones with the highest numbers.
    pub fn exhaust_except(&mut self, free: u32) {
        let last = self.clusters + 2;
        for cluster in self.next_cluster..last - free {
            self.set_fat(cluster, FAT_END);
        }
    }
}

/// The usual 512-byte layout.
pub const FAT_512: FatGeometry = FatGeometry {
    sector: 512,
    sectors_per_cluster: 1,
    block: 512,
};

/// Eight sectors per cluster on a 512-byte device.
pub const FAT_MULTI: FatGeometry = FatGeometry {
    sector: 512,
    sectors_per_cluster: 8,
    block: 512,
};

/// 4 KiB sectors on a device with 4 KiB blocks.
pub const FAT_4K: FatGeometry = FatGeometry {
    sector: 4096,
    sectors_per_cluster: 1,
    block: 4096,
};

/// 4 KiB filesystem sectors on a 512-byte device.
pub const FAT_4K_ON_512: FatGeometry = FatGeometry {
    sector: 4096,
    sectors_per_cluster: 2,
    block: 512,
};

/// Geometry of an exFAT volume.
#[derive(Clone, Copy)]
pub struct ExfatGeometry {
    pub sector_shift: u32,
    pub cluster_shift: u32,
    pub block: usize,
    pub clusters: u32,
}

pub const EXFAT_512: ExfatGeometry = ExfatGeometry {
    sector_shift: 9,
    cluster_shift: 0,
    block: 512,
    clusters: 512,
};

pub const EXFAT_MULTI: ExfatGeometry = ExfatGeometry {
    sector_shift: 9,
    cluster_shift: 3,
    block: 512,
    clusters: 256,
};

pub const EXFAT_4K: ExfatGeometry = ExfatGeometry {
    sector_shift: 12,
    cluster_shift: 0,
    block: 4096,
    clusters: 256,
};

const EXFAT_FAT_OFFSET: u32 = 24;
pub const EXFAT_ROOT: u32 = 4;
const EXFAT_BITMAP: u32 = 2;
const EXFAT_UPCASE: u32 = 3;
pub const EXFAT_END: u32 = 0xffff_ffff;

/// The table this module writes: ASCII and Latin-1 letters map to capitals.
pub fn upcase(unit: u16) -> u16 {
    match unit {
        0x61..=0x7a | 0xe0..=0xf6 | 0xf8..=0xfe => unit - 0x20,
        other => other,
    }
}

pub fn name_hash(name: &str) -> u16 {
    let mut hash = 0u16;
    for unit in name.encode_utf16() {
        let unit = upcase(unit);
        hash = hash.rotate_right(1).wrapping_add(unit & 0xff);
        hash = hash.rotate_right(1).wrapping_add(unit >> 8);
    }
    hash
}

pub fn set_checksum(entries: &[[u8; 32]]) -> u16 {
    let mut checksum = 0u16;
    for (entry_index, entry) in entries.iter().enumerate() {
        for (byte_index, &byte) in entry.iter().enumerate() {
            if entry_index == 0 && (byte_index == 2 || byte_index == 3) {
                continue;
            }
            checksum = checksum.rotate_right(1).wrapping_add(byte as u16);
        }
    }
    checksum
}

fn boot_checksum(region: &[u8]) -> u32 {
    let mut checksum = 0u32;
    for (index, &byte) in region.iter().enumerate() {
        if index == 106 || index == 107 || index == 112 {
            continue;
        }
        checksum = checksum.rotate_right(1).wrapping_add(byte as u32);
    }
    checksum
}

/// Options for one exFAT file entry set.
#[derive(Clone, Copy)]
pub struct ExfatEntry<'a> {
    pub name: &'a str,
    pub attributes: u16,
    pub cluster: u32,
    pub valid_length: u64,
    pub data_length: u64,
    pub contiguous: bool,
}

/// Builds an entry set with a correct name hash and checksum.
pub fn exfat_entry_set(file: ExfatEntry<'_>) -> Vec<[u8; 32]> {
    let units: Vec<u16> = file.name.encode_utf16().collect();
    let name_entries = units.len().div_ceil(15);
    let mut primary = [0u8; 32];
    primary[0] = 0x85;
    primary[1] = (1 + name_entries) as u8;
    primary[4..6].copy_from_slice(&file.attributes.to_le_bytes());
    primary[8..12].copy_from_slice(&0x5a21_1234u32.to_le_bytes());
    primary[12..16].copy_from_slice(&0x5a21_5678u32.to_le_bytes());
    primary[16..20].copy_from_slice(&0x5a21_9abcu32.to_le_bytes());

    let mut stream = [0u8; 32];
    stream[0] = 0xc0;
    stream[1] = if file.contiguous { 0x03 } else { 0x01 };
    stream[3] = units.len() as u8;
    stream[4..6].copy_from_slice(&name_hash(file.name).to_le_bytes());
    stream[8..16].copy_from_slice(&file.valid_length.to_le_bytes());
    stream[20..24].copy_from_slice(&file.cluster.to_le_bytes());
    stream[24..32].copy_from_slice(&file.data_length.to_le_bytes());

    let mut entries = vec![primary, stream];
    for chunk in units.chunks(15) {
        let mut entry = [0u8; 32];
        entry[0] = 0xc1;
        for (index, unit) in chunk.iter().enumerate() {
            entry[2 + index * 2..4 + index * 2].copy_from_slice(&unit.to_le_bytes());
        }
        entries.push(entry);
    }
    let checksum = set_checksum(&entries);
    entries[0][2..4].copy_from_slice(&checksum.to_le_bytes());
    entries
}

/// An exFAT volume under construction.
pub struct ExfatImage {
    pub disk: SparseDisk,
    pub geometry: ExfatGeometry,
    pub heap_offset: u32,
    next_cluster: u32,
    next_slot: BTreeMap<u32, u32>,
    bitmap: Vec<u8>,
    pub upcase_checksum_offset: u64,
    pub bitmap_entry_offset: u64,
}

impl ExfatImage {
    pub fn new(geometry: ExfatGeometry) -> Self {
        let sector = 1u64 << geometry.sector_shift;
        let spc = 1u32 << geometry.cluster_shift;
        let fat_sectors = ((geometry.clusters as u64 + 2) * 4).div_ceil(sector) as u32;
        let heap_offset = (EXFAT_FAT_OFFSET + fat_sectors).next_multiple_of(spc);
        let total = heap_offset as u64 + geometry.clusters as u64 * spc as u64;
        let disk = SparseDisk::new(total * sector, geometry.block);

        let mut region = vec![0u8; 11 * sector as usize];
        {
            let boot = &mut region[..512];
            boot[..3].copy_from_slice(&[0xeb, 0x76, 0x90]);
            boot[3..11].copy_from_slice(b"EXFAT   ");
            boot[72..80].copy_from_slice(&total.to_le_bytes());
            boot[80..84].copy_from_slice(&EXFAT_FAT_OFFSET.to_le_bytes());
            boot[84..88].copy_from_slice(&fat_sectors.to_le_bytes());
            boot[88..92].copy_from_slice(&heap_offset.to_le_bytes());
            boot[92..96].copy_from_slice(&geometry.clusters.to_le_bytes());
            boot[96..100].copy_from_slice(&EXFAT_ROOT.to_le_bytes());
            boot[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
            boot[108] = geometry.sector_shift as u8;
            boot[109] = geometry.cluster_shift as u8;
            boot[110] = 1;
            boot[510] = 0x55;
            boot[511] = 0xaa;
        }
        let mut image = Self {
            disk,
            geometry,
            heap_offset,
            next_cluster: EXFAT_ROOT + 1,
            next_slot: BTreeMap::new(),
            bitmap: vec![0u8; (geometry.clusters as usize).div_ceil(8)],
            upcase_checksum_offset: 0,
            bitmap_entry_offset: 0,
        };
        image.write_boot_region(0, &region);
        image.write_boot_region(12, &region);

        image.set_fat(0, 0xffff_fff8);
        image.set_fat(1, EXFAT_END);
        for cluster in [EXFAT_BITMAP, EXFAT_UPCASE, EXFAT_ROOT] {
            image.set_fat(cluster, EXFAT_END);
            image.mark_used(cluster);
        }

        // An up-case table with its identity runs compressed, as formatters
        // write it. The whole table must fit one cluster.
        let table = compressed_upcase_table();
        let checksum = table.iter().fold(0u32, |sum, &byte| {
            sum.rotate_right(1).wrapping_add(byte as u32)
        });
        let upcase_offset = image.cluster_offset(EXFAT_UPCASE);
        image.disk.write_at(upcase_offset, &table);

        let mut bitmap_entry = [0u8; 32];
        bitmap_entry[0] = 0x81;
        bitmap_entry[20..24].copy_from_slice(&EXFAT_BITMAP.to_le_bytes());
        bitmap_entry[24..32].copy_from_slice(&(image.bitmap.len() as u64).to_le_bytes());
        let mut upcase_entry = [0u8; 32];
        upcase_entry[0] = 0x82;
        upcase_entry[4..8].copy_from_slice(&checksum.to_le_bytes());
        upcase_entry[20..24].copy_from_slice(&EXFAT_UPCASE.to_le_bytes());
        upcase_entry[24..32].copy_from_slice(&(table.len() as u64).to_le_bytes());
        let root = image.cluster_offset(EXFAT_ROOT);
        image.bitmap_entry_offset = root;
        image.upcase_checksum_offset = root + 32 + 4;
        image.push_entries(EXFAT_ROOT, &[bitmap_entry, upcase_entry]);
        image.flush_bitmap();
        image
    }

    fn write_boot_region(&mut self, base_sector: u64, region: &[u8]) {
        let sector = 1u64 << self.geometry.sector_shift;
        let base = base_sector * sector;
        self.disk.write_at(base, region);
        let checksum = boot_checksum(region);
        let mut stored = vec![0u8; sector as usize];
        for chunk in stored.chunks_exact_mut(4) {
            chunk.copy_from_slice(&checksum.to_le_bytes());
        }
        self.disk.write_at(base + 11 * sector, &stored);
    }

    pub fn cluster_size(&self) -> u64 {
        1u64 << (self.geometry.sector_shift + self.geometry.cluster_shift)
    }

    pub fn fat_offset(&self, cluster: u32) -> u64 {
        ((EXFAT_FAT_OFFSET as u64) << self.geometry.sector_shift) + cluster as u64 * 4
    }

    pub fn set_fat(&mut self, cluster: u32, value: u32) {
        let offset = self.fat_offset(cluster);
        self.disk.write_at(offset, &value.to_le_bytes());
    }

    pub fn cluster_offset(&self, cluster: u32) -> u64 {
        ((self.heap_offset as u64) << self.geometry.sector_shift)
            + (cluster as u64 - 2) * self.cluster_size()
    }

    pub fn mark_used(&mut self, cluster: u32) {
        let index = (cluster - 2) as usize;
        self.bitmap[index / 8] |= 1 << (index % 8);
    }

    pub fn flush_bitmap(&mut self) {
        let offset = self.cluster_offset(EXFAT_BITMAP);
        let bitmap = self.bitmap.clone();
        self.disk.write_at(offset, &bitmap);
    }

    pub fn alloc_cluster(&mut self) -> u32 {
        let cluster = self.next_cluster;
        self.next_cluster += 2;
        self.set_fat(cluster, EXFAT_END);
        self.mark_used(cluster);
        self.flush_bitmap();
        cluster
    }

    pub fn push_entries(&mut self, dir: u32, entries: &[[u8; 32]]) -> u64 {
        let per_cluster = (self.cluster_size() / 32) as u32;
        let mut first = None;
        for entry in entries {
            let slot = *self.next_slot.entry(dir).or_insert(0);
            let mut cluster = dir;
            for _ in 0..slot / per_cluster {
                let next = self.disk.u32_at(self.fat_offset(cluster));
                cluster = if next == EXFAT_END {
                    let grown = self.alloc_cluster();
                    self.set_fat(cluster, grown);
                    grown
                } else {
                    next
                };
            }
            let offset = self.cluster_offset(cluster) + (slot % per_cluster) as u64 * 32;
            self.disk.write_at(offset, entry);
            first.get_or_insert(offset);
            self.next_slot.insert(dir, slot + 1);
        }
        first.unwrap_or(0)
    }

    pub fn write_chain(&mut self, data: &[u8]) -> u32 {
        let cluster_size = self.cluster_size() as usize;
        let count = data.len().div_ceil(cluster_size).max(1);
        let mut first = 0;
        let mut last = 0;
        for index in 0..count {
            let cluster = self.alloc_cluster();
            if index == 0 {
                first = cluster;
            } else {
                self.set_fat(last, cluster);
            }
            let start = index * cluster_size;
            let end = data.len().min(start + cluster_size);
            if start < end {
                let offset = self.cluster_offset(cluster);
                self.disk.write_at(offset, &data[start..end]);
            }
            last = cluster;
        }
        first
    }

    /// Writes `data` into consecutive clusters with no FAT links.
    pub fn write_contiguous(&mut self, data: &[u8]) -> u32 {
        let cluster_size = self.cluster_size() as usize;
        let count = data.len().div_ceil(cluster_size).max(1) as u32;
        let first = self.next_cluster;
        self.next_cluster += count + 1;
        for cluster in first..first + count {
            self.mark_used(cluster);
        }
        self.flush_bitmap();
        let offset = self.cluster_offset(first);
        self.disk.write_at(offset, data);
        first
    }

    /// Returns the offset of the set's first entry.
    pub fn add_file(&mut self, dir: u32, name: &str, data: &[u8]) -> u64 {
        let cluster = if data.is_empty() {
            0
        } else {
            self.write_chain(data)
        };
        self.push_entries(
            dir,
            &exfat_entry_set(ExfatEntry {
                name,
                attributes: 0x20,
                cluster,
                valid_length: data.len() as u64,
                data_length: data.len() as u64,
                contiguous: false,
            }),
        )
    }

    pub fn add_contiguous_file(&mut self, dir: u32, name: &str, data: &[u8]) -> u64 {
        let cluster = self.write_contiguous(data);
        self.push_entries(
            dir,
            &exfat_entry_set(ExfatEntry {
                name,
                attributes: 0x20,
                cluster,
                valid_length: data.len() as u64,
                data_length: data.len() as u64,
                contiguous: true,
            }),
        )
    }

    pub fn add_dir(&mut self, dir: u32, name: &str) -> u32 {
        let cluster = self.alloc_cluster();
        let length = self.cluster_size();
        self.push_entries(
            dir,
            &exfat_entry_set(ExfatEntry {
                name,
                attributes: 0x10,
                cluster,
                valid_length: length,
                data_length: length,
                contiguous: false,
            }),
        );
        cluster
    }

    /// Recomputes the boot checksum after an edit to the main region.
    pub fn reseal_boot(&mut self, base_sector: u64) {
        let sector = 1u64 << self.geometry.sector_shift;
        let mut region = vec![0u8; 11 * sector as usize];
        self.disk.read_at(base_sector * sector, &mut region);
        self.write_boot_region(base_sector, &region);
    }
}

/// An up-case table in the compressed form the format defines: a mapping for
/// each changed unit, and `0xffff, count` for each identity run.
fn compressed_upcase_table() -> Vec<u8> {
    let mut units: Vec<u16> = Vec::new();
    let mut unit = 0u32;
    while unit <= 0xffff {
        let mapped = upcase(unit as u16);
        if mapped != unit as u16 {
            units.push(mapped);
            unit += 1;
            continue;
        }
        let mut run = 1u32;
        while unit + run <= 0xffff && upcase((unit + run) as u16) == (unit + run) as u16 {
            run += 1;
        }
        if run < 3 {
            for offset in 0..run {
                units.push((unit + offset) as u16);
            }
        } else {
            units.push(0xffff);
            units.push(run.min(0xffff) as u16);
            run = run.min(0xffff);
        }
        unit += run;
    }
    units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
}
