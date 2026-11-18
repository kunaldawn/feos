//! [`Mm`]: a process's address space, VMAs and page tables together, and the rules
//! that keep them and the machine's promises in step.
//!
//! What a process is charged for, in pages, is three counts: `charged` (pages promised
//! to it, with or without their frames yet), `owned` (frames it has that no promise
//! covers: a read-only image page, a `MAP_NORESERVE` page that was written) and its page
//! tables. Their sum is what `USER_MEMORY_LIMIT` limits. `unbacked` is the part of
//! `charged` without frames, which is exactly what this space holds in [`Promises`].
//!
//! A private charged range's share of `unbacked` is its pages less the frames of its
//! own it has; a shared object's is its pages less its frames, counted once, by the
//! space that made it. Every change below keeps both in step.

use alloc::vec::Vec;

use crate::shared::{ObjectRef, SharedAnon};
use crate::vma::{Backing, Prot, Vma, VmaList};
use crate::{AddressSpace, Entry, FileKey, FileRef, FrameSource, PageBits, Promises, PAGE_SIZE};

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
    /// Heap the shared objects this space made hold, by [`SharedAnon::heap_cost`].
    object_heap: usize,
}

/// The entry a page of `frame` gets under `prot`. The zero frame and the page cache's
/// frames (`cached`) are never writable through it.
fn page_entry(frame: usize, prot: Prot, zero: usize, cached: bool) -> Entry {
    if prot.is_none() {
        return Entry::Hidden { frame, cached };
    }
    Entry::Present {
        frame,
        writable: prot.write && frame != zero && !cached,
        executable: prot.exec,
        cached,
    }
}

/// The entry of a writable shared mapping's page of the cache once it's dirty: the one
/// entry of the cache's frames that's writable.
const fn dirty_entry(frame: usize, prot: Prot) -> Entry {
    Entry::Present { frame, writable: true, executable: prot.exec, cached: true }
}

const fn allowed(prot: Prot, access: Access) -> bool {
    !prot.is_none() && (!access.write || prot.write) && (!access.execute || prot.exec)
}

/// An entry's frame, whether the hardware may have it, and whether it's the cache's.
const fn entry_frame(entry: Entry) -> Option<(usize, bool, bool)> {
    match entry {
        Entry::Present { frame, cached, .. } => Some((frame, true, cached)),
        Entry::Hidden { frame, cached } => Some((frame, false, cached)),
        Entry::Empty => None,
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
            object_heap: 0,
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

    /// Pages promised to this space.
    pub const fn charged(&self) -> usize {
        self.charged
    }

    /// Frames this space has that no promise covers.
    pub const fn owned(&self) -> usize {
        self.owned
    }

    /// Of the pages promised, those still without frames: what this space holds in
    /// [`Promises`].
    pub const fn unbacked(&self) -> usize {
        self.unbacked
    }

    /// Page tables below the root.
    pub const fn tables(&self) -> usize {
        self.space.tables()
    }

    /// Heap the shared objects this space made hold, besides their leaves.
    pub const fn object_heap(&self) -> usize {
        self.object_heap
    }

    /// What counts against the limit: pages charged or owned, and tables.
    pub const fn committed(&self) -> usize {
        self.charged + self.owned + self.space.tables()
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
        self.space.set(page, page_entry(frame, prot, self.zero, false));
        // A page of the cache's that the copy replaces lets go of it.
        if let Some((old_frame, _, true)) = entry_frame(old) {
            ctx.frames.put(old_frame);
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
                let mut new = page_entry(frame, prot, self.zero, cached);
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
                self.space.set(page, page_entry(self.zero, prot, self.zero, false));
                Ok(())
            }
            (Entry::Empty, Source::Object(object, index)) => {
                if !self.tables_for(page, ctx) {
                    return Err(FaultError::Bus);
                }
                let object_charged = object.charged();
                let fits = self.fits(1);
                let (owned, unbacked) = (&mut self.owned, &mut self.unbacked);
                let mut make = || Self::alloc_page(object_charged, (&mut *owned, &mut *unbacked), fits, ctx);
                let (frame, _) = object.frame_or_make(index, &mut make).map_err(|_| FaultError::Bus)?;
                if !ctx.frames.get(frame) {
                    return Err(FaultError::Bus);
                }
                self.space.set(page, page_entry(frame, prot, self.zero, false));
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
                self.space.set(page, page_entry(frame, prot, self.zero, true));
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
            let room = self.object_heap.checked_add(heap).is_some_and(|total| total <= self.layout.object_heap_limit);
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
        if charged {
            self.charged += pages;
            self.unbacked += pages;
        }
        self.object_heap += heap;
        self.vmas.insert(Vma {
            start,
            end,
            prot: request.prot,
            backing,
            charged,
            noreserve: request.noreserve,
            image: request.image,
            written: request.prot.write,
        });
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
            let entry = Entry::Present { frame: first_frame + (page - start), writable: true, executable: false, cached: false };
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
        });
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

    /// Takes `[start, end)` out of the VMAs and gives back what its pages hold. Room for
    /// the splits must have been made.
    fn remove(&mut self, start: usize, end: usize, ctx: &mut Ctx<'_>) {
        self.vmas.split(start);
        self.vmas.split(end);
        let range = self.vmas.overlapping(start, end);
        for index in range.clone() {
            if let Some(vma) = self.vmas.get(index).cloned() {
                self.drop_pages(&vma, ctx);
            }
        }
        let counts = (&mut self.charged, &mut self.owned, &mut self.unbacked, &mut self.object_heap);
        for vma in self.vmas.drain(range) {
            if let Backing::Shared { object, .. } = vma.backing {
                // The last reference: the object goes with its last mapping.
                if ObjectRef::count(&object) == 1 {
                    Self::object_gone(&object, (&mut *counts.0, &mut *counts.1, &mut *counts.2, &mut *counts.3), ctx);
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
                    frames.put(frame);
                }
                Backing::File { .. } if !cached => {
                    own += 1;
                    frames.put(frame);
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

    /// A shared object's last mapping has gone: its frames and its promise go too.
    fn object_gone(object: &SharedAnon, counts: (&mut usize, &mut usize, &mut usize, &mut usize), ctx: &mut Ctx<'_>) {
        let (charged, owned, unbacked, heap) = counts;
        *heap -= SharedAnon::heap_cost(object.pages());
        let was_charged = object.charged();
        let frames = object.release_all(ctx.frames);
        if was_charged {
            let held = object.pages() - frames;
            *unbacked -= held;
            *charged -= object.pages();
            ctx.promises.release(held);
        } else {
            *owned -= frames;
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
            let frames = object.frames();
            pages += object.pages();
            covered += frames;
            promise += object.pages() - frames;
        }
        drop(objects);
        if newly {
            let tables = self.space.tables_wanted(start, end);
            if !self.fits((pages - covered) + tables) || !ctx.promises.promise(promise, ctx.frames.free_count()) {
                undo_exec(&self.vmas);
                return Err(MmError::NoMemory);
            }
            if !self.make_tables(start, end, ctx) {
                ctx.promises.release(promise);
                undo_exec(&self.vmas);
                return Err(MmError::NoMemory);
            }
        }

        // Nothing fails from here. The entries change first, so that a shared file piece
        // made read-only has no writable entry left when it stops counting as a writer.
        let zero = self.zero;
        let flush = &mut *ctx.flush;
        self.space.update(
            start,
            end,
            &mut |_, old| match entry_frame(old) {
                Some((frame, _, cached)) => page_entry(frame, prot, zero, cached),
                None => old,
            },
            &mut |page, old, _| {
                if matches!(old, Entry::Present { .. }) {
                    flush(page);
                }
            },
        );
        self.vmas.split(start);
        self.vmas.split(end);
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
                        if !object.charged() {
                            object.set_charged();
                            let frames = object.frames();
                            self.charged += object.pages();
                            self.unbacked += object.pages() - frames;
                            self.owned -= frames;
                        }
                    }
                    Backing::Lent => {}
                }
                vma.charged = true;
            }
            vma.prot = prot;
            vma.written |= prot.write;
        }
        self.vmas.join_around(range.start, range.end);
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

    /// Gives everything back: every page, object, promise and table. No CPU has these
    /// tables loaded any more, so nothing needs flushing.
    pub fn destroy(mut self, frames: &mut dyn FrameSource, promises: &Promises) {
        let mut flush = |_: usize| {};
        let mut files = NoFiles;
        let mut ctx = Ctx { frames, promises, flush: &mut flush, files: &mut files };
        for index in 0..self.vmas.len() {
            if let Some(vma) = self.vmas.get(index).cloned() {
                self.drop_pages(&vma, &mut ctx);
            }
        }
        let counts = (&mut self.charged, &mut self.owned, &mut self.unbacked, &mut self.object_heap);
        let all = 0..self.vmas.len();
        for vma in self.vmas.drain(all) {
            if let Backing::Shared { object, .. } = vma.backing {
                if ObjectRef::count(&object) == 1 {
                    Self::object_gone(&object, (&mut *counts.0, &mut *counts.1, &mut *counts.2, &mut *counts.3), &mut ctx);
                }
            }
        }
        debug_assert_eq!((self.charged, self.owned, self.unbacked, self.object_heap), (0, 0, 0, 0));
        // Anything left would be a bug above; the machine keeps its promises either way.
        ctx.promises.release(self.unbacked);
        self.space.destroy(ctx.frames);
    }
}
