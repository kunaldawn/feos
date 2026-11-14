//! A process's address space: its VMAs, its user page tables and the frames they point at.
//!
//! Both ports walk the same tree for user memory: three levels of 512 entries, for
//! 1 GiB, 2 MiB and 4 KiB. On AArch64 that's the top of a 39-bit translation; on
//! x86_64 it's a page directory pointer table under the process's PML4. What differs
//! is how an entry is written, which the port says in a [`PageBits`].
//!
//! [`Mm`] is the address space: the [VMA list](vma) says what's mapped and how, the
//! page tables ([`AddressSpace`]) only which pages have frames. A reservation, a
//! `PROT_NONE` range or a page never touched has no entry, and its first touch is a
//! fault that [`Mm::fault`] answers from the VMA: a private page that's only read gets
//! the machine's zero frame, read-only; one that's written gets a frame of its own; a
//! shared page gets its object's frame. Frames are counted (see `feos-frames`): a page
//! table entry holds a reference, and so does a copy in flight, which pins it.
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
//! # Charging
//!
//! Pages that could need a frame of the process's own that can't be taken back are
//! promised when they're mapped ([`Promises`]): writable private pages and writable
//! shared ones, unless `MAP_NORESERVE` says not to. Everything else is free until it's
//! touched, so a program can reserve far more than the machine has. A charged range's
//! page tables are made with it, so its faults never need a frame nobody promised;
//! an uncharged range's are made when a fault needs them. Empty leaf tables go when
//! the range they were for is unmapped.
//!
//! The memory this manages is what [`FrameSource`] hands out. A frame's address must be
//! usable as a pointer, as it is on an identity map and in a host test.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub use feos_hal::{PageAccess, PageBits};

mod mm;
mod promise;
mod shared;
pub mod vma;

pub use mm::{Access, FaultError, MapRequest, Mm, MmError, Pinned, Place};
pub use promise::Promises;
pub use shared::SharedAnon;
pub use vma::{Backing, Prot, Vma};

/// Bytes in a page.
pub const PAGE_SIZE: usize = 4096;
const ENTRIES: usize = 512;
const L1_SHIFT: usize = 30;
const L2_SHIFT: usize = 21;
const L3_SHIFT: usize = 12;
/// Bytes one leaf table maps.
const LEAF_SPAN: usize = 1 << L2_SHIFT;
/// Bytes one middle table maps.
const MIDDLE_SPAN: usize = 1 << L1_SHIFT;

/// Where page tables and pages come from, and go back to.
pub trait FrameSource {
    /// A free frame, with one reference: the caller's. It needn't be cleared. A page
    /// table's frame goes back with [`free`](Self::free), a page's with
    /// [`put`](Self::put).
    fn alloc(&mut self) -> Option<usize>;
    /// Gives back a frame nobody else references.
    fn free(&mut self, frame: usize);
    /// Another reference to a frame that has one. `false` if its count is at the cap.
    fn get(&mut self, frame: usize) -> bool;
    /// Drops a reference. The frame goes when none is left, and nothing pins it.
    fn put(&mut self, frame: usize);
    /// Pins a referenced frame for a copy. `false` if its pin count is at the cap.
    fn pin(&mut self, frame: usize) -> bool;
    /// Drops a pin.
    fn unpin(&mut self, frame: usize);
    /// Frames free right now, for [`Promises`].
    fn free_count(&self) -> usize;
}

/// A soft entry: one the hardware sees as invalid but that holds a frame, in the address
/// bits, for a page made inaccessible that keeps its contents. Neither port's address
/// bits nor its valid bit are up here.
const SOFT: u64 = 1 << 58;
const SOFT_FRAME: u64 = 1 << 62;
const SOFT_BITS: u64 = SOFT | SOFT_FRAME;

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

/// One process's page tables over `[base, end)`: which pages have frames, and what may
/// be done with them. What should be there is [`Mm`]'s business.
#[derive(Debug)]
pub struct AddressSpace {
    /// The top table these mappings are in. Its other entries aren't this code's.
    root: usize,
    base: usize,
    end: usize,
    bits: PageBits,
    /// Tables below the root.
    tables: usize,
}

/// What's in a leaf entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Empty,
    /// Valid to the hardware.
    Present { frame: usize, writable: bool, executable: bool },
    /// Holds a frame no access is allowed to.
    Hidden { frame: usize },
}

impl AddressSpace {
    /// An empty space in a fresh table. `window` must be page aligned and fit the tree.
    pub fn new(frames: &mut dyn FrameSource, bits: PageBits, window: (usize, usize)) -> Option<Self> {
        let (base, end) = window;
        if base % PAGE_SIZE != 0 || end % PAGE_SIZE != 0 || base >= end || end > (ENTRIES << L1_SHIFT) {
            return None;
        }
        // Soft entries have to be told from everything a port writes.
        if (bits.present | bits.address) & SOFT_BITS != 0 {
            return None;
        }
        let root = new_table(frames)?;
        Some(Self { root, base, end, bits, tables: 0 })
    }

    /// The top table's address.
    pub const fn root(&self) -> usize {
        self.root
    }

    /// Page tables below the root.
    pub const fn tables(&self) -> usize {
        self.tables
    }

    /// Whether `[address, address + length)` is inside the window and doesn't wrap.
    pub fn in_window(&self, address: usize, length: usize) -> bool {
        address >= self.base && length <= self.end - self.base && address <= self.end - length
    }

    /// The window.
    pub const fn window(&self) -> (usize, usize) {
        (self.base, self.end)
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

    /// How many tables a page at `address` would need made: none, a leaf, or a leaf and
    /// the middle table above it.
    pub(crate) fn missing_tables(&self, address: usize) -> usize {
        // SAFETY: the root is ours, and the index is masked to fit.
        let top = unsafe { read_entry(self.root, (address >> L1_SHIFT) & (ENTRIES - 1)) };
        if top & self.bits.present == 0 {
            return 2;
        }
        usize::from(self.leaf_slot(address).is_none())
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
                self.tables += 1;
                // SAFETY: as above.
                unsafe { write_entry(table, index, next as u64 | self.bits.table) };
                table = next;
            }
        }
        Some((table, (address >> L3_SHIFT) & (ENTRIES - 1)))
    }

    /// Makes every table `[start, end)` could need. On failure what was made stays,
    /// empty, for [`sweep`](Self::sweep) to take back.
    pub(crate) fn make_tables(&mut self, start: usize, end: usize, frames: &mut dyn FrameSource) -> bool {
        let mut at = start & !(LEAF_SPAN - 1);
        while at < end {
            if self.leaf_slot_or_new(at, frames).is_none() {
                return false;
            }
            at += LEAF_SPAN;
        }
        true
    }

    /// Tables `[start, end)` would need made.
    pub(crate) fn tables_wanted(&self, start: usize, end: usize) -> usize {
        let mut wanted = 0;
        let mut at = start & !(LEAF_SPAN - 1);
        let mut middle_counted = usize::MAX;
        while at < end {
            match self.missing_tables(at) {
                2 => {
                    // One middle per gigabyte, however many leaves under it.
                    let middle = at >> L1_SHIFT;
                    wanted += if middle_counted == middle { 1 } else { 2 };
                    middle_counted = middle;
                }
                missing => wanted += missing,
            }
            at += LEAF_SPAN;
        }
        wanted
    }

    pub(crate) fn entry(&self, address: usize) -> Entry {
        match self.leaf_slot(address) {
            // SAFETY: the slot is in a table of this space.
            Some((table, index)) => self.decode(unsafe { read_entry(table, index) }),
            None => Entry::Empty,
        }
    }

    fn decode(&self, entry: u64) -> Entry {
        let frame = (entry & self.bits.address) as usize;
        if entry & self.bits.present != 0 {
            Entry::Present {
                frame,
                writable: self.bits.is_writable(entry),
                executable: entry & self.bits.no_execute == 0,
            }
        } else if entry & SOFT_FRAME != 0 {
            Entry::Hidden { frame }
        } else {
            Entry::Empty
        }
    }

    fn encode(&self, entry: Entry) -> u64 {
        match entry {
            Entry::Empty => 0,
            Entry::Present { frame, writable, executable } => {
                frame as u64 | self.bits.leaf(PageAccess { write: writable, execute: executable })
            }
            Entry::Hidden { frame } => frame as u64 | SOFT | SOFT_FRAME,
        }
    }

    /// Writes the entry for `address`, whose tables must be there. `false` if they
    /// aren't.
    pub(crate) fn set(&mut self, address: usize, entry: Entry) -> bool {
        let value = self.encode(entry);
        match self.leaf_slot(address) {
            Some((table, index)) => {
                // SAFETY: the slot is in a table of this space.
                unsafe { write_entry(table, index, value) };
                true
            }
            None => matches!(entry, Entry::Empty),
        }
    }

    /// As [`set`](Self::set), making the tables if they aren't there.
    pub(crate) fn set_making(&mut self, address: usize, entry: Entry, frames: &mut dyn FrameSource) -> bool {
        let value = self.encode(entry);
        match self.leaf_slot_or_new(address, frames) {
            Some((table, index)) => {
                // SAFETY: the slot is in a table of this space.
                unsafe { write_entry(table, index, value) };
                true
            }
            None => false,
        }
    }

    /// Calls `each` with every page in `[start, end)` that has an entry, skipping whole
    /// tables that aren't there, and writes back what it returns. Then, for each entry
    /// that changed, calls `after` with the page and the old and new entries: a flush
    /// and a frame given back go there, once the entry pointing at it is gone.
    pub(crate) fn update(
        &mut self,
        start: usize,
        end: usize,
        each: &mut dyn FnMut(usize, Entry) -> Entry,
        after: &mut dyn FnMut(usize, Entry, Entry),
    ) {
        let mut at = start;
        while at < end {
            let leaf_end = ((at & !(LEAF_SPAN - 1)) + LEAF_SPAN).min(end);
            let Some((table, _)) = self.leaf_slot(at) else {
                at = leaf_end;
                continue;
            };
            while at < leaf_end {
                let index = (at >> L3_SHIFT) & (ENTRIES - 1);
                // SAFETY: the slot is in a table of this space.
                let raw = unsafe { read_entry(table, index) };
                if raw != 0 {
                    let old = self.decode(raw);
                    let new = each(at, old);
                    if new != old {
                        let value = self.encode(new);
                        // SAFETY: as above.
                        unsafe { write_entry(table, index, value) };
                        after(at, old, new);
                    }
                }
                at += PAGE_SIZE;
            }
        }
    }

    /// Calls `each` with every page in `[start, end)` that has an entry.
    pub(crate) fn visit(&self, start: usize, end: usize, each: &mut dyn FnMut(usize, Entry)) {
        let mut at = start;
        while at < end {
            let leaf_end = ((at & !(LEAF_SPAN - 1)) + LEAF_SPAN).min(end);
            let Some((table, _)) = self.leaf_slot(at) else {
                at = leaf_end;
                continue;
            };
            while at < leaf_end {
                // SAFETY: the slot is in a table of this space.
                let raw = unsafe { read_entry(table, (at >> L3_SHIFT) & (ENTRIES - 1)) };
                if raw != 0 {
                    each(at, self.decode(raw));
                }
                at += PAGE_SIZE;
            }
        }
    }

    /// Frees the tables under `[start, end)` that are empty, unless `keep` says the span
    /// one maps (its start and length) is still wanted. `flush` is told the start of each
    /// span whose table goes, after its entry is cleared and before its frame is freed,
    /// so no CPU can walk a table that's been given back.
    pub(crate) fn sweep(
        &mut self,
        start: usize,
        end: usize,
        keep: &dyn Fn(usize, usize) -> bool,
        flush: &mut dyn FnMut(usize),
        frames: &mut dyn FrameSource,
    ) {
        if start >= end {
            return;
        }
        let mut middle_at = start & !(MIDDLE_SPAN - 1);
        while middle_at < end {
            let top_index = (middle_at >> L1_SHIFT) & (ENTRIES - 1);
            // SAFETY: the root is ours, and the index is masked to fit.
            let top = unsafe { read_entry(self.root, top_index) };
            if top & self.bits.present != 0 && self.user_slot(top_index) {
                let middle = (top & self.bits.address) as usize;
                let low = start.max(middle_at) & !(LEAF_SPAN - 1);
                let high = end.min(middle_at + MIDDLE_SPAN);
                let mut leaf_at = low;
                while leaf_at < high {
                    let index = (leaf_at >> L2_SHIFT) & (ENTRIES - 1);
                    // SAFETY: a present entry in the root points at a table of ours.
                    let entry = unsafe { read_entry(middle, index) };
                    if entry & self.bits.present != 0 && !keep(leaf_at, LEAF_SPAN) {
                        let leaf = (entry & self.bits.address) as usize;
                        // SAFETY: as above, a leaf table of ours.
                        if (0..ENTRIES).all(|slot| unsafe { read_entry(leaf, slot) } == 0) {
                            // SAFETY: as above.
                            unsafe { write_entry(middle, index, 0) };
                            flush(leaf_at);
                            frames.free(leaf);
                            self.tables -= 1;
                        }
                    }
                    leaf_at += LEAF_SPAN;
                }
                // SAFETY: as above.
                let empty = (0..ENTRIES).all(|slot| unsafe { read_entry(middle, slot) } == 0);
                if empty && !keep(middle_at, MIDDLE_SPAN) {
                    // SAFETY: the root is ours.
                    unsafe { write_entry(self.root, top_index, 0) };
                    flush(middle_at);
                    frames.free(middle);
                    self.tables -= 1;
                }
            }
            middle_at += MIDDLE_SPAN;
        }
    }

    /// The frame and offset behind `address`, if user code could make this access.
    pub fn translate(&self, address: usize, write: bool) -> Result<usize, BadAddress> {
        if !self.in_window(address, 1) {
            return Err(BadAddress);
        }
        match self.entry(address) {
            Entry::Present { frame, writable, .. } if writable || !write => Ok(frame + address % PAGE_SIZE),
            _ => Err(BadAddress),
        }
    }

    /// The kernel's address of the word at `address`, for a futex: aligned and mapped.
    pub fn translate_word(&self, address: usize) -> Result<usize, BadAddress> {
        if !address.is_multiple_of(4) {
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
    /// For the kernel setting up a program, not for an address a program gave, and never
    /// into a frame anything else shares (the zero frame, a lent one).
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

    /// Frees every table, the top table included. The pages are the caller's to have
    /// given back first.
    pub(crate) fn destroy(self, frames: &mut dyn FrameSource) {
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
                if l2 & self.bits.present != 0 {
                    frames.free((l2 & self.bits.address) as usize);
                }
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
mod tests;
