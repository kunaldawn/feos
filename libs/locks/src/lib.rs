//! Advisory file locks, as POSIX's `fcntl` record locks and BSD's `flock` have them: what
//! conflicts with what, how one owner's ranges split, merge and convert, and the caps that
//! keep programs from filling the kernel's memory with locks.
//!
//! The table only decides. Everything else is the kernel's: which file a descriptor is
//! (a [`FileKey`], the node identity that stays a file's while anything holds it), who
//! owns a lock, waiting for a conflict to go, and letting go of a process's locks when it
//! closes a descriptor or ends.
//!
//! Two kinds of owner, which never conflict with each other, as on Linux:
//!
//! - A **process** owns its record locks ([`Owner::Process`]). Its locks on one file are
//!   kept disjoint, adjacent ranges of one kind merged, so a new lock over part of an old
//!   one of the other kind splits it, and unlocking the middle of one leaves two.
//! - An **open file description** owns its `flock` lock ([`Owner::Description`]), one per
//!   file, always the whole file. Changing it from shared to exclusive or back lets go of
//!   the old lock first, so another owner may get in between (BSD's and Linux's behaviour
//!   too): [`Refused::released`] says so, for the kernel to wake whoever was waiting.
//!
//! The caps are a machine-wide count of records, whoever owns them, and a count per
//! process of the records it placed (its record locks, and the `flock` locks it took,
//! charged to it for as long as they last). A change that would add records past either
//! is [`LockError::NoLocks`] (`ENOLCK`), before anything changes; one that doesn't add
//! any (a merge, an unlock that doesn't split) is never refused for them. Growth of the
//! table is fallible, and refused the same way.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;

#[cfg(test)]
mod tests;

/// A file, by its identity: the mount's number and the file's inode number on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileKey {
    pub dev: u64,
    pub ino: u64,
}

/// Who a lock is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// A process, by pid: a record lock.
    Process(u32),
    /// An open file description, by whatever the kernel tells one by while it lives: a
    /// `flock` lock.
    Description(usize),
}

impl Owner {
    /// Whether locks of `self` and `other` can conflict at all: record locks only with
    /// record locks, `flock` locks with `flock` locks.
    const fn same_class(self, other: Self) -> bool {
        matches!((self, other), (Self::Process(_), Self::Process(_)) | (Self::Description(_), Self::Description(_)))
    }
}

/// A lock's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Shared: `F_RDLCK`, `LOCK_SH`.
    Read,
    /// Exclusive: `F_WRLCK`, `LOCK_EX`.
    Write,
}

/// Bytes `start` to `end` of a file, both included; an `end` of [`Range::END`] is to the
/// end of the file, wherever that comes to be. [`range_of`] never makes one ending at
/// [`MAX_OFFSET`] (that's `END`), so every `start` and [`length`](Self::length) of the
/// ranges it makes, and of their splits and merges, fits an `i64`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub start: u64,
    pub end: u64,
}

impl Range {
    /// The `end` of a range that runs on past the end of the file: `len` 0.
    pub const END: u64 = u64::MAX;
    /// All of a file, as a `flock` lock is.
    pub const WHOLE: Self = Self { start: 0, end: Self::END };

    const fn overlaps(self, other: Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }

    /// Overlapping or right next to each other, so that one lock could cover both.
    const fn touches(self, other: Self) -> bool {
        self.start <= other.end.saturating_add(1) && other.start <= self.end.saturating_add(1)
    }

    fn union(self, other: Self) -> Self {
        Self { start: self.start.min(other.start), end: self.end.max(other.end) }
    }

    /// Its `l_len`: the bytes it covers, or zero for one that runs to the end.
    pub const fn length(self) -> u64 {
        if self.end == Self::END {
            0
        } else {
            self.end - self.start + 1
        }
    }
}

/// Why a `struct flock`'s `start` and `length` name no range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeError {
    /// It starts before the file does: `EINVAL`.
    Invalid,
    /// It runs past the largest offset there is: `EOVERFLOW`.
    Overflow,
}

/// The largest offset a file can have, and so a lock's last byte short of [`Range::END`].
pub const MAX_OFFSET: u64 = i64::MAX as u64;

/// The range a `struct flock` names: `start` counted from `base` (zero, the description's
/// offset or the file's size, by its `whence`), `length` bytes from there, zero for all
/// that follows, or negative for the bytes before it, as POSIX says.
pub fn range_of(base: u64, start: i64, length: i64) -> Result<Range, RangeError> {
    let base = i64::try_from(base).map_err(|_| RangeError::Overflow)?;
    let first = base.checked_add(start).ok_or(RangeError::Overflow)?;
    if first < 0 {
        return Err(RangeError::Invalid);
    }
    match length {
        0 => Ok(Range { start: first as u64, end: Range::END }),
        length if length > 0 => {
            let last = first.checked_add(length - 1).ok_or(RangeError::Overflow)?;
            // One that reaches the largest offset covers all that can follow, so it's kept
            // as one that runs to the end (Linux's `OFFSET_MAX` is the same). Otherwise a
            // merge could make `[0, MAX_OFFSET]`, whose length doesn't fit an `l_len`, and
            // unlocking up to it would leave a lock past every offset there is.
            let end = if last as u64 == MAX_OFFSET { Range::END } else { last as u64 };
            Ok(Range { start: first as u64, end })
        }
        // The bytes before `first`, which can't start before the file does. `first` isn't
        // negative, so this can't overflow.
        length => {
            let begin = first + length;
            if begin < 0 {
                return Err(RangeError::Invalid);
            }
            Ok(Range { start: begin as u64, end: first as u64 - 1 })
        }
    }
}

/// One lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lock {
    pub file: FileKey,
    pub owner: Owner,
    pub kind: Kind,
    pub range: Range,
    /// The process it's charged to, which `F_GETLK` reports: a record lock's owner, or the
    /// process that took a `flock` lock.
    pub pid: u32,
}

/// The lock in the way of one asked for, as `F_GETLK` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub kind: Kind,
    pub range: Range,
    pub pid: u32,
}

/// Why a lock wasn't taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockError {
    /// Another owner's lock is in the way: `EAGAIN`, or wait.
    Conflict(Conflict),
    /// Past a cap, or no memory for the table: `ENOLCK`.
    NoLocks,
}

/// A `flock` that wasn't taken, and whether the owner's old lock went anyway (a change of
/// kind isn't atomic), in which case whoever waits for the file should look again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused {
    pub why: LockError,
    pub released: bool,
}

/// Every lock the machine has, against a machine-wide and a per-process cap.
pub struct LockTable {
    locks: Vec<Lock>,
    limit: usize,
    per_process: usize,
}

impl LockTable {
    /// An empty table that keeps at most `limit` locks, at most `per_process` of them
    /// charged to any one process.
    pub const fn new(limit: usize, per_process: usize) -> Self {
        Self { locks: Vec::new(), limit, per_process }
    }

    /// Locks held, by everyone.
    pub fn len(&self) -> usize {
        self.locks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.locks.is_empty()
    }

    /// Every lock, in no particular order.
    pub fn locks(&self) -> &[Lock] {
        &self.locks
    }

    /// Locks charged to the process `pid`.
    pub fn held_by(&self, pid: u32) -> usize {
        self.locks.iter().filter(|lock| lock.pid == pid).count()
    }

    /// The first lock of another owner's (of `owner`'s class) on `file` that a lock of
    /// `kind` over `range` would conflict with: anything over a write lock, a write lock
    /// over anything.
    pub fn conflict(&self, file: FileKey, owner: Owner, kind: Kind, range: Range) -> Option<Conflict> {
        self.locks
            .iter()
            .find(|lock| {
                lock.file == file
                    && lock.owner != owner
                    && lock.owner.same_class(owner)
                    && lock.range.overlaps(range)
                    && (kind == Kind::Write || lock.kind == Kind::Write)
            })
            .map(|lock| Conflict { kind: lock.kind, range: lock.range, pid: lock.pid })
    }

    /// Room for `growth` more records charged to `pid`, or [`LockError::NoLocks`].
    fn make_room(&mut self, pid: u32, growth: usize) -> Result<(), LockError> {
        if growth == 0 {
            return Ok(());
        }
        if self.locks.len() + growth > self.limit || self.held_by(pid) + growth > self.per_process {
            return Err(LockError::NoLocks);
        }
        let wanted = self.locks.len() + growth;
        if wanted > self.locks.capacity() {
            // Doubling, as a `Vec` would, but never past the cap: the table's own memory is
            // bounded by it.
            let capacity = (self.locks.capacity() * 2).max(16).clamp(wanted, self.limit);
            self.locks.try_reserve_exact(capacity - self.locks.len()).map_err(|_| LockError::NoLocks)?;
        }
        Ok(())
    }

    /// Sets `owner`'s lock of `kind` over `range` of `file`, or with `kind` `None` takes
    /// its locks off that range, charged to `pid`: `F_SETLK`. What the owner held of the
    /// range before is replaced (a read lock under a new write lock becomes the write
    /// lock), and the pieces outside the range kept; adjacent ranges of the same kind
    /// become one. All or nothing: a conflict or a cap changes nothing. `Ok(true)` if a
    /// lock was let go of or weakened, so that whoever waits for the file should look
    /// again.
    pub fn set(&mut self, file: FileKey, owner: Owner, pid: u32, kind: Option<Kind>, range: Range) -> Result<bool, LockError> {
        if let Some(kind) = kind {
            if let Some(conflict) = self.conflict(file, owner, kind, range) {
                return Err(LockError::Conflict(conflict));
            }
        }
        // The owner's locks on the file are disjoint, so only one can stick out on each
        // side of the range: at most two pieces are kept, whatever's replaced.
        let replaced = |lock: &Lock| {
            lock.file == file && lock.owner == owner && (lock.range.overlaps(range) || (Some(lock.kind) == kind && lock.range.touches(range)))
        };
        let mut merged = range;
        let (mut removed, mut freed) = (0, false);
        let (mut left, mut right) = (None, None);
        for lock in self.locks.iter().filter(|lock| replaced(lock)) {
            removed += 1;
            if Some(lock.kind) == kind {
                merged = merged.union(lock.range);
                continue;
            }
            freed |= kind.is_none() || lock.kind == Kind::Write;
            if lock.range.start < range.start {
                left = Some(Lock { range: Range { start: lock.range.start, end: range.start - 1 }, ..*lock });
            }
            if lock.range.end > range.end {
                right = Some(Lock { range: Range { start: range.end + 1, end: lock.range.end }, ..*lock });
            }
        }
        let added = usize::from(left.is_some()) + usize::from(right.is_some()) + usize::from(kind.is_some());
        self.make_room(pid, added.saturating_sub(removed))?;
        self.locks.retain(|lock| !replaced(lock));
        // Room was made above for anything more than what went: none of these allocates.
        self.locks.extend(left);
        self.locks.extend(right);
        if let Some(kind) = kind {
            self.locks.push(Lock { file, owner, kind, range: merged, pid });
        }
        Ok(freed)
    }

    /// The `flock` lock of the description `description` on `file`: takes one of `kind`,
    /// charged to `pid`, or with `None` lets go of it. Asking for the kind already held
    /// does nothing; asking for the other lets go of the old one first, which isn't undone
    /// if the new one can't be had.
    pub fn flock(&mut self, file: FileKey, description: usize, pid: u32, kind: Option<Kind>) -> Result<bool, Refused> {
        let owner = Owner::Description(description);
        let held = self.locks.iter().position(|lock| lock.file == file && lock.owner == owner);
        if let Some(at) = held {
            if Some(self.locks[at].kind) == kind {
                return Ok(false);
            }
            self.locks.swap_remove(at);
        }
        let released = held.is_some();
        let Some(kind) = kind else {
            return Ok(released);
        };
        if let Some(conflict) = self.conflict(file, owner, kind, Range::WHOLE) {
            return Err(Refused { why: LockError::Conflict(conflict), released });
        }
        self.make_room(pid, 1).map_err(|why| Refused { why, released })?;
        self.locks.push(Lock { file, owner, kind, range: Range::WHOLE, pid });
        Ok(released)
    }

    /// Lets go of every lock `owner` has on `file`: what closing any descriptor for a file
    /// does to its process's record locks. Whether there were any.
    pub fn release_file(&mut self, owner: Owner, file: FileKey) -> bool {
        let before = self.locks.len();
        self.locks.retain(|lock| !(lock.owner == owner && lock.file == file));
        self.locks.len() != before
    }

    /// Lets go of every lock `owner` has: a process's as it ends, a description's `flock`
    /// as it closes. Whether there were any.
    pub fn release_owner(&mut self, owner: Owner) -> bool {
        let before = self.locks.len();
        self.locks.retain(|lock| lock.owner != owner);
        self.locks.len() != before
    }
}
