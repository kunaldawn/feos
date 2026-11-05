//! A small first-fit heap for the kernel.
//!
//! One arena, a sorted free list and a 16-byte header in front of each block.
//! Allocation and deallocation are O(n) in the number of free blocks, which is
//! fine for the kernel's few `alloc` users.
//!
//! The crate holds the algorithm only. The kernel owns the arena and the lock,
//! and tells the heap which process each allocation belongs to.

#![cfg_attr(not(test), no_std)]

use core::alloc::Layout;
use core::ptr;

/// Header stored immediately before every payload, and the block granularity.
pub const HEADER: usize = 16;
/// Sentinel for "no block".
const NONE: usize = usize::MAX;
/// Marks a header as a live allocation.
const MAGIC: u32 = 0xfe05_a110;

const _: () = assert!(core::mem::size_of::<usize>() == 8);
const _: () = assert!(core::mem::size_of::<Header>() == HEADER);

#[repr(C)]
#[derive(Clone, Copy)]
struct Header {
    /// Total block size including this header.
    size: usize,
    /// Free block: offset of the next free block, or `NONE`.
    /// Allocated block: `MAGIC` in the low half, the owner in the high half.
    /// Free-list links are small offsets or `NONE`, so a free block can never
    /// pass as allocated, which is what catches a double free.
    word: usize,
}

const fn tag(owner: u32) -> usize {
    MAGIC as usize | ((owner as usize) << 32)
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}

/// Why a pointer was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeapError {
    /// The pointer is outside the arena or misaligned.
    NotInArena,
    /// The header is not a live allocation: a double free, or corruption.
    BadHeader,
}

/// What a successful [`Heap::free`] released.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Freed {
    /// Block size, including the header.
    pub size: usize,
    /// Owner passed to [`Heap::alloc`].
    pub owner: u32,
}

/// What a successful [`Heap::resize`] changed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Resized {
    /// Block size before, including the header.
    pub old_size: usize,
    /// Block size after, including the header.
    pub new_size: usize,
    /// Owner passed to [`Heap::alloc`].
    pub owner: u32,
}

/// A snapshot of the heap.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    /// Total arena size in bytes.
    pub total_bytes: usize,
    /// Bytes owned by live allocations, including headers and padding.
    pub used_bytes: usize,
    /// Bytes still on the free list.
    pub free_bytes: usize,
    /// Number of free-list entries.
    pub free_blocks: usize,
}

/// The free list over one arena.
pub struct Heap {
    base: usize,
    size: usize,
    head: usize,
    /// Bytes in live blocks, headers included, so [`stats`](Self::stats) needn't
    /// walk anything with the lock held.
    used: usize,
    /// Entries on the free list.
    free_blocks: usize,
}

impl Heap {
    /// A heap with no arena. Every allocation fails until [`init`](Self::init).
    pub const fn empty() -> Self {
        Self {
            base: 0,
            size: 0,
            head: NONE,
            used: 0,
            free_blocks: 0,
        }
    }

    /// Returns `true` once [`init`](Self::init) has run.
    pub const fn is_initialized(&self) -> bool {
        self.size != 0
    }

    /// Takes ownership of the arena `[base, base + size)`.
    ///
    /// The range is trimmed to [`HEADER`] boundaries.
    ///
    /// # Safety
    /// The range must be writable, unused and live as long as this heap.
    pub unsafe fn init(&mut self, base: *mut u8, size: usize) {
        let start = base as usize;
        let aligned = align_up(start, HEADER).expect("arena address");
        let size = size.saturating_sub(aligned - start) & !(HEADER - 1);
        self.base = aligned;
        self.size = size;
        self.head = NONE;
        self.used = 0;
        self.free_blocks = 0;
        if size >= HEADER * 2 {
            self.head = 0;
            self.free_blocks = 1;
            // SAFETY: offset 0 is inside the arena and the list is empty.
            unsafe {
                self.write(
                    0,
                    Header {
                        size,
                        word: NONE,
                    },
                );
            }
        }
    }

    unsafe fn read(&self, offset: usize) -> Header {
        // SAFETY: the caller passes an offset inside the arena.
        unsafe { ptr::read((self.base + offset) as *const Header) }
    }

    unsafe fn write(&mut self, offset: usize, header: Header) {
        // SAFETY: the caller passes an offset inside the arena.
        unsafe { ptr::write((self.base + offset) as *mut Header, header) }
    }

    /// Reads a free-list entry, refusing a corrupt link.
    fn free_entry(&self, offset: usize) -> Option<Header> {
        if offset % HEADER != 0 || offset.checked_add(HEADER)? > self.size {
            return None;
        }
        // SAFETY: the bounds were checked above.
        let header = unsafe { self.read(offset) };
        if header.size < HEADER || offset.checked_add(header.size)? > self.size {
            return None;
        }
        Some(header)
    }

    /// Reads and validates the header of a live allocation.
    fn live_block(&self, allocation: *mut u8) -> Result<(usize, Header), HeapError> {
        let address = allocation as usize;
        if address < self.base + HEADER
            || address >= self.base + self.size
            || address % HEADER != 0
        {
            return Err(HeapError::NotInArena);
        }
        let offset = address - self.base - HEADER;
        // SAFETY: the bounds were checked above.
        let header = unsafe { self.read(offset) };
        if header.word as u32 != MAGIC
            || header.size < HEADER * 2
            || header.size % HEADER != 0
            || offset + header.size > self.size
        {
            return Err(HeapError::BadHeader);
        }
        Ok((offset, header))
    }

    fn set_next(&mut self, previous: usize, next: usize) {
        if previous == NONE {
            self.head = next;
        } else {
            // SAFETY: `previous` is a free-list entry read by the caller.
            unsafe {
                let mut before = self.read(previous);
                before.word = next;
                self.write(previous, before);
            }
        }
    }

    /// Allocates one block aligned to `layout`, recorded as owned by `owner`.
    ///
    /// Returns null when no free block fits.
    pub fn alloc(&mut self, layout: Layout, owner: u32) -> *mut u8 {
        self.alloc_sized(layout, owner).0
    }

    /// Like [`alloc`](Self::alloc), but also returns the block's size including
    /// its header, which is what [`Freed::size`] will report for it.
    pub fn alloc_sized(&mut self, layout: Layout, owner: u32) -> (*mut u8, usize) {
        let align = layout.align().max(HEADER);
        let Some(size) = align_up(layout.size().max(1), HEADER) else {
            return (ptr::null_mut(), 0);
        };

        let mut previous = NONE;
        let mut current = self.head;
        while current != NONE {
            let Some(header) = self.free_entry(current) else {
                return (ptr::null_mut(), 0);
            };
            let block_end = current + header.size;

            // Align the address, not the offset: the arena base need not share
            // the caller's alignment.
            let start = self.base + current + HEADER;
            let Some(payload) = align_up(start, align).map(|address| address - self.base) else {
                return (ptr::null_mut(), 0);
            };
            let Some(end) = payload.checked_add(size) else {
                return (ptr::null_mut(), 0);
            };

            if end <= block_end {
                let header_offset = payload - HEADER;
                let prefix = header_offset - current;
                let tail = block_end - end;

                // A tail too small to ever satisfy a request stays in this
                // block, so it isn't lost as an unusable free-list entry.
                let (end, link) = if tail >= HEADER * 2 {
                    self.free_blocks += 1;
                    // SAFETY: the tail lies inside the free block.
                    unsafe {
                        self.write(
                            end,
                            Header {
                                size: tail,
                                word: header.word,
                            },
                        );
                    }
                    (end, end)
                } else {
                    (block_end, header.word)
                };

                if prefix >= HEADER {
                    // Keep the alignment padding as a free block.
                    // SAFETY: `current` is a free-list entry.
                    unsafe {
                        self.write(
                            current,
                            Header {
                                size: prefix,
                                word: link,
                            },
                        );
                    }
                } else {
                    self.free_blocks -= 1;
                    self.set_next(previous, link);
                }

                let block = end - header_offset;
                self.used += block;
                // SAFETY: the header lies inside the free block just split.
                unsafe {
                    self.write(
                        header_offset,
                        Header {
                            size: block,
                            word: tag(owner),
                        },
                    );
                }
                return ((self.base + payload) as *mut u8, block);
            }

            previous = current;
            current = header.word;
        }

        (ptr::null_mut(), 0)
    }

    /// Puts `[offset, offset + size)` on the free list, merging neighbours.
    fn insert_free(&mut self, offset: usize, size: usize) {
        let mut previous = NONE;
        let mut current = self.head;
        while current != NONE && current < offset {
            let Some(header) = self.free_entry(current) else {
                // A corrupt list can't accept the block; leak it.
                return;
            };
            previous = current;
            current = header.word;
        }

        self.used -= size;
        self.free_blocks += 1;
        let mut merged = Header {
            size,
            word: current,
        };
        if current != NONE && offset + size == current {
            if let Some(next) = self.free_entry(current) {
                merged.size += next.size;
                merged.word = next.word;
                self.free_blocks -= 1;
            }
        }

        if previous != NONE {
            // SAFETY: `previous` was validated by `free_entry` above.
            let mut before = unsafe { self.read(previous) };
            if previous + before.size == offset {
                before.size += merged.size;
                before.word = merged.word;
                // SAFETY: as above.
                unsafe { self.write(previous, before) };
                self.free_blocks -= 1;
                return;
            }
        }

        // SAFETY: `offset` is a block inside the arena that the caller owns.
        unsafe { self.write(offset, merged) };
        self.set_next(previous, offset);
    }

    /// Returns a block to the free list.
    ///
    /// A pointer that isn't a live allocation is refused rather than ignored,
    /// so the kernel can report the corruption instead of hiding it.
    pub fn free(&mut self, allocation: *mut u8) -> Result<Freed, HeapError> {
        let (offset, header) = self.live_block(allocation)?;
        self.insert_free(offset, header.size);
        Ok(Freed {
            size: header.size,
            owner: (header.word >> 32) as u32,
        })
    }

    /// Resizes a block without moving it, when that is possible.
    ///
    /// Growing takes space from an adjacent free block; shrinking returns the
    /// tail to the free list. `Ok(None)` means the block must move.
    pub fn resize(
        &mut self,
        allocation: *mut u8,
        new_size: usize,
    ) -> Result<Option<Resized>, HeapError> {
        let (offset, header) = self.live_block(allocation)?;
        let owner = (header.word >> 32) as u32;
        let Some(needed) = align_up(new_size.max(1), HEADER).and_then(|n| n.checked_add(HEADER))
        else {
            return Ok(None);
        };
        let old_size = header.size;
        let done = |new_size| {
            Ok(Some(Resized {
                old_size,
                new_size,
                owner,
            }))
        };

        if needed <= old_size {
            let tail = old_size - needed;
            if tail < HEADER * 2 {
                return done(old_size);
            }
            // SAFETY: the block is live and owned by the caller.
            unsafe {
                self.write(
                    offset,
                    Header {
                        size: needed,
                        word: header.word,
                    },
                );
            }
            self.insert_free(offset + needed, tail);
            return done(needed);
        }

        // The free list is sorted, so stop at the first entry past this block.
        let block_end = offset + old_size;
        let mut previous = NONE;
        let mut current = self.head;
        while current != NONE && current < block_end {
            let Some(entry) = self.free_entry(current) else {
                return Ok(None);
            };
            previous = current;
            current = entry.word;
        }
        if current != block_end {
            return Ok(None);
        }
        let Some(next) = self.free_entry(current) else {
            return Ok(None);
        };
        let total = old_size + next.size;
        if total < needed {
            return Ok(None);
        }

        let tail = total - needed;
        let (size, link) = if tail >= HEADER * 2 {
            self.free_blocks += 1;
            let tail_offset = offset + needed;
            // SAFETY: the tail lies inside the free neighbour being absorbed.
            unsafe {
                self.write(
                    tail_offset,
                    Header {
                        size: tail,
                        word: next.word,
                    },
                );
            }
            (needed, tail_offset)
        } else {
            (total, next.word)
        };
        self.free_blocks -= 1;
        self.used += size - old_size;
        self.set_next(previous, link);
        // SAFETY: the block is live and owned by the caller.
        unsafe {
            self.write(
                offset,
                Header {
                    size,
                    word: header.word,
                },
            );
        }
        done(size)
    }

    /// The current usage, from counters kept as blocks change hands.
    pub fn stats(&self) -> Stats {
        Stats {
            total_bytes: self.size,
            used_bytes: self.used,
            free_bytes: self.size.saturating_sub(self.used),
            free_blocks: self.free_blocks,
        }
    }

    /// Walks every block in address order and returns the bytes `owner` holds.
    ///
    /// Each block's header leads to the next, so this also proves that the
    /// arena has no gaps. Returns `None` when a header is corrupt.
    pub fn owned_bytes(&self, owner: u32) -> Option<usize> {
        let mut total = 0usize;
        let mut offset = 0usize;
        let mut free = self.head;
        while offset < self.size {
            let header = self.free_entry(offset)?;
            if offset == free {
                free = header.word;
            } else if header.word as u32 != MAGIC {
                return None;
            } else if (header.word >> 32) as u32 == owner {
                total += header.size;
            }
            offset += header.size;
        }
        (offset == self.size && free == NONE).then_some(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARENA: usize = 1 << 20;

    /// A heap over a leaked arena whose base is `skew` bytes past a page.
    fn heap(skew: usize) -> Heap {
        let arena = vec![0u8; ARENA + 4096].leak();
        let page = (4096 - arena.as_ptr() as usize % 4096) % 4096;
        let mut heap = Heap::empty();
        // SAFETY: the arena is leaked, so it outlives the heap, and the range
        // stays inside it.
        unsafe { heap.init(arena.as_mut_ptr().add(page + skew), ARENA - skew) };
        heap
    }

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    #[test]
    fn align_up_handles_power_of_two_alignment() {
        assert_eq!(align_up(0, 16), Some(0));
        assert_eq!(align_up(1, 16), Some(16));
        assert_eq!(align_up(16, 16), Some(16));
        assert_eq!(align_up(17, 16), Some(32));
        assert_eq!(align_up(usize::MAX, 16), None);
    }

    #[test]
    fn large_alignment_applies_to_the_address_not_the_offset() {
        // KRN-13: with a base that is 16 mod 64, aligning the offset returned
        // an address that was 0x30 mod 64.
        for skew in [0, 16, 32, 48] {
            let mut heap = heap(skew);
            for align in [32, 64, 128, 4096] {
                let pointer = heap.alloc(layout(64, align), 1);
                assert!(!pointer.is_null());
                assert_eq!(pointer as usize % align, 0, "skew {skew} align {align}");
            }
            assert!(heap.owned_bytes(1).is_some());
        }
    }

    #[test]
    fn free_merges_back_into_a_single_block() {
        let mut heap = heap(0);
        let total = heap.stats().free_bytes;
        let pointers: Vec<_> = (1..20).map(|n| heap.alloc(layout(n * 37, 16), 1)).collect();
        assert!(heap.stats().free_bytes < total);
        // Odd then even, so most frees land between two allocated neighbours.
        for pointer in pointers.iter().skip(1).step_by(2).chain(pointers.iter().step_by(2)) {
            heap.free(*pointer).unwrap();
        }
        let stats = heap.stats();
        assert_eq!(stats.free_bytes, total);
        assert_eq!(stats.free_blocks, 1);
    }

    #[test]
    fn double_free_is_refused() {
        let mut heap = heap(0);
        let first = heap.alloc(layout(100, 16), 7);
        let second = heap.alloc(layout(100, 16), 7);
        assert!(heap.free(first).is_ok());
        assert_eq!(heap.free(first), Err(HeapError::BadHeader));
        assert!(heap.free(second).is_ok());
        assert_eq!(heap.stats().free_blocks, 1);
    }

    #[test]
    fn bad_pointers_are_refused() {
        let mut heap = heap(0);
        let pointer = heap.alloc(layout(100, 16), 7);
        assert_eq!(heap.free(16 as *mut u8), Err(HeapError::NotInArena));
        assert_eq!(heap.free(core::ptr::null_mut()), Err(HeapError::NotInArena));
        assert_eq!(
            heap.free(unsafe { pointer.add(1) }),
            Err(HeapError::NotInArena)
        );
        assert_eq!(
            heap.free(unsafe { pointer.add(16) }),
            Err(HeapError::BadHeader)
        );

        // An overrun of the previous block's payload destroys the magic.
        let header = unsafe { pointer.sub(HEADER) };
        unsafe { header.add(8).write(0x41) };
        assert_eq!(heap.free(pointer), Err(HeapError::BadHeader));
    }

    #[test]
    fn the_size_reported_by_alloc_matches_free() {
        let mut heap = heap(0);
        // Leave a tail too small to split, so the block gets bigger than asked.
        let (first, size) = heap.alloc_sized(layout(ARENA - 64, 16), 1);
        assert_eq!(heap.free(first).unwrap().size, size);
        let (second, size) = heap.alloc_sized(layout(100, 16), 1);
        assert_eq!(size, 112 + HEADER);
        assert_eq!(heap.free(second).unwrap().size, size);
    }

    #[test]
    fn free_reports_size_and_owner() {
        let mut heap = heap(0);
        let pointer = heap.alloc(layout(100, 16), 42);
        let freed = heap.free(pointer).unwrap();
        assert_eq!(freed.owner, 42);
        assert_eq!(freed.size, 112 + HEADER);
    }

    #[test]
    fn resize_grows_into_the_free_neighbour_without_moving() {
        let mut heap = heap(0);
        let pointer = heap.alloc(layout(100, 16), 3);
        unsafe { pointer.write_bytes(0x5a, 100) };
        let resized = heap.resize(pointer, 5000).unwrap().unwrap();
        assert_eq!(resized.owner, 3);
        assert!(resized.new_size >= 5000 + HEADER);
        assert_eq!(heap.owned_bytes(3), Some(resized.new_size));
        assert_eq!(unsafe { pointer.add(99).read() }, 0x5a);
    }

    #[test]
    fn resize_refuses_when_the_neighbour_is_allocated() {
        let mut heap = heap(0);
        let pointer = heap.alloc(layout(100, 16), 3);
        let _next = heap.alloc(layout(100, 16), 3);
        assert_eq!(heap.resize(pointer, 5000), Ok(None));
    }

    #[test]
    fn resize_shrinks_and_returns_the_tail() {
        let mut heap = heap(0);
        let total = heap.stats().free_bytes;
        let pointer = heap.alloc(layout(4096, 16), 3);
        let resized = heap.resize(pointer, 16).unwrap().unwrap();
        assert_eq!(resized.new_size, 32);
        assert_eq!(heap.stats().free_blocks, 1);
        assert_eq!(heap.stats().free_bytes, total - 32);
        assert!(heap.free(pointer).is_ok());
        assert_eq!(heap.stats().free_bytes, total);
    }

    #[test]
    fn owners_are_tracked_per_block() {
        let mut heap = heap(0);
        let first = heap.alloc(layout(100, 16), 2);
        let second = heap.alloc(layout(200, 16), 3);
        let third = heap.alloc(layout(300, 16), 2);
        assert_eq!(heap.owned_bytes(2), Some(112 + 16 + 304 + 16));
        assert_eq!(heap.owned_bytes(3), Some(208 + 16));
        assert_eq!(heap.owned_bytes(9), Some(0));
        heap.free(first).unwrap();
        heap.free(third).unwrap();
        assert_eq!(heap.owned_bytes(2), Some(0));
        heap.free(second).unwrap();
    }

    #[test]
    fn exhaustion_returns_null_and_recovers() {
        let mut heap = heap(0);
        assert!(heap.alloc(layout(ARENA * 2, 16), 1).is_null());
        let everything = heap.alloc(layout(ARENA - HEADER, 16), 1);
        assert!(!everything.is_null());
        assert!(heap.alloc(layout(16, 16), 1).is_null());
        heap.free(everything).unwrap();
        assert!(!heap.alloc(layout(16, 16), 1).is_null());
    }

    #[test]
    fn empty_heap_refuses_everything() {
        let mut heap = Heap::empty();
        assert!(!heap.is_initialized());
        assert!(heap.alloc(layout(16, 16), 1).is_null());
        assert_eq!(heap.free(16 as *mut u8), Err(HeapError::NotInArena));
    }

    #[test]
    fn random_workload_keeps_the_arena_consistent() {
        let mut heap = heap(16);
        let total = heap.stats().free_bytes;
        let mut live: Vec<(*mut u8, usize, u8)> = Vec::new();
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as usize
        };

        for round in 0..20_000usize {
            let choice = next() % 10;
            if choice < 5 || live.is_empty() {
                let size = 1 + next() % 2000;
                let align = 1usize << (next() % 8);
                let pointer = heap.alloc(layout(size, align), 1);
                if !pointer.is_null() {
                    assert_eq!(pointer as usize % align, 0);
                    let fill = round as u8;
                    unsafe { pointer.write_bytes(fill, size) };
                    live.push((pointer, size, fill));
                }
            } else if choice < 8 {
                let (pointer, size, fill) = live.swap_remove(next() % live.len());
                for offset in [0, size / 2, size - 1] {
                    assert_eq!(unsafe { pointer.add(offset).read() }, fill);
                }
                heap.free(pointer).unwrap();
            } else {
                let index = next() % live.len();
                let (pointer, size, fill) = live[index];
                let new_size = 1 + next() % 4000;
                if heap.resize(pointer, new_size).unwrap().is_some() {
                    let kept = size.min(new_size);
                    assert_eq!(unsafe { pointer.add(kept - 1).read() }, fill);
                    unsafe { pointer.write_bytes(fill, new_size) };
                    live[index].1 = new_size;
                }
            }
            if round % 512 == 0 {
                assert!(heap.owned_bytes(1).is_some(), "corrupt at round {round}");
            }
        }

        for (pointer, _, _) in live {
            heap.free(pointer).unwrap();
        }
        let stats = heap.stats();
        assert_eq!(stats.free_bytes, total);
        assert_eq!(stats.free_blocks, 1);
    }

    /// The counters against the free list itself, through every kind of change.
    #[test]
    fn counters_agree_with_the_free_list() {
        fn walked(heap: &Heap) -> (usize, usize) {
            let (mut bytes, mut blocks) = (0, 0);
            let mut current = heap.head;
            while current != NONE {
                let header = heap.free_entry(current).unwrap();
                bytes += header.size;
                blocks += 1;
                current = header.word;
            }
            (bytes, blocks)
        }

        let mut arena = vec![0u128; 16 * 1024];
        let mut heap = Heap::empty();
        // SAFETY: the arena outlives the heap and nothing else uses it.
        unsafe { heap.init(arena.as_mut_ptr().cast(), 16 * 1024 * 16) };

        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as usize
        };
        let mut live: Vec<(*mut u8, usize)> = Vec::new();
        for _ in 0..5_000 {
            match next() % 4 {
                0 | 1 => {
                    let align = 1 << (next() % 8);
                    let size = next() % 3000 + 1;
                    let layout = Layout::from_size_align(size, align).unwrap();
                    let block = heap.alloc(layout, 1);
                    if !block.is_null() {
                        live.push((block, size));
                    }
                }
                2 if !live.is_empty() => {
                    let (block, _) = live.swap_remove(next() % live.len());
                    heap.free(block).unwrap();
                }
                3 if !live.is_empty() => {
                    let index = next() % live.len();
                    let size = next() % 4000 + 1;
                    if heap.resize(live[index].0, size).unwrap().is_some() {
                        live[index].1 = size;
                    }
                }
                _ => {}
            }
            let stats = heap.stats();
            assert_eq!((stats.free_bytes, stats.free_blocks), walked(&heap));
            assert_eq!(stats.free_bytes + stats.used_bytes, stats.total_bytes);
        }
    }
}
