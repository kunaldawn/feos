//! The VMA list: what a process has mapped, range by range.
//!
//! The page tables say which pages have frames; this says what may be there at all. A
//! reservation, a `PROT_NONE` range or a page never touched has no page-table entry, so
//! the fault path and every call that changes mappings starts here. The list is sorted
//! and never overlaps; neighbours with the same attributes are joined, so a program that
//! maps and unmaps in the usual way keeps it short. It's capped at
//! [`MAX_VMAS`](feos_abi::MAX_VMAS), so its heap is bounded by what a program can ask for.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::shared::SharedAnon;
use crate::PAGE_SIZE;

/// What a range may be used for. Write without read is read and write: no port can
/// map a page writable and not readable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Prot {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Prot {
    pub const NONE: Self = Self { read: false, write: false, exec: false };
    pub const READ: Self = Self { read: true, write: false, exec: false };
    pub const READ_WRITE: Self = Self { read: true, write: true, exec: false };
    pub const READ_EXEC: Self = Self { read: true, write: false, exec: true };

    /// No access at all.
    pub const fn is_none(self) -> bool {
        !self.read && !self.write && !self.exec
    }
}

/// What's behind a range's pages.
#[derive(Clone, Debug)]
pub enum Backing {
    /// Frames of this process's own, the zero frame until a page is written.
    Private,
    /// A shared-anonymous object, from its page `first` on.
    Shared { object: Arc<SharedAnon>, first: usize },
    /// Frames the kernel owns and lent, such as the window surface: mapped when the
    /// range was, never changed by a program and never freed here.
    Lent,
}

/// One mapped range.
#[derive(Clone, Debug)]
pub struct Vma {
    pub start: usize,
    pub end: usize,
    pub prot: Prot,
    pub backing: Backing,
    /// Promised: every page that has no frame of its own yet holds a promise.
    pub charged: bool,
    /// Made with `MAP_NORESERVE`: never charged, whatever its protection becomes.
    pub noreserve: bool,
    /// The program's image, loaded by the kernel: its contents can't be made again, so
    /// `MADV_DONTNEED` leaves them alone.
    pub image: bool,
    /// Has been writable. Only an image range that never was may be executable.
    pub written: bool,
}

impl Vma {
    pub const fn pages(&self) -> usize {
        (self.end - self.start) / PAGE_SIZE
    }

    /// Whether `next` can be joined onto the end of this.
    fn joins(&self, next: &Vma) -> bool {
        if self.end != next.start
            || self.prot != next.prot
            || self.charged != next.charged
            || self.noreserve != next.noreserve
            || self.image != next.image
            || self.written != next.written
        {
            return false;
        }
        match (&self.backing, &next.backing) {
            (Backing::Private, Backing::Private) => true,
            (Backing::Shared { object, first }, Backing::Shared { object: other, first: next_first }) => {
                Arc::ptr_eq(object, other) && first + self.pages() == *next_first
            }
            _ => false,
        }
    }

    /// Cuts this at `at`, inside it, keeping the low part and returning the high one.
    fn split_off(&mut self, at: usize) -> Vma {
        let mut high = self.clone();
        high.start = at;
        if let Backing::Shared { first, .. } = &mut high.backing {
            *first += (at - self.start) / PAGE_SIZE;
        }
        self.end = at;
        high
    }
}

/// Why the list couldn't change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmaError {
    /// It would hold more than its cap, or there's no heap for it.
    Full,
}

/// A process's VMAs.
#[derive(Debug)]
pub struct VmaList {
    list: Vec<Vma>,
    cap: usize,
    /// Every gap between the VMAs before this index is too small for a mapping, so a
    /// first-fit search can start here. Searches move it up; any change below it
    /// brings it down.
    skip: usize,
}

impl VmaList {
    pub const fn new(cap: usize) -> Self {
        Self { list: Vec::new(), cap, skip: 0 }
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn iter(&self) -> core::slice::Iter<'_, Vma> {
        self.list.iter()
    }

    pub fn get(&self, index: usize) -> Option<&Vma> {
        self.list.get(index)
    }

    pub(crate) fn get_mut(&mut self, index: usize) -> Option<&mut Vma> {
        self.list.get_mut(index)
    }

    /// The VMA holding `address`.
    pub fn find(&self, address: usize) -> Option<usize> {
        let index = self.list.partition_point(|vma| vma.end <= address);
        (index < self.list.len() && self.list[index].start <= address).then_some(index)
    }

    /// The indices of the VMAs overlapping `[start, end)`.
    pub fn overlapping(&self, start: usize, end: usize) -> core::ops::Range<usize> {
        let first = self.list.partition_point(|vma| vma.end <= start);
        let last = first + self.list[first..].partition_point(|vma| vma.start < end);
        first..last
    }

    /// Whether nothing is mapped in `[start, end)`.
    pub fn is_free(&self, start: usize, end: usize) -> bool {
        self.overlapping(start, end).is_empty()
    }

    /// Whether every page of `[start, end)` is mapped.
    pub fn covers(&self, start: usize, end: usize) -> bool {
        let mut at = start;
        for vma in &self.list[self.overlapping(start, end)] {
            if vma.start > at {
                return false;
            }
            at = vma.end;
        }
        at >= end
    }

    /// How many more VMAs cutting `[start, end)` out would leave: one for each end that
    /// falls inside a VMA, less the VMAs inside.
    fn pieces_left(&self, start: usize, end: usize) -> (usize, usize) {
        let range = self.overlapping(start, end);
        if range.is_empty() {
            return (0, 0);
        }
        let first = &self.list[range.start];
        let last = &self.list[range.end - 1];
        let kept = usize::from(first.start < start) + usize::from(last.end > end);
        (kept, range.len())
    }

    /// Whether `[start, end)` can be split from its neighbours, cut out too if `cut`,
    /// and `added` VMAs put in, within the cap; and room is made for it now, so the
    /// change can't fail half way.
    pub fn reserve_for(&mut self, start: usize, end: usize, added: usize, cut: bool) -> Result<(), VmaError> {
        let (kept, inside) = self.pieces_left(start, end);
        let most = self.list.len() + kept + added;
        let after = if cut { most - inside } else { most };
        if after > self.cap {
            return Err(VmaError::Full);
        }
        // The splits come before the cut, so the list holds `most` for a moment.
        self.list
            .try_reserve(most.saturating_sub(self.list.len()))
            .map_err(|_| VmaError::Full)
    }

    /// Splits whatever VMA holds `at` so that a VMA starts there. Room must have been
    /// made with [`reserve_for`](Self::reserve_for).
    pub(crate) fn split(&mut self, at: usize) {
        if let Some(index) = self.find(at) {
            if self.list[index].start != at {
                let high = self.list[index].split_off(at);
                self.list.insert(index + 1, high);
                self.skip = self.skip.min(index);
            }
        }
    }

    /// Takes the VMAs in `range` out, for the caller to finish off.
    pub(crate) fn drain(&mut self, range: core::ops::Range<usize>) -> alloc::vec::Drain<'_, Vma> {
        self.skip = self.skip.min(range.start);
        self.list.drain(range)
    }

    /// Puts a VMA into a gap, joining it to its neighbours where it can. Room must have
    /// been made with [`reserve_for`](Self::reserve_for).
    pub(crate) fn insert(&mut self, vma: Vma) {
        let index = self.list.partition_point(|other| other.end <= vma.start);
        debug_assert!(index == self.list.len() || self.list[index].start >= vma.end);
        self.list.insert(index, vma);
        self.skip = self.skip.min(index);
        self.join_around(index, index + 1);
    }

    /// Joins neighbours in `[first - 1, last]` that have come to match.
    pub(crate) fn join_around(&mut self, first: usize, last: usize) {
        let mut index = first.saturating_sub(1);
        let mut last = last.min(self.list.len());
        while index + 1 < self.list.len() && index < last {
            if self.list[index].joins(&self.list[index + 1]) {
                let next = self.list.remove(index + 1);
                self.list[index].end = next.end;
                self.skip = self.skip.min(index);
                last = last.saturating_sub(1);
            } else {
                index += 1;
            }
        }
    }

    /// The lowest address in `[low, high)` with room for `length` bytes and an unmapped
    /// guard page on either side, as v1's bump allocator left one after each mapping.
    /// `low` is the same for every search: it's the start of the mmap part of the window.
    pub fn gap(&mut self, length: usize, low: usize, high: usize) -> Option<usize> {
        let mut index = self.list.partition_point(|vma| vma.end.saturating_add(PAGE_SIZE) <= low);
        let mut at = low;
        if self.skip > index {
            index = self.skip;
            at = at.max(self.list[index - 1].end.checked_add(PAGE_SIZE)?);
        }
        let mut dense = true;
        while index < self.list.len() {
            let vma = &self.list[index];
            if vma.start >= high {
                break;
            }
            let fits_before = at.checked_add(length).and_then(|end| end.checked_add(PAGE_SIZE)).is_some_and(|end| end <= vma.start);
            if fits_before {
                return (at + length <= high).then_some(at);
            }
            // A page with a guard either side would have fitted: not dense here.
            if at.saturating_add(2 * PAGE_SIZE) <= vma.start {
                dense = false;
            }
            at = at.max(vma.end.checked_add(PAGE_SIZE)?);
            index += 1;
            if dense {
                self.skip = index;
            }
        }
        at.checked_add(length).filter(|&end| end <= high).map(|_| at)
    }
}
