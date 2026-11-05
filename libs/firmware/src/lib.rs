//! Parsers for what firmware tells the kernel about the machine: ACPI tables,
//! the multiboot memory map and flattened device trees.
//!
//! Everything here reads input it can't trust, and a parser runs before there's
//! anything to report a panic with. So no parser indexes, slices or does
//! arithmetic unchecked: bad input is `None` or a shorter answer, never a panic.
//! Nothing allocates, either, because the first caller is what sets up memory.

#![cfg_attr(not(test), no_std)]

pub mod acpi;
pub mod fdt;
pub mod multiboot;

/// A range of physical addresses.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Region {
    /// First address.
    pub base: u64,
    /// Length in bytes.
    pub size: u64,
}

impl Region {
    /// One past the last address, or `None` if that doesn't fit in 64 bits.
    pub const fn end(&self) -> Option<u64> {
        self.base.checked_add(self.size)
    }
}

/// Most usable-RAM ranges a [`RegionList`] keeps. Firmware reports a handful.
pub const MAX_REGIONS: usize = 16;

/// A bounded list of memory ranges.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RegionList {
    regions: [Region; MAX_REGIONS],
    count: usize,
}

impl RegionList {
    /// An empty list.
    pub const fn new() -> Self {
        Self {
            regions: [Region { base: 0, size: 0 }; MAX_REGIONS],
            count: 0,
        }
    }

    /// Adds a range, dropping empty ones and those beyond the list's capacity.
    pub fn push(&mut self, region: Region) {
        if region.size == 0 {
            return;
        }
        if let Some(entry) = self.regions.get_mut(self.count) {
            *entry = region;
            self.count += 1;
        }
    }

    /// The ranges, in the order they were added.
    pub fn as_slice(&self) -> &[Region] {
        self.regions.get(..self.count).unwrap_or(&[])
    }

    /// Bytes covered, ignoring any overlap.
    pub fn total(&self) -> u64 {
        self.as_slice()
            .iter()
            .fold(0, |total, region| total.saturating_add(region.size))
    }
}

impl Default for RegionList {
    fn default() -> Self {
        Self::new()
    }
}

/// Physical memory as the parsers see it. The kernel implements this over its
/// identity map, and tests over a blob.
pub trait PhysMem {
    /// Fills `out` from `address`. Returns `false`, having read nothing the
    /// parser may rely on, when the range isn't known to be readable.
    fn read(&self, address: u64, out: &mut [u8]) -> bool;
}

pub(crate) fn read_u32<M: PhysMem + ?Sized>(mem: &M, address: u64) -> Option<u32> {
    let mut bytes = [0; 4];
    mem.read(address, &mut bytes).then(|| u32::from_le_bytes(bytes))
}

pub(crate) fn read_u64<M: PhysMem + ?Sized>(mem: &M, address: u64) -> Option<u64> {
    let mut bytes = [0; 8];
    mem.read(address, &mut bytes).then(|| u64::from_le_bytes(bytes))
}

/// A little-endian field of a byte slice, or `None` past its end.
pub(crate) fn field<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    let end = offset.checked_add(N)?;
    bytes.get(offset..end)?.try_into().ok()
}
