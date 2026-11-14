//! A physical page frame allocator: one bit per 4 KiB frame.
//!
//! Boot adds the RAM firmware reports, takes out what's in use, and from then on
//! this hands out runs of contiguous frames. A bitmap costs a fixed amount of
//! memory for all the RAM it can cover, needs no memory of its own to start with,
//! and can tell a double free from a real one. A search is linear in the frames
//! passed over, which is fine for how rarely this is called.
//!
//! # Counts
//!
//! Beside the bitmap, each frame has four bytes of [metadata](Meta): a map count (the
//! page-table entries, and other owners such as a shared-anonymous object, that point
//! at it), a pin count (copies in flight through it) and a few flags. A frame handed
//! out by [`alloc_counted`](Frames::alloc_counted) is freed when the last of both goes,
//! whichever that is, so a copy that pinned a frame keeps it alive past an unmap. A
//! frame from [`alloc`](Frames::alloc) has no counts and goes back with
//! [`free`](Frames::free). A count at its cap refuses another reference rather than
//! wrap: the caller says `ENOMEM` for a mapping and `EAGAIN` for a pin.
//!
//! The crate holds the algorithm only. The kernel owns the static and the lock.

#![cfg_attr(not(test), no_std)]

/// Bytes in a frame.
pub const FRAME_SIZE: u64 = 4096;

const WORD_BITS: usize = u64::BITS as usize;

/// Why a free was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FreeError {
    /// The address isn't on a frame boundary, or isn't RAM this allocator has.
    NotOurs,
    /// A frame in the run wasn't allocated: a double free.
    NotAllocated,
    /// A frame in the run is counted: it goes when its counts reach zero, not before.
    Counted,
}

/// Why a count couldn't change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CountError {
    /// The address isn't on a frame boundary, or isn't RAM this allocator has.
    NotOurs,
    /// The frame isn't counted, or has no reference of the kind to drop: a double put.
    NotCounted,
    /// The count is at its cap.
    Saturated,
}

/// Most map references one frame can have.
pub const MAX_MAPS: u32 = (1 << 20) - 1;
/// Most copies one frame can be pinned by at once.
pub const MAX_PINS: u32 = (1 << 9) - 1;

/// One frame's counts and flags, packed into a word.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Meta(u32);

const MAP_BITS: u32 = MAX_MAPS;
const PIN_SHIFT: u32 = 20;
const PIN_BITS: u32 = MAX_PINS << PIN_SHIFT;

impl Meta {
    /// Page-table entries and other owners pointing at the frame.
    pub const fn maps(self) -> u32 {
        self.0 & MAP_BITS
    }

    /// Copies in flight through the frame.
    pub const fn pins(self) -> u32 {
        (self.0 & PIN_BITS) >> PIN_SHIFT
    }

    /// Whether every flag in `flags` is set.
    pub const fn has(self, flags: u32) -> bool {
        self.0 & flags == flags
    }
}

/// Flags in a frame's [`Meta`].
pub mod flag {
    /// The page cache owns the frame, rather than an anonymous mapping.
    pub const PAGE_CACHE: u32 = 1 << 29;
    /// The frame is shared copy-on-write.
    pub const COPY_ON_WRITE: u32 = 1 << 30;
    /// The instruction cache has been made to agree with the frame's contents.
    pub const ICACHE_SYNCED: u32 = 1 << 31;
    /// Every flag there is.
    pub const ALL: u32 = PAGE_CACHE | COPY_ON_WRITE | ICACHE_SYNCED;
}

/// Frames, up to `WORDS * 64` of them, from physical address `base` up.
pub struct Frames<const WORDS: usize> {
    /// Set bit: free to hand out.
    free: [u64; WORDS],
    /// Set bit: ever added as RAM. A free is refused for anything else.
    ram: [u64; WORDS],
    /// Counts and flags, a word of them per frame.
    meta: [[Meta; WORD_BITS]; WORDS],
    base: u64,
    total: usize,
    available: usize,
    /// Where the next search starts. Every word below this has no free frame.
    hint: usize,
}

impl<const WORDS: usize> Frames<WORDS> {
    /// An allocator with no RAM, covering addresses from `base` up.
    pub const fn new(base: u64) -> Self {
        Self {
            free: [0; WORDS],
            ram: [0; WORDS],
            meta: [[Meta(0); WORD_BITS]; WORDS],
            base: base & !(FRAME_SIZE - 1),
            total: 0,
            available: 0,
            hint: 0,
        }
    }

    /// One past the highest address this allocator can cover.
    pub const fn limit(&self) -> u64 {
        self.base + (WORDS * WORD_BITS) as u64 * FRAME_SIZE
    }

    /// The whole frames inside a range, as an index range, clipped to what's covered.
    fn inner_frames(&self, base: u64, size: u64) -> (usize, usize) {
        let end = base.saturating_add(size).min(self.limit());
        let start = base.max(self.base).saturating_add(FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
        let end = end & !(FRAME_SIZE - 1);
        if end <= start {
            return (0, 0);
        }
        (((start - self.base) / FRAME_SIZE) as usize, ((end - self.base) / FRAME_SIZE) as usize)
    }

    /// Every frame touching a range, clipped to what's covered.
    fn outer_frames(&self, base: u64, size: u64) -> (usize, usize) {
        let end = base
            .saturating_add(size)
            .saturating_add(FRAME_SIZE - 1)
            .min(self.limit());
        let start = base.max(self.base) & !(FRAME_SIZE - 1);
        let end = end & !(FRAME_SIZE - 1);
        if end <= start {
            return (0, 0);
        }
        (((start - self.base) / FRAME_SIZE) as usize, ((end - self.base) / FRAME_SIZE) as usize)
    }

    fn bit(bits: &[u64; WORDS], frame: usize) -> bool {
        bits[frame / WORD_BITS] >> (frame % WORD_BITS) & 1 != 0
    }

    /// Adds RAM, using only the whole frames inside the range, so a partial frame
    /// at either end isn't touched. Frames covered twice count once.
    pub fn add_ram(&mut self, base: u64, size: u64) {
        let (start, end) = self.inner_frames(base, size);
        for frame in start..end {
            let (word, bit) = (frame / WORD_BITS, 1u64 << (frame % WORD_BITS));
            if self.ram[word] & bit == 0 {
                self.ram[word] |= bit;
                self.free[word] |= bit;
                self.total += 1;
                self.available += 1;
            }
        }
        self.hint = self.hint.min(start / WORD_BITS);
    }

    /// Marks everything touching a range as in use for good, whether or not it's
    /// RAM. Its frames still count towards the total.
    pub fn reserve(&mut self, base: u64, size: u64) {
        let (start, end) = self.outer_frames(base, size);
        for frame in start..end {
            let (word, bit) = (frame / WORD_BITS, 1u64 << (frame % WORD_BITS));
            if self.free[word] & bit != 0 {
                self.free[word] &= !bit;
                self.available -= 1;
            }
        }
    }

    /// Takes `count` contiguous frames, aligned to `align` frames, a power of two.
    /// Returns the physical address of the first.
    pub fn alloc(&mut self, count: usize, align: usize) -> Option<u64> {
        if count == 0 || !align.is_power_of_two() || count > self.available {
            return None;
        }
        let frames = WORDS * WORD_BITS;
        let mut frame = self.hint * WORD_BITS;
        let mut first_free = None;
        while frame < frames {
            // A word with nothing free can be stepped over whole.
            if frame.is_multiple_of(WORD_BITS) && self.free[frame / WORD_BITS] == 0 {
                frame += WORD_BITS;
                continue;
            }
            if !Self::bit(&self.free, frame) {
                frame += 1;
                continue;
            }
            first_free.get_or_insert(frame);
            // Alignment is of the address, as a device or a page table needs it.
            let address_frame = (self.base / FRAME_SIZE) as usize + frame;
            if !address_frame.is_multiple_of(align) {
                frame += align - address_frame % align;
                continue;
            }
            if let Some(blocker) = (frame..frame + count)
                .take_while(|&candidate| candidate < frames)
                .find(|&candidate| !Self::bit(&self.free, candidate))
            {
                frame = blocker + 1;
                continue;
            }
            if frame + count > frames {
                return None;
            }
            for taken in frame..frame + count {
                self.free[taken / WORD_BITS] &= !(1 << (taken % WORD_BITS));
            }
            self.available -= count;
            // Words below the first free frame seen are still all taken.
            if let Some(first) = first_free {
                self.hint = first / WORD_BITS;
            }
            return Some(self.base + frame as u64 * FRAME_SIZE);
        }
        None
    }

    /// Returns a run of frames from [`alloc`](Self::alloc). All of it is checked
    /// before any of it changes.
    pub fn free(&mut self, address: u64, count: usize) -> Result<(), FreeError> {
        if !address.is_multiple_of(FRAME_SIZE) || address < self.base {
            return Err(FreeError::NotOurs);
        }
        let start = ((address - self.base) / FRAME_SIZE) as usize;
        let end = start.checked_add(count).ok_or(FreeError::NotOurs)?;
        if end > WORDS * WORD_BITS {
            return Err(FreeError::NotOurs);
        }
        for frame in start..end {
            if !Self::bit(&self.ram, frame) {
                return Err(FreeError::NotOurs);
            }
            if Self::bit(&self.free, frame) {
                return Err(FreeError::NotAllocated);
            }
            if self.meta_at(frame).0 & (MAP_BITS | PIN_BITS) != 0 {
                return Err(FreeError::Counted);
            }
        }
        for frame in start..end {
            self.free[frame / WORD_BITS] |= 1 << (frame % WORD_BITS);
            *self.meta_mut(frame) = Meta(0);
        }
        self.available += count;
        self.hint = self.hint.min(start / WORD_BITS);
        Ok(())
    }

    fn meta_at(&self, frame: usize) -> Meta {
        self.meta[frame / WORD_BITS][frame % WORD_BITS]
    }

    fn meta_mut(&mut self, frame: usize) -> &mut Meta {
        &mut self.meta[frame / WORD_BITS][frame % WORD_BITS]
    }

    /// The index of an allocated frame at `address`.
    fn allocated(&self, address: u64) -> Result<usize, CountError> {
        if !address.is_multiple_of(FRAME_SIZE) || address < self.base || address >= self.limit() {
            return Err(CountError::NotOurs);
        }
        let frame = ((address - self.base) / FRAME_SIZE) as usize;
        if !Self::bit(&self.ram, frame) || Self::bit(&self.free, frame) {
            return Err(CountError::NotOurs);
        }
        Ok(frame)
    }

    /// Takes one frame with a map count of one, the caller's reference. It's freed when
    /// that and every later reference and pin are gone.
    pub fn alloc_counted(&mut self) -> Option<u64> {
        let address = self.alloc(1, 1)?;
        let frame = ((address - self.base) / FRAME_SIZE) as usize;
        *self.meta_mut(frame) = Meta(1);
        Some(address)
    }

    /// A counted frame's counts and flags.
    pub fn meta(&self, address: u64) -> Result<Meta, CountError> {
        self.allocated(address).map(|frame| self.meta_at(frame))
    }

    /// Adds a map reference to a counted frame that has one already.
    pub fn get(&mut self, address: u64) -> Result<(), CountError> {
        let frame = self.allocated(address)?;
        let meta = self.meta_mut(frame);
        match meta.maps() {
            0 => Err(CountError::NotCounted),
            MAX_MAPS => Err(CountError::Saturated),
            _ => {
                meta.0 += 1;
                Ok(())
            }
        }
    }

    /// Drops a map reference. Returns whether that freed the frame: it was the last
    /// reference, and nothing has it pinned.
    pub fn put(&mut self, address: u64) -> Result<bool, CountError> {
        let frame = self.allocated(address)?;
        let meta = self.meta_mut(frame);
        if meta.maps() == 0 {
            return Err(CountError::NotCounted);
        }
        meta.0 -= 1;
        Ok(self.free_if_unused(frame))
    }

    /// Pins a counted frame that's mapped, for a copy through it.
    pub fn pin(&mut self, address: u64) -> Result<(), CountError> {
        let frame = self.allocated(address)?;
        let meta = self.meta_mut(frame);
        if meta.maps() == 0 {
            return Err(CountError::NotCounted);
        }
        if meta.pins() == MAX_PINS {
            return Err(CountError::Saturated);
        }
        meta.0 += 1 << PIN_SHIFT;
        Ok(())
    }

    /// Drops a pin. Returns whether that freed the frame, its last mapping having gone
    /// while the copy was using it.
    pub fn unpin(&mut self, address: u64) -> Result<bool, CountError> {
        let frame = self.allocated(address)?;
        let meta = self.meta_mut(frame);
        if meta.pins() == 0 {
            return Err(CountError::NotCounted);
        }
        meta.0 -= 1 << PIN_SHIFT;
        Ok(self.free_if_unused(frame))
    }

    /// Sets or clears flags of a counted frame.
    pub fn set_flags(&mut self, address: u64, flags: u32, on: bool) -> Result<(), CountError> {
        let frame = self.allocated(address)?;
        let meta = self.meta_mut(frame);
        if meta.0 & (MAP_BITS | PIN_BITS) == 0 {
            return Err(CountError::NotCounted);
        }
        let flags = flags & flag::ALL;
        if on {
            meta.0 |= flags;
        } else {
            meta.0 &= !flags;
        }
        Ok(())
    }

    fn free_if_unused(&mut self, frame: usize) -> bool {
        if self.meta_at(frame).0 & (MAP_BITS | PIN_BITS) != 0 {
            return false;
        }
        *self.meta_mut(frame) = Meta(0);
        self.free[frame / WORD_BITS] |= 1 << (frame % WORD_BITS);
        self.available += 1;
        self.hint = self.hint.min(frame / WORD_BITS);
        true
    }

    /// Bytes of RAM added.
    pub const fn total_bytes(&self) -> u64 {
        self.total as u64 * FRAME_SIZE
    }

    /// Bytes free to hand out.
    pub const fn free_bytes(&self) -> u64 {
        self.available as u64 * FRAME_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Small = Frames<4>;
    const BASE: u64 = 0x4000_0000;

    fn ram(frames: u64) -> Small {
        let mut allocator = Small::new(BASE);
        allocator.add_ram(BASE, frames * FRAME_SIZE);
        allocator
    }

    #[test]
    fn hands_out_each_frame_once_and_takes_it_back() {
        let mut allocator = ram(4);
        let taken: Vec<u64> = (0..4).map(|_| allocator.alloc(1, 1).unwrap()).collect();
        assert_eq!(taken, [BASE, BASE + 0x1000, BASE + 0x2000, BASE + 0x3000]);
        assert_eq!(allocator.alloc(1, 1), None);
        assert_eq!(allocator.free_bytes(), 0);

        assert_eq!(allocator.free(BASE + 0x1000, 1), Ok(()));
        assert_eq!(allocator.alloc(1, 1), Some(BASE + 0x1000));
    }

    #[test]
    fn a_run_is_contiguous_or_not_at_all() {
        let mut allocator = ram(8);
        let first = allocator.alloc(3, 1).unwrap();
        let second = allocator.alloc(3, 1).unwrap();
        assert_eq!(second, first + 3 * FRAME_SIZE);
        allocator.free(first, 3).unwrap();
        // There are five free frames, but no run of four.
        assert_eq!(allocator.alloc(4, 1), None);
        assert_eq!(allocator.alloc(3, 1), Some(first));
    }

    #[test]
    fn alignment_is_of_the_address() {
        // A base that's not itself aligned to what's asked for.
        let mut allocator = Small::new(0x1000);
        allocator.add_ram(0x1000, 64 * FRAME_SIZE);
        let address = allocator.alloc(2, 16).unwrap();
        assert_eq!(address % (16 * FRAME_SIZE), 0);
        assert_eq!(allocator.alloc(1, 3), None);
        assert_eq!(allocator.alloc(0, 1), None);
    }

    #[test]
    fn reserved_memory_is_never_handed_out() {
        let mut allocator = ram(8);
        // Touching a frame's last byte reserves all of it.
        allocator.reserve(BASE + 0xfff, 1);
        allocator.reserve(BASE + 3 * FRAME_SIZE, FRAME_SIZE + 1);
        assert_eq!(allocator.free_bytes(), 5 * FRAME_SIZE);
        assert_eq!(allocator.total_bytes(), 8 * FRAME_SIZE);
        let mut taken = Vec::new();
        while let Some(address) = allocator.alloc(1, 1) {
            taken.push(address);
        }
        assert_eq!(taken, [BASE + 0x1000, BASE + 0x2000, BASE + 0x5000, BASE + 0x6000, BASE + 0x7000]);
    }

    #[test]
    fn partial_frames_of_ram_are_not_used() {
        let mut allocator = Small::new(0);
        allocator.add_ram(0x800, 0x2000);
        // 0x800..0x2800 holds only the whole frame at 0x1000.
        assert_eq!(allocator.total_bytes(), FRAME_SIZE);
        assert_eq!(allocator.alloc(1, 1), Some(0x1000));
    }

    #[test]
    fn ram_outside_the_range_is_ignored() {
        let mut allocator = Small::new(BASE);
        allocator.add_ram(0, BASE);
        allocator.add_ram(allocator.limit(), 1 << 40);
        assert_eq!(allocator.total_bytes(), 0);
        allocator.add_ram(BASE - FRAME_SIZE, 2 * FRAME_SIZE);
        assert_eq!(allocator.total_bytes(), FRAME_SIZE);
        allocator.add_ram(u64::MAX - 10, 100);
        allocator.reserve(u64::MAX - 10, 100);
    }

    #[test]
    fn overlapping_ram_counts_once() {
        let mut allocator = ram(4);
        allocator.add_ram(BASE, 4 * FRAME_SIZE);
        assert_eq!(allocator.total_bytes(), 4 * FRAME_SIZE);
    }

    #[test]
    fn bad_frees_are_refused_and_change_nothing() {
        let mut allocator = ram(8);
        let address = allocator.alloc(2, 1).unwrap();
        assert_eq!(allocator.free(address + 1, 1), Err(FreeError::NotOurs));
        assert_eq!(allocator.free(BASE - FRAME_SIZE, 1), Err(FreeError::NotOurs));
        assert_eq!(allocator.free(BASE + 8 * FRAME_SIZE, 1), Err(FreeError::NotOurs));
        assert_eq!(allocator.free(address, usize::MAX), Err(FreeError::NotOurs));
        // The run's last frame was never allocated, so no part of it is freed.
        assert_eq!(allocator.free(address, 3), Err(FreeError::NotAllocated));
        assert_eq!(allocator.free_bytes(), 6 * FRAME_SIZE);

        assert_eq!(allocator.free(address, 2), Ok(()));
        assert_eq!(allocator.free(address, 2), Err(FreeError::NotAllocated));
    }

    #[test]
    fn a_run_at_the_very_top_fits() {
        let mut allocator = Small::new(0);
        let frames = 4 * 64;
        allocator.add_ram(0, frames as u64 * FRAME_SIZE);
        allocator.reserve(0, (frames as u64 - 2) * FRAME_SIZE);
        assert_eq!(allocator.alloc(3, 1), None);
        assert_eq!(allocator.alloc(2, 1), Some((frames as u64 - 2) * FRAME_SIZE));
    }

    #[test]
    fn count_up_and_down_frees_at_zero() {
        let mut allocator = ram(4);
        let frame = allocator.alloc_counted().unwrap();
        assert_eq!(allocator.free_bytes(), 3 * FRAME_SIZE);
        allocator.get(frame).unwrap();
        allocator.get(frame).unwrap();
        assert_eq!(allocator.meta(frame).unwrap().maps(), 3);
        assert_eq!(allocator.put(frame), Ok(false));
        assert_eq!(allocator.put(frame), Ok(false));
        assert_eq!(allocator.free_bytes(), 3 * FRAME_SIZE);
        assert_eq!(allocator.put(frame), Ok(true));
        assert_eq!(allocator.free_bytes(), 4 * FRAME_SIZE);
        // And handed out again, with no counts left over.
        let again = allocator.alloc(1, 1).unwrap();
        assert_eq!(again, frame);
        assert_eq!(allocator.meta(again).unwrap(), Meta::default());
        allocator.free(again, 1).unwrap();
    }

    #[test]
    fn double_free_refused() {
        let mut allocator = ram(4);
        let frame = allocator.alloc_counted().unwrap();
        // A counted frame doesn't go back by hand while it's referenced.
        assert_eq!(allocator.free(frame, 1), Err(FreeError::Counted));
        assert_eq!(allocator.put(frame), Ok(true));
        // Gone: a second put, a get, a pin or a free are all refused.
        assert_eq!(allocator.put(frame), Err(CountError::NotOurs));
        assert_eq!(allocator.get(frame), Err(CountError::NotOurs));
        assert_eq!(allocator.pin(frame), Err(CountError::NotOurs));
        assert_eq!(allocator.free(frame, 1), Err(FreeError::NotAllocated));
        // A frame that was never counted has no reference to drop.
        let plain = allocator.alloc(1, 1).unwrap();
        assert_eq!(allocator.put(plain), Err(CountError::NotCounted));
        assert_eq!(allocator.get(plain), Err(CountError::NotCounted));
        assert_eq!(allocator.unpin(plain), Err(CountError::NotCounted));
        assert_eq!(allocator.put(plain + 1), Err(CountError::NotOurs));
        assert_eq!(allocator.free_bytes(), 3 * FRAME_SIZE);
        allocator.free(plain, 1).unwrap();
    }

    #[test]
    fn map_count_saturation_refused() {
        let mut allocator = ram(2);
        let frame = allocator.alloc_counted().unwrap();
        for _ in 1..MAX_MAPS {
            allocator.get(frame).unwrap();
        }
        assert_eq!(allocator.meta(frame).unwrap().maps(), MAX_MAPS);
        assert_eq!(allocator.get(frame), Err(CountError::Saturated));
        // The pins and flags beside it weren't touched by the refused one.
        assert_eq!(allocator.meta(frame).unwrap().pins(), 0);
        for _ in 0..MAX_MAPS - 1 {
            assert_eq!(allocator.put(frame), Ok(false));
        }
        assert_eq!(allocator.put(frame), Ok(true));
        assert_eq!(allocator.free_bytes(), 2 * FRAME_SIZE);
    }

    #[test]
    fn pin_cap_refused() {
        let mut allocator = ram(2);
        let frame = allocator.alloc_counted().unwrap();
        for _ in 0..MAX_PINS {
            allocator.pin(frame).unwrap();
        }
        assert_eq!(allocator.pin(frame), Err(CountError::Saturated));
        assert_eq!(allocator.meta(frame).unwrap().maps(), 1);
        // Unmapped while pinned: it stays until the last copy is done with it.
        assert_eq!(allocator.put(frame), Ok(false));
        assert_eq!(allocator.free_bytes(), FRAME_SIZE);
        // Nothing new may pin or map it once its mappings are gone.
        assert_eq!(allocator.get(frame), Err(CountError::NotCounted));
        assert_eq!(allocator.pin(frame), Err(CountError::NotCounted));
        for _ in 0..MAX_PINS - 1 {
            assert_eq!(allocator.unpin(frame), Ok(false));
        }
        assert_eq!(allocator.unpin(frame), Ok(true));
        assert_eq!(allocator.free_bytes(), 2 * FRAME_SIZE);
    }

    #[test]
    fn flags_live_beside_the_counts() {
        let mut allocator = ram(2);
        let frame = allocator.alloc_counted().unwrap();
        allocator.set_flags(frame, flag::ICACHE_SYNCED | flag::COPY_ON_WRITE, true).unwrap();
        allocator.get(frame).unwrap();
        let meta = allocator.meta(frame).unwrap();
        assert!(meta.has(flag::ICACHE_SYNCED | flag::COPY_ON_WRITE));
        assert!(!meta.has(flag::PAGE_CACHE));
        assert_eq!(meta.maps(), 2);
        allocator.set_flags(frame, flag::COPY_ON_WRITE, false).unwrap();
        assert!(!allocator.meta(frame).unwrap().has(flag::COPY_ON_WRITE));
        allocator.put(frame).unwrap();
        assert_eq!(allocator.put(frame), Ok(true));
        // The next owner starts with none.
        let again = allocator.alloc_counted().unwrap();
        assert_eq!(allocator.meta(again).unwrap(), Meta(1));
    }

    /// Random allocations against a model of who owns what.
    #[test]
    fn nothing_is_ever_handed_out_twice() {
        let mut allocator = Frames::<8>::new(0);
        allocator.add_ram(0, 400 * FRAME_SIZE);
        allocator.reserve(0, 3 * FRAME_SIZE);
        let mut owned = vec![false; 512];
        let mut live: Vec<(u64, usize)> = Vec::new();
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..20_000 {
            if next() % 3 != 0 || live.is_empty() {
                let count = (next() % 8) as usize + 1;
                let align = 1 << (next() % 4);
                if let Some(address) = allocator.alloc(count, align) {
                    assert_eq!(address / FRAME_SIZE % align as u64, 0);
                    for frame in 0..count {
                        let index = (address / FRAME_SIZE) as usize + frame;
                        assert!(index >= 3 && index < 400);
                        assert!(!owned[index], "frame {index} handed out twice");
                        owned[index] = true;
                    }
                    live.push((address, count));
                }
            } else {
                let (address, count) = live.swap_remove(next() as usize % live.len());
                allocator.free(address, count).unwrap();
                for frame in 0..count {
                    owned[(address / FRAME_SIZE) as usize + frame] = false;
                }
            }
            let in_use = live.iter().map(|run| run.1).sum::<usize>() as u64;
            assert_eq!(allocator.free_bytes(), (397 - in_use) * FRAME_SIZE);
        }
    }
}
