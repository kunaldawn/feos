use super::*;
use feos_abi::{
    MAX_VMAS, OBJECT_HEAP_LIMIT, USER_ARGS_SIZE, USER_BASE, USER_END, USER_IMAGE_END, USER_MEMORY_LIMIT, USER_MMAP_BASE,
    USER_MMAP_END, USER_STACK_GUARD, USER_STACK_SIZE, USER_STACK_TOP,
};
use std::alloc::{alloc, dealloc, Layout as HeapLayout};
use std::collections::HashMap;

/// Hands out page-aligned host memory, counts references as `feos-frames` does, and
/// notices anything given back wrongly or never given back.
struct Pool {
    /// Each live frame's map and pin counts.
    live: HashMap<usize, (u32, u32)>,
    limit: usize,
    max_maps: u32,
    max_pins: u32,
}

impl Pool {
    fn limited(limit: usize) -> Self {
        Self { live: HashMap::new(), limit, max_maps: 1 << 20, max_pins: 511 }
    }
    fn layout() -> HeapLayout {
        HeapLayout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap()
    }
    fn release(&mut self, frame: usize) {
        self.live.remove(&frame);
        // SAFETY: it was allocated with this layout, and is gone from the map.
        unsafe { dealloc(frame as *mut u8, Self::layout()) };
    }
}

impl FrameSource for Pool {
    fn alloc(&mut self) -> Option<usize> {
        if self.live.len() >= self.limit {
            return None;
        }
        // SAFETY: the layout has a size.
        let frame = unsafe { alloc(Self::layout()) } as usize;
        assert!(self.live.insert(frame, (1, 0)).is_none());
        // Junk, so that anything relying on a cleared frame fails.
        // SAFETY: just allocated.
        unsafe { core::ptr::write_bytes(frame as *mut u8, 0xa5, PAGE_SIZE) };
        Some(frame)
    }
    fn free(&mut self, frame: usize) {
        let counts = self.live.get(&frame).copied();
        assert_eq!(counts, Some((1, 0)), "freed {frame:#x} twice, never had it, or while it's shared");
        self.release(frame);
    }
    fn get(&mut self, frame: usize) -> bool {
        let counts = self.live.get_mut(&frame).expect("a reference to a frame that's gone");
        assert!(counts.0 > 0);
        if counts.0 == self.max_maps {
            return false;
        }
        counts.0 += 1;
        true
    }
    fn put(&mut self, frame: usize) {
        let counts = self.live.get_mut(&frame).expect("put of a frame that's gone");
        assert!(counts.0 > 0, "put of {frame:#x} with no references");
        counts.0 -= 1;
        if *counts == (0, 0) {
            self.release(frame);
        }
    }
    fn put_shared(&mut self, frame: usize) -> bool {
        let counts = self.live.get_mut(&frame).expect("put of a frame that's gone");
        assert!(counts.0 > 0, "put of {frame:#x} with no references");
        if counts.0 == 1 {
            return false;
        }
        counts.0 -= 1;
        true
    }
    fn pins(&self, frame: usize) -> u32 {
        self.live.get(&frame).map_or(0, |counts| counts.1)
    }
    fn maps(&self, frame: usize) -> u32 {
        self.live.get(&frame).map_or(0, |counts| counts.0)
    }
    fn pin(&mut self, frame: usize) -> bool {
        let counts = self.live.get_mut(&frame).expect("pin of a frame that's gone");
        assert!(counts.0 > 0);
        if counts.1 == self.max_pins {
            return false;
        }
        counts.1 += 1;
        true
    }
    fn unpin(&mut self, frame: usize) {
        let counts = self.live.get_mut(&frame).expect("unpin of a frame that's gone");
        assert!(counts.1 > 0);
        counts.1 -= 1;
        if *counts == (0, 0) {
            self.release(frame);
        }
    }
    fn free_count(&self) -> usize {
        self.limit.saturating_sub(self.live.len())
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let leaked = self.live.len();
        for frame in self.live.keys().copied().collect::<Vec<_>>() {
            self.release(frame);
        }
        if !std::thread::panicking() {
            assert_eq!(leaked, 0, "{leaked} frames leaked");
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
const RW: Prot = Prot::READ_WRITE;
const RO: Prot = Prot::READ;
const LOW: usize = USER_MMAP_BASE;
const GIB: usize = 1 << 30;

fn layout() -> mm::Layout {
    mm::Layout {
        window: (USER_BASE, USER_END),
        area: (USER_MMAP_BASE, USER_MMAP_END),
        page_limit: USER_MEMORY_LIMIT / PAGE_SIZE,
        max_vmas: MAX_VMAS,
        object_heap_limit: OBJECT_HEAP_LIMIT,
        space: 1,
    }
}

/// The cache's view of a test's frames: the same pool the space's come from.
struct CacheView<'a>(&'a mut dyn FrameSource);

impl feos_pagecache::CacheFrames for CacheView<'_> {
    fn alloc(&mut self) -> Option<usize> {
        self.0.alloc()
    }
    fn get(&mut self, frame: usize) -> Option<u32> {
        // The count after isn't known here, and only matters to idle accounting.
        self.0.get(frame).then_some(3)
    }
    fn drop_page(&mut self, frame: usize) -> bool {
        self.0.put(frame);
        false
    }
    fn refs(&self, _frame: usize) -> u32 {
        2
    }
    fn free_count(&self) -> usize {
        self.0.free_count()
    }
}

/// The cache's view of the pool itself, which knows the counts.
struct PoolView<'a>(&'a mut Pool);

impl feos_pagecache::CacheFrames for PoolView<'_> {
    fn alloc(&mut self) -> Option<usize> {
        self.0.alloc()
    }
    fn get(&mut self, frame: usize) -> Option<u32> {
        self.0.get(frame).then(|| self.0.live[&frame].0)
    }
    fn drop_page(&mut self, frame: usize) -> bool {
        let last = self.0.live[&frame] == (1, 0);
        self.0.put(frame);
        last
    }
    fn refs(&self, frame: usize) -> u32 {
        self.0.live.get(&frame).map_or(0, |counts| counts.0)
    }
    fn free_count(&self) -> usize {
        self.0.free_count()
    }
}

fn forget(_key: FileKey) {}

/// Files for a test's spaces: their bytes, read into a page cache as the kernel does.
struct Files {
    cache: feos_pagecache::PageCache,
    data: HashMap<FileKey, Vec<u8>>,
    /// Pages read from the files, as opposed to found in the cache.
    reads: usize,
    /// Frames the instruction cache was synced for.
    synced: Vec<usize>,
}

impl Files {
    fn new() -> Self {
        Self { cache: feos_pagecache::PageCache::new(forget), data: HashMap::new(), reads: 0, synced: Vec::new() }
    }

    /// A file of `pages` pages and a few bytes more, each byte from its offset and `seed`.
    fn add(&mut self, ino: u64, bytes: usize, seed: u8) -> FileRef {
        let key = FileKey { dev: 1, ino };
        let data: Vec<u8> = (0..bytes).map(|at| (at / 7) as u8 ^ seed).collect();
        self.data.insert(key, data);
        self.cache.insert_file(key, bytes as u64).unwrap().0
    }
}

impl mm::FilePages for Files {
    fn page(&mut self, file: &FileRef, index: u64, exec: bool, frames: &mut dyn FrameSource) -> Result<usize, FaultError> {
        let synced = &mut self.synced;
        let mut sync = |frame: usize| synced.push(frame);
        let mut view = CacheView(frames);
        loop {
            let change = match self.cache.find_page(file, index, exec, &mut view, &mut sync) {
                feos_pagecache::Lookup::Hit(frame) => return Ok(frame),
                feos_pagecache::Lookup::Miss { change } => change,
                _ => return Err(FaultError::Bus),
            };
            let frame = self.cache.take_frame(&mut view, 0).ok_or(FaultError::Bus)?;
            let data = &self.data[&file.key()];
            let at = (index as usize * PAGE_SIZE).min(data.len());
            let page = &data[at..(at + PAGE_SIZE).min(data.len())];
            // SAFETY: a frame of the pool's, a page long.
            unsafe {
                core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE);
                core::ptr::copy_nonoverlapping(page.as_ptr(), frame as *mut u8, page.len());
            }
            self.reads += 1;
            match self.cache.insert_page(file, index, frame, change, exec, true, &mut view, &mut sync) {
                Ok(Some(frame)) => return Ok(frame),
                Err(feos_pagecache::Lookup::Miss { .. }) => continue,
                _ => return Err(FaultError::Bus),
            }
        }
    }

    fn dirty(&mut self, file: &FileRef, index: u64, frame: usize) -> Result<(), FaultError> {
        if self.cache.mark_dirty(file, index, frame) {
            Ok(())
        } else {
            Err(FaultError::Bus)
        }
    }
}

/// A machine: frames, its promises, what was flushed, and the zero frame.
struct World {
    pool: Pool,
    promises: Promises,
    flushed: Vec<usize>,
    zero: usize,
    files: Files,
}

impl World {
    fn new() -> Self {
        Self::limited(usize::MAX)
    }

    fn limited(frames: usize) -> Self {
        let mut pool = Pool::limited(frames);
        let zero = pool.alloc().unwrap();
        // SAFETY: just allocated, a page long.
        unsafe { core::ptr::write_bytes(zero as *mut u8, 0, PAGE_SIZE) };
        Self { pool, promises: Promises::new(0), flushed: Vec::new(), zero, files: Files::new() }
    }

    fn mm(&mut self) -> Mm {
        self.mm_with(BITS, layout())
    }

    fn mm_with(&mut self, bits: PageBits, layout: mm::Layout) -> Mm {
        Mm::new(&mut self.pool, bits, layout, self.zero).unwrap()
    }

    fn with<R>(&mut self, run: impl FnOnce(&mut Ctx<'_>) -> R) -> R {
        let flushed = &mut self.flushed;
        let mut flush = |address: usize| flushed.push(address);
        let mut ctx = Ctx { frames: &mut self.pool, promises: &self.promises, flush: &mut flush, files: &mut self.files };
        run(&mut ctx)
    }

    fn map(&mut self, mm: &mut Mm, request: MapRequest) -> Result<usize, MmError> {
        self.with(|ctx| mm.map(&request, ctx))
    }

    fn anywhere(&mut self, mm: &mut Mm, length: usize, prot: Prot) -> usize {
        self.map(mm, MapRequest::anonymous(length, prot)).unwrap()
    }

    fn write(&mut self, mm: &mut Mm, address: usize, data: &[u8]) {
        self.with(|ctx| mm.fault_in(address, data.len(), true, ctx)).unwrap();
        mm.space().copy_out(address, data).unwrap();
    }

    fn read(&mut self, mm: &mut Mm, address: usize, length: usize) -> Vec<u8> {
        let mut out = vec![0x5a; length];
        self.with(|ctx| mm.fault_in(address, length, false, ctx)).unwrap();
        mm.space().copy_in(address, &mut out).unwrap();
        out
    }

    /// Frames in use beyond the zero frame.
    fn in_use(&self) -> usize {
        self.pool.live.len() - 1
    }

    fn done(mut self, mm: Mm) {
        mm.destroy(&mut self.pool, &self.promises);
        // The cache lets its pages go, as the kernel's would once nothing maps them.
        self.files.cache.drop_idle(&mut PoolView(&mut self.pool));
        assert_eq!(self.promises.held(), 0, "promises left behind");
        assert_eq!(self.promises.underflows(), 0);
        let zero = self.zero;
        self.pool.free(zero);
    }
}

/// The counts that must always agree: what the space holds in promises is what the
/// machine has promised, and the limit's count is the sum of its parts.
fn consistent(world: &World, mm: &Mm) {
    assert_eq!(mm.unbacked(), world.promises.held());
    assert_eq!(mm.committed(), mm.charged() + mm.owned() + mm.tables() + mm.held());
    assert!(mm.unbacked() <= mm.charged());
}

#[test]
fn data_comes_back_across_a_page_boundary() {
    let mut world = World::new();
    let mut mm = world.mm();
    let at = world.anywhere(&mut mm, 2 * PAGE_SIZE, RW);
    let data: Vec<u8> = (0..300).map(|n| n as u8).collect();
    world.write(&mut mm, at + PAGE_SIZE - 100, &data);
    assert_eq!(world.read(&mut mm, at + PAGE_SIZE - 100, 300), data);
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn new_pages_are_zero() {
    let mut world = World::new();
    let mut mm = world.mm();
    let at = world.anywhere(&mut mm, 2 * PAGE_SIZE, RW);
    // Written pages are cleared of the pool's junk, and read pages are the zero frame.
    world.write(&mut mm, at, &[1]);
    assert_eq!(world.read(&mut mm, at + 1, 64), vec![0u8; 64]);
    assert_eq!(world.read(&mut mm, at + PAGE_SIZE, 64), vec![0u8; 64]);
    world.done(mm);
}

#[test]
fn wild_addresses_and_lengths_are_refused() {
    let mut world = World::new();
    let mut mm = world.mm();
    let at = world.anywhere(&mut mm, PAGE_SIZE, RW);
    world.write(&mut mm, at, &[1]);
    let mut small = [0u8; 8];
    for address in [
        0,
        1,
        0x4020_0000, // the kernel, on AArch64
        0x0200_0000, // the kernel, on x86_64
        0x0900_0000, // a UART
        USER_BASE - 1,
        USER_END,
        USER_END - 1,
        usize::MAX,
        usize::MAX - 7,
        1 << 63,
        at + PAGE_SIZE, // the guard page after the mapping
    ] {
        assert_eq!(mm.space().copy_in(address, &mut small), Err(BadAddress), "{address:#x}");
        assert_eq!(mm.space().copy_out(address, &small), Err(BadAddress), "{address:#x}");
        assert_eq!(mm.space().translate(address, false), Err(BadAddress), "{address:#x}");
        let fault = world.with(|ctx| mm.fault(address, Access::default(), ctx));
        assert_eq!(fault, Err(FaultError::Segv), "{address:#x}");
    }
    // Starts good, and runs off the mapped page.
    assert_eq!(mm.space().copy_in(at + PAGE_SIZE - 4, &mut small), Err(BadAddress));
    assert_eq!(world.with(|ctx| mm.fault_in(at + PAGE_SIZE - 4, 8, false, ctx)), Err(FaultError::Segv));
    world.done(mm);
}

#[test]
fn lengths_that_wrap_are_refused_without_touching_anything() {
    let mut world = World::new();
    let mut mm = world.mm();
    world.anywhere(&mut mm, PAGE_SIZE, RW);
    let space = mm.space();
    assert!(!space.in_window(USER_BASE, usize::MAX));
    assert!(!space.in_window(usize::MAX, 2));
    assert!(!space.in_window(USER_BASE, USER_END - USER_BASE + 1));
    assert!(space.in_window(USER_BASE, USER_END - USER_BASE));
    assert!(space.validate(USER_BASE, usize::MAX, false).is_err());
    assert!(space.validate(usize::MAX - 2, 8, true).is_err());
    assert!(space.validate(USER_BASE + 1, USER_END - USER_BASE, false).is_err());
    assert!(space.validate(0, 0, false).is_ok());
    assert_eq!(world.with(|ctx| mm.fault_in(LOW, usize::MAX, true, ctx)), Err(FaultError::Segv));
    assert!(!mm.covers(LOW, usize::MAX));
    world.done(mm);
}

/// An image segment, loaded by the kernel and run.
fn image(length: usize, prot: Prot, at: usize) -> MapRequest {
    MapRequest { place: Place::Exact(at), image: true, ..MapRequest::anonymous(length, prot) }
}

#[test]
fn a_write_needs_a_writable_page() {
    for bits in [BITS, ARM_BITS] {
        let mut world = World::new();
        let mut mm = world.mm_with(bits, layout());
        world.map(&mut mm, image(PAGE_SIZE, Prot::READ_EXEC, USER_BASE)).unwrap();
        world.map(&mut mm, image(PAGE_SIZE, RW, USER_BASE + PAGE_SIZE)).unwrap();
        assert_eq!(mm.space().copy_out(USER_BASE, &[1]), Err(BadAddress));
        assert_eq!(world.with(|ctx| mm.fault_in(USER_BASE, 1, true, ctx)), Err(FaultError::Segv));
        // Good in one page, refused in the next, and nothing's written.
        assert_eq!(mm.space().copy_out(USER_BASE + PAGE_SIZE - 1, &[9, 9]), Err(BadAddress));
        mm.space().copy_out(USER_BASE + PAGE_SIZE, &[1]).unwrap();
        assert_eq!(world.read(&mut mm, USER_BASE, 1), [0]);
        // The loader can still fill read-only pages.
        // SAFETY: a test's own space, and the image's own frame.
        unsafe { mm.space().load(USER_BASE, &[42]).unwrap() };
        assert_eq!(world.read(&mut mm, USER_BASE, 1), [42]);
        // An image is populated, a frame of its own per page, read-only ones too.
        assert_eq!((mm.owned(), mm.charged(), mm.unbacked()), (1, 1, 0));
        consistent(&world, &mm);
        world.done(mm);
    }
}

#[test]
fn the_encoding_says_what_was_asked() {
    let rw = PageAccess { write: true, execute: false };
    let ro = PageAccess { write: false, execute: false };
    let rx = PageAccess { write: false, execute: true };
    assert_ne!(BITS.leaf(rw) & BITS.writable, 0);
    assert_eq!(BITS.leaf(ro) & BITS.writable, 0);
    assert_ne!(BITS.leaf(rw) & BITS.no_execute, 0);
    assert_eq!(BITS.leaf(rx) & BITS.no_execute, 0);
    assert_ne!(ARM_BITS.leaf(ro) & ARM_BITS.read_only, 0);
    assert_eq!(ARM_BITS.leaf(rw) & ARM_BITS.read_only, 0);
    assert!(ARM_BITS.is_writable(ARM_BITS.leaf(rw)));
    assert!(!ARM_BITS.is_writable(ARM_BITS.leaf(rx)));
    assert!(BITS.is_writable(BITS.leaf(rw)));
    assert!(!BITS.is_writable(BITS.leaf(rx)));
}

#[test]
fn mapping_twice_or_outside_the_window_fails() {
    let mut world = World::new();
    let mut mm = world.mm();
    world.map(&mut mm, image(PAGE_SIZE, RW, USER_BASE)).unwrap();
    assert_eq!(world.map(&mut mm, image(PAGE_SIZE, RW, USER_BASE)), Err(MmError::Exists));
    assert_eq!(world.map(&mut mm, image(PAGE_SIZE, RW, USER_BASE + 1)), Err(MmError::Invalid));
    assert_eq!(world.map(&mut mm, image(1, RW, USER_BASE + PAGE_SIZE)), Err(MmError::Invalid));
    assert_eq!(world.map(&mut mm, image(0, RW, USER_BASE + PAGE_SIZE)), Err(MmError::Invalid));
    assert_eq!(world.map(&mut mm, image(PAGE_SIZE, RW, USER_BASE - PAGE_SIZE)), Err(MmError::Invalid));
    assert_eq!(world.map(&mut mm, image(PAGE_SIZE, RW, USER_END)), Err(MmError::Invalid));
    assert_eq!(world.map(&mut mm, image(2 * PAGE_SIZE, RW, USER_END - PAGE_SIZE)), Err(MmError::Invalid));
    assert_eq!(mm.vmas().len(), 1);
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn running_out_of_frames_midway_leaves_nothing_mapped() {
    let mut world = World::limited(16);
    let mut mm = world.mm();
    let before = world.in_use();
    let request = MapRequest { populate: true, noreserve: true, ..MapRequest::anonymous(64 * PAGE_SIZE, RW) };
    assert_eq!(world.map(&mut mm, request), Err(MmError::NoMemory));
    assert_eq!(world.in_use(), before);
    assert_eq!((mm.vmas().len(), mm.committed()), (0, 0));
    assert_eq!(mm.space().translate(LOW, false), Err(BadAddress));
    // Charged, it's refused before anything happens: there's nothing to promise it with.
    let request = MapRequest { populate: true, ..MapRequest::anonymous(64 * PAGE_SIZE, RW) };
    assert_eq!(world.map(&mut mm, request), Err(MmError::NoMemory));
    assert_eq!(world.in_use(), before);
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn the_page_limit_is_kept() {
    let mut world = World::new();
    let mut mm = world.mm_with(BITS, mm::Layout { page_limit: 8, ..layout() });
    // Four pages and their two tables fit; four more and no more tables don't.
    world.anywhere(&mut mm, 4 * PAGE_SIZE, RW);
    assert_eq!(mm.committed(), 6);
    assert_eq!(world.map(&mut mm, MapRequest::anonymous(4 * PAGE_SIZE, RW)), Err(MmError::NoMemory));
    world.anywhere(&mut mm, 2 * PAGE_SIZE, RW);
    // Uncharged is free until it's touched; then it counts too.
    let none = world.anywhere(&mut mm, 1 << 33, Prot::NONE);
    let sparse = MapRequest { noreserve: true, ..MapRequest::anonymous(4 * PAGE_SIZE, RW) };
    let sparse = world.map(&mut mm, sparse).unwrap();
    assert_eq!(world.with(|ctx| mm.fault(sparse, Access { write: true, execute: false }, ctx)), Err(FaultError::Bus));
    assert_eq!(world.with(|ctx| mm.protect(none, PAGE_SIZE, RW, ctx)), Err(MmError::NoMemory));
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn unmapping_frees_and_holes_are_fine() {
    let mut world = World::new();
    let mut mm = world.mm();
    let at = world.anywhere(&mut mm, 4 * PAGE_SIZE, RW);
    world.write(&mut mm, at, &[1; 4 * PAGE_SIZE]);
    let before = world.in_use();
    world.flushed.clear();
    world.with(|ctx| mm.unmap(at + PAGE_SIZE, 8 * PAGE_SIZE, ctx)).unwrap();
    assert_eq!(&world.flushed[..3], [at + PAGE_SIZE, at + 2 * PAGE_SIZE, at + 3 * PAGE_SIZE]);
    assert_eq!(world.in_use(), before - 3);
    assert_eq!(mm.charged(), 1);
    assert_eq!(mm.space().translate(at + PAGE_SIZE, false), Err(BadAddress));
    mm.space().translate(at, false).unwrap();
    // A range with nothing in it at all.
    world.with(|ctx| mm.unmap(LOW + 64 * GIB / 64, PAGE_SIZE, ctx)).unwrap();
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn destroying_frees_everything_across_the_tree() {
    let mut world = World::new();
    let mut mm = world.mm();
    // Pages that need their own tables at every level.
    for address in [LOW, LOW + (2 << 20), LOW + GIB, USER_MMAP_END - PAGE_SIZE] {
        let request = MapRequest { place: Place::Fixed(address), ..MapRequest::anonymous(PAGE_SIZE, RW) };
        world.map(&mut mm, request).unwrap();
        world.write(&mut mm, address, &[3]);
    }
    let request = MapRequest { shared: true, ..MapRequest::anonymous(PAGE_SIZE, RW) };
    let shared = world.map(&mut mm, request).unwrap();
    world.write(&mut mm, shared, &[4]);
    world.done(mm);
}

#[test]
fn the_kernels_part_of_a_top_table_is_left_alone() {
    let mut world = World::new();
    let mut mm = world.mm();
    world.anywhere(&mut mm, PAGE_SIZE, RW);
    // What a port does: the kernel's mappings in the same top table, at indices
    // outside the window. Those aren't frames from this pool: freeing one fails.
    // SAFETY: the root is a page, and index 1 is inside it.
    unsafe { write_entry(mm.root(), 1, 0x4000_0000 | BITS.table) };
    world.done(mm);
}

#[test]
fn lent_frames_are_not_freed() {
    let mut world = World::new();
    let lent = world.pool.alloc().unwrap();
    let mut mm = world.mm();
    let at = world.with(|ctx| mm.map_lent(lent, PAGE_SIZE, ctx)).unwrap();
    mm.space().copy_out(at, &[5]).unwrap();
    // SAFETY: the pool's frame, still live.
    assert_eq!(unsafe { *(lent as *const u8) }, 5);
    // The program's memory as much as any: owned, so within its limit.
    assert_eq!((mm.charged(), mm.owned()), (0, 1));
    consistent(&world, &mm);
    // No call may change it.
    assert_eq!(world.with(|ctx| mm.unmap(at, PAGE_SIZE, ctx)), Err(MmError::Invalid));
    assert_eq!(world.with(|ctx| mm.protect(at, PAGE_SIZE, RO, ctx)), Err(MmError::Invalid));
    let over = MapRequest { place: Place::Fixed(at), ..MapRequest::anonymous(PAGE_SIZE, RW) };
    assert_eq!(world.map(&mut mm, over), Err(MmError::Invalid));
    // A copy through it needs no pin.
    let pinned = mm.pin(at, true, &mut world.pool).unwrap();
    mm.unpin(pinned, &mut world.pool);
    mm.destroy(&mut world.pool, &world.promises);
    // Still the lender's.
    world.pool.free(lent);
    let mm = world.mm();
    world.done(mm);
}

#[test]
fn lent_frames_count_against_the_limit() {
    let mut world = World::new();
    let lent = world.pool.alloc().unwrap();
    let mut mm = world.mm_with(BITS, mm::Layout { page_limit: 4, ..layout() });
    // Four lent pages and their tables are more than four pages.
    assert_eq!(world.with(|ctx| mm.map_lent(lent, 4 * PAGE_SIZE, ctx)), Err(MmError::NoMemory));
    assert_eq!((mm.vmas().len(), mm.committed()), (0, 0));
    world.pool.free(lent);
    world.done(mm);
}

#[test]
fn held_frames_count_against_the_limit() {
    let mut world = World::new();
    let mut mm = world.mm_with(BITS, mm::Layout { page_limit: 8, ..layout() });
    assert!(mm.hold(6));
    assert_eq!(mm.committed(), 6);
    // What's held leaves the space less room, and more than the rest is refused whole.
    assert!(!mm.hold(3));
    assert_eq!(world.with(|ctx| mm.map(&MapRequest::anonymous(4 * PAGE_SIZE, RW), ctx)), Err(MmError::NoMemory));
    consistent(&world, &mm);
    mm.unhold(6);
    assert_eq!(mm.committed(), 0);
    assert!(world.with(|ctx| mm.map(&MapRequest::anonymous(4 * PAGE_SIZE, RW), ctx)).is_ok());
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn futex_words_must_be_aligned_and_mapped() {
    let mut world = World::new();
    let mut mm = world.mm();
    let at = world.anywhere(&mut mm, PAGE_SIZE, RW);
    world.write(&mut mm, at, &[0; 8]);
    assert!(mm.space().translate_word(at + 4).is_ok());
    assert_eq!(mm.space().translate_word(at + 2), Err(BadAddress));
    assert_eq!(mm.space().translate_word(at + PAGE_SIZE), Err(BadAddress));
    assert_eq!(mm.space().translate_word(0x4020_0000), Err(BadAddress));
    world.done(mm);
}

#[test]
fn repeated_create_and_destroy_leaks_nothing() {
    let mut world = World::new();
    for round in 0..100 {
        let mut mm = world.mm();
        let at = world.anywhere(&mut mm, 8 * PAGE_SIZE + round * PAGE_SIZE, RW);
        world.write(&mut mm, at, &[1; 3 * PAGE_SIZE]);
        let request = MapRequest { place: Place::Fixed(USER_MMAP_END - 4 * PAGE_SIZE), ..MapRequest::anonymous(4 * PAGE_SIZE, RW) };
        world.map(&mut mm, request).unwrap();
        world.with(|ctx| mm.unmap(at, PAGE_SIZE, ctx)).unwrap();
        mm.destroy(&mut world.pool, &world.promises);
        assert_eq!(world.in_use(), 0);
        assert_eq!(world.promises.held(), 0);
    }
    let mm = world.mm();
    world.done(mm);
}

#[test]
fn noreserve_without_frames_is_an_answer() {
    let mut world = World::limited(4);
    let mut mm = world.mm();
    let request = MapRequest { noreserve: true, ..MapRequest::anonymous(8 * PAGE_SIZE, RW) };
    let at = world.map(&mut mm, request).unwrap();
    // The zero frame and the root are two; the tables take the other two.
    assert_eq!(world.with(|ctx| mm.fault(at, Access { write: false, execute: false }, ctx)), Ok(()));
    assert_eq!(world.with(|ctx| mm.fault(at, Access { write: true, execute: false }, ctx)), Err(FaultError::Bus));
    assert_eq!(world.with(|ctx| mm.fault_in(at, PAGE_SIZE, true, ctx)), Err(FaultError::Bus));
    assert_eq!(mm.unbacked(), 0);
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn protection_changes_and_contents_survive_no_access() {
    for bits in [BITS, ARM_BITS] {
        let mut world = World::new();
        let mut mm = world.mm_with(bits, layout());
        let at = world.anywhere(&mut mm, 4 * PAGE_SIZE, RW);
        world.write(&mut mm, at, &[42]);
        world.flushed.clear();
        world.with(|ctx| mm.protect(at, 4 * PAGE_SIZE, RO, ctx)).unwrap();
        assert_eq!(world.flushed, [at]);
        assert_eq!(mm.space().copy_out(at, &[1]), Err(BadAddress));
        assert_eq!(world.with(|ctx| mm.fault(at, Access { write: true, execute: false }, ctx)), Err(FaultError::Segv));
        // A page touched now gets the new protection.
        assert_eq!(world.read(&mut mm, at + PAGE_SIZE, 1), [0]);
        assert_eq!(mm.space().copy_out(at + PAGE_SIZE, &[1]), Err(BadAddress));
        // No access at all: not readable, and not given a frame by a fault either.
        world.with(|ctx| mm.protect(at, 4 * PAGE_SIZE, Prot::NONE, ctx)).unwrap();
        let mut back = [0u8; 1];
        assert_eq!(mm.space().copy_in(at, &mut back), Err(BadAddress));
        assert_eq!(world.with(|ctx| mm.fault(at, Access::default(), ctx)), Err(FaultError::Segv));
        assert_eq!(world.with(|ctx| mm.fault(at + 2 * PAGE_SIZE, Access::default(), ctx)), Err(FaultError::Segv));
        // And back, with what was there.
        world.with(|ctx| mm.protect(at, 4 * PAGE_SIZE, RW, ctx)).unwrap();
        assert_eq!(world.read(&mut mm, at, 1), [42]);
        world.write(&mut mm, at + 3 * PAGE_SIZE, &[5]);
        world.write(&mut mm, at + PAGE_SIZE, &[6]);
        assert_eq!(mm.vmas().len(), 1, "the pieces join again");
        consistent(&world, &mm);
        world.done(mm);
    }
}

#[test]
fn protecting_a_hole_changes_nothing() {
    let mut world = World::new();
    let mut mm = world.mm();
    let first = world.anywhere(&mut mm, PAGE_SIZE, RW);
    let second = world.anywhere(&mut mm, PAGE_SIZE, RW);
    assert_eq!(second, first + 2 * PAGE_SIZE);
    world.write(&mut mm, first, &[1]);
    assert_eq!(world.with(|ctx| mm.protect(first, 3 * PAGE_SIZE, RO, ctx)), Err(MmError::NoMemory));
    mm.space().copy_out(first, &[1]).unwrap();
    assert_eq!(mm.vmas().len(), 2);
    world.done(mm);
}

#[test]
fn frames_kept_through_no_access_are_freed_both_ways() {
    let mut world = World::new();
    let mut mm = world.mm();
    let at = world.anywhere(&mut mm, 2 * PAGE_SIZE, RW);
    world.write(&mut mm, at, &[1; 2 * PAGE_SIZE]);
    world.with(|ctx| mm.protect(at, 2 * PAGE_SIZE, Prot::NONE, ctx)).unwrap();
    let before = world.in_use();
    world.flushed.clear();
    world.with(|ctx| mm.unmap(at, PAGE_SIZE, ctx)).unwrap();
    assert!(world.flushed.is_empty(), "nothing valid to flush");
    assert_eq!(world.in_use(), before - 1);
    // The other goes with the space; the pool notices a leak.
    world.done(mm);
}

#[test]
fn soft_entries_never_look_valid() {
    for bits in [BITS, ARM_BITS] {
        assert_eq!((SOFT | SOFT_FRAME) & bits.present, 0);
        assert_eq!((SOFT | SOFT_FRAME) & bits.address, 0);
    }
    let mut pool = Pool::limited(usize::MAX);
    let clash = PageBits { present: SOFT, ..BITS };
    assert!(AddressSpace::new(&mut pool, clash, (USER_BASE, USER_END)).is_none());
}

#[test]
fn a_bad_window_is_refused() {
    let mut pool = Pool::limited(usize::MAX);
    assert!(AddressSpace::new(&mut pool, BITS, (1, USER_END)).is_none());
    assert!(AddressSpace::new(&mut pool, BITS, (USER_BASE, USER_BASE)).is_none());
    assert!(AddressSpace::new(&mut pool, BITS, (0, 1 << 40)).is_none());
    let bad_area = mm::Layout { area: (USER_BASE - PAGE_SIZE, USER_MMAP_END), ..layout() };
    assert!(Mm::new(&mut pool, BITS, bad_area, 0).is_none());
}

#[test]
fn vma_first_fit_leaves_guards() {
    let mut world = World::new();
    let mut mm = world.mm();
    let a = world.anywhere(&mut mm, PAGE_SIZE, RW);
    let b = world.anywhere(&mut mm, 2 * PAGE_SIZE, RW);
    assert_eq!(a, LOW);
    assert_eq!(b, a + 2 * PAGE_SIZE, "a guard page between");
    assert!(mm.vmas().find(a + PAGE_SIZE).is_none());
    world.with(|ctx| mm.unmap(a, PAGE_SIZE, ctx)).unwrap();
    // The lowest gap that fits, guard and all.
    assert_eq!(world.anywhere(&mut mm, PAGE_SIZE, RW), a);
    let c = world.anywhere(&mut mm, 3 * PAGE_SIZE, RO);
    assert_eq!(c, b + 3 * PAGE_SIZE);
    // A gap too small for a page and its guards is passed over.
    world.with(|ctx| mm.unmap(b, 2 * PAGE_SIZE, ctx)).unwrap();
    let request = MapRequest { place: Place::Fixed(b), ..MapRequest::anonymous(PAGE_SIZE, RW) };
    world.map(&mut mm, request).unwrap();
    let d = world.anywhere(&mut mm, 2 * PAGE_SIZE, RW);
    assert_eq!(d, c + 4 * PAGE_SIZE);
    // Neighbours that touch and match are one VMA; these don't touch.
    assert_eq!(mm.vmas().len(), 4);
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn vma_munmap_splits() {
    let mut world = World::new();
    let mut mm = world.mm();
    let at = world.anywhere(&mut mm, 3 * PAGE_SIZE, RW);
    for page in 0..3 {
        world.write(&mut mm, at + page * PAGE_SIZE, &[page as u8 + 1]);
    }
    world.with(|ctx| mm.unmap(at + PAGE_SIZE, PAGE_SIZE, ctx)).unwrap();
    assert_eq!(mm.vmas().len(), 2);
    assert_eq!(world.read(&mut mm, at, 1), [1]);
    assert_eq!(world.read(&mut mm, at + 2 * PAGE_SIZE, 1), [3]);
    assert!(!mm.covers(at, 3 * PAGE_SIZE));
    assert_eq!(world.with(|ctx| mm.fault(at + PAGE_SIZE, Access::default(), ctx)), Err(FaultError::Segv));
    assert_eq!(mm.charged(), 2);
    // Both pieces, and the hole between, in one call.
    world.with(|ctx| mm.unmap(at, 3 * PAGE_SIZE, ctx)).unwrap();
    assert_eq!((mm.vmas().len(), mm.charged(), mm.tables()), (0, 0, 0));
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn fixed_replaces_atomically() {
    let mut world = World::limited(64);
    let mut mm = world.mm();
    let at = world.anywhere(&mut mm, 4 * PAGE_SIZE, RW);
    world.write(&mut mm, at, &[7; 4 * PAGE_SIZE]);
    let fixed = MapRequest { place: Place::Fixed(at + PAGE_SIZE), ..MapRequest::anonymous(2 * PAGE_SIZE, RW) };
    assert_eq!(world.map(&mut mm, fixed), Ok(at + PAGE_SIZE));
    assert_eq!(world.read(&mut mm, at + PAGE_SIZE, 2 * PAGE_SIZE), vec![0; 2 * PAGE_SIZE]);
    assert_eq!(world.read(&mut mm, at, 1), [7]);
    assert_eq!(world.read(&mut mm, at + 3 * PAGE_SIZE, 1), [7]);
    // The old and new are alike, so they're joined.
    assert_eq!(mm.vmas().len(), 1);
    // One that can't be promised changes nothing: the old pages are all still there.
    world.write(&mut mm, at, &[9; 4 * PAGE_SIZE]);
    let huge = MapRequest { place: Place::Fixed(at), ..MapRequest::anonymous(1024 * PAGE_SIZE, RW) };
    assert_eq!(world.map(&mut mm, huge), Err(MmError::NoMemory));
    assert_eq!(world.read(&mut mm, at, 4 * PAGE_SIZE), vec![9; 4 * PAGE_SIZE]);
    assert_eq!(mm.vmas().len(), 1);
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn fixed_noreplace_is_eexist() {
    let mut world = World::new();
    let mut mm = world.mm();
    let at = world.anywhere(&mut mm, 2 * PAGE_SIZE, RW);
    let over = |place| MapRequest { place, ..MapRequest::anonymous(PAGE_SIZE, RW) };
    assert_eq!(world.map(&mut mm, over(Place::FixedNoReplace(at + PAGE_SIZE))), Err(MmError::Exists));
    let free = at + 16 * PAGE_SIZE;
    assert_eq!(world.map(&mut mm, over(Place::FixedNoReplace(free))), Ok(free));
    assert_eq!(world.map(&mut mm, over(Place::Fixed(free + 1))), Err(MmError::Invalid));
    assert_eq!(world.map(&mut mm, over(Place::Fixed(USER_BASE))), Err(MmError::NoMemory), "below the mmap area");
    assert_eq!(world.map(&mut mm, over(Place::Fixed(USER_MMAP_END))), Err(MmError::NoMemory));
    assert_eq!(world.map(&mut mm, over(Place::Fixed(usize::MAX & !(PAGE_SIZE - 1)))), Err(MmError::NoMemory));
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn hint_used_when_free() {
    let mut world = World::new();
    let mut mm = world.mm();
    let hint = LOW + GIB + 123;
    let request = MapRequest { place: Place::Hint(hint), ..MapRequest::anonymous(PAGE_SIZE, RW) };
    assert_eq!(world.map(&mut mm, request), Ok(LOW + GIB));
    world.done(mm);
}

#[test]
fn hint_ignored_when_taken() {
    let mut world = World::new();
    let mut mm = world.mm();
    let hint = LOW + GIB;
    let request = MapRequest { place: Place::Hint(hint), ..MapRequest::anonymous(2 * PAGE_SIZE, RW) };
    assert_eq!(world.map(&mut mm, request.clone()), Ok(hint));
    let again = world.map(&mut mm, request.clone()).unwrap();
    assert!(again + 2 * PAGE_SIZE <= hint || again >= hint + 2 * PAGE_SIZE);
    assert_eq!(again, LOW, "the lowest gap instead");
    // Outside the mmap area is no hint at all.
    let outside = MapRequest { place: Place::Hint(USER_BASE), ..request };
    assert_eq!(world.map(&mut mm, outside), Ok(LOW + 3 * PAGE_SIZE));
    world.done(mm);
}

#[test]
fn prot_none_takes_no_charge() {
    let mut world = World::new();
    let mut mm = world.mm();
    // Twice the limit, reserved.
    let at = world.anywhere(&mut mm, 2 * USER_MEMORY_LIMIT, Prot::NONE);
    assert_eq!((mm.charged(), world.promises.held()), (0, 0));
    // Read-only is free too, and so is anything with `MAP_NORESERVE`.
    world.anywhere(&mut mm, GIB, RO);
    world.map(&mut mm, MapRequest { noreserve: true, ..MapRequest::anonymous(GIB, RW) }).unwrap();
    assert_eq!((mm.charged(), world.promises.held()), (0, 0));
    // v1's `MMAP` promises everything, whatever it says.
    let v1 = MapRequest { charge_all: true, ..MapRequest::anonymous(16 * PAGE_SIZE, Prot::NONE) };
    world.map(&mut mm, v1).unwrap();
    assert_eq!((mm.charged(), world.promises.held()), (16, 16));
    world.with(|ctx| mm.unmap(at, 2 * USER_MEMORY_LIMIT, ctx)).unwrap();
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn prot_none_builds_no_tables() {
    let mut world = World::new();
    let mut mm = world.mm();
    let before = world.in_use();
    let at = world.anywhere(&mut mm, 4 * GIB, Prot::NONE);
    assert_eq!((mm.tables(), world.in_use()), (0, before));
    // Made accessible without a charge, it still has no tables until a page is touched.
    let sparse = MapRequest { noreserve: true, ..MapRequest::anonymous(6 * GIB, Prot::NONE) };
    let sparse = world.map(&mut mm, sparse).unwrap();
    world.with(|ctx| mm.protect(sparse, 6 * GIB, RW, ctx)).unwrap();
    assert_eq!((mm.tables(), mm.charged()), (0, 0));
    world.write(&mut mm, sparse + 5 * GIB, &[1]);
    assert_eq!((mm.tables(), mm.owned()), (2, 1));
    world.with(|ctx| mm.unmap(at, 4 * GIB, ctx)).unwrap();
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn mprotect_to_write_charges() {
    let mut world = World::limited(4096);
    let mut mm = world.mm();
    let at = world.anywhere(&mut mm, 16 * PAGE_SIZE, RO);
    world.read(&mut mm, at, PAGE_SIZE);
    assert_eq!(mm.charged(), 0);
    world.with(|ctx| mm.protect(at, 16 * PAGE_SIZE, RW, ctx)).unwrap();
    assert_eq!((mm.charged(), world.promises.held()), (16, 16));
    // The page that was only read gets its own frame on its first write.
    world.write(&mut mm, at, &[1]);
    assert_eq!(world.promises.held(), 15);
    // Back to read-only keeps the charge: it's only given back with the pages.
    world.with(|ctx| mm.protect(at, 16 * PAGE_SIZE, RO, ctx)).unwrap();
    assert_eq!(mm.charged(), 16);
    // More than the machine can promise is refused, and changes nothing.
    let big = world.anywhere(&mut mm, 8192 * PAGE_SIZE, RO);
    assert_eq!(world.with(|ctx| mm.protect(big, 8192 * PAGE_SIZE, RW, ctx)), Err(MmError::NoMemory));
    assert_eq!(mm.space().copy_out(big, &[1]), Err(BadAddress));
    assert_eq!(world.with(|ctx| mm.fault(big, Access { write: true, execute: false }, ctx)), Err(FaultError::Segv));
    // A part of it is fine, and splits it.
    world.with(|ctx| mm.protect(big + PAGE_SIZE, PAGE_SIZE, RW, ctx)).unwrap();
    assert_eq!(mm.vmas().len(), 4);
    assert_eq!(mm.charged(), 17);
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn charged_range_promises_its_tables() {
    // Enough frames for the pages and their tables, and no more.
    let pages = 1024;
    let mut world = World::limited(1 + 1 + pages + 3);
    let mut mm = world.mm();
    // Across a 2 MiB line: one middle table and two leaves.
    let at = LOW + (2 << 20) - 512 * PAGE_SIZE;
    let request = MapRequest { place: Place::Fixed(at), ..MapRequest::anonymous(pages * PAGE_SIZE, RW) };
    world.map(&mut mm, request).unwrap();
    assert_eq!(mm.tables(), 3);
    // So every page can be given its frame, all of them.
    for page in 0..pages {
        assert_eq!(world.with(|ctx| mm.fault(at + page * PAGE_SIZE, Access { write: true, execute: false }, ctx)), Ok(()));
    }
    assert_eq!((mm.tables(), mm.unbacked(), world.pool.free_count()), (3, 0, 0));
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn empty_leaf_tables_freed() {
    let mut world = World::new();
    let mut mm = world.mm();
    let before = world.in_use();
    let mut placed = Vec::new();
    for gib in 1..4 {
        let at = LOW + gib * GIB + 4 * PAGE_SIZE;
        let request = MapRequest { place: Place::Fixed(at), ..MapRequest::anonymous(PAGE_SIZE, RW) };
        world.map(&mut mm, request).unwrap();
        world.write(&mut mm, at, &[1]);
        placed.push(at);
    }
    assert_eq!(mm.tables(), 6);
    // An unmap that leaves a table's other range still mapped keeps it.
    let neighbour = MapRequest { place: Place::Fixed(placed[0] + 8 * PAGE_SIZE), ..MapRequest::anonymous(PAGE_SIZE, RW) };
    let neighbour = world.map(&mut mm, neighbour).unwrap();
    world.flushed.clear();
    for at in &placed {
        world.with(|ctx| mm.unmap(*at, PAGE_SIZE, ctx)).unwrap();
    }
    assert_eq!(mm.tables(), 2);
    // Each table's span was flushed after its entry went.
    assert!(world.flushed.contains(&(LOW + 2 * GIB)));
    world.with(|ctx| mm.unmap(neighbour, PAGE_SIZE, ctx)).unwrap();
    assert_eq!((mm.tables(), world.in_use()), (0, before));
    // A sweep of a thousand mappings across the window leaves nothing either.
    for round in 0..1000 {
        let at = LOW + (round % 11) * GIB + (round % 512) * (2 << 20);
        let request = MapRequest { place: Place::Fixed(at), ..MapRequest::anonymous(PAGE_SIZE, RW) };
        world.map(&mut mm, request).unwrap();
        world.write(&mut mm, at, &[1]);
        world.with(|ctx| mm.unmap(at, PAGE_SIZE, ctx)).unwrap();
    }
    assert_eq!((mm.tables(), world.in_use()), (0, before));
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn dontneed_reads_zero() {
    let mut world = World::new();
    let mut mm = world.mm();
    let at = world.anywhere(&mut mm, 4 * PAGE_SIZE, RW);
    world.write(&mut mm, at, &[9; 4 * PAGE_SIZE]);
    let used = world.in_use();
    assert_eq!(world.promises.held(), 0);
    world.with(|ctx| mm.discard(at + PAGE_SIZE, 2 * PAGE_SIZE, ctx)).unwrap();
    // The frames go, and their promise comes back.
    assert_eq!(world.in_use(), used - 2);
    assert_eq!(world.promises.held(), 2);
    assert_eq!(world.read(&mut mm, at + PAGE_SIZE, 2 * PAGE_SIZE), vec![0; 2 * PAGE_SIZE]);
    assert_eq!(world.read(&mut mm, at, 1), [9]);
    assert_eq!(world.read(&mut mm, at + 3 * PAGE_SIZE, 1), [9]);
    // An unmapped page in the range: nothing happens.
    assert_eq!(world.with(|ctx| mm.discard(at, 5 * PAGE_SIZE, ctx)), Err(MmError::NoMemory));
    assert_eq!(world.read(&mut mm, at, 1), [9]);
    // The program's image keeps what was loaded.
    world.map(&mut mm, image(PAGE_SIZE, RO, USER_BASE)).unwrap();
    // SAFETY: the image's own frame.
    unsafe { mm.space().load(USER_BASE, &[3]).unwrap() };
    world.with(|ctx| mm.discard(USER_BASE, PAGE_SIZE, ctx)).unwrap();
    assert_eq!(world.read(&mut mm, USER_BASE, 1), [3]);
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn read_fault_maps_zero_frame() {
    let mut world = World::new();
    let mut mm = world.mm();
    let at = world.anywhere(&mut mm, 2 * PAGE_SIZE, RW);
    let used = world.in_use();
    world.with(|ctx| mm.fault(at, Access::default(), ctx)).unwrap();
    assert_eq!(mm.space().translate(at, false), Ok(world.zero));
    assert_eq!(mm.space().translate(at, true), Err(BadAddress), "the zero frame is never writable");
    assert_eq!((world.in_use(), world.promises.held()), (used, 2));
    world.flushed.clear();
    world.with(|ctx| mm.fault(at, Access { write: true, execute: false }, ctx)).unwrap();
    let own = mm.space().translate(at, true).unwrap();
    assert_ne!(own, world.zero);
    assert_eq!(world.flushed, [at], "the read-only entry was replaced");
    assert_eq!((world.in_use(), world.promises.held()), (used + 1, 1));
    // SAFETY: the zero frame, still a page.
    assert!(unsafe { core::slice::from_raw_parts(world.zero as *const u8, PAGE_SIZE) }.iter().all(|&byte| byte == 0));
    // Nothing anonymous runs, zero frame or not.
    assert_eq!(world.with(|ctx| mm.fault(at + PAGE_SIZE, Access { write: false, execute: true }, ctx)), Err(FaultError::Segv));
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn shared_anon_object_one_frame() {
    let mut world = World::new();
    let mut mm = world.mm();
    let used = world.in_use();
    let request = MapRequest { shared: true, ..MapRequest::anonymous(4 * PAGE_SIZE, RW) };
    let at = world.map(&mut mm, request).unwrap();
    assert_eq!((mm.charged(), world.promises.held()), (4, 4));
    world.write(&mut mm, at + PAGE_SIZE, &[8]);
    let frame = mm.space().translate(at + PAGE_SIZE, false).unwrap();
    // A read of a shared page is its object's frame, not the zero frame.
    world.read(&mut mm, at, 1);
    assert_ne!(mm.space().translate(at, false).unwrap(), world.zero);
    // Leaving this mapping keeps the page in the object: the same frame comes back.
    world.with(|ctx| mm.discard(at + PAGE_SIZE, PAGE_SIZE, ctx)).unwrap();
    assert_eq!(world.promises.held(), 2);
    assert_eq!(world.read(&mut mm, at + PAGE_SIZE, 1), [8]);
    assert_eq!(mm.space().translate(at + PAGE_SIZE, false).unwrap(), frame);
    // Split three ways, the pieces are one object, and join again.
    world.with(|ctx| mm.protect(at + PAGE_SIZE, PAGE_SIZE, RO, ctx)).unwrap();
    assert_eq!(mm.vmas().len(), 3);
    let objects: Vec<_> = mm
        .vmas()
        .iter()
        .map(|vma| match &vma.backing {
            Backing::Shared { object, first } => (crate::ObjectRef::as_ptr(object), *first),
            _ => panic!("not shared"),
        })
        .collect();
    assert!(objects.iter().all(|(object, _)| *object == objects[0].0));
    assert_eq!(objects.iter().map(|(_, first)| *first).collect::<Vec<_>>(), [0, 1, 2]);
    assert_eq!(mm.space().translate(at + PAGE_SIZE, false).unwrap(), frame);
    world.with(|ctx| mm.protect(at + PAGE_SIZE, PAGE_SIZE, RW, ctx)).unwrap();
    assert_eq!(mm.vmas().len(), 1);
    // Part unmapped, the object keeps its frames; all of it, and they go.
    world.with(|ctx| mm.unmap(at, PAGE_SIZE, ctx)).unwrap();
    // Its two frames, and the two tables the range had.
    assert_eq!(world.in_use(), used + 2 + 2);
    world.with(|ctx| mm.unmap(at + PAGE_SIZE, 3 * PAGE_SIZE, ctx)).unwrap();
    assert_eq!((world.in_use(), world.promises.held(), mm.charged()), (used, 0, 0));
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn pinned_frame_outlives_unmap() {
    let mut world = World::new();
    let mut mm = world.mm();
    let at = world.anywhere(&mut mm, PAGE_SIZE, RW);
    world.write(&mut mm, at, &[0x77]);
    let used = world.in_use();
    let pinned = mm.pin(at, true, &mut world.pool).unwrap();
    let kernel_address = pinned.address;
    world.with(|ctx| mm.unmap(at, PAGE_SIZE, ctx)).unwrap();
    // The mapping and its tables are gone, but not the frame the copy is using.
    assert_eq!(world.in_use(), used - 2);
    // SAFETY: pinned, so still the pool's.
    assert_eq!(unsafe { *(kernel_address as *const u8) }, 0x77);
    mm.unpin(pinned, &mut world.pool);
    assert_eq!(world.in_use(), used - 3);
    // A pin needs the access, and the zero frame needs none.
    let at = world.anywhere(&mut mm, PAGE_SIZE, RO);
    assert!(matches!(mm.pin(at, false, &mut world.pool), Err(PinError::Fault)));
    world.read(&mut mm, at, 1);
    assert!(matches!(mm.pin(at, true, &mut world.pool), Err(PinError::Fault)));
    let pinned = mm.pin(at, false, &mut world.pool).unwrap();
    mm.unpin(pinned, &mut world.pool);
    // At the pin cap, the next is refused.
    world.pool.max_pins = 1;
    let at = world.anywhere(&mut mm, PAGE_SIZE, RW);
    world.write(&mut mm, at, &[1]);
    let pinned = mm.pin(at, false, &mut world.pool).unwrap();
    assert!(matches!(mm.pin(at, false, &mut world.pool), Err(PinError::Busy)));
    mm.unpin(pinned, &mut world.pool);
    world.done(mm);
}

#[test]
fn vma_cap_is_enomem() {
    let mut world = World::new();
    let mut mm = world.mm();
    for _ in 0..MAX_VMAS {
        world.anywhere(&mut mm, PAGE_SIZE, RW);
    }
    assert_eq!(mm.vmas().len(), MAX_VMAS);
    let held = world.promises.held();
    assert_eq!(world.map(&mut mm, MapRequest::anonymous(PAGE_SIZE, RW)), Err(MmError::NoMemory));
    assert_eq!(world.promises.held(), held, "the refused one promised nothing");
    // A split would pass the cap too; joining two doesn't.
    let first = LOW;
    world.with(|ctx| mm.unmap(first, PAGE_SIZE, ctx)).unwrap();
    let three = MapRequest { place: Place::Fixed(first), ..MapRequest::anonymous(PAGE_SIZE, RW) };
    world.map(&mut mm, three).unwrap();
    let wide = world.map(&mut mm, MapRequest::anonymous(3 * PAGE_SIZE, RW));
    assert_eq!(wide, Err(MmError::NoMemory));
    world.with(|ctx| mm.unmap(first, PAGE_SIZE, ctx)).unwrap();
    let wide = world.map(&mut mm, MapRequest::anonymous(3 * PAGE_SIZE, RW)).unwrap();
    assert_eq!(world.with(|ctx| mm.protect(wide + PAGE_SIZE, PAGE_SIZE, RO, ctx)), Err(MmError::NoMemory));
    assert_eq!(world.with(|ctx| mm.unmap(wide + PAGE_SIZE, PAGE_SIZE, ctx)), Err(MmError::NoMemory));
    assert_eq!(mm.vmas().len(), MAX_VMAS);
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn exec_only_for_code_never_written() {
    let mut world = World::new();
    let mut mm = world.mm();
    let anon = world.anywhere(&mut mm, PAGE_SIZE, RW);
    assert_eq!(world.with(|ctx| mm.protect(anon, PAGE_SIZE, Prot::READ_EXEC, ctx)), Err(MmError::Denied));
    world.map(&mut mm, image(2 * PAGE_SIZE, Prot::READ_EXEC, USER_BASE)).unwrap();
    let write_exec = Prot { read: true, write: true, exec: true };
    assert_eq!(world.with(|ctx| mm.protect(USER_BASE, PAGE_SIZE, write_exec, ctx)), Err(MmError::Denied));
    // Code made writable stops being executable, and can't be made executable again.
    world.with(|ctx| mm.protect(USER_BASE, PAGE_SIZE, RW, ctx)).unwrap();
    world.write(&mut mm, USER_BASE, &[0xc3]);
    assert_eq!(world.with(|ctx| mm.protect(USER_BASE, PAGE_SIZE, Prot::READ_EXEC, ctx)), Err(MmError::Denied));
    // Its neighbour, never written, still may.
    world.with(|ctx| mm.protect(USER_BASE + PAGE_SIZE, PAGE_SIZE, RO, ctx)).unwrap();
    world.with(|ctx| mm.protect(USER_BASE + PAGE_SIZE, PAGE_SIZE, Prot::READ_EXEC, ctx)).unwrap();
    assert_eq!(world.with(|ctx| mm.fault(USER_BASE, Access { write: false, execute: true }, ctx)), Err(FaultError::Segv));
    assert_eq!(world.with(|ctx| mm.fault(USER_BASE + PAGE_SIZE, Access { write: false, execute: true }, ctx)), Ok(()));
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn image_and_stack_go_anywhere_in_the_window_but_mmap_does_not() {
    let mut world = World::new();
    let mut mm = world.mm();
    world.map(&mut mm, image(PAGE_SIZE, RO, USER_BASE)).unwrap();
    let stack = MapRequest { place: Place::Exact(USER_END - 2 * PAGE_SIZE), ..MapRequest::anonymous(PAGE_SIZE, RW) };
    world.map(&mut mm, stack).unwrap();
    // A whole area's worth fits in the mmap area, and no more.
    let area = USER_MMAP_END - USER_MMAP_BASE;
    assert_eq!(world.map(&mut mm, MapRequest::anonymous(area + PAGE_SIZE, Prot::NONE)), Err(MmError::NoMemory));
    assert_eq!(world.anywhere(&mut mm, area - PAGE_SIZE, Prot::NONE), LOW);
    world.done(mm);
}

#[test]
fn the_v2_layout_starts_a_program_as_the_kernel_does() {
    let mut world = World::new();
    let mut mm = world.mm();
    // The argument area and the stack below it, both charged, as the kernel maps them.
    let args = USER_STACK_TOP - USER_ARGS_SIZE;
    let stack = args - USER_STACK_SIZE;
    world.map(&mut mm, MapRequest { place: Place::Exact(args), ..MapRequest::anonymous(USER_ARGS_SIZE, RW) }).unwrap();
    world.map(&mut mm, MapRequest { place: Place::Exact(stack), ..MapRequest::anonymous(USER_STACK_SIZE, RW) }).unwrap();
    assert_eq!(mm.charged(), (USER_ARGS_SIZE + USER_STACK_SIZE) / PAGE_SIZE);
    consistent(&world, &mm);
    // Lazily backed: nothing has a frame until it's touched, then just that page.
    let before = world.in_use();
    world.write(&mut mm, USER_STACK_TOP - 8, &[1; 8]);
    world.write(&mut mm, stack, &[2]);
    assert_eq!(world.in_use(), before + 2);
    // Above the argument area and below the stack are the unmapped page and the guard.
    let read = Access { write: false, execute: false };
    assert_eq!(world.with(|ctx| mm.fault(USER_STACK_TOP, read, ctx)), Err(FaultError::Segv));
    assert_eq!(world.with(|ctx| mm.fault(stack - 1, read, ctx)), Err(FaultError::Segv));
    assert_eq!(world.with(|ctx| mm.fault(USER_MMAP_END, read, ctx)), Err(FaultError::Segv));
    assert_eq!(stack - USER_STACK_GUARD, USER_MMAP_END);
    // The mmap area runs right up to the guard, and `MAP_FIXED` can't reach past it.
    let last = MapRequest { place: Place::Fixed(USER_MMAP_END - PAGE_SIZE), ..MapRequest::anonymous(PAGE_SIZE, RW) };
    assert_eq!(world.map(&mut mm, last), Ok(USER_MMAP_END - PAGE_SIZE));
    let over = MapRequest { place: Place::Fixed(stack - PAGE_SIZE), ..MapRequest::anonymous(PAGE_SIZE, RW) };
    assert_eq!(world.map(&mut mm, over), Err(MmError::NoMemory));
    // Nor below the mmap area: the image, and the interpreter's gigabyte.
    for at in [USER_BASE - PAGE_SIZE, USER_BASE, USER_IMAGE_END, USER_MMAP_BASE - PAGE_SIZE] {
        let below = MapRequest { place: Place::Fixed(at), ..MapRequest::anonymous(PAGE_SIZE, RW) };
        assert_eq!(world.map(&mut mm, below), Err(MmError::NoMemory), "at {at:#x}");
    }
    let past = MapRequest { place: Place::Fixed(USER_END), ..MapRequest::anonymous(PAGE_SIZE, RW) };
    assert_eq!(world.map(&mut mm, past), Err(MmError::NoMemory));
    consistent(&world, &mm);
    // The top-level entries the window's top uses (index 511) go with the space.
    world.done(mm);
}

#[test]
fn the_whole_window_is_usable_far_above_the_old_end() {
    let mut world = World::new();
    let mut mm = world.mm();
    // A gigabyte at 300 GiB, where a hint puts it, and pages of it made usable there.
    let hint = 300 * GIB;
    let at = world.map(&mut mm, MapRequest { place: Place::Hint(hint), ..MapRequest::anonymous(GIB, Prot::NONE) }).unwrap();
    assert_eq!(at, hint);
    world.with(|ctx| mm.protect(at, 2 * PAGE_SIZE, RW, ctx)).unwrap();
    world.write(&mut mm, at + PAGE_SIZE, &[7]);
    assert_eq!(world.read(&mut mm, at + PAGE_SIZE, 1), [7]);
    // All a process may charge, reserved with no charge, made writable and touched a
    // page a gigabyte: it costs the pages and their tables, and no promise.
    let span = USER_MEMORY_LIMIT;
    let held = world.promises.held();
    let big = world.map(&mut mm, MapRequest { noreserve: true, ..MapRequest::anonymous(span, Prot::NONE) }).unwrap();
    world.with(|ctx| mm.protect(big, span, RW, ctx)).unwrap();
    let before = world.in_use();
    for gib in 0..span / GIB {
        world.write(&mut mm, big + gib * GIB + gib, &[gib as u8 + 1]);
    }
    for gib in 0..span / GIB {
        assert_eq!(world.read(&mut mm, big + gib * GIB + gib, 1), [gib as u8 + 1]);
    }
    // A page, a leaf table and at worst a middle table for each gigabyte.
    assert!(world.in_use() - before <= 3 * (span / GIB) + 1);
    assert_eq!(world.promises.held(), held);
    consistent(&world, &mm);
    // Charged, the same reservation made writable is more than a process may have.
    let charged = world.anywhere(&mut mm, span + GIB, Prot::NONE);
    assert_eq!(world.with(|ctx| mm.protect(charged, span + GIB, RW, ctx)), Err(MmError::NoMemory));
    world.with(|ctx| mm.unmap(big, span, ctx)).unwrap();
    world.with(|ctx| mm.unmap(at, GIB, ctx)).unwrap();
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn shared_object_heap_is_budgeted_and_remnants_keep_only_their_share() {
    let mut world = World::new();
    let big = 2 * GIB;
    let cost = SharedAnon::heap_cost(big / PAGE_SIZE);
    // Room for three, and not quite for the smallest object after them.
    let limit = 3 * cost + SharedAnon::heap_cost(1) - 1;
    let mut mm = world.mm_with(BITS, mm::Layout { object_heap_limit: limit, ..layout() });
    // A reservation no access can touch still costs its directory, so it's counted.
    let mut made = 0;
    loop {
        let request = MapRequest { shared: true, ..MapRequest::anonymous(big, Prot::NONE) };
        let Ok(at) = world.map(&mut mm, request) else {
            break;
        };
        made += 1;
        // All but a page gone: the remnant keeps the object, and its cost.
        world.with(|ctx| mm.unmap(at + PAGE_SIZE, big - PAGE_SIZE, ctx)).unwrap();
        assert_eq!(mm.object_heap(), made * cost);
        assert!(made < 100, "the budget never ran out");
    }
    assert_eq!(made, 3);
    assert_eq!((mm.charged(), world.promises.held()), (0, 0));
    // Refused, a shared map changes nothing; a private one still works.
    let request = MapRequest { shared: true, ..MapRequest::anonymous(PAGE_SIZE, RW) };
    assert_eq!(world.map(&mut mm, request), Err(MmError::NoMemory));
    assert_eq!((mm.charged(), world.promises.held(), mm.vmas().len()), (0, 0, made));
    world.anywhere(&mut mm, PAGE_SIZE, RW);
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn small_shared_objects_are_small() {
    // A one-page object costs a header and one pointer, and a touch a small leaf.
    assert!(SharedAnon::heap_cost(1) <= 128);
    let mut world = World::new();
    let mut mm = world.mm();
    let mut objects = Vec::new();
    for _ in 0..64 {
        let request = MapRequest { shared: true, ..MapRequest::anonymous(PAGE_SIZE, RW) };
        let at = world.map(&mut mm, request).unwrap();
        world.write(&mut mm, at, &[3]);
        objects.push(at);
    }
    assert_eq!(mm.object_heap(), 64 * SharedAnon::heap_cost(1));
    assert_eq!((mm.charged(), world.promises.held()), (64, 0));
    for at in objects {
        assert_eq!(world.read(&mut mm, at, 1), [3]);
        world.with(|ctx| mm.unmap(at, PAGE_SIZE, ctx)).unwrap();
    }
    assert_eq!((mm.object_heap(), mm.charged()), (0, 0));
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn every_charged_piece_of_an_object_has_its_tables() {
    let mut world = World::new();
    let mut mm = world.mm();
    let length = 4 << 20;
    let request = MapRequest { shared: true, ..MapRequest::anonymous(length, RO) };
    let at = world.map(&mut mm, request).unwrap();
    let last = at + length - PAGE_SIZE;
    // The first page charges the whole object; the last is in another leaf table.
    world.with(|ctx| mm.protect(at, PAGE_SIZE, RW, ctx)).unwrap();
    assert_eq!(mm.charged(), length / PAGE_SIZE);
    world.with(|ctx| mm.protect(last, PAGE_SIZE, RW, ctx)).unwrap();
    assert_eq!(mm.charged(), length / PAGE_SIZE);
    consistent(&world, &mm);
    // No frame beyond what's promised: the write is promised its page and its table.
    world.pool.limit = world.pool.live.len() + world.promises.held();
    world.write(&mut mm, last, &[9]);
    assert_eq!(world.read(&mut mm, last, 1), [9]);
    world.pool.limit = usize::MAX;
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn interleaved_pieces_of_objects_are_charged_once_each() {
    let mut world = World::new();
    let mut mm = world.mm();
    let shared = |length| MapRequest { shared: true, ..MapRequest::anonymous(length, RO) };
    let at = world.map(&mut mm, shared(8 * PAGE_SIZE)).unwrap();
    // Another object in the middle of the first: pieces of A, B, A.
    let middle = MapRequest { place: Place::Fixed(at + 2 * PAGE_SIZE), ..shared(2 * PAGE_SIZE) };
    world.map(&mut mm, middle).unwrap();
    assert_eq!(mm.vmas().len(), 3);
    world.with(|ctx| mm.protect(at, 8 * PAGE_SIZE, RW, ctx)).unwrap();
    // The first object whole, though two of its pages are no longer mapped, and the
    // second: each once.
    assert_eq!((mm.charged(), world.promises.held()), (10, 10));
    consistent(&world, &mm);
    world.done(mm);
}

/// A mapping of `file` from its first page.
fn file_request(file: &FileRef, length: usize, prot: Prot, shared: bool) -> MapRequest {
    MapRequest {
        shared,
        file: Some(mm::FileMap { file: file.clone(), first: 0, may_write: false }),
        ..MapRequest::anonymous(length, prot)
    }
}

/// What the file `ino` holds at `offset`.
fn file_byte(world: &World, ino: u64, offset: usize) -> u8 {
    world.files.data[&FileKey { dev: 1, ino }][offset]
}

#[test]
fn private_file_page_copies_on_write() {
    let mut world = World::new();
    let mut mm = world.mm();
    let file = world.files.add(1, 2 * PAGE_SIZE + 100, 0x11);
    let at = world.map(&mut mm, file_request(&file, 3 * PAGE_SIZE, RW, false)).unwrap();
    // Read first: the cache's frame, shared with a second mapping of it.
    let other = world.map(&mut mm, file_request(&file, 3 * PAGE_SIZE, RO, false)).unwrap();
    assert_eq!(world.read(&mut mm, at + 5, 1)[0], file_byte(&world, 1, 5));
    assert_eq!(world.read(&mut mm, other + 5, 1)[0], file_byte(&world, 1, 5));
    let cached = mm.space().translate(at, false).unwrap();
    assert_eq!(cached, mm.space().translate(other, false).unwrap(), "one frame for both");
    assert_eq!(world.files.reads, 1);
    // A write copies: the file and the other mapping don't see it.
    world.write(&mut mm, at + 5, &[0xff]);
    assert_ne!(mm.space().translate(at, false).unwrap(), cached);
    assert_eq!(world.read(&mut mm, at + 5, 1)[0], 0xff);
    assert_eq!(world.read(&mut mm, other + 5, 1)[0], file_byte(&world, 1, 5));
    // A page written before it was read is copied straight away; the tail of the last
    // page reads as zeroes, and past the end is SIGBUS.
    world.write(&mut mm, at + 2 * PAGE_SIZE + 99, &[1, 2]);
    let tail = world.read(&mut mm, at + 2 * PAGE_SIZE + 98, 3);
    assert_eq!(tail, [file_byte(&world, 1, 2 * PAGE_SIZE + 98), 1, 2]);
    let short = world.files.add(2, PAGE_SIZE, 0);
    let small = world.map(&mut mm, file_request(&short, 2 * PAGE_SIZE, RO, false)).unwrap();
    let fault = world.with(|ctx| mm.fault(small + PAGE_SIZE, Access::default(), ctx));
    assert_eq!(fault, Err(FaultError::Bus), "a page wholly past the end");
    // Copies are charged as anonymous writable pages are.
    consistent(&world, &mm);
    assert_eq!(mm.unbacked(), 3 - 2, "two of the three charged pages have copies");
    world.with(|ctx| mm.unmap(at, 3 * PAGE_SIZE, ctx)).unwrap();
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn cache_frame_never_written_in_place() {
    let mut world = World::new();
    let mut mm = world.mm();
    let file = world.files.add(3, PAGE_SIZE, 0x33);
    let at = world.map(&mut mm, file_request(&file, PAGE_SIZE, RW, false)).unwrap();
    world.read(&mut mm, at, 1);
    let cached = mm.space().translate(at, false).unwrap();
    // Read, its entry is read-only even in a writable range.
    assert_eq!(mm.space().translate(at, true), Err(BadAddress));
    // Brought up to date by a stale fault, it still isn't writable.
    world.with(|ctx| mm.fault(at, Access::default(), ctx)).unwrap();
    assert_eq!(mm.space().translate(at, true), Err(BadAddress));
    // mprotect doesn't make it writable either.
    world.with(|ctx| mm.protect(at, PAGE_SIZE, RO, ctx)).unwrap();
    world.with(|ctx| mm.protect(at, PAGE_SIZE, RW, ctx)).unwrap();
    assert_eq!(mm.space().translate(at, true), Err(BadAddress));
    world.write(&mut mm, at, &[0]);
    // SAFETY: the cache's frame, which the pool still has.
    assert_eq!(unsafe { *(cached as *const u8) }, file_byte(&world, 3, 0), "the cache's copy is as the file");
    // A shared mapping is never writable at all.
    let shared = world.map(&mut mm, file_request(&file, PAGE_SIZE, RO, true)).unwrap();
    assert_eq!(world.with(|ctx| mm.protect(shared, PAGE_SIZE, RW, ctx)), Err(MmError::Denied));
    assert_eq!(world.map(&mut mm, file_request(&file, PAGE_SIZE, RW, true)), Err(MmError::Denied));
    world.read(&mut mm, shared, 1);
    assert_eq!(mm.space().translate(shared, false).unwrap(), cached);
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn exec_refused_after_copy() {
    let mut world = World::new();
    let mut mm = world.mm();
    let file = world.files.add(4, 2 * PAGE_SIZE, 0x44);
    let at = world.map(&mut mm, file_request(&file, 2 * PAGE_SIZE, RW, false)).unwrap();
    world.read(&mut mm, at, 1);
    world.write(&mut mm, at + PAGE_SIZE, &[0xc3]);
    let rx = Prot::READ_EXEC;
    // The page that was read may run; the one written may not, nor anything with it.
    world.with(|ctx| mm.protect(at, PAGE_SIZE, RO, ctx)).unwrap();
    world.with(|ctx| mm.protect(at, PAGE_SIZE, rx, ctx)).unwrap();
    assert_eq!(file.exec_pages(), 1);
    assert_eq!(world.with(|ctx| mm.protect(at, 2 * PAGE_SIZE, rx, ctx)), Err(MmError::Denied));
    assert_eq!(world.with(|ctx| mm.protect(at + PAGE_SIZE, PAGE_SIZE, rx, ctx)), Err(MmError::Denied));
    assert_eq!(file.exec_pages(), 1, "a refusal changes nothing");
    // Thrown away, the copy is the file again, which may run.
    world.with(|ctx| mm.discard(at + PAGE_SIZE, PAGE_SIZE, ctx)).unwrap();
    world.with(|ctx| mm.protect(at + PAGE_SIZE, PAGE_SIZE, RO, ctx)).unwrap();
    world.with(|ctx| mm.protect(at + PAGE_SIZE, PAGE_SIZE, rx, ctx)).unwrap();
    assert_eq!(file.exec_pages(), 2);
    world.with(|ctx| mm.fault(at + PAGE_SIZE, Access { write: false, execute: true }, ctx)).unwrap();
    assert!(world.files.synced.contains(&(mm.space().translate(at + PAGE_SIZE, false).unwrap())), "synced before it runs");
    // Executable can't be written; and a file open for writing can't be made to run.
    assert_eq!(world.with(|ctx| mm.fault(at, Access { write: true, execute: false }, ctx)), Err(FaultError::Segv));
    world.with(|ctx| mm.protect(at, 2 * PAGE_SIZE, RO, ctx)).unwrap();
    assert_eq!(file.exec_pages(), 0);
    file.open_writer().unwrap();
    assert_eq!(world.with(|ctx| mm.protect(at, PAGE_SIZE, rx, ctx)), Err(MmError::TextBusy));
    let request = file_request(&file, PAGE_SIZE, rx, false);
    assert_eq!(world.map(&mut mm, request), Err(MmError::TextBusy));
    file.close_writer();
    // Anonymous memory never runs.
    let anonymous = world.anywhere(&mut mm, PAGE_SIZE, RO);
    assert_eq!(world.with(|ctx| mm.protect(anonymous, PAGE_SIZE, rx, ctx)), Err(MmError::Denied));
    world.with(|ctx| mm.unmap(at, 2 * PAGE_SIZE, ctx)).unwrap();
    assert_eq!(file.exec_pages(), 0, "unmapped, nothing of it runs");
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn file_mappings_are_in_the_reverse_map_and_zapped_on_truncation() {
    let mut world = World::new();
    let mut mm = world.mm();
    let file = world.files.add(5, 4 * PAGE_SIZE, 0x55);
    let at = world.map(&mut mm, file_request(&file, 4 * PAGE_SIZE, RW, false)).unwrap();
    let mut spaces = Vec::new();
    file.each_mapper(&mut |space| spaces.push(space));
    assert_eq!(spaces, [1]);
    world.read(&mut mm, at, 1);
    world.read(&mut mm, at + 2 * PAGE_SIZE, 1);
    world.write(&mut mm, at + 3 * PAGE_SIZE, &[9]);
    // Cut to two pages: pages 2 and 3 leave the mapping, the copy too.
    world.with(|ctx| mm.zap_file(file.key(), 2, ctx));
    assert!(mm.space().translate(at, false).is_ok());
    assert_eq!(mm.space().translate(at + 2 * PAGE_SIZE, false), Err(BadAddress));
    assert_eq!(mm.space().translate(at + 3 * PAGE_SIZE, false), Err(BadAddress));
    consistent(&world, &mm);
    // A split keeps the file offsets right.
    world.with(|ctx| mm.unmap(at, PAGE_SIZE, ctx)).unwrap();
    assert_eq!(world.read(&mut mm, at + PAGE_SIZE + 3, 1)[0], file_byte(&world, 5, PAGE_SIZE + 3));
    world.with(|ctx| mm.unmap(at, 4 * PAGE_SIZE, ctx)).unwrap();
    let mut spaces = Vec::new();
    file.each_mapper(&mut |space| spaces.push(space));
    assert!(spaces.is_empty());
    world.done(mm);
}

#[test]
fn shared_file_write_fault_marks_dirty() {
    let mut world = World::new();
    let mut mm = world.mm();
    let file = world.files.add(6, 2 * PAGE_SIZE, 0x66);
    let writable = |length| MapRequest {
        shared: true,
        file: Some(mm::FileMap { file: file.clone(), first: 0, may_write: true }),
        ..MapRequest::anonymous(length, RW)
    };
    // Not open for writing: a shared mapping can't be writable, made so or later.
    assert_eq!(world.map(&mut mm, file_request(&file, PAGE_SIZE, RW, true)), Err(MmError::Denied));
    let read_only = world.map(&mut mm, file_request(&file, PAGE_SIZE, RO, true)).unwrap();
    assert_eq!(world.with(|ctx| mm.protect(read_only, PAGE_SIZE, RW, ctx)), Err(MmError::Denied));
    let at = world.map(&mut mm, writable(2 * PAGE_SIZE)).unwrap();
    assert_eq!(file.write_pages(), 2, "counted as a writer");
    // Read first: the cache's frame, read-only, and clean.
    assert_eq!(world.read(&mut mm, at + 3, 1)[0], file_byte(&world, 6, 3));
    let cached = mm.space().translate(at, false).unwrap();
    assert_eq!(mm.space().translate(at, true), Err(BadAddress));
    assert_eq!(file.dirty_pages(), 0);
    // The first write marks it dirty and makes the entry writable, on the same frame:
    // the other mapping, and the cache, see it.
    world.write(&mut mm, at + 3, &[0xd1]);
    assert_eq!(file.dirty_pages(), 1);
    assert_eq!(mm.space().translate(at, true), Ok(cached));
    assert_eq!(world.read(&mut mm, read_only + 3, 1)[0], 0xd1);
    // A page written before it was read is dirty at once; neither took a promise.
    world.write(&mut mm, at + PAGE_SIZE, &[0xd2]);
    assert_eq!(file.dirty_pages(), 2);
    assert_eq!((mm.charged(), world.promises.held()), (0, 0));
    // A stale fault keeps it writable; mprotect to read-only stops it counting as a
    // writer, and back to writable it's read-only until written again.
    world.with(|ctx| mm.fault(at, Access::default(), ctx)).unwrap();
    assert_eq!(mm.space().translate(at, true), Ok(cached));
    world.with(|ctx| mm.protect(at, 2 * PAGE_SIZE, RO, ctx)).unwrap();
    assert_eq!((file.write_pages(), mm.space().translate(at, true)), (0, Err(BadAddress)));
    world.with(|ctx| mm.protect(at, PAGE_SIZE, RW, ctx)).unwrap();
    assert_eq!(file.write_pages(), 1);
    assert_eq!(mm.space().translate(at, true), Err(BadAddress));
    world.write(&mut mm, at, &[0xd3]);
    assert_eq!(mm.space().translate(at, true), Ok(cached));
    // Mapped writable, it can't run; nor be mapped writable while it runs.
    let rx = Prot::READ_EXEC;
    assert_eq!(world.map(&mut mm, file_request(&file, PAGE_SIZE, rx, false)), Err(MmError::TextBusy));
    world.with(|ctx| mm.unmap(at, 2 * PAGE_SIZE, ctx)).unwrap();
    assert_eq!(file.write_pages(), 0, "unmapped, it writes nothing");
    let code = world.map(&mut mm, file_request(&file, PAGE_SIZE, rx, false)).unwrap();
    assert_eq!(world.map(&mut mm, writable(PAGE_SIZE)), Err(MmError::TextBusy));
    assert_eq!(file.write_pages(), 0, "a refusal changes nothing");
    // The cache has what was written, dirty until written back.
    // SAFETY: the cache's frame, which the pool still has.
    assert_eq!(unsafe { *((cached + 3) as *const u8) }, 0xd1);
    assert_eq!(file.dirty_pages(), 2);
    world.with(|ctx| mm.unmap(code, PAGE_SIZE, ctx)).unwrap();
    world.with(|ctx| mm.unmap(read_only, PAGE_SIZE, ctx)).unwrap();
    consistent(&world, &mm);
    // Written back, the pages are clean and can go with the rest.
    struct Sink;
    impl feos_pagecache::PageWriter for Sink {
        fn write(&mut self, _key: FileKey, _offset: u64, fill: &mut dyn FnMut(&mut [u8]) -> usize) -> bool {
            let mut page = [0u8; PAGE_SIZE];
            fill(&mut page);
            true
        }
    }
    assert_eq!(world.files.cache.write_back(&file, 0, u64::MAX, &PoolView(&mut world.pool), &mut Sink), Ok(2));
    assert_eq!(file.dirty_pages(), 0);
    world.done(mm);
}

#[test]
fn unmap_with_pinned_page_defers_free() {
    // A copy pinned a page, another thread unmaps it: the mapping, its tables and its
    // promise go at once, the frame only with the last pin, and what the copy writes
    // into it meanwhile lands somewhere nobody else has.
    let mut world = World::new();
    let mut mm = world.mm();
    let request = MapRequest { charge_all: true, ..MapRequest::anonymous(2 * PAGE_SIZE, RW) };
    let at = world.map(&mut mm, request).unwrap();
    let pinned = world.with(|ctx| mm.translate_and_pin(at + PAGE_SIZE + 8, true, ctx)).unwrap();
    let again = world.with(|ctx| mm.translate_and_pin(at + PAGE_SIZE + 16, false, ctx)).unwrap();
    assert_eq!(pinned.address + 8, again.address);
    let used = world.in_use();
    world.with(|ctx| mm.unmap(at, 2 * PAGE_SIZE, ctx)).unwrap();
    assert_eq!(world.promises.held(), 0);
    assert_eq!(mm.charged(), 0);
    // The page's frame is still out, the tables aren't.
    assert_eq!(world.in_use(), used - 2);
    // SAFETY: pinned, so still the pool's, and nothing maps it any more.
    unsafe { *(pinned.address as *mut u8) = 0x42 };
    mm.unpin(pinned, &mut world.pool);
    assert_eq!(world.in_use(), used - 2);
    mm.unpin(again, &mut world.pool);
    assert_eq!(world.in_use(), used - 3);
    // Mapped again, it's a fresh page.
    let at = world.map(&mut mm, MapRequest { place: Place::Fixed(at), ..MapRequest::anonymous(2 * PAGE_SIZE, RW) }).unwrap();
    assert_eq!(world.read(&mut mm, at + PAGE_SIZE + 8, 1), [0]);
    consistent(&world, &mm);
    world.done(mm);
}

#[test]
fn translate_and_pin_is_atomic() {
    let mut world = World::new();
    let mut mm = world.mm();
    // An untouched page is given its frame and pinned in the one step, the frame the
    // entry has for that access: its own for a write, never the zero frame.
    let at = world.anywhere(&mut mm, PAGE_SIZE, RW);
    let pinned = world.with(|ctx| mm.translate_and_pin(at, true, ctx)).unwrap();
    let frame = mm.space().translate(at, true).unwrap();
    assert_eq!(pinned.address, frame);
    assert_ne!(frame, world.zero);
    assert_eq!(world.pool.live[&frame], (1, 1));
    mm.unpin(pinned, &mut world.pool);
    // A read of an untouched page pins nothing: the zero frame isn't counted.
    let other = world.anywhere(&mut mm, PAGE_SIZE, RW);
    let read = world.with(|ctx| mm.translate_and_pin(other, false, ctx)).unwrap();
    assert_eq!(read.address, world.zero);
    mm.unpin(read, &mut world.pool);
    // A failure leaves nothing pinned and nothing mapped that wasn't.
    let used = world.in_use();
    let readonly = world.anywhere(&mut mm, PAGE_SIZE, RO);
    assert_eq!(world.with(|ctx| mm.translate_and_pin(readonly, true, ctx)).unwrap_err(), PinError::Fault);
    assert_eq!(world.with(|ctx| mm.translate_and_pin(USER_MMAP_END, false, ctx)).unwrap_err(), PinError::Fault);
    assert!(world.pool.live.values().all(|&(_, pins)| pins == 0));
    world.with(|ctx| mm.unmap(readonly, PAGE_SIZE, ctx)).unwrap();
    assert_eq!(world.in_use(), used);
    // At the cap, the pin is refused and the page is still mapped, unpinned.
    world.pool.max_pins = 1;
    let first = world.with(|ctx| mm.translate_and_pin(at, false, ctx)).unwrap();
    assert_eq!(world.with(|ctx| mm.translate_and_pin(at, true, ctx)).unwrap_err(), PinError::Busy);
    assert_eq!(world.pool.live[&frame], (1, 1));
    mm.unpin(first, &mut world.pool);
    // No frame to give an untouched page: nothing pinned, and it's an answer.
    let mut tight = World::limited(64);
    let mut small = tight.mm();
    let noreserve = tight
        .map(&mut small, MapRequest { noreserve: true, ..MapRequest::anonymous(64 * PAGE_SIZE, RW) })
        .unwrap();
    let mut errors = Vec::new();
    for page in 0..64 {
        match tight.with(|ctx| small.translate_and_pin(noreserve + page * PAGE_SIZE, true, ctx)) {
            Ok(pinned) => small.unpin(pinned, &mut tight.pool),
            Err(error) => errors.push(error),
        }
    }
    assert!(!errors.is_empty() && errors.iter().all(|error| *error == PinError::NoFrame));
    assert!(tight.pool.live.values().all(|&(_, pins)| pins == 0));
    tight.done(small);
    world.done(mm);
}

#[test]
fn futex_words_are_named_by_what_maps_them() {
    let mut world = World::new();
    let mut mm = world.mm();
    let private = world.anywhere(&mut mm, PAGE_SIZE, RW);
    assert_eq!(mm.word_key(private + 4), Some(WordKey::Own(private + 4)));
    assert_eq!(mm.word_key(private + PAGE_SIZE + PAGE_SIZE / 2), None);
    // Two mappings of one shared object agree on their words, wherever they are.
    let request = MapRequest { shared: true, ..MapRequest::anonymous(4 * PAGE_SIZE, RW) };
    let at = world.map(&mut mm, request).unwrap();
    world.with(|ctx| mm.protect(at + PAGE_SIZE, PAGE_SIZE, RO, ctx)).unwrap();
    let Some(WordKey::Shared { object, offset }) = mm.word_key(at + 2 * PAGE_SIZE + 8) else {
        panic!("a shared word is shared");
    };
    assert_eq!(offset, (2 * PAGE_SIZE + 8) as u64);
    let mut seen = Vec::new();
    mm.shared_words(at + PAGE_SIZE, at + 3 * PAGE_SIZE, &mut |object, low, high| seen.push((object, low, high)));
    assert_eq!(seen, [(object, PAGE_SIZE as u64, 2 * PAGE_SIZE as u64), (object, 2 * PAGE_SIZE as u64, 3 * PAGE_SIZE as u64)]);
    // A file: shared mappings name the file, private ones the space.
    let file = world.files.add(9, 2 * PAGE_SIZE, 0x19);
    let shared = world.map(&mut mm, file_request(&file, 2 * PAGE_SIZE, RO, true)).unwrap();
    let copy = world.map(&mut mm, file_request(&file, 2 * PAGE_SIZE, RO, false)).unwrap();
    assert_eq!(
        mm.word_key(shared + PAGE_SIZE),
        Some(WordKey::Shared { object: FileRef::as_ptr(&file) as usize, offset: PAGE_SIZE as u64 })
    );
    assert_eq!(mm.word_key(copy + PAGE_SIZE), Some(WordKey::Own(copy + PAGE_SIZE)));
    let mut seen = Vec::new();
    mm.shared_words(copy, copy + 2 * PAGE_SIZE, &mut |object, low, high| seen.push((object, low, high)));
    assert!(seen.is_empty());
    world.done(mm);
}

// Stage 1d.1: `FORK`, copy-on-write and strict charging.

/// Runs a fork test with each port's way of writing entries.
fn on_both(test: fn(PageBits)) {
    test(BITS);
    test(ARM_BITS);
}

/// A space for a fork test, written the `bits` way.
fn forking(world: &mut World, bits: PageBits) -> Mm {
    world.mm_with(bits, layout())
}

/// `parent`'s child, which reverse maps know as space 2.
fn fork_of(world: &mut World, parent: &mut Mm) -> Mm {
    world.with(|ctx| parent.fork(2, ctx)).expect("the fork")
}

/// Whether the entry at `address` is shared copy-on-write.
fn is_cow(mm: &Mm, address: usize) -> bool {
    matches!(mm.space().entry(address), Entry::Present { cow: true, .. } | Entry::Hidden { cow: true, .. })
}

/// The frame behind `address`, for reading.
fn frame_at(mm: &Mm, address: usize) -> usize {
    mm.space().translate(address, false).unwrap() & !(PAGE_SIZE - 1)
}

/// The extra references the pool's frames have, over all of them that `spaces` map as
/// their own private pages: what a fork's charging says the machine owes.
fn debt(world: &World, spaces: &[&Mm]) -> usize {
    let mut frames = std::collections::HashSet::new();
    for mm in spaces {
        for vma in mm.vmas().iter() {
            if !matches!(vma.backing, Backing::Private | Backing::File { shared: false, .. }) {
                continue;
            }
            mm.space().visit(vma.start, vma.end, &mut |_, entry| match entry {
                Entry::Present { frame, cached: false, .. } | Entry::Hidden { frame, cached: false, .. } if frame != world.zero => {
                    frames.insert(frame);
                }
                _ => {}
            });
        }
    }
    frames.iter().map(|frame| world.pool.live[frame].0 as usize - 1).sum()
}

/// What the charged shared objects `spaces` map hold in promises, each object once.
fn objects_owed(spaces: &[&Mm]) -> usize {
    let mut seen = std::collections::HashMap::new();
    for mm in spaces {
        for vma in mm.vmas().iter() {
            if let Backing::Shared { object, .. } = &vma.backing {
                let owed = if object.charged() { object.pages() - object.frames() } else { 0 };
                seen.insert(crate::ObjectRef::as_ptr(object) as usize, owed);
            }
        }
    }
    seen.values().sum()
}

/// The machine's promises are each space's own, its objects' and the forks' debt, exactly.
fn owed(world: &World, spaces: &[&Mm]) {
    let own: usize = spaces.iter().map(|mm| mm.private_unbacked()).sum();
    let expected = own + objects_owed(spaces) + debt(world, spaces);
    assert_eq!(world.promises.held(), expected, "promises held against what's owed");
    assert_eq!(world.promises.underflows(), 0);
}

/// Gives back a child and its parent, and checks nothing is left.
fn both_done(mut world: World, parent: Mm, child: Mm) {
    child.destroy(&mut world.pool, &world.promises);
    world.done(parent);
}

#[test]
fn fork_shares_until_write() {
    on_both(|bits| {
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let at = world.anywhere(&mut parent, 4 * PAGE_SIZE, RW);
        for page in 0..4 {
            world.write(&mut parent, at + page * PAGE_SIZE, &[page as u8 + 1]);
        }
        let mut child = fork_of(&mut world, &mut parent);
        for page in 0..4 {
            let address = at + page * PAGE_SIZE;
            assert_eq!(frame_at(&parent, address), frame_at(&child, address), "shared");
            assert!(is_cow(&parent, address) && is_cow(&child, address));
            assert_eq!(parent.space().translate(address, true), Err(BadAddress), "read-only in the parent");
            assert_eq!(child.space().translate(address, true), Err(BadAddress), "and in the child");
            assert_eq!(world.read(&mut child, address, 1), [page as u8 + 1]);
        }
        // The parent's entries that became read-only were flushed.
        assert!(world.flushed.contains(&at));
        // Each writes its own page; the other keeps what it had.
        world.write(&mut child, at, &[0xc0]);
        world.write(&mut parent, at + PAGE_SIZE, &[0xb1]);
        assert_eq!(world.read(&mut parent, at, 1), [1]);
        assert_eq!(world.read(&mut child, at, 1), [0xc0]);
        assert_eq!(world.read(&mut child, at + PAGE_SIZE, 1), [2]);
        assert_eq!(world.read(&mut parent, at + PAGE_SIZE, 1), [0xb1]);
        assert_eq!(frame_at(&parent, at + 2 * PAGE_SIZE), frame_at(&child, at + 2 * PAGE_SIZE));
        owed(&world, &[&parent, &child]);
        both_done(world, parent, child);
    });
}

#[test]
fn cow_copy_at_count_two() {
    on_both(|bits| {
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let at = world.anywhere(&mut parent, PAGE_SIZE, RW);
        world.write(&mut parent, at, &[7]);
        let frame = frame_at(&parent, at);
        let mut child = fork_of(&mut world, &mut parent);
        assert_eq!(world.pool.live[&frame].0, 2);
        // The writer copies; the frame stays the other's, at one reference again.
        world.write(&mut parent, at, &[8]);
        assert_ne!(frame_at(&parent, at), frame);
        assert_eq!(frame_at(&child, at), frame);
        assert_eq!(world.pool.live[&frame].0, 1);
        assert!(!is_cow(&parent, at) && parent.space().translate(at, true).is_ok());
        assert_eq!(world.read(&mut child, at, 1), [7]);
        owed(&world, &[&parent, &child]);
        both_done(world, parent, child);
    });
}

#[test]
fn cow_reuse_at_count_one_anon_only() {
    on_both(|bits| {
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let at = world.anywhere(&mut parent, 2 * PAGE_SIZE, RW);
        world.write(&mut parent, at, &[3]);
        world.write(&mut parent, at + PAGE_SIZE, &[4]);
        let frame = frame_at(&parent, at);
        let child = fork_of(&mut world, &mut parent);
        child.destroy(&mut world.pool, &world.promises);
        // Nothing else has it: the write takes the frame back in place, nothing allocated.
        let used = world.in_use();
        assert!(is_cow(&parent, at));
        world.write(&mut parent, at, &[5]);
        assert_eq!(frame_at(&parent, at), frame);
        assert_eq!(world.in_use(), used);
        assert!(!is_cow(&parent, at));
        // A copy in flight through the frame keeps it from being taken back: copied.
        let other = frame_at(&parent, at + PAGE_SIZE);
        let child = fork_of(&mut world, &mut parent);
        child.destroy(&mut world.pool, &world.promises);
        let pinned = parent.pin(at + PAGE_SIZE, false, &mut world.pool).unwrap();
        world.write(&mut parent, at + PAGE_SIZE, &[6]);
        assert_ne!(frame_at(&parent, at + PAGE_SIZE), other);
        // SAFETY: pinned, so still the pool's.
        assert_eq!(unsafe { *(pinned.address as *const u8) }, 4, "the copy in flight sees what it pinned");
        parent.unpin(pinned, &mut world.pool);
        owed(&world, &[&parent]);
        consistent(&world, &parent);
        world.done(parent);
    });
}

#[test]
fn cow_never_reuses_cache_frame() {
    on_both(|bits| {
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let file = world.files.add(21, PAGE_SIZE, 0x21);
        let at = world.map(&mut parent, file_request(&file, PAGE_SIZE, RW, false)).unwrap();
        world.read(&mut parent, at, 1);
        let cached = frame_at(&parent, at);
        let child = fork_of(&mut world, &mut parent);
        assert_eq!(frame_at(&child, at), cached, "the cache's frame, shared as it is");
        assert!(!is_cow(&child, at), "a cache frame is never the space's to take back");
        child.destroy(&mut world.pool, &world.promises);
        // Even with nothing else mapping it, a write copies: it's the file's.
        world.write(&mut parent, at, &[0xee]);
        assert_ne!(frame_at(&parent, at), cached);
        assert_eq!(world.read(&mut parent, at, 1), [0xee]);
        assert_eq!(file_byte(&world, 21, 0), 0x21 ^ 0);
        // SAFETY: the cache still has the frame.
        assert_eq!(unsafe { *(cached as *const u8) }, file_byte(&world, 21, 0));
        consistent(&world, &parent);
        world.done(parent);
    });
}

#[test]
fn pinned_page_copied_at_fork() {
    on_both(|bits| {
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let at = world.anywhere(&mut parent, 2 * PAGE_SIZE, RW);
        world.write(&mut parent, at, &[1]);
        world.write(&mut parent, at + PAGE_SIZE, &[2]);
        let frame = frame_at(&parent, at);
        // A copy of the kernel's is in flight into the page as the fork comes.
        let pinned = parent.pin(at, true, &mut world.pool).unwrap();
        let mut child = fork_of(&mut world, &mut parent);
        assert_ne!(frame_at(&child, at), frame, "the child has a copy of its own");
        assert_eq!(frame_at(&parent, at), frame);
        assert!(!is_cow(&parent, at) && parent.space().translate(at, true).is_ok(), "still the parent's, writable");
        // What the kernel writes through the pin after the fork is the parent's alone.
        // SAFETY: pinned for writing.
        unsafe { *(pinned.address as *mut u8) = 0x99 };
        parent.unpin(pinned, &mut world.pool);
        assert_eq!(world.read(&mut parent, at, 1), [0x99]);
        assert_eq!(world.read(&mut child, at, 1), [1]);
        // The page without a pin is shared as usual.
        assert!(is_cow(&child, at + PAGE_SIZE));
        owed(&world, &[&parent, &child]);
        both_done(world, parent, child);
    });
}

#[test]
fn write_pin_breaks_cow() {
    on_both(|bits| {
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let at = world.anywhere(&mut parent, PAGE_SIZE, RW);
        world.write(&mut parent, at, &[0x10]);
        let mut child = fork_of(&mut world, &mut parent);
        let shared = frame_at(&child, at);
        // A pin for writing is refused on the shared entry, and readying it copies first.
        assert!(matches!(child.pin(at, true, &mut world.pool), Err(PinError::Fault)));
        let pinned = world.with(|ctx| child.translate_and_pin(at, true, ctx)).unwrap();
        assert_ne!(pinned.address & !(PAGE_SIZE - 1), shared, "the kernel never writes a shared frame");
        // SAFETY: pinned for writing.
        unsafe { *(pinned.address as *mut u8) = 0x20 };
        child.unpin(pinned, &mut world.pool);
        assert_eq!(world.read(&mut parent, at, 1), [0x10]);
        assert_eq!(world.read(&mut child, at, 1), [0x20]);
        owed(&world, &[&parent, &child]);
        both_done(world, parent, child);
    });
}

#[test]
fn shared_stays_shared() {
    on_both(|bits| {
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let request = MapRequest { shared: true, ..MapRequest::anonymous(2 * PAGE_SIZE, RW) };
        let at = world.map(&mut parent, request).unwrap();
        world.write(&mut parent, at, &[1]);
        let file = world.files.add(22, PAGE_SIZE, 0x22);
        let mapped = world.map(&mut parent, file_request(&file, PAGE_SIZE, RO, true)).unwrap();
        world.read(&mut parent, mapped, 1);
        let mut child = fork_of(&mut world, &mut parent);
        assert_eq!(frame_at(&child, at), frame_at(&parent, at));
        assert!(!is_cow(&child, at) && child.space().translate(at, true).is_ok(), "shared, and writable");
        world.write(&mut child, at, &[2]);
        assert_eq!(world.read(&mut parent, at, 1), [2]);
        world.write(&mut parent, at + 8, &[3]);
        assert_eq!(world.read(&mut child, at + 8, 1), [3]);
        assert_eq!(frame_at(&child, mapped), frame_at(&parent, mapped), "the file's page too");
        // The object is promised once, however many spaces count it.
        assert_eq!((parent.charged(), child.charged()), (2, 2));
        owed(&world, &[&parent, &child]);
        assert_eq!(world.promises.held(), 1, "the object's one page without a frame");
        // The parent going first leaves the object to the child.
        let mut world = world;
        parent.destroy(&mut world.pool, &world.promises);
        assert_eq!(world.read(&mut child, at, 1), [2]);
        world.write(&mut child, at + PAGE_SIZE, &[4]);
        assert_eq!(world.promises.held(), 0);
        world.done(child);
    });
}

#[test]
fn untouched_shared_anon_shared_after_fork() {
    on_both(|bits| {
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let request = MapRequest { shared: true, ..MapRequest::anonymous(3 * PAGE_SIZE, RW) };
        let at = world.map(&mut parent, request).unwrap();
        // Neither has touched a page before the fork.
        let mut child = fork_of(&mut world, &mut parent);
        world.write(&mut child, at + PAGE_SIZE, &[0x5a]);
        assert_eq!(world.read(&mut parent, at + PAGE_SIZE, 1), [0x5a]);
        assert_eq!(frame_at(&parent, at + PAGE_SIZE), frame_at(&child, at + PAGE_SIZE));
        // And the other way, and a read first.
        assert_eq!(world.read(&mut child, at + 2 * PAGE_SIZE, 1), [0]);
        world.write(&mut parent, at + 2 * PAGE_SIZE, &[0x6b]);
        assert_eq!(world.read(&mut child, at + 2 * PAGE_SIZE, 1), [0x6b]);
        assert_eq!(world.promises.held(), 1);
        both_done(world, parent, child);
    });
}

#[test]
fn fork_debt_is_sum_of_extra_refs() {
    on_both(|bits| {
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let at = world.anywhere(&mut parent, 10 * PAGE_SIZE, RW);
        for page in 0..4 {
            world.write(&mut parent, at + page * PAGE_SIZE, &[page as u8]);
        }
        // Read-only pages with frames of their own are charged too: `mprotect` could make
        // them writable without asking the fork again.
        let image = world.map(&mut parent, image(PAGE_SIZE, RO, LOW + 512 * PAGE_SIZE)).unwrap();
        assert_eq!(world.promises.held(), 6);
        let first = fork_of(&mut world, &mut parent);
        // The child's six untouched charged pages, and one per frame it shares.
        assert_eq!(world.promises.held(), 6 + 6 + 5);
        let second = world.with(|ctx| parent.fork(3, ctx)).unwrap();
        assert_eq!(world.promises.held(), 6 * 3 + 5 * 2);
        assert_eq!(world.pool.live[&frame_at(&parent, image)].0, 3);
        owed(&world, &[&parent, &first, &second]);
        // However they go, it comes back to the parent's own.
        first.destroy(&mut world.pool, &world.promises);
        owed(&world, &[&parent, &second]);
        assert_eq!(world.promises.held(), 6 * 2 + 5);
        both_done(world, parent, second);
    });
}

#[test]
fn cow_copy_consumes_promise() {
    on_both(|bits| {
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let at = world.anywhere(&mut parent, 8 * PAGE_SIZE, RW);
        for page in 0..8 {
            world.write(&mut parent, at + page * PAGE_SIZE, &[page as u8]);
        }
        let mut child = fork_of(&mut world, &mut parent);
        assert_eq!(world.promises.held(), 8);
        // No frame to spare beyond what's promised: every copy still comes, each with
        // the promise its reference held, and nothing else can be had.
        world.pool.limit = world.pool.live.len() + world.promises.held();
        assert!(!world.promises.promise(1, world.pool.free_count()));
        for page in 0..8 {
            let space = if page % 2 == 0 { &mut child } else { &mut parent };
            world.write(space, at + page * PAGE_SIZE, &[0xf0 | page as u8]);
        }
        assert_eq!((world.promises.held(), world.pool.free_count()), (0, 0));
        owed(&world, &[&parent, &child]);
        world.pool.limit = usize::MAX;
        both_done(world, parent, child);
    });
}

#[test]
fn unmap_of_shared_page_releases_one() {
    on_both(|bits| {
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let at = world.anywhere(&mut parent, 3 * PAGE_SIZE, RW);
        for page in 0..3 {
            world.write(&mut parent, at + page * PAGE_SIZE, &[page as u8]);
        }
        let mut child = fork_of(&mut world, &mut parent);
        assert_eq!(world.promises.held(), 3);
        let frame = frame_at(&parent, at);
        world.with(|ctx| child.unmap(at, PAGE_SIZE, ctx)).unwrap();
        assert_eq!((world.promises.held(), world.pool.live[&frame].0), (2, 1));
        // A discard lets go of the frame but keeps the page's own promise.
        world.with(|ctx| child.discard(at + PAGE_SIZE, PAGE_SIZE, ctx)).unwrap();
        assert_eq!(world.promises.held(), 2, "one debt gone, the page's promise back");
        // The last reference gives back nothing more.
        world.with(|ctx| parent.unmap(at, PAGE_SIZE, ctx)).unwrap();
        assert_eq!(world.promises.held(), 2);
        owed(&world, &[&parent, &child]);
        both_done(world, parent, child);
    });
}

#[test]
fn fork_frees_all_on_exit() {
    on_both(|bits| {
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let at = world.anywhere(&mut parent, 16 * PAGE_SIZE, RW);
        let shared = world.map(&mut parent, MapRequest { shared: true, ..MapRequest::anonymous(4 * PAGE_SIZE, RW) }).unwrap();
        let file = world.files.add(23, 2 * PAGE_SIZE, 0x23);
        let mapped = world.map(&mut parent, file_request(&file, 2 * PAGE_SIZE, RW, false)).unwrap();
        world.read(&mut parent, mapped, 1);
        for page in 0..8 {
            world.write(&mut parent, at + page * PAGE_SIZE, &[1]);
        }
        world.write(&mut parent, shared, &[1]);
        let (used, held, tables) = (world.in_use(), world.promises.held(), parent.tables());
        for round in 0..100 {
            let mut child = world.with(|ctx| parent.fork(2 + round, ctx)).unwrap();
            // Writes in both, in an order that changes round to round.
            let page = round as usize % 16;
            world.write(&mut child, at + page * PAGE_SIZE, &[2]);
            world.write(&mut parent, at + ((page + 3) % 16) * PAGE_SIZE, &[3]);
            world.write(&mut child, mapped + PAGE_SIZE, &[4]);
            world.write(&mut child, shared + PAGE_SIZE, &[5]);
            owed(&world, &[&parent, &child]);
            child.destroy(&mut world.pool, &world.promises);
            owed(&world, &[&parent]);
        }
        // Counts back where they were, give or take the pages the parent wrote, which
        // the promises it held for them paid for.
        assert_eq!(parent.tables(), tables);
        assert_eq!(world.in_use() + world.promises.held(), used + held + 1, "one shared page the children touched");
        consistent(&world, &parent);
        world.done(parent);
    });
}

#[test]
fn fork_refused_changes_nothing() {
    on_both(|bits| {
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let at = world.anywhere(&mut parent, 64 * PAGE_SIZE, RW);
        world.write(&mut parent, at, &[1]);
        // Not enough frames for the child's promises: refused, the parent as it was.
        world.pool.limit = world.pool.live.len() + world.promises.held() + 10;
        let (used, held) = (world.in_use(), world.promises.held());
        let refused = world.with(|ctx| parent.fork(2, ctx));
        assert_eq!(refused.err(), Some(MmError::NoMemory));
        assert_eq!((world.in_use(), world.promises.held()), (used, held));
        world.write(&mut parent, at, &[2]);
        assert_eq!(world.read(&mut parent, at, 1), [2]);
        world.pool.limit = usize::MAX;
        consistent(&world, &parent);
        world.done(parent);
    });
}

#[test]
fn cow_bit_is_56_on_both() {
    assert_eq!(crate::COW, 1 << 56);
    for bits in [BITS, ARM_BITS] {
        let port = bits.present | bits.address | bits.leaf | bits.writable | bits.read_only | bits.no_execute;
        assert_eq!(port & crate::COW, 0, "the hardware's bits never include it");
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let at = world.anywhere(&mut parent, 2 * PAGE_SIZE, RW);
        world.write(&mut parent, at, &[1]);
        world.write(&mut parent, at + PAGE_SIZE, &[1]);
        world.with(|ctx| parent.protect(at + PAGE_SIZE, PAGE_SIZE, Prot::NONE, ctx)).unwrap();
        let child = fork_of(&mut world, &mut parent);
        let mut raw = Vec::new();
        parent.space().visit(at, at + 2 * PAGE_SIZE, &mut |_, entry| raw.push(entry));
        assert!(matches!(raw[0], Entry::Present { cow: true, writable: false, .. }));
        assert!(matches!(raw[1], Entry::Hidden { cow: true, .. }), "kept through no access");
        // Made writable again, it stays shared and read-only.
        world.with(|ctx| parent.protect(at + PAGE_SIZE, PAGE_SIZE, RW, ctx)).unwrap();
        assert!(is_cow(&parent, at + PAGE_SIZE) && parent.space().translate(at + PAGE_SIZE, true).is_err());
        both_done(world, parent, child);
    }
}

#[test]
fn shared_pieces_counted_per_space() {
    on_both(|bits| {
        let mut world = World::new();
        let mut parent = forking(&mut world, bits);
        let request = MapRequest { shared: true, ..MapRequest::anonymous(4 * PAGE_SIZE, RW) };
        let at = world.map(&mut parent, request).unwrap();
        world.write(&mut parent, at, &[1]);
        let mut child = fork_of(&mut world, &mut parent);
        let heap = SharedAnon::heap_cost(4);
        assert_eq!((child.charged(), child.object_heap()), (4, heap));
        // Cut in three in the child: the object counts there until its last piece goes.
        world.with(|ctx| child.protect(at + PAGE_SIZE, PAGE_SIZE, RO, ctx)).unwrap();
        assert_eq!(child.vmas().len(), 3);
        for (piece, pages) in [(0, 1), (2, 2)] {
            world.with(|ctx| child.unmap(at + piece * PAGE_SIZE, pages * PAGE_SIZE, ctx)).unwrap();
            assert_eq!((child.charged(), child.object_heap()), (4, heap));
        }
        world.with(|ctx| child.unmap(at + PAGE_SIZE, PAGE_SIZE, ctx)).unwrap();
        assert_eq!((child.charged(), child.object_heap()), (0, 0));
        // The parent's count, and the object, are as they were.
        assert_eq!((parent.charged(), parent.object_heap()), (4, heap));
        assert_eq!(world.read(&mut parent, at, 1), [1]);
        owed(&world, &[&parent, &child]);
        both_done(world, parent, child);
    });
}

#[test]
fn many_shared_objects_stay_cheap() {
    // Thousands of one-page objects, as `malicious --shared-flood` makes: mapped,
    // forked, touched and unmapped without a pass over all of them for each.
    let mut world = World::new();
    let mut parent = world.mm();
    let started = std::time::Instant::now();
    let mut at = Vec::new();
    for _ in 0..8000 {
        let request = MapRequest { shared: true, ..MapRequest::anonymous(PAGE_SIZE, RW) };
        at.push(world.map(&mut parent, request).unwrap());
    }
    let child = fork_of(&mut world, &mut parent);
    for &address in at.iter().step_by(7) {
        world.write(&mut parent, address, &[1]);
    }
    for &address in &at {
        world.with(|ctx| parent.unmap(address, PAGE_SIZE, ctx)).unwrap();
    }
    assert_eq!((parent.charged(), parent.object_heap()), (0, 0));
    child.destroy(&mut world.pool, &world.promises);
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "took {:?}", started.elapsed());
    world.done(parent);
}
