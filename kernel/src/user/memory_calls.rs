//! A program's memory: its address space, the calls that change it, and its faults.
//!
//! What's mapped and how is `feos_vm::Mm`'s business; this is the boundary around it:
//! the arguments of `mmap`, `mmap2`, `munmap`, `mprotect`, `madvise` and `msync`
//! checked as `docs/feos-abi-v2.md` says, frames from the kernel's allocator, the
//! machine's promises, and futex waiters woken when their words are unmapped; and the
//! points where what shared writable file mappings dirtied is written back (`MSYNC`,
//! `MUNMAP`, a `MAP_FIXED` over one, and the space's end). Each call holds the space's
//! lock for what it changes (see `process::UserMm`), and lets it go before it writes
//! back or reads ahead through the page cache.

use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::vec::Vec;

use feos_abi::{errno, madv, map, ms, prot as prot_bits};
use feos_fs::vfs::DeviceKind;
use feos_fs::FileKind;
use feos_hal::{Arch, MemoryFault};
use feos_vm::vma::Backing;
use feos_vm::{
    Access, FaultError, FileKey, FileRef, FrameSource, Layout, MapRequest, Mm, MmError, Place, Promises, Prot, PAGE_SIZE,
};

use super::process::{MmState, UserMm};
use super::tlb;
use crate::pagecache;
use crate::{memory, Port};

/// Frames for user memory and page tables come straight from the kernel's allocator,
/// counted, so a frame goes when the last thing pointing at it does.
pub(super) struct KernelFrames;

impl FrameSource for KernelFrames {
    fn alloc(&mut self) -> Option<usize> {
        // A page nothing maps is a frame to be had: the promises count them.
        loop {
            if let Some(frame) = memory::alloc_counted() {
                return Some(frame);
            }
            if !pagecache::reclaim_one() {
                return None;
            }
        }
    }

    fn free(&mut self, frame: usize) {
        memory::put_frame(frame);
    }

    fn get(&mut self, frame: usize) -> bool {
        memory::get_frame(frame)
    }

    fn put(&mut self, frame: usize) {
        // The cache counts the pages nothing maps, which can be taken back at once.
        memory::put_frame_noting(frame);
    }

    fn put_shared(&mut self, frame: usize) -> bool {
        memory::put_frame_if_shared(frame)
    }

    fn pins(&self, frame: usize) -> u32 {
        memory::frame_pins(frame)
    }

    fn maps(&self, frame: usize) -> u32 {
        memory::frame_refs(frame)
    }

    fn pin(&mut self, frame: usize) -> bool {
        memory::pin_frame(frame)
    }

    fn unpin(&mut self, frame: usize) {
        memory::unpin_frame(frame);
    }

    fn free_count(&self) -> usize {
        memory::free_bytes() / PAGE_SIZE + pagecache::reclaimable()
    }
}

/// Frames no promise may count on: page tables, kernel stacks and the rest come from
/// the same allocator, and have to be there when a promise is kept.
pub const SPARE_FRAMES: usize = 4096;

/// Pages promised to programs, across the machine, that don't have frames yet.
pub(super) static PROMISES: Promises = Promises::new(SPARE_FRAMES);

/// The frame every untouched private page reads as. Made once, at boot, and never
/// written or freed.
static ZERO_FRAME: AtomicUsize = AtomicUsize::new(0);

/// Makes the zero frame. Before any program runs, so that it isn't a frame a program
/// seems to have taken and kept.
pub fn init() {
    let Some(frame) = memory::alloc_frames(1, 1) else {
        panic!("no frame for the zero page");
    };
    // SAFETY: just allocated, a page long, and the identity map covers it.
    unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE) };
    ZERO_FRAME.store(frame, Ordering::Release);
}

/// A new, empty address space, which file reverse maps know as `space`.
pub(super) fn new_mm(space: u32) -> Option<Mm> {
    let layout = Layout {
        window: (feos_abi::USER_BASE, feos_abi::USER_END),
        area: (feos_abi::USER_MMAP_BASE, feos_abi::USER_MMAP_END),
        page_limit: feos_abi::USER_MEMORY_LIMIT / PAGE_SIZE,
        max_vmas: feos_abi::MAX_VMAS,
        object_heap_limit: feos_abi::OBJECT_HEAP_LIMIT,
        space,
    };
    Mm::new(&mut KernelFrames, Port::user_page_bits(), layout, ZERO_FRAME.load(Ordering::Acquire))
}

/// What follows every change to a space: the walker sees it, a clamped release is
/// reported, and cache pages a promise was counted from are given back.
pub(super) fn after_change() {
    Port::sync_user_tables();
    report_underflows();
    // What was promised may have been counted from idle cache pages: they're given back
    // now, so the frames promised are free ones.
    pagecache::rebalance();
}

/// Releases of more promises than were held, already reported.
static UNDERFLOWS_SEEN: AtomicUsize = AtomicUsize::new(0);

/// Says so if a release has been clamped since last time: a bug, which a debug build
/// would have stopped on, and which leaves the machine promising less than it owes.
fn report_underflows() {
    let now = PROMISES.underflows();
    if now != UNDERFLOWS_SEEN.load(Ordering::Relaxed) && UNDERFLOWS_SEEN.swap(now, Ordering::Relaxed) != now {
        feos_console::println!("[user] {now} promise release(s) clamped: more given back than was held");
    }
}

/// Pages promised and not yet given frames, across the machine.
pub fn promised() -> usize {
    PROMISES.held()
}

/// Releases of promises that were clamped, having been more than was held: always zero
/// unless there's a bug.
pub fn promise_underflows() -> usize {
    PROMISES.underflows()
}

/// How a fault from a program went.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FaultOutcome {
    /// The page is there now: the program tries again.
    Retry,
    /// Nothing is mapped there for that: the program ends with status 139.
    Segv,
    /// It's mapped, but there's no frame for it: the program ends with status 135.
    Bus,
}

/// The last fault each thread took, as page and access, with how many times in a row,
/// so that a fault the tables say shouldn't happen can't loop forever. A call of the
/// thread's own clears it.
static LAST_FAULT: [AtomicUsize; feos_app::MAX_PROCESSES] = [const { AtomicUsize::new(0) }; feos_app::MAX_PROCESSES];
static REPEATS: [AtomicUsize; feos_app::MAX_PROCESSES] = [const { AtomicUsize::new(0) }; feos_app::MAX_PROCESSES];
/// The space's [`UserMm::revokes`] at each thread's last fault: another thread's unmap,
/// `mprotect` or discard between two faults makes the second a new one, not a repeat,
/// however often a thread whose pages are purged under it faults on the same page.
static LAST_REVOKES: [AtomicUsize; feos_app::MAX_PROCESSES] = [const { AtomicUsize::new(0) }; feos_app::MAX_PROCESSES];

/// Forgets the last fault: something may have changed since, or the slot has a new
/// thread, which mustn't inherit the last one's.
pub fn forget_fault(slot: usize) {
    LAST_FAULT[slot].store(0, Ordering::Relaxed);
    REPEATS[slot].store(0, Ordering::Relaxed);
}

/// Answers a memory fault from the program in `slot`: gives the page what its mapping
/// says, or says why not. Called from `sched::fault_entry`, the fault's kernel thread,
/// with interrupts on, so it can wait for a lock.
/// The code of a `SIGSEGV` at `address` in the process of the thread in `slot`: no access
/// to what's mapped there, or nothing mapped.
pub fn segv_code(slot: usize, address: u64) -> i32 {
    let mapped = super::process(slot).is_some_and(|process| process.mm().lock().mm.covers(address as usize, 1));
    if mapped {
        feos_abi::si::SEGV_ACCERR
    } else {
        feos_abi::si::SEGV_MAPERR
    }
}

pub fn resolve_fault(slot: usize, fault: MemoryFault) -> FaultOutcome {
    let Some(process) = super::process(slot) else {
        return FaultOutcome::Segv;
    };
    let mm = process.mm();
    apply_zaps(mm);
    let address = fault.address as usize;
    let page = address & !(PAGE_SIZE - 1);
    let key = page | usize::from(fault.write) | usize::from(fault.execute) << 1 | 1 << 2;
    // Read before the lock: a revoke counts itself before it takes anything away, so a
    // fault it causes always sees it moved.
    let revokes = mm.revokes();
    let last = LAST_FAULT[slot].swap(key, Ordering::Relaxed);
    let last_revokes = LAST_REVOKES[slot].swap(revokes, Ordering::Relaxed);
    let repeats = if last == key && last_revokes == revokes {
        REPEATS[slot].fetch_add(1, Ordering::Relaxed) + 1
    } else {
        REPEATS[slot].store(0, Ordering::Relaxed);
        0
    };
    // The same fault again with nothing between, from this thread or any other: whatever
    // the tables say, this CPU may have a stale entry. Once more after a flush is a fault
    // the tables can't fix.
    if repeats == 1 {
        tlb::flush_page(mm.tlb(), page);
    } else if repeats > 1 {
        return FaultOutcome::Segv;
    }
    let access = Access { write: fault.write, execute: fault.execute };
    let mut state = mm.lock();
    let resolved = mm.change(&mut state, |mm, ctx| mm.fault(address, access, ctx));
    if resolved.is_ok() {
        mm.note_fault(&state);
    }
    drop(state);
    match resolved {
        Ok(()) => FaultOutcome::Retry,
        Err(FaultError::Segv) => FaultOutcome::Segv,
        Err(FaultError::Bus) => FaultOutcome::Bus,
    }
}

/// Takes the pages of truncated files past their new ends out of this space, if a
/// truncation said there may be some: copies included, so they fault with `SIGBUS`.
pub(super) fn apply_zaps(user: &UserMm) {
    if !pagecache::take_zap(user.space()) {
        return;
    }
    let mut state = user.lock();
    let mut cut: alloc::vec::Vec<(FileKey, u64)> = alloc::vec::Vec::new();
    for vma in state.mm.vmas().iter() {
        if let Backing::File { file, .. } = &vma.backing {
            let key = file.key();
            if !cut.iter().any(|(seen, _)| *seen == key) && cut.try_reserve(1).is_ok() {
                cut.push((key, file.size().div_ceil(PAGE_SIZE as u64)));
            }
        }
    }
    for (key, first) in cut {
        user.revoke(&mut state, |mm, ctx| mm.zap_file(key, first, ctx));
    }
}

/// What an `MmError` is called across the boundary.
fn mm_errno(error: MmError) -> isize {
    -match error {
        MmError::Invalid => errno::EINVAL,
        MmError::NoMemory => errno::ENOMEM,
        MmError::Exists => errno::EEXIST,
        MmError::Denied => errno::EACCES,
        MmError::TextBusy => errno::ETXTBSY,
    }
}

/// A `prot` from `mmap2` or `mprotect`, if it's only bits there are.
fn protection(prot: usize) -> Option<Prot> {
    if prot & !(prot_bits::READ | prot_bits::WRITE | prot_bits::EXEC) != 0 {
        return None;
    }
    Some(Prot {
        // Nothing can be written or run that can't be read.
        read: prot != 0,
        write: prot & prot_bits::WRITE != 0,
        exec: prot & prot_bits::EXEC != 0,
    })
}

/// v1's `mmap`: private and anonymous, never executable, every page promised.
pub(super) fn sys_mmap(user: &UserMm, length: usize, prot: usize) -> isize {
    let prot = match prot {
        prot_bits::NONE => Prot::NONE,
        prot_bits::READ => Prot::READ,
        p if p == prot_bits::READ | prot_bits::WRITE || p == prot_bits::WRITE => Prot::READ_WRITE,
        _ => return -errno::EINVAL,
    };
    if length == 0 {
        return -errno::EINVAL;
    }
    let mut state = user.lock();
    if length > state.mm.page_limit() * PAGE_SIZE {
        return -errno::ENOMEM;
    }
    let Some(length) = length.checked_next_multiple_of(PAGE_SIZE) else {
        return -errno::ENOMEM;
    };
    let request = MapRequest { charge_all: true, ..MapRequest::anonymous(length, prot) };
    match user.change(&mut state, |mm, ctx| mm.map(&request, ctx)) {
        Ok(address) => address as isize,
        Err(error) => mm_errno(error),
    }
}

/// Whether a 32-bit argument is `-1`, from C or from Rust.
const fn is_minus_one(value: usize) -> bool {
    value == u32::MAX as usize || value == usize::MAX
}

/// What an fd given to `mmap2` is.
pub(super) enum MapFd {
    /// Not open.
    Closed,
    /// Open, but nothing a mapping can be made of: a pipe or the console.
    NotMappable,
    /// A file not open for reading.
    Unreadable,
    /// A file open for reading, by identity, and whether it's open for writing too.
    File { id: feos_fs::vfs::FileId, writable: bool },
}

/// `mmap2(hint, length, prot, flags, fd, offset)`, checked in the order the design
/// gives. `source` says what `fd` is, if it's a 32-bit argument at all.
pub(super) fn sys_mmap2(
    user: &UserMm,
    args: [usize; 6],
    source: impl FnOnce(usize) -> MapFd,
) -> isize {
    let [hint, length, prot, flags, fd, offset] = args;
    // The length: anything up to the size of the mmap area, as a reservation can be
    // bigger than what the process may have charged.
    if length == 0 {
        return -errno::EINVAL;
    }
    let (low, high) = user.lock().mm.area();
    let Some(length) = length.checked_next_multiple_of(PAGE_SIZE).filter(|&length| length <= high - low) else {
        return -errno::ENOMEM;
    };
    let Some(prot) = protection(prot) else {
        return -errno::EINVAL;
    };
    let anonymous = flags & map::ANONYMOUS != 0;
    if prot.write && prot.exec || prot.exec && anonymous {
        return -errno::EACCES;
    }
    if flags & !map::ALL != 0 {
        return -errno::EINVAL;
    }
    let shared = match flags & map::TYPE {
        map::SHARED | map::SHARED_VALIDATE => true,
        map::PRIVATE => false,
        _ => return -errno::EINVAL,
    };
    let (fixed, noreplace) = (flags & map::FIXED != 0, flags & map::FIXED_NOREPLACE != 0);
    if fixed && noreplace {
        return -errno::EINVAL;
    }
    let mut file = None;
    if anonymous {
        if !(fd == 0 || is_minus_one(fd)) || offset != 0 {
            return -errno::EINVAL;
        }
    } else {
        // An fd is its low 32 bits, and the rest must be nothing or their sign.
        let valid = fd >> 32 == 0 || fd >> 31 == usize::MAX >> 31;
        let (id, writable) = match if valid { source(fd & 0xffff_ffff) } else { MapFd::Closed } {
            MapFd::Closed => return -errno::EBADF,
            MapFd::NotMappable => return -errno::ENODEV,
            MapFd::Unreadable => return -errno::EACCES,
            MapFd::File { id, writable } => (id, writable),
        };
        if !offset.is_multiple_of(PAGE_SIZE) {
            return -errno::EINVAL;
        }
        if offset.checked_add(length).is_none() {
            return -errno::EOVERFLOW;
        }
        // Shared and writable writes the file: it has to be open for writing.
        if shared && prot.write && !writable {
            return -errno::EACCES;
        }
        let Some(tree) = super::tree() else {
            return -errno::ENODEV;
        };
        // What the node is decides, not what the path says: "//dev/null" or
        // "/tmp/../dev/zero" name devices too. By identity, so an unlinked file still maps.
        let stat = match tree.stat_file(id) {
            Ok(stat) => stat,
            Err(error) => return super::fs_errno(error),
        };
        match stat.device {
            // `/dev/zero` maps as anonymous memory (`file` stays `None`).
            Some(DeviceKind::Zero) => {
                if prot.exec {
                    return -errno::EACCES;
                }
            }
            // No other device is a file of pages.
            Some(_) => return -errno::ENODEV,
            None => {
                if stat.kind != FileKind::File {
                    return -errno::ENODEV;
                }
                match pagecache::file_for(id, stat.size) {
                    Ok(entry) => {
                        let first = (offset / PAGE_SIZE) as u64;
                        file = Some(feos_vm::FileMap { file: entry, first, may_write: writable });
                    }
                    Err(error) => return super::fs_errno(error),
                }
            }
        }
    }
    let place = if fixed {
        Place::Fixed(hint)
    } else if noreplace {
        Place::FixedNoReplace(hint)
    } else if hint != 0 {
        Place::Hint(hint)
    } else {
        Place::Anywhere
    };
    let request = MapRequest {
        length,
        prot,
        place,
        shared,
        noreserve: flags & map::NORESERVE != 0,
        populate: flags & map::POPULATE != 0,
        charge_all: false,
        image: false,
        file,
    };
    // What a `MAP_FIXED` replaces is unmapped, so what shared mappings there dirtied is
    // written back after, as `MUNMAP` would, and futex waiters on its words are woken.
    let mut state = user.lock();
    let end = hint.saturating_add(length);
    let (replaced, shared) = if fixed {
        match (file_ranges_in(&state.mm, hint, end, false), shared_words_in(&state.mm, hint, end)) {
            (Ok(files), Ok(shared)) => (files, shared),
            (Err(error), _) | (_, Err(error)) => return error,
        }
    } else {
        (Vec::new(), Vec::new())
    };
    // A fixed mapping replaces what was there.
    let mapped = if fixed {
        user.revoke(&mut state, |mm, ctx| mm.map(&request, ctx))
    } else {
        user.change(&mut state, |mm, ctx| mm.map(&request, ctx))
    };
    let result = match mapped {
        Ok(address) => address as isize,
        Err(error) => mm_errno(error),
    };
    drop(state);
    if fixed && result >= 0 {
        user.unmapped(hint, end, &shared);
    }
    write_back_all(replaced);
    result
}

/// The shared objects and files mapped in `[start, end)`, and which of their bytes, for
/// waking the futex waiters on them once it's unmapped. `ENOMEM` if there's no heap
/// for the list, before anything changes.
fn shared_words_in(mm: &Mm, start: usize, end: usize) -> Result<Vec<(usize, u64, u64)>, isize> {
    let mut words = Vec::new();
    let mut failed = false;
    mm.shared_words(start, end, &mut |object, low, high| {
        if words.try_reserve(1).is_ok() {
            words.push((object, low, high));
        } else {
            failed = true;
        }
    });
    if failed {
        Err(-errno::ENOMEM)
    } else {
        Ok(words)
    }
}

/// The pages of files mapped in `[start, end)`, as `(file, first page, pages)`: what to
/// write back once the range is unmapped, or synced. Shared mappings, and with `!sync`
/// private ones of a file with dirty pages too, whose entries may be what kept those
/// dirty at the last write-back. `ENOMEM` if there's no heap for the list, before
/// anything changes.
fn file_ranges_in(mm: &Mm, start: usize, end: usize, sync: bool) -> Result<Vec<(FileRef, u64, u64)>, isize> {
    let mut ranges: Vec<(FileRef, u64, u64)> = Vec::new();
    if start >= end {
        return Ok(ranges);
    }
    for vma in mm.vmas().in_range(start, end) {
        if let Backing::File { file, first, shared, .. } = &vma.backing {
            if !*shared && (sync || file.dirty_pages() == 0) {
                continue;
            }
            let low = vma.start.max(start);
            let count = ((vma.end.min(end) - low) / PAGE_SIZE) as u64;
            ranges.try_reserve(1).map_err(|_| -errno::ENOMEM)?;
            ranges.push((file.clone(), first + ((low - vma.start) / PAGE_SIZE) as u64, count));
        }
    }
    Ok(ranges)
}

/// Writes back the dirty pages of each range, once whatever mapped them here is gone:
/// only the unmapped pages, so a one-page `MUNMAP` costs a page's work. A page is clean
/// after unless some entry still has it and the file is mapped writable somewhere. A
/// failure leaves them dirty for the next write-back; nobody is told, as `MUNMAP` can't
/// say.
fn write_back_all(ranges: Vec<(FileRef, u64, u64)>) {
    for (file, first, count) in ranges {
        let _ = pagecache::write_back(&file, first, count);
    }
}

/// Unmaps every shared file mapping of a process that's ending, and every private one of
/// a file with dirty pages, and writes back the pages each had. The rest of its space goes
/// with it, all at once.
pub(super) fn write_back_at_exit(user: &UserMm, state: &mut MmState) {
    // One at a time, so nothing needs the heap: a space that goes always gets here.
    loop {
        let next = state.mm.vmas().iter().find_map(|vma| match &vma.backing {
            Backing::File { file, first, shared, .. } if *shared || file.dirty_pages() > 0 => {
                Some((vma.start, vma.end, file.clone(), *first))
            }
            _ => None,
        });
        let Some((start, end, file, first)) = next else {
            return;
        };
        let unmapped = user.revoke(state, |mm, ctx| mm.unmap(start, end - start, ctx));
        let _ = pagecache::write_back(&file, first, ((end - start) / PAGE_SIZE) as u64);
        if unmapped.is_err() {
            // Can't be: a whole VMA, cut out, needs no room. Not to loop on, either way.
            return;
        }
    }
}

/// A range for `munmap`, `mprotect`, `madvise` and `msync`: page aligned and in the
/// window, with its length rounded up to a page.
fn range(user: &UserMm, address: usize, length: usize) -> Option<usize> {
    if !address.is_multiple_of(PAGE_SIZE) {
        return None;
    }
    let length = length.checked_next_multiple_of(PAGE_SIZE)?;
    user.in_window(address, length).then_some(length)
}

pub(super) fn sys_munmap(user: &UserMm, address: usize, length: usize) -> isize {
    if length == 0 {
        return -errno::EINVAL;
    }
    let Some(length) = range(user, address, length) else {
        return -errno::EINVAL;
    };
    let mut state = user.lock();
    let end = address + length;
    let (files, shared) = match (file_ranges_in(&state.mm, address, end, false), shared_words_in(&state.mm, address, end)) {
        (Ok(files), Ok(shared)) => (files, shared),
        (Err(error), _) | (_, Err(error)) => return error,
    };
    let result = match user.revoke(&mut state, |mm, ctx| mm.unmap(address, length, ctx)) {
        Ok(()) => 0,
        Err(error) => mm_errno(error),
    };
    drop(state);
    if result == 0 {
        user.unmapped(address, end, &shared);
    }
    write_back_all(files);
    result
}

/// `THREAD_EXIT`'s memory work: the ending thread's clear word (`clear`, zero for none)
/// written with zero, then `[address, address + length)` unmapped if `length` isn't zero,
/// both under one hold of the space's lock, so a joiner woken by the word and quick to
/// map over the range can't have its mapping taken by the unmap; and only then, with the
/// lock let go, the word's waiters woken. `EINVAL`, with nothing done, for a range
/// `MUNMAP` would refuse; the thread carries on then.
pub(super) fn thread_exit(user: &UserMm, clear: usize, address: usize, length: usize) -> Result<(), isize> {
    let range_end = if length == 0 {
        None
    } else {
        let length = range(user, address, length).ok_or(-errno::EINVAL)?;
        Some(address + length)
    };
    let mut state = user.lock();
    let mut lists = None;
    if let Some(end) = range_end {
        if state.mm.touches_lent(address, end - address) {
            return Err(-errno::EINVAL);
        }
        lists = match (file_ranges_in(&state.mm, address, end, false), shared_words_in(&state.mm, address, end)) {
            (Ok(files), Ok(shared)) => Some((files, shared)),
            (Err(error), _) | (_, Err(error)) => return Err(error),
        };
    }
    let key = user.clear_word(&mut state, clear);
    // Past the word, the thread is going whatever happens: an unmap that can't be done
    // (no room to split a VMA) leaves the range mapped, the process's to deal with.
    let unmapped = range_end.is_some_and(|end| user.revoke(&mut state, |mm, ctx| mm.unmap(address, end - address, ctx)).is_ok());
    drop(state);
    if let (Some(end), Some((files, shared))) = (range_end, lists) {
        if unmapped {
            user.unmapped(address, end, &shared);
        }
        write_back_all(files);
    }
    if let Some(key) = key {
        crate::sched::futex_wake_user(key, usize::MAX);
    }
    Ok(())
}

pub(super) fn sys_mprotect(user: &UserMm, address: usize, length: usize, prot: usize) -> isize {
    let Some(prot) = protection(prot) else {
        return -errno::EINVAL;
    };
    if length == 0 {
        return -errno::EINVAL;
    }
    let Some(length) = range(user, address, length) else {
        return -errno::EINVAL;
    };
    if prot.write && prot.exec {
        return -errno::EACCES;
    }
    let mut state = user.lock();
    if prot.exec {
        // Image pages the loader didn't make executable were written through the
        // kernel's data side only, so on AArch64 their instructions may not be what the
        // program would fetch: synced before they can be run. Only the image may be made
        // executable, and it has all its frames; a refusal below makes this harmless.
        state.mm.each_frame(address, length, &mut |frame| Port::sync_user_code(frame, PAGE_SIZE));
    }
    match user.revoke(&mut state, |mm, ctx| mm.protect(address, length, prot, ctx)) {
        Ok(()) => 0,
        Err(error) => mm_errno(error),
    }
}

pub(super) fn sys_madvise(user: &UserMm, address: usize, length: usize, advice: usize) -> isize {
    let Some(length) = range(user, address, length) else {
        return -errno::EINVAL;
    };
    let discard = match advice {
        madv::DONTNEED | madv::FREE => true,
        madv::NORMAL
        | madv::RANDOM
        | madv::SEQUENTIAL
        | madv::WILLNEED
        | madv::DONTFORK
        | madv::DOFORK
        | madv::HUGEPAGE
        | madv::NOHUGEPAGE
        | madv::DONTDUMP
        | madv::DODUMP => false,
        _ => return -errno::EINVAL,
    };
    let mut state = user.lock();
    if !state.mm.covers(address, length) {
        return -errno::ENOMEM;
    }
    if advice == madv::WILLNEED {
        // The file pages in the range are read into the cache now, in the call, with
        // the space let go: the cache comes after it.
        let end = address + length;
        let mut wanted: alloc::vec::Vec<(FileRef, u64, u64)> = alloc::vec::Vec::new();
        for vma in state.mm.vmas().iter().filter(|vma| vma.start < end && vma.end > address) {
            if let Backing::File { file, first, .. } = &vma.backing {
                let low = vma.start.max(address);
                let count = ((vma.end.min(end) - low) / PAGE_SIZE) as u64;
                if wanted.try_reserve(1).is_ok() {
                    wanted.push((file.clone(), first + ((low - vma.start) / PAGE_SIZE) as u64, count));
                }
            }
        }
        drop(state);
        for (file, first, count) in wanted {
            pagecache::read_ahead(&file, first, count);
        }
        return 0;
    }
    if !discard {
        return 0;
    }
    // A discarded page's frame is no longer its word's, as after `MUNMAP`: a waiter that
    // pinned and checked the old one must not block on it, so the unmap count moves and
    // the range's waiters are woken, as `MUNMAP` does.
    let end = address + length;
    let shared = match shared_words_in(&state.mm, address, end) {
        Ok(shared) => shared,
        Err(error) => return error,
    };
    let result = match user.revoke(&mut state, |mm, ctx| mm.discard(address, length, ctx)) {
        Ok(()) => 0,
        Err(error) => mm_errno(error),
    };
    drop(state);
    if result == 0 {
        user.unmapped(address, end, &shared);
    }
    result
}

/// `msync`: with `MS_SYNC`, the dirty pages of the shared file mappings in the range are
/// written back and the filesystems flushed before it returns, `EIO` if that fails.
/// `MS_ASYNC` has no background write-back to start, and `MS_INVALIDATE` nothing to throw
/// away, since every mapping of a file shares the cache's frames.
pub(super) fn sys_msync(user: &UserMm, address: usize, length: usize, flags: usize) -> isize {
    if flags & !(ms::ASYNC | ms::SYNC | ms::INVALIDATE) != 0 || (flags & ms::ASYNC != 0) == (flags & ms::SYNC != 0) {
        return -errno::EINVAL;
    }
    let Some(length) = range(user, address, length) else {
        return -errno::EINVAL;
    };
    let state = user.lock();
    if !state.mm.covers(address, length) {
        return -errno::ENOMEM;
    }
    if flags & ms::SYNC == 0 {
        return 0;
    }
    let wanted = match file_ranges_in(&state.mm, address, address + length, true) {
        Ok(wanted) => wanted,
        Err(error) => return error,
    };
    drop(state);
    if wanted.is_empty() {
        return 0;
    }
    let mut failed = false;
    for (file, first, count) in wanted {
        failed |= pagecache::write_back(&file, first, count).is_err();
    }
    // Written back is in the filesystem; synced, it's on the disk.
    let synced = super::tree().is_some_and(|tree| tree.sync_if_dirty().is_ok());
    if failed || !synced {
        -errno::EIO
    } else {
        0
    }
}
