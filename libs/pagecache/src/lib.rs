//! The page cache: the frames that hold files' pages, shared by every mapping of a file.
//!
//! A file mapping reads its pages from here, so two programs running one binary run the
//! same frames, and a page read once is there for the next reader. A page is keyed by
//! its file's [identity](FileKey), which outlives renames, and its index in the file.
//! The cache holds one reference on each of its frames; every page-table entry pointing
//! at one holds another (see `feos-frames`), so a page that's mapped can't go, and one
//! that isn't can be taken back whenever its frame is wanted.
//!
//! # What's kept per file
//!
//! A [`CacheFile`], reached by a counted [`FileRef`]: the index holds one, every VMA
//! mapping the file another, and a program's file open for writing a third. Beside the
//! pages it's the file's entry in the kernel's lock table: how many opens may write it,
//! how many pages are mapped to run, and which address spaces map it (the reverse map).
//! The check and the change of those counts are one critical section, the file's lock,
//! which is how `ETXTBSY` can't be raced past.
//!
//! # Coherence
//!
//! The filesystem tells the cache of every change to a file's bytes before and after it
//! happens (`feos_fs::vfs::FileHook`, which the kernel forwards here): a write updates the
//! cached pages in place, so every mapping sees the file as it is; a truncation drops
//! the pages past the new end and zeroes the tail of the last; a removal drops them all.
//! A fill reads the file with no lock of the cache held, so the cache's locks are always
//! taken inside the filesystem's and never the other way round. Each change bumps the
//! file's change count, and a fill that started before a change is thrown away rather
//! than put in: it may have read what the change replaced.
//!
//! # Dirty pages
//!
//! A shared writable mapping writes the cache's frame itself. Its pages are mapped
//! read-only until the first write fault, which marks the page dirty
//! ([`mark_dirty`](PageCache::mark_dirty)) before the entry is made writable. A dirty
//! page is written back by [`write_back`](PageCache::write_back), whose writer takes the
//! page's bytes and writes them as one step under the filesystem's lock, so no write
//! through the filesystem can land between the two. Once an entry is writable, writes
//! through it don't fault, so the cache can't tell whether a page changed after it was
//! written back. So a page is clean after a write-back only if nothing could have written
//! it since its bytes were taken: no entry had its frame then (a write from then on
//! faults, and the fault marks it again), or no writable shared mapping of its file
//! existed then, after or in between ([`map_shared_write`](FileRef::map_shared_write)).
//! A page some writable mapping still has stays dirty, and is written again at each
//! write-back. Reclaim never takes a dirty page, and
//! [`reclaimable`](PageCache::reclaimable) doesn't count them; a change through the
//! filesystem that fails part way keeps them, as it keeps every page an entry has.
//!
//! # Memory
//!
//! A page's frame is taken only from frames free beyond what the machine has promised
//! (`reserved`), or by taking back a page nothing maps. Those are found by a clock over
//! the files, with a second chance for pages used since it last passed, which is an
//! approximation of least recently used that costs nothing on a hit but a bit. Pages
//! nothing maps are counted ([`idle`](PageCache::idle)), so the kernel can count them
//! as frames it could have.
//!
//! The cache's own heap is taken fallibly: a page's slot is in a small leaf, made on the
//! first touch of the 32 pages it covers, under a directory sized by the file.
//!
//! A frame's address must be usable as a pointer, as it is on an identity map and in a
//! host test: the cache writes into its pages when a file changes.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::alloc::{alloc, alloc_zeroed, dealloc, Layout};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr::NonNull;
use core::sync::atomic::{fence, AtomicBool, AtomicUsize, Ordering};

use feos_sync::{Mutex, MutexGuard, SpinLock};

/// Bytes in a page.
pub const PAGE_SIZE: usize = 4096;

/// Pages per leaf of a file's table.
const LEAF: usize = 32;

/// Most address spaces that may map one file at once.
pub const MAX_MAPPERS: usize = 1024;

/// A file's identity: its mount's number and its inode number.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FileKey {
    pub dev: u64,
    pub ino: u64,
}

/// What the cache needs of the frame allocator.
pub trait CacheFrames {
    /// A frame for a new page, with one reference (the cache's) and marked as the
    /// cache's. It needn't be cleared.
    fn alloc(&mut self) -> Option<usize>;
    /// Another reference to one of the cache's frames, for a page-table entry: the count
    /// after, or `None` at the cap.
    fn get(&mut self, frame: usize) -> Option<u32>;
    /// The cache lets a frame go: it's no longer marked the cache's, and the cache's
    /// reference is dropped. Says whether that was the only one, so the frame is freed.
    fn drop_page(&mut self, frame: usize) -> bool;
    /// References to a frame now.
    fn refs(&self, frame: usize) -> u32;
    /// Frames free now.
    fn free_count(&self) -> usize;
}

/// Writes dirty pages back to their files, for [`PageCache::write_back`].
pub trait PageWriter {
    /// Writes a page's bytes at `offset` of the file `key`. `fill` puts them in the
    /// buffer it's given, a page long, and says how many there are (none: nothing to
    /// write). The writer calls `fill` and writes what it gave as one step that no other
    /// change to the file can come between, which is what lets the cache call the page
    /// clean after. `false` if the write failed.
    fn write(&mut self, key: FileKey, offset: u64, fill: &mut dyn FnMut(&mut [u8]) -> usize) -> bool;
}

/// A write-back failed for some page: `EIO`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteBackError;

/// Why a file couldn't be changed or mapped as asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockError {
    /// `ETXTBSY`: it's mapped to run and this would write it, or the other way round.
    TextBusy,
    /// No heap for the entry, or past [`MAX_MAPPERS`].
    NoMemory,
}

#[derive(Clone, Copy, Default, Debug)]
struct Slot {
    /// Zero for no page.
    frame: usize,
    /// The instruction cache agrees with the frame's contents.
    synced: bool,
    /// Used since the clock last passed.
    accessed: bool,
    /// Written through a mapping since it was last written back.
    dirty: bool,
    /// A write-back took its bytes, and no write fault has marked it since: what lets the
    /// write-back tell that nothing came between taking them and calling it clean.
    taken: bool,
}

type Leaf = [Slot; LEAF];

#[derive(Debug)]
struct FileState {
    leaves: Vec<Option<Box<Leaf>>>,
    /// Pages held.
    pages: usize,
    /// The file's size, as the filesystem last said.
    size: u64,
    /// Bumped by every change to the file's bytes.
    change: u64,
    /// Its last name is gone: no pages, and no more to come.
    dead: bool,
    /// Opens that may write it.
    writers: usize,
    /// Writes in progress that no open counts: the kernel's apps', through the tree.
    writing: usize,
    /// Pages of it mapped to run, across every space.
    exec_pages: usize,
    /// Pages of it in writable shared mappings, across every space. Each counts as a
    /// writable open for `ETXTBSY`.
    write_pages: usize,
    /// Bumped whenever `write_pages` grows, so a write-back can tell that a writable
    /// mapping came and went while it wrote.
    write_gen: u64,
    /// Pages held that are dirty.
    dirty: usize,
    /// Spaces mapping it, and how many pages each.
    mappers: Vec<(u32, usize)>,
}

impl FileState {
    fn slot(&mut self, index: u64) -> Option<&mut Slot> {
        let index = usize::try_from(index).ok()?;
        self.leaves.get_mut(index / LEAF)?.as_mut().map(|leaf| &mut leaf[index % LEAF])
    }

    /// A copy of the slot for `index`, if its leaf is there.
    fn peek(&self, index: u64) -> Option<Slot> {
        let index = usize::try_from(index).ok()?;
        self.leaves.get(index / LEAF)?.as_ref().map(|leaf| leaf[index % LEAF])
    }

    /// The slot for `index`, making its leaf and growing the directory if need be.
    fn slot_or_new(&mut self, index: u64) -> Option<&mut Slot> {
        let index = usize::try_from(index).ok()?;
        let leaf = index / LEAF;
        if leaf >= self.leaves.len() {
            self.leaves.try_reserve(leaf + 1 - self.leaves.len()).ok()?;
            self.leaves.resize_with(leaf + 1, || None);
        }
        let slot = &mut self.leaves[leaf];
        let leaf_box = match slot {
            Some(leaf) => leaf,
            None => slot.insert(new_leaf()?),
        };
        Some(&mut leaf_box[index % LEAF])
    }

    /// Every page from `first` on, as `(index, slot)`.
    fn each_from(&mut self, first: u64, each: &mut dyn FnMut(u64, &mut Slot)) {
        self.each_in(first, u64::MAX, each);
    }

    /// Every page in `[first, end)`, as `(index, slot)`: only the leaves the range covers
    /// are looked at.
    fn each_in(&mut self, first: u64, end: u64, each: &mut dyn FnMut(u64, &mut Slot)) {
        let first = usize::try_from(first).unwrap_or(usize::MAX);
        let end = usize::try_from(end).unwrap_or(usize::MAX);
        for (number, leaf) in self.leaves.iter_mut().enumerate().skip(first / LEAF) {
            if number.saturating_mul(LEAF) >= end {
                return;
            }
            let Some(leaf) = leaf else {
                continue;
            };
            for (offset, slot) in leaf.iter_mut().enumerate() {
                let index = number * LEAF + offset;
                if index >= first && index < end && slot.frame != 0 {
                    each(index as u64, slot);
                }
            }
        }
    }

    /// The first dirty page in `[first, end)`. It stops there: a write-back asks once per
    /// dirty page, under the file's lock, so a walk to the end each time would cost the
    /// square of the file's pages.
    fn first_dirty(&self, first: u64, end: u64) -> Option<u64> {
        let first = usize::try_from(first).ok()?;
        let end = usize::try_from(end).unwrap_or(usize::MAX);
        for (number, leaf) in self.leaves.iter().enumerate().skip(first / LEAF) {
            if number.saturating_mul(LEAF) >= end {
                return None;
            }
            let Some(leaf) = leaf else {
                continue;
            };
            for (offset, slot) in leaf.iter().enumerate() {
                let index = number * LEAF + offset;
                if index >= end {
                    return None;
                }
                if index >= first && slot.frame != 0 && slot.dirty {
                    return Some(index as u64);
                }
            }
        }
        None
    }

    fn is_unused(&self) -> bool {
        self.pages == 0
            && self.writers == 0
            && self.writing == 0
            && self.exec_pages == 0
            && self.write_pages == 0
            && self.mappers.is_empty()
    }

}

fn new_leaf() -> Option<Box<Leaf>> {
    let layout = Layout::new::<Leaf>();
    // SAFETY: the layout isn't zero sized.
    let raw = unsafe { alloc_zeroed(layout) }.cast::<Leaf>();
    // SAFETY: allocated with `Leaf`'s layout, and zeroes are a valid `Leaf`: no frame,
    // not synced, not accessed.
    (!raw.is_null()).then(|| unsafe { Box::from_raw(raw) })
}

/// One file's pages and lock-table counts.
pub struct CacheFile {
    key: FileKey,
    refs: AtomicUsize,
    state: SpinLock<FileState>,
    /// Held by a program's write through the filesystem for the whole of it, so an
    /// `O_APPEND` write's look at the end and the write there are one step to every
    /// other write (section 3.3 of the ABI design), as Linux's inode lock makes them.
    writes: Mutex<()>,
    /// The cache's `forget`, for the last reference to call once the file has died.
    forget: fn(FileKey),
    /// The file died while something outside the cache still held it: its hold on the
    /// file goes with the last reference, not before. A description or a mapping acts on
    /// the file by its identity, and an identity let go of is a number the filesystem
    /// may give the next file it makes.
    forget_at_last: AtomicBool,
}

/// A counted reference to a [`CacheFile`], as an `Arc` would be, but made fallibly: the
/// kernel makes one inside a change to an address space, where a failed allocation
/// couldn't be contained.
pub struct FileRef(NonNull<CacheFile>);

// SAFETY: the file is shared through its own lock, and the count is atomic.
unsafe impl Send for FileRef {}
// SAFETY: as above.
unsafe impl Sync for FileRef {}

impl FileRef {
    fn new(key: FileKey, size: u64, forget: fn(FileKey)) -> Option<Self> {
        let layout = Layout::new::<CacheFile>();
        // SAFETY: the layout isn't zero sized.
        let raw = unsafe { alloc(layout) }.cast::<CacheFile>();
        let pointer = NonNull::new(raw)?;
        let state = FileState {
            leaves: Vec::new(),
            pages: 0,
            size,
            change: 0,
            dead: false,
            writers: 0,
            writing: 0,
            exec_pages: 0,
            write_pages: 0,
            write_gen: 0,
            dirty: 0,
            mappers: Vec::new(),
        };
        // SAFETY: just allocated with `CacheFile`'s layout, and not yet initialised.
        unsafe {
            pointer.as_ptr().write(CacheFile {
                key,
                refs: AtomicUsize::new(1),
                state: SpinLock::new(state),
                writes: Mutex::new(()),
                forget,
                forget_at_last: AtomicBool::new(false),
            });
        }
        Some(Self(pointer))
    }

    fn file(&self) -> &CacheFile {
        // SAFETY: the allocation lives while any reference does, this one included.
        unsafe { self.0.as_ref() }
    }

    /// The file's identity.
    pub fn key(&self) -> FileKey {
        self.file().key
    }

    /// Whether two references are to the same file's entry.
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.0 == other.0
    }

    /// The entry's address: one per file while anything references it, so it names the
    /// file to whoever maps it, as a shared futex word's key does.
    pub fn as_ptr(this: &Self) -> *const CacheFile {
        this.0.as_ptr()
    }

    /// References to the entry, this one included.
    pub fn count(this: &Self) -> usize {
        this.file().refs.load(Ordering::Acquire)
    }

    /// The file's size, as last told.
    pub fn size(&self) -> u64 {
        self.file().state.lock().size
    }

    /// The file's change count, for [`PageCache::set_size_if`].
    pub fn change(&self) -> u64 {
        self.file().state.lock().change
    }

    /// Whether the file's last name has gone.
    pub fn is_dead(&self) -> bool {
        self.file().state.lock().dead
    }

    /// Pages held for it.
    pub fn pages(&self) -> usize {
        self.file().state.lock().pages
    }

    /// Takes the file's write lock (see [`CacheFile`]'s `writes`), which may wait for
    /// another write to finish.
    pub fn write_lock(&self) -> MutexGuard<'_, ()> {
        self.file().writes.lock()
    }

    /// An open that may write: `TextBusy` if any page of it is mapped to run.
    pub fn open_writer(&self) -> Result<(), LockError> {
        let mut state = self.file().state.lock();
        if state.exec_pages > 0 {
            return Err(LockError::TextBusy);
        }
        state.writers += 1;
        Ok(())
    }

    /// That open has closed.
    pub fn close_writer(&self) {
        let mut state = self.file().state.lock();
        state.writers = state.writers.saturating_sub(1);
    }

    /// Opens that may write it.
    pub fn writers(&self) -> usize {
        self.file().state.lock().writers
    }

    /// `pages` more pages mapped to run: `TextBusy` if anything may be writing it.
    pub fn map_exec(&self, pages: usize) -> Result<(), LockError> {
        let mut state = self.file().state.lock();
        if state.writers > 0 || state.writing > 0 || state.write_pages > 0 {
            return Err(LockError::TextBusy);
        }
        state.exec_pages += pages;
        Ok(())
    }

    /// `pages` pages no longer mapped to run.
    pub fn unmap_exec(&self, pages: usize) {
        let mut state = self.file().state.lock();
        debug_assert!(state.exec_pages >= pages, "more executable pages unmapped than mapped");
        state.exec_pages = state.exec_pages.saturating_sub(pages);
    }

    /// Pages mapped to run.
    pub fn exec_pages(&self) -> usize {
        self.file().state.lock().exec_pages
    }

    /// `pages` more pages in writable shared mappings, which count as a writable open:
    /// `TextBusy` if any of the file is mapped to run.
    pub fn map_shared_write(&self, pages: usize) -> Result<(), LockError> {
        let mut state = self.file().state.lock();
        if state.exec_pages > 0 {
            return Err(LockError::TextBusy);
        }
        state.write_pages += pages;
        state.write_gen += 1;
        Ok(())
    }

    /// `pages` pages no longer in writable shared mappings: their entries are already
    /// read-only or gone.
    pub fn unmap_shared_write(&self, pages: usize) {
        let mut state = self.file().state.lock();
        debug_assert!(state.write_pages >= pages, "more shared writable pages unmapped than mapped");
        state.write_pages = state.write_pages.saturating_sub(pages);
    }

    /// Pages in writable shared mappings.
    pub fn write_pages(&self) -> usize {
        self.file().state.lock().write_pages
    }

    /// Dirty pages held for it.
    pub fn dirty_pages(&self) -> usize {
        self.file().state.lock().dirty
    }

    /// Space `space` maps `pages` more pages of the file. `false` if the reverse map has
    /// no room for another space.
    pub fn add_mapper(&self, space: u32, pages: usize) -> bool {
        let mut state = self.file().state.lock();
        if let Some(entry) = state.mappers.iter_mut().find(|entry| entry.0 == space) {
            entry.1 += pages;
            return true;
        }
        if state.mappers.len() >= MAX_MAPPERS || state.mappers.try_reserve(1).is_err() {
            return false;
        }
        state.mappers.push((space, pages));
        true
    }

    /// Space `space` maps `pages` fewer.
    pub fn remove_mapper(&self, space: u32, pages: usize) {
        let mut state = self.file().state.lock();
        if let Some(at) = state.mappers.iter().position(|entry| entry.0 == space) {
            let entry = &mut state.mappers[at];
            entry.1 = entry.1.saturating_sub(pages);
            if entry.1 == 0 {
                state.mappers.swap_remove(at);
            }
        }
    }

    /// Calls `each` with every space mapping the file.
    pub fn each_mapper(&self, each: &mut dyn FnMut(u32)) {
        let state = self.file().state.lock();
        for &(space, _) in &state.mappers {
            each(space);
        }
    }
}

impl Clone for FileRef {
    fn clone(&self) -> Self {
        // References are held by the index, by VMAs (bounded per space) and by opens, so
        // the count can't come near overflowing.
        self.file().refs.fetch_add(1, Ordering::Relaxed);
        Self(self.0)
    }
}

impl Drop for FileRef {
    fn drop(&mut self) {
        if self.file().refs.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        fence(Ordering::Acquire);
        // A dead file's hold, kept for whatever held it after it died. `forget` never
        // takes the tree's lock, so this may be wherever a reference goes.
        if self.file().forget_at_last.load(Ordering::Relaxed) {
            (self.file().forget)(self.file().key);
        }
        // The index's reference goes last for a live file, and only once its pages have;
        // a dead file's went when it died. So there are none here, unless the whole
        // cache is going, which only a test's does.
        // SAFETY: the last reference, so nothing else can reach the allocation, made by
        // `new` with this layout.
        unsafe {
            core::ptr::drop_in_place(self.0.as_ptr());
            dealloc(self.0.as_ptr().cast(), Layout::new::<CacheFile>());
        }
    }
}

impl core::fmt::Debug for FileRef {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let key = self.key();
        write!(out, "FileRef({}:{})", key.dev, key.ino)
    }
}

/// What [`PageCache::find_page`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The page's frame, with a reference taken for the caller's entry.
    Hit(usize),
    /// Not there: read it with no lock held, then [`insert_page`](PageCache::insert_page),
    /// telling it the change count seen here.
    Miss { change: u64 },
    /// Wholly past the end of the file, or the file is gone: `SIGBUS`.
    PastEnd,
    /// The frame has as many references as it can.
    Saturated,
}

/// How a file's bytes changed, as the filesystem said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change<'a> {
    /// `data` written at `offset`; the file is `size` bytes now.
    Written { offset: u64, data: &'a [u8], size: u64 },
    /// Cut or grown to `size`.
    Resized { size: u64 },
    /// A change of `[offset, offset + length)` failed part way; the file is `size` bytes.
    Failed { offset: u64, length: u64, size: u64 },
    /// Nothing changed.
    Nothing,
}

struct Index {
    /// Sorted by key.
    files: Vec<FileRef>,
    /// Where the clock's hand is: a file, and a page in it.
    hand: (usize, u64),
}

impl Index {
    fn find(&self, key: FileKey) -> Result<usize, usize> {
        self.files.binary_search_by(|file| file.key().cmp(&key))
    }
}

/// The cache. See the module docs.
pub struct PageCache {
    index: SpinLock<Index>,
    /// Pages held.
    pages: AtomicUsize,
    /// Of them, those no entry maps.
    idle: AtomicUsize,
    /// Of them, those dirty.
    dirty: AtomicUsize,
    /// Told the key of each file the cache stops keeping, to let its filesystem's hold go.
    forget: fn(FileKey),
}

/// Copies `data` into a frame at `offset`.
///
/// A move, not a copy: what a file is written from may be a mapping of that same page.
/// The kernel copies a program's buffer before writing it, but nothing makes every
/// writer do so.
///
/// # Safety
/// `frame` is a page the cache holds, and `offset + data.len() <= PAGE_SIZE`.
unsafe fn write_frame(frame: usize, offset: usize, data: &[u8]) {
    // SAFETY: as the caller promises; `copy` allows the two to overlap.
    unsafe { core::ptr::copy(data.as_ptr(), (frame + offset) as *mut u8, data.len()) };
}

impl PageCache {
    /// An empty cache. `forget` is told of each file it stops keeping.
    pub const fn new(forget: fn(FileKey)) -> Self {
        Self {
            index: SpinLock::new(Index { files: Vec::new(), hand: (0, 0) }),
            pages: AtomicUsize::new(0),
            idle: AtomicUsize::new(0),
            dirty: AtomicUsize::new(0),
            forget,
        }
    }

    /// Pages held.
    pub fn pages(&self) -> usize {
        self.pages.load(Ordering::Acquire)
    }

    /// Pages held that nothing maps, which could be taken back at once.
    pub fn idle(&self) -> usize {
        self.idle.load(Ordering::Acquire)
    }

    /// Pages held that are dirty, across every file.
    pub fn dirty(&self) -> usize {
        self.dirty.load(Ordering::Acquire)
    }

    /// Pages that could be taken back at once: those nothing maps, less any that may be
    /// dirty. Dirty pages aren't counted as idle or not, so this is a floor, which is
    /// what a promise counting on them needs.
    pub fn reclaimable(&self) -> usize {
        self.idle().saturating_sub(self.dirty())
    }

    /// Files kept.
    pub fn files(&self) -> usize {
        self.index.lock().files.len()
    }

    /// The kernel's say that the last entry mapping one of the cache's frames has gone.
    pub fn note_unmapped(&self) {
        self.idle.fetch_add(1, Ordering::AcqRel);
    }

    fn page_gone(&self, was_idle: bool) {
        self.pages.fetch_sub(1, Ordering::AcqRel);
        if was_idle {
            self.idle.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// The entry for `key`, if the cache has one.
    pub fn file(&self, key: FileKey) -> Option<FileRef> {
        let index = self.index.lock();
        index.find(key).ok().map(|at| index.files[at].clone())
    }

    /// The entry for `key`, made if it isn't there with the file `size` bytes long.
    /// Says whether it was made: if not, the caller's hold on the file is one too many.
    pub fn insert_file(&self, key: FileKey, size: u64) -> Result<(FileRef, bool), LockError> {
        let mut index = self.index.lock();
        match index.find(key) {
            Ok(at) => Ok((index.files[at].clone(), false)),
            Err(at) => {
                index.files.try_reserve(1).map_err(|_| LockError::NoMemory)?;
                let file = FileRef::new(key, size, self.forget).ok_or(LockError::NoMemory)?;
                index.files.insert(at, file.clone());
                if index.hand.0 > at {
                    index.hand.0 += 1;
                }
                Ok((file, true))
            }
        }
    }

    /// Sets the size of `file` to what the filesystem said, unless the file has changed
    /// since `change` was read, when what it said may be old.
    pub fn set_size_if(&self, file: &FileRef, size: u64, change: u64) {
        let mut state = file.file().state.lock();
        if state.change == change {
            state.size = size;
        }
    }

    /// Looks for page `index` of `file`. `exec` says it's to be run: `sync` is called
    /// with its frame first if the instruction cache may not agree with it.
    pub fn find_page(
        &self,
        file: &FileRef,
        index: u64,
        exec: bool,
        frames: &mut dyn CacheFrames,
        sync: &mut dyn FnMut(usize),
    ) -> Lookup {
        let mut state = file.file().state.lock();
        if state.dead || index.saturating_mul(PAGE_SIZE as u64) >= state.size {
            return Lookup::PastEnd;
        }
        let change = state.change;
        let Some(slot) = state.slot(index).filter(|slot| slot.frame != 0) else {
            return Lookup::Miss { change };
        };
        let frame = slot.frame;
        let Some(after) = frames.get(frame) else {
            return Lookup::Saturated;
        };
        slot.accessed = true;
        if exec && !slot.synced {
            sync(frame);
            slot.synced = true;
        }
        if after == 2 {
            // It was idle: the cache's was the only reference.
            self.idle.fetch_sub(1, Ordering::AcqRel);
        }
        Lookup::Hit(frame)
    }

    /// A frame for a page about to be read: free beyond the `reserved` frames the machine
    /// has promised, or taken from a page nothing maps. `None` if there's neither.
    pub fn take_frame(&self, frames: &mut dyn CacheFrames, reserved: usize) -> Option<usize> {
        // Short by however many: other allocations (page tables, kernel stacks) take free
        // frames without asking, so the free ones can be well under what's reserved.
        let free = frames.free_count();
        if free <= reserved {
            self.reclaim(reserved + 1 - free, frames);
        }
        for _ in 0..2 {
            if frames.free_count() > reserved {
                if let Some(frame) = frames.alloc() {
                    return Some(frame);
                }
            }
            if self.reclaim(1, frames) == 0 {
                return None;
            }
        }
        None
    }

    /// Gives back a frame from [`take_frame`](Self::take_frame) that wasn't put in.
    pub fn discard_frame(&self, frame: usize, frames: &mut dyn CacheFrames) {
        frames.drop_page(frame);
    }

    /// Puts `frame`, read while `change` was the file's change count, in as page `index`,
    /// and returns the page's frame with a reference taken for the caller if `want_ref`.
    /// If the file changed meanwhile, or the page is past its end now, the frame isn't
    /// put in and `Err` says what to do; if another read put the page in first, that's
    /// the one returned and `frame` is given back here.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_page(
        &self,
        file: &FileRef,
        index: u64,
        frame: usize,
        change: u64,
        exec: bool,
        want_ref: bool,
        frames: &mut dyn CacheFrames,
        sync: &mut dyn FnMut(usize),
    ) -> Result<Option<usize>, Lookup> {
        let mut state = file.file().state.lock();
        if state.dead || index.saturating_mul(PAGE_SIZE as u64) >= state.size {
            drop(state);
            frames.drop_page(frame);
            return Err(Lookup::PastEnd);
        }
        if state.change != change {
            let change = state.change;
            drop(state);
            frames.drop_page(frame);
            return Err(Lookup::Miss { change });
        }
        let Some(slot) = state.slot_or_new(index) else {
            drop(state);
            frames.drop_page(frame);
            return Err(Lookup::Saturated);
        };
        let new = slot.frame == 0;
        if new {
            *slot = Slot { frame, synced: false, accessed: true, dirty: false, taken: false };
        } else {
            frames.drop_page(frame);
        }
        let frame = slot.frame;
        if exec && !slot.synced {
            sync(frame);
            slot.synced = true;
        }
        if new {
            state.pages += 1;
            self.pages.fetch_add(1, Ordering::AcqRel);
            self.idle.fetch_add(1, Ordering::AcqRel);
        }
        if !want_ref {
            return Ok(None);
        }
        match frames.get(frame) {
            Some(after) => {
                if after == 2 {
                    self.idle.fetch_sub(1, Ordering::AcqRel);
                }
                Ok(Some(frame))
            }
            None => Err(Lookup::Saturated),
        }
    }

    /// A writable shared mapping's first write to page `index` of `file`, whose frame its
    /// entry has as `frame`: the page is dirty from now on, before the entry is made
    /// writable. `false` if the page isn't that frame any more (truncated, removed, or
    /// dropped by a failed change), when the write has nowhere to go: `SIGBUS`.
    pub fn mark_dirty(&self, file: &FileRef, index: u64, frame: usize) -> bool {
        let mut state = file.file().state.lock();
        if state.dead || index.saturating_mul(PAGE_SIZE as u64) >= state.size {
            return false;
        }
        let Some(slot) = state.slot(index).filter(|slot| slot.frame == frame) else {
            return false;
        };
        // Its bytes are about to change under any instruction cache that has them.
        slot.synced = false;
        slot.accessed = true;
        // A write-back that took its bytes before this write can't call it clean.
        slot.taken = false;
        if !core::mem::replace(&mut slot.dirty, true) {
            state.dirty += 1;
            self.dirty.fetch_add(1, Ordering::AcqRel);
        }
        true
    }

    /// The first dirty page of `file` in `[from, end)`.
    fn next_dirty(&self, file: &FileRef, from: u64, end: u64) -> Option<u64> {
        let state = file.file().state.lock();
        if state.dirty == 0 {
            return None;
        }
        state.first_dirty(from, end)
    }

    /// Writes the dirty pages of `file` in `[first, first + count)` back through
    /// `writer`, and says how many were written. A page is left clean if nothing could
    /// have written it since its bytes were taken (see the module docs): no entry mapped
    /// it then, or no writable shared mapping of the file existed then, after, or in
    /// between; otherwise it stays dirty, to be written again at the next write-back.
    /// Every dirty page is tried; `Err` if any write failed, and those stay dirty.
    pub fn write_back(
        &self,
        file: &FileRef,
        first: u64,
        count: u64,
        frames: &dyn CacheFrames,
        writer: &mut dyn PageWriter,
    ) -> Result<usize, WriteBackError> {
        let end = first.saturating_add(count);
        let page = PAGE_SIZE as u64;
        let mut at = first;
        let mut written = 0;
        let mut failed = false;
        while let Some(index) = self.next_dirty(file, at, end) {
            at = index + 1;
            // What the page was when its bytes were taken: (frame, write_gen, mapped
            // writable then, mapped at all then).
            let mut taken = None;
            let ok = writer.write(file.key(), index * page, &mut |buffer| {
                let mut state = file.file().state.lock();
                let size = state.size;
                let (generation, mapped) = (state.write_gen, state.write_pages > 0);
                let Some(slot) = state.slot(index) else {
                    return 0;
                };
                if slot.frame == 0 || !slot.dirty || index * page >= size {
                    return 0;
                }
                let length = (size - index * page).min(page) as usize;
                let length = length.min(buffer.len());
                // SAFETY: a frame of the cache's, a page long, kept by the cache while its
                // lock is held.
                unsafe { core::ptr::copy_nonoverlapping(slot.frame as *const u8, buffer.as_mut_ptr(), length) };
                slot.taken = true;
                // The cache's reference alone: no entry has it, so any write from now on
                // faults first, and the fault's `mark_dirty` clears `taken`. Entries only
                // get the frame under this lock.
                let unmapped = frames.refs(slot.frame) == 1;
                taken = Some((slot.frame, generation, mapped, unmapped));
                length
            });
            if !ok {
                // Whether or not it got as far as taking the bytes: a tree that refuses
                // first (a read-only mount, a file gone) wrote nothing either.
                failed = true;
                continue;
            }
            let Some((frame, generation, mapped, unmapped)) = taken else {
                // Nothing to write: clean, gone or past the end by the time it was asked.
                continue;
            };
            written += 1;
            let mut state = file.file().state.lock();
            // Nothing could have written it since its bytes were taken: no writable
            // mapping then, none now, and none in between.
            let unwritable = !mapped && state.write_pages == 0 && state.write_gen == generation;
            let mut cleaned = false;
            if let Some(slot) = state.slot(index).filter(|slot| slot.frame == frame && slot.dirty && slot.taken) {
                if unmapped || unwritable {
                    slot.dirty = false;
                    cleaned = true;
                }
                slot.taken = false;
            }
            if cleaned {
                self.lose_dirty(&mut state, 1);
            }
        }
        if failed {
            Err(WriteBackError)
        } else {
            Ok(written)
        }
    }

    /// Copies what `file`'s dirty pages hold over `out`, which was read from the file at
    /// `offset`: a read through the filesystem sees what mappings have written that
    /// hasn't been written back yet.
    pub fn read_dirty(&self, file: &FileRef, offset: u64, out: &mut [u8]) {
        let state = file.file().state.lock();
        if state.dirty == 0 {
            return;
        }
        let page = PAGE_SIZE as u64;
        let end = offset.saturating_add(out.len() as u64);
        let mut at = offset;
        while at < end {
            let index = at / page;
            let within = (at % page) as usize;
            let count = ((page - at % page).min(end - at)) as usize;
            if let Some(slot) = state.peek(index).filter(|slot| slot.frame != 0 && slot.dirty) {
                let start = (at - offset) as usize;
                // SAFETY: a frame of the cache's, kept while its lock is held, and
                // `within + count` is at most a page.
                unsafe { core::ptr::copy_nonoverlapping((slot.frame + within) as *const u8, out[start..].as_mut_ptr(), count) };
            }
            at += count as u64;
        }
    }

    /// Before a write or truncation of `key`: `TextBusy` if any of it is mapped to run.
    /// Otherwise it counts as a write in progress until [`changed`](Self::changed).
    pub fn may_change(&self, key: FileKey) -> Result<(), LockError> {
        let Some(file) = self.file(key) else {
            return Ok(());
        };
        let mut state = file.file().state.lock();
        if state.exec_pages > 0 {
            return Err(LockError::TextBusy);
        }
        state.writing += 1;
        Ok(())
    }

    /// After what [`may_change`](Self::may_change) allowed: brings the cached pages up to
    /// date. Calls `mappers` with each space mapping the file if pages it may have mapped
    /// are gone, for it to take them out of its tables.
    pub fn changed(&self, key: FileKey, change: Change<'_>, frames: &mut dyn CacheFrames, mappers: &mut dyn FnMut(u32)) {
        let Some(file) = self.file(key) else {
            return;
        };
        let mut state = file.file().state.lock();
        state.writing = state.writing.saturating_sub(1);
        state.change += 1;
        let page = PAGE_SIZE as u64;
        let old_pages = state.size.div_ceil(page);
        let mut cut = false;
        // Grown: what a shared writable mapping wrote past the old end, in the old last
        // page, was never the file's, and the file reads as zeroes there now.
        let grown_to = match change {
            Change::Written { size, .. } | Change::Resized { size } | Change::Failed { size, .. } => size,
            Change::Nothing => state.size,
        };
        if grown_to > state.size && state.size % page != 0 {
            let old_end = state.size;
            if let Some(slot) = state.slot(old_end / page).filter(|slot| slot.frame != 0) {
                let within = (old_end % page) as usize;
                // SAFETY: a frame of the cache's, cleared from `within` to its end.
                unsafe { core::ptr::write_bytes((slot.frame + within) as *mut u8, 0, PAGE_SIZE - within) };
                slot.synced = false;
            }
        }
        match change {
            Change::Nothing => {}
            Change::Written { offset, data, size } => {
                state.size = size;
                let end = offset.saturating_add(data.len() as u64);
                let mut at = offset;
                while at < end {
                    let index = at / page;
                    let within = (at % page) as usize;
                    let count = ((page - at % page).min(end - at)) as usize;
                    if let Some(slot) = state.slot(index).filter(|slot| slot.frame != 0) {
                        let start = (at - offset) as usize;
                        // SAFETY: a frame of the cache's, and `within + count` is at most
                        // a page.
                        unsafe { write_frame(slot.frame, within, &data[start..start + count]) };
                        // Its instructions are new: synced again before they next run.
                        slot.synced = false;
                    }
                    at += count as u64;
                }
            }
            Change::Resized { size } => {
                state.size = size;
                let keep = size.div_ceil(page);
                cut = self.drop_from(&mut state, keep, frames);
                if size % page != 0 {
                    if let Some(slot) = state.slot(size / page).filter(|slot| slot.frame != 0) {
                        let within = (size % page) as usize;
                        // SAFETY: a frame of the cache's, cleared from `within` to its end.
                        unsafe { core::ptr::write_bytes((slot.frame + within) as *mut u8, 0, PAGE_SIZE - within) };
                        slot.synced = false;
                    }
                }
            }
            Change::Failed { offset, length, size } => {
                // What's there isn't known, so a page nothing maps is read again. A dirty
                // one is kept: its bytes are what mappings wrote, newer than the file's,
                // and its write-back puts them over whatever the change left. So is one an
                // entry maps: dropped, the entry would be left on a frame that isn't the
                // file's any more, whose writes no write-back would see (and a writable
                // mapping's first write would find nowhere to go). Its mappings go on
                // seeing what they saw until it's unmapped and taken back.
                state.size = size;
                let first = offset / page;
                let last = offset.saturating_add(length).div_ceil(page);
                let mut dropped = 0;
                state.each_in(first, last, &mut |_, slot| {
                    if slot.dirty || frames.refs(slot.frame) != 1 {
                        return;
                    }
                    let idle = frames.drop_page(slot.frame);
                    self.page_gone(idle);
                    *slot = Slot::default();
                    dropped += 1;
                });
                state.pages -= dropped;
                cut |= self.drop_from(&mut state, size.div_ceil(page), frames);
            }
        }
        // A mapping may have a private copy of a page now past the end, which isn't a
        // frame of the cache's, so whether a mapped cache page was dropped doesn't say
        // whether anything has to leave a table: any cut that took whole pages off does.
        cut |= state.size.div_ceil(page) < old_pages;
        drop(state);
        if cut {
            file.each_mapper(mappers);
        }
    }

    /// Drops every page from `first` on. Says whether any was mapped.
    fn drop_from(&self, state: &mut FileState, first: u64, frames: &mut dyn CacheFrames) -> bool {
        let mut mapped = false;
        let mut dropped = 0;
        let mut dirty = 0;
        state.each_from(first, &mut |_, slot| {
            let idle = frames.drop_page(slot.frame);
            self.page_gone(idle);
            mapped |= !idle;
            dirty += usize::from(slot.dirty);
            *slot = Slot::default();
            dropped += 1;
        });
        state.pages -= dropped;
        self.lose_dirty(state, dirty);
        mapped
    }

    /// `count` dirty pages of `state`'s have gone without being written back.
    fn lose_dirty(&self, state: &mut FileState, count: usize) {
        state.dirty -= count;
        self.dirty.fetch_sub(count, Ordering::AcqRel);
    }

    /// The file's last name has gone: its pages go, and it's no longer kept. Mappings
    /// keep the pages they have; their other pages fault with `SIGBUS`. It's forgotten
    /// once nothing references it: until then the filesystem keeps its number, so what
    /// still holds it finds nothing by it rather than a file made since.
    pub fn removed(&self, key: FileKey, frames: &mut dyn CacheFrames) {
        let file = {
            let mut index = self.index.lock();
            let Ok(at) = index.find(key) else {
                return;
            };
            if index.hand.0 > at {
                index.hand.0 -= 1;
            }
            index.files.remove(at)
        };
        {
            let mut state = file.file().state.lock();
            state.dead = true;
            state.change += 1;
            self.drop_from(&mut state, 0, frames);
            state.leaves = Vec::new();
        }
        // The index's reference: the last, or the last but what still holds it.
        file.file().forget_at_last.store(true, Ordering::Relaxed);
        drop(file);
    }

    /// The mount `dev` is about to go: every file of it that only the cache refers to
    /// goes, so its hold doesn't keep the mount.
    pub fn unmounting(&self, dev: u64, frames: &mut dyn CacheFrames) {
        let mut gone = Vec::new();
        {
            let mut index = self.index.lock();
            let mut at = 0;
            while at < index.files.len() {
                let file = &index.files[at];
                let unused = FileRef::count(file) == 1 && {
                    let state = file.file().state.lock();
                    state.writers == 0 && state.exec_pages == 0 && state.mappers.is_empty()
                };
                if file.key().dev != dev || !unused || gone.try_reserve(1).is_err() {
                    at += 1;
                    continue;
                }
                if index.hand.0 > at {
                    index.hand.0 -= 1;
                }
                let file = index.files.remove(at);
                let mut state = file.file().state.lock();
                self.drop_from(&mut state, 0, frames);
                state.leaves = Vec::new();
                drop(state);
                gone.push(file);
            }
        }
        for file in gone {
            (self.forget)(file.key());
        }
    }

    /// Takes back up to `want` pages nothing maps, by the clock. Files left with
    /// nothing are forgotten. Returns how many pages went.
    pub fn reclaim(&self, want: usize, frames: &mut dyn CacheFrames) -> usize {
        self.sweep(want, false, frames)
    }

    /// Takes back every page nothing maps, used lately or not, and forgets every file
    /// left with nothing: what's left is what's in use.
    pub fn drop_idle(&self, frames: &mut dyn CacheFrames) -> usize {
        self.sweep(usize::MAX, true, frames)
    }

    fn sweep(&self, want: usize, everything: bool, frames: &mut dyn CacheFrames) -> usize {
        let mut freed = 0;
        let mut forgotten: Vec<FileKey> = Vec::new();
        {
            let mut index = self.index.lock();
            // Twice round at most: the first pass may only clear the accessed bits.
            let total = index.files.len();
            let mut visited = 0;
            while freed < want && visited <= 2 * total && !index.files.is_empty() {
                let (mut at, from) = index.hand;
                if at >= index.files.len() {
                    at = 0;
                }
                let file = index.files[at].clone();
                let mut state = file.file().state.lock();
                let mut last = from;
                let mut done = true;
                let mut doomed = 0usize;
                state.each_from(from, &mut |page, slot| {
                    if freed + doomed >= want {
                        done = false;
                        return;
                    }
                    last = page;
                    // Mapped, or holding what the file doesn't yet: kept.
                    if frames.refs(slot.frame) != 1 || slot.dirty {
                        return;
                    }
                    if slot.accessed && !everything {
                        slot.accessed = false;
                        return;
                    }
                    let idle = frames.drop_page(slot.frame);
                    self.page_gone(idle);
                    *slot = Slot::default();
                    doomed += 1;
                });
                state.pages -= doomed;
                freed += doomed;
                let unused = state.is_unused();
                drop(state);
                if !done {
                    index.hand = (at, last);
                    break;
                }
                visited += 1;
                // Only the index refers to it, and it holds nothing: forgotten.
                if unused && FileRef::count(&file) == 2 && forgotten.try_reserve(1).is_ok() {
                    drop(file);
                    let file = index.files.remove(at);
                    forgotten.push(file.key());
                    index.hand = (at, 0);
                } else {
                    index.hand = (at + 1, 0);
                }
            }
        }
        for key in forgotten {
            (self.forget)(key);
        }
        freed
    }
}

#[cfg(test)]
mod tests;
