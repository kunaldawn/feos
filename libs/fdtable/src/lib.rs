//! A process's file descriptor table: which open file description each descriptor
//! refers to, and whether it closes on `EXEC`.
//!
//! The table is generic over what it holds (the kernel's `Arc` of an open file
//! description), and does nothing to it but keep it and give it back: whatever comes out
//! of the table, by a close, a `dup3` over an open descriptor or a sweep, is returned to
//! the caller, which drops it once it has let go of whatever lock guards the table. A
//! close can be real work (a pipe end's last close wakes a reader, a file's writes back
//! what its mappings dirtied), and none of that belongs under the table's lock.
//!
//! The table grows as it's used, up to the limit it's made with, and never by more than
//! a descriptor at a time beyond the highest one asked for, so a process with three
//! descriptors open has room for three. Growth is fallible: no memory is
//! [`FdError::NoMemory`], never a panic, since the size comes from what a program asks.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;

/// Why the table couldn't do what was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdError {
    /// The descriptor isn't open, or is past the limit: `EBADF`.
    BadFd,
    /// The request makes no sense, such as `dup3` onto itself: `EINVAL`.
    Invalid,
    /// Every descriptor from where the search starts up to the limit is open: `EMFILE`.
    Full,
    /// No memory to grow the table with: `ENOMEM`, or `EMFILE` where a call can only
    /// say that.
    NoMemory,
}

/// One open descriptor.
#[derive(Clone, Debug)]
struct Entry<T> {
    open: T,
    cloexec: bool,
}

/// The table. `T` is what a descriptor refers to.
#[derive(Debug)]
pub struct FdTable<T> {
    entries: Vec<Option<Entry<T>>>,
    /// One past the highest descriptor there may be.
    limit: usize,
}

impl<T: Clone> FdTable<T> {
    /// An empty table for descriptors below `limit`.
    pub const fn new(limit: usize) -> Self {
        Self { entries: Vec::new(), limit }
    }

    /// One past the highest descriptor there may be.
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// How many descriptors the table has room for now, open or not.
    pub fn capacity(&self) -> usize {
        self.entries.len()
    }

    /// How many descriptors are open.
    pub fn open_count(&self) -> usize {
        self.entries.iter().filter(|entry| entry.is_some()).count()
    }

    /// What `fd` refers to, if it's open.
    pub fn get(&self, fd: usize) -> Option<&T> {
        self.entries.get(fd)?.as_ref().map(|entry| &entry.open)
    }

    /// Whether `fd` closes on `EXEC`, if it's open.
    pub fn cloexec(&self, fd: usize) -> Option<bool> {
        self.entries.get(fd)?.as_ref().map(|entry| entry.cloexec)
    }

    /// Sets whether `fd` closes on `EXEC`.
    pub fn set_cloexec(&mut self, fd: usize, cloexec: bool) -> Result<(), FdError> {
        match self.entries.get_mut(fd) {
            Some(Some(entry)) => {
                entry.cloexec = cloexec;
                Ok(())
            }
            _ => Err(FdError::BadFd),
        }
    }

    /// The lowest descriptor at or above `from` that isn't open, whether or not the
    /// table has grown to it yet.
    pub fn lowest_free(&self, from: usize) -> Option<usize> {
        if from >= self.limit {
            return None;
        }
        let found = self.entries.iter().enumerate().skip(from).find(|(_, entry)| entry.is_none());
        match found {
            Some((fd, _)) => Some(fd),
            None => Some(self.entries.len().max(from)).filter(|&fd| fd < self.limit),
        }
    }

    /// Makes room for descriptor `fd`, which is below the limit.
    fn grow_to(&mut self, fd: usize) -> Result<(), FdError> {
        if fd < self.entries.len() {
            return Ok(());
        }
        let more = fd + 1 - self.entries.len();
        self.entries.try_reserve(more).map_err(|_| FdError::NoMemory)?;
        self.entries.resize_with(fd + 1, || None);
        Ok(())
    }

    /// Whether `count` descriptors at or above `from` could be had now: for a call that
    /// makes several (a pipe's two ends), to fail before anything is made.
    pub fn has_room(&self, from: usize, count: usize) -> bool {
        let below = self.entries.iter().skip(from).filter(|entry| entry.is_none()).count();
        let beyond = self.limit.saturating_sub(self.entries.len().max(from));
        below + beyond >= count
    }

    /// Puts `open` at the lowest free descriptor at or above `from`, and says which. On
    /// failure `open` comes back, so that the caller decides where it's dropped.
    pub fn insert(&mut self, open: T, cloexec: bool, from: usize) -> Result<usize, (FdError, T)> {
        let Some(fd) = self.lowest_free(from) else {
            return Err((FdError::Full, open));
        };
        if let Err(error) = self.grow_to(fd) {
            return Err((error, open));
        }
        self.entries[fd] = Some(Entry { open, cloexec });
        Ok(fd)
    }

    /// Takes `fd` out of the table, and gives back what it referred to.
    pub fn remove(&mut self, fd: usize) -> Result<T, FdError> {
        let entry = self.entries.get_mut(fd).and_then(Option::take).ok_or(FdError::BadFd)?;
        // A table that has shrunk to what's open grows again as cheaply as it shrank.
        while matches!(self.entries.last(), Some(None)) {
            self.entries.pop();
        }
        Ok(entry.open)
    }

    /// `dup3`: makes `new` refer to what `old` does, closing `new` first if it was open,
    /// and gives back what `new` referred to before, if anything, for the caller to close.
    /// `old == new` is [`FdError::Invalid`], and `new` past the limit or `old` not open
    /// [`FdError::BadFd`]; nothing changes when it fails.
    pub fn dup3(&mut self, old: usize, new: usize, cloexec: bool) -> Result<Option<T>, FdError> {
        // In Linux's order: the same descriptor first, whether or not it's open.
        if old == new {
            return Err(FdError::Invalid);
        }
        if new >= self.limit {
            return Err(FdError::BadFd);
        }
        let open = self.get(old).cloned().ok_or(FdError::BadFd)?;
        self.grow_to(new)?;
        let before = self.entries[new].replace(Entry { open, cloexec });
        Ok(before.map(|entry| entry.open))
    }

    /// `F_DUPFD`: a new descriptor, the lowest free at or above `from`, referring to what
    /// `old` does.
    pub fn dup_from(&mut self, old: usize, from: usize, cloexec: bool) -> Result<usize, FdError> {
        let open = self.get(old).cloned().ok_or(FdError::BadFd)?;
        // What comes back on failure is a copy of what `old` still holds.
        self.insert(open, cloexec, from).map_err(|(error, _)| error)
    }

    /// Closes every descriptor marked to close on `EXEC`, and gives back what they
    /// referred to.
    pub fn cloexec_sweep(&mut self) -> Vec<T> {
        let mut closed = Vec::new();
        for slot in &mut self.entries {
            if slot.as_ref().is_some_and(|entry| entry.cloexec) {
                if let Some(entry) = slot.take() {
                    closed.push(entry.open);
                }
            }
        }
        while matches!(self.entries.last(), Some(None)) {
            self.entries.pop();
        }
        closed
    }

    /// `CLOSE_RANGE`: closes every open descriptor from `first` to `last` inclusive, and
    /// hands each to `closed`; with `cloexec_only`, marks them to close on `EXEC` instead
    /// and closes nothing. `first > last` is [`FdError::Invalid`].
    pub fn close_range(&mut self, first: usize, last: usize, cloexec_only: bool, mut closed: impl FnMut(T)) -> Result<(), FdError> {
        if first > last {
            return Err(FdError::Invalid);
        }
        let end = last.saturating_add(1).min(self.entries.len());
        for slot in self.entries.iter_mut().take(end).skip(first) {
            if cloexec_only {
                if let Some(entry) = slot {
                    entry.cloexec = true;
                }
            } else if let Some(entry) = slot.take() {
                closed(entry.open);
            }
        }
        while matches!(self.entries.last(), Some(None)) {
            self.entries.pop();
        }
        Ok(())
    }

    /// Puts `open` at `fd` itself, closing what was there, and gives that back for the
    /// caller to close: a spawn's `OPEN` action. `fd` past the limit is
    /// [`FdError::BadFd`], and `open` comes back on failure.
    pub fn put(&mut self, fd: usize, open: T, cloexec: bool) -> Result<Option<T>, (FdError, T)> {
        if fd >= self.limit {
            return Err((FdError::BadFd, open));
        }
        if let Err(error) = self.grow_to(fd) {
            return Err((error, open));
        }
        Ok(self.entries[fd].replace(Entry { open, cloexec }).map(|entry| entry.open))
    }

    /// A copy of the table, each descriptor referring to what it does here, its
    /// close-on-exec flag as it is: a new process's, before its spawn's actions.
    /// [`FdError::NoMemory`] if there's no room for it.
    pub fn try_clone(&self) -> Result<Self, FdError> {
        let mut entries = Vec::new();
        entries.try_reserve_exact(self.entries.len()).map_err(|_| FdError::NoMemory)?;
        entries.extend(self.entries.iter().cloned());
        Ok(Self { entries, limit: self.limit })
    }

    /// Every open descriptor, lowest first, with what it refers to.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &T)> {
        self.entries.iter().enumerate().filter_map(|(fd, entry)| entry.as_ref().map(|entry| (fd, &entry.open)))
    }

    /// Takes everything out, for the caller to close, leaving the table empty.
    pub fn drain(&mut self) -> Vec<T> {
        let entries = core::mem::take(&mut self.entries);
        entries.into_iter().flatten().map(|entry| entry.open).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    const LIMIT: usize = 1024;

    #[test]
    fn put_replaces_and_clone_shares() {
        let mut table = FdTable::new(LIMIT);
        let first = Rc::new(1);
        assert_eq!(table.insert(first.clone(), false, 0), Ok(0));
        assert_eq!(table.put(5, Rc::new(5), true).map_err(|(error, _)| error), Ok(None));
        assert_eq!(table.cloexec(5), Some(true));
        let before = table.put(0, Rc::new(9), false).ok().flatten();
        assert!(before.is_some_and(|before| Rc::ptr_eq(&before, &first)));
        assert_eq!(table.put(LIMIT, Rc::new(0), false).map_err(|(error, _)| error), Err(FdError::BadFd));
        let copy = table.try_clone().unwrap();
        assert!(Rc::ptr_eq(copy.get(5).unwrap(), table.get(5).unwrap()));
        assert_eq!(copy.cloexec(5), Some(true));
        assert_eq!(copy.open_count(), 2);
        assert_eq!(Rc::strong_count(table.get(5).unwrap()), 2);
    }

    #[test]
    fn lowest_free() {
        let mut table = FdTable::new(LIMIT);
        for expected in 0..5 {
            assert_eq!(table.insert(expected, false, 0), Ok(expected));
        }
        assert_eq!(table.remove(1), Ok(1));
        assert_eq!(table.remove(3), Ok(3));
        // The lowest free first, then the next, then past the end.
        assert_eq!(table.insert(10, false, 0), Ok(1));
        assert_eq!(table.insert(11, false, 0), Ok(3));
        assert_eq!(table.insert(12, false, 0), Ok(5));
        // From a floor, the lowest free at or above it, even beyond what the table has.
        assert_eq!(table.lowest_free(2), Some(6));
        assert_eq!(table.insert(13, false, 100), Ok(100));
        assert_eq!(table.lowest_free(6), Some(6));
        assert_eq!(table.capacity(), 101);
        assert_eq!(table.lowest_free(LIMIT), None);
        assert_eq!(table.insert(14, false, LIMIT), Err((FdError::Full, 14)));
        assert_eq!(table.remove(77), Err(FdError::BadFd));
        assert_eq!(table.remove(5000), Err(FdError::BadFd));
    }

    #[test]
    fn grows_to_max() {
        let mut table = FdTable::new(LIMIT);
        for fd in 0..LIMIT {
            assert_eq!(table.insert(fd, false, 0), Ok(fd));
            // One more at a time, never far beyond what's open.
            assert_eq!(table.capacity(), fd + 1);
        }
        assert_eq!(table.open_count(), LIMIT);
        assert_eq!(table.insert(0, false, 0), Err((FdError::Full, 0)));
        assert!(!table.has_room(0, 1));
        // A gap is found again, and is the only room there is.
        assert_eq!(table.remove(500), Ok(500));
        assert!(table.has_room(0, 1));
        assert!(!table.has_room(0, 2));
        assert!(!table.has_room(501, 1));
        assert_eq!(table.insert(9, false, 0), Ok(500));
        // Emptied from the top, it shrinks to what's open.
        for fd in (10..LIMIT).rev() {
            assert_eq!(table.remove(fd), Ok(if fd == 500 { 9 } else { fd }));
        }
        assert_eq!(table.capacity(), 10);
        assert!(table.has_room(0, LIMIT - 10));
        assert!(!table.has_room(0, LIMIT - 9));
    }

    #[test]
    fn dup3_replaces() {
        let mut table = FdTable::new(LIMIT);
        let first = Rc::new("first");
        let second = Rc::new("second");
        assert_eq!(table.insert(first.clone(), false, 0), Ok(0));
        assert_eq!(table.insert(second.clone(), false, 0), Ok(1));
        // Onto an open descriptor: it now shares the first, and what it had comes back.
        let replaced = table.dup3(0, 1, false).unwrap();
        assert!(Rc::ptr_eq(&replaced.unwrap(), &second));
        assert!(Rc::ptr_eq(table.get(1).unwrap(), &first));
        assert_eq!(Rc::strong_count(&first), 3);
        // Onto one far beyond the table: it grows to hold it.
        assert_eq!(table.dup3(0, 900, true).map(|old| old.is_none()), Ok(true));
        assert_eq!(table.capacity(), 901);
        assert_eq!(table.cloexec(900), Some(true));
        assert_eq!(table.cloexec(0), Some(false));
        // Refused, with nothing changed.
        assert_eq!(table.dup3(0, 0, false).err(), Some(FdError::Invalid));
        assert_eq!(table.dup3(0, LIMIT, false).err(), Some(FdError::BadFd));
        assert_eq!(table.dup3(5, 6, false).err(), Some(FdError::BadFd));
        assert_eq!(table.dup3(5, 5, false).err(), Some(FdError::Invalid));
        assert!(table.get(6).is_none());
        assert_eq!(Rc::strong_count(&first), 4);
        // F_DUPFD: the lowest free at or above the floor.
        assert_eq!(table.dup_from(0, 0, false), Ok(2));
        assert_eq!(table.dup_from(1, 899, true), Ok(899));
        assert_eq!(table.dup_from(1, 899, false), Ok(901));
        assert_eq!(table.dup_from(7, 0, false), Err(FdError::BadFd));
    }

    #[test]
    fn cloexec_sweep() {
        let mut table = FdTable::new(LIMIT);
        for fd in 0..10 {
            assert_eq!(table.insert(fd, fd % 3 == 0, 0), Ok(fd));
        }
        assert_eq!(table.set_cloexec(4, true), Ok(()));
        assert_eq!(table.set_cloexec(3, false), Ok(()));
        assert_eq!(table.set_cloexec(42, true), Err(FdError::BadFd));
        let mut closed = table.cloexec_sweep();
        closed.sort_unstable();
        assert_eq!(closed, [0, 4, 6, 9]);
        let open: Vec<usize> = table.iter().map(|(fd, _)| fd).collect();
        assert_eq!(open, [1, 2, 3, 5, 7, 8]);
        // The top went with them.
        assert_eq!(table.capacity(), 9);
        assert!(table.cloexec_sweep().is_empty());
    }

    #[test]
    fn close_range() {
        let mut table = FdTable::new(LIMIT);
        for fd in 0..20 {
            assert_eq!(table.insert(fd, false, 0), Ok(fd));
        }
        let mut closed = Vec::new();
        assert_eq!(table.close_range(5, 9, false, |open| closed.push(open)), Ok(()));
        assert_eq!(closed, [5, 6, 7, 8, 9]);
        // Past the table and up to the largest there could be.
        closed.clear();
        assert_eq!(table.close_range(15, u32::MAX as usize, false, |open| closed.push(open)), Ok(()));
        assert_eq!(closed, [15, 16, 17, 18, 19]);
        assert_eq!(table.capacity(), 15);
        // Empty ranges, and ranges of nothing open, are fine; a reversed one isn't.
        assert_eq!(table.close_range(7, 7, false, |_| panic!("nothing open there")), Ok(()));
        assert_eq!(table.close_range(500, 600, false, |_| panic!("nothing open there")), Ok(()));
        assert_eq!(table.close_range(9, 3, false, |_| panic!("refused")), Err(FdError::Invalid));
        let open: Vec<usize> = table.iter().map(|(fd, _)| fd).collect();
        assert_eq!(open, [0, 1, 2, 3, 4, 10, 11, 12, 13, 14]);
        // Everything.
        closed.clear();
        assert_eq!(table.close_range(0, usize::MAX, false, |open| closed.push(open)), Ok(()));
        assert_eq!(closed.len(), 10);
        assert_eq!(table.capacity(), 0);
    }

    #[test]
    fn close_range_cloexec() {
        let mut table = FdTable::new(LIMIT);
        for fd in 0..8 {
            assert_eq!(table.insert(fd, false, 0), Ok(fd));
        }
        assert_eq!(table.close_range(2, 5, true, |_| panic!("only marked")), Ok(()));
        assert_eq!(table.open_count(), 8);
        let marked: Vec<bool> = (0..8).map(|fd| table.cloexec(fd).unwrap()).collect();
        assert_eq!(marked, [false, false, true, true, true, true, false, false]);
        let mut swept = table.cloexec_sweep();
        swept.sort_unstable();
        assert_eq!(swept, [2, 3, 4, 5]);
        assert_eq!(table.close_range(6, 3, true, |_| {}), Err(FdError::Invalid));
    }

    #[test]
    fn drain_gives_everything_back() {
        let mut table = FdTable::new(4);
        let shared = Rc::new(());
        for _ in 0..4 {
            table.insert(shared.clone(), false, 0).unwrap();
        }
        assert_eq!(table.insert(shared.clone(), false, 0).map_err(|(error, _)| error), Err(FdError::Full));
        assert_eq!(Rc::strong_count(&shared), 5);
        drop(table.drain());
        assert_eq!(Rc::strong_count(&shared), 1);
        assert_eq!(table.capacity(), 0);
    }
}
