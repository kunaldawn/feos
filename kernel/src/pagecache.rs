//! The kernel's page cache: `feos_pagecache` given frames, files and an instruction cache.
//!
//! What's cached and when it goes is the library's business. This is the glue: frames
//! from the allocator, marked as the cache's; pages read from the shared tree by file
//! identity; the instruction cache synced once per frame before a page first runs; and
//! the tree's [`FileHook`], which keeps the cached pages as the files are and refuses
//! writes to a file mapped to run (`ETXTBSY`), for kernel apps as for programs; and
//! writing back what shared writable mappings dirtied, through
//! [`VfsTree::write_back`], which takes the page's bytes under the tree's lock.
//!
//! Lock order: an address space, then the cache, then the tree, for anything that holds
//! one while it takes another, with one exception made safe by the cache's design: the
//! hook runs under the tree's lock and takes the cache's, which is why a fill never
//! holds a lock of the cache's while it reads the tree.

use core::sync::atomic::{AtomicBool, Ordering};

use feos_fs::vfs::{Change, FileHook, FileId, VfsTree};
use feos_fs::FsError;
use feos_hal::Arch;
use feos_sync::{Mutex, MutexGuard};
use feos_pagecache::{CacheFrames, FileKey, FileRef, LockError, Lookup, PageCache, PageWriter};
use feos_vm::{FaultError, FilePages, FrameSource, PAGE_SIZE};

use crate::{memory, sched, Port};

/// The cache.
pub static CACHE: PageCache = PageCache::new(forget);

fn tree() -> Option<&'static dyn VfsTree> {
    sched::platform().filesystem.and_then(|filesystem| filesystem.tree())
}

const fn key_of(file: FileId) -> FileKey {
    FileKey { dev: file.dev, ino: file.ino }
}

const fn id_of(key: FileKey) -> FileId {
    FileId { dev: key.dev, ino: key.ino }
}

/// The cache no longer keeps `key`: its hold on the file goes.
fn forget(key: FileKey) {
    if let Some(tree) = tree() {
        tree.release(id_of(key));
    }
}

/// Frames for the cache, straight from the allocator.
pub struct Frames;

impl CacheFrames for Frames {
    fn alloc(&mut self) -> Option<usize> {
        memory::alloc_cache_frame()
    }

    fn get(&mut self, frame: usize) -> Option<u32> {
        memory::get_frame_counted(frame)
    }

    fn drop_page(&mut self, frame: usize) -> bool {
        memory::drop_cache_frame(frame)
    }

    fn refs(&self, frame: usize) -> u32 {
        memory::frame_refs(frame)
    }

    fn free_count(&self) -> usize {
        memory::free_bytes() / PAGE_SIZE
    }
}

/// Frames the cache must leave free: every promise the machine has made, and the spare.
fn reserved() -> usize {
    crate::user::promised() + crate::user::SPARE_FRAMES
}

/// Keeps the frames promised free, not merely idle in the cache: promises count idle
/// pages as frames to be had, but the kernel's own allocations (stacks, tables) only see
/// free ones. Run after anything that may have promised more.
pub fn rebalance() {
    let free = memory::free_bytes() / PAGE_SIZE;
    let wanted = reserved();
    if free < wanted && CACHE.idle() > 0 {
        CACHE.reclaim(wanted - free, &mut Frames);
    }
}

/// Takes back one page nothing maps, for an allocation that found no free frame.
pub fn reclaim_one() -> bool {
    CACHE.reclaim(1, &mut Frames) > 0
}

/// Pages the cache holds, and of them those nothing maps.
pub fn counts() -> (usize, usize) {
    (CACHE.pages(), CACHE.idle())
}

/// Pages that could be taken back at once, for promises to count on: those nothing maps
/// that aren't dirty.
pub fn reclaimable() -> usize {
    CACHE.reclaimable()
}

/// Lets every page nothing maps go, for a count of frames that doesn't depend on what
/// happened to be read lately.
pub fn drop_idle() -> usize {
    CACHE.drop_idle(&mut Frames)
}

/// A frame that left an entry, which leaves the cache's own reference alone on it. Called
/// by `memory::put_frame_noting` under the allocator's lock.
pub fn note_unmapped() {
    CACHE.note_unmapped();
}

/// How many address spaces reverse maps can tell apart: each live space has a number of
/// its own below this (see `user::process::UserMm`), whichever slots its threads are in.
/// Twice the tasks, for spaces whose processes are still being torn down.
pub const SPACES: usize = 2 * feos_app::MAX_PROCESSES;

/// Address spaces that have pages of a truncated file past its end, which they take out
/// of their tables at their next call or fault, by their numbers: one address space isn't
/// changed from outside it.
static ZAP: [AtomicBool; SPACES] = [const { AtomicBool::new(false) }; SPACES];

/// Whether the space numbered `space` has truncated files to take out of its tables.
pub fn take_zap(space: u32) -> bool {
    ZAP.get(space as usize).is_some_and(|flag| flag.swap(false, Ordering::AcqRel))
}

/// Taken by [`file_for`] for the whole of making a hold, and by whatever decides from
/// [`held`] whether a file's last name may go, across the decision and the removal. So
/// a removal either sees the new hold and makes the file an orphan, or comes first, and
/// the hold fails `NotFound`: never a description of a file whose name went as it was
/// opened. Taken before the cache's lock and the tree's.
static REMOVING: Mutex<()> = Mutex::new(());

/// What a removal decided by [`held`] holds across the decision and the removal.
pub fn removing() -> MutexGuard<'static, ()> {
    REMOVING.lock()
}

/// The entry for the file `file`, `size` bytes long, made and held if there's none.
pub fn file_for(file: FileId, size: u64) -> Result<FileRef, FsError> {
    let _removing = REMOVING.lock();
    let key = key_of(file);
    if let Some(entry) = CACHE.file(key) {
        return Ok(entry);
    }
    let tree = tree().ok_or(FsError::NotFound)?;
    // Held outside the cache's lock: holding takes the tree's.
    tree.hold(file)?;
    let (entry, made) = match CACHE.insert_file(key, size) {
        Ok(found) => found,
        Err(_) => {
            tree.release(file);
            return Err(FsError::OutOfMemory);
        }
    };
    if !made {
        tree.release(file);
        return Ok(entry);
    }
    // The size given may be from before a write the hook told no entry of: asked again,
    // and kept unless the file has changed since this entry was made.
    if let Ok(stat) = tree.stat_file(file) {
        CACHE.set_size_if(&entry, stat.size, 0);
    }
    Ok(entry)
}

/// Whether anything but the cache itself holds the file `file`'s entry: a description, a
/// mapping, a running program's text. Its unlinking makes it an orphan if so. Asked
/// under [`removing`], or the answer may be stale by the time it's acted on.
pub fn held(file: FileId) -> bool {
    // The index's reference and the one just taken to look are the cache's own.
    CACHE.file(key_of(file)).is_some_and(|entry| FileRef::count(&entry) > 2)
}

fn sync(frame: usize) {
    Port::sync_user_code(frame, PAGE_SIZE);
}

/// Reads page `index` of `file` into `frame`, zeroing what's past its end.
fn read_page(tree: &dyn VfsTree, file: FileKey, index: u64, frame: usize) -> Result<(), FsError> {
    // SAFETY: a frame just taken for this, a page long, mapped by the identity map, that
    // nothing else can reach until it's put in the cache.
    let page = unsafe { core::slice::from_raw_parts_mut(frame as *mut u8, PAGE_SIZE) };
    let mut done = 0;
    while done < PAGE_SIZE {
        let offset = index * PAGE_SIZE as u64 + done as u64;
        match tree.read_file(id_of(file), offset, &mut page[done..])? {
            0 => break,
            count => done += count,
        }
    }
    page[done..].fill(0);
    Ok(())
}

/// Reads of a page a fault makes before giving up, when every one is thrown away because
/// the file changed meanwhile. A fault runs with kills held off, so this bounds how long
/// a program writing the file over and over (anywhere in it: the change count is the
/// whole file's) can keep another in the kernel.
const FILL_TRIES: usize = 8;

/// File pages for faults, from the cache.
pub struct CachePages;

impl FilePages for CachePages {
    fn page(&mut self, file: &FileRef, index: u64, exec: bool, _frames: &mut dyn FrameSource) -> Result<usize, FaultError> {
        let tree = tree().ok_or(FaultError::Bus)?;
        let mut sync = sync;
        for attempt in 0..FILL_TRIES {
            // A task being killed ends whatever this returns: no more reads for it.
            if attempt > 0 && sched::kill_requested() {
                return Err(FaultError::Bus);
            }
            let change = match CACHE.find_page(file, index, exec, &mut Frames, &mut sync) {
                Lookup::Hit(frame) => return Ok(frame),
                Lookup::Miss { change } => change,
                Lookup::PastEnd | Lookup::Saturated => return Err(FaultError::Bus),
            };
            // No lock of the cache's is held from here until the page goes in.
            let frame = CACHE.take_frame(&mut Frames, reserved()).ok_or(FaultError::Bus)?;
            if read_page(tree, file.key(), index, frame).is_err() {
                CACHE.discard_frame(frame, &mut Frames);
                return Err(FaultError::Bus);
            }
            match CACHE.insert_page(file, index, frame, change, exec, true, &mut Frames, &mut sync) {
                Ok(Some(frame)) => return Ok(frame),
                // The file changed while it was being read: again.
                Err(Lookup::Miss { .. }) => continue,
                Ok(None) | Err(_) => return Err(FaultError::Bus),
            }
        }
        Err(FaultError::Bus)
    }

    fn dirty(&mut self, file: &FileRef, index: u64, frame: usize) -> Result<(), FaultError> {
        if CACHE.mark_dirty(file, index, frame) {
            Ok(())
        } else {
            Err(FaultError::Bus)
        }
    }
}

/// Writes pages back through the tree.
struct TreeWriter(&'static dyn VfsTree);

impl PageWriter for TreeWriter {
    fn write(&mut self, key: FileKey, offset: u64, fill: &mut dyn FnMut(&mut [u8]) -> usize) -> bool {
        // A page's bytes, taken under the tree's lock and written from here: the program
        // may be writing the frame itself meanwhile, on another CPU.
        let mut page = [0u8; PAGE_SIZE];
        self.0.write_back(id_of(key), offset, &mut page, fill).is_ok()
    }
}

/// Writes the dirty pages of `file` in `[first, first + count)` back to the filesystem.
/// `Err` (`EIO`) if any write failed; those pages stay dirty.
pub fn write_back(file: &FileRef, first: u64, count: u64) -> Result<(), ()> {
    if file.dirty_pages() == 0 {
        return Ok(());
    }
    let tree = tree().ok_or(())?;
    CACHE.write_back(file, first, count, &Frames, &mut TreeWriter(tree)).map(|_| ()).map_err(|_| ())
}

/// Writes back every dirty page of the file `file` is, if the cache has any: for a
/// program's `CLOSE` of it.
pub fn write_back_id(file: FileId) -> Result<(), ()> {
    if CACHE.dirty() == 0 {
        return Ok(());
    }
    match CACHE.file(key_of(file)) {
        Some(entry) => write_back(&entry, 0, u64::MAX),
        None => Ok(()),
    }
}

/// Copies what mappings have written to the file `file`, and not yet written back, over
/// `out`, read from it at `offset`: a program's `READ` sees the file as its mappings do.
pub fn read_dirty(file: FileId, offset: u64, out: &mut [u8]) {
    if CACHE.dirty() == 0 {
        return;
    }
    if let Some(entry) = CACHE.file(key_of(file)) {
        CACHE.read_dirty(&entry, offset, out);
    }
}

/// Dirty pages, across every file.
pub fn dirty() -> usize {
    CACHE.dirty()
}

/// Whether any page anywhere is dirty: what lets `READ` and `CLOSE` skip asking.
pub fn any_dirty() -> bool {
    CACHE.dirty() > 0
}

/// Reads pages `[first, first + count)` of `file` into the cache, for `MADV_WILLNEED`,
/// as far as there are frames for them.
pub fn read_ahead(file: &FileRef, first: u64, count: u64) {
    let Some(tree) = tree() else {
        return;
    };
    let mut sync = sync;
    for index in first..first.saturating_add(count) {
        let change = match CACHE.find_page(file, index, false, &mut Frames, &mut sync) {
            Lookup::Hit(frame) => {
                // Looked at, not mapped: the reference goes straight back.
                memory::put_frame_noting(frame);
                continue;
            }
            Lookup::Miss { change } => change,
            Lookup::PastEnd | Lookup::Saturated => return,
        };
        let Some(frame) = CACHE.take_frame(&mut Frames, reserved()) else {
            return;
        };
        if read_page(tree, file.key(), index, frame).is_err() {
            CACHE.discard_frame(frame, &mut Frames);
            return;
        }
        if CACHE.insert_page(file, index, frame, change, false, false, &mut Frames, &mut sync).is_err() {
            return;
        }
    }
}

/// What the tree tells of every change to a file's bytes.
struct Hook;

impl FileHook for Hook {
    fn may_change(&self, file: FileId) -> Result<(), FsError> {
        CACHE.may_change(key_of(file)).map_err(|error| match error {
            LockError::TextBusy => FsError::TextBusy,
            LockError::NoMemory => FsError::OutOfMemory,
        })
    }

    fn changed(&self, file: FileId, change: Change<'_>) {
        let change = match change {
            Change::Written { offset, data, size } => feos_pagecache::Change::Written { offset, data, size },
            Change::Resized { size } => feos_pagecache::Change::Resized { size },
            Change::Failed { offset, length, size } => feos_pagecache::Change::Failed { offset, length, size },
            Change::Nothing => feos_pagecache::Change::Nothing,
        };
        CACHE.changed(key_of(file), change, &mut Frames, &mut |space| {
            if let Some(flag) = ZAP.get(space as usize) {
                flag.store(true, Ordering::Release);
            }
        });
    }

    fn removed(&self, file: FileId) {
        CACHE.removed(key_of(file), &mut Frames);
    }

    fn unmounting(&self, dev: u64) {
        CACHE.unmounting(dev, &mut Frames);
    }
}

static HOOK: Hook = Hook;

/// Tells the tree to tell the cache of changes. Once there's a tree, before any program
/// can map a file.
pub fn init() {
    if let Some(tree) = tree() {
        tree.set_file_hook(&HOOK);
    }
}
