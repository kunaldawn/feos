//! Kernel stacks: one per task slot, made from frames when the slot is taken, in a
//! window of the kernel's address space kept for them.
//!
//! A thousand slots of 256 KiB stacks would be a quarter of a gigabyte, so stacks can't
//! sit in `.bss` as they did with 32 slots. Each slot instead has [`STRIDE`] of the
//! window at `WINDOW_BASE + slot * STRIDE`, and its stack is the top of that: 256 KiB
//! for a kernel-linked app, 64 KiB for a user thread, which runs on its kernel stack
//! only while in a call or a fault. The rest of the slot's stretch is never mapped, so
//! an overflow faults at once however big the frame that overflows: the guard gap is
//! at least a quarter of a megabyte. The window's tables are made at boot (see
//! `Arch::reserve_kernel_window`), so mapping a stack never allocates a table, and no
//! table ever changes under another CPU or a process's copy of the top-level entries.
//!
//! A stack's frames are contiguous. A driver handed a buffer on a stack gives the device
//! a bus address, and [`bus_address`] turns the window address into the frames' through
//! the hook in `feos_hal::dma`: a buffer on one stack is contiguous on the bus too.
//!
//! # A stack's life
//!
//! `NONE` (nothing mapped) becomes `BUSY` when a spawn [`take`]s the slot's stack: the
//! frames are allocated and mapped then, or the previous stack is reused if it's the
//! right size and still there. When the slot's task has left the table and been
//! collected, nothing runs on the stack any more and it's [`idle`]: still mapped. The
//! reaper, and anything counting frames, [`trim`]s idle stacks, which goes through
//! `TRIMMING` back to `NONE`: the pages are unmapped, every CPU forgets them, and the
//! frames go back. A reader of a stack that isn't its own ([`high_water`], for `ps`)
//! holds the slot's reader count, which keeps a trim off until it's done.
//!
//! Whoever moves a stack out of `IDLE`, or holds a reader count, can't be killed until
//! it has finished: a kernel app reaped there would leave a stack `TRIMMING`, which
//! [`trim`] would wait for forever, or a reader counted that never goes, or a stack
//! `BUSY` with nothing mapped. Each holds a [`NoKill`] for it, as a lock is held.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};

use feos_hal::{Arch, Board};

use crate::sched::NoKill;
use crate::{memory, Machine, Port};

/// Slots, the boot task's included: it has the linker's stack, and its stretch of the
/// window goes unused.
const SLOTS: usize = feos_app::MAX_PROCESSES;

/// Where the window starts: the gigabyte below the one AArch64's ECAM is mapped at, the
/// last two below the user window. Nothing of either port's identity map is there.
pub const WINDOW_BASE: usize = 62 << 30;

/// How much of the window each slot has: the most a stack can be, and its guard gap.
pub const STRIDE: usize = 512 * 1024;

/// A kernel-linked app's stack.
pub const KERNEL_STACK: usize = 256 * 1024;

/// A user thread's kernel stack, for its calls and faults. `ktest kstack` checks the
/// deepest calls leave half of it unused.
pub const USER_STACK: usize = 64 * 1024;

const PAGE: usize = 4096;

const WINDOW_SIZE: usize = SLOTS * STRIDE;

const _: () = {
    assert!(KERNEL_STACK < STRIDE && USER_STACK < STRIDE);
    assert!(WINDOW_SIZE <= 1 << 30, "the window is one top-level entry");
    assert!(WINDOW_BASE + WINDOW_SIZE <= 63 << 30, "the window overlaps AArch64's ECAM");
    assert!(WINDOW_BASE + WINDOW_SIZE <= feos_abi::USER_BASE);
    assert!(Machine::RAM_LIMIT as usize <= WINDOW_BASE, "RAM reaches the stack window");
};

/// Words at the bottom of a stack that must never change.
pub const CANARY_WORDS: usize = 8;
pub const CANARY: u64 = 0xdead_57ac_dead_57ac;
/// Fills the rest of a stack, so the high-water mark can be measured.
const STACK_FILL: u64 = 0x5a5a_5a5a_5a5a_5a5a;

const NONE: u8 = 0;
const BUSY: u8 = 1;
const IDLE: u8 = 2;
const TRIMMING: u8 = 3;

/// Each slot's stack: `NONE`, `BUSY`, `IDLE` or `TRIMMING`, as the module describes.
static STATE: [AtomicU8; SLOTS] = [const { AtomicU8::new(NONE) }; SLOTS];
/// The first of each slot's stack's frames, and its size: set while it's taken, and
/// read by [`bus_address`] for a buffer on it, which only its own task can have.
static FRAMES: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
static SIZE: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
/// Readers of each slot's stack other than its task.
static READERS: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];

/// The window is there: until it is, nothing is translated.
static READY: AtomicBool = AtomicBool::new(false);

/// The most of a user thread's stack, and of a kernel app's, any task had used by the
/// time it gave its stack back, since boot or [`reset_peaks`].
static USER_PEAK: AtomicUsize = AtomicUsize::new(0);
static KERNEL_PEAK: AtomicUsize = AtomicUsize::new(0);

/// Makes the window. On the boot CPU, before any process's tables are made and before
/// the other CPUs start.
pub fn init() {
    let mut alloc_table = || memory::alloc_frames(1, 1);
    // SAFETY: nothing is in the window yet, no process has tables, and the other CPUs
    // haven't started.
    if !unsafe { Port::reserve_kernel_window(WINDOW_BASE, WINDOW_SIZE, &mut alloc_table) } {
        panic!("no room for the kernel stack window at {WINDOW_BASE:#x}");
    }
    feos_hal::dma::set_translation(bus_address);
    READY.store(true, Ordering::Release);
    feos_console::println!(
        "[mem] kernel stacks: {SLOTS} slots in a window at {WINDOW_BASE:#x}, {} KiB for kernel apps, {} KiB for user threads",
        KERNEL_STACK / 1024,
        USER_STACK / 1024
    );
}

/// One past the top of `slot`'s stack.
pub const fn top(slot: usize) -> usize {
    WINDOW_BASE + (slot + 1) * STRIDE
}

/// The slot whose stretch of the window `address` is in.
fn slot_of(address: usize) -> Option<usize> {
    let offset = address.checked_sub(WINDOW_BASE)?;
    let slot = offset / STRIDE;
    (slot < SLOTS).then_some(slot)
}

/// `[bottom, top)` of `slot`'s stack as it's taken now.
pub fn range(slot: usize) -> (usize, usize) {
    let size = SIZE[slot].load(Ordering::Relaxed);
    (top(slot) - size, top(slot))
}

/// If `address` is in the guard gap below `slot`'s stack, the bounds of that stack.
pub fn overflow_of(slot: usize, address: usize) -> Option<(usize, usize)> {
    let (bottom, top) = range(slot);
    (slot_of(address) == Some(slot) && address < bottom).then_some((bottom, top))
}

/// The bus address of `address`, for a device: see `feos_hal::dma`. `None` outside the
/// window, where the identity map's address is the bus address.
fn bus_address(address: usize) -> Option<u64> {
    let slot = slot_of(address)?;
    let frames = FRAMES[slot].load(Ordering::Acquire);
    let (bottom, _) = range(slot);
    (frames != 0 && address >= bottom).then(|| (frames + (address - bottom)) as u64)
}

/// Fills the words `[bottom, end)` of a stack, with the canary at the bottom.
///
/// # Safety
/// The range must be unused stack memory.
pub unsafe fn paint(bottom: usize, end: usize) {
    let words = (end - bottom) / core::mem::size_of::<u64>();
    let stack = bottom as *mut u64;
    for word in 0..words {
        let value = if word < CANARY_WORDS { CANARY } else { STACK_FILL };
        // SAFETY: the caller owns the range, and stacks are 16-byte aligned.
        unsafe { stack.add(word).write_volatile(value) };
    }
}

/// Bytes of the stack starting at `bottom` that have ever been used.
///
/// The range must be mapped for as long as this runs: the boot stack, a stack the
/// caller runs on or has taken, or one [`high_water`] holds a reader count on.
pub fn measure(bottom: usize, size: usize) -> usize {
    if bottom == 0 {
        return 0;
    }
    let total = size / core::mem::size_of::<u64>();
    let stack = bottom as *const u64;
    for word in CANARY_WORDS..total {
        // SAFETY: as above. Its owner may be writing to it, which at worst makes this
        // estimate slightly stale.
        if unsafe { stack.add(word).read_volatile() } != STACK_FILL {
            return (total - word) * core::mem::size_of::<u64>();
        }
    }
    0
}

/// How much of `slot`'s stack its task has used, or zero if it has none: for `ps`, from
/// another task, so a trim waits until it's done.
pub fn high_water(slot: usize) -> usize {
    let _no_kill = NoKill::new();
    // Dekker's way with a trim (see `start_trim`): either this sees the trim's state, or
    // the trim sees this reader.
    READERS[slot].fetch_add(1, Ordering::SeqCst);
    let state = STATE[slot].load(Ordering::SeqCst);
    let used = if state == BUSY || state == IDLE {
        let (bottom, top) = range(slot);
        measure(bottom, top - bottom)
    } else {
        0
    };
    READERS[slot].fetch_sub(1, Ordering::SeqCst);
    used
}

/// The most of a user thread's kernel stack, and of a kernel app's, any task had used
/// by when it gave its stack back.
pub fn peaks() -> (usize, usize) {
    (USER_PEAK.load(Ordering::Acquire), KERNEL_PEAK.load(Ordering::Acquire))
}

/// Starts [`peaks`] over.
pub fn reset_peaks() {
    USER_PEAK.store(0, Ordering::Release);
    KERNEL_PEAK.store(0, Ordering::Release);
}

/// Bytes of frames in stacks now, taken or idle.
pub fn bytes() -> usize {
    (1..SLOTS)
        .filter(|&slot| STATE[slot].load(Ordering::Relaxed) != NONE)
        .map(|slot| SIZE[slot].load(Ordering::Relaxed))
        .sum()
}

/// Reader counts held, and stacks being given back, now: both zero once nobody is
/// reading or trimming, for `ktest kstack-kill` to check nothing was left behind.
pub fn in_flight() -> (u32, usize) {
    let readers = (1..SLOTS).map(|slot| READERS[slot].load(Ordering::Acquire)).sum();
    let trimming = (1..SLOTS).filter(|&slot| STATE[slot].load(Ordering::Acquire) == TRIMMING).count();
    (readers, trimming)
}

/// Notes how much of `slot`'s stack was used, as it's given back or handed on.
fn note_peak(slot: usize) {
    let (bottom, top) = range(slot);
    let used = measure(bottom, top - bottom);
    let peak = if top - bottom == USER_STACK { &USER_PEAK } else { &KERNEL_PEAK };
    peak.fetch_max(used, Ordering::AcqRel);
}

/// Takes `slot`'s stack, `size` bytes, for a task about to be started in the slot,
/// which the caller has reserved: the one already there if it's that size, or a new
/// one. `None` if there aren't the frames. In task context, with interrupts on: giving
/// a stack of the wrong size back may have to ask other CPUs to forget it.
pub fn take(slot: usize, size: usize) -> Option<(usize, usize)> {
    debug_assert!(slot != 0 && slot < SLOTS && size.is_multiple_of(PAGE));
    let _no_kill = NoKill::new();
    loop {
        match STATE[slot].load(Ordering::Acquire) {
            NONE => {
                if STATE[slot].compare_exchange(NONE, BUSY, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    break;
                }
            }
            IDLE => {
                // A reader, from `ps`, is done in a moment: the stack is still idle
                // for it, and this tries again once it has had the CPU.
                if !start_trim(slot) {
                    crate::sched::yield_now();
                } else {
                    note_peak(slot);
                    if SIZE[slot].load(Ordering::Relaxed) == size {
                        STATE[slot].store(BUSY, Ordering::Release);
                        return Some(range(slot));
                    }
                    unmap(slot);
                    forget_everywhere();
                    release(slot);
                    STATE[slot].store(BUSY, Ordering::Release);
                    break;
                }
            }
            // The reaper is giving it back; it'll be `NONE` in a moment.
            TRIMMING => crate::sched::yield_now(),
            _ => panic!("the kernel stack of slot {slot} was taken while its last task still had it"),
        }
    }
    let Some(frames) = memory::alloc_frames(size / PAGE, 1) else {
        STATE[slot].store(NONE, Ordering::Release);
        return None;
    };
    let bottom = top(slot) - size;
    for page in 0..size / PAGE {
        // SAFETY: the slot's stretch of the window is unmapped while it's `NONE`, which
        // this has just taken it from, and the frames were just allocated.
        unsafe { Port::map_kernel_page(bottom + page * PAGE, frames + page * PAGE) };
    }
    SIZE[slot].store(size, Ordering::Relaxed);
    FRAMES[slot].store(frames, Ordering::Release);
    Some((bottom, top(slot)))
}

/// Nothing runs on `slot`'s stack any more: its task has left the table and been
/// collected. Under the scheduler lock, so it only marks it.
pub fn idle(slot: usize) {
    if slot != 0 && slot < SLOTS {
        let _ = STATE[slot].compare_exchange(BUSY, IDLE, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// Moves an idle stack to `TRIMMING`, unless it's being read: then it leaves it idle and
/// says no.
fn start_trim(slot: usize) -> bool {
    if STATE[slot].compare_exchange(IDLE, TRIMMING, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return false;
    }
    if READERS[slot].load(Ordering::SeqCst) != 0 {
        STATE[slot].store(IDLE, Ordering::SeqCst);
        return false;
    }
    true
}

/// Takes the pages of `slot`'s stack out of the window.
fn unmap(slot: usize) {
    let (bottom, top) = range(slot);
    for page in (bottom..top).step_by(PAGE) {
        // SAFETY: the stack is `TRIMMING`: nothing runs on it or reads it.
        unsafe { Port::unmap_kernel_page(page) };
    }
}

/// Has every CPU forget the pages just unmapped, where unmapping didn't already.
fn forget_everywhere() {
    if !Port::USER_FLUSH_BROADCAST {
        crate::user::tlb::shootdown_all();
    }
}

/// Gives back the frames of `slot`'s stack, unmapped and forgotten.
fn release(slot: usize) {
    let frames = FRAMES[slot].swap(0, Ordering::AcqRel);
    let size = SIZE[slot].swap(0, Ordering::Relaxed);
    if frames != 0 {
        memory::free_frames(frames, size / PAGE);
    }
}

/// Gives back every idle stack, and returns how many. In task context, with interrupts
/// on: by the reaper as tasks go, and by whatever counts free frames, so it also waits
/// for any stack another task is giving back: once it returns, every stack that was idle
/// is back.
pub fn trim() -> usize {
    if !READY.load(Ordering::Acquire) {
        return 0;
    }
    let count = trim_idle();
    while (1..SLOTS).any(|slot| STATE[slot].load(Ordering::Acquire) == TRIMMING) {
        crate::sched::yield_now();
    }
    count
}

fn trim_idle() -> usize {
    let _no_kill = NoKill::new();
    let mut taken = [0u16; SLOTS];
    let mut count = 0;
    for slot in 1..SLOTS {
        if STATE[slot].load(Ordering::Relaxed) == IDLE && start_trim(slot) {
            note_peak(slot);
            unmap(slot);
            taken[count] = slot as u16;
            count += 1;
        }
    }
    if count == 0 {
        return 0;
    }
    // Once for them all: on x86_64, each is an interrupt to every other CPU.
    forget_everywhere();
    for &slot in &taken[..count] {
        release(slot as usize);
        STATE[slot as usize].store(NONE, Ordering::Release);
    }
    count
}
