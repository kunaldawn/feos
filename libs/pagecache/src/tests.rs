use super::*;
use std::collections::HashMap;
use std::sync::Mutex;

/// Frames from the host's heap, counted as the kernel counts them.
struct TestFrames {
    refs: HashMap<usize, u32>,
    cached: HashMap<usize, bool>,
    free: usize,
    allocated: usize,
}

impl TestFrames {
    fn new(free: usize) -> Self {
        Self { refs: HashMap::new(), cached: HashMap::new(), free, allocated: 0 }
    }

    /// Drops an entry's reference, as `feos-vm` does when an entry goes, and tells the
    /// cache if that left it the only one.
    fn put(&mut self, frame: usize, cache: &PageCache) {
        let refs = self.refs.get_mut(&frame).unwrap();
        *refs -= 1;
        if *refs == 1 && self.cached[&frame] {
            cache.note_unmapped();
        }
        if *refs == 0 {
            self.release(frame);
        }
    }

    fn release(&mut self, frame: usize) {
        self.refs.remove(&frame);
        self.cached.remove(&frame);
        self.free += 1;
        self.allocated -= 1;
        // SAFETY: allocated below with this layout.
        unsafe { std::alloc::dealloc(frame as *mut u8, std::alloc::Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap()) };
    }
}

impl CacheFrames for TestFrames {
    fn alloc(&mut self) -> Option<usize> {
        if self.free == 0 {
            return None;
        }
        // SAFETY: not zero sized.
        let frame = unsafe { std::alloc::alloc(std::alloc::Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap()) } as usize;
        self.free -= 1;
        self.allocated += 1;
        self.refs.insert(frame, 1);
        self.cached.insert(frame, true);
        Some(frame)
    }

    fn get(&mut self, frame: usize) -> Option<u32> {
        let refs = self.refs.get_mut(&frame)?;
        *refs += 1;
        Some(*refs)
    }

    fn drop_page(&mut self, frame: usize) -> bool {
        self.cached.insert(frame, false);
        let refs = self.refs.get_mut(&frame).unwrap();
        *refs -= 1;
        if *refs == 0 {
            self.release(frame);
            true
        } else {
            false
        }
    }

    fn refs(&self, frame: usize) -> u32 {
        self.refs.get(&frame).copied().unwrap_or(0)
    }

    fn free_count(&self) -> usize {
        self.free
    }
}

static FORGOTTEN: Mutex<Vec<FileKey>> = Mutex::new(Vec::new());

fn forget(key: FileKey) {
    FORGOTTEN.lock().unwrap().push(key);
}

const KEY: FileKey = FileKey { dev: 1, ino: 7 };

/// A file of `size` bytes, each byte its offset's low bits plus `seed`.
fn contents(size: usize, seed: u8) -> Vec<u8> {
    (0..size).map(|at| (at as u8).wrapping_add(seed)).collect()
}

/// Reads page `index` of `data` the way the kernel does: missing, read, put in.
fn fault(cache: &PageCache, file: &FileRef, index: u64, data: &[u8], frames: &mut TestFrames) -> Result<usize, Lookup> {
    let mut sync = |_frame: usize| {};
    loop {
        let change = match cache.find_page(file, index, false, frames, &mut sync) {
            Lookup::Hit(frame) => return Ok(frame),
            Lookup::Miss { change } => change,
            other => return Err(other),
        };
        let frame = cache.take_frame(frames, 0).ok_or(Lookup::Saturated)?;
        let start = index as usize * PAGE_SIZE;
        let page = &data[start.min(data.len())..(start + PAGE_SIZE).min(data.len())];
        // SAFETY: a frame of ours, a page long.
        unsafe {
            core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE);
            core::ptr::copy_nonoverlapping(page.as_ptr(), frame as *mut u8, page.len());
        }
        match cache.insert_page(file, index, frame, change, false, true, frames, &mut sync) {
            Ok(frame) => return Ok(frame.unwrap()),
            Err(Lookup::Miss { .. }) => continue,
            Err(other) => return Err(other),
        }
    }
}

fn byte(frame: usize, offset: usize) -> u8 {
    // SAFETY: a frame a test holds a reference to.
    unsafe { *((frame + offset) as *const u8) }
}

#[test]
fn hit_shares_frame() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(3 * PAGE_SIZE, 1);
    let (file, made) = cache.insert_file(KEY, data.len() as u64).unwrap();
    assert!(made);
    let first = fault(&cache, &file, 1, &data, &mut frames).unwrap();
    let second = fault(&cache, &file, 1, &data, &mut frames).unwrap();
    assert_eq!(first, second, "one frame for every mapping of a page");
    assert_eq!(frames.refs(first), 3, "the cache's and two entries'");
    assert_eq!(byte(first, 5), data[PAGE_SIZE + 5]);
    let (again, made) = cache.insert_file(KEY, 0).unwrap();
    assert!(!made && FileRef::ptr_eq(&file, &again));
    assert_eq!((cache.pages(), cache.idle()), (1, 0));
    frames.put(first, &cache);
    frames.put(first, &cache);
    assert_eq!(cache.idle(), 1);
}

#[test]
fn miss_reads_once() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let (file, _) = cache.insert_file(KEY, 2 * PAGE_SIZE as u64).unwrap();
    let mut sync = |_frame: usize| {};
    let Lookup::Miss { change } = cache.find_page(&file, 0, false, &mut frames, &mut sync) else {
        panic!("a page never read is a miss");
    };
    // Two readers miss at once; the second one's read is given back.
    let one = cache.take_frame(&mut frames, 0).unwrap();
    let two = cache.take_frame(&mut frames, 0).unwrap();
    let kept = cache.insert_page(&file, 0, one, change, false, true, &mut frames, &mut sync).unwrap().unwrap();
    let other = cache.insert_page(&file, 0, two, change, false, true, &mut frames, &mut sync).unwrap().unwrap();
    assert_eq!((kept, other), (one, one));
    assert_eq!(frames.allocated, 1, "the second read's frame went back");
    assert_eq!(cache.pages(), 1);
    // A read that started before a change is thrown away, not put in.
    let Lookup::Miss { change } = cache.find_page(&file, 1, false, &mut frames, &mut sync) else {
        panic!("page 1 isn't there");
    };
    cache.may_change(KEY).unwrap();
    cache.changed(KEY, Change::Written { offset: 0, data: b"x", size: 2 * PAGE_SIZE as u64 }, &mut frames, &mut |_| {});
    let stale = cache.take_frame(&mut frames, 0).unwrap();
    assert!(matches!(
        cache.insert_page(&file, 1, stale, change, false, true, &mut frames, &mut sync),
        Err(Lookup::Miss { .. })
    ));
    assert_eq!(frames.allocated, 1);
    // Past the end is past the end.
    assert_eq!(cache.find_page(&file, 2, false, &mut frames, &mut sync), Lookup::PastEnd);
}

#[test]
fn write_hook_updates_cached_page() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(2 * PAGE_SIZE, 0);
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    let frame = fault(&cache, &file, 0, &data, &mut frames).unwrap();
    // A write across the end of the cached page and into one that isn't cached.
    let written = vec![0xee; 100];
    cache.may_change(KEY).unwrap();
    let offset = PAGE_SIZE as u64 - 50;
    let size = 2 * PAGE_SIZE as u64 + 10;
    cache.changed(KEY, Change::Written { offset, data: &written, size }, &mut frames, &mut |_| {});
    assert_eq!(byte(frame, PAGE_SIZE - 51), data[PAGE_SIZE - 51]);
    assert_eq!(byte(frame, PAGE_SIZE - 50), 0xee);
    assert_eq!(byte(frame, PAGE_SIZE - 1), 0xee);
    assert_eq!(file.size(), size, "the file grew");
    assert_eq!(cache.pages(), 1, "a write doesn't read anything in");
}

#[test]
fn write_hook_clears_icache_synced() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let (file, _) = cache.insert_file(KEY, PAGE_SIZE as u64).unwrap();
    let data = contents(PAGE_SIZE, 3);
    let mut synced = 0;
    let frame = fault(&cache, &file, 0, &data, &mut frames).unwrap();
    let mut count = |_frame: usize| synced += 1;
    assert_eq!(cache.find_page(&file, 0, true, &mut frames, &mut count), Lookup::Hit(frame));
    assert_eq!(cache.find_page(&file, 0, true, &mut frames, &mut count), Lookup::Hit(frame));
    assert_eq!(synced, 1, "synced once per frame");
    cache.may_change(KEY).unwrap();
    cache.changed(KEY, Change::Written { offset: 0, data: b"ret", size: PAGE_SIZE as u64 }, &mut frames, &mut |_| {});
    let mut count = |_frame: usize| synced += 1;
    assert_eq!(cache.find_page(&file, 0, true, &mut frames, &mut count), Lookup::Hit(frame));
    assert_eq!(synced, 2, "new contents are synced again");
}

#[test]
fn truncate_drops_pages_past_end() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(4 * PAGE_SIZE, 9);
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    for index in 0..4 {
        let frame = fault(&cache, &file, index, &data, &mut frames).unwrap();
        if index != 3 {
            frames.put(frame, &cache);
        }
    }
    file.add_mapper(5, 1);
    let mut told = Vec::new();
    cache.may_change(KEY).unwrap();
    cache.changed(KEY, Change::Resized { size: PAGE_SIZE as u64 + 100 }, &mut frames, &mut |space| told.push(space));
    assert_eq!(cache.pages(), 2, "pages 2 and 3 are gone");
    assert_eq!(told, [5], "a mapped page went: its mappers are told");
    let mut sync = |_frame: usize| {};
    assert_eq!(cache.find_page(&file, 2, false, &mut frames, &mut sync), Lookup::PastEnd);
    // The last page's tail is zero now.
    let Lookup::Hit(frame) = cache.find_page(&file, 1, false, &mut frames, &mut sync) else {
        panic!("page 1 stays");
    };
    assert_eq!(byte(frame, 99), data[PAGE_SIZE + 99]);
    assert_eq!(byte(frame, 100), 0);
    // Page 3's frame is still mapped, and still the mapping's, no longer the cache's.
    assert_eq!(frames.allocated, 3);
    assert_eq!(cache.idle(), 1, "page 0 is idle; page 1 was just mapped");
}

#[test]
fn reverse_map_lists_mappers() {
    let cache = PageCache::new(forget);
    let (file, _) = cache.insert_file(KEY, PAGE_SIZE as u64).unwrap();
    assert!(file.add_mapper(1, 3));
    assert!(file.add_mapper(2, 1));
    assert!(file.add_mapper(1, 2));
    let mut spaces = Vec::new();
    file.each_mapper(&mut |space| spaces.push(space));
    spaces.sort_unstable();
    assert_eq!(spaces, [1, 2]);
    file.remove_mapper(1, 3);
    file.remove_mapper(2, 1);
    let mut spaces = Vec::new();
    file.each_mapper(&mut |space| spaces.push(space));
    assert_eq!(spaces, [1], "space 1 still maps two pages");
    file.remove_mapper(1, 2);
    let mut spaces = Vec::new();
    file.each_mapper(&mut |space| spaces.push(space));
    assert!(spaces.is_empty());
    for space in 0..MAX_MAPPERS as u32 {
        assert!(file.add_mapper(space, 1));
    }
    assert!(!file.add_mapper(MAX_MAPPERS as u32, 1), "the reverse map is bounded");
}

#[test]
fn lru_reclaims_unmapped_only() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(8 * PAGE_SIZE, 2);
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    let mut mapped = Vec::new();
    for index in 0..8 {
        let frame = fault(&cache, &file, index, &data, &mut frames).unwrap();
        if index % 2 == 0 {
            frames.put(frame, &cache);
        } else {
            mapped.push(frame);
        }
    }
    assert_eq!(cache.idle(), 4);
    // The first pass only takes the accessed bits; the pages go on the second.
    let freed = cache.reclaim(100, &mut frames);
    assert_eq!(freed, 4);
    assert_eq!((cache.pages(), cache.idle()), (4, 0));
    let mut sync = |_frame: usize| {};
    for index in 0..8 {
        let found = cache.find_page(&file, index, false, &mut frames, &mut sync);
        if index % 2 == 0 {
            assert!(matches!(found, Lookup::Miss { .. }), "page {index}");
        } else {
            assert_eq!(found, Lookup::Hit(mapped[index as usize / 2]), "page {index}");
        }
    }
}

#[test]
fn reclaim_never_frees_mapped() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(16);
    let data = contents(16 * PAGE_SIZE, 4);
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    let frames_held: Vec<usize> = (0..16).map(|index| fault(&cache, &file, index, &data, &mut frames).unwrap()).collect();
    assert_eq!(cache.drop_idle(&mut frames), 0);
    assert_eq!(cache.take_frame(&mut frames, 0), None, "nothing free and nothing idle");
    for &frame in &frames_held {
        assert_eq!(frames.refs(frame), 2);
    }
    // Unmapped, every one can go, and the file with them.
    for frame in frames_held {
        frames.put(frame, &cache);
    }
    drop(file);
    FORGOTTEN.lock().unwrap().retain(|key| *key != KEY);
    assert_eq!(cache.drop_idle(&mut frames), 16);
    assert_eq!((cache.pages(), cache.idle(), cache.files(), frames.allocated), (0, 0, 0, 0));
    assert!(FORGOTTEN.lock().unwrap().contains(&KEY), "its hold is let go");
}

#[test]
fn fill_respects_promises() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(10);
    let (file, _) = cache.insert_file(KEY, 64 * PAGE_SIZE as u64).unwrap();
    // Eight of ten frames are promised: two may be taken, and no more.
    let reserved = 8;
    let one = cache.take_frame(&mut frames, reserved).unwrap();
    let two = cache.take_frame(&mut frames, reserved).unwrap();
    assert_eq!(cache.take_frame(&mut frames, reserved), None);
    let mut sync = |_frame: usize| {};
    let mut put_in = |frame: usize, index: u64, frames: &mut TestFrames| {
        cache.insert_page(&file, index, frame, 0, false, true, frames, &mut sync).unwrap().unwrap()
    };
    let one = put_in(one, 0, &mut frames);
    let _two = put_in(two, 1, &mut frames);
    // Once a page is unmapped, its frame is the one taken.
    frames.put(one, &cache);
    let again = cache.take_frame(&mut frames, reserved);
    assert!(again.is_some());
    assert_eq!(frames.free_count() + frames.allocated, 10);
    assert_eq!(frames.free_count(), reserved, "never below what's promised");
}

#[test]
fn lock_table_counts() {
    let cache = PageCache::new(forget);
    let (file, _) = cache.insert_file(KEY, PAGE_SIZE as u64).unwrap();
    file.open_writer().unwrap();
    assert_eq!(file.map_exec(1), Err(LockError::TextBusy), "open for writing: can't run");
    file.close_writer();
    file.map_exec(2).unwrap();
    assert_eq!(file.open_writer(), Err(LockError::TextBusy), "mapped to run: can't write");
    assert_eq!(cache.may_change(KEY), Err(LockError::TextBusy));
    file.unmap_exec(2);
    cache.may_change(KEY).unwrap();
    assert_eq!(file.map_exec(1), Err(LockError::TextBusy), "a write in progress");
    cache.changed(KEY, Change::Nothing, &mut TestFrames::new(0), &mut |_| {});
    file.map_exec(1).unwrap();
}

#[test]
fn a_removed_file_is_forgotten_and_its_mappings_keep_their_pages() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(10);
    let key = FileKey { dev: 3, ino: 3 };
    let data = contents(2 * PAGE_SIZE, 0);
    let (file, _) = cache.insert_file(key, data.len() as u64).unwrap();
    let mapped = fault(&cache, &file, 0, &data, &mut frames).unwrap();
    let idle = fault(&cache, &file, 1, &data, &mut frames).unwrap();
    frames.put(idle, &cache);
    cache.removed(key, &mut frames);
    assert!(cache.file(key).is_none());
    // Held still, by what mapped it: the number isn't the filesystem's to give again.
    assert!(!FORGOTTEN.lock().unwrap().contains(&key));
    assert_eq!((cache.pages(), cache.idle()), (0, 0));
    assert_eq!(frames.refs(mapped), 1, "the mapping's");
    let mut sync = |_frame: usize| {};
    assert_eq!(cache.find_page(&file, 1, false, &mut frames, &mut sync), Lookup::PastEnd);
    // A new file of that number is a new entry.
    let (fresh, made) = cache.insert_file(key, 1).unwrap();
    assert!(made && !FileRef::ptr_eq(&file, &fresh));
    frames.put(mapped, &cache);
    assert_eq!(frames.allocated, 0);
    assert!(!FORGOTTEN.lock().unwrap().contains(&key));
    drop(file);
    assert!(FORGOTTEN.lock().unwrap().contains(&key), "with the last reference");
    drop(fresh);
}

#[test]
fn a_removed_file_nothing_else_holds_is_forgotten_at_once() {
    let cache = PageCache::new(forget);
    let key = FileKey { dev: 3, ino: 9 };
    drop(cache.insert_file(key, 1).unwrap());
    cache.removed(key, &mut TestFrames::new(0));
    assert!(FORGOTTEN.lock().unwrap().contains(&key));
}

#[test]
fn a_fill_short_by_many_takes_back_enough() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(20);
    let data = contents(10 * PAGE_SIZE, 1);
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    for index in 0..10 {
        let frame = fault(&cache, &file, index, &data, &mut frames).unwrap();
        frames.put(frame, &cache);
    }
    // Something else took frames without asking: free is now well under what's reserved.
    let reserved = frames.free_count() + 3;
    let frame = cache.take_frame(&mut frames, reserved).expect("idle pages make up the difference");
    assert!(frames.free_count() > reserved - 1);
    cache.discard_frame(frame, &mut frames);
}

#[test]
fn one_page_at_a_time_the_clock_goes_round_every_file() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(4 * PAGE_SIZE, 5);
    let files: Vec<FileRef> = (0..3)
        .map(|ino| cache.insert_file(FileKey { dev: 9, ino }, data.len() as u64).unwrap().0)
        .collect();
    for file in &files {
        for index in 0..4 {
            let frame = fault(&cache, file, index, &data, &mut frames).unwrap();
            frames.put(frame, &cache);
        }
    }
    let freed: usize = (0..12).map(|_| cache.reclaim(1, &mut frames)).sum();
    assert_eq!(freed, 12);
    assert_eq!((cache.pages(), cache.idle()), (0, 0));
}

#[test]
fn truncate_tells_mappers_with_only_private_copies() {
    // A mapping that copied page 1 on write holds a frame that isn't the cache's: the
    // cache's page 1 is idle, but the copy past the new end must still leave the table.
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(2 * PAGE_SIZE, 4);
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    let frame = fault(&cache, &file, 1, &data, &mut frames).unwrap();
    frames.put(frame, &cache);
    file.add_mapper(5, 2);
    let mut told = Vec::new();
    cache.may_change(KEY).unwrap();
    cache.changed(KEY, Change::Resized { size: PAGE_SIZE as u64 - 10 }, &mut frames, &mut |space| told.push(space));
    assert_eq!(told, [5], "whole pages went: the mappers are told");
    // A cut within the last page takes no whole page off: nothing to tell.
    let mut told = Vec::new();
    cache.may_change(KEY).unwrap();
    cache.changed(KEY, Change::Resized { size: 100 }, &mut frames, &mut |space| told.push(space));
    assert!(told.is_empty());
    // Nor does a failed change that leaves the size.
    cache.may_change(KEY).unwrap();
    cache.changed(KEY, Change::Failed { offset: 0, length: 10, size: 100 }, &mut frames, &mut |space| told.push(space));
    assert!(told.is_empty());
}

/// A file's bytes as its filesystem holds them, written back to by the cache; a write
/// can be made to fail.
struct Disk {
    data: Vec<u8>,
    fail: bool,
    writes: usize,
}

impl PageWriter for Disk {
    fn write(&mut self, _key: FileKey, offset: u64, fill: &mut dyn FnMut(&mut [u8]) -> usize) -> bool {
        let mut page = [0u8; PAGE_SIZE];
        let length = fill(&mut page);
        if length == 0 {
            return true;
        }
        if self.fail {
            return false;
        }
        let at = offset as usize;
        self.data[at..at + length].copy_from_slice(&page[..length]);
        self.writes += 1;
        true
    }
}

/// A shared writable mapping's write to page `index`: mapped, marked dirty, then written
/// through the entry, as `feos-vm` and the program do.
fn write_mapped(cache: &PageCache, file: &FileRef, index: u64, at: usize, value: u8, data: &[u8], frames: &mut TestFrames) -> usize {
    let frame = fault(cache, file, index, data, frames).unwrap();
    assert!(cache.mark_dirty(file, index, frame));
    // SAFETY: a frame of the cache's, which the test's entry holds a reference to.
    unsafe { *((frame + at) as *mut u8) = value };
    frame
}

#[test]
fn dirty_on_first_write() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(3 * PAGE_SIZE, 6);
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    file.map_shared_write(3).unwrap();
    // Read only: clean.
    let read = fault(&cache, &file, 0, &data, &mut frames).unwrap();
    assert_eq!((file.dirty_pages(), cache.dirty()), (0, 0));
    // The first write marks it, once however many writes follow.
    let frame = write_mapped(&cache, &file, 1, 5, 0xaa, &data, &mut frames);
    assert!(cache.mark_dirty(&file, 1, frame));
    assert_eq!((file.dirty_pages(), cache.dirty()), (1, 1));
    // A frame the page isn't, or past the end: nowhere for the write to go.
    assert!(!cache.mark_dirty(&file, 1, read));
    assert!(!cache.mark_dirty(&file, 3, frame));
    // Mapped writable, the file can't be made to run; and the other way round.
    assert_eq!(file.map_exec(1), Err(LockError::TextBusy));
    file.unmap_shared_write(3);
    file.map_exec(1).unwrap();
    assert_eq!(file.map_shared_write(1), Err(LockError::TextBusy));
    file.unmap_exec(1);
    // Dirty and unmapped, it isn't reclaimed, nor counted as reclaimable.
    frames.put(read, &cache);
    frames.put(frame, &cache);
    assert_eq!(cache.idle(), 2);
    assert_eq!(cache.reclaimable(), 1);
    assert_eq!(cache.drop_idle(&mut frames), 1, "only the clean page goes");
    let mut sync = |_frame: usize| {};
    assert_eq!(cache.find_page(&file, 1, false, &mut frames, &mut sync), Lookup::Hit(frame));
    assert_eq!(byte(frame, 5), 0xaa, "what was written is kept");
    // A truncation past it drops it, and its dirt with it.
    frames.put(frame, &cache);
    cache.may_change(KEY).unwrap();
    cache.changed(KEY, Change::Resized { size: 10 }, &mut frames, &mut |_| {});
    assert_eq!((file.dirty_pages(), cache.dirty(), cache.pages()), (0, 0, 0));
}

#[test]
fn msync_writes_back() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(3 * PAGE_SIZE + 100, 7);
    let mut disk = Disk { data: data.clone(), fail: false, writes: 0 };
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    file.map_shared_write(4).unwrap();
    let pages: Vec<usize> = (0..4).map(|index| write_mapped(&cache, &file, index, 50, index as u8, &data, &mut frames)).collect();
    // A range: only its pages are written, and the last page only as far as the file goes.
    assert_eq!(cache.write_back(&file, 1, 1, &frames, &mut disk), Ok(1));
    assert_eq!(disk.data[PAGE_SIZE + 50], 1);
    assert_eq!(disk.data[50], data[50], "page 0 is outside the range");
    assert_eq!(cache.write_back(&file, 3, 1, &frames, &mut disk), Ok(1));
    assert_eq!(disk.data.len(), data.len(), "nothing written past the end");
    assert_eq!(disk.data[3 * PAGE_SIZE + 50], 3);
    // Still mapped writable: the pages stay dirty, and are written again next time.
    assert_eq!(file.dirty_pages(), 4);
    assert_eq!(cache.write_back(&file, 0, u64::MAX, &frames, &mut disk), Ok(4));
    assert_eq!(disk.writes, 6);
    assert_eq!(disk.data[50], 0);
    // A page nobody wrote isn't written at all.
    assert_eq!(cache.write_back(&file, 4, 10, &frames, &mut disk), Ok(0));
    for frame in pages {
        frames.put(frame, &cache);
    }
}

#[test]
fn munmap_writes_back() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(2 * PAGE_SIZE, 8);
    let mut disk = Disk { data: data.clone(), fail: false, writes: 0 };
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    file.map_shared_write(2).unwrap();
    let frame = write_mapped(&cache, &file, 0, 9, 0x99, &data, &mut frames);
    // Unmapped: the entry goes, then the writable mapping's count, then the write-back,
    // after which nothing can write the page without faulting first: clean.
    frames.put(frame, &cache);
    file.unmap_shared_write(2);
    assert_eq!(cache.write_back(&file, 0, u64::MAX, &frames, &mut disk), Ok(1));
    assert_eq!(disk.data[9], 0x99);
    assert_eq!((file.dirty_pages(), cache.dirty(), cache.reclaimable()), (0, 0, 1));
    assert_eq!(cache.write_back(&file, 0, u64::MAX, &frames, &mut disk), Ok(0), "clean: nothing to write");
    // A write fault that came while a page was being written back, after its bytes were
    // taken, leaves it dirty: what it let be written isn't on the disk.
    let frame = write_mapped(&cache, &file, 1, 1, 0x11, &data, &mut frames);
    frames.put(frame, &cache);
    struct Racing<'a>(&'a PageCache, &'a FileRef, usize, Disk);
    impl PageWriter for Racing<'_> {
        fn write(&mut self, key: FileKey, offset: u64, fill: &mut dyn FnMut(&mut [u8]) -> usize) -> bool {
            let done = self.3.write(key, offset, fill);
            self.1.map_shared_write(1).unwrap();
            assert!(self.0.mark_dirty(self.1, 1, self.2));
            // SAFETY: a frame of the cache's, which the cache still holds.
            unsafe { *((self.2 + 1) as *mut u8) = 0x12 };
            self.1.unmap_shared_write(1);
            done
        }
    }
    let mut racing = Racing(&cache, &file, frame, disk);
    assert_eq!(cache.write_back(&file, 0, u64::MAX, &frames, &mut racing), Ok(1));
    assert_eq!((file.dirty_pages(), racing.3.data[PAGE_SIZE + 1]), (1, 0x11), "kept dirty");
    assert_eq!(cache.write_back(&file, 0, u64::MAX, &frames, &mut racing.3), Ok(1));
    assert_eq!((file.dirty_pages(), racing.3.data[PAGE_SIZE + 1]), (0, 0x12));
    assert_eq!(cache.drop_idle(&mut frames), 2, "clean and unmapped, both can go");
}

#[test]
fn write_back_error_is_eio() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(2 * PAGE_SIZE, 9);
    let mut disk = Disk { data: data.clone(), fail: true, writes: 0 };
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    file.map_shared_write(2).unwrap();
    let pages = [
        write_mapped(&cache, &file, 0, 0, 1, &data, &mut frames),
        write_mapped(&cache, &file, 1, 0, 2, &data, &mut frames),
    ];
    for frame in pages {
        frames.put(frame, &cache);
    }
    file.unmap_shared_write(2);
    assert_eq!(cache.write_back(&file, 0, u64::MAX, &frames, &mut disk), Err(WriteBackError));
    assert_eq!(file.dirty_pages(), 2, "a page that wasn't written stays dirty");
    assert_eq!(cache.drop_idle(&mut frames), 0, "and isn't thrown away");
    disk.fail = false;
    assert_eq!(cache.write_back(&file, 0, u64::MAX, &frames, &mut disk), Ok(2));
    assert_eq!((disk.data[0], disk.data[PAGE_SIZE]), (1, 2));
    assert_eq!(file.dirty_pages(), 0);
}

#[test]
fn read_sees_dirty_page() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(3 * PAGE_SIZE, 10);
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    file.map_shared_write(3).unwrap();
    let frame = write_mapped(&cache, &file, 1, 0, 0x77, &data, &mut frames);
    // SAFETY: as in `write_mapped`.
    unsafe { *((frame + PAGE_SIZE - 1) as *mut u8) = 0x78 };
    // Read from the file across pages 0 to 2: what page 1's mapping wrote shows through,
    // and the clean pages are as the file has them.
    let start = PAGE_SIZE - 10;
    let mut out = data[start..start + PAGE_SIZE + 20].to_vec();
    cache.read_dirty(&file, start as u64, &mut out);
    assert_eq!(&out[..10], &data[start..PAGE_SIZE]);
    assert_eq!(out[10], 0x77);
    assert_eq!(out[10 + PAGE_SIZE - 1], 0x78);
    assert_eq!(out[10 + 1], data[PAGE_SIZE + 1]);
    assert_eq!(&out[10 + PAGE_SIZE..], &data[2 * PAGE_SIZE..2 * PAGE_SIZE + 10]);
    frames.put(frame, &cache);
}

#[test]
fn growth_clears_what_a_mapping_wrote_past_the_end() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(100, 11);
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    file.map_shared_write(1).unwrap();
    // A shared writable mapping may write the last page past the end: not the file's.
    let frame = write_mapped(&cache, &file, 0, 200, 0xbb, &data, &mut frames);
    cache.may_change(KEY).unwrap();
    cache.changed(KEY, Change::Written { offset: 300, data: b"z", size: 301 }, &mut frames, &mut |_| {});
    assert_eq!(byte(frame, 99), data[99]);
    assert_eq!(byte(frame, 200), 0, "the gap the write left reads as zeroes");
    assert_eq!(byte(frame, 300), b'z');
    frames.put(frame, &cache);
}

#[test]
fn unmapped_page_is_clean_while_the_file_is_mapped_writable() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(4 * PAGE_SIZE, 12);
    let mut disk = Disk { data: data.clone(), fail: false, writes: 0 };
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    // One page stays mapped writable for good; the others are written and unmapped.
    file.map_shared_write(4).unwrap();
    let kept = write_mapped(&cache, &file, 0, 0, 0xa0, &data, &mut frames);
    for index in 1..4 {
        let frame = write_mapped(&cache, &file, index, 0, 0xa0 + index as u8, &data, &mut frames);
        frames.put(frame, &cache);
    }
    file.unmap_shared_write(3);
    assert_eq!(cache.write_back(&file, 0, u64::MAX, &frames, &mut disk), Ok(4));
    // No entry has them: nothing can write them without faulting, so they're clean and
    // can be taken back. The page still mapped writable stays dirty.
    assert_eq!(file.dirty_pages(), 1);
    assert_eq!(cache.drop_idle(&mut frames), 3);
    assert_eq!(disk.data[3 * PAGE_SIZE], 0xa3);
    // Mapped again and written, a page is dirty again, and written again.
    let frame = write_mapped(&cache, &file, 2, 0, 0xb2, &disk.data.clone(), &mut frames);
    frames.put(frame, &cache);
    assert_eq!(cache.write_back(&file, 1, 3, &frames, &mut disk), Ok(1));
    assert_eq!((disk.data[2 * PAGE_SIZE], file.dirty_pages()), (0xb2, 1));
    frames.put(kept, &cache);
    file.unmap_shared_write(1);
}

#[test]
fn write_back_refused_before_taking_the_bytes_is_eio() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(PAGE_SIZE, 13);
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    file.map_shared_write(1).unwrap();
    let frame = write_mapped(&cache, &file, 0, 0, 1, &data, &mut frames);
    frames.put(frame, &cache);
    file.unmap_shared_write(1);
    // As the tree does for a read-only mount or file: refused before `fill` is called.
    struct Refusing;
    impl PageWriter for Refusing {
        fn write(&mut self, _key: FileKey, _offset: u64, _fill: &mut dyn FnMut(&mut [u8]) -> usize) -> bool {
            false
        }
    }
    assert_eq!(cache.write_back(&file, 0, u64::MAX, &frames, &mut Refusing), Err(WriteBackError));
    assert_eq!((file.dirty_pages(), cache.reclaimable()), (1, 0), "still dirty, and kept");
}

#[test]
fn failed_change_keeps_dirty_and_mapped_pages() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(100);
    let data = contents(3 * PAGE_SIZE, 14);
    let mut disk = Disk { data: data.clone(), fail: false, writes: 0 };
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    file.map_shared_write(3).unwrap();
    // Page 0 dirty and mapped writable; page 1 clean and mapped; page 2 clean, unmapped.
    let dirty = write_mapped(&cache, &file, 0, 7, 0xd0, &data, &mut frames);
    let mapped = fault(&cache, &file, 1, &data, &mut frames).unwrap();
    let idle = fault(&cache, &file, 2, &data, &mut frames).unwrap();
    frames.put(idle, &cache);
    // A write over all three fails part way.
    let mut told = Vec::new();
    cache.may_change(KEY).unwrap();
    cache.changed(KEY, Change::Failed { offset: 0, length: 3 * PAGE_SIZE as u64, size: data.len() as u64 }, &mut frames, &mut |space| told.push(space));
    assert!(told.is_empty(), "nothing an entry has went");
    assert_eq!((cache.pages(), file.dirty_pages()), (2, 1), "only the idle clean page went");
    let mut sync = |_frame: usize| {};
    assert_eq!(cache.find_page(&file, 0, false, &mut frames, &mut sync), Lookup::Hit(dirty));
    frames.put(dirty, &cache);
    assert_eq!(byte(dirty, 7), 0xd0, "what the mapping wrote is kept");
    // The writable mapping's first write to the clean page still has somewhere to go.
    assert!(cache.mark_dirty(&file, 1, mapped));
    frames.put(dirty, &cache);
    frames.put(mapped, &cache);
    file.unmap_shared_write(3);
    assert_eq!(cache.write_back(&file, 0, u64::MAX, &frames, &mut disk), Ok(2));
    assert_eq!(disk.data[7], 0xd0);
    assert_eq!(file.dirty_pages(), 0);
}

#[test]
fn first_dirty_stops_at_the_end_asked() {
    let cache = PageCache::new(forget);
    let mut frames = TestFrames::new(200);
    let data = contents(100 * PAGE_SIZE, 15);
    let (file, _) = cache.insert_file(KEY, data.len() as u64).unwrap();
    file.map_shared_write(100).unwrap();
    let pages: Vec<usize> = [3u64, 40, 90].iter().map(|&index| write_mapped(&cache, &file, index, 0, 1, &data, &mut frames)).collect();
    let state = file.file().state.lock();
    assert_eq!(state.first_dirty(0, u64::MAX), Some(3));
    assert_eq!(state.first_dirty(4, u64::MAX), Some(40));
    assert_eq!(state.first_dirty(4, 40), None, "the end isn't in the range");
    assert_eq!(state.first_dirty(41, 91), Some(90));
    assert_eq!(state.first_dirty(91, u64::MAX), None);
    drop(state);
    for frame in pages {
        frames.put(frame, &cache);
    }
    file.unmap_shared_write(100);
}
