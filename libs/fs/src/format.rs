//! Writes an empty filesystem onto a device.

use crate::block::{WritableBlockDevice, WriteClass};
use crate::tree::try_zeroed;
use crate::FsError;

/// Fewest clusters a FAT32 volume may have: fewer, and every other driver would
/// take it for FAT16 whatever the boot sector says.
const FAT32_MIN_CLUSTERS: u64 = 65_525;
/// Most clusters a 28-bit entry can address, less the reserved values.
const FAT32_MAX_CLUSTERS: u64 = 0x0fff_fff5;
const RESERVED_SECTORS: u64 = 32;
const SECTOR: u64 = 512;
const ROOT_CLUSTER: u32 = 2;
const CLEAR_CHUNK: usize = 64 * 1024;

/// Writes zeros over a range of blocks, as a whole-number-of-blocks transfer.
fn clear<D: WritableBlockDevice>(device: &mut D, start: u64, sectors: u64) -> Result<(), FsError> {
    let chunk = try_zeroed(CLEAR_CHUNK)?;
    let per_chunk = CLEAR_CHUNK as u64 / SECTOR;
    let mut done = 0u64;
    while done < sectors {
        let count = (sectors - done).min(per_chunk);
        device.write_classified(
            start + done,
            &chunk[..(count * SECTOR) as usize],
            WriteClass::Data,
        )?;
        done += count;
    }
    Ok(())
}

/// Sectors per cluster for a volume this size, as Microsoft's table has it, but
/// never below 4 KiB where the volume is big enough: that matches a cache line,
/// so one file's update doesn't share a line with another's.
fn sectors_per_cluster(total: u64) -> u64 {
    if total >= 8 * (FAT32_MIN_CLUSTERS + 4096) {
        match total {
            0..=16_777_215 => 8,
            16_777_216..=33_554_431 => 16,
            33_554_432..=67_108_863 => 32,
            _ => 64,
        }
    } else {
        1
    }
}

/// Formats `device` as an empty FAT32 volume called `label`.
///
/// Needs at least about 33 MiB, and 512-byte blocks. Existing contents are lost;
/// it's for the caller to be sure.
pub fn format_fat32<D: WritableBlockDevice>(device: &mut D, label: &str) -> Result<(), FsError> {
    if device.block_size() != SECTOR as usize {
        return Err(FsError::UnsupportedBlockSize);
    }
    if device.is_read_only() {
        return Err(FsError::ReadOnly);
    }
    let total = device.block_count().min(u32::MAX as u64);
    let spc = sectors_per_cluster(total);

    // Size the FAT for every sector being a cluster: slightly generous, as the FAT
    // itself takes some, but always enough. Too small is a corrupt volume.
    let fat_sectors = ((total / spc + 2) * 4).div_ceil(SECTOR);
    // Start the data area on a cluster boundary, so clusters are cache lines.
    let reserved = (RESERVED_SECTORS + 2 * fat_sectors).next_multiple_of(spc) - 2 * fat_sectors;
    let clusters = total.saturating_sub(reserved + 2 * fat_sectors) / spc;
    if clusters < FAT32_MIN_CLUSTERS {
        return Err(FsError::NoSpace);
    }
    if clusters > FAT32_MAX_CLUSTERS {
        return Err(FsError::Unsupported);
    }

    let mut boot = [0u8; 512];
    boot[..3].copy_from_slice(&[0xeb, 0x58, 0x90]);
    boot[3..11].copy_from_slice(b"FEOS    ");
    boot[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    boot[13] = spc as u8;
    boot[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
    boot[16] = 2;
    boot[21] = 0xf8;
    boot[24..26].copy_from_slice(&63u16.to_le_bytes());
    boot[26..28].copy_from_slice(&255u16.to_le_bytes());
    boot[32..36].copy_from_slice(&(total as u32).to_le_bytes());
    boot[36..40].copy_from_slice(&(fat_sectors as u32).to_le_bytes());
    boot[44..48].copy_from_slice(&ROOT_CLUSTER.to_le_bytes());
    boot[48..50].copy_from_slice(&1u16.to_le_bytes());
    boot[50..52].copy_from_slice(&6u16.to_le_bytes());
    boot[64] = 0x80;
    boot[66] = 0x29;
    // Good enough to tell two volumes apart: this kernel has no entropy source.
    let serial = (total as u32).wrapping_mul(0x9e37_79b9) ^ 0x4645_4f53;
    boot[67..71].copy_from_slice(&serial.to_le_bytes());
    let mut name = [b' '; 11];
    for (slot, byte) in name.iter_mut().zip(label.bytes()) {
        *slot = byte.to_ascii_uppercase();
    }
    boot[71..82].copy_from_slice(&name);
    boot[82..90].copy_from_slice(b"FAT32   ");
    boot[510] = 0x55;
    boot[511] = 0xaa;

    let mut fsinfo = [0u8; 512];
    fsinfo[..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    fsinfo[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    // The root directory has the first cluster.
    fsinfo[488..492].copy_from_slice(&((clusters - 1) as u32).to_le_bytes());
    fsinfo[492..496].copy_from_slice(&(ROOT_CLUSTER + 1).to_le_bytes());
    fsinfo[508..512].copy_from_slice(&0xaa55_0000u32.to_le_bytes());

    // Clear what a reader would trust before the boot sector says it's valid.
    clear(device, 0, reserved)?;
    let data_start = reserved + 2 * fat_sectors;
    clear(device, reserved, 2 * fat_sectors)?;
    clear(device, data_start, spc)?;

    // Checkers expect the root to agree with the boot sector about the label.
    let mut root = [0u8; 512];
    root[..11].copy_from_slice(&name);
    root[11] = 0x08;
    device.write_classified(data_start, &root, WriteClass::Data)?;

    let mut first_fat = [0u8; 512];
    first_fat[..4].copy_from_slice(&0x0fff_fff8u32.to_le_bytes());
    first_fat[4..8].copy_from_slice(&0x0fff_ffffu32.to_le_bytes());
    first_fat[8..12].copy_from_slice(&0x0fff_ffffu32.to_le_bytes());
    device.write_classified(reserved, &first_fat, WriteClass::Fat)?;
    device.write_classified(reserved + fat_sectors, &first_fat, WriteClass::Fat)?;

    device.write_classified(1, &fsinfo, WriteClass::Fat)?;
    device.write_classified(7, &fsinfo, WriteClass::Fat)?;
    device.write_classified(6, &boot, WriteClass::Dir)?;
    // The boot sector last: until it lands, nothing mounts a half-made volume.
    device.write_classified(0, &boot, WriteClass::Dir)?;
    device.commit()
}

/// Smallest exFAT volume this will make: a bitmap, an up-case table, a root
/// directory and something to put in it.
const EXFAT_MIN_BYTES: u64 = 1024 * 1024;
const EXFAT_FAT_OFFSET: u64 = 24;
const EXFAT_EOC: u32 = 0xffff_ffff;

/// An up-case table in the compressed form the format allows: identity runs, with
/// only `a` to `z` mapped. `0xffff` then a count means that many identity units.
fn exfat_upcase() -> [u16; 30] {
    let mut table = [0u16; 30];
    table[0] = 0xffff;
    table[1] = b'a' as u16;
    for (index, slot) in table[2..28].iter_mut().enumerate() {
        *slot = b'A' as u16 + index as u16;
    }
    table[28] = 0xffff;
    table[29] = 0xffff - (b'z' as u16);
    table
}

fn exfat_table_checksum(bytes: &[u8]) -> u32 {
    let mut sum = 0u32;
    for &byte in bytes {
        sum = sum.rotate_right(1).wrapping_add(byte as u32);
    }
    sum
}

/// Writes a chain of consecutive clusters into a FAT sector buffer's worth of
/// entries, one write per entry: there are few of them.
fn exfat_chain<D: WritableBlockDevice>(
    device: &mut D,
    fat_start: u64,
    first: u32,
    clusters: u32,
) -> Result<(), FsError> {
    for cluster in first..first + clusters {
        let value = if cluster + 1 == first + clusters {
            EXFAT_EOC
        } else {
            cluster + 1
        };
        let block = fat_start + cluster as u64 * 4 / SECTOR;
        let within = (cluster as u64 * 4 % SECTOR) as usize;
        let mut sector = [0u8; 512];
        device.read_block(block, &mut sector)?;
        sector[within..within + 4].copy_from_slice(&value.to_le_bytes());
        device.write_classified(block, &sector, WriteClass::Fat)?;
    }
    Ok(())
}

/// Formats `device` as an empty exFAT volume called `label`. Works down to about
/// a megabyte, where FAT32 has no legal layout. Needs 512-byte blocks.
pub fn format_exfat<D: WritableBlockDevice>(device: &mut D, label: &str) -> Result<(), FsError> {
    if device.block_size() != SECTOR as usize {
        return Err(FsError::UnsupportedBlockSize);
    }
    if device.is_read_only() {
        return Err(FsError::ReadOnly);
    }
    let total = device.block_count();
    if total * SECTOR < EXFAT_MIN_BYTES {
        return Err(FsError::NoSpace);
    }
    // 4 KiB clusters to 256 MiB, then 32 KiB, as the usual tools choose.
    let cluster_shift: u32 = if total * SECTOR <= 256 * 1024 * 1024 { 3 } else { 6 };
    let spc = 1u64 << cluster_shift;

    let mut fat_sectors = ((total / spc + 2) * 4).div_ceil(SECTOR);
    let heap_start = (EXFAT_FAT_OFFSET + fat_sectors).next_multiple_of(spc);
    let clusters = (total - heap_start) / spc;
    fat_sectors = ((clusters + 2) * 4).div_ceil(SECTOR);
    if !(8..=0xffff_fff5).contains(&clusters) {
        return Err(FsError::NoSpace);
    }

    let cluster_bytes = spc * SECTOR;
    let bitmap_bytes = clusters.div_ceil(8);
    let bitmap_clusters = bitmap_bytes.div_ceil(cluster_bytes) as u32;
    let upcase = exfat_upcase();
    let mut upcase_bytes = [0u8; 60];
    for (slot, unit) in upcase_bytes.as_chunks_mut::<2>().0.iter_mut().zip(upcase.iter()) {
        slot.copy_from_slice(&unit.to_le_bytes());
    }
    let upcase_cluster = 2 + bitmap_clusters;
    let root_cluster = upcase_cluster + 1;
    let cluster_block = |cluster: u32| heap_start + (cluster as u64 - 2) * spc;

    // Boot region last, as with FAT32: until it lands, nothing mounts this.
    clear(device, 0, EXFAT_FAT_OFFSET)?;
    clear(device, EXFAT_FAT_OFFSET, fat_sectors)?;
    clear(device, cluster_block(2), bitmap_clusters as u64 * spc)?;
    clear(device, cluster_block(upcase_cluster), spc)?;
    clear(device, cluster_block(root_cluster), spc)?;

    // FAT.
    let mut first = [0u8; 512];
    first[..4].copy_from_slice(&0xffff_fff8u32.to_le_bytes());
    first[4..8].copy_from_slice(&EXFAT_EOC.to_le_bytes());
    device.write_classified(EXFAT_FAT_OFFSET, &first, WriteClass::Fat)?;
    exfat_chain(device, EXFAT_FAT_OFFSET, 2, bitmap_clusters)?;
    exfat_chain(device, EXFAT_FAT_OFFSET, upcase_cluster, 1)?;
    exfat_chain(device, EXFAT_FAT_OFFSET, root_cluster, 1)?;

    // Bitmap: the three system streams are allocated.
    let used = bitmap_clusters + 2;
    let mut sector = [0u8; 512];
    for index in 0..used {
        // They're all in the first sector unless the volume is vast, and a vast
        // volume has a bitmap long enough to matter: write sector by sector.
        let byte = (index / 8) as usize;
        if byte >= 512 {
            return Err(FsError::Unsupported);
        }
        sector[byte] |= 1 << (index % 8);
    }
    device.write_classified(cluster_block(2), &sector, WriteClass::Data)?;

    // Up-case table.
    let mut sector = [0u8; 512];
    sector[..upcase_bytes.len()].copy_from_slice(&upcase_bytes);
    device.write_classified(cluster_block(upcase_cluster), &sector, WriteClass::Data)?;

    // Root directory: the volume label, the bitmap and the up-case table.
    let mut root = [0u8; 512];
    root[0] = 0x83;
    let units: alloc::vec::Vec<u16> = label.encode_utf16().take(11).collect();
    root[1] = units.len() as u8;
    for (slot, unit) in root[2..24].as_chunks_mut::<2>().0.iter_mut().zip(units.iter()) {
        slot.copy_from_slice(&unit.to_le_bytes());
    }
    let bitmap = &mut root[32..64];
    bitmap[0] = 0x81;
    bitmap[20..24].copy_from_slice(&2u32.to_le_bytes());
    bitmap[24..32].copy_from_slice(&bitmap_bytes.to_le_bytes());
    let table = &mut root[64..96];
    table[0] = 0x82;
    table[4..8].copy_from_slice(&exfat_table_checksum(&upcase_bytes).to_le_bytes());
    table[20..24].copy_from_slice(&upcase_cluster.to_le_bytes());
    table[24..32].copy_from_slice(&(upcase_bytes.len() as u64).to_le_bytes());
    device.write_classified(cluster_block(root_cluster), &root, WriteClass::Data)?;
    device.commit()?;

    // Boot region, main and backup.
    let mut region = [0u8; 12 * 512];
    let boot = &mut region[..512];
    boot[..3].copy_from_slice(&[0xeb, 0x76, 0x90]);
    boot[3..11].copy_from_slice(b"EXFAT   ");
    boot[72..80].copy_from_slice(&total.to_le_bytes());
    boot[80..84].copy_from_slice(&(EXFAT_FAT_OFFSET as u32).to_le_bytes());
    boot[84..88].copy_from_slice(&(fat_sectors as u32).to_le_bytes());
    boot[88..92].copy_from_slice(&(heap_start as u32).to_le_bytes());
    boot[92..96].copy_from_slice(&(clusters as u32).to_le_bytes());
    boot[96..100].copy_from_slice(&root_cluster.to_le_bytes());
    // Good enough to tell two volumes apart: this kernel has no entropy source.
    let serial = (total as u32).wrapping_mul(0x9e37_79b9) ^ 0x4645_4f53;
    boot[100..104].copy_from_slice(&serial.to_le_bytes());
    boot[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
    boot[108] = 9;
    boot[109] = cluster_shift as u8;
    boot[110] = 1;
    boot[111] = 0x80;
    boot[112] = 0xff;
    boot[510] = 0x55;
    boot[511] = 0xaa;
    for extended in 1..9 {
        region[extended * 512 + 510] = 0x55;
        region[extended * 512 + 511] = 0xaa;
    }
    // Sector 11 repeats a checksum of the other eleven, skipping the fields that
    // change at run time: VolumeFlags and PercentInUse.
    let mut sum = 0u32;
    for (index, &byte) in region[..11 * 512].iter().enumerate() {
        if matches!(index, 106 | 107 | 112) {
            continue;
        }
        sum = sum.rotate_right(1).wrapping_add(byte as u32);
    }
    for word in region[11 * 512..].as_chunks_mut::<4>().0 {
        word.copy_from_slice(&sum.to_le_bytes());
    }
    device.write_classified(12, &region, WriteClass::Dir)?;
    device.write_classified(0, &region, WriteClass::Dir)?;
    device.commit()
}

/// Formats `device` as FAT32 if it's big enough, and exFAT if not.
pub fn format<D: WritableBlockDevice>(device: &mut D, label: &str) -> Result<(), FsError> {
    match format_fat32(device, label) {
        Err(FsError::NoSpace) => format_exfat(device, label),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{mount_writable, Exfat, Fat32, FileSystem, NodeId, RamDisk, WritableFileSystem};

    fn exercise(mut fs: alloc::boxed::Box<dyn WritableFileSystem + '_>) {
        assert!(!fs.is_damaged());
        assert_eq!(fs.child_count(NodeId::ROOT), 0);
        let total = fs.stats().free_blocks;
        assert!(total > 0);
        let dir = fs.create_dir(NodeId::ROOT, "Folder").unwrap();
        let file = fs.create_file(dir, "a long file name.txt").unwrap();
        let data = alloc::vec![9u8; 100_000];
        fs.write(file, 0, &data).unwrap();
        fs.flush().unwrap();
        assert_eq!(fs.read_to_vec(file).unwrap(), data);
        assert!(fs.resolve("/FOLDER/A LONG FILE NAME.TXT").is_some(), "case-insensitive");
        assert!(fs.stats().free_blocks < total);
    }

    #[test]
    fn fat32_comes_out_mountable_and_writable() {
        let mut disk = RamDisk::new(40 * 1024 * 1024, 512).unwrap();
        format_fat32(&mut disk, "fresh").unwrap();
        let fs = Fat32::mount(&mut disk).unwrap();
        assert_eq!(fs.stats().kind, "fat32");
        drop(fs);
        exercise(mount_writable(&mut disk).unwrap());
    }

    #[test]
    fn exfat_comes_out_mountable_and_writable_and_small() {
        for bytes in [1024 * 1024, 8 * 1024 * 1024, 300 * 1024 * 1024] {
            let mut disk = RamDisk::new(bytes, 512).unwrap();
            format_exfat(&mut disk, "fresh").unwrap();
            let fs = Exfat::mount(&mut disk).unwrap();
            assert_eq!(fs.stats().kind, "exfat");
            assert!(!fs.is_damaged());
            drop(fs);
            if bytes > 1024 * 1024 {
                exercise(mount_writable(&mut disk).unwrap());
            }
        }
    }

    #[test]
    fn the_size_chooses_the_format() {
        let mut small = RamDisk::new(8 * 1024 * 1024, 512).unwrap();
        format(&mut small, "x").unwrap();
        assert_eq!(crate::detect(&small).unwrap(), crate::VolumeKind::Exfat);
        let mut large = RamDisk::new(40 * 1024 * 1024, 512).unwrap();
        format(&mut large, "x").unwrap();
        assert_eq!(crate::detect(&large).unwrap(), crate::VolumeKind::Fat32);
    }

    #[test]
    fn what_can_not_be_formatted_is_refused() {
        let mut tiny = RamDisk::new(4096, 512).unwrap();
        assert_eq!(format(&mut tiny, "x"), Err(FsError::NoSpace));
        let mut sectors = RamDisk::new(8 * 1024 * 1024, 4096).unwrap();
        assert_eq!(format(&mut sectors, "x"), Err(FsError::UnsupportedBlockSize));
    }
}
