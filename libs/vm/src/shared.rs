//! Shared-anonymous objects: the memory behind `MAP_SHARED | MAP_ANONYMOUS`.
//!
//! A page of a shared mapping has to be the same frame in every mapping of it, whichever
//! touches it first, so the frames belong to an object rather than to a page table.
//! Every VMA mapping part of the object holds a reference to it, a split of one included,
//! and `FORK` will share the object rather than the frames that happen to exist. The
//! object holds one map count on each of its frames; each page-table entry pointing at
//! one holds another. It's filled under its own lock.
//!
//! The kernel heap an object holds is bounded two ways. Its header and directory are
//! sized by the object, touched or not, so they're counted against the
//! [budget](feos_abi::OBJECT_HEAP_LIMIT) of the space that made it, from when it's made
//! until it goes. Its leaves are made by first touches, a small one per [`LEAF`] pages,
//! so they cost at most a leaf per frame the object holds: bounded by the frames the
//! machine has, whoever holds them, at a 16th of a frame each.

use alloc::alloc::{alloc, alloc_zeroed, dealloc, Layout};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr::NonNull;
use core::sync::atomic::{fence, AtomicUsize, Ordering};

use feos_sync::SpinLock;

use crate::FrameSource;

/// Pages per leaf of the object's table. Small, so that one frame touched alone costs
/// little heap: 256 bytes.
const LEAF: usize = 32;

type Leaf = [usize; LEAF];

#[derive(Debug)]
struct Inner {
    /// Frame addresses, a leaf per [`LEAF`] pages; a missing leaf has none yet, and zero
    /// is a page with no frame.
    leaves: Vec<Option<Box<Leaf>>>,
    /// Frames the object holds.
    frames: usize,
    /// Whether the object's pages are promised: all of them, from when it was made or
    /// first made writable.
    charged: bool,
}

/// A shared-anonymous object of a fixed number of pages.
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

impl core::fmt::Debug for SharedAnon {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(out, "SharedAnon({} pages)", self.pages)
    }
}

/// A leaf of zeroes, or `None` if the heap has no room for one.
fn new_leaf() -> Option<Box<Leaf>> {
    let layout = Layout::new::<Leaf>();
    // SAFETY: the layout isn't zero sized.
    let raw = unsafe { alloc_zeroed(layout) }.cast::<Leaf>();
    // SAFETY: allocated by the global allocator with `Leaf`'s layout, and zeroes are a
    // valid `Leaf`.
    (!raw.is_null()).then(|| unsafe { Box::from_raw(raw) })
}

impl SharedAnon {
    /// Kernel heap an object of `pages` pages holds whatever is touched: what a space's
    /// [budget](feos_abi::OBJECT_HEAP_LIMIT) is charged for it.
    pub fn heap_cost(pages: usize) -> usize {
        core::mem::size_of::<Counted>()
            .saturating_add(pages.div_ceil(LEAF).saturating_mul(core::mem::size_of::<Option<Box<Leaf>>>()))
    }

    /// An object of `pages` pages with no frames yet. `None` if its table can't have
    /// room.
    pub(crate) fn new(pages: usize, charged: bool) -> Option<Self> {
        let mut leaves = Vec::new();
        leaves.try_reserve_exact(pages.div_ceil(LEAF)).ok()?;
        leaves.resize_with(pages.div_ceil(LEAF), || None);
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
        let leaf = inner.leaves.get(index / LEAF)?.as_ref()?;
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
        if index >= self.pages {
            return Err(ObjectError::OutOfRange);
        }
        let mut inner = self.inner.lock();
        let Some(slot) = inner.leaves.get_mut(index / LEAF) else {
            return Err(ObjectError::OutOfRange);
        };
        // A leaf made here stays, even if `make` fails below: an empty leaf is a few
        // hundred bytes, which the next touch of it uses, and it goes with the object.
        let leaf = match slot {
            Some(leaf) => leaf,
            None => slot.insert(new_leaf().ok_or(ObjectError::NoTable)?),
        };
        let entry = &mut leaf[index % LEAF];
        if *entry != 0 {
            return Ok((*entry, false));
        }
        let frame = make().ok_or(ObjectError::NoFrame)?;
        *entry = frame;
        inner.frames += 1;
        Ok((frame, true))
    }

    /// Gives every frame back, for the object's last mapping going. Returns how many
    /// there were.
    pub(crate) fn release_all(&self, frames: &mut dyn FrameSource) -> usize {
        let mut inner = self.inner.lock();
        let mut count = 0;
        for slot in inner.leaves.iter_mut() {
            let Some(leaf) = slot.take() else {
                continue;
            };
            for &frame in leaf.iter() {
                if frame != 0 {
                    frames.put(frame);
                    count += 1;
                }
            }
        }
        inner.frames = 0;
        count
    }
}

/// An object and how many references there are to it.
struct Counted {
    count: AtomicUsize,
    object: SharedAnon,
}

/// A counted reference to a [`SharedAnon`], as an `Arc` would be, but made fallibly:
/// the kernel makes one inside a change to a space, where a kill is held off and a
/// failed allocation couldn't be contained.
pub struct ObjectRef(NonNull<Counted>);

// SAFETY: the object is shared through its own lock, and the count is atomic, as for
// `Arc<SharedAnon>`.
unsafe impl Send for ObjectRef {}
// SAFETY: as above.
unsafe impl Sync for ObjectRef {}

impl ObjectRef {
    /// The first reference to `object`, or `None` if the heap has no room.
    pub(crate) fn new(object: SharedAnon) -> Option<Self> {
        let layout = Layout::new::<Counted>();
        // SAFETY: the layout isn't zero sized.
        let raw = unsafe { alloc(layout) }.cast::<Counted>();
        let pointer = NonNull::new(raw)?;
        // SAFETY: just allocated with `Counted`'s layout, and not yet initialised.
        unsafe { pointer.as_ptr().write(Counted { count: AtomicUsize::new(1), object }) };
        Some(Self(pointer))
    }

    fn counted(&self) -> &Counted {
        // SAFETY: the allocation lives while any reference does, this one included.
        unsafe { self.0.as_ref() }
    }

    /// Whether two references are to the same object.
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.0 == other.0
    }

    /// The object's address, to tell objects apart.
    pub fn as_ptr(this: &Self) -> *const SharedAnon {
        &this.counted().object
    }

    /// References to the object, this one included.
    pub fn count(this: &Self) -> usize {
        this.counted().count.load(Ordering::Acquire)
    }
}

impl Clone for ObjectRef {
    fn clone(&self) -> Self {
        // A reference is held by a VMA, of which a space has at most `MAX_VMAS`, so the
        // count can't come near overflowing.
        self.counted().count.fetch_add(1, Ordering::Relaxed);
        Self(self.0)
    }
}

impl Drop for ObjectRef {
    fn drop(&mut self) {
        if self.counted().count.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        // Everything any other reference did to the object happens before it goes.
        fence(Ordering::Acquire);
        // SAFETY: this was the last reference, so nothing else can reach the
        // allocation, which was made by `new` with this layout.
        unsafe {
            core::ptr::drop_in_place(self.0.as_ptr());
            dealloc(self.0.as_ptr().cast(), Layout::new::<Counted>());
        }
    }
}

impl core::ops::Deref for ObjectRef {
    type Target = SharedAnon;

    fn deref(&self) -> &SharedAnon {
        &self.counted().object
    }
}

impl core::fmt::Debug for ObjectRef {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.counted().object.fmt(out)
    }
}
