//! What the EFI loader tells the kernel, and the parsing it takes to get there.
//!
//! The loader and the kernel are built separately, for different targets, so the
//! layout of [`Handoff`] is a promise: `repr(C)`, versioned, and checked on arrival.
//! The ELF and memory map parsers run on data that isn't to be trusted either, and
//! are tested on the host.

#![cfg_attr(not(test), no_std)]

/// What a [`Handoff`] starts with: "FEOSBOOT".
pub const MAGIC: u64 = u64::from_le_bytes(*b"FEOSBOOT");
/// The version of the layout this code knows.
pub const VERSION: u32 = 1;
/// Ranges of RAM carried. A fragmented map keeps its largest.
pub const MAX_RAM_RANGES: usize = 16;
/// Where to find the 64-bit entry point: a mark in the first 8 KiB of an x86_64 image,
/// followed by the address. A multiboot entry starts in 32-bit mode, and firmware
/// doesn't.
pub const X86_ENTRY_MARK: u64 = u64::from_le_bytes(*b"FEOSEFI\0");

/// A range of physical memory.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Range {
    /// Start.
    pub base: u64,
    /// Length in bytes.
    pub size: u64,
}

impl Range {
    /// One past the end, saturating.
    pub const fn end(&self) -> u64 {
        self.base.saturating_add(self.size)
    }

    /// True if any address is in both.
    pub const fn overlaps(&self, other: &Self) -> bool {
        self.base < other.end() && other.base < self.end()
    }
}

/// A linear framebuffer firmware has set up. The pixels are 32-bit, blue in the low
/// byte: anything else isn't reported.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Framebuffer {
    /// Address, or zero when there's none.
    pub base: u64,
    /// Bytes in it.
    pub size: u64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixels from one row to the next.
    pub stride: u32,
    /// Unused.
    pub reserved: u32,
}

/// What the loader knows and the kernel needs. Both ends read it while it's where the
/// loader put it, outside the kernel's image; the kernel copies out what it keeps
/// before it allocates anything.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Handoff {
    /// [`MAGIC`].
    pub magic: u64,
    /// [`VERSION`].
    pub version: u32,
    /// How many of `ram` are in use.
    pub ram_count: u32,
    /// Usable RAM: free, or what boot services no longer need.
    pub ram: [Range; MAX_RAM_RANGES],
    /// The firmware's framebuffer.
    pub framebuffer: Framebuffer,
    /// The ACPI RSDP, or zero.
    pub rsdp: u64,
    /// The device tree, or zero.
    pub device_tree: u64,
}

impl Handoff {
    /// An empty one.
    pub const fn new() -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            ram_count: 0,
            ram: [Range { base: 0, size: 0 }; MAX_RAM_RANGES],
            framebuffer: Framebuffer {
                base: 0,
                size: 0,
                width: 0,
                height: 0,
                stride: 0,
                reserved: 0,
            },
            rsdp: 0,
            device_tree: 0,
        }
    }

    /// The ranges of usable RAM.
    pub fn ram(&self) -> &[Range] {
        self.ram.get(..self.ram_count as usize).unwrap_or(&self.ram)
    }

    /// Whether what's at this address can be read as one of these. What looks at a
    /// boot information pointer can't know which kind it was given.
    ///
    /// # Safety
    /// The eight bytes at `address` must be readable, and so must the whole structure
    /// if they hold the mark.
    pub unsafe fn at(address: usize) -> Option<&'static Self> {
        if address == 0 || address % 8 != 0 {
            return None;
        }
        // SAFETY: as promised by the caller.
        let magic = unsafe { core::ptr::read_volatile(address as *const u64) };
        if magic != MAGIC {
            return None;
        }
        // SAFETY: as promised by the caller. It lasts until the kernel allocates,
        // and every field is valid for any bytes.
        let handoff = unsafe { &*(address as *const Self) };
        (handoff.version == VERSION).then_some(handoff)
    }
}

impl Default for Handoff {
    fn default() -> Self {
        Self::new()
    }
}

/// A piece of a kernel image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment<'a> {
    /// Where it goes.
    pub address: u64,
    /// What goes there.
    pub data: &'a [u8],
    /// How much memory it takes: what's past the data, the kernel clears.
    pub memory_size: u64,
}

/// The fields of an ELF image that loading needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Elf<'a> {
    image: &'a [u8],
    /// The entry point.
    pub entry: u64,
    table: u64,
    entry_size: u64,
    count: u64,
}

const PT_LOAD: u32 = 1;

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    let bytes = data.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    let bytes = data.get(at..at.checked_add(8)?)?;
    let mut word = [0u8; 8];
    word.copy_from_slice(bytes);
    Some(u64::from_le_bytes(word))
}

impl<'a> Elf<'a> {
    /// Checks the header of a little-endian 64-bit executable for this machine type
    /// (62 for x86_64, 183 for AArch64).
    pub fn parse(image: &'a [u8], machine: u16) -> Option<Self> {
        if image.get(..4)? != b"\x7fELF" || *image.get(4)? != 2 || *image.get(5)? != 1 {
            return None;
        }
        if u16_at(image, 18)? != machine {
            return None;
        }
        let entry_size = u64::from(u16_at(image, 54)?);
        if entry_size < 56 {
            return None;
        }
        Some(Self {
            image,
            entry: u64_at(image, 24)?,
            table: u64_at(image, 32)?,
            entry_size,
            count: u64::from(u16_at(image, 56)?),
        })
    }

    /// The `index`th loadable segment, or `None` past the last. Its data is cut to
    /// what the file really has. A segment whose numbers don't add up ends the list
    /// with `Err`.
    pub fn segment(&self, mut index: usize) -> Result<Option<Segment<'a>>, ()> {
        for number in 0..self.count {
            let at = number
                .checked_mul(self.entry_size)
                .and_then(|offset| offset.checked_add(self.table))
                .and_then(|offset| usize::try_from(offset).ok())
                .ok_or(())?;
            if u32_at(self.image, at).ok_or(())? != PT_LOAD {
                continue;
            }
            if index != 0 {
                index -= 1;
                continue;
            }
            let offset = usize::try_from(u64_at(self.image, at + 8).ok_or(())?).map_err(|_| ())?;
            let address = u64_at(self.image, at + 24).ok_or(())?;
            let file_size = usize::try_from(u64_at(self.image, at + 32).ok_or(())?).map_err(|_| ())?;
            let memory_size = u64_at(self.image, at + 40).ok_or(())?;
            let end = offset.checked_add(file_size).ok_or(())?;
            if file_size as u64 > memory_size || address.checked_add(memory_size).is_none() {
                return Err(());
            }
            let data = self.image.get(offset..end).ok_or(())?;
            return Ok(Some(Segment {
                address,
                data,
                memory_size,
            }));
        }
        Ok(None)
    }

    /// The range of memory all the segments occupy.
    pub fn extent(&self) -> Result<Range, ()> {
        let mut low = u64::MAX;
        let mut high = 0u64;
        let mut index = 0;
        while let Some(segment) = self.segment(index)? {
            low = low.min(segment.address);
            high = high.max(segment.address + segment.memory_size);
            index += 1;
        }
        if index == 0 {
            return Err(());
        }
        Ok(Range {
            base: low,
            size: high - low,
        })
    }

    /// Finds a mark in the start of the image's first segment, and the word after it.
    pub fn marked_word(&self, mark: u64) -> Option<u64> {
        let first = self.segment(0).ok()??;
        let window = first.data.get(..first.data.len().min(8192))?;
        window
            .chunks_exact(8)
            .position(|chunk| chunk == mark.to_le_bytes())
            .and_then(|index| u64_at(window, (index + 1) * 8))
    }
}

/// EFI memory types this kernel may use once boot services have gone.
pub const fn is_usable_type(kind: u32) -> bool {
    // Loader code and data, boot services code and data, conventional.
    matches!(kind, 1..=4 | 7)
}

/// Reads an EFI memory map, whose entries are `stride` bytes each, and calls `visit`
/// with each entry's type, start and length.
pub fn walk_memory_map(map: &[u8], stride: usize, mut visit: impl FnMut(u32, u64, u64)) {
    // The specification's entries are 40 bytes, and only ever grow.
    if stride < 40 {
        return;
    }
    for entry in map.chunks_exact(stride) {
        let (Some(kind), Some(base), Some(pages)) = (u32_at(entry, 0), u64_at(entry, 8), u64_at(entry, 24)) else {
            return;
        };
        let size = pages.saturating_mul(4096);
        visit(kind, base, size);
    }
}

/// Builds the list of usable RAM from an EFI memory map: neighbours merged, and
/// the largest kept if there are more than fit.
pub fn usable_ram(map: &[u8], stride: usize, out: &mut [Range; MAX_RAM_RANGES]) -> u32 {
    let mut count = 0usize;
    walk_memory_map(map, stride, |kind, base, size| {
        if !is_usable_type(kind) || size == 0 {
            return;
        }
        // Grows a range this one follows or leads, which is nearly always possible:
        // a map's in order, and usually one of a run of kinds.
        for range in out.iter_mut().take(count) {
            if range.end() == base {
                range.size += size;
                return;
            }
            if base.saturating_add(size) == range.base {
                range.base = base;
                range.size += size;
                return;
            }
        }
        if count < MAX_RAM_RANGES {
            out[count] = Range { base, size };
            count += 1;
            return;
        }
        // Replaces the smallest, if this is bigger.
        let mut smallest = 0;
        for index in 1..count {
            if out[index].size < out[smallest].size {
                smallest = index;
            }
        }
        if size > out[smallest].size {
            out[smallest] = Range { base, size };
        }
    });
    // A later range can have joined two that were apart.
    let mut index = 0;
    while index < count {
        let mut other = 0;
        while other < count {
            if other != index && out[index].end() == out[other].base {
                out[index].size += out[other].size;
                out[other] = out[count - 1];
                count -= 1;
                if index == count {
                    index = other;
                }
                other = 0;
                continue;
            }
            other += 1;
        }
        index += 1;
    }
    count as u32
}

/// Whether the whole of `wanted` lies in one of `ranges`.
pub fn is_covered(ranges: &[Range], wanted: &Range) -> bool {
    ranges
        .iter()
        .any(|range| range.base <= wanted.base && wanted.end() <= range.end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: u32, base: u64, pages: u64) -> [u8; 48] {
        let mut out = [0u8; 48];
        out[0..4].copy_from_slice(&kind.to_le_bytes());
        out[8..16].copy_from_slice(&base.to_le_bytes());
        out[24..32].copy_from_slice(&pages.to_le_bytes());
        out
    }

    fn map(entries: &[[u8; 48]]) -> Vec<u8> {
        entries.iter().flatten().copied().collect()
    }

    #[test]
    fn neighbours_merge_and_other_kinds_stay_out() {
        let data = map(&[
            entry(7, 0, 0x80),
            entry(0, 0x80000, 0x20),    // reserved
            entry(2, 0x100000, 0x100),  // loader data
            entry(7, 0x200000, 0x100),  // free
            entry(4, 0x300000, 0x100),  // boot services data
            entry(10, 0x400000, 0x10),  // ACPI NVS
            entry(9, 0x500000, 0x10),   // ACPI reclaim
            entry(5, 0x600000, 0x10),   // runtime code
            entry(7, 0x700000, 0x10),
        ]);
        let mut out = [Range::default(); MAX_RAM_RANGES];
        let count = usable_ram(&data, 48, &mut out) as usize;
        assert_eq!(
            &out[..count],
            &[
                Range { base: 0, size: 0x80000 },
                Range { base: 0x100000, size: 0x300000 },
                Range { base: 0x700000, size: 0x10000 },
            ]
        );
    }

    #[test]
    fn out_of_order_neighbours_merge_too() {
        let data = map(&[entry(7, 0x20000, 16), entry(7, 0, 16), entry(7, 0x10000, 16)]);
        let mut out = [Range::default(); MAX_RAM_RANGES];
        let count = usable_ram(&data, 48, &mut out) as usize;
        assert_eq!(&out[..count], &[Range { base: 0, size: 0x30000 }]);
    }

    #[test]
    fn a_fragmented_map_keeps_the_largest() {
        let mut entries = Vec::new();
        for index in 0..40u64 {
            // Apart from each other, and growing.
            entries.push(entry(7, index * 0x100_0000, index + 1));
        }
        let data = map(&entries);
        let mut out = [Range::default(); MAX_RAM_RANGES];
        let count = usable_ram(&data, 48, &mut out) as usize;
        assert_eq!(count, MAX_RAM_RANGES);
        let smallest = out.iter().map(|range| range.size).min().unwrap();
        assert_eq!(smallest, 25 * 4096);
    }

    #[test]
    fn a_nonsense_stride_reads_nothing() {
        let data = map(&[entry(7, 0, 1)]);
        let mut out = [Range::default(); MAX_RAM_RANGES];
        assert_eq!(usable_ram(&data, 8, &mut out), 0);
    }

    #[test]
    fn coverage_is_all_or_nothing() {
        let ranges = [Range { base: 0x1000, size: 0x1000 }];
        assert!(is_covered(&ranges, &Range { base: 0x1800, size: 0x800 }));
        assert!(!is_covered(&ranges, &Range { base: 0x1800, size: 0x801 }));
        assert!(!is_covered(&ranges, &Range { base: 0, size: 0x1001 }));
    }

    /// A little ELF: a header, two program headers and some data.
    fn elf(machine: u16) -> Vec<u8> {
        let mut out = vec![0u8; 64 + 2 * 56 + 16];
        out[..4].copy_from_slice(b"\x7fELF");
        out[4] = 2;
        out[5] = 1;
        out[18..20].copy_from_slice(&machine.to_le_bytes());
        out[24..32].copy_from_slice(&0x100040u64.to_le_bytes());
        out[32..40].copy_from_slice(&64u64.to_le_bytes());
        out[54..56].copy_from_slice(&56u16.to_le_bytes());
        out[56..58].copy_from_slice(&2u16.to_le_bytes());
        let header = |out: &mut Vec<u8>, at: usize, offset: u64, address: u64, file: u64, memory: u64| {
            out[at..at + 4].copy_from_slice(&1u32.to_le_bytes());
            out[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
            out[at + 16..at + 24].copy_from_slice(&address.to_le_bytes());
            out[at + 24..at + 32].copy_from_slice(&address.to_le_bytes());
            out[at + 32..at + 40].copy_from_slice(&file.to_le_bytes());
            out[at + 40..at + 48].copy_from_slice(&memory.to_le_bytes());
        };
        header(&mut out, 64, 176, 0x100000, 16, 16);
        header(&mut out, 64 + 56, 0, 0x200000, 0, 0x8000);
        out[176..184].copy_from_slice(&X86_ENTRY_MARK.to_le_bytes());
        out[184..192].copy_from_slice(&0x100123u64.to_le_bytes());
        out
    }

    #[test]
    fn reads_segments_and_their_extent() {
        let image = elf(62);
        let parsed = Elf::parse(&image, 62).unwrap();
        assert_eq!(parsed.entry, 0x100040);
        let first = parsed.segment(0).unwrap().unwrap();
        assert_eq!((first.address, first.data.len(), first.memory_size), (0x100000, 16, 16));
        let second = parsed.segment(1).unwrap().unwrap();
        assert_eq!((second.address, second.data.len(), second.memory_size), (0x200000, 0, 0x8000));
        assert_eq!(parsed.segment(2), Ok(None));
        assert_eq!(parsed.extent(), Ok(Range { base: 0x100000, size: 0x108000 }));
        assert_eq!(parsed.marked_word(X86_ENTRY_MARK), Some(0x100123));
        assert_eq!(parsed.marked_word(7), None);
    }

    #[test]
    fn another_machine_is_refused() {
        assert!(Elf::parse(&elf(183), 62).is_none());
        assert!(Elf::parse(&[], 62).is_none());
        let mut bad = elf(62);
        bad[4] = 1;
        assert!(Elf::parse(&bad, 62).is_none());
    }

    #[test]
    fn a_segment_that_lies_is_an_error() {
        // File size past the end of the image.
        let mut bad = elf(62);
        bad[64 + 32..64 + 40].copy_from_slice(&100_000u64.to_le_bytes());
        bad[64 + 40..64 + 48].copy_from_slice(&100_000u64.to_le_bytes());
        assert_eq!(Elf::parse(&bad, 62).unwrap().segment(0), Err(()));
        // More in the file than in memory.
        let mut bad = elf(62);
        bad[64 + 40..64 + 48].copy_from_slice(&1u64.to_le_bytes());
        assert_eq!(Elf::parse(&bad, 62).unwrap().segment(0), Err(()));
        // Wrapping round the address space.
        let mut bad = elf(62);
        bad[64 + 24..64 + 32].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(Elf::parse(&bad, 62).unwrap().segment(0), Err(()));
    }

    #[test]
    fn no_input_panics() {
        let mut state = 0x7654_3210_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let good = elf(62);
        for round in 0..100_000usize {
            let mut image = good.clone();
            for _ in 0..1 + round % 6 {
                let at = next() as usize % image.len();
                image[at] = next() as u8;
            }
            let length = round % (image.len() + 1);
            if let Some(parsed) = Elf::parse(&image[..length], 62) {
                let _ = parsed.extent();
                let _ = parsed.marked_word(X86_ENTRY_MARK);
            }
            let mut out = [Range::default(); MAX_RAM_RANGES];
            usable_ram(&image[..length], 40 + round % 16, &mut out);
        }
    }

    #[test]
    fn a_handoff_is_found_by_its_mark() {
        let handoff = Handoff::new();
        let address = core::ptr::from_ref(&handoff) as usize;
        // SAFETY: the structure is right there.
        assert!(unsafe { Handoff::at(address) }.is_some());
        // SAFETY: and so is this one, which isn't one.
        let other = [0u64; 2];
        assert!(unsafe { Handoff::at(other.as_ptr() as usize) }.is_none());
        assert!(unsafe { Handoff::at(0) }.is_none());
    }
}
