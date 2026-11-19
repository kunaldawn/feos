//! Exit records (stage 1c.8): what a user process's parent keeps about each child it has
//! started and not yet reaped, running or ended.
//!
//! A record is allocated in the parent before its child exists (`SPAWN2`, v1's `SPAWN`),
//! so a child's end only fills it in, under the scheduler's lock, without allocating, and
//! a child is never lost to a ring of recent exits. Three references keep one:
//!
//! - the parent's list, until `WAIT4` reaps it or the parent ends;
//! - the child's [`Process`](super::Process), which adds its memory use and its own
//!   reaped children's to it as it goes;
//! - the scheduler's, one for the child's whole thread group: every thread's slot points
//!   at it (`sched`'s `RECORD`) without counting, and the last thread to be collected
//!   fills it in and gives that reference up. Under the scheduler's lock nothing may be
//!   freed, so it's given up onto [`release`]'s list, and dropped later in task context.
//!
//! A record's pid is held (see [`held`]) for as long as the record lives, so no new task
//! takes it while the parent could still wait for it, or `KILL` it as a zombie. Its
//! group's is held too, as a zombie is still in its group: a new process given that pid
//! and leading a group of its own would otherwise be in the same group as it, for the
//! parent's `WAIT4` of the group.

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use feos_signal::Status;
use feos_sync::TryArc;

/// One child, as its parent knows it.
pub struct ChildRecord {
    /// Its pid, from when its task is made (under the scheduler's lock), 0 before.
    pid: AtomicU32,
    /// Its process group, kept as it changes, for `WAIT4` of a group.
    pgid: AtomicU32,
    /// How it ended, as [`Status::to_bits`]; 0 while it lives. Written once, under the
    /// scheduler's lock, as its last thread is collected.
    end: AtomicU64,
    /// A stop or continue not yet reported, as [`Status::to_bits`]; 0 for none. Written
    /// under the scheduler's lock, taken by `WAIT4`.
    change: AtomicU64,
    /// Scheduler ticks its threads ran, and its reaped children's.
    ticks: AtomicU64,
    /// The most pages it, or one of its reaped children, ever had.
    peak: AtomicU64,
    /// Page faults its spaces resolved, and its reaped children's.
    faults: AtomicU64,
    /// The next record on [`RELEASED`], once the scheduler has given it up.
    next: AtomicPtr<ChildRecord>,
    /// Its parent ignored `SIGCHLD` or had `SA_NOCLDWAIT` when it ended: nobody waits
    /// for it, and the parent's list drops it once it's settled.
    detached: AtomicBool,
}

/// What a reaped child used, with its own reaped children: what `RUSAGE_CHILDREN` adds up.
#[derive(Clone, Copy, Default, Debug)]
pub struct Usage {
    pub ticks: u64,
    pub peak: u64,
    pub faults: u64,
}

impl ChildRecord {
    /// A record for a child about to be made, or `None` with no heap for one.
    pub fn new() -> Option<TryArc<Self>> {
        TryArc::new(Self {
            pid: AtomicU32::new(0),
            pgid: AtomicU32::new(0),
            end: AtomicU64::new(0),
            change: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            faults: AtomicU64::new(0),
            next: AtomicPtr::new(core::ptr::null_mut()),
            detached: AtomicBool::new(false),
        })
        .ok()
    }

    /// Its child's pid, or 0 while it's being made.
    pub fn pid(&self) -> u32 {
        self.pid.load(Ordering::Acquire)
    }

    /// Its child's process group.
    pub fn pgid(&self) -> u32 {
        self.pgid.load(Ordering::Acquire)
    }

    /// The child is `pid`, in the group `pgid`: under the scheduler's lock, as its task is
    /// committed. Its pid and group are held from here until the record goes.
    pub fn made(&self, pid: u32, pgid: u32) {
        hold(pid, true);
        hold(pgid, true);
        self.pgid.store(pgid, Ordering::Release);
        self.pid.store(pid, Ordering::Release);
    }

    /// Its child has moved to the group `pgid`. Under the scheduler's lock.
    pub fn set_pgid(&self, pgid: u32) {
        // Only a made record is held, and only one has a group to change.
        if self.pid.load(Ordering::Acquire) != 0 {
            hold(pgid, true);
            hold(self.pgid.swap(pgid, Ordering::AcqRel), false);
        } else {
            self.pgid.store(pgid, Ordering::Release);
        }
    }

    /// How its child ended, if it has.
    pub fn end(&self) -> Option<Status> {
        Status::from_bits(self.end.load(Ordering::Acquire))
    }

    /// Its child has ended with `status`. Under the scheduler's lock: complete before
    /// anyone waiting is woken.
    pub fn ended(&self, status: Status) {
        self.change.store(0, Ordering::Release);
        self.end.store(status.to_bits(), Ordering::Release);
    }

    /// Its parent doesn't wait for it: under the scheduler's lock, as it ends.
    pub fn detach(&self) {
        self.detached.store(true, Ordering::Release);
    }

    /// Whether its parent doesn't wait for it.
    pub fn detached(&self) -> bool {
        self.detached.load(Ordering::Acquire)
    }

    /// Its child has stopped or been continued, not yet reported: a continue replaces a
    /// stop nobody has seen, as on Linux. Under the scheduler's lock.
    pub fn changed(&self, status: Status) {
        if self.end.load(Ordering::Acquire) == 0 {
            self.change.store(status.to_bits(), Ordering::Release);
        }
    }

    /// Takes the stop or continue not yet reported, if it's one `wanted` accepts: each is
    /// reported once.
    pub fn take_change(&self, wanted: impl Fn(Status) -> bool) -> Option<Status> {
        let bits = self.change.load(Ordering::Acquire);
        let status = Status::from_bits(bits).filter(|&status| wanted(status))?;
        self.change.compare_exchange(bits, 0, Ordering::AcqRel, Ordering::Acquire).ok()?;
        Some(status)
    }

    /// Adds a thread's CPU time, as it's collected. Under the scheduler's lock.
    pub fn add_ticks(&self, ticks: u64) {
        self.ticks.fetch_add(ticks, Ordering::AcqRel);
    }

    /// Adds what the child's process used besides CPU time, and what its own reaped
    /// children did, as the process goes.
    pub fn add_usage(&self, usage: Usage) {
        self.ticks.fetch_add(usage.ticks, Ordering::AcqRel);
        self.peak.fetch_max(usage.peak, Ordering::AcqRel);
        self.faults.fetch_add(usage.faults, Ordering::AcqRel);
    }

    /// Everything the child used, its reaped children with it.
    pub fn usage(&self) -> Usage {
        Usage {
            ticks: self.ticks.load(Ordering::Acquire),
            peak: self.peak.load(Ordering::Acquire),
            faults: self.faults.load(Ordering::Acquire),
        }
    }
}

impl Drop for ChildRecord {
    /// Its pid, and its group's, are free to be given again.
    fn drop(&mut self) {
        let pid = *self.pid.get_mut();
        if pid != 0 {
            hold(pid, false);
            hold(*self.pgid.get_mut(), false);
        }
    }
}

/// Records the scheduler has given up and nobody has dropped yet.
static RELEASED: AtomicPtr<ChildRecord> = AtomicPtr::new(core::ptr::null_mut());

/// Gives up the scheduler's reference to a record, `record` from [`TryArc::into_raw`],
/// without dropping it: under the scheduler's lock, where nothing may be freed. A record
/// is given up once, by its child's last thread, so it's on the list at most once.
///
/// # Safety
///
/// `record` came from `TryArc::into_raw` and stands for a reference not yet taken back.
pub unsafe fn release(record: *const ChildRecord) {
    // SAFETY: the reference keeps the record alive until `drain` takes it back.
    let node = unsafe { &*record };
    let mut head = RELEASED.load(Ordering::Acquire);
    loop {
        node.next.store(head, Ordering::Relaxed);
        match RELEASED.compare_exchange_weak(head, record.cast_mut(), Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(now) => head = now,
        }
    }
}

/// Drops the references [`release`] gave up, freeing whatever they were the last of. In
/// task context, with no lock held.
pub fn drain() {
    let mut next = RELEASED.swap(core::ptr::null_mut(), Ordering::AcqRel);
    while !next.is_null() {
        // SAFETY: everything on the list was put there by `release`, once, with its
        // reference, which only this takes back.
        let record = unsafe { TryArc::from_raw(next.cast_const()) };
        next = record.next.load(Ordering::Acquire);
        drop(record);
    }
}

/// How many buckets [`held`] counts pids in. A bucket with any record in it holds every
/// pid that falls in it, which only matters once the pid counter has wrapped, and then
/// only skips pids: there are four billion.
const BUCKETS: usize = 1 << 16;

// The scheduler tries this many pids at most, and each held bucket covers one in every
// `BUCKETS` of them: fewer buckets, less what's held exactly, and one is always found
// while any bucket is free.
const _: () = assert!(BUCKETS * 3 / 2 <= feos_sched::PID_TRIES);

/// Records alive in each bucket of pids.
static HOLDS: [AtomicU32; BUCKETS] = [const { AtomicU32::new(0) }; BUCKETS];

fn hold(pid: u32, held: bool) {
    let bucket = &HOLDS[pid as usize % BUCKETS];
    if held {
        bucket.fetch_add(1, Ordering::AcqRel);
    } else {
        bucket.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Whether `pid` may be a record's that's still alive: a child nobody has reaped yet, or
/// one whose parent hasn't dropped its record. Lock-free, for the scheduler's table.
pub fn held(pid: u32) -> bool {
    HOLDS[pid as usize % BUCKETS].load(Ordering::Acquire) != 0
}
