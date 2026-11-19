//! What a program is, beyond its task: a [`Process`], shared and counted, and its
//! address space, a [`UserMm`], shared and counted beneath it.
//!
//! A slot points at the process its task belongs to: every thread of a process has a
//! slot, and a reference, of its own. The process goes with its last reference, which
//! the scheduler hands to task context to drop (see `sched`'s teardown), and its space
//! with the last process, or anything else, holding that.
//!
//! The space is behind a sleeping mutex, so its tables and VMAs change under it and
//! nowhere else. Copies to and from it don't hold it while they wait: see
//! [`UserMm::copy_in`] and [`with_user_buffer`](super::with_user_buffer).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use feos_abi::{errno, MAX_CHILDREN, PROCESS_HEAP_LIMIT};
use feos_app::MAX_PROCESSES;
use feos_sync::{Mutex, MutexGuard, TryArc};
use feos_vm::{Ctx, FaultError, Mm, PinError, Pinned, WordKey, PAGE_SIZE};

use super::children::{ChildRecord, Usage};
use super::signal::Signals;
use super::memory_calls::{self, KernelFrames, PROMISES};
use super::tlb::{Gather, GatherFrames, Tlb};
use super::files::Files;
use super::NoKill;
use crate::pagecache::{self, CachePages};
use crate::{memory, surface, Port};

use feos_hal::Arch;

/// What a space holds that only its lock may touch.
pub struct MmState {
    /// The tables and VMAs. Taken out only when the space goes.
    pub mm: ManuallyDrop<Mm>,
    /// The window surface, as its index, address and length.
    pub surface: Option<(usize, usize, usize)>,
}

/// An address space, shared by every thread of the process using it.
pub struct UserMm {
    /// What names its private futex words: a number no other space has had, tagged so
    /// it can't be an object's address (see [`user_key`](Self::user_key)).
    id: usize,
    /// What to load: the same as the Mm's root, or a table above it. Fixed for life.
    root: usize,
    /// The user window, fixed for life.
    window: (usize, usize),
    tlb: Tlb,
    /// Bumped by every unmap, before the futex waiters in the range are woken, so a wait
    /// that checked its word before the unmap but blocks after it doesn't block.
    unmaps: AtomicU32,
    /// Bumped by every change that can take a page away from a thread (an unmap, a
    /// protection change, a discard, a truncation's zap), before the tables change: a
    /// fault that sees it unmoved since the same thread's last fault at the same place
    /// knows no other thread made it fault again.
    revokes: AtomicUsize,
    /// What file reverse maps know it as: a number no other live space has, or
    /// [`UNNUMBERED`] for a self-test's, which maps no files.
    space: u32,
    /// Faults its threads took that it was given a page for, for `GETRUSAGE`'s `minflt`.
    faults: AtomicU64,
    /// The most pages it has had frames for, as last seen, for `maxrss`.
    peak_pages: AtomicUsize,
    state: Mutex<MmState>,
}

/// Spaces made so far, for their ids.
static SPACES: AtomicUsize = AtomicUsize::new(0);

/// Which space numbers are taken. A space's number is its own, not its first thread's
/// slot: the slot can be reused by another process while the space lives on in the
/// others, and two live spaces sharing a number would share truncation notices.
static SPACE_NUMBERS: [AtomicBool; pagecache::SPACES] = [const { AtomicBool::new(false) }; pagecache::SPACES];

/// The space number of a space that maps no files.
const UNNUMBERED: u32 = u32::MAX;

/// A free space number, taken.
fn take_space_number() -> Option<u32> {
    SPACE_NUMBERS
        .iter()
        .position(|used| used.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok())
        .map(|number| number as u32)
}

impl UserMm {
    /// A new, empty space, its tables ready to load, with a number of its own for file
    /// reverse maps if `numbered`.
    pub fn new(numbered: bool) -> Option<Arc<Self>> {
        // The Arc's room is taken first, before anything is safe from a kill: a heap that's
        // run out panics there, and with no counted lock held that ends just the caller,
        // where under `NoKill` it would stop the machine. Nothing below allocates from the
        // kernel heap except `new_mm`, which fails softly.
        let mut cell = Arc::<Self>::new_uninit();
        let _no_kill = NoKill::new();
        let space = if numbered { take_space_number()? } else { UNNUMBERED };
        let give_back = |space: u32| {
            if let Some(used) = SPACE_NUMBERS.get(space as usize) {
                used.store(false, Ordering::Release);
            }
        };
        let Some(mm) = memory_calls::new_mm(space) else {
            give_back(space);
            return None;
        };
        let mut alloc = || memory::alloc_frames(1, 1);
        // SAFETY: a table just made for this, on a machine past boot.
        let Some(root) = (unsafe { Port::prepare_user_tables(mm.root(), &mut alloc) }) else {
            mm.destroy(&mut KernelFrames, &PROMISES);
            give_back(space);
            return None;
        };
        let window = mm.space().window();
        let id = (SPACES.fetch_add(1, Ordering::Relaxed) + 1) << 2 | 1;
        // Just made, so nothing else has it.
        Arc::get_mut(&mut cell)?.write(Self {
            id,
            root,
            window,
            tlb: Tlb::new(),
            unmaps: AtomicU32::new(0),
            revokes: AtomicUsize::new(0),
            space,
            faults: AtomicU64::new(0),
            peak_pages: AtomicUsize::new(0),
            state: Mutex::new(MmState { mm: ManuallyDrop::new(mm), surface: None }),
        });
        // SAFETY: written just above.
        Some(unsafe { cell.assume_init() })
    }

    /// What file reverse maps know it as.
    pub const fn space(&self) -> u32 {
        self.space
    }

    /// Counts a fault resolved, and the pages it has frames for now, under its lock as
    /// `state`.
    pub fn note_fault(&self, state: &MmState) {
        self.faults.fetch_add(1, Ordering::Relaxed);
        self.note_resident(state);
    }

    /// Notes the pages it has frames for now, under its lock as `state`, for its peak.
    pub fn note_resident(&self, state: &MmState) {
        let resident = (state.mm.charged() - state.mm.unbacked()) + state.mm.owned();
        self.peak_pages.fetch_max(resident, Ordering::Relaxed);
    }

    /// Faults resolved, and the most pages it has had frames for.
    pub fn usage(&self) -> (u64, usize) {
        self.note_resident(&self.lock());
        (self.faults.load(Ordering::Relaxed), self.peak_pages.load(Ordering::Relaxed))
    }

    /// The table to load.
    pub const fn root(&self) -> usize {
        self.root
    }

    /// Its TLB bookkeeping, for loading it.
    pub fn tlb(&self) -> &Tlb {
        &self.tlb
    }

    /// Takes the space's lock.
    pub fn lock(&self) -> MutexGuard<'_, MmState> {
        self.state.lock()
    }

    /// The user window, `[base, end)`.
    pub const fn window(&self) -> (usize, usize) {
        self.window
    }

    /// Whether `[address, address + length)` is in the user window, without the lock.
    pub fn in_window(&self, address: usize, length: usize) -> bool {
        let (base, end) = self.window;
        address >= base && length <= end - base && address <= end - length
    }

    /// Runs a change to the space, whose lock the caller holds as `state`, with what it
    /// needs from the machine: safe from a kill while a frame is between the allocator
    /// and a table, with the TLBs flushed and frames given back only once no CPU can
    /// reach them, and visible to the table walker after.
    pub fn change<R>(&self, state: &mut MmState, run: impl FnOnce(&mut Mm, &mut Ctx<'_>) -> R) -> R {
        let gather = Gather::new(&self.tlb);
        let result = {
            let _no_kill = NoKill::new();
            let mut frames = GatherFrames(&gather);
            let mut flush = |address: usize| gather.flush(address);
            let mut files = CachePages;
            let mut ctx = Ctx { frames: &mut frames, promises: &PROMISES, flush: &mut flush, files: &mut files };
            let result = run(&mut state.mm, &mut ctx);
            gather.finish();
            result
        };
        memory_calls::after_change();
        result
    }

    /// Runs a change that may take pages away, as [`change`](Self::change) does, noted
    /// first in the count that faults check (see [`revokes`](Self::revokes)).
    pub fn revoke<R>(&self, state: &mut MmState, run: impl FnOnce(&mut Mm, &mut Ctx<'_>) -> R) -> R {
        self.revokes.fetch_add(1, Ordering::AcqRel);
        self.change(state, run)
    }

    /// How many changes that may take pages away have begun: a fault again at the same
    /// place with this unmoved can't be any thread's doing.
    pub fn revokes(&self) -> usize {
        self.revokes.load(Ordering::Acquire)
    }

    /// Takes the lock and runs a change.
    pub fn with<R>(&self, run: impl FnOnce(&mut Mm, &mut Ctx<'_>) -> R) -> R {
        let mut state = self.lock();
        self.change(&mut state, run)
    }

    /// Readies every page of a user range the kernel is about to copy through, as a
    /// touch from the program would. `EFAULT` for what the program couldn't touch that
    /// way, `ENOMEM` for a page with no frame to give it.
    pub fn fault_in(&self, address: usize, length: usize, write: bool) -> Result<(), isize> {
        match self.with(|mm, ctx| mm.fault_in(address, length, write, ctx)) {
            Ok(()) => Ok(()),
            Err(FaultError::Segv) => Err(-feos_abi::errno::EFAULT),
            Err(FaultError::Bus) => Err(-feos_abi::errno::ENOMEM),
        }
    }

    /// Pins the page at `address` for a copy, readied as a touch would ready it, with
    /// the lock held just for that.
    fn pin(&self, address: usize, write: bool) -> Result<Pinned, isize> {
        let mut state = self.lock();
        // A page that's there already needs no change, and nothing to follow one.
        match state.mm.pin(address, write, &mut KernelFrames) {
            Ok(pinned) => Ok(pinned),
            Err(PinError::Fault) => self.change(&mut state, |mm, ctx| mm.translate_and_pin(address, write, ctx)).map_err(pin_errno),
            Err(error) => Err(pin_errno(error)),
        }
    }

    /// Copies from the program's memory, a page at a time, each pinned for its copy and
    /// copied with the lock released.
    pub fn copy_in(&self, address: usize, out: &mut [u8]) -> Result<(), isize> {
        if !self.in_window(address, out.len()) {
            return Err(-feos_abi::errno::EFAULT);
        }
        // All of it, first: a bad range fails before anything is copied.
        self.fault_in(address, out.len(), false)?;
        let mut done = 0;
        while done < out.len() {
            let here = address + done;
            let count = (PAGE_SIZE - here % PAGE_SIZE).min(out.len() - done);
            let pinned = self.pin(here, false)?;
            // SAFETY: pinned for reading, so a frame of this space's (or the zero frame)
            // that stays until it's unpinned, and `count` stops at its page's end. The
            // kernel's mapping of a frame is its address.
            unsafe { core::ptr::copy_nonoverlapping(pinned.address as *const u8, out[done..].as_mut_ptr(), count) };
            pinned.unpin(&mut KernelFrames);
            done += count;
        }
        Ok(())
    }

    /// Copies to the program's memory, which it must be able to write: as
    /// [`copy_in`](Self::copy_in). A page unmapped by another thread part way leaves
    /// what was copied before it.
    pub fn copy_out(&self, address: usize, data: &[u8]) -> Result<(), isize> {
        if !self.in_window(address, data.len()) {
            return Err(-feos_abi::errno::EFAULT);
        }
        self.fault_in(address, data.len(), true)?;
        let mut done = 0;
        while done < data.len() {
            let here = address + done;
            let count = (PAGE_SIZE - here % PAGE_SIZE).min(data.len() - done);
            let pinned = self.pin(here, true)?;
            // SAFETY: pinned for writing, so a frame the program may write (its own, or
            // a shared mapping's, dirty), which stays until it's unpinned.
            unsafe { core::ptr::copy_nonoverlapping(data[done..].as_ptr(), pinned.address as *mut u8, count) };
            pinned.unpin(&mut KernelFrames);
            done += count;
        }
        Ok(())
    }

    /// Copies one page's worth of the program's memory, at most to its page's end, into
    /// `out`, pinned only for the copy.
    pub fn read_page(&self, address: usize, out: &mut [u8]) -> Result<(), isize> {
        let pinned = self.pin(address, false)?;
        // SAFETY: as in `copy_in`; the caller keeps `out` within the page.
        unsafe { core::ptr::copy_nonoverlapping(pinned.address as *const u8, out.as_mut_ptr(), out.len()) };
        pinned.unpin(&mut KernelFrames);
        Ok(())
    }

    /// Copies `data` to the program's memory at `address`, within its page, pinned only
    /// for the copy.
    pub fn write_page(&self, address: usize, data: &[u8]) -> Result<(), isize> {
        let pinned = self.pin(address, true)?;
        // SAFETY: as in `copy_out`; the caller keeps `data` within the page.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), pinned.address as *mut u8, data.len()) };
        pinned.unpin(&mut KernelFrames);
        Ok(())
    }

    /// The futex word at `address`: its key, and its frame pinned, for the wait to
    /// check, and the unmap count to check alongside it. A word in writable memory is
    /// given its own frame first: one on the shared zero frame would change when the
    /// page was first written, and the waiter would be checking the wrong word.
    pub fn futex_word(&self, address: usize) -> Result<(feos_sched::UserKey, Pinned, u32), isize> {
        if !address.is_multiple_of(4) || !self.in_window(address, 4) {
            return Err(-feos_abi::errno::EFAULT);
        }
        let mut state = self.lock();
        let unmaps = self.unmaps.load(Ordering::Acquire);
        let pinned = self.change(&mut state, |mm, ctx| match mm.translate_and_pin(address, true, ctx) {
            Err(PinError::Fault) => mm.translate_and_pin(address, false, ctx),
            other => other,
        });
        let pinned = pinned.map_err(pin_errno)?;
        match state.mm.word_key(address) {
            Some(key) => Ok((self.user_key(key), pinned, unmaps)),
            None => {
                pinned.unpin(&mut KernelFrames);
                Err(-feos_abi::errno::EFAULT)
            }
        }
    }

    /// Whether `address` is on an executable page of the program's: somewhere a thread
    /// may start.
    pub fn is_executable(&self, address: usize) -> bool {
        let state = self.lock();
        let vmas = state.mm.vmas();
        vmas.find(address).and_then(|index| vmas.get(index)).is_some_and(|vma| vma.prot.exec)
    }

    /// Writes zero to the `u32` at `address`, a thread's clear word, if it's still mapped
    /// and writable, as part of a change the caller holds the lock for as `state`; and
    /// says what its futex key is, for the wake that comes once the lock is let go. A
    /// word that isn't there is skipped: the thread ends anyway.
    pub fn clear_word(&self, state: &mut MmState, address: usize) -> Option<feos_sched::UserKey> {
        if address == 0 || !address.is_multiple_of(4) || !self.in_window(address, 4) {
            return None;
        }
        let pinned = self.change(state, |mm, ctx| mm.translate_and_pin(address, true, ctx)).ok()?;
        // SAFETY: pinned for writing, aligned, and the kernel's mapping of a frame is its
        // address; the frame stays until it's unpinned just below.
        unsafe { (*(pinned.address as *const AtomicU32)).store(0, Ordering::Release) };
        pinned.unpin(&mut KernelFrames);
        state.mm.word_key(address).map(|key| self.user_key(key))
    }

    /// The key of the futex word at `address`, for a wake.
    pub fn futex_key(&self, address: usize) -> Result<feos_sched::UserKey, isize> {
        if !address.is_multiple_of(4) || !self.in_window(address, 4) {
            return Err(-feos_abi::errno::EFAULT);
        }
        // Mapped for reading, as a wait would need it: a wake of nothing is an `EFAULT`.
        self.fault_in(address, 4, false)?;
        let state = self.lock();
        state.mm.word_key(address).map(|key| self.user_key(key)).ok_or(-feos_abi::errno::EFAULT)
    }

    /// A word's key: private words by this space's id and their address, shared ones
    /// by their object's address and offset. Ids are odd and objects' addresses are
    /// aligned, so the two never meet.
    fn user_key(&self, key: WordKey) -> feos_sched::UserKey {
        match key {
            WordKey::Own(address) => feos_sched::UserKey { space: self.id, offset: address as u64 },
            WordKey::Shared { object, offset } => feos_sched::UserKey { space: object, offset },
        }
    }

    /// The count a futex wait checks hasn't moved.
    pub fn unmap_count(&self) -> &AtomicU32 {
        &self.unmaps
    }

    /// Notes that `[start, end)`, whose shared parts the caller collected in `shared`
    /// before it went, is no longer mapped, and wakes every futex waiter on a word in it.
    pub fn unmapped(&self, start: usize, end: usize, shared: &[(usize, u64, u64)]) {
        self.unmaps.fetch_add(1, Ordering::AcqRel);
        crate::sched::futex_wake_user_range(self.id, start as u64, end as u64);
        for &(object, low, high) in shared {
            crate::sched::futex_wake_user_range(object, low, high);
        }
    }
}

/// What a failed pin is called across the boundary.
fn pin_errno(error: PinError) -> isize {
    -match error {
        PinError::Fault => feos_abi::errno::EFAULT,
        PinError::Busy => feos_abi::errno::EAGAIN,
        PinError::NoFrame => feos_abi::errno::ENOMEM,
    }
}

impl Drop for UserMm {
    /// Gives everything back. No CPU has these tables loaded any more. What its shared
    /// mappings dirtied is written back first, which is why a space only goes in task
    /// context.
    fn drop(&mut self) {
        let mut state = self.state.lock();
        memory_calls::write_back_at_exit(self, &mut state);
        let surface = state.surface.take();
        // SAFETY: taken once, here, as the space goes; nothing reaches it after.
        let mm = unsafe { ManuallyDrop::take(&mut state.mm) };
        drop(state);
        let tables = mm.root();
        mm.destroy(&mut KernelFrames, &PROMISES);
        if self.root != tables && self.root != 0 {
            memory::free_frames(self.root, 1);
        }
        // Its ASID's entries go before the ASID can be anyone else's.
        self.tlb.release();
        // The space lent the surface's frames, and has let them go.
        if let Some((index, _, _)) = surface {
            surface::release(index);
        }
        // Nothing maps a file for it now, so its number can be another's.
        if let Some(used) = SPACE_NUMBERS.get(self.space as usize) {
            used.store(false, Ordering::Release);
        }
        // Its mappings may have been the last holds on files unlinked while in use.
        super::fs_calls::sweep_orphans();
    }
}

/// A program: its space and its files, and its pid, which is its thread group's.
pub struct Process {
    pid: u32,
    /// The space, which only `EXEC` changes: see [`replace_image`](Self::replace_image).
    mm: UnsafeCell<Arc<UserMm>>,
    files: Mutex<Files>,
    /// Held to start a thread, and by `EXEC` from before it ends the others until it's
    /// alone, so the count of threads is only checked and changed by one at a time and
    /// neither ever sees half a thread. `FORK` will take it too. Taken through
    /// [`creating`](Self::creating), which a kill ends.
    creation: Mutex<()>,
    /// Moved on whenever the creation lock is let go, for those waiting for it.
    creation_turns: AtomicU32,
    /// Scheduler ticks of CPU time its threads used that have ended, for
    /// `CLOCK_PROCESS_CPUTIME_ID`. Added to and read under the scheduler's lock, with the
    /// live threads' own counts, so the sum never counts a thread twice or goes back.
    ended_ticks: AtomicU64,
    /// What of the kernel heap its calls hold for it, against [`PROCESS_HEAP_LIMIT`].
    heap: TryArc<HeapBudget>,
    /// The canonical path of the program it runs, for `CURRENT_EXE`. Changed only by
    /// `EXEC`, as `mm` is.
    exe: UnsafeCell<String>,
    /// The exit records of the children it has started and not reaped, running or
    /// ended, each taken before its child is made: at most [`MAX_CHILDREN`]. One whose
    /// pid is 0 is being made. See [`super::children`].
    children: Mutex<Vec<TryArc<ChildRecord>>>,
    /// Its own record in its parent's list, if a program started it: what it used is
    /// added there as it goes.
    record: Option<TryArc<ChildRecord>>,
    /// What its reaped children used, and theirs, for `RUSAGE_CHILDREN`: ticks, the most
    /// pages any had, and faults.
    reaped_ticks: AtomicU64,
    reaped_peak: AtomicU64,
    reaped_faults: AtomicU64,
    /// Its dispositions and pending signals, which every slot of it points at for the
    /// scheduler (see [`super::signal`]).
    signals: Signals,
}

// SAFETY: everything in it is `Sync` but the space and the program path, which only the
// process's own threads read, and which `replace_image` changes only when there's one
// thread and it holds no reference to either (see there).
unsafe impl Sync for Process {}
// SAFETY: as above; nothing in it belongs to one CPU or task.
unsafe impl Send for Process {}

/// The creation lock, held. Let go, it wakes whoever's waiting.
pub struct Creating<'a> {
    guard: Option<MutexGuard<'a, ()>>,
    turns: &'a AtomicU32,
}

impl Drop for Creating<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        self.turns.fetch_add(1, Ordering::Release);
        feos_sync::futex_wake(self.turns, usize::MAX);
    }
}

impl Process {
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Its signal state.
    pub fn signals(&self) -> &Signals {
        &self.signals
    }

    /// Takes the creation lock, or gives up with `None` if the calling thread is being
    /// killed: an `EXEC` holds the lock while it waits for every other thread to end,
    /// one waiting here among them, which a plain lock would keep waiting for ever.
    pub fn creating(&self) -> Option<Creating<'_>> {
        loop {
            let seen = self.creation_turns.load(Ordering::Acquire);
            if let Some(guard) = self.creation.try_lock() {
                return Some(Creating { guard: Some(guard), turns: &self.creation_turns });
            }
            if crate::sched::kill_requested() {
                return None;
            }
            // A thread being killed never blocks, so its next look comes soon enough.
            feos_sync::futex_wait(&self.creation_turns, seen);
        }
    }

    /// Puts a new image's space and program path in place of the old, for `EXEC`, and
    /// hands back the old ones, to drop once the thread's tables are the new space's.
    ///
    /// # Safety
    ///
    /// The caller is the process's only thread and holds its creation lock (so no other
    /// can start), and nothing on its stack still uses a reference that [`mm`](Self::mm)
    /// or [`exe`](Self::exe) gave out: nobody else reads either.
    pub unsafe fn replace_image(&self, mm: Arc<UserMm>, exe: String) -> (Arc<UserMm>, String) {
        // SAFETY: see above: nothing else is looking.
        unsafe { (core::mem::replace(&mut *self.mm.get(), mm), core::mem::replace(&mut *self.exe.get(), exe)) }
    }

    /// CPU time, in scheduler ticks, of the threads that have ended. Under the
    /// scheduler's lock.
    pub fn ended_ticks(&self) -> u64 {
        self.ended_ticks.load(Ordering::Relaxed)
    }

    /// Folds an ending thread's CPU time in. Under the scheduler's lock, as the thread
    /// stops being counted as live.
    pub fn fold_ticks(&self, ticks: u64) {
        self.ended_ticks.fetch_add(ticks, Ordering::Relaxed);
    }

    pub fn mm(&self) -> &UserMm {
        // SAFETY: see `replace_image`, the only change.
        unsafe { &*self.mm.get() }
    }

    /// The canonical path of the program it runs.
    pub fn exe(&self) -> &str {
        // SAFETY: as for `mm`.
        unsafe { &*self.exe.get() }
    }

    /// Takes the file table's lock. Nothing is closed with it held: what comes out of
    /// the table is dropped once the guard has gone.
    pub fn files(&self) -> MutexGuard<'_, Files> {
        self.files.lock()
    }

    /// Takes a record for a child about to be made, in the list already: `EAGAIN` with
    /// [`MAX_CHILDREN`] unreaped already. The scheduler fills its pid in as the child's
    /// task is made, or [`child_failed`](Self::child_failed) takes it out again.
    pub fn reserve_child(&self) -> Result<TryArc<ChildRecord>, isize> {
        let mut gone = Vec::new();
        let mut children = self.children.lock();
        // At the cap, the children nobody waits for that have ended and settled make room:
        // a parent ignoring `SIGCHLD` may never look at its list.
        if children.len() >= MAX_CHILDREN && gone.try_reserve(children.len()).is_ok() {
            let mut index = 0;
            while index < children.len() {
                let record = &children[index];
                if record.detached() && record.end().is_some() && crate::sched::settled(record.pid()) {
                    gone.push(children.swap_remove(index));
                } else {
                    index += 1;
                }
            }
        }
        if children.len() >= MAX_CHILDREN {
            drop(children);
            drop(gone);
            return Err(-errno::EAGAIN);
        }
        children.try_reserve(1).map_err(|_| -errno::ENOMEM)?;
        let record = ChildRecord::new().ok_or(-errno::ENOMEM)?;
        children.push(record.clone());
        Ok(record)
    }

    /// The child `record` was taken for wasn't made after all.
    pub fn child_failed(&self, record: &TryArc<ChildRecord>) {
        let gone = {
            let mut children = self.children.lock();
            children.iter().position(|other| TryArc::ptr_eq(other, record)).map(|at| children.swap_remove(at))
        };
        drop(gone);
    }

    /// The records of its unreaped children. Nothing is dropped with the guard held: what
    /// comes out of the list goes once it's let go.
    pub fn children(&self) -> MutexGuard<'_, Vec<TryArc<ChildRecord>>> {
        self.children.lock()
    }

    /// Adds what a child it has just reaped used, with that child's own, to what
    /// `RUSAGE_CHILDREN` reports.
    pub fn add_reaped(&self, usage: Usage) {
        self.reaped_ticks.fetch_add(usage.ticks, Ordering::AcqRel);
        self.reaped_peak.fetch_max(usage.peak, Ordering::AcqRel);
        self.reaped_faults.fetch_add(usage.faults, Ordering::AcqRel);
    }

    /// What its reaped children used, and theirs.
    pub fn reaped(&self) -> Usage {
        Usage {
            ticks: self.reaped_ticks.load(Ordering::Acquire),
            peak: self.reaped_peak.load(Ordering::Acquire),
            faults: self.reaped_faults.load(Ordering::Acquire),
        }
    }

    /// Charges `bytes` of kernel heap a call is about to take for the process, and keeps
    /// them charged until the [`HeapCharge`] goes: `ENOMEM` past the process's budget or,
    /// with every process's charges together, past [`USER_HEAP_SHARE`] of the heap.
    pub fn charge_heap(&self, bytes: usize) -> Result<HeapCharge, isize> {
        HeapBudget::charge(&self.heap, bytes)
    }

    /// Counts `pages` frames a call takes for the process outside its space against its
    /// page limit, as its own pages are, until the [`PageCharge`] goes: `ENOMEM` past it.
    pub fn charge_pages(&self, pages: usize) -> Result<PageCharge, isize> {
        // SAFETY: as for `mm`.
        let mm = unsafe { &*self.mm.get() };
        if !mm.lock().mm.hold(pages) {
            return Err(-errno::ENOMEM);
        }
        Ok(PageCharge { mm: mm.clone(), pages })
    }
}

/// Frames held for a process outside its space, counted against its limit until this
/// goes. It keeps the space, so it's let go in the call that made it, never left to
/// outlive the process.
pub struct PageCharge {
    mm: Arc<UserMm>,
    pages: usize,
}

impl Drop for PageCharge {
    fn drop(&mut self) {
        self.mm.lock().mm.unhold(self.pages);
    }
}

/// A quarter of the kernel heap is the most every process's calls can hold between them:
/// the rest is the kernel's own (its apps, drivers, filesystems and network), which
/// programs, however many, can't take from.
pub const USER_HEAP_SHARE: usize = 4;

/// What every process's [`HeapCharge`]s add up to.
static USER_HEAP: AtomicUsize = AtomicUsize::new(0);

/// The kernel heap one process's calls hold for it (section 2.1 of the ABI design).
/// Shared and counted, so a charge can outlive the process it was made for, as an open
/// file description can.
pub struct HeapBudget {
    charged: AtomicUsize,
}

impl HeapBudget {
    fn charge(budget: &TryArc<Self>, bytes: usize) -> Result<HeapCharge, isize> {
        let fits = |limit: usize| move |used: usize| used.checked_add(bytes).filter(|&total| total <= limit);
        budget.charged.try_update(Ordering::AcqRel, Ordering::Acquire, fits(PROCESS_HEAP_LIMIT)).map_err(|_| -errno::ENOMEM)?;
        let machine = crate::allocator::stats().total_bytes / USER_HEAP_SHARE;
        if USER_HEAP.try_update(Ordering::AcqRel, Ordering::Acquire, fits(machine)).is_err() {
            budget.charged.fetch_sub(bytes, Ordering::AcqRel);
            return Err(-errno::ENOMEM);
        }
        Ok(HeapCharge { budget: budget.clone(), bytes })
    }
}

/// Kernel heap charged to a process, given back when this goes.
pub struct HeapCharge {
    budget: TryArc<HeapBudget>,
    bytes: usize,
}

impl Drop for HeapCharge {
    fn drop(&mut self) {
        self.budget.charged.fetch_sub(self.bytes, Ordering::AcqRel);
        USER_HEAP.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

struct ProcessCell(UnsafeCell<Option<Arc<Process>>>);

// SAFETY: a cell belongs to the task in its slot, which sets it as it starts. The
// scheduler takes it only once that task has gone, under its lock, before the slot can
// be reused.
unsafe impl Sync for ProcessCell {}

static PROCESSES: [ProcessCell; MAX_PROCESSES] = [const { ProcessCell(UnsafeCell::new(None)) }; MAX_PROCESSES];

/// Makes the process for the program `exe` starting in `slot`, with pid `pid`, and gives
/// it the slot: with `files` for its descriptors and working directory if its spawn made
/// them, or else the standard streams, relative paths starting from `cwd`, and `umask`.
/// `None` if there's no memory for it.
pub fn create(slot: usize, pid: u32, cwd: &str, umask: u32, exe: &str, files: Option<Files>) -> Option<&'static Process> {
    let files = match files {
        Some(files) => files,
        None => Files::new(cwd, umask)?,
    };
    let mm = UserMm::new(true)?;
    let heap = TryArc::new(HeapBudget { charged: AtomicUsize::new(0) }).ok()?;
    let mut path = String::new();
    path.try_reserve_exact(exe.len()).ok()?;
    path.push_str(exe);
    let process = Arc::new(Process {
        pid,
        mm: UnsafeCell::new(mm),
        files: Mutex::new(files),
        creation: Mutex::new(()),
        creation_turns: AtomicU32::new(0),
        ended_ticks: AtomicU64::new(0),
        heap,
        exe: UnsafeCell::new(path),
        children: Mutex::new(Vec::new()),
        record: crate::sched::own_record(slot),
        reaped_ticks: AtomicU64::new(0),
        reaped_peak: AtomicU64::new(0),
        reaped_faults: AtomicU64::new(0),
        signals: Signals::new(crate::sched::inherited_ignored(slot)),
    });
    let signals = process.signals() as *const Signals;
    // SAFETY: see `ProcessCell`: the slot's own task, before anything else can look.
    unsafe { *PROCESSES[slot].0.get() = Some(process) };
    // The scheduler may look from here: the slot holds the process until it's taken.
    crate::sched::publish_signals(slot, signals);
    current(slot)
}

impl Drop for Process {
    /// The pipes of its standard streams are held in its pid's name, by every thread of
    /// it: they close with the last reference, here, in task context, as its record locks
    /// go. Its descriptors'
    /// open file descriptions go with the file table just after, the same way.
    fn drop(&mut self) {
        // What it used goes into its parent's record of it, before the parent can reap
        // it: the record isn't reaped while any of its threads' teardowns is still under
        // way, and this is one. CPU time the scheduler adds itself, thread by thread.
        if let Some(record) = &self.record {
            let (faults, peak) = self.mm().usage();
            let reaped = self.reaped();
            record.add_usage(Usage { ticks: reaped.ticks, peak: reaped.peak.max(peak as u64), faults: reaped.faults + faults });
        }
        feos_app::pipe::release_pid(self.pid);
        // Its record locks, every one: its descriptors' descriptions may outlive it, and
        // their `flock` locks with them, but these are the process's.
        super::lock_calls::process_ended(self.pid);
    }
}

/// Gives the slot of a new thread its reference to `process`. The slot is reserved, so
/// its task hasn't started.
pub fn adopt(slot: usize, process: Arc<Process>) {
    let signals = process.signals() as *const Signals;
    // SAFETY: see `ProcessCell`: a reserved slot, before anything can look.
    unsafe { *PROCESSES[slot].0.get() = Some(process) };
    crate::sched::publish_signals(slot, signals);
}

/// Another reference to the calling task's process, which is in `slot`, for a thread
/// it starts.
pub fn share(slot: usize) -> Option<Arc<Process>> {
    // SAFETY: see `current`.
    unsafe { (*PROCESSES[slot].0.get()).clone() }
}

/// Takes the process of a task that has gone, or is going and has left its tables. The
/// caller drops it only once the slot's pointer at its signals is gone, which is cleared
/// under the scheduler's lock (`sched::publish_signals`), so nothing under that lock is
/// left looking at a process that's being dropped.
pub fn take(slot: usize) -> Option<Arc<Process>> {
    // SAFETY: see `ProcessCell`.
    unsafe { (*PROCESSES[slot].0.get()).take() }
}

/// The process of the calling task, which is in `slot`.
pub fn current(slot: usize) -> Option<&'static Process> {
    // SAFETY: see `ProcessCell`: the slot's task is the caller, so the cell won't be
    // taken while it's running. It mustn't be used past an exit, which takes it.
    unsafe { (*PROCESSES[slot].0.get()).as_deref() }
}

/// An address space for a self-test, never run: ktest drives the TLB and ASID paths
/// with them.
pub struct TestSpace(Arc<UserMm>);

impl TestSpace {
    pub fn new() -> Option<Self> {
        UserMm::new(false).map(Self)
    }

    pub fn mm(&self) -> &UserMm {
        &self.0
    }

    /// Maps a page at `address` and puts `value` in its first word. Returns its frame.
    pub fn map_word(&self, address: usize, value: u64) -> Option<usize> {
        let request = feos_vm::MapRequest {
            place: feos_vm::Place::Exact(address),
            populate: true,
            ..feos_vm::MapRequest::anonymous(PAGE_SIZE, feos_vm::Prot::READ_WRITE)
        };
        self.0.with(|mm, ctx| mm.map(&request, ctx)).ok()?;
        self.0.copy_out(address, &value.to_ne_bytes()).ok()?;
        let state = self.0.lock();
        state.mm.space().translate(address, false).ok()
    }

    /// Unmaps the page at `address`.
    pub fn unmap(&self, address: usize) -> bool {
        let done = self.0.revoke(&mut self.0.lock(), |mm, ctx| mm.unmap(address, PAGE_SIZE, ctx)).is_ok();
        self.0.unmapped(address, address + PAGE_SIZE, &[]);
        done
    }

    /// `MADV_DONTNEED` of the page at `address`, as a program would ask for it.
    pub fn discard(&self, address: usize) -> bool {
        memory_calls::sys_madvise(&self.0, address, PAGE_SIZE, feos_abi::madv::DONTNEED) == 0
    }

    /// A thread's end, as `THREAD_EXIT` makes it, with its clear word at `address` inside
    /// the page it unmaps there. The word's frame is pinned across it, so what the end
    /// left in the word can be read after the page has gone: the word, and whether the
    /// page went.
    pub fn exit_clearing(&self, address: usize) -> Option<(u32, bool)> {
        let pinned = self.0.pin(address, false).ok()?;
        let ended = memory_calls::thread_exit(&self.0, address, address & !(PAGE_SIZE - 1), PAGE_SIZE).is_ok();
        // SAFETY: pinned, so the frame stays whatever happened to the mapping, and the
        // kernel's address for it is its own; aligned, as the caller's word is.
        let word = unsafe { (*(pinned.address as *const AtomicU32)).load(Ordering::Acquire) };
        pinned.unpin(&mut KernelFrames);
        let gone = self.0.lock().mm.space().translate(address, false).is_err();
        ended.then_some((word, gone))
    }

    /// A futex wait on the word at `address`, as a program's: its frame pinned, and the
    /// unmap count checked with it. The call's result.
    pub fn wait_word(&self, address: usize, expected: u32) -> isize {
        super::sys_futex_wait(&self.0, address, expected as usize, 0)
    }
}
