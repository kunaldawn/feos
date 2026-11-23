//! Which CPUs may hold an address space's TLB entries, and getting them to forget.
//!
//! Every space has a [`Tlb`]: the CPUs that have its tables loaded, and its ASID where
//! the port has them. Loading tables goes through [`load`], which keeps both.
//!
//! AArch64 tags entries with the space's ASID and its invalidations reach every CPU,
//! so a flush is one instruction wherever the space is loaded, and changing tables
//! forgets nothing. A space keeps its ASID until it's gone (see `feos_vm::asid` for the
//! generations that recycle them), when it's flushed and given back.
//!
//! x86_64 has no PCIDs here, so loading other tables forgets everything, and `invlpg`
//! only reaches the CPU it runs on. A page another CPU may have cached is a TLB
//! shootdown: that CPU is interrupted ([`Board::IPI_TLB`]), flushes, and says so, and
//! the frame the entry pointed at isn't given back until every CPU asked has. The
//! order that makes it sound: a CPU publishes that it's loading the space (`SeqCst`)
//! before it loads the tables, and stops saying so only after it has loaded others; the
//! CPU changing the space reads who has it after it has cleared the entry. So a CPU
//! either walks the tables after the entry is gone, or is asked. Loading the same
//! tables again, another thread of the same space, reloads nothing.
//!
//! The initiator waits with interrupts on and holding no spin lock: the CPU it waits
//! for may be waiting for a lock the initiator would otherwise be keeping from it, and
//! two CPUs each shooting down the other's entries both have to take the interrupt.

use core::cell::UnsafeCell;
use core::sync::atomic::{fence, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use feos_hal::{Arch, Board, Clock, IpiTarget};
use feos_sync::IrqSpinLock;
use feos_vm::asid::Asids;
use feos_vm::FrameSource;

use crate::{Machine, Port};

const MAX_CPUS: usize = Port::MAX_CPUS;

/// One space's TLB bookkeeping.
pub struct Tlb {
    /// The CPUs that have the space's tables loaded, a bit each.
    cpus: AtomicU32,
    /// Its generation and ASID, as `feos_vm::asid` keeps them, changed under [`ASIDS`].
    context: AtomicU64,
}

impl Tlb {
    pub const fn new() -> Self {
        Self { cpus: AtomicU32::new(0), context: AtomicU64::new(0) }
    }

    /// The CPUs that have the space loaded now.
    pub fn cpus(&self) -> u32 {
        self.cpus.load(Ordering::SeqCst)
    }

    /// Its generation and ASID as last given, `generation << 16 | asid`, zero before it's
    /// first loaded or on a port without ASIDs: for `ktest asid-rollover`.
    pub fn context(&self) -> u64 {
        let _asids = ASIDS.lock();
        self.context.load(Ordering::Relaxed)
    }

    /// The space is going: no CPU has it loaded. Its ASID is flushed everywhere and
    /// given back.
    pub fn release(&self) {
        debug_assert_eq!(self.cpus(), 0, "a space dropped while loaded");
        if ASID_BITS.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut asids = ASIDS.lock();
        if let Some(asid) = asids.release(self.context.load(Ordering::Relaxed)) {
            Port::flush_user_space(asid);
        }
    }
}

/// Every space's ASID, behind one lock: taken with interrupts masked, inside the
/// scheduler's lock when a CPU switches, so nothing is taken inside it.
static ASIDS: IrqSpinLock<Asids<MAX_CPUS>> = IrqSpinLock::new(Asids::new(0));

/// How wide the ASIDs are, for the paths that don't need the lock to know there are none.
static ASID_BITS: AtomicU32 = AtomicU32::new(0);

/// The space each CPU has loaded, or null for the kernel's tables.
static LOADED: [AtomicPtr<Tlb>; MAX_CPUS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_CPUS];

/// Shootdowns asked of each CPU, and the last one it has done: it's done when its
/// count reaches the ticket the asker took.
static ASKED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static DONE: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Shootdowns sent, and frames held back until one was acknowledged, for `ktest tlb`.
static SHOOTDOWNS: AtomicUsize = AtomicUsize::new(0);
static HELD_BACK: AtomicUsize = AtomicUsize::new(0);

/// Takes the port's ASID width. Before any program runs.
pub fn init() {
    let bits = Port::user_asid_bits();
    ASID_BITS.store(bits, Ordering::Relaxed);
    if bits != 0 {
        let mut asids = ASIDS.lock();
        asids.set_bits(bits);
        Port::flush_user_all();
    }
}

/// Shootdowns sent so far, and frames that waited for one.
pub fn shootdown_counts() -> (usize, usize) {
    (SHOOTDOWNS.load(Ordering::Relaxed), HELD_BACK.load(Ordering::Relaxed))
}

/// ASID generations so far (rollovers plus one), and the width in use.
pub fn asid_state() -> (u64, u32) {
    let asids = ASIDS.lock();
    (asids.generation(), asids.bits())
}

/// Gives out `bits`-bit ASIDs from now on, flushing everything, for `ktest
/// asid-rollover` to drive the 8-bit path on a CPU with 16. Nothing on a port
/// without ASIDs.
pub fn force_asid_bits(bits: u32) {
    if ASID_BITS.load(Ordering::Relaxed) == 0 {
        return;
    }
    // Flushed under the lock: once it's let go, a `load` may be given an ASID some other
    // space had in the old generation, and must not find that space's entries.
    let mut asids = ASIDS.lock();
    asids.set_bits(bits);
    Port::flush_user_all();
}

/// Loads `root` on this CPU, `cpu`, the tables of the space `tlb` keeps, or the
/// kernel's with `root` zero. With interrupts masked.
pub fn load(cpu: usize, root: usize, tlb: *const Tlb) {
    let old = LOADED[cpu].load(Ordering::Relaxed).cast_const();
    let bit = 1u32 << cpu;
    if root == 0 || tlb.is_null() {
        Port::activate_user_tables(0, 0);
        if !old.is_null() {
            if ASID_BITS.load(Ordering::Relaxed) != 0 {
                ASIDS.lock().deactivate(cpu);
            }
            // SAFETY: a space this CPU has loaded outlives its being loaded: it's only
            // dropped once every CPU has loaded something else.
            unsafe { &*old }.cpus.fetch_and(!bit, Ordering::SeqCst);
            LOADED[cpu].store(core::ptr::null_mut(), Ordering::Relaxed);
        }
        return;
    }
    // SAFETY: the caller's space is alive: a slot, or a self-test, holds it.
    let space = unsafe { &*tlb };
    if old != tlb {
        // Published before the tables are loaded: see the module's comment.
        space.cpus.fetch_or(bit, Ordering::SeqCst);
    }
    if ASID_BITS.load(Ordering::Relaxed) == 0 {
        Port::activate_user_tables(root, 0);
    } else {
        let mut asids = ASIDS.lock();
        let mut context = space.context.load(Ordering::Relaxed);
        let (asid, flush) = asids.activate(&mut context, cpu);
        space.context.store(context, Ordering::Relaxed);
        if flush {
            Port::flush_user_all();
        }
        // Loaded under the lock. Once `activate` has made this CPU's active context the
        // new one, a rollover elsewhere reserves only that, frees the outgoing space's
        // ASID and flushes once; were the outgoing tables still loaded after it, a walk
        // could refill entries tagged with that ASID which nothing would flush again
        // before it went to some other space.
        Port::activate_user_tables(root, asid);
    }
    if !old.is_null() && old != tlb {
        // SAFETY: as above.
        unsafe { &*old }.cpus.fetch_and(!bit, Ordering::SeqCst);
    }
    LOADED[cpu].store(tlb.cast_mut(), Ordering::Relaxed);
}

/// Forgets the page at `address` of the space `tlb` keeps, on this CPU and wherever a
/// flush reaches by itself, whose entry has just been cleared or changed. Returns the
/// other CPUs that still have to be asked: none where flushes are broadcast.
fn flush_here(tlb: &Tlb, address: usize) -> u32 {
    if Port::USER_FLUSH_BROADCAST {
        if ASID_BITS.load(Ordering::Relaxed) != 0 {
            // Under the lock, so the ASID can't change between reading and flushing it:
            // a CPU that loads the space with a new one does so after, and walks the
            // tables as they are now.
            let asids = ASIDS.lock();
            if let Some(asid) = asids.current(tlb.context.load(Ordering::Relaxed)) {
                Port::flush_user_page(address, asid);
            }
        } else {
            Port::flush_user_page(address, 0);
        }
        return 0;
    }
    let flags = Port::irq_save_and_disable();
    let cpu = Port::cpu_index();
    if core::ptr::eq(LOADED[cpu].load(Ordering::Relaxed), tlb) {
        Port::flush_user_page(address, 0);
    }
    // Who has it is read after the entry was cleared.
    fence(Ordering::SeqCst);
    let others = tlb.cpus.load(Ordering::SeqCst) & !(1 << cpu);
    Port::irq_restore(flags);
    others
}

/// Forgets one page of a space everywhere, now: for a stale entry, outside a change.
pub fn flush_page(tlb: &Tlb, address: usize) {
    let others = flush_here(tlb, address);
    if others != 0 {
        shootdown(others);
    }
}

/// Asks every other CPU that has joined the scheduler to flush, and waits until each
/// has: for kernel stack pages just unmapped, which any CPU may have cached. What a
/// shootdown's interrupt does there, reloading its tables, forgets those as well, since
/// they aren't global. In task context with interrupts on, holding no spin lock.
pub fn shootdown_all() {
    // This CPU too, whichever it is now: the unmapping may have run on another before a
    // migration, and only flushed there.
    let flags = Port::irq_save_and_disable();
    let me = Port::cpu_index();
    Port::flush_user_local();
    Port::irq_restore(flags);
    let online = crate::sched::online_cpus().min(MAX_CPUS);
    let others = ((1u64 << online) - 1) as u32 & !(1 << me);
    if others != 0 {
        shootdown(others);
    }
}

/// Asks each CPU in `cpus` to flush its user entries, and waits until every one has.
fn shootdown(cpus: u32) {
    debug_assert!(Port::irqs_enabled(), "a TLB shootdown with interrupts masked");
    let mut tickets = [0u64; MAX_CPUS];
    for (cpu, ticket) in tickets.iter_mut().enumerate() {
        if cpus & (1 << cpu) != 0 {
            *ticket = ASKED[cpu].fetch_add(1, Ordering::AcqRel) + 1;
            Machine::interrupts().send_ipi(IpiTarget::Cpu(cpu), Machine::IPI_TLB);
        }
    }
    SHOOTDOWNS.fetch_add(1, Ordering::Relaxed);
    let clock = Port::services();
    let start = clock.ticks();
    let limit = clock.frequency().saturating_mul(10);
    for (cpu, ticket) in tickets.iter().enumerate() {
        while *ticket != 0 && DONE[cpu].load(Ordering::Acquire) < *ticket {
            core::hint::spin_loop();
            if clock.ticks().wrapping_sub(start) > limit {
                panic!("TLB shootdown: CPU {cpu} didn't answer in 10 s");
            }
        }
    }
}

/// A shootdown's interrupt, on the CPU it reached.
pub fn on_ipi() {
    let cpu = Port::cpu_index();
    let asked = ASKED[cpu].load(Ordering::Acquire);
    Port::flush_user_local();
    DONE[cpu].fetch_max(asked, Ordering::AcqRel);
}

/// Frames a change gives back while some other CPU may still hold an entry pointing
/// at them, before a shootdown: few, since a shootdown makes room.
const HELD: usize = 64;

/// What one change to a space owes the TLBs: the CPUs to ask before the frames it gave
/// back can go, and those frames meanwhile. [`finish`](Self::finish) settles it.
pub struct Gather<'a> {
    tlb: &'a Tlb,
    /// CPUs to ask, from the flushes so far.
    pending: UnsafeCell<u32>,
    /// Frames held back, with what letting each go is.
    held: UnsafeCell<([(usize, Held); HELD], usize)>,
}

impl<'a> Gather<'a> {
    pub fn new(tlb: &'a Tlb) -> Self {
        Self { tlb, pending: UnsafeCell::new(0), held: UnsafeCell::new(([(0, Held::Page); HELD], 0)) }
    }

    /// Forgets a page whose entry has just changed.
    pub fn flush(&self, address: usize) {
        let others = flush_here(self.tlb, address);
        // SAFETY: a gather belongs to one change on one task, and nothing here re-enters.
        unsafe { *self.pending.get() |= others };
    }

    /// Whether some other CPU may still reach what was just flushed.
    fn pending(&self) -> bool {
        // SAFETY: as in `flush`.
        unsafe { *self.pending.get() != 0 }
    }

    /// Gives a frame back now, or once the CPUs that may still reach it have flushed.
    fn give_back(&self, frame: usize, kind: Held) {
        if !self.pending() {
            release(frame, kind);
            return;
        }
        // SAFETY: as in `flush`.
        let full = unsafe {
            let held = &mut *self.held.get();
            held.0[held.1] = (frame, kind);
            held.1 += 1;
            held.1 == HELD
        };
        HELD_BACK.fetch_add(1, Ordering::Relaxed);
        if full {
            self.settle();
        }
    }

    /// Asks whoever has to be asked, then gives back what was held for it.
    fn settle(&self) {
        // SAFETY: as in `flush`.
        let pending = unsafe { core::mem::take(&mut *self.pending.get()) };
        if pending != 0 {
            shootdown(pending);
        }
        // SAFETY: as in `flush`.
        let held = unsafe { &mut *self.held.get() };
        for &(frame, kind) in &held.0[..held.1] {
            release(frame, kind);
        }
        held.1 = 0;
    }

    /// Settles the change: called once it's done, before its result is used.
    pub fn finish(self) {
        self.settle();
    }
}

/// What a held frame is waiting to have done: a table freed, a page's reference put, or
/// the pin that kept a frame whose shared reference already went from being freed.
#[derive(Clone, Copy)]
enum Held {
    Table,
    Page,
    Pin,
}

fn release(frame: usize, kind: Held) {
    match kind {
        Held::Table => crate::memory::put_frame(frame),
        Held::Page => crate::memory::put_frame_noting(frame),
        Held::Pin => crate::memory::unpin_frame(frame),
    }
}

/// The kernel's frames, with what a change gives back passed through its [`Gather`].
pub struct GatherFrames<'a, 'b>(pub &'b Gather<'a>);

impl FrameSource for GatherFrames<'_, '_> {
    fn alloc(&mut self) -> Option<usize> {
        super::KernelFrames.alloc()
    }

    fn free(&mut self, frame: usize) {
        self.0.give_back(frame, Held::Table);
    }

    fn get(&mut self, frame: usize) -> bool {
        super::KernelFrames.get(frame)
    }

    fn put(&mut self, frame: usize) {
        self.0.give_back(frame, Held::Page);
    }

    fn put_shared(&mut self, frame: usize) -> bool {
        if !self.0.pending() {
            return super::KernelFrames.put_shared(frame);
        }
        // Never the last reference, but the space sharing the frame may put that one at
        // any moment, waiting only for its own CPUs, and this space's may still reach the
        // frame until this change settles: freed, it could be handed to anyone. So it's
        // pinned first, while this reference keeps it, and the pin is what's held: a
        // pinned frame outlives its last put, and the sharer, seeing it pinned, copies
        // rather than writes it in place. Pinned before the drop, so that stays one step.
        if !crate::memory::pin_frame(frame) {
            // Pins at the cap: wait for this space's CPUs now instead.
            self.0.settle();
            return super::KernelFrames.put_shared(frame);
        }
        if super::KernelFrames.put_shared(frame) {
            self.0.give_back(frame, Held::Pin);
            true
        } else {
            // The last reference after all, for a plain put, which is held as ever.
            crate::memory::unpin_frame(frame);
            false
        }
    }

    fn pins(&self, frame: usize) -> u32 {
        super::KernelFrames.pins(frame)
    }

    fn maps(&self, frame: usize) -> u32 {
        super::KernelFrames.maps(frame)
    }

    fn pin(&mut self, frame: usize) -> bool {
        super::KernelFrames.pin(frame)
    }

    fn unpin(&mut self, frame: usize) {
        super::KernelFrames.unpin(frame);
    }

    fn free_count(&self) -> usize {
        super::KernelFrames.free_count()
    }
}
