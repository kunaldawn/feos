//! Shared-anonymous objects: the memory behind `MAP_SHARED | MAP_ANONYMOUS`.
//!
//! A page of a shared mapping has to be the same frame in every mapping of it, whichever
//! touches it first, so the frames belong to an object rather than to a page table.
//! Every VMA mapping part of the object holds a reference to it, a split of one included,
//! and `FORK` will share the object rather than the frames that happen to exist. The
//! object holds one map count on each of its frames; each page-table entry pointing at
//! one holds another. It's filled under its own lock.

use alloc::vec::Vec;

use feos_sync::SpinLock;

use crate::FrameSource;

/// Pages per leaf of the object's table: a leaf is a page of frame addresses.
const LEAF: usize = 512;

#[derive(Debug)]
struct Inner {
    /// Frame addresses, a leaf per 512 pages; an empty leaf has none yet, and zero is a
    /// page with no frame.
    leaves: Vec<Vec<usize>>,
    /// Frames the object holds.
    frames: usize,
    /// Whether the object's pages are promised: all of them, from when it was made or
    /// first made writable.
    charged: bool,
}

/// A shared-anonymous object of a fixed number of pages.
#[derive(Debug)]
pub struct SharedAnon {
    pages: usize,
    inner: SpinLock<Inner>,
}

/// Why a page couldn't be given a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObjectError {
    /// No heap for the object's table.
    NoTable,
    /// No frame for the page.
    NoFrame,
    /// Past the object's end.
    OutOfRange,
}

impl SharedAnon {
    /// An object of `pages` pages with no frames yet. `None` if its table can't have
    /// room.
    pub(crate) fn new(pages: usize, charged: bool) -> Option<Self> {
        let mut leaves = Vec::new();
        leaves.try_reserve_exact(pages.div_ceil(LEAF)).ok()?;
        leaves.resize_with(pages.div_ceil(LEAF), Vec::new);
        Some(Self {
            pages,
            inner: SpinLock::new(Inner { leaves, frames: 0, charged }),
        })
    }

    /// Pages in the object.
    pub fn pages(&self) -> usize {
        self.pages
    }

    /// Frames the object holds.
    pub fn frames(&self) -> usize {
        self.inner.lock().frames
    }

    /// Whether the object's pages are promised.
    pub fn charged(&self) -> bool {
        self.inner.lock().charged
    }

    pub(crate) fn set_charged(&self) {
        self.inner.lock().charged = true;
    }

    /// The frame of page `index`, if it has one.
    pub fn frame(&self, index: usize) -> Option<usize> {
        let inner = self.inner.lock();
        let leaf = inner.leaves.get(index / LEAF)?;
        leaf.get(index % LEAF).copied().filter(|&frame| frame != 0)
    }

    /// The frame of page `index`, given one by `make` if it has none: `Ok((frame,
    /// made))`. `make` runs under the object's lock, so two first touches can't make
    /// two frames. A frame `make` returns belongs to the object, with its one count.
    pub(crate) fn frame_or_make(
        &self,
        index: usize,
        make: &mut dyn FnMut() -> Option<usize>,
    ) -> Result<(usize, bool), ObjectError> {
        let mut inner = self.inner.lock();
        let Some(leaf) = inner.leaves.get_mut(index / LEAF) else {
            return Err(ObjectError::OutOfRange);
        };
        if leaf.is_empty() {
            leaf.try_reserve_exact(LEAF).map_err(|_| ObjectError::NoTable)?;
            leaf.resize(LEAF, 0);
        }
        let slot = &mut leaf[index % LEAF];
        if *slot != 0 {
            return Ok((*slot, false));
        }
        let frame = make().ok_or(ObjectError::NoFrame)?;
        *slot = frame;
        inner.frames += 1;
        Ok((frame, true))
    }

    /// Gives every frame back, for the object's last mapping going. Returns how many
    /// there were.
    pub(crate) fn release_all(&self, frames: &mut dyn FrameSource) -> usize {
        let mut inner = self.inner.lock();
        let mut count = 0;
        for leaf in inner.leaves.iter_mut() {
            for slot in leaf.iter_mut() {
                if *slot != 0 {
                    frames.put(*slot);
                    *slot = 0;
                    count += 1;
                }
            }
            *leaf = Vec::new();
        }
        inner.frames = 0;
        count
    }
}
