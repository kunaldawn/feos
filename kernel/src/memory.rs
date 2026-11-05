//! The machine's RAM: what firmware says there is, who has which of it.
//!
//! Boot adds the RAM firmware reports to a frame allocator and takes out the kernel
//! image and what the board says is below it: low memory on a PC, the device tree on
//! `virt`. Anything else firmware needs is outside what it reported as RAM. The heap's
//! arena comes out of what's left, and later callers such as the page table code
//! ask for frames here.

use feos_frames::{Frames, FRAME_SIZE};
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
    for &(base, size) in &ranges[..count] {
        let end = base.saturating_add(size);
        fits |= base <= kernel_end && kernel_end <= end;
        let usable = end.min(Machine::RAM_LIMIT).saturating_sub(base);
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
    frames.reserve(0, Machine::LOW_RESERVED.min(kernel_start));
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

/// Bytes of RAM firmware reported, within what's mapped.
pub fn total_bytes() -> usize {
    FRAMES.lock().total_bytes() as usize
}

/// Bytes of frames free to hand out.
pub fn free_bytes() -> usize {
    FRAMES.lock().free_bytes() as usize
}
