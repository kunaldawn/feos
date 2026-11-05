//! A process's address space: its user page tables and the frames they point at.
//!
//! Both ports walk the same tree for user memory: three levels of 512 entries, for
//! 1 GiB, 2 MiB and 4 KiB. On AArch64 that's the top of a 39-bit translation; on
//! x86_64 it's a page directory pointer table under the process's PML4. What differs
//! is how an entry is written, which the port says in a [`PageBits`].
//!
//! # The kernel never dereferences a user address
//!
//! [`AddressSpace::copy_in`] and [`AddressSpace::copy_out`] walk the tables and copy
//! through the kernel's own mapping of the frame, which the identity map makes the
//! frame's physical address. So a wild pointer from a program is a failed lookup
//! here, not a fault in the kernel, and there's no fixup table: nothing to get wrong
//! and no fault to take. It also lets SMAP and PAN stay on all the time. A process
//! has one thread, so nothing changes its tables during a call.
//!
//! The memory this manages is what [`FrameSource`] hands out. A frame's address must be
//! usable as a pointer, as it is on an identity map and in a host test.

#![cfg_attr(not(test), no_std)]

pub use feos_hal::{PageAccess, PageBits};

/// Bytes in a page.
pub const PAGE_SIZE: usize = 4096;
const ENTRIES: usize = 512;
const L1_SHIFT: usize = 30;
const L2_SHIFT: usize = 21;
const L3_SHIFT: usize = 12;

/// Where page tables and pages come from, and go back to.
pub trait FrameSource {
    /// A free frame. It needn't be cleared.
    fn alloc(&mut self) -> Option<usize>;
    /// Gives a frame back.
    fn free(&mut self, frame: usize);
}

/// Why a mapping change failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// The range isn't page aligned or isn't inside the window.
    BadRange,
    /// Something's mapped there already.
    AlreadyMapped,
    /// No more frames, or the limit was reached.
    OutOfMemory,
}

/// A user pointer that isn't good for the access asked of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadAddress;

/// Reads one table entry.
///
/// # Safety
/// `table` is a page table and `index` is in it.
unsafe fn read_entry(table: usize, index: usize) -> u64 {
    // SAFETY: as the caller promises.
    unsafe { (table as *const u64).add(index).read_volatile() }
}

/// Writes one table entry.
///
/// # Safety
/// As for [`read_entry`].
unsafe fn write_entry(table: usize, index: usize, value: u64) {
    // SAFETY: as the caller promises.
    unsafe { (table as *mut u64).add(index).write_volatile(value) }
}

/// One process's mappings in `[base, end)`.
#[derive(Debug)]
pub struct AddressSpace {
    /// The top table these mappings are in. Its other entries aren't this code's.
    root: usize,
    base: usize,
    end: usize,
    bits: PageBits,
    /// Pages mapped that are this space's to free.
    owned_pages: usize,
    /// The most pages this space may own.
    page_limit: usize,
}

impl AddressSpace {
    /// An empty space in a fresh table. `window` must be page aligned and fit the tree.
    pub fn new(
        frames: &mut dyn FrameSource,
        bits: PageBits,
        window: (usize, usize),
        page_limit: usize,
    ) -> Option<Self> {
        let (base, end) = window;
        if base % PAGE_SIZE != 0 || end % PAGE_SIZE != 0 || base >= end || end > (ENTRIES << L1_SHIFT) {
            return None;
        }
        let root = new_table(frames)?;
        Some(Self {
            root,
            base,
            end,
            bits,
            owned_pages: 0,
            page_limit,
        })
    }

    /// The top table's address.
    pub const fn root(&self) -> usize {
        self.root
    }

    /// Pages mapped and owned.
    pub const fn pages(&self) -> usize {
        self.owned_pages
    }

    /// Whether `[address, address + length)` is inside the window and doesn't wrap.
    pub fn in_window(&self, address: usize, length: usize) -> bool {
        address >= self.base && length <= self.end - self.base && address <= self.end - length
    }

    fn range_is_pages(&self, address: usize, length: usize) -> bool {
        length != 0
            && address % PAGE_SIZE == 0
            && length % PAGE_SIZE == 0
            && self.in_window(address, length)
    }

    /// The entry of the leaf table for `address`, if the tables are there.
    fn leaf_slot(&self, address: usize) -> Option<(usize, usize)> {
        let mut table = self.root;
        for shift in [L1_SHIFT, L2_SHIFT] {
            let index = (address >> shift) & (ENTRIES - 1);
            // SAFETY: `table` came from here, and the index is masked to fit.
            let entry = unsafe { read_entry(table, index) };
            if entry & self.bits.present == 0 {
                return None;
            }
            table = (entry & self.bits.address) as usize;
        }
        Some((table, (address >> L3_SHIFT) & (ENTRIES - 1)))
    }

    /// As [`leaf_slot`](Self::leaf_slot), making the tables that aren't there.
    fn leaf_slot_or_new(&mut self, address: usize, frames: &mut dyn FrameSource) -> Option<(usize, usize)> {
        let mut table = self.root;
        for shift in [L1_SHIFT, L2_SHIFT] {
            let index = (address >> shift) & (ENTRIES - 1);
            // SAFETY: `table` came from here, and the index is masked to fit.
            let entry = unsafe { read_entry(table, index) };
            if entry & self.bits.present != 0 {
                table = (entry & self.bits.address) as usize;
            } else {
                let next = new_table(frames)?;
                // SAFETY: as above.
                unsafe { write_entry(table, index, next as u64 | self.bits.table) };
                table = next;
            }
        }
        Some((table, (address >> L3_SHIFT) & (ENTRIES - 1)))
    }

    /// Maps fresh, zeroed pages over `[address, address + length)`. On failure, what it
    /// mapped so far is unmapped again.
    pub fn map_new(
        &mut self,
        address: usize,
        length: usize,
        access: PageAccess,
        frames: &mut dyn FrameSource,
    ) -> Result<(), MapError> {
        if !self.range_is_pages(address, length) {
            return Err(MapError::BadRange);
        }
        if self.owned_pages.saturating_add(length / PAGE_SIZE) > self.page_limit {
            return Err(MapError::OutOfMemory);
        }
        let mut done = 0;
        while done < length {
            let result = self.map_one(address + done, access, frames);
            if let Err(error) = result {
                if done != 0 {
                    self.unmap(address, done, frames);
                }
                return Err(error);
            }
            done += PAGE_SIZE;
        }
        Ok(())
    }

    fn map_one(&mut self, address: usize, access: PageAccess, frames: &mut dyn FrameSource) -> Result<(), MapError> {
        let (table, index) = self.leaf_slot_or_new(address, frames).ok_or(MapError::OutOfMemory)?;
        // SAFETY: the slot is in a table of this space.
        if unsafe { read_entry(table, index) } & self.bits.present != 0 {
            return Err(MapError::AlreadyMapped);
        }
        let frame = frames.alloc().ok_or(MapError::OutOfMemory)?;
        // SAFETY: the source just gave this frame, and it's a page long.
        unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE) };
        // SAFETY: the slot is in a table of this space.
        unsafe { write_entry(table, index, frame as u64 | self.bits.leaf(access)) };
        self.owned_pages += 1;
        Ok(())
    }

    /// Maps frames this space doesn't own, for memory shared with the kernel. They
    /// must be taken out with [`unmap_shared`](Self::unmap_shared).
    pub fn map_shared(
        &mut self,
        address: usize,
        first_frame: usize,
        length: usize,
        access: PageAccess,
        frames: &mut dyn FrameSource,
    ) -> Result<(), MapError> {
        if !self.range_is_pages(address, length) || first_frame % PAGE_SIZE != 0 {
            return Err(MapError::BadRange);
        }
        let mut done = 0;
        while done < length {
            let (table, index) = self
                .leaf_slot_or_new(address + done, frames)
                .ok_or(MapError::OutOfMemory)?;
            // SAFETY: the slot is in a table of this space.
            if unsafe { read_entry(table, index) } & self.bits.present != 0 {
                return Err(MapError::AlreadyMapped);
            }
            let frame = first_frame + done;
            // SAFETY: as above.
            unsafe { write_entry(table, index, frame as u64 | self.bits.leaf(access)) };
            done += PAGE_SIZE;
        }
        Ok(())
    }

    /// Unmaps what [`map_shared`](Self::map_shared) mapped, leaving the frames alone.
    pub fn unmap_shared(&mut self, address: usize, length: usize) {
        let mut done = 0;
        while done < length {
            if let Some((table, index)) = self.leaf_slot(address + done) {
                // SAFETY: the slot is in a table of this space.
                unsafe { write_entry(table, index, 0) };
            }
            done += PAGE_SIZE;
        }
    }

    /// Unmaps and frees the pages in the range, and calls `on_page` for each address
    /// that was mapped, for flushing. Holes are fine. Returns the pages freed.
    pub fn unmap_each(
        &mut self,
        address: usize,
        length: usize,
        frames: &mut dyn FrameSource,
        on_page: &mut dyn FnMut(usize),
    ) -> Result<usize, MapError> {
        if !self.range_is_pages(address, length) {
            return Err(MapError::BadRange);
        }
        let mut freed = 0;
        let mut done = 0;
        while done < length {
            let page = address + done;
            done += PAGE_SIZE;
            let Some((table, index)) = self.leaf_slot(page) else {
                continue;
            };
            // SAFETY: the slot is in a table of this space.
            let entry = unsafe { read_entry(table, index) };
            if entry & self.bits.present == 0 {
                continue;
            }
            // SAFETY: as above.
            unsafe { write_entry(table, index, 0) };
            on_page(page);
            frames.free((entry & self.bits.address) as usize);
            self.owned_pages -= 1;
            freed += 1;
        }
        Ok(freed)
    }

    /// [`unmap_each`](Self::unmap_each) with nobody to tell, for a range that was
    /// never reachable.
    fn unmap(&mut self, address: usize, length: usize, frames: &mut dyn FrameSource) {
        let _ = self.unmap_each(address, length, frames, &mut |_| {});
    }

    /// The frame and offset behind `address`, if user code could make this access.
    pub fn translate(&self, address: usize, write: bool) -> Result<usize, BadAddress> {
        if !self.in_window(address, 1) {
            return Err(BadAddress);
        }
        let (table, index) = self.leaf_slot(address).ok_or(BadAddress)?;
        // SAFETY: the slot is in a table of this space.
        let entry = unsafe { read_entry(table, index) };
        if entry & self.bits.present == 0 || (write && !self.bits.is_writable(entry)) {
            return Err(BadAddress);
        }
        Ok((entry & self.bits.address) as usize + address % PAGE_SIZE)
    }

    /// The kernel's address of the word at `address`, for a futex: aligned and mapped.
    pub fn translate_word(&self, address: usize) -> Result<usize, BadAddress> {
        if address % 4 != 0 {
            return Err(BadAddress);
        }
        self.translate(address, false)
    }

    /// Checks every page of a range a call is about to use, so a call that can't
    /// finish fails before it changes anything.
    pub fn validate(&self, address: usize, length: usize, write: bool) -> Result<(), BadAddress> {
        if length == 0 {
            return Ok(());
        }
        if !self.in_window(address, length) {
            return Err(BadAddress);
        }
        let first = address / PAGE_SIZE;
        let last = (address + length - 1) / PAGE_SIZE;
        for page in first..=last {
            self.translate(page * PAGE_SIZE, write)?;
        }
        Ok(())
    }

    /// Copies from user memory to `out`.
    pub fn copy_in(&self, address: usize, out: &mut [u8]) -> Result<(), BadAddress> {
        self.validate(address, out.len(), false)?;
        let mut done = 0;
        while done < out.len() {
            let here = address + done;
            let source = self.translate(here, false)?;
            let count = (PAGE_SIZE - here % PAGE_SIZE).min(out.len() - done);
            // SAFETY: `translate` found this frame mapped, and `count` stops at its end.
            unsafe { core::ptr::copy_nonoverlapping(source as *const u8, out[done..].as_mut_ptr(), count) };
            done += count;
        }
        Ok(())
    }

    /// Copies `data` to user memory, which must be writable by the program.
    pub fn copy_out(&self, address: usize, data: &[u8]) -> Result<(), BadAddress> {
        self.validate(address, data.len(), true)?;
        // SAFETY: nothing else writes these tables during a call.
        unsafe { self.fill(address, data) }
    }

    /// Copies `data` to user memory the program may not be able to write, as a
    /// loader does.
    ///
    /// # Safety
    /// For the kernel setting up a program, not for an address a program gave.
    pub unsafe fn load(&self, address: usize, data: &[u8]) -> Result<(), BadAddress> {
        self.validate(address, data.len(), false)?;
        // SAFETY: as the caller promises.
        unsafe { self.fill(address, data) }
    }

    /// # Safety
    /// The range was validated.
    unsafe fn fill(&self, address: usize, data: &[u8]) -> Result<(), BadAddress> {
        let mut done = 0;
        while done < data.len() {
            let here = address + done;
            let target = self.translate(here, false)?;
            let count = (PAGE_SIZE - here % PAGE_SIZE).min(data.len() - done);
            // SAFETY: `translate` found this frame mapped, and `count` stops at its end.
            unsafe { core::ptr::copy_nonoverlapping(data[done..].as_ptr(), target as *mut u8, count) };
            done += count;
        }
        Ok(())
    }

    /// Frees every page and table, the top table included.
    pub fn destroy(self, frames: &mut dyn FrameSource) {
        self.destroy_keeping(frames, &[]);
    }

    /// As [`destroy`](Self::destroy). Frames under `keep`, a list of `(first frame,
    /// bytes)`, are shared and left alone.
    pub fn destroy_keeping(self, frames: &mut dyn FrameSource, keep: &[(usize, usize)]) {
        let kept = |frame: usize| keep.iter().any(|&(first, bytes)| frame >= first && frame < first + bytes);
        for first in 0..ENTRIES {
            // SAFETY: the root is ours, and the indices are in range.
            let l1 = unsafe { read_entry(self.root, first) };
            if !self.user_slot(first) || l1 & self.bits.present == 0 {
                continue;
            }
            let l2_table = (l1 & self.bits.address) as usize;
            for second in 0..ENTRIES {
                // SAFETY: a present entry in our top table points at a table of ours.
                let l2 = unsafe { read_entry(l2_table, second) };
                if l2 & self.bits.present == 0 {
                    continue;
                }
                let l3_table = (l2 & self.bits.address) as usize;
                for third in 0..ENTRIES {
                    // SAFETY: as above.
                    let leaf = unsafe { read_entry(l3_table, third) };
                    if leaf & self.bits.present != 0 {
                        let frame = (leaf & self.bits.address) as usize;
                        if !kept(frame) {
                            frames.free(frame);
                        }
                    }
                }
                frames.free(l3_table);
            }
            frames.free(l2_table);
        }
        frames.free(self.root);
    }

    /// Whether a top-table entry belongs to this space's window. The rest of the top
    /// table may hold the kernel's mappings, which aren't this space's to free.
    fn user_slot(&self, index: usize) -> bool {
        let first = self.base >> L1_SHIFT;
        let last = (self.end - 1) >> L1_SHIFT;
        (first..=last).contains(&index)
    }
}

fn new_table(frames: &mut dyn FrameSource) -> Option<usize> {
    let table = frames.alloc()?;
    // SAFETY: the source just gave this frame, and it's a page long.
    unsafe { core::ptr::write_bytes(table as *mut u8, 0, PAGE_SIZE) };
    Some(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{alloc, dealloc, Layout};
    use std::collections::HashSet;

    /// Hands out page-aligned host memory and notices anything given back wrongly.
    struct Pool {
        live: HashSet<usize>,
        allocated: usize,
        limit: usize,
    }

    impl Pool {
        fn new() -> Self {
            Self::limited(usize::MAX)
        }
        fn limited(limit: usize) -> Self {
            Self { live: HashSet::new(), allocated: 0, limit }
        }
        fn layout() -> Layout {
            Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap()
        }
    }

    impl FrameSource for Pool {
        fn alloc(&mut self) -> Option<usize> {
            if self.allocated >= self.limit {
                return None;
            }
            // SAFETY: the layout has a size.
            let frame = unsafe { alloc(Self::layout()) } as usize;
            assert!(self.live.insert(frame));
            self.allocated += 1;
            // Junk, so that anything relying on a cleared frame fails.
            // SAFETY: just allocated.
            unsafe { core::ptr::write_bytes(frame as *mut u8, 0xa5, PAGE_SIZE) };
            Some(frame)
        }
        fn free(&mut self, frame: usize) {
            assert!(self.live.remove(&frame), "freed {frame:#x} twice or never had it");
            // SAFETY: it was allocated with this layout, and is gone from the set.
            unsafe { dealloc(frame as *mut u8, Self::layout()) };
        }
    }

    impl Drop for Pool {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                assert!(self.live.is_empty(), "{} frames leaked", self.live.len());
            }
        }
    }

    const BITS: PageBits = PageBits {
        present: 1,
        table: 0b11,
        leaf: 0b101,
        writable: 0b10,
        read_only: 0,
        no_execute: 1 << 63,
        address: 0x000f_ffff_ffff_f000,
    };
    /// The other style of encoding: a bit for read-only instead of one for writable.
    const ARM_BITS: PageBits = PageBits {
        present: 1,
        table: 0b11,
        leaf: 0b11 | 1 << 10 | 1 << 6,
        writable: 0,
        read_only: 1 << 7,
        no_execute: 1 << 54,
        // Wider than the real one, for a host's pointers.
        address: 0x0000_ffff_ffff_f000,
    };
    const BASE: usize = 0x10_0000_0000;
    const END: usize = 0x14_0000_0000;
    const RW: PageAccess = PageAccess { write: true, execute: false };
    const RO: PageAccess = PageAccess { write: false, execute: false };
    const RX: PageAccess = PageAccess { write: false, execute: true };

    fn space(frames: &mut Pool) -> AddressSpace {
        AddressSpace::new(frames, BITS, (BASE, END), 1 << 20).unwrap()
    }

    #[test]
    fn data_comes_back_across_a_page_boundary() {
        let mut frames = Pool::new();
        let vm = space(&mut frames);
        let mut vm = vm;
        vm.map_new(BASE, 2 * PAGE_SIZE, RW, &mut frames).unwrap();
        let data: Vec<u8> = (0..300).map(|n| n as u8).collect();
        vm.copy_out(BASE + PAGE_SIZE - 100, &data).unwrap();
        let mut back = vec![0u8; 300];
        vm.copy_in(BASE + PAGE_SIZE - 100, &mut back).unwrap();
        assert_eq!(data, back);
        vm.destroy(&mut frames);
    }

    #[test]
    fn new_pages_are_zero() {
        let mut frames = Pool::new();
        let mut vm = space(&mut frames);
        vm.map_new(BASE + PAGE_SIZE, PAGE_SIZE, RO, &mut frames).unwrap();
        let mut back = [1u8; 64];
        vm.copy_in(BASE + PAGE_SIZE, &mut back).unwrap();
        assert_eq!(back, [0u8; 64]);
        vm.destroy(&mut frames);
    }

    #[test]
    fn wild_addresses_and_lengths_are_refused() {
        let mut frames = Pool::new();
        let mut vm = space(&mut frames);
        vm.map_new(BASE, PAGE_SIZE, RW, &mut frames).unwrap();
        let mut small = [0u8; 8];
        for address in [
            0,
            1,
            0x4020_0000,           // the kernel, on AArch64
            0x0200_0000,           // the kernel, on x86_64
            0x0900_0000,           // a UART
            BASE - 1,
            END,
            END - 1,
            usize::MAX,
            usize::MAX - 7,
            1 << 63,
            BASE + PAGE_SIZE,      // unmapped, in the window
        ] {
            assert_eq!(vm.copy_in(address, &mut small), Err(BadAddress), "{address:#x}");
            assert_eq!(vm.copy_out(address, &small), Err(BadAddress), "{address:#x}");
            assert_eq!(vm.translate(address, false), Err(BadAddress), "{address:#x}");
        }
        // Starts good, and runs off the mapped page.
        assert_eq!(vm.copy_in(BASE + PAGE_SIZE - 4, &mut small), Err(BadAddress));
        vm.destroy(&mut frames);
    }

    #[test]
    fn lengths_that_wrap_are_refused_without_touching_anything() {
        let mut frames = Pool::new();
        let mut vm = space(&mut frames);
        vm.map_new(BASE, PAGE_SIZE, RW, &mut frames).unwrap();
        assert!(!vm.in_window(BASE, usize::MAX));
        assert!(!vm.in_window(usize::MAX, 2));
        assert!(!vm.in_window(BASE, END - BASE + 1));
        assert!(vm.in_window(BASE, END - BASE));
        assert!(vm.validate(BASE, usize::MAX, false).is_err());
        assert!(vm.validate(usize::MAX - 2, 8, true).is_err());
        assert!(vm.validate(BASE + 1, END - BASE, false).is_err());
        assert!(vm.validate(0, 0, false).is_ok());
        vm.destroy(&mut frames);
    }

    #[test]
    fn a_write_needs_a_writable_page() {
        for bits in [BITS, ARM_BITS] {
            let mut frames = Pool::new();
            let mut vm = AddressSpace::new(&mut frames, bits, (BASE, END), 1 << 20).unwrap();
            vm.map_new(BASE, PAGE_SIZE, RX, &mut frames).unwrap();
            vm.map_new(BASE + PAGE_SIZE, PAGE_SIZE, RW, &mut frames).unwrap();
            assert_eq!(vm.copy_out(BASE, &[1]), Err(BadAddress));
            // Good in one page, refused in the next, and nothing's written.
            assert_eq!(vm.copy_out(BASE + PAGE_SIZE - 1, &[9, 9]), Err(BadAddress));
            vm.copy_out(BASE + PAGE_SIZE, &[1]).unwrap();
            let mut back = [7u8; 1];
            vm.copy_in(BASE, &mut back).unwrap();
            assert_eq!(back, [0]);
            // The loader can still fill read-only pages.
            // SAFETY: a test's own space.
            unsafe { vm.load(BASE, &[42]).unwrap() };
            vm.copy_in(BASE, &mut back).unwrap();
            assert_eq!(back, [42]);
            vm.destroy(&mut frames);
        }
    }

    #[test]
    fn the_encoding_says_what_was_asked() {
        assert_ne!(BITS.leaf(RW) & BITS.writable, 0);
        assert_eq!(BITS.leaf(RO) & BITS.writable, 0);
        assert_ne!(BITS.leaf(RW) & BITS.no_execute, 0);
        assert_eq!(BITS.leaf(RX) & BITS.no_execute, 0);
        assert_ne!(ARM_BITS.leaf(RO) & ARM_BITS.read_only, 0);
        assert_eq!(ARM_BITS.leaf(RW) & ARM_BITS.read_only, 0);
        assert!(ARM_BITS.is_writable(ARM_BITS.leaf(RW)));
        assert!(!ARM_BITS.is_writable(ARM_BITS.leaf(RX)));
        assert!(BITS.is_writable(BITS.leaf(RW)));
        assert!(!BITS.is_writable(BITS.leaf(RX)));
    }

    #[test]
    fn mapping_twice_or_outside_the_window_fails() {
        let mut frames = Pool::new();
        let mut vm = space(&mut frames);
        vm.map_new(BASE, PAGE_SIZE, RW, &mut frames).unwrap();
        assert_eq!(vm.map_new(BASE, PAGE_SIZE, RW, &mut frames), Err(MapError::AlreadyMapped));
        assert_eq!(vm.map_new(BASE + 1, PAGE_SIZE, RW, &mut frames), Err(MapError::BadRange));
        assert_eq!(vm.map_new(BASE, 1, RW, &mut frames), Err(MapError::BadRange));
        assert_eq!(vm.map_new(BASE, 0, RW, &mut frames), Err(MapError::BadRange));
        assert_eq!(vm.map_new(BASE - PAGE_SIZE, PAGE_SIZE, RW, &mut frames), Err(MapError::BadRange));
        assert_eq!(vm.map_new(END, PAGE_SIZE, RW, &mut frames), Err(MapError::BadRange));
        assert_eq!(vm.map_new(END - PAGE_SIZE, 2 * PAGE_SIZE, RW, &mut frames), Err(MapError::BadRange));
        vm.destroy(&mut frames);
    }

    #[test]
    fn running_out_of_frames_midway_leaves_nothing_mapped() {
        let mut frames = Pool::limited(10);
        let mut vm = space(&mut frames);
        let before = vm.pages();
        assert_eq!(
            vm.map_new(BASE, 64 * PAGE_SIZE, RW, &mut frames),
            Err(MapError::OutOfMemory)
        );
        assert_eq!(vm.pages(), before);
        assert_eq!(vm.translate(BASE, false), Err(BadAddress));
        vm.destroy(&mut frames);
    }

    #[test]
    fn the_page_limit_is_kept() {
        let mut frames = Pool::new();
        let mut vm = AddressSpace::new(&mut frames, BITS, (BASE, END), 4).unwrap();
        vm.map_new(BASE, 4 * PAGE_SIZE, RW, &mut frames).unwrap();
        assert_eq!(vm.map_new(BASE + 8 * PAGE_SIZE, PAGE_SIZE, RW, &mut frames), Err(MapError::OutOfMemory));
        vm.destroy(&mut frames);
    }

    #[test]
    fn unmapping_frees_and_holes_are_fine() {
        let mut frames = Pool::new();
        let mut vm = space(&mut frames);
        vm.map_new(BASE, 4 * PAGE_SIZE, RW, &mut frames).unwrap();
        let mut seen = Vec::new();
        let freed = vm
            .unmap_each(BASE + PAGE_SIZE, 8 * PAGE_SIZE, &mut frames, &mut |page| seen.push(page))
            .unwrap();
        assert_eq!(freed, 3);
        assert_eq!(seen, [BASE + PAGE_SIZE, BASE + 2 * PAGE_SIZE, BASE + 3 * PAGE_SIZE]);
        assert_eq!(vm.pages(), 1);
        assert_eq!(vm.translate(BASE + PAGE_SIZE, false), Err(BadAddress));
        vm.translate(BASE, false).unwrap();
        vm.destroy(&mut frames);
    }

    #[test]
    fn destroying_frees_everything_across_the_tree() {
        let mut frames = Pool::new();
        let mut vm = space(&mut frames);
        // Pages that need their own tables at every level.
        for address in [BASE, BASE + (2 << 20), BASE + (1 << 30), END - PAGE_SIZE] {
            vm.map_new(address, PAGE_SIZE, RW, &mut frames).unwrap();
        }
        vm.destroy(&mut frames);
        assert!(frames.live.is_empty());
    }

    #[test]
    fn the_kernels_part_of_a_top_table_is_left_alone() {
        let mut frames = Pool::new();
        let mut vm = space(&mut frames);
        vm.map_new(BASE, PAGE_SIZE, RW, &mut frames).unwrap();
        // What a port does: the kernel's mappings in the same top table, at indices
        // outside the window. Those aren't frames from this pool: freeing one fails.
        // SAFETY: the root is a page, and index 1 is inside it.
        unsafe { write_entry(vm.root(), 1, 0x4000_0000 | BITS.table) };
        vm.destroy(&mut frames);
    }

    #[test]
    fn shared_frames_are_not_freed() {
        let mut frames = Pool::new();
        let shared = frames.alloc().unwrap();
        let mut vm = space(&mut frames);
        vm.map_shared(BASE, shared, PAGE_SIZE, RW, &mut frames).unwrap();
        vm.copy_out(BASE, &[5]).unwrap();
        // SAFETY: the pool's frame, still live.
        assert_eq!(unsafe { *(shared as *const u8) }, 5);
        assert_eq!(vm.pages(), 0);
        vm.unmap_shared(BASE, PAGE_SIZE);
        assert_eq!(vm.translate(BASE, false), Err(BadAddress));
        vm.destroy(&mut frames);
        frames.free(shared);

        // And the same if it's still mapped at the end.
        let shared = frames.alloc().unwrap();
        let mut vm = space(&mut frames);
        vm.map_shared(BASE, shared, PAGE_SIZE, RW, &mut frames).unwrap();
        vm.destroy_keeping(&mut frames, &[(shared, PAGE_SIZE)]);
        frames.free(shared);
    }

    #[test]
    fn futex_words_must_be_aligned_and_mapped() {
        let mut frames = Pool::new();
        let mut vm = space(&mut frames);
        vm.map_new(BASE, PAGE_SIZE, RW, &mut frames).unwrap();
        assert!(vm.translate_word(BASE + 4).is_ok());
        assert_eq!(vm.translate_word(BASE + 2), Err(BadAddress));
        assert_eq!(vm.translate_word(BASE + PAGE_SIZE), Err(BadAddress));
        assert_eq!(vm.translate_word(0x4020_0000), Err(BadAddress));
        vm.destroy(&mut frames);
    }

    #[test]
    fn repeated_create_and_destroy_leaks_nothing() {
        let mut frames = Pool::new();
        for round in 0..200 {
            let mut vm = space(&mut frames);
            vm.map_new(BASE + round * PAGE_SIZE, 8 * PAGE_SIZE, RW, &mut frames).unwrap();
            vm.map_new(END - 4 * PAGE_SIZE, 4 * PAGE_SIZE, RW, &mut frames).unwrap();
            vm.unmap_each(BASE + round * PAGE_SIZE, PAGE_SIZE, &mut frames, &mut |_| {}).unwrap();
            vm.destroy(&mut frames);
            assert!(frames.live.is_empty());
        }
    }

    #[test]
    fn a_bad_window_is_refused() {
        let mut frames = Pool::new();
        assert!(AddressSpace::new(&mut frames, BITS, (1, END), 8).is_none());
        assert!(AddressSpace::new(&mut frames, BITS, (BASE, BASE), 8).is_none());
        assert!(AddressSpace::new(&mut frames, BITS, (0, 1 << 40), 8).is_none());
    }
}
