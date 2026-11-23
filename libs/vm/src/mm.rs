//! [`Mm`]: a process's address space, VMAs and page tables together, and the rules
//! that keep them and the machine's promises in step.
//!
//! What a process is charged for, in pages, is three counts: `charged` (pages promised
//! to it, with or without their frames yet), `owned` (frames it has that no promise
//! covers: a read-only image page, a `MAP_NORESERVE` page that was written) and its page
//! tables, and `held`, frames the kernel holds for the process outside its space while
//! a call of its uses them (a spawn's staged arguments). Their sum is what
//! `USER_MEMORY_LIMIT` limits. `unbacked` is the part of
//! `charged` without frames, which is exactly what this space holds in [`Promises`].
//!
//! A private charged range's share of `unbacked` is its pages less the frames of its
//! own it has. Every change below keeps both in step.
//!
//! A shared-anonymous object's promise is the object's own (see [`SharedAnon`]), since
//! after a fork several spaces map it and any of them may touch a page or be the last to
//! let it go. What it counts against a space's limit is kept per space, in the space's
//! list of the objects it maps: the object's pages while it's charged, otherwise the
//! frames this space's touches made for it, and its heap; a space's count goes when its
//! last mapping of the object does.
//!
//! # Fork
//!
//! [`Mm::fork`] copies a space for a child. Shared mappings share their object or file;
//! a private page with a frame of the space's own is shared copy-on-write, both entries
//! read-only with the [`COW`](crate::COW) bit, and the frame's map count raised; a
//! private page some copy of the kernel's has pinned while it's writable is copied for
//! the child at once, so what the kernel writes through the pin can't show up there.
//!
//! Charging follows frames, not spaces: every extra reference a fork adds to a frame of a
//! space's own holds one promise, so the debt is always the sum over such frames of their
//! map count less one, beside every charged page without a frame. The first write to a
//! shared frame copies it with that promise; a space letting go of one that another
//! still has gives one back ([`FrameSource::put_shared`] says which, in one step); the
//! last reference gives back nothing more. So whoever writes first, its copy was
//! promised. A frame at map count one, pinned by nothing, is taken back in place by a
//! write; a page-cache frame never is, as it's the file's.

use alloc::vec::Vec;

use crate::shared::{ObjectRef, SharedAnon};
use crate::vma::{Backing, Prot, Vma, VmaList};
use crate::{AddressSpace, Entry, FileKey, FileRef, FrameSource, PageBits, Promises, LEAF_SPAN, PAGE_SIZE};

/// What a fault, or a copy the kernel is about to make, wants to do with a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Access {
    pub write: bool,
    pub execute: bool,
}

/// Why a fault couldn't be answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultError {
    /// Nothing is mapped there, or not for that access: `SIGSEGV`.
    Segv,
    /// The page is mapped, but there's no frame to give it: `SIGBUS`.
    Bus,
}

/// Why a mapping call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmError {
    /// `EINVAL`.
    Invalid,
    /// `ENOMEM`: no promise, over the limit, past the cap, or not mapped.
    NoMemory,
    /// `EEXIST`.
    Exists,
    /// `EACCES`: not something this memory may be made.
    Denied,
    /// `ETXTBSY`: a file open for writing mapped to run.
    TextBusy,
}

/// Why a page couldn't be pinned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinError {
    /// Not mapped for that access.
    Fault,
    /// Its pin count is at the cap: `EAGAIN`.
    Busy,
    /// Mapped, but there was no frame to give it: `ENOMEM`.
    NoFrame,
}

/// What a futex word is, for its key: in the space's own memory, where its address in
/// the space names it, or in memory other spaces may map too, where the object or file
/// and the offset in it do, so every process mapping it agrees on the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordKey {
    /// Private memory, at this address.
    Own(usize),
    /// A shared-anonymous object or a shared file mapping, named by its address (an
    /// allocation that lives as long as anything maps it), at this byte offset.
    Shared { object: usize, offset: u64 },
}

/// Where a mapping goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// The lowest gap in the mmap area, with a guard page either side.
    Anywhere,
    /// Here if it's free and in the mmap area, otherwise anywhere.
    Hint(usize),
    /// Exactly here in the mmap area, replacing what's there.
    Fixed(usize),
    /// Exactly here in the mmap area, if nothing's there.
    FixedNoReplace(usize),
    /// Exactly here anywhere in the window, if nothing's there: for the kernel's own
    /// mappings, the program image and the stack.
    Exact(usize),
}

/// A file to map, from its page `first` on. `may_write`: it's open for writing, which a
/// shared mapping needs to be writable.
#[derive(Clone, Debug)]
pub struct FileMap {
    pub file: FileRef,
    pub first: u64,
    pub may_write: bool,
}

/// A mapping to make.
#[derive(Clone, Debug)]
pub struct MapRequest {
    /// Bytes, a whole number of pages.
    pub length: usize,
    pub prot: Prot,
    pub place: Place,
    /// `MAP_SHARED`: a new shared-anonymous object.
    pub shared: bool,
    /// `MAP_NORESERVE`.
    pub noreserve: bool,
    /// `MAP_POPULATE`.
    pub populate: bool,
    /// Promise every page whatever `prot` says, as v1's `MMAP` did.
    pub charge_all: bool,
    /// The program's image: populated with frames of its own, read-only pages too, for
    /// the kernel to load.
    pub image: bool,
    /// A file's pages rather than anonymous memory; with `shared`, never copied.
    pub file: Option<FileMap>,
}

impl MapRequest {
    /// Private anonymous memory, anywhere.
    pub const fn anonymous(length: usize, prot: Prot) -> Self {
        Self {
            length,
            prot,
            place: Place::Anywhere,
            shared: false,
            noreserve: false,
            populate: false,
            charge_all: false,
            image: false,
            file: None,
        }
    }
}

/// What a space may be.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    /// Every user address: `[start, end)`.
    pub window: (usize, usize),
    /// Where mappings that don't say where go, and where fixed ones may.
    pub area: (usize, usize),
    /// Most pages charged, owned and in tables.
    pub page_limit: usize,
    /// Most VMAs.
    pub max_vmas: usize,
    /// Most bytes of kernel heap the shared objects this space made may hold, besides
    /// their leaves: see [`SharedAnon::heap_cost`].
    pub object_heap_limit: usize,
    /// The space's number, as files' reverse maps know it.
    pub space: u32,
}

/// Where file pages come from: the page cache, and the files behind it.
pub trait FilePages {
    /// The cache's frame for page `index` of `file`, read in if it isn't there, with a
    /// reference taken for the entry about to point at it. With `exec` the entry will
    /// let it run, so the instruction cache must agree with it first. `Bus` for a page
    /// wholly past the end of the file, a file that's gone, or no frame to read it into.
    /// `frames` is where the space's frames come from, and the reference goes back to.
    fn page(&mut self, file: &FileRef, index: u64, exec: bool, frames: &mut dyn FrameSource) -> Result<usize, FaultError>;

    /// A writable shared mapping's first write to page `index` of `file`, whose frame
    /// the entry has as `frame`: the page is dirty from now on, before the entry lets it
    /// be written. `Bus` if the page isn't that frame any more, so the write has nowhere
    /// to go.
    fn dirty(&mut self, file: &FileRef, index: u64, frame: usize) -> Result<(), FaultError>;
}

/// No files: for spaces that map none, such as a test's.
pub struct NoFiles;

impl FilePages for NoFiles {
    fn page(&mut self, _file: &FileRef, _index: u64, _exec: bool, _frames: &mut dyn FrameSource) -> Result<usize, FaultError> {
        Err(FaultError::Bus)
    }

    fn dirty(&mut self, _file: &FileRef, _index: u64, _frame: usize) -> Result<(), FaultError> {
        Err(FaultError::Bus)
    }
}

/// What a change to a space needs from the machine.
pub struct Ctx<'a> {
    pub frames: &'a mut dyn FrameSource,
    pub promises: &'a Promises,
    /// Forgets a user page, or a table's span, on every CPU that may have it cached.
    pub flush: &'a mut dyn FnMut(usize),
    pub files: &'a mut dyn FilePages,
}

/// A frame the kernel is copying through, which can't go until it's [unpinned](Mm::unpin).
#[derive(Debug)]
#[must_use]
pub struct Pinned {
    /// The kernel's address for the user address that was pinned.
    pub address: usize,
    frame: Option<usize>,
}

impl Pinned {
    /// Lets the frame go. Needs nothing of the space, so a copy that ran without its
    /// lock needn't take it again.
    pub fn unpin(self, frames: &mut dyn FrameSource) {
        if let Some(frame) = self.frame {
            frames.unpin(frame);
        }
    }
}

/// A process's address space.
#[derive(Debug)]
pub struct Mm {
    space: AddressSpace,
    vmas: VmaList,
    /// The machine's zero frame, mapped read-only for reads of untouched private pages.
    /// Never counted, never freed here.
    zero: usize,
    layout: Layout,
    charged: usize,
    owned: usize,
    unbacked: usize,
    /// Frames the kernel holds for the process outside the space: see [`hold`](Self::hold).
    held: usize,
    /// The shared objects it maps, each once, what each counts here, and their heap.
    objects: Objects,
}

/// The entry a page of `frame` gets under `prot`. The zero frame, the page cache's
/// frames (`cached`) and frames shared copy-on-write (`cow`) are never writable through
/// it.
fn page_entry(frame: usize, prot: Prot, zero: usize, cached: bool, cow: bool) -> Entry {
    if prot.is_none() {
        return Entry::Hidden { frame, cached, cow };
    }
    Entry::Present {
        frame,
        writable: prot.write && frame != zero && !cached && !cow,
        executable: prot.exec,
        cached,
        cow,
    }
}

/// The entry of a writable shared mapping's page of the cache once it's dirty: the one
/// entry of the cache's frames that's writable.
const fn dirty_entry(frame: usize, prot: Prot) -> Entry {
    Entry::Present { frame, writable: true, executable: prot.exec, cached: true, cow: false }
}

/// Whether an entry's frame is shared copy-on-write.
const fn entry_cow(entry: Entry) -> bool {
    matches!(entry, Entry::Present { cow: true, .. } | Entry::Hidden { cow: true, .. })
}

/// The same entry, marked as shared copy-on-write: read-only, whatever its range is.
const fn shared_entry(entry: Entry) -> Entry {
    match entry {
        Entry::Present { frame, executable, cached, .. } => Entry::Present { frame, writable: false, executable, cached, cow: true },
        Entry::Hidden { frame, cached, .. } => Entry::Hidden { frame, cached, cow: true },
        Entry::Empty => Entry::Empty,
    }
}

/// A shared object a space maps, and what it counts against that space's limit: its
/// pages while it's charged, otherwise the frames this space's touches made for it.
/// Named by its address, which stays the object's while any VMA of the space holds it.
#[derive(Debug, Clone, Copy)]
struct Attributed {
    object: usize,
    charged: usize,
    owned: usize,
    /// The space's VMAs that map part of it: it stops counting here with the last.
    pieces: usize,
}

/// The shared objects a space maps, each once, sorted by address so a fault, a split or
/// an unmap finds one in a few steps however many there are, and what they count here.
#[derive(Debug, Default)]
struct Objects {
    list: Vec<Attributed>,
    /// The sums of their counts, which `charged` and `owned` add.
    charged: usize,
    owned: usize,
    /// Heap they hold, by [`SharedAnon::heap_cost`].
    heap: usize,
}

impl Objects {
    fn find(&mut self, object: usize) -> Option<&mut Attributed> {
        let at = self.list.binary_search_by_key(&object, |counted| counted.object).ok()?;
        self.list.get_mut(at)
    }

    /// Counts `object` here, if it isn't already: `charged` pages, and its heap. Room in
    /// the list must have been made.
    fn attribute(&mut self, object: &ObjectRef, charged: usize) {
        let address = ObjectRef::as_ptr(object) as usize;
        let Err(at) = self.list.binary_search_by_key(&address, |counted| counted.object) else {
            return;
        };
        debug_assert!(self.list.len() < self.list.capacity(), "no room made for an object");
        self.list.insert(at, Attributed { object: address, charged, owned: 0, pieces: 0 });
        self.charged += charged;
        self.heap += SharedAnon::heap_cost(object.pages());
    }

    /// One more or one fewer VMA of this space maps part of the object `vma` does, if
    /// it's a shared one: the last going takes what the object counted here with it.
    fn piece(&mut self, vma: &Vma, more: bool) {
        let Backing::Shared { object, .. } = &vma.backing else {
            return;
        };
        let address = ObjectRef::as_ptr(object) as usize;
        let Ok(at) = self.list.binary_search_by_key(&address, |counted| counted.object) else {
            return;
        };
        let counted = &mut self.list[at];
        if more {
            counted.pieces += 1;
            return;
        }
        counted.pieces -= 1;
        if counted.pieces == 0 {
            let counted = self.list.remove(at);
            self.charged -= counted.charged;
            self.owned -= counted.owned;
            self.heap -= SharedAnon::heap_cost(object.pages());
        }
    }
}

const fn allowed(prot: Prot, access: Access) -> bool {
    !prot.is_none() && (!access.write || prot.write) && (!access.execute || prot.exec)
}

/// An entry's frame, whether the hardware may have it, and whether it's the cache's.
const fn entry_frame(entry: Entry) -> Option<(usize, bool, bool)> {
    match entry {
        Entry::Present { frame, cached, .. } => Some((frame, true, cached)),
        Entry::Hidden { frame, cached, .. } => Some((frame, false, cached)),
        Entry::Empty => None,
    }
}

/// Lets go of an entry's reference to a frame of a space's own. One a fork left another
/// space sharing gives back the promise its extra reference held (see [`Mm::fork`]); the
/// last reference is an ordinary put.
fn put_own(frames: &mut dyn FrameSource, promises: &Promises, frame: usize) {
    if frames.put_shared(frame) {
        promises.release(1);
    } else {
        frames.put(frame);
    }
}

impl Mm {
    /// An empty space. `zero` is the machine's zero frame.
    pub fn new(frames: &mut dyn FrameSource, bits: PageBits, layout: Layout, zero: usize) -> Option<Self> {
        let (low, high) = layout.area;
        if low < layout.window.0 || high > layout.window.1 || low >= high {
            return None;
        }
        Some(Self {
            space: AddressSpace::new(frames, bits, layout.window)?,
            vmas: VmaList::new(layout.max_vmas),
            zero,
            layout,
            charged: 0,
            owned: 0,
            unbacked: 0,
            held: 0,
            objects: Objects::default(),
        })
    }

    /// The page tables, for translating and copying.
    pub const fn space(&self) -> &AddressSpace {
        &self.space
    }

    /// The top table's address.
    pub const fn root(&self) -> usize {
        self.space.root()
    }

    /// The VMAs.
    pub const fn vmas(&self) -> &VmaList {
        &self.vmas
    }

    /// Pages promised to this space, a charged shared object's among them.
    pub const fn charged(&self) -> usize {
        self.charged + self.objects.charged
    }

    /// Frames this space has that no promise covers.
    pub const fn owned(&self) -> usize {
        self.owned + self.objects.owned
    }

    /// Of the pages promised, those still without frames: what this space holds in
    /// [`Promises`], with the promises of the charged objects it maps. An object shared
    /// with another space after a fork is in both spaces' counts, but promised once.
    pub fn unbacked(&self) -> usize {
        self.unbacked + self.objects.list.iter().filter(|counted| counted.charged > 0).map(|counted| {
            // SAFETY: an object in the list is one a VMA of this space holds a reference
            // to, and the list loses it before the last such VMA goes.
            let object = unsafe { &*(counted.object as *const SharedAnon) };
            if object.charged() {
                object.pages() - object.frames()
            } else {
                0
            }
        }).sum::<usize>()
    }

    /// Of the pages promised, those of private ranges without frames: this space's own
    /// share of [`Promises`], whatever objects it maps.
    pub const fn private_unbacked(&self) -> usize {
        self.unbacked
    }

    /// Page tables below the root.
    pub const fn tables(&self) -> usize {
        self.space.tables()
    }

    /// Heap the shared objects this space made hold, besides their leaves.
    pub const fn object_heap(&self) -> usize {
        self.objects.heap
    }

    /// Pages this space has frames for, as far as it's cheap to say: its private pages'
    /// frames, and those its touches made for uncharged objects. For `maxrss`, on every
    /// fault, so a charged object's frames, which any sharer may have made, aren't counted.
    pub const fn resident(&self) -> usize {
        (self.charged - self.unbacked) + self.owned + self.objects.owned
    }

    /// Frames the kernel holds for the process outside the space.
    pub const fn held(&self) -> usize {
        self.held
    }

    /// What counts against the limit: pages charged or owned, tables, and frames held.
    pub const fn committed(&self) -> usize {
        self.charged() + self.owned() + self.space.tables() + self.held
    }

    /// Counts `pages` frames the kernel takes for the process outside the space (a
    /// spawn's staged arguments) against its limit, for as long as they're held: `false`,
    /// and nothing counted, past the limit. [`unhold`](Self::unhold) gives them back.
    pub fn hold(&mut self, pages: usize) -> bool {
        if !self.fits(pages) {
            return false;
        }
        self.held += pages;
        true
    }

    /// Gives back what [`hold`](Self::hold) counted.
    pub fn unhold(&mut self, pages: usize) {
        debug_assert!(pages <= self.held);
        self.held -= pages.min(self.held);
    }

    /// The mmap area.
    pub const fn area(&self) -> (usize, usize) {
        self.layout.area
    }

    /// The most pages this space may have charged, owned and in tables.
    pub const fn page_limit(&self) -> usize {
        self.layout.page_limit
    }

    /// Whether `more` pages charged or owned would keep this space within its limit.
    pub fn fits(&self, more: usize) -> bool {
        self.committed().checked_add(more).is_some_and(|total| total <= self.layout.page_limit)
    }

    /// Whether every page of `[start, start + length)` is mapped.
    pub fn covers(&self, start: usize, length: usize) -> bool {
        start.checked_add(length).is_some_and(|end| self.vmas.covers(start, end))
    }

    /// Whether any of `[start, start + length)` is lent memory, which no call may change.
    pub fn touches_lent(&self, start: usize, length: usize) -> bool {
        let end = start.saturating_add(length);
        self.vmas.in_range(start, end).iter().any(|vma| matches!(vma.backing, Backing::Lent))
    }

    /// Keeps the promise to `[start, end)`'s tables by making them: promise, make, then
    /// give the promise back, as everything that takes frames does.
    fn make_tables(&mut self, start: usize, end: usize, ctx: &mut Ctx<'_>) -> bool {
        let wanted = self.space.tables_wanted(start, end);
        if wanted == 0 {
            return true;
        }
        if !self.fits(wanted) || !ctx.promises.promise(wanted, ctx.frames.free_count()) {
            return false;
        }
        let made = self.space.make_tables(start, end, ctx.frames);
        ctx.promises.release(wanted);
        if !made {
            self.sweep(start, end, ctx);
        }
        made
    }

    /// Frees the empty tables under `[start, end)` that no charged range still needs.
    fn sweep(&mut self, start: usize, end: usize, ctx: &mut Ctx<'_>) {
        let vmas = &self.vmas;
        let keep = |span: usize, length: usize| vmas.in_range(span, span + length).iter().any(|vma| vma.charged);
        self.space.sweep(start, end, &keep, ctx.flush, ctx.frames);
    }

    /// A zeroed frame for a page: promised already if `charged`, otherwise promised,
    /// taken and released here.
    fn alloc_page(
        charged: bool,
        counts: (&mut usize, &mut usize),
        fits: bool,
        ctx: &mut Ctx<'_>,
    ) -> Option<usize> {
        let (owned, unbacked) = counts;
        let frame = if charged {
            let frame = ctx.frames.alloc()?;
            debug_assert!(*unbacked > 0, "a charged page with no promise");
            *unbacked = unbacked.saturating_sub(1);
            ctx.promises.release(1);
            frame
        } else {
            if !fits || !ctx.promises.promise(1, ctx.frames.free_count()) {
                return None;
            }
            let frame = ctx.frames.alloc();
            ctx.promises.release(1);
            let frame = frame?;
            *owned += 1;
            frame
        };
        // SAFETY: the source just gave this frame, and it's a page long.
        unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE) };
        Some(frame)
    }

    /// Makes the tables a fault at `page` needs, for a range nothing promised them to.
    fn tables_for(&mut self, page: usize, ctx: &mut Ctx<'_>) -> bool {
        let missing = self.space.missing_tables(page);
        if missing == 0 {
            return true;
        }
        if !self.fits(missing) || !ctx.promises.promise(missing, ctx.frames.free_count()) {
            return false;
        }
        let made = self.space.make_tables(page, page + PAGE_SIZE, ctx.frames);
        ctx.promises.release(missing);
        made
    }

    /// Gives a private page a frame of its own, for a write, or to load: zeroed, or a
    /// copy of `source`.
    fn own_page(&mut self, page: usize, prot: Prot, charged: bool, source: Option<usize>, ctx: &mut Ctx<'_>) -> Result<(), FaultError> {
        if !self.tables_for(page, ctx) {
            return Err(FaultError::Bus);
        }
        let fits = self.fits(1);
        let frame = Self::alloc_page(charged, (&mut self.owned, &mut self.unbacked), fits, ctx).ok_or(FaultError::Bus)?;
        if let Some(source) = source {
            // SAFETY: both are frames a page long: one just given, the other one an entry
            // or the caller holds a reference to.
            unsafe { core::ptr::copy_nonoverlapping(source as *const u8, frame as *mut u8, PAGE_SIZE) };
        }
        let old = self.space.entry(page);
        if matches!(old, Entry::Present { .. }) {
            // A valid entry for another frame: AArch64 wants break-before-make, the old
            // one gone from every TLB before the new one is written, or a CPU may use
            // either, or take a conflict abort. x86 doesn't mind, and it costs it little.
            self.space.set(page, Entry::Empty);
            (ctx.flush)(page);
        }
        self.space.set(page, page_entry(frame, prot, self.zero, false, false));
        // A page of the cache's that the copy replaces lets go of it.
        if let Some((old_frame, _, true)) = entry_frame(old) {
            ctx.frames.put(old_frame);
        }
        Ok(())
    }

    /// The first write to a private page whose frame a fork shared. Nothing else maps it
    /// any more, and no copy is in flight through it: the frame is this space's again,
    /// written in place (nothing else can come to map it but a fork of this space, which
    /// would need its lock). Otherwise a copy, which the promise the extra reference held
    /// pays for; a frame only this space maps, that a copy in flight keeps it from taking
    /// back, has its copy promised now.
    fn break_cow(&mut self, page: usize, frame: usize, prot: Prot, ctx: &mut Ctx<'_>) -> Result<(), FaultError> {
        let own = |frame: usize| Entry::Present { frame, writable: prot.write, executable: prot.exec, cached: false, cow: false };
        let sole = ctx.frames.maps(frame) == 1;
        if sole && ctx.frames.pins(frame) == 0 {
            self.space.set(page, own(frame));
            (ctx.flush)(page);
            return Ok(());
        }
        if sole && !ctx.promises.promise(1, ctx.frames.free_count()) {
            return Err(FaultError::Bus);
        }
        let copy = ctx.frames.alloc();
        if sole {
            ctx.promises.release(1);
        }
        let copy = copy.ok_or(FaultError::Bus)?;
        // The frame can't change meanwhile: while this space still has its reference, the
        // others that share it can only copy it too.
        // SAFETY: both are frames a page long: one just given, the other one this space's
        // entry holds a reference to.
        unsafe { core::ptr::copy_nonoverlapping(frame as *const u8, copy as *mut u8, PAGE_SIZE) };
        // Break before make, as in `own_page`.
        self.space.set(page, Entry::Empty);
        (ctx.flush)(page);
        self.space.set(page, own(copy));
        // The reference goes. If another space still has the frame, the promise this one
        // held is what the copy used; if not, the frame goes once no CPU can reach it.
        if ctx.frames.put_shared(frame) {
            ctx.promises.release(1);
        } else {
            ctx.frames.put(frame);
        }
        Ok(())
    }

    /// Answers a fault at `address`: gives the page what its VMA says it should have,
    /// or says why not. Also how the kernel readies a page it's about to copy through.
    pub fn fault(&mut self, address: usize, access: Access, ctx: &mut Ctx<'_>) -> Result<(), FaultError> {
        if !self.space.in_window(address, 1) {
            return Err(FaultError::Segv);
        }
        let page = address & !(PAGE_SIZE - 1);
        let index = self.vmas.find(page).ok_or(FaultError::Segv)?;
        let vma = &self.vmas.get(index).ok_or(FaultError::Segv)?;
        let (prot, charged) = (vma.prot, vma.charged);
        enum Source {
            Private,
            Object(ObjectRef, usize),
            File(FileRef, u64, bool),
        }
        let source = match &vma.backing {
            Backing::Private => Source::Private,
            Backing::Shared { object, first } => Source::Object(object.clone(), first + (page - vma.start) / PAGE_SIZE),
            Backing::File { file, first, shared, .. } => {
                Source::File(file.clone(), first + ((page - vma.start) / PAGE_SIZE) as u64, *shared)
            }
            Backing::Lent => {
                return match self.space.entry(page) {
                    Entry::Present { writable, .. } if allowed(prot, access) && (writable || !access.write) => Ok(()),
                    _ => Err(FaultError::Segv),
                };
            }
        };
        if !allowed(prot, access) {
            return Err(FaultError::Segv);
        }
        match (self.space.entry(page), source) {
            (Entry::Present { frame, writable, .. }, Source::Private) if access.write && !writable && frame == self.zero => {
                // The first write to a page that was only read: a frame of its own.
                self.own_page(page, prot, charged, None, ctx)
            }
            (Entry::Present { frame, cached: true, .. }, Source::File(_, _, false)) if access.write => {
                // The first write to a private page of a file: a copy of the space's own.
                self.own_page(page, prot, charged, Some(frame), ctx)
            }
            (Entry::Present { frame, cow: true, .. }, Source::Private | Source::File(_, _, false)) if access.write => {
                // The first write to a frame a fork shares: a copy, or the frame back.
                self.break_cow(page, frame, prot, ctx)
            }
            (Entry::Present { frame, cached: true, writable: false, .. }, Source::File(file, index, true)) if access.write => {
                // The first write through a writable shared mapping since the page was
                // mapped: dirty before the entry lets it be written, so write-back knows.
                ctx.files.dirty(&file, index, frame)?;
                self.space.set(page, dirty_entry(frame, prot));
                (ctx.flush)(page);
                Ok(())
            }
            (old @ (Entry::Present { .. } | Entry::Hidden { .. }), _) => {
                // The entry is behind its VMA, or a CPU had a stale one: bring it up to
                // date, and the access goes through when it's tried again.
                let Some((frame, _, cached)) = entry_frame(old) else {
                    return Ok(());
                };
                let mut new = page_entry(frame, prot, self.zero, cached, entry_cow(old));
                // A shared page made writable by a write stays so while its range is.
                if let (Entry::Present { writable: true, cached: true, .. }, Entry::Present { writable, .. }) = (old, &mut new) {
                    *writable = prot.write;
                }
                if new != old {
                    self.space.set(page, new);
                    (ctx.flush)(page);
                }
                Ok(())
            }
            (Entry::Empty, Source::Private) if access.write => self.own_page(page, prot, charged, None, ctx),
            // Nothing anonymous runs: there's nothing there to run.
            (Entry::Empty, Source::Private) if access.execute => Err(FaultError::Segv),
            (Entry::Empty, Source::Private) => {
                if !self.tables_for(page, ctx) {
                    return Err(FaultError::Bus);
                }
                self.space.set(page, page_entry(self.zero, prot, self.zero, false, false));
                Ok(())
            }
            (Entry::Empty, Source::Object(object, index)) => {
                if !self.tables_for(page, ctx) {
                    return Err(FaultError::Bus);
                }
                let fits = self.fits(1);
                let mut uncharged = false;
                // A charged object's page takes its frame from the object's promise; an
                // uncharged one's is promised now, and counts against this space's limit.
                let mut make = |charged: bool| {
                    let frame = if charged {
                        let frame = ctx.frames.alloc()?;
                        ctx.promises.release(1);
                        frame
                    } else {
                        if !fits || !ctx.promises.promise(1, ctx.frames.free_count()) {
                            return None;
                        }
                        let frame = ctx.frames.alloc();
                        ctx.promises.release(1);
                        uncharged = true;
                        frame?
                    };
                    // SAFETY: the source just gave this frame, and it's a page long.
                    unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE) };
                    Some(frame)
                };
                let (frame, made) = object.frame_or_make(index, &mut make).map_err(|_| FaultError::Bus)?;
                if made && uncharged {
                    let address = ObjectRef::as_ptr(&object) as usize;
                    if let Some(counted) = self.objects.find(address) {
                        counted.owned += 1;
                        self.objects.owned += 1;
                    }
                }
                if !ctx.frames.get(frame) {
                    return Err(FaultError::Bus);
                }
                self.space.set(page, page_entry(frame, prot, self.zero, false, false));
                Ok(())
            }
            (Entry::Empty, Source::File(file, index, shared)) => {
                if !self.tables_for(page, ctx) {
                    return Err(FaultError::Bus);
                }
                // The entry will let it run whenever the range does, whatever this access.
                let frame = ctx.files.page(&file, index, prot.exec, &mut *ctx.frames)?;
                if access.write && !shared {
                    // Written before it was read: copied straight away, and the cache's
                    // reference given back.
                    let copied = self.own_page(page, prot, charged, Some(frame), ctx);
                    ctx.frames.put(frame);
                    return copied;
                }
                if access.write {
                    // Written through a shared mapping: dirty first, then writable.
                    if let Err(error) = ctx.files.dirty(&file, index, frame) {
                        ctx.frames.put(frame);
                        return Err(error);
                    }
                    self.space.set(page, dirty_entry(frame, prot));
                    return Ok(());
                }
                self.space.set(page, page_entry(frame, prot, self.zero, true, false));
                Ok(())
            }
        }
    }

    /// Readies every page of a user range the kernel is about to copy through, for
    /// reading or for writing, as a fault from the program would.
    pub fn fault_in(&mut self, address: usize, length: usize, write: bool, ctx: &mut Ctx<'_>) -> Result<(), FaultError> {
        if length == 0 {
            return Ok(());
        }
        if !self.space.in_window(address, length) {
            return Err(FaultError::Segv);
        }
        let first = address / PAGE_SIZE;
        let last = (address + length - 1) / PAGE_SIZE;
        for page in first..=last {
            let page = page * PAGE_SIZE;
            match self.space.entry(page) {
                Entry::Present { writable, .. } if writable || !write => {}
                _ => self.fault(page, Access { write, execute: false }, ctx)?,
            }
        }
        Ok(())
    }

    /// Pins the frame behind `address` for a copy, if the program could make this
    /// access. The zero frame and lent frames aren't counted, and don't need it.
    pub fn pin(&self, address: usize, write: bool, frames: &mut dyn FrameSource) -> Result<Pinned, PinError> {
        let at = self.space.translate(address, write).map_err(|_| PinError::Fault)?;
        let frame = at & !(PAGE_SIZE - 1);
        let counted = frame != self.zero
            && self
                .vmas
                .find(address)
                .and_then(|index| self.vmas.get(index))
                .is_some_and(|vma| !matches!(vma.backing, Backing::Lent));
        if counted && !frames.pin(frame) {
            return Err(PinError::Busy);
        }
        Ok(Pinned { address: at, frame: counted.then_some(frame) })
    }

    /// Readies the page at `address` for a copy the kernel is about to make, as a touch
    /// from the program would, and pins its frame, as one step: nothing else holds the
    /// space meanwhile, so the frame pinned is the one the entry has, for that access.
    /// The copy can then run without the space, and the frame stays until it's
    /// unpinned whatever happens to the mapping. On failure nothing is pinned.
    pub fn translate_and_pin(&mut self, address: usize, write: bool, ctx: &mut Ctx<'_>) -> Result<Pinned, PinError> {
        self.fault_in(address, 1, write, ctx).map_err(|error| match error {
            FaultError::Segv => PinError::Fault,
            FaultError::Bus => PinError::NoFrame,
        })?;
        self.pin(address, write, ctx.frames)
    }

    /// What names the futex word at `address`, if anything is mapped there.
    pub fn word_key(&self, address: usize) -> Option<WordKey> {
        let vma = self.vmas.get(self.vmas.find(address)?)?;
        let within = (address - vma.start) as u64;
        Some(match &vma.backing {
            Backing::Shared { object, first } => WordKey::Shared {
                object: ObjectRef::as_ptr(object) as usize,
                offset: (*first * PAGE_SIZE) as u64 + within,
            },
            Backing::File { file, first, shared: true, .. } => WordKey::Shared {
                object: FileRef::as_ptr(file) as usize,
                offset: *first * PAGE_SIZE as u64 + within,
            },
            _ => WordKey::Own(address),
        })
    }

    /// Calls `each` with every shared object or file mapped in `[start, end)`, and the
    /// byte offsets in it that are: where futex words named by [`WordKey::Shared`] would
    /// lose their memory if the range went.
    pub fn shared_words(&self, start: usize, end: usize, each: &mut dyn FnMut(usize, u64, u64)) {
        for vma in self.vmas.in_range(start, end) {
            let (low, high) = (vma.start.max(start), vma.end.min(end));
            let (object, first) = match &vma.backing {
                Backing::Shared { object, first } => (ObjectRef::as_ptr(object) as usize, (*first * PAGE_SIZE) as u64),
                Backing::File { file, first, shared: true, .. } => (FileRef::as_ptr(file) as usize, *first * PAGE_SIZE as u64),
                _ => continue,
            };
            each(object, first + (low - vma.start) as u64, first + (high - vma.start) as u64);
        }
    }

    /// Lets a pinned frame go.
    pub fn unpin(&self, pinned: Pinned, frames: &mut dyn FrameSource) {
        pinned.unpin(frames);
    }

    /// Maps memory, and says where. Atomic: on failure nothing has changed, but for a
    /// `populate` that runs out, which leaves the range unmapped.
    pub fn map(&mut self, request: &MapRequest, ctx: &mut Ctx<'_>) -> Result<usize, MmError> {
        let length = request.length;
        if length == 0 || !length.is_multiple_of(PAGE_SIZE) {
            return Err(MmError::Invalid);
        }
        let (low, high) = self.layout.area;
        let in_area = |at: usize| at.checked_add(length).is_some_and(|end| at >= low && end <= high);
        let (start, replace) = match request.place {
            Place::Fixed(at) | Place::FixedNoReplace(at) => {
                if !at.is_multiple_of(PAGE_SIZE) {
                    return Err(MmError::Invalid);
                }
                if !in_area(at) {
                    return Err(MmError::NoMemory);
                }
                if !self.vmas.is_free(at, at + length) {
                    if matches!(request.place, Place::FixedNoReplace(_)) {
                        return Err(MmError::Exists);
                    }
                    if self.touches_lent(at, length) {
                        return Err(MmError::Invalid);
                    }
                }
                (at, matches!(request.place, Place::Fixed(_)))
            }
            Place::Exact(at) => {
                if !at.is_multiple_of(PAGE_SIZE) || !self.space.in_window(at, length) {
                    return Err(MmError::Invalid);
                }
                if !self.vmas.is_free(at, at + length) {
                    return Err(MmError::Exists);
                }
                (at, false)
            }
            Place::Hint(hint) => {
                let at = hint & !(PAGE_SIZE - 1);
                if in_area(at) && self.vmas.is_free(at, at + length) {
                    (at, false)
                } else {
                    (self.vmas.gap(length, low, high).ok_or(MmError::NoMemory)?, false)
                }
            }
            Place::Anywhere => (self.vmas.gap(length, low, high).ok_or(MmError::NoMemory)?, false),
        };
        let end = start + length;
        let pages = length / PAGE_SIZE;
        let shared_file = request.shared && request.file.is_some();
        // A shared writable mapping writes the file: it has to be open for writing.
        let shared_write = shared_file && request.prot.write;
        if shared_write && !request.file.as_ref().is_some_and(|map| map.may_write) {
            return Err(MmError::Denied);
        }
        let charged = !request.noreserve && (request.charge_all || request.prot.write) && !shared_file;

        // Everything that can fail, before anything changes.
        self.vmas.reserve_for(start, end, 1, true).map_err(|_| MmError::NoMemory)?;
        // A file's lock-table counts: what runs can't be written, nor what's mapped shared
        // and writable run (never both: W^X), and the reverse map.
        if let Some(map) = &request.file {
            if request.prot.exec {
                map.file.map_exec(pages).map_err(|_| MmError::TextBusy)?;
            }
            if shared_write {
                map.file.map_shared_write(pages).map_err(|_| MmError::TextBusy)?;
            }
            if !map.file.add_mapper(self.layout.space, pages) {
                if request.prot.exec {
                    map.file.unmap_exec(pages);
                }
                if shared_write {
                    map.file.unmap_shared_write(pages);
                }
                return Err(MmError::NoMemory);
            }
        }
        let undo_file = |request: &MapRequest, space: u32| {
            if let Some(map) = &request.file {
                if request.prot.exec {
                    map.file.unmap_exec(pages);
                }
                if shared_write {
                    map.file.unmap_shared_write(pages);
                }
                map.file.remove_mapper(space, pages);
            }
        };
        if charged {
            // What a replace gives back isn't counted: the limit is kept either way.
            if !self.fits(pages + self.space.tables_wanted(start, end))
                || !ctx.promises.promise(pages, ctx.frames.free_count())
            {
                undo_file(request, self.layout.space);
                return Err(MmError::NoMemory);
            }
        }
        // A shared object's heap is counted against this space's budget, and made
        // fallibly: a kill is held off here, so a failed allocation would stop the
        // machine rather than end the program.
        let anonymous_shared = request.shared && request.file.is_none();
        let heap = if anonymous_shared { SharedAnon::heap_cost(pages) } else { 0 };
        let object = if anonymous_shared {
            let room = self.objects.heap.checked_add(heap).is_some_and(|total| total <= self.layout.object_heap_limit)
                && self.objects.list.try_reserve(1).is_ok();
            let object = room.then(|| SharedAnon::new(pages, charged)).flatten().and_then(ObjectRef::new);
            if object.is_none() {
                if charged {
                    ctx.promises.release(pages);
                }
                return Err(MmError::NoMemory);
            }
            object
        } else {
            None
        };
        let backing = match (object, &request.file) {
            (Some(object), _) => Backing::Shared { object, first: 0 },
            (None, Some(map)) => Backing::File {
                file: map.file.clone(),
                first: map.first,
                shared: request.shared,
                may_write: map.may_write,
            },
            (None, None) => Backing::Private,
        };
        if charged && !self.make_tables(start, end, ctx) {
            ctx.promises.release(pages);
            undo_file(request, self.layout.space);
            return Err(MmError::NoMemory);
        }

        // Nothing fails from here.
        if replace {
            self.remove(start, end, ctx);
        }
        match &backing {
            // The object's promise is its own; this space counts its pages, and its heap.
            Backing::Shared { object, .. } => self.objects.attribute(object, if charged { pages } else { 0 }),
            _ if charged => {
                self.charged += pages;
                self.unbacked += pages;
            }
            _ => {}
        }
        let vma = Vma {
            start,
            end,
            prot: request.prot,
            backing,
            charged,
            noreserve: request.noreserve,
            image: request.image,
            written: request.prot.write,
        };
        self.objects.piece(&vma, true);
        let objects = &mut self.objects;
        self.vmas.insert(vma, &mut |joined| objects.piece(joined, false));
        if replace {
            self.sweep(start, end, ctx);
        }
        if (request.populate || request.image) && self.populate(start, end, request.image, ctx).is_err() {
            // The range is ours, so taking it out can't fail.
            self.remove(start, end, ctx);
            self.sweep(start, end, ctx);
            return Err(MmError::NoMemory);
        }
        Ok(start)
    }

    /// Gives every page of a new mapping what a touch would: a frame of its own for a
    /// writable private page, or with `own` for any; the zero frame for a read-only one;
    /// its object's frame for a shared one.
    fn populate(&mut self, start: usize, end: usize, own: bool, ctx: &mut Ctx<'_>) -> Result<(), FaultError> {
        let Some(index) = self.vmas.find(start) else {
            return Err(FaultError::Segv);
        };
        let (prot, charged, shared_file) = match self.vmas.get(index) {
            Some(vma) => (vma.prot, vma.charged, vma.shared_file()),
            None => return Err(FaultError::Segv),
        };
        // A shared file page is read in, not written: populating doesn't make it dirty.
        let write = prot.write && !shared_file;
        let mut page = start;
        while page < end {
            if matches!(self.space.entry(page), Entry::Empty) {
                if own {
                    self.own_page(page, prot, charged, None, ctx)?;
                } else if !prot.is_none() {
                    self.fault(page, Access { write, execute: false }, ctx)?;
                }
            }
            page += PAGE_SIZE;
        }
        Ok(())
    }

    /// Maps frames the kernel lends, such as a window surface, read and write, anywhere.
    pub fn map_lent(&mut self, first_frame: usize, length: usize, ctx: &mut Ctx<'_>) -> Result<usize, MmError> {
        if length == 0 || !length.is_multiple_of(PAGE_SIZE) || !first_frame.is_multiple_of(PAGE_SIZE) {
            return Err(MmError::Invalid);
        }
        let (low, high) = self.layout.area;
        let start = self.vmas.gap(length, low, high).ok_or(MmError::NoMemory)?;
        let end = start + length;
        self.vmas.reserve_for(start, end, 1, true).map_err(|_| MmError::NoMemory)?;
        // Lent frames are the program's memory as much as any, so they count against
        // its limit as frames it owns, until the range goes with the space.
        let pages = length / PAGE_SIZE;
        if !self.fits(pages + self.space.tables_wanted(start, end)) || !self.make_tables(start, end, ctx) {
            return Err(MmError::NoMemory);
        }
        self.owned += pages;
        let mut page = start;
        while page < end {
            let entry = Entry::Present { frame: first_frame + (page - start), writable: true, executable: false, cached: false, cow: false };
            self.space.set(page, entry);
            page += PAGE_SIZE;
        }
        self.vmas.insert(Vma {
            start,
            end,
            prot: Prot::READ_WRITE,
            backing: Backing::Lent,
            charged: false,
            noreserve: false,
            image: false,
            written: true,
        }, &mut |_| {});
        Ok(start)
    }

    /// Unmaps `[start, start + length)`, page aligned and in the window, holes and all.
    pub fn unmap(&mut self, start: usize, length: usize, ctx: &mut Ctx<'_>) -> Result<(), MmError> {
        let end = start.checked_add(length).ok_or(MmError::Invalid)?;
        if self.touches_lent(start, length) {
            return Err(MmError::Invalid);
        }
        self.vmas.reserve_for(start, end, 0, true).map_err(|_| MmError::NoMemory)?;
        self.remove(start, end, ctx);
        self.sweep(start, end, ctx);
        Ok(())
    }

    /// Splits the VMA holding `at` so that one starts there, as [`VmaList::split`] does,
    /// counting a new piece of a shared object.
    fn split(&mut self, at: usize) {
        if let Some(high) = self.vmas.split(at) {
            self.objects.piece(high, true);
        }
    }

    /// Takes `[start, end)` out of the VMAs and gives back what its pages hold. Room for
    /// the splits must have been made.
    fn remove(&mut self, start: usize, end: usize, ctx: &mut Ctx<'_>) {
        self.split(start);
        self.split(end);
        let range = self.vmas.overlapping(start, end);
        for index in range.clone() {
            if let Some(vma) = self.vmas.get(index).cloned() {
                self.drop_pages(&vma, ctx);
            }
        }
        for vma in self.vmas.drain(range) {
            // An object whose last mapping here this is stops counting here.
            self.objects.piece(&vma, false);
            if let Backing::Shared { object, .. } = vma.backing {
                // The last reference: the object goes with its last mapping, whichever
                // space that's in.
                if let Some(last) = object.release() {
                    last.release_all(ctx.frames, ctx.promises);
                }
            }
        }
    }

    /// Clears a VMA's entries and gives back what its own pages held.
    fn drop_pages(&mut self, vma: &Vma, ctx: &mut Ctx<'_>) {
        let zero = self.zero;
        let mut own = 0;
        let frames = &mut *ctx.frames;
        let flush = &mut *ctx.flush;
        let promises = ctx.promises;
        let backing = &vma.backing;
        self.space.update(vma.start, vma.end, &mut |_, _| Entry::Empty, &mut |page, old, _| {
            let Some((frame, present, cached)) = entry_frame(old) else {
                return;
            };
            if present {
                flush(page);
            }
            match backing {
                Backing::Lent => {}
                Backing::Private if frame == zero => {}
                Backing::Private => {
                    own += 1;
                    put_own(frames, promises, frame);
                }
                Backing::File { .. } if !cached => {
                    own += 1;
                    put_own(frames, promises, frame);
                }
                Backing::Shared { .. } | Backing::File { .. } => frames.put(frame),
            }
        });
        match &vma.backing {
            Backing::Lent => self.owned -= vma.pages(),
            Backing::Private | Backing::File { .. } => {
                if vma.charged {
                    let held = vma.pages() - own;
                    self.unbacked -= held;
                    self.charged -= vma.pages();
                    ctx.promises.release(held);
                } else {
                    self.owned -= own;
                }
            }
            Backing::Shared { .. } => {}
        }
        if let Backing::File { file, .. } = &vma.backing {
            if vma.prot.exec {
                file.unmap_exec(vma.pages());
            }
            // Its entries are gone: nothing here writes the file any more.
            if vma.shared_file_writable() {
                file.unmap_shared_write(vma.pages());
            }
            file.remove_mapper(self.layout.space, vma.pages());
        }
    }

    /// Calls `each` with every frame mapped in `[start, start + length)`, accessible or
    /// not: for the kernel to sync code it's about to make executable.
    pub fn each_frame(&self, start: usize, length: usize, each: &mut dyn FnMut(usize)) {
        let end = start.saturating_add(length);
        self.space.visit(start, end, &mut |_, entry| {
            if let Some((frame, ..)) = entry_frame(entry) {
                each(frame);
            }
        });
    }

    /// Frames of its own a private range has: not the zero frame, nor the cache's.
    fn own_frames(&self, start: usize, end: usize) -> usize {
        let mut own = 0;
        self.space.visit(start, end, &mut |_, entry| {
            if entry_frame(entry).is_some_and(|(frame, _, cached)| frame != self.zero && !cached) {
                own += 1;
            }
        });
        own
    }

    /// Whether `[start, end)` may be made executable: file pages straight from the cache,
    /// none of them copied; or, for a program image the kernel copied in, never written.
    fn may_run(&self, start: usize, end: usize) -> bool {
        self.vmas.in_range(start, end).iter().all(|vma| match &vma.backing {
            Backing::Private => vma.image && !vma.written,
            Backing::File { .. } => self.own_frames(vma.start.max(start), vma.end.min(end)) == 0,
            Backing::Shared { .. } | Backing::Lent => false,
        })
    }

    /// Changes the protection of `[start, start + length)`, every page of which must be
    /// mapped. A range made writable is charged then, unless it was made with
    /// `MAP_NORESERVE`. Executable is only for file pages that were never written here,
    /// and for the program's own image, never written.
    pub fn protect(&mut self, start: usize, length: usize, prot: Prot, ctx: &mut Ctx<'_>) -> Result<(), MmError> {
        let end = start.checked_add(length).ok_or(MmError::Invalid)?;
        if !self.vmas.covers(start, end) {
            return Err(MmError::NoMemory);
        }
        if self.touches_lent(start, length) {
            return Err(MmError::Invalid);
        }
        let range = self.vmas.overlapping(start, end);
        if prot.exec && (prot.write || !self.may_run(start, end)) {
            return Err(MmError::Denied);
        }
        // A shared file mapping writes the file: it has to have been open for writing.
        let read_only_file = |vma: &Vma| matches!(vma.backing, Backing::File { shared: true, may_write: false, .. });
        if prot.write && self.vmas.in_range(start, end).iter().any(read_only_file) {
            return Err(MmError::Denied);
        }
        self.vmas.reserve_for(start, end, 0, false).map_err(|_| MmError::NoMemory)?;
        // Pieces of files newly made executable count in their lock-table entries, which
        // refuses a file open for writing: before anything else changes.
        if prot.exec {
            let mut done = 0;
            let mut refused = false;
            for vma in self.vmas.in_range(start, end) {
                if let (Backing::File { file, .. }, false) = (&vma.backing, vma.prot.exec) {
                    let pages = (vma.end.min(end) - vma.start.max(start)) / PAGE_SIZE;
                    if file.map_exec(pages).is_err() {
                        refused = true;
                        break;
                    }
                }
                done += 1;
            }
            if refused {
                for vma in self.vmas.in_range(start, end).iter().take(done) {
                    if let (Backing::File { file, .. }, false) = (&vma.backing, vma.prot.exec) {
                        file.unmap_exec((vma.end.min(end) - vma.start.max(start)) / PAGE_SIZE);
                    }
                }
                return Err(MmError::TextBusy);
            }
        }
        // Shared file pieces newly made writable count as writable opens, which a file
        // mapped to run refuses. Never with the above: nothing is writable and executable.
        let newly_shared_write = |vma: &Vma| vma.shared_file() && !vma.prot.write && prot.write;
        if prot.write {
            let mut done = 0;
            let mut refused = false;
            for vma in self.vmas.in_range(start, end) {
                if let (Backing::File { file, .. }, true) = (&vma.backing, newly_shared_write(vma)) {
                    if file.map_shared_write((vma.end.min(end) - vma.start.max(start)) / PAGE_SIZE).is_err() {
                        refused = true;
                        break;
                    }
                }
                done += 1;
            }
            if refused {
                for vma in self.vmas.in_range(start, end).iter().take(done) {
                    if let (Backing::File { file, .. }, true) = (&vma.backing, newly_shared_write(vma)) {
                        file.unmap_shared_write((vma.end.min(end) - vma.start.max(start)) / PAGE_SIZE);
                    }
                }
                return Err(MmError::TextBusy);
            }
        }
        let undo_exec = |vmas: &VmaList| {
            for vma in vmas.in_range(start, end) {
                let pages = (vma.end.min(end) - vma.start.max(start)) / PAGE_SIZE;
                match &vma.backing {
                    Backing::File { file, .. } if prot.exec && !vma.prot.exec => file.unmap_exec(pages),
                    Backing::File { file, .. } if newly_shared_write(vma) => file.unmap_shared_write(pages),
                    _ => {}
                }
            }
        };

        // What's newly charged: pages, the promise for those without frames, and the
        // owned frames that a promise will cover from now on.
        // `newly` is whether any VMA becomes charged, which promises its tables even
        // when it adds no pages: a piece of an object another piece already charged.
        let (mut pages, mut promise, mut covered, mut newly) = (0, 0, 0, false);
        // Uncharged objects in the range, each counted once however many pieces it's in;
        // sorted rather than searched, so a range of many pieces stays cheap.
        let mut objects: Vec<*const SharedAnon> = Vec::new();
        if prot.write {
            for vma in self.vmas.iter().skip(range.start).take(range.len()) {
                // Shared file pages are the cache's, which can always write them back.
                if vma.charged || vma.noreserve || vma.shared_file() || matches!(vma.backing, Backing::Lent) {
                    continue;
                }
                newly = true;
                match &vma.backing {
                    Backing::Private | Backing::File { .. } => {
                        let (low, high) = (vma.start.max(start), vma.end.min(end));
                        let own = self.own_frames(low, high);
                        pages += (high - low) / PAGE_SIZE;
                        covered += own;
                        promise += (high - low) / PAGE_SIZE - own;
                    }
                    Backing::Shared { object, .. } => {
                        if !object.charged() {
                            if objects.try_reserve(1).is_err() {
                                undo_exec(&self.vmas);
                                return Err(MmError::NoMemory);
                            }
                            objects.push(ObjectRef::as_ptr(object));
                        }
                    }
                    Backing::Lent => {}
                }
            }
        }
        objects.sort_unstable();
        objects.dedup();
        for &object in &objects {
            // SAFETY: each is an object a VMA of this space holds a reference to, and
            // nothing can drop a VMA while `self` is borrowed here.
            let object = unsafe { &*object };
            pages += object.pages();
            // What this space's touches made of it, which it counts already.
            covered += self.objects.find(object as *const SharedAnon as usize).map_or(0, |counted| counted.owned);
        }
        // Each object charges itself, its promise taken under its own lock (another space
        // sharing it may be doing the same); undone if anything after fails.
        let mut charged_here = 0;
        let undo_objects = |objects: &[*const SharedAnon], count: usize, promises: &Promises| {
            for &object in objects.iter().take(count) {
                // SAFETY: as above.
                unsafe { &*object }.uncharge(promises);
            }
        };
        if newly {
            let tables = self.space.tables_wanted(start, end);
            if !self.fits((pages - covered) + tables) || !ctx.promises.promise(promise, ctx.frames.free_count()) {
                undo_exec(&self.vmas);
                return Err(MmError::NoMemory);
            }
            let mut refused = false;
            for index in 0..objects.len() {
                let object = objects[index];
                // SAFETY: as above.
                match unsafe { &*object }.charge(ctx.promises, ctx.frames.free_count()) {
                    Ok(true) => {
                        // Those charged here come first, for the undo.
                        objects.swap(charged_here, index);
                        charged_here += 1;
                    }
                    Ok(false) => {}
                    Err(()) => {
                        refused = true;
                        break;
                    }
                }
            }
            if refused || !self.make_tables(start, end, ctx) {
                undo_objects(&objects, charged_here, ctx.promises);
                ctx.promises.release(promise);
                undo_exec(&self.vmas);
                return Err(MmError::NoMemory);
            }
        }
        drop(objects);

        // Nothing fails from here. The entries change first, so that a shared file piece
        // made read-only has no writable entry left when it stops counting as a writer.
        let zero = self.zero;
        let flush = &mut *ctx.flush;
        self.space.update(
            start,
            end,
            &mut |_, old| match entry_frame(old) {
                Some((frame, _, cached)) => page_entry(frame, prot, zero, cached, entry_cow(old)),
                None => old,
            },
            &mut |page, old, _| {
                if matches!(old, Entry::Present { .. }) {
                    flush(page);
                }
            },
        );
        self.split(start);
        self.split(end);
        let range = self.vmas.overlapping(start, end);
        for index in range.clone() {
            let (piece_start, piece_end) = match self.vmas.get(index) {
                Some(vma) => (vma.start, vma.end),
                None => continue,
            };
            let own = self.own_frames(piece_start, piece_end);
            let Some(vma) = self.vmas.get_mut(index) else {
                continue;
            };
            if vma.prot.exec && !prot.exec {
                if let Backing::File { file, .. } = &vma.backing {
                    file.unmap_exec(vma.pages());
                }
            }
            if vma.shared_file_writable() && !prot.write {
                if let Backing::File { file, .. } = &vma.backing {
                    file.unmap_shared_write(vma.pages());
                }
            }
            if prot.write && !vma.charged && !vma.noreserve && !vma.shared_file() {
                match &vma.backing {
                    Backing::Private | Backing::File { .. } => {
                        let pages = vma.pages();
                        self.charged += pages;
                        self.unbacked += pages - own;
                        self.owned -= own;
                    }
                    Backing::Shared { object, .. } => {
                        // Charged now, by this call or another space's: here it counts its
                        // pages from now on, in place of the frames this space made.
                        let address = ObjectRef::as_ptr(object) as usize;
                        if let Some(counted) = self.objects.find(address) {
                            if counted.charged == 0 {
                                counted.charged = object.pages();
                                let owned = core::mem::take(&mut counted.owned);
                                self.objects.charged += object.pages();
                                self.objects.owned -= owned;
                            }
                        }
                    }
                    Backing::Lent => {}
                }
                vma.charged = true;
            }
            vma.prot = prot;
            vma.written |= prot.write;
        }
        let objects = &mut self.objects;
        self.vmas.join_around(range.start, range.end, &mut |joined| objects.piece(joined, false));
        Ok(())
    }

    /// `MADV_DONTNEED`: private pages in `[start, start + length)` lose their frames and
    /// read as zeroes, or as the file, next time, keeping their promise; shared pages only
    /// leave this mapping. Every page must be mapped. A program image the kernel copied
    /// in keeps its pages: there's no file to read them again from.
    pub fn discard(&mut self, start: usize, length: usize, ctx: &mut Ctx<'_>) -> Result<(), MmError> {
        let end = start.checked_add(length).ok_or(MmError::Invalid)?;
        if !self.vmas.covers(start, end) {
            return Err(MmError::NoMemory);
        }
        let range = self.vmas.overlapping(start, end);
        for index in range {
            let Some(vma) = self.vmas.get(index).cloned() else {
                continue;
            };
            let (low, high) = (vma.start.max(start), vma.end.min(end));
            if !(matches!(vma.backing, Backing::Lent) || matches!(vma.backing, Backing::Private) && vma.image) {
                self.clear(&vma, low, high, ctx);
            }
        }
        Ok(())
    }

    /// Clears the entries of `[low, high)`, in `vma`, and gives back what they held: a
    /// page of the space's own keeps its promise, if it had one, for the next touch.
    fn clear(&mut self, vma: &Vma, low: usize, high: usize, ctx: &mut Ctx<'_>) {
        let zero = self.zero;
        let promises = ctx.promises;
        let frames = &mut *ctx.frames;
        let flush = &mut *ctx.flush;
        let mut own = 0;
        let counted = matches!(vma.backing, Backing::Private | Backing::File { .. });
        self.space.update(low, high, &mut |_, _| Entry::Empty, &mut |page, old, _| {
            let Some((frame, present, cached)) = entry_frame(old) else {
                return;
            };
            if present {
                flush(page);
            }
            if frame == zero {
                return;
            }
            if counted && !cached {
                own += 1;
                // The page is owed its frame again: the promise is back before the
                // frame is.
                if vma.charged {
                    promises.hold(1);
                }
                put_own(frames, promises, frame);
                return;
            }
            frames.put(frame);
        });
        if counted {
            if vma.charged {
                self.unbacked += own;
            } else {
                self.owned -= own;
            }
        }
    }

    /// The file `key` has been cut so that its pages from `first` on are gone: they leave
    /// every mapping of them here, copies included, as Linux's truncation does, and fault
    /// with `SIGBUS` from now on.
    pub fn zap_file(&mut self, key: FileKey, first: u64, ctx: &mut Ctx<'_>) {
        let mut index = 0;
        while let Some(vma) = self.vmas.get(index).cloned() {
            index += 1;
            let Backing::File { file, first: vma_first, .. } = &vma.backing else {
                continue;
            };
            if file.key() != key {
                continue;
            }
            let last = vma_first + vma.pages() as u64;
            if last <= first {
                continue;
            }
            let skip = first.saturating_sub(*vma_first) as usize;
            let low = vma.start + skip * PAGE_SIZE;
            self.clear(&vma, low, vma.end, ctx);
        }
    }

    /// A copy of this space for a forked child, which file reverse maps know as `space`
    /// (see the module's Fork). Under this space's lock: `ctx.flush` forgets the entries
    /// made read-only here, which this space's other threads may have cached, and
    /// `ctx.frames` is where the child's frames and tables come from too.
    ///
    /// `ENOMEM` if the promises can't all be taken, or there's no frame for a copy or a
    /// table, no heap for the lists, or a file's counts are full. Then nothing of the
    /// child is left, and this space is as it was but for entries marked copy-on-write,
    /// whose next write takes their frame back in place.
    pub fn fork(&mut self, space: u32, ctx: &mut Ctx<'_>) -> Result<Mm, MmError> {
        let layout = Layout { space, ..self.layout };
        let mut child = Mm::new(ctx.frames, self.space.bits, layout, self.zero).ok_or(MmError::NoMemory)?;
        if child.vmas.reserve(self.vmas.len()).is_err() || child.objects.list.try_reserve(self.objects.list.len()).is_err() {
            child.destroy_with(ctx);
            return Err(MmError::NoMemory);
        }
        // One promise for each page the child is charged for, and one for each frame of
        // this space's own the child is about to share, or have a copy of.
        let zero = self.zero;
        let mut units = 0;
        for vma in self.vmas.iter() {
            if matches!(vma.backing, Backing::Private | Backing::File { shared: false, .. }) {
                units += if vma.charged { vma.pages() } else { self.own_frames(vma.start, vma.end) };
            }
        }
        if !ctx.promises.promise(units, ctx.frames.free_count()) {
            child.destroy_with(ctx);
            return Err(MmError::NoMemory);
        }
        // What's promised and not yet handed to the child: given back if it fails.
        let mut left = units;
        let failed = |child: Mm, left: usize, ctx: &mut Ctx<'_>| {
            ctx.promises.release(left);
            child.destroy_with(ctx);
            Err(MmError::NoMemory)
        };
        for index in 0..self.vmas.len() {
            let Some(vma) = self.vmas.get(index).cloned() else {
                continue;
            };
            // The window surface isn't inherited: the child's is unmapped.
            if matches!(vma.backing, Backing::Lent) {
                continue;
            }
            let pages = vma.pages();
            // A file's counts, as the mapping took them, for the child's copy of it.
            if let Backing::File { file, .. } = &vma.backing {
                let exec = vma.prot.exec;
                let shared_write = vma.shared_file_writable();
                if exec && file.map_exec(pages).is_err() {
                    return failed(child, left, ctx);
                }
                if shared_write && file.map_shared_write(pages).is_err() {
                    if exec {
                        file.unmap_exec(pages);
                    }
                    return failed(child, left, ctx);
                }
                if !file.add_mapper(space, pages) {
                    if exec {
                        file.unmap_exec(pages);
                    }
                    if shared_write {
                        file.unmap_shared_write(pages);
                    }
                    return failed(child, left, ctx);
                }
            }
            // In the child from here, so a failure gives back what its pages have.
            if child.vmas.reserve_for(vma.start, vma.end, 1, true).is_err() {
                return failed(child, left, ctx);
            }
            match &vma.backing {
                Backing::Shared { object, .. } => child.objects.attribute(object, if object.charged() { object.pages() } else { 0 }),
                _ if vma.charged => {
                    child.charged += pages;
                    child.unbacked += pages;
                    left -= pages;
                }
                _ => {}
            }
            child.objects.piece(&vma, true);
            let objects = &mut child.objects;
            child.vmas.insert(vma.clone(), &mut |joined| objects.piece(joined, false));
            // A charged range's tables are made with it; an uncharged one's where this
            // space has them, for the entries about to be copied into them.
            let made = if vma.charged {
                child.make_tables(vma.start, vma.end, ctx)
            } else {
                let mut at = vma.start;
                let mut made = true;
                while made && at < vma.end {
                    if self.space.leaf_slot(at).is_some() {
                        made = child.tables_for(at, ctx);
                    }
                    at = (at & !(LEAF_SPAN - 1)) + LEAF_SPAN;
                }
                made
            };
            if !made {
                return failed(child, left, ctx);
            }

            // The entries: this space's own frames shared copy-on-write, or copied when a
            // copy of the kernel's is in flight through them while they're writable; every
            // other frame shared as it is.
            let mut refused = false;
            let mut own = 0;
            let frames = &mut *ctx.frames;
            let flush = &mut *ctx.flush;
            let promises = ctx.promises;
            let into = &mut child.space;
            let private = matches!(vma.backing, Backing::Private | Backing::File { shared: false, .. });
            let shared_file = vma.shared_file();
            let prot = vma.prot;
            self.space.update(
                vma.start,
                vma.end,
                &mut |page, old| {
                    let Some((frame, _, cached)) = entry_frame(old) else {
                        return old;
                    };
                    if refused || into.leaf_slot(page).is_none() {
                        refused = true;
                        return old;
                    }
                    let (kept, given) = if private && frame != zero && !cached {
                        let pinned_writable = matches!(old, Entry::Present { writable: true, .. }) && frames.pins(frame) > 0;
                        if pinned_writable {
                            let Some(copy) = frames.alloc() else {
                                refused = true;
                                return old;
                            };
                            // SAFETY: both are frames a page long: one just given, the
                            // other one this space's entry holds.
                            unsafe { core::ptr::copy_nonoverlapping(frame as *const u8, copy as *mut u8, PAGE_SIZE) };
                            promises.release(1);
                            (old, page_entry(copy, prot, zero, false, false))
                        } else {
                            if !frames.get(frame) {
                                refused = true;
                                return old;
                            }
                            (shared_entry(old), shared_entry(old))
                        }
                    } else if frame == zero {
                        (old, old)
                    } else {
                        if !frames.get(frame) {
                            refused = true;
                            return old;
                        }
                        // A shared file's page comes read-only, so the child's first
                        // write marks it dirty again for write-back.
                        let given = if shared_file { page_entry(frame, prot, zero, true, false) } else { old };
                        (old, given)
                    };
                    if private && frame != zero && !cached {
                        own += 1;
                    }
                    into.set(page, given);
                    kept
                },
                &mut |page, old, new| {
                    if matches!(old, Entry::Present { writable: true, .. }) && !matches!(new, Entry::Present { writable: true, .. }) {
                        flush(page);
                    }
                },
            );
            // Each frame given promised its page: from the range's charge, or from what's
            // left.
            if vma.charged {
                child.unbacked -= own;
            } else if private {
                child.owned += own;
                left -= own;
            }
            if refused {
                return failed(child, left, ctx);
            }
        }
        debug_assert_eq!(left, 0, "a fork promised more than it handed over");
        ctx.promises.release(left);
        Ok(child)
    }

    /// Gives everything back: every page, object, promise and table. No CPU has these
    /// tables loaded any more, so nothing needs flushing.
    pub fn destroy(self, frames: &mut dyn FrameSource, promises: &Promises) {
        let mut flush = |_: usize| {};
        let mut files = NoFiles;
        let mut ctx = Ctx { frames, promises, flush: &mut flush, files: &mut files };
        self.destroy_with(&mut ctx);
    }

    /// [`destroy`](Self::destroy), with what a change has: for a fork that failed part
    /// way, whose child nothing has loaded.
    fn destroy_with(mut self, ctx: &mut Ctx<'_>) {
        for index in 0..self.vmas.len() {
            if let Some(vma) = self.vmas.get(index).cloned() {
                self.drop_pages(&vma, ctx);
            }
        }
        let all = 0..self.vmas.len();
        for vma in self.vmas.drain(all) {
            self.objects.piece(&vma, false);
            if let Backing::Shared { object, .. } = vma.backing {
                if let Some(last) = object.release() {
                    last.release_all(ctx.frames, ctx.promises);
                }
            }
        }
        debug_assert_eq!(
            (self.charged, self.owned, self.unbacked, self.held, self.objects.heap, self.objects.charged, self.objects.owned),
            (0, 0, 0, 0, 0, 0, 0)
        );
        // Anything left would be a bug above; the machine keeps its promises either way.
        ctx.promises.release(self.unbacked);
        self.space.destroy(ctx.frames);
    }
}
