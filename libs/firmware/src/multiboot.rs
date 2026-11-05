//! The multiboot (version 1) information structure's memory map.

use crate::{read_u32, read_u64, PhysMem, Region, RegionList};

/// `flags` bit: `mem_lower` and `mem_upper` are valid.
const FLAG_MEM: u32 = 1 << 0;
/// `flags` bit: `mmap_length` and `mmap_addr` are valid.
const FLAG_MMAP: u32 = 1 << 6;
/// `mmap` entry type for usable RAM.
const TYPE_RAM: u32 = 1;
/// Offsets in the information structure.
const OFFSET_MEM_UPPER: u64 = 8;
const OFFSET_MMAP_LENGTH: u64 = 44;
const OFFSET_MMAP_ADDR: u64 = 48;
/// Smallest valid entry after its `size` word: base, length, type.
const MIN_ENTRY: u32 = 20;
/// Entries walked before a corrupt map is given up on.
const MAX_ENTRIES: usize = 128;

/// Reads the usable RAM ranges from the structure at `info`.
///
/// Prefers the full map. A loader without one gets the older `mem_upper` figure,
/// which only counts contiguous RAM from 1 MiB. `None` means the structure
/// doesn't say.
pub fn usable_ram<M: PhysMem + ?Sized>(mem: &M, info: u64) -> Option<RegionList> {
    if info == 0 {
        return None;
    }
    let flags = read_u32(mem, info)?;
    if flags & FLAG_MMAP != 0 {
        if let Some(list) = read_map(mem, info) {
            if list.total() != 0 {
                return Some(list);
            }
        }
    }
    if flags & FLAG_MEM != 0 {
        let upper = read_u32(mem, info.checked_add(OFFSET_MEM_UPPER)?)?;
        let mut list = RegionList::new();
        list.push(Region {
            base: 0x10_0000,
            size: u64::from(upper) * 1024,
        });
        return Some(list);
    }
    None
}

fn read_map<M: PhysMem + ?Sized>(mem: &M, info: u64) -> Option<RegionList> {
    let length = read_u32(mem, info.checked_add(OFFSET_MMAP_LENGTH)?)?;
    let address = read_u32(mem, info.checked_add(OFFSET_MMAP_ADDR)?)?;

    let mut list = RegionList::new();
    let start = u64::from(address);
    let end = start.checked_add(u64::from(length))?;
    let mut entry = start;
    for _ in 0..MAX_ENTRIES {
        if entry.checked_add(u64::from(MIN_ENTRY) + 4)? > end {
            break;
        }
        // `size` doesn't count itself.
        let size = read_u32(mem, entry)?;
        if size < MIN_ENTRY {
            break;
        }
        let base = read_u64(mem, entry + 4)?;
        let bytes = read_u64(mem, entry + 12)?;
        let kind = read_u32(mem, entry + 20)?;
        if kind == TYPE_RAM && base.checked_add(bytes).is_some() {
            list.push(Region { base, size: bytes });
        }
        entry = entry.checked_add(u64::from(size) + 4)?;
    }
    Some(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// What QEMU's multiboot loader left behind on a 512 MiB q35: the structure at
    /// 0x9500 and its map at 0x9000.
    const INFO: &[u8] = include_bytes!("../fixtures/q35-multiboot-info.bin");
    const MAP: &[u8] = include_bytes!("../fixtures/q35-multiboot-mmap.bin");

    struct Blobs(Vec<(u64, Vec<u8>)>);

    impl PhysMem for Blobs {
        fn read(&self, address: u64, out: &mut [u8]) -> bool {
            for (base, blob) in &self.0 {
                if address >= *base {
                    let offset = (address - base) as usize;
                    if let Some(bytes) = blob.get(offset..offset + out.len()) {
                        out.copy_from_slice(bytes);
                        return true;
                    }
                }
            }
            false
        }
    }

    #[test]
    fn real_map_from_qemu() {
        let mem = Blobs(vec![(0x9500, INFO.to_vec()), (0x9000, MAP.to_vec())]);
        let list = usable_ram(&mem, 0x9500).unwrap();
        assert_eq!(
            list.as_slice(),
            &[
                Region { base: 0, size: 0x9_fc00 },
                Region { base: 0x10_0000, size: 0x1fb0_0000 },
            ]
        );
    }

    #[test]
    fn zero_pointer_and_unreadable_memory_say_nothing() {
        let mem = Blobs(vec![]);
        assert_eq!(usable_ram(&mem, 0), None);
        assert_eq!(usable_ram(&mem, 0x9500), None);
    }

    #[test]
    fn mem_upper_is_the_fallback() {
        let mut info = INFO.to_vec();
        info[0..4].copy_from_slice(&FLAG_MEM.to_le_bytes());
        let mem = Blobs(vec![(0x9500, info)]);
        let list = usable_ram(&mem, 0x9500).unwrap();
        assert_eq!(list.as_slice(), &[Region { base: 0x10_0000, size: 519168 * 1024 }]);
    }

    #[test]
    fn corrupt_entries_end_the_walk_without_panicking() {
        for size in [0u32, 1, 19, u32::MAX, u32::MAX - 3] {
            let mut map = MAP.to_vec();
            map[0..4].copy_from_slice(&size.to_le_bytes());
            let mem = Blobs(vec![(0x9500, INFO.to_vec()), (0x9000, map)]);
            let _ = usable_ram(&mem, 0x9500);
        }
        // A map that claims to wrap the address space.
        let mut info = INFO.to_vec();
        info[44..48].copy_from_slice(&u32::MAX.to_le_bytes());
        info[48..52].copy_from_slice(&u32::MAX.to_le_bytes());
        let mem = Blobs(vec![(0x9500, info), (0x9000, MAP.to_vec())]);
        let _ = usable_ram(&mem, 0x9500);
    }

    #[test]
    fn every_truncation_and_byte_flip_is_survivable() {
        for cut in 0..MAP.len() {
            let mem = Blobs(vec![(0x9500, INFO.to_vec()), (0x9000, MAP[..cut].to_vec())]);
            let _ = usable_ram(&mem, 0x9500);
        }
        for index in 0..MAP.len() {
            let mut map = MAP.to_vec();
            map[index] ^= 0xff;
            let mem = Blobs(vec![(0x9500, INFO.to_vec()), (0x9000, map)]);
            let _ = usable_ram(&mem, 0x9500);
        }
    }
}
