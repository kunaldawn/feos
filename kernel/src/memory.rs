//! The machine's RAM: what firmware says there is, who has which of it.
//!
//! Boot adds the RAM firmware reports to a frame allocator and takes out the kernel
//! image and what the board says is below it: low memory on a PC, the device tree on
//! `virt`. Anything else firmware needs is outside what it reported as RAM. The heap's
//! arena comes out of what's left, and later callers such as the page table code
//! ask for frames here.

use feos_frames::{flag, CountError, Frames, FRAME_SIZE};
use feos_hal::Board;
use feos_sync::IrqSpinLock;

use feos_hal::Arch;

use crate::{Machine, Port};

/// Words of bitmap: 128 KiB, enough for 4 GiB of frames, the most either board maps.
const BITMAP_WORDS: usize = 16 * 1024;

static FRAMES: IrqSpinLock<Frames<BITMAP_WORDS>> = IrqSpinLock::new(Frames::new(0));

/// The heap never gets less than this if the machine has it, which is what the
/// video player and the writable filesystems were sized for.
const HEAP_PREFERRED: u64 = 64 * 1024 * 1024;
/// More heap than this has no user.
const HEAP_MAX: u64 = 1024 * 1024 * 1024;
/// What's assumed to follow the kernel when firmware doesn't say.
const FALLBACK_RAM: u64 = 64 * 1024 * 1024;

/// The heap's share of what's free: half, past the preferred size, or as much as
/// fits while leaving others something.
const fn heap_size(free: u64) -> u64 {
    let half = free / 2;
    let wanted = if half > HEAP_PREFERRED { half } else { HEAP_PREFERRED };
    let wanted = if wanted > HEAP_MAX { HEAP_MAX } else { wanted };
    let most = free - free / 8;
    (if wanted < most { wanted } else { most }) & !(FRAME_SIZE - 1)
}

/// Finds the machine's RAM and gives the heap its arena.
pub fn init() {
    let (kernel_start, kernel_end) = Machine::kernel_range();
    let (kernel_start, kernel_end) = (kernel_start as u64, kernel_end as u64);

    let mut ranges = [(0u64, 0u64); 16];
    let count = Machine::usable_ram(&mut ranges);

    let mut frames = FRAMES.lock();
    let mut fits = false;
    let mut ram_end = 0;
    for &(base, size) in &ranges[..count] {
        let end = base.saturating_add(size);
        fits |= base <= kernel_end && kernel_end <= end;
        let usable = end.min(Machine::RAM_LIMIT).saturating_sub(base);
        if usable > 0 {
            ram_end = ram_end.max(base + usable);
        }
        // SAFETY: firmware says it's RAM, and the other CPUs haven't started.
        unsafe { Port::map_ram_cached(base as usize, usable as usize) };
        frames.add_ram(base, usable);
    }
    if count == 0 {
        feos_console::println!("[mem] firmware reports no RAM; assuming a little past the kernel");
        frames.add_ram(kernel_end, FALLBACK_RAM);
    } else if !fits {
        // QEMU loads the image regardless, so this may not get far, but what's
        // past the end of RAM reads as nothing and the report says why.
        panic!("the kernel image ends at {kernel_end:#x}, past the RAM firmware reports");
    }
    frames.reserve(0, kernel_start);
    frames.reserve(kernel_start, kernel_end - kernel_start);

    let total = frames.total_bytes();
    let heap = heap_size(frames.free_bytes());
    let frame_count = (heap / FRAME_SIZE) as usize;
    let Some(arena) = frames.alloc(frame_count, 1) else {
        panic!("no run of {heap} bytes for the heap in {total} bytes of RAM");
    };
    let free = frames.free_bytes();
    drop(frames);

    // SAFETY: these frames were just taken for this, and are never given back.
    unsafe { crate::allocator::init(arena as usize, heap as usize) };

    feos_console::println!(
        "[mem] {} MiB of RAM in {} range(s); heap {} MiB at {arena:#x}, {} MiB of frames free",
        total / (1024 * 1024),
        count.max(1),
        heap / (1024 * 1024),
        free / (1024 * 1024)
    );

    // Task stacks' window, from frames now that the kernel's are out of the way, and
    // before anything maps a process: then the check covers it too.
    crate::kstack::init();
    check_below_user_window(ram_end);
}

/// Stops the machine unless the RAM it uses and everything the kernel maps end below
/// the lowest user address. Every process's top table holds the kernel's entries beside
/// its own, so a kernel mapping inside the window would be a range a process's tables
/// claim and the kernel's own entry overrides, or the other way round. Device windows
/// are all in the kernel's map, so checking the map covers every one the boards use: a
/// machine that put one higher (a big guest's 64-bit PCI hole on a PC, say) has to have
/// it mapped somewhere else first.
fn check_below_user_window(ram_end: u64) {
    let window = feos_abi::USER_BASE;
    let mapped = Port::kernel_map_end();
    if ram_end > window as u64 || mapped > window {
        panic!(
            "the kernel's memory reaches into the user window at {window:#x}: RAM in use ends at {ram_end:#x}, \
             the kernel's mappings at {mapped:#x}"
        );
    }
    feos_console::println!(
        "[mem] kernel mappings end at {mapped:#x}, RAM in use at {ram_end:#x}: below the user window at {window:#x}"
    );
}

/// Takes `count` contiguous frames, aligned to `align` frames, and returns the
/// address of the first. They're mapped already: the identity map covers RAM.
pub fn alloc_frames(count: usize, align: usize) -> Option<usize> {
    FRAMES.lock().alloc(count, align).map(|address| address as usize)
}

/// Returns frames from [`alloc_frames`].
pub fn free_frames(address: usize, count: usize) {
    if let Err(error) = FRAMES.lock().free(address as u64, count) {
        panic!("bad free of {count} frames at {address:#x}: {error:?}");
    }
}

/// Takes one frame holding one reference, the caller's: see `feos_frames`'s counts. It
/// goes back when the last reference and pin go.
pub fn alloc_counted() -> Option<usize> {
    FRAMES.lock().alloc_counted().map(|address| address as usize)
}

/// Another reference to a counted frame. `false` at the cap.
pub fn get_frame(address: usize) -> bool {
    match FRAMES.lock().get(address as u64) {
        Ok(()) => true,
        Err(CountError::Saturated) => false,
        Err(error) => panic!("bad reference to the frame at {address:#x}: {error:?}"),
    }
}

/// Drops a reference to a counted frame.
pub fn put_frame(address: usize) {
    if let Err(error) = FRAMES.lock().put(address as u64) {
        panic!("bad put of the frame at {address:#x}: {error:?}");
    }
}

/// Drops a reference to a counted frame, and if that left a frame of the page cache's
/// with nothing but the cache's own reference, counts it as idle in the cache.
///
/// The count is taken in the same critical section as the drop: once the lock goes,
/// another CPU can take the frame again (a hit) or free it (reclaim), and either takes
/// one off the idle count, which must not run before this adds it or the count wraps.
pub fn put_frame_noting(address: usize) {
    let mut frames = FRAMES.lock();
    let cached = frames.meta(address as u64).is_ok_and(|meta| meta.has(flag::PAGE_CACHE));
    match frames.put(address as u64) {
        Ok(true) => {}
        Ok(false) => {
            if cached && frames.meta(address as u64).is_ok_and(|meta| meta.maps() == 1) {
                crate::pagecache::note_unmapped();
            }
        }
        Err(error) => panic!("bad put of the frame at {address:#x}: {error:?}"),
    }
}

/// A counted frame for the page cache, marked as its.
pub fn alloc_cache_frame() -> Option<usize> {
    let mut frames = FRAMES.lock();
    let address = frames.alloc_counted()?;
    let _ = frames.set_flags(address, flag::PAGE_CACHE, true);
    Some(address as usize)
}

/// Another reference to a counted frame, and how many there are now. `None` at the cap.
pub fn get_frame_counted(address: usize) -> Option<u32> {
    let mut frames = FRAMES.lock();
    match frames.get(address as u64) {
        Ok(()) => frames.meta(address as u64).ok().map(|meta| meta.maps()),
        Err(CountError::Saturated) => None,
        Err(error) => panic!("bad reference to the frame at {address:#x}: {error:?}"),
    }
}

/// The page cache lets one of its frames go: no longer marked its, and its reference
/// dropped. Says whether that freed it.
pub fn drop_cache_frame(address: usize) -> bool {
    let mut frames = FRAMES.lock();
    let _ = frames.set_flags(address as u64, flag::PAGE_CACHE, false);
    match frames.put(address as u64) {
        Ok(freed) => freed,
        Err(error) => panic!("bad drop of the cache's frame at {address:#x}: {error:?}"),
    }
}

/// References to a counted frame now.
pub fn frame_refs(address: usize) -> u32 {
    FRAMES.lock().meta(address as u64).map_or(0, |meta| meta.maps())
}

/// Pins a counted frame for a copy through it. `false` at the cap.
pub fn pin_frame(address: usize) -> bool {
    match FRAMES.lock().pin(address as u64) {
        Ok(()) => true,
        Err(CountError::Saturated) => false,
        Err(error) => panic!("bad pin of the frame at {address:#x}: {error:?}"),
    }
}

/// Lets a pinned frame go.
pub fn unpin_frame(address: usize) {
    if let Err(error) = FRAMES.lock().unpin(address as u64) {
        panic!("bad unpin of the frame at {address:#x}: {error:?}");
    }
}

/// Bytes of RAM firmware reported, within what's mapped.
pub fn total_bytes() -> usize {
    FRAMES.lock().total_bytes() as usize
}

/// Bytes of frames free to hand out.
pub fn free_bytes() -> usize {
    FRAMES.lock().free_bytes() as usize
}
