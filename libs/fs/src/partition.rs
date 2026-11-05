//! MBR and GPT partition tables, and a view of one partition.
//!
//! Tables are untrusted input: every offset is range-checked, a looping chain of
//! extended partitions ends, and a GPT must pass both of its CRCs before it's
//! believed.

use alloc::vec::Vec;

use crate::block::{BlockDevice, WritableBlockDevice, WriteClass};
use crate::tree::try_zeroed;
use crate::FsError;

/// Most entries a GPT is read for. Disks in practice have 128.
const MAX_GPT_ENTRIES: u32 = 1024;
/// Logical partitions followed in an extended partition.
const MAX_LOGICAL: usize = 64;
/// Partitions reported from one disk.
pub const MAX_PARTITIONS: usize = 64;

/// A filesystem hint from a partition table. Filesystem detection has the last
/// word, because tables are often vague or wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PartitionHint {
    /// An MBR type byte.
    Mbr(u8),
    /// A GPT type GUID, in on-disk byte order.
    Gpt([u8; 16]),
    /// The whole disk, with no partition table.
    WholeDisk,
}

/// One partition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PartitionInfo {
    /// First block.
    pub start: u64,
    /// Length in blocks.
    pub count: u64,
    /// What the table said about it.
    pub hint: PartitionHint,
}

/// CRC-32 as a GPT uses it (IEEE, reflected).
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn read_exact<D: BlockDevice>(device: &D, block: u64, out: &mut [u8]) -> Result<(), FsError> {
    device.read_blocks(block, out)
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn le64(bytes: &[u8], at: usize) -> u64 {
    le32(bytes, at) as u64 | (le32(bytes, at + 4) as u64) << 32
}

fn push(found: &mut Vec<PartitionInfo>, info: PartitionInfo) -> Result<(), FsError> {
    if found.len() < MAX_PARTITIONS {
        found.try_reserve(1).map_err(|_| FsError::OutOfMemory)?;
        found.push(info);
    }
    Ok(())
}

/// Reads and checks one GPT header and its entries.
fn read_gpt<D: BlockDevice>(
    device: &D,
    lba: u64,
    sector: &[u8],
    found: &mut Vec<PartitionInfo>,
) -> Result<bool, FsError> {
    let block_size = device.block_size();
    if &sector[..8] != b"EFI PART" {
        return Ok(false);
    }
    let header_size = le32(sector, 12) as usize;
    if !(92..=block_size).contains(&header_size) {
        return Ok(false);
    }
    let mut header = try_zeroed(header_size)?;
    header.copy_from_slice(&sector[..header_size]);
    header[16..20].fill(0);
    if crc32(&header) != le32(sector, 16) || le64(sector, 24) != lba {
        return Ok(false);
    }

    let entries_lba = le64(sector, 72);
    let count = le32(sector, 80);
    let entry_size = le32(sector, 84) as usize;
    if count == 0
        || count > MAX_GPT_ENTRIES
        || entry_size < 128
        || entry_size > 4096
        || !entry_size.is_power_of_two()
    {
        return Ok(false);
    }
    let bytes = count as usize * entry_size;
    let blocks = bytes.div_ceil(block_size) as u64;
    let total = device.block_count();
    match entries_lba.checked_add(blocks) {
        Some(end) if end <= total && entries_lba >= 1 => {}
        _ => return Ok(false),
    }
    let mut table = try_zeroed(blocks as usize * block_size)?;
    read_exact(device, entries_lba, &mut table)?;
    if crc32(&table[..bytes]) != le32(sector, 88) {
        return Ok(false);
    }

    let usable_first = le64(sector, 40);
    let usable_last = le64(sector, 48);
    for entry in table[..bytes].chunks_exact(entry_size) {
        let mut guid = [0u8; 16];
        guid.copy_from_slice(&entry[..16]);
        if guid == [0; 16] {
            continue;
        }
        let first = le64(entry, 32);
        let last = le64(entry, 40);
        if first > last || first < usable_first || last > usable_last || last >= total {
            continue;
        }
        push(
            found,
            PartitionInfo {
                start: first,
                count: last - first + 1,
                hint: PartitionHint::Gpt(guid),
            },
        )?;
    }
    Ok(true)
}

fn parse_gpt<D: BlockDevice>(device: &D) -> Result<Option<Vec<PartitionInfo>>, FsError> {
    let total = device.block_count();
    let mut sector = try_zeroed(device.block_size())?;
    let mut found = Vec::new();
    read_exact(device, 1, &mut sector)?;
    if read_gpt(device, 1, &sector, &mut found)? {
        return Ok(Some(found));
    }
    // The primary is damaged; the backup header is on the last block.
    found.clear();
    read_exact(device, total - 1, &mut sector)?;
    if read_gpt(device, total - 1, &sector, &mut found)? {
        return Ok(Some(found));
    }
    Ok(None)
}

fn parse_extended<D: BlockDevice>(
    device: &D,
    extended_start: u64,
    extended_count: u64,
    found: &mut Vec<PartitionInfo>,
) -> Result<(), FsError> {
    let total = device.block_count();
    let mut sector = try_zeroed(device.block_size())?;
    let mut ebr = extended_start;
    for _ in 0..MAX_LOGICAL {
        if ebr >= total {
            return Ok(());
        }
        read_exact(device, ebr, &mut sector)?;
        if sector[510] != 0x55 || sector[511] != 0xaa {
            return Ok(());
        }
        let first = &sector[446..462];
        let kind = first[4];
        let start = ebr.saturating_add(le32(first, 8) as u64);
        let count = le32(first, 12) as u64;
        if kind != 0 && count != 0 && start.saturating_add(count) <= total {
            push(
                found,
                PartitionInfo {
                    start,
                    count,
                    hint: PartitionHint::Mbr(kind),
                },
            )?;
        }
        let second = &sector[462..478];
        let next = le32(second, 8) as u64;
        if !matches!(second[4], 0x05 | 0x0f | 0x85) || next == 0 || next >= extended_count {
            return Ok(());
        }
        // Each link has to move forward, which a loop can't.
        let following = extended_start + next;
        if following <= ebr {
            return Ok(());
        }
        ebr = following;
    }
    Ok(())
}

/// Reads the partition table of `device`.
///
/// Returns an empty list for a disk with none. That's the normal answer for a
/// disk holding one filesystem directly, as a boot sector also ends in `55 AA`:
/// callers try [`detect`](crate::detect) first and come here otherwise.
pub fn read_partitions<D: BlockDevice>(device: &D) -> Result<Vec<PartitionInfo>, FsError> {
    let block_size = device.block_size();
    let total = device.block_count();
    if block_size < 512 || total < 2 {
        return Ok(Vec::new());
    }
    let mut sector = try_zeroed(block_size)?;
    read_exact(device, 0, &mut sector)?;

    let mut found = Vec::new();
    let has_signature = sector[510] == 0x55 && sector[511] == 0xaa;
    if has_signature {
        let protective = (0..4).any(|index| sector[446 + index * 16 + 4] == 0xee);
        if protective {
            if let Some(gpt) = parse_gpt(device)? {
                return Ok(gpt);
            }
        }
    } else {
        // A GPT without a protective MBR is out of spec, but it says what it is.
        return Ok(parse_gpt(device)?.unwrap_or_default());
    }

    for index in 0..4 {
        let entry = &sector[446 + index * 16..462 + index * 16];
        let kind = entry[4];
        let start = le32(entry, 8) as u64;
        let count = le32(entry, 12) as u64;
        if kind == 0 || kind == 0xee || count == 0 || start == 0 {
            continue;
        }
        if matches!(kind, 0x05 | 0x0f | 0x85) {
            parse_extended(device, start, count, &mut found)?;
            continue;
        }
        if start.saturating_add(count) > total {
            continue;
        }
        push(
            &mut found,
            PartitionInfo {
                start,
                count,
                hint: PartitionHint::Mbr(kind),
            },
        )?;
    }
    Ok(found)
}

/// One partition of a device, addressed from zero.
pub struct Partition<D> {
    device: D,
    start: u64,
    count: u64,
}

impl<D: BlockDevice> Partition<D> {
    /// A view of `count` blocks at `start`. Fails if it runs off the device.
    pub fn new(device: D, start: u64, count: u64) -> Result<Self, FsError> {
        match start.checked_add(count) {
            Some(end) if end <= device.block_count() => Ok(Self {
                device,
                start,
                count,
            }),
            _ => Err(FsError::OutOfBounds),
        }
    }

    fn translate(&self, block: u64, bytes: usize) -> Result<u64, FsError> {
        let blocks = (bytes / self.device.block_size().max(1)) as u64;
        match block.checked_add(blocks) {
            Some(end) if end <= self.count => Ok(self.start + block),
            _ => Err(FsError::OutOfBounds),
        }
    }
}

impl<D: BlockDevice> BlockDevice for Partition<D> {
    fn block_size(&self) -> usize {
        self.device.block_size()
    }

    fn block_count(&self) -> u64 {
        self.count
    }

    fn read_block(&self, block: u64, out: &mut [u8]) -> Result<(), FsError> {
        self.device.read_block(self.translate(block, out.len())?, out)
    }

    fn read_blocks(&self, start: u64, out: &mut [u8]) -> Result<(), FsError> {
        self.device.read_blocks(self.translate(start, out.len())?, out)
    }

    fn is_read_only(&self) -> bool {
        self.device.is_read_only()
    }
}

impl<D: WritableBlockDevice> WritableBlockDevice for Partition<D> {
    fn write_block(&mut self, block: u64, data: &[u8]) -> Result<(), FsError> {
        let block = self.translate(block, data.len())?;
        self.device.write_block(block, data)
    }

    fn write_blocks(&mut self, start: u64, data: &[u8]) -> Result<(), FsError> {
        let start = self.translate(start, data.len())?;
        self.device.write_blocks(start, data)
    }

    fn write_classified(
        &mut self,
        start: u64,
        data: &[u8],
        class: WriteClass,
    ) -> Result<(), FsError> {
        let start = self.translate(start, data.len())?;
        self.device.write_classified(start, data, class)
    }

    fn flush(&mut self) -> Result<(), FsError> {
        self.device.flush()
    }

    fn commit(&mut self) -> Result<(), FsError> {
        self.device.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RamDisk;

    const BLOCKS: usize = 4096;

    fn disk() -> RamDisk {
        RamDisk::new(BLOCKS * 512, 512).unwrap()
    }

    fn mbr_entry(bytes: &mut [u8], index: usize, kind: u8, start: u32, count: u32) {
        let at = 446 + index * 16;
        bytes[at + 4] = kind;
        bytes[at + 8..at + 12].copy_from_slice(&start.to_le_bytes());
        bytes[at + 12..at + 16].copy_from_slice(&count.to_le_bytes());
        bytes[510] = 0x55;
        bytes[511] = 0xaa;
    }

    fn write_sector(disk: &mut RamDisk, lba: u64, sector: &[u8]) {
        disk.write_blocks(lba, sector).unwrap();
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn reads_primary_mbr_entries_and_skips_impossible_ones() {
        let mut device = disk();
        let mut sector = [0u8; 512];
        mbr_entry(&mut sector, 0, 0x0c, 2048, 1024);
        mbr_entry(&mut sector, 1, 0x07, 3072, 512);
        mbr_entry(&mut sector, 2, 0x0c, 4000, 4000); // Runs off the disk.
        write_sector(&mut device, 0, &sector);
        let parts = read_partitions(&device).unwrap();
        assert_eq!(
            parts,
            [
                PartitionInfo {
                    start: 2048,
                    count: 1024,
                    hint: PartitionHint::Mbr(0x0c)
                },
                PartitionInfo {
                    start: 3072,
                    count: 512,
                    hint: PartitionHint::Mbr(0x07)
                },
            ]
        );
    }

    #[test]
    fn follows_logical_partitions_and_survives_a_loop() {
        let mut device = disk();
        let mut sector = [0u8; 512];
        mbr_entry(&mut sector, 0, 0x0f, 1000, 2000);
        write_sector(&mut device, 0, &sector);

        let mut first = [0u8; 512];
        mbr_entry(&mut first, 0, 0x0c, 8, 100);
        mbr_entry(&mut first, 1, 0x05, 200, 300);
        write_sector(&mut device, 1000, &first);
        let mut second = [0u8; 512];
        mbr_entry(&mut second, 0, 0x06, 8, 50);
        mbr_entry(&mut second, 1, 0x05, 0, 300); // Back to the first link.
        write_sector(&mut device, 1200, &second);

        let parts = read_partitions(&device).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!((parts[0].start, parts[0].count), (1008, 100));
        assert_eq!((parts[1].start, parts[1].count), (1208, 50));
    }

    fn write_gpt(device: &mut RamDisk, header_lba: u64, entries_lba: u64) {
        let mut table = vec![0u8; 128 * 128];
        table[..16].copy_from_slice(&[0xaa; 16]);
        table[32..40].copy_from_slice(&2048u64.to_le_bytes());
        table[40..48].copy_from_slice(&3071u64.to_le_bytes());
        // A second entry past the usable range is ignored.
        table[128..144].copy_from_slice(&[0xbb; 16]);
        table[128 + 32..128 + 40].copy_from_slice(&100u64.to_le_bytes());
        table[128 + 40..128 + 48].copy_from_slice(&1_000_000u64.to_le_bytes());
        device.write_blocks(entries_lba, &table).unwrap();

        let mut header = [0u8; 512];
        header[..8].copy_from_slice(b"EFI PART");
        header[12..16].copy_from_slice(&92u32.to_le_bytes());
        header[24..32].copy_from_slice(&header_lba.to_le_bytes());
        header[40..48].copy_from_slice(&34u64.to_le_bytes());
        header[48..56].copy_from_slice(&(BLOCKS as u64 - 34).to_le_bytes());
        header[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        header[80..84].copy_from_slice(&128u32.to_le_bytes());
        header[84..88].copy_from_slice(&128u32.to_le_bytes());
        header[88..92].copy_from_slice(&crc32(&table).to_le_bytes());
        let crc = crc32(&header[..92]);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
        device.write_blocks(header_lba, &header).unwrap();
    }

    fn protective(device: &mut RamDisk) {
        let mut sector = [0u8; 512];
        mbr_entry(&mut sector, 0, 0xee, 1, BLOCKS as u32 - 1);
        write_sector(device, 0, &sector);
    }

    #[test]
    fn reads_a_gpt_and_rejects_bad_crcs() {
        let mut device = disk();
        protective(&mut device);
        write_gpt(&mut device, 1, 2);
        let parts = read_partitions(&device).unwrap();
        assert_eq!(
            parts,
            [PartitionInfo {
                start: 2048,
                count: 1024,
                hint: PartitionHint::Gpt([0xaa; 16])
            }]
        );

        // Damage the entries: the CRC stops the table being believed.
        device.write_blocks(2, &[0x5a; 512]).unwrap();
        assert!(read_partitions(&device).unwrap().is_empty());
    }

    #[test]
    fn falls_back_to_the_backup_gpt_header() {
        let mut device = disk();
        protective(&mut device);
        write_gpt(&mut device, BLOCKS as u64 - 1, BLOCKS as u64 - 33);
        let parts = read_partitions(&device).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].start, 2048);
    }

    #[test]
    fn an_absurd_gpt_is_ignored_without_panicking() {
        let mut device = disk();
        protective(&mut device);
        let mut header = [0u8; 512];
        header[..8].copy_from_slice(b"EFI PART");
        for size in [0u32, 91, 513, u32::MAX] {
            header[12..16].copy_from_slice(&size.to_le_bytes());
            device.write_blocks(1, &header).unwrap();
            assert!(read_partitions(&device).unwrap().is_empty());
        }
    }

    #[test]
    fn a_partition_translates_and_confines_access() {
        let mut device = disk();
        device.write_blocks(10, &[9u8; 512]).unwrap();
        let mut part = Partition::new(&mut device, 10, 2).unwrap();
        assert_eq!(part.block_count(), 2);
        let mut block = [0u8; 512];
        part.read_block(0, &mut block).unwrap();
        assert_eq!(block, [9u8; 512]);
        assert_eq!(part.read_block(2, &mut block), Err(FsError::OutOfBounds));
        let mut two = [0u8; 1024];
        assert_eq!(part.read_blocks(1, &mut two), Err(FsError::OutOfBounds));
        part.write_block(1, &[4u8; 512]).unwrap();
        assert_eq!(device.data()[11 * 512], 4);
        assert!(Partition::new(&device, BLOCKS as u64, 1).is_err());
    }

    #[test]
    fn a_blank_disk_has_no_partitions() {
        assert!(read_partitions(&disk()).unwrap().is_empty());
    }
}
