//! A physical page frame allocator: one bit per 4 KiB frame.
//!
//! Boot adds the RAM firmware reports, takes out what's in use, and from then on
//! this hands out runs of contiguous frames. A bitmap costs a fixed amount of
//! memory for all the RAM it can cover, needs no memory of its own to start with,
//! and can tell a double free from a real one. A search is linear in the frames
//! passed over, which is fine for how rarely this is called.
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
}

/// Frames, up to `WORDS * 64` of them, from physical address `base` up.
pub struct Frames<const WORDS: usize> {
    /// Set bit: free to hand out.
    free: [u64; WORDS],
    /// Set bit: ever added as RAM. A free is refused for anything else.
    ram: [u64; WORDS],
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

    fn get(bits: &[u64; WORDS], frame: usize) -> bool {
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
            if !Self::get(&self.free, frame) {
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
                .find(|&candidate| !Self::get(&self.free, candidate))
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
            if !Self::get(&self.ram, frame) {
                return Err(FreeError::NotOurs);
            }
            if Self::get(&self.free, frame) {
                return Err(FreeError::NotAllocated);
            }
        }
        for frame in start..end {
            self.free[frame / WORD_BITS] |= 1 << (frame % WORD_BITS);
        }
        self.available += count;
        self.hint = self.hint.min(start / WORD_BITS);
        Ok(())
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
