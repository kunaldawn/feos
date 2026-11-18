//! Advisory locks (stage 1c.5): `FLOCK`, and `fcntl`'s record locks `F_GETLK`, `F_SETLK`
//! and `F_SETLKW`.
//!
//! One table for the machine (`feos_locks`, which decides conflicts, ranges and caps),
//! keyed by the node identity a description holds its file by, which the tree keeps the
//! file's while anything holds it. Owners: a process (by pid) for record locks, an open
//! file description (by its address, which is its own for as long as it lives) for
//! `flock`. They go:
//!
//! - a process's record locks on a file, when it closes any descriptor for the file
//!   ([`descriptor_closed`], from every place a descriptor leaves its table), and all of
//!   them when it ends ([`process_ended`], from the `Process`'s drop, after its last
//!   thread);
//! - a description's `flock`, with its last reference ([`description_closed`], from its
//!   drop).
//!
//! A wait (`F_SETLKW`, `flock` without `LOCK_NB`) is a kernel futex wait on a counter
//! that every change letting go of a lock moves on, holding nothing: no table lock, no
//! descriptor table, no user memory pinned. A kill ends it (`KILLED`). A refused attempt
//! never moves the counter, so waiters can't wake each other forever.
//!
//! The table is a leaf lock: nothing else is taken with it held, and it's never held
//! while user memory is copied (the `Flock` is copied in before and out after) or the
//! tree is asked for a size.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use feos_abi::{errno, f, int_arg, lock as op, seek as whence, Flock, MAX_LOCKS, MAX_PROCESS_LOCKS};
use feos_fs::vfs::FileId;
use feos_locks::{FileKey, Kind, LockError, LockTable, Owner, Range, RangeError, Refused};
use feos_sync::{futex_wait, futex_wake, Mutex, TryArc};

use super::files::{fd_arg, lookup, Object, Ofd, OfdRef};
use super::process::Process;
use super::{fs_errno, tree, KILLED};
use crate::sched;

static TABLE: Mutex<LockTable> = Mutex::new(LockTable::new(MAX_LOCKS, MAX_PROCESS_LOCKS));

/// Moved on by every change that let go of or weakened a lock: what a waiter waits on.
static CHANGES: AtomicU32 = AtomicU32::new(0);

/// How many locks the table holds, as of its last change: a close looks at this before
/// taking the table's lock, so closes cost nothing while nobody locks anything.
static HELD: AtomicUsize = AtomicUsize::new(0);

/// Locks held machine-wide, for `ktest`.
pub fn count() -> usize {
    HELD.load(Ordering::Acquire)
}

const fn key_of(file: FileId) -> FileKey {
    FileKey { dev: file.dev, ino: file.ino }
}

/// What `ofd`'s `flock` is owned as.
fn description_of(ofd: &Ofd) -> usize {
    ofd as *const Ofd as usize
}

/// Whoever waits for a lock looks again.
fn changed() {
    CHANGES.fetch_add(1, Ordering::Release);
    futex_wake(&CHANGES, usize::MAX);
}

/// Runs `change` on the table, and wakes waiters if it let anything go.
fn with_table<T>(change: impl FnOnce(&mut LockTable) -> (T, bool)) -> T {
    let (result, released) = {
        let mut table = TABLE.lock();
        let done = change(&mut table);
        HELD.store(table.len(), Ordering::Release);
        done
    };
    if released {
        changed();
    }
    result
}

/// Tries `attempt` until it gets its lock, or with `wait` false only once: `EAGAIN` on a
/// conflict, `ENOLCK` at a cap, `KILLED` if the caller is killed while it waits.
fn take(wait: bool, mut attempt: impl FnMut(&mut LockTable) -> Result<bool, Refused>) -> isize {
    loop {
        // Read before the attempt, so a release between the attempt and the wait isn't
        // missed: the wait returns at once.
        let seen = CHANGES.load(Ordering::Acquire);
        let result = with_table(|table| match attempt(table) {
            Ok(released) => (Ok(()), released),
            Err(refused) => (Err(refused.why), refused.released),
        });
        match result {
            Ok(()) => return 0,
            Err(LockError::NoLocks) => return -errno::ENOLCK,
            Err(LockError::Conflict(_)) if !wait => return -errno::EAGAIN,
            // Signals are stage 1c.9's: until then only a kill ends the wait.
            Err(LockError::Conflict(_)) => {
                if sched::kill_requested() {
                    return KILLED;
                }
                futex_wait(&CHANGES, seen);
            }
        }
    }
}

/// The file a lock on `ofd` is a lock on: a regular file's or a device node's identity,
/// or for a `flock` (`record` false) a directory's too. `EBADF` for an `O_PATH`
/// descriptor, `EINVAL` for a pipe, the console or a stream, which have none.
///
/// A directory takes no record locks: its description takes no hold on its identity, so
/// a record lock could outlive the directory and meet whatever its inode number goes to
/// next, held by a process for as long as it keeps any descriptor. A `flock` on one is
/// the description's, and goes when it closes.
fn lockable(ofd: &Ofd, record: bool) -> Result<FileKey, isize> {
    if ofd.path_only {
        return Err(-errno::EBADF);
    }
    match &ofd.object {
        Object::File { id, .. } => Ok(key_of(*id)),
        Object::Directory { id, .. } if !record => Ok(key_of(*id)),
        _ => Err(-errno::EINVAL),
    }
}

/// `flock`.
pub fn sys_flock(process: &Process, fd: usize, operation: usize) -> isize {
    let ofd = match lookup(process, fd) {
        Ok(ofd) => ofd,
        Err(error) => return error,
    };
    let Some(operation) = int_arg(operation).map(|value| value as u32 as usize) else {
        return -errno::EINVAL;
    };
    let kind = match operation & !op::NB {
        op::SH => Some(Kind::Read),
        op::EX => Some(Kind::Write),
        op::UN => None,
        _ => return -errno::EINVAL,
    };
    let file = match lockable(&ofd, false) {
        Ok(file) => file,
        Err(error) => return error,
    };
    let description = description_of(&ofd);
    let pid = process.pid();
    // Marked before it can hold one, so its last close looks.
    if kind.is_some() {
        ofd.mark_flocked();
    }
    take(operation & op::NB == 0, |table| table.flock(file, description, pid, kind))
}

/// The open file description's `flock` goes with it: from its drop.
pub fn description_closed(ofd: &Ofd) {
    let description = description_of(ofd);
    with_table(|table| ((), table.release_owner(Owner::Description(description))));
}

/// `process` has closed a descriptor that referred to `ofd`: its record locks on that
/// file go, whatever other descriptors it has for it, as POSIX says.
pub fn descriptor_closed(process: &Process, ofd: &Ofd) {
    if HELD.load(Ordering::Acquire) == 0 {
        return;
    }
    let Ok(file) = lockable(ofd, true) else {
        return;
    };
    let owner = Owner::Process(process.pid());
    with_table(|table| ((), table.release_file(owner, file)));
}

/// The process `pid` has ended, every thread of it: its record locks go.
pub fn process_ended(pid: u32) {
    if HELD.load(Ordering::Acquire) == 0 {
        return;
    }
    with_table(|table| ((), table.release_owner(Owner::Process(pid))));
}

/// `fcntl`'s `F_GETLK`, `F_SETLK` and `F_SETLKW` on descriptor `fd`, which referred to
/// `ofd` when the call looked; `arg` points at a [`Flock`], read once.
pub fn sys_fcntl_lock(process: &Process, fd: usize, ofd: &OfdRef, command: usize, arg: usize) -> isize {
    let mut bytes = [0u8; Flock::SIZE];
    if let Err(error) = process.mm().copy_in(arg, &mut bytes) {
        return error;
    }
    let given = Flock::from_bytes(bytes);
    let kind = match given.kind {
        f::RDLCK => Some(Kind::Read),
        f::WRLCK => Some(Kind::Write),
        f::UNLCK if command != f::GETLK => None,
        _ => return -errno::EINVAL,
    };
    let file = match lockable(ofd, true) {
        Ok(file) => file,
        Err(error) => return error,
    };
    // Setting a lock takes the access it's a lock for; looking doesn't.
    if command != f::GETLK && ((kind == Some(Kind::Read) && !ofd.readable()) || (kind == Some(Kind::Write) && !ofd.writable())) {
        return -errno::EBADF;
    }
    let range = match range_of(ofd, &given) {
        Ok(range) => range,
        Err(error) => return error,
    };
    let pid = process.pid();
    let owner = Owner::Process(pid);
    if command == f::GETLK {
        let Some(kind) = kind else {
            return -errno::EINVAL;
        };
        let conflict = TABLE.lock().conflict(file, owner, kind, range);
        let answer = match conflict {
            Some(conflict) => Flock {
                kind: if conflict.kind == Kind::Write { f::WRLCK } else { f::RDLCK },
                whence: whence::SET as i16,
                start: conflict.range.start as i64,
                length: conflict.range.length() as i64,
                pid: conflict.pid as i32,
                ..Flock::default()
            },
            None => Flock { kind: f::UNLCK, ..given },
        };
        return match process.mm().copy_out(arg, &answer.to_bytes()) {
            Ok(()) => 0,
            Err(error) => error,
        };
    }
    let attempt = |table: &mut LockTable| table.set(file, owner, pid, kind, range).map_err(|why| Refused { why, released: false });
    let result = take(command == f::SETLKW, attempt);
    if result == 0 && kind.is_some() && !still_open(process, fd, ofd) {
        // Another thread closed the descriptor while this one waited, which let go of
        // the process's locks on the file before this one was had: this one goes too,
        // as on Linux, rather than outlive every descriptor it could be undone through.
        with_table(|table| ((), table.set(file, owner, pid, None, range).unwrap_or(false)));
    }
    result
}

/// Whether `fd` still refers to `ofd`.
fn still_open(process: &Process, fd: usize, ofd: &OfdRef) -> bool {
    let Some(fd) = fd_arg(fd) else {
        return false;
    };
    process.files().table.get(fd).is_some_and(|now| TryArc::ptr_eq(now, ofd))
}

/// The bytes `given` names, its `start` counted as its `whence` says: from zero, the
/// description's offset or the file's end.
fn range_of(ofd: &Ofd, given: &Flock) -> Result<Range, isize> {
    // A negative `whence` is no `SEEK_*` either, as a huge one.
    let base = match given.whence as u16 as usize {
        whence::SET => 0,
        whence::CURRENT => *ofd.offset.lock(),
        whence::END => match &ofd.object {
            Object::File { id, .. } | Object::Directory { id, .. } => {
                let tree = tree().ok_or(-errno::EIO)?;
                tree.stat_file(*id).map_err(fs_errno)?.size
            }
            _ => return Err(-errno::EINVAL),
        },
        _ => return Err(-errno::EINVAL),
    };
    feos_locks::range_of(base, given.start, given.length).map_err(|error| match error {
        RangeError::Invalid => -errno::EINVAL,
        RangeError::Overflow => -errno::EOVERFLOW,
    })
}
