use super::*;

const FILE: FileKey = FileKey { dev: 1, ino: 10 };
const OTHER: FileKey = FileKey { dev: 1, ino: 11 };

fn table() -> LockTable {
    LockTable::new(4096, 256)
}

const fn range(start: u64, end: u64) -> Range {
    Range { start, end }
}

fn posix(table: &mut LockTable, pid: u32, kind: Option<Kind>, start: u64, end: u64) -> Result<bool, LockError> {
    table.set(FILE, Owner::Process(pid), pid, kind, range(start, end))
}

/// Pid `pid`'s locks on `FILE`, in order.
fn held(table: &LockTable, pid: u32) -> Vec<(Kind, u64, u64)> {
    let mut out: Vec<_> = table
        .locks()
        .iter()
        .filter(|lock| lock.file == FILE && lock.owner == Owner::Process(pid))
        .map(|lock| (lock.kind, lock.range.start, lock.range.end))
        .collect();
    out.sort_by_key(|&(_, start, _)| start);
    out
}

#[test]
fn range_split_merge() {
    let mut table = table();
    // Adjacent and overlapping ranges of one kind become one.
    posix(&mut table, 1, Some(Kind::Read), 0, 9).unwrap();
    posix(&mut table, 1, Some(Kind::Read), 10, 19).unwrap();
    posix(&mut table, 1, Some(Kind::Read), 15, 29).unwrap();
    assert_eq!(held(&table, 1), [(Kind::Read, 0, 29)]);
    // A write lock in the middle splits the read lock around it.
    posix(&mut table, 1, Some(Kind::Write), 5, 14).unwrap();
    assert_eq!(held(&table, 1), [(Kind::Read, 0, 4), (Kind::Write, 5, 14), (Kind::Read, 15, 29)]);
    // And a read lock over all of it is one again.
    posix(&mut table, 1, Some(Kind::Read), 0, 29).unwrap();
    assert_eq!(held(&table, 1), [(Kind::Read, 0, 29)]);
    // To the end of the file, merged with what reaches it.
    posix(&mut table, 1, Some(Kind::Read), 30, Range::END).unwrap();
    assert_eq!(held(&table, 1), [(Kind::Read, 0, Range::END)]);
    // Ranges that don't touch stay apart, and other files are other files.
    let mut table = self::table();
    posix(&mut table, 1, Some(Kind::Write), 0, 9).unwrap();
    posix(&mut table, 1, Some(Kind::Write), 11, 19).unwrap();
    table.set(OTHER, Owner::Process(1), 1, Some(Kind::Write), range(10, 10)).unwrap();
    assert_eq!(held(&table, 1), [(Kind::Write, 0, 9), (Kind::Write, 11, 19)]);
    assert_eq!(table.len(), 3);
}

#[test]
fn read_locks_share() {
    let mut table = table();
    posix(&mut table, 1, Some(Kind::Read), 0, 99).unwrap();
    posix(&mut table, 2, Some(Kind::Read), 50, 149).unwrap();
    assert_eq!(table.conflict(FILE, Owner::Process(3), Kind::Read, range(0, Range::END)), None);
    let conflict = table.conflict(FILE, Owner::Process(3), Kind::Write, range(120, 130)).unwrap();
    assert_eq!((conflict.pid, conflict.kind), (2, Kind::Read));
    // A process's own locks are never in its way: it upgrades past what it shares.
    assert_eq!(posix(&mut table, 1, Some(Kind::Write), 0, 49), Ok(false));
    assert!(matches!(posix(&mut table, 1, Some(Kind::Write), 0, 60), Err(LockError::Conflict(_))));
}

#[test]
fn write_conflicts() {
    let mut table = table();
    posix(&mut table, 1, Some(Kind::Write), 10, 19).unwrap();
    let refused = posix(&mut table, 2, Some(Kind::Read), 19, 19);
    assert_eq!(refused, Err(LockError::Conflict(Conflict { kind: Kind::Write, range: range(10, 19), pid: 1 })));
    // Right next to it is not over it.
    posix(&mut table, 2, Some(Kind::Write), 20, 29).unwrap();
    posix(&mut table, 2, Some(Kind::Write), 0, 9).unwrap();
    // A refused lock changed nothing.
    assert_eq!(held(&table, 2), [(Kind::Write, 0, 9), (Kind::Write, 20, 29)]);
    assert_eq!(held(&table, 1), [(Kind::Write, 10, 19)]);
    // Record locks and `flock` locks don't see each other.
    assert_eq!(table.flock(FILE, 0x1000, 3, Some(Kind::Write)), Ok(false));
    assert_eq!(table.conflict(FILE, Owner::Process(3), Kind::Write, range(30, 40)), None);
}

#[test]
fn unlock_middle_splits() {
    let mut table = table();
    posix(&mut table, 1, Some(Kind::Write), 0, 99).unwrap();
    assert_eq!(posix(&mut table, 1, None, 40, 59), Ok(true));
    assert_eq!(held(&table, 1), [(Kind::Write, 0, 39), (Kind::Write, 60, 99)]);
    posix(&mut table, 2, Some(Kind::Write), 40, 59).unwrap();
    // Unlocking what isn't held is nothing, and says so.
    assert_eq!(posix(&mut table, 1, None, 40, 59), Ok(false));
    // Unlocking across several takes what's covered and trims the ends.
    assert_eq!(posix(&mut table, 1, None, 30, 69), Ok(true));
    assert_eq!(held(&table, 1), [(Kind::Write, 0, 29), (Kind::Write, 70, 99)]);
    // A write lock made a read lock lets readers in: worth waking them for.
    assert_eq!(posix(&mut table, 1, Some(Kind::Read), 0, 29), Ok(true));
    assert_eq!(posix(&mut table, 1, None, 0, Range::END), Ok(true));
    assert!(held(&table, 1).is_empty());
}

#[test]
fn close_any_fd_drops_posix_locks() {
    let mut table = table();
    posix(&mut table, 1, Some(Kind::Write), 0, 9).unwrap();
    posix(&mut table, 1, Some(Kind::Read), 20, 29).unwrap();
    table.set(OTHER, Owner::Process(1), 1, Some(Kind::Write), range(0, 9)).unwrap();
    posix(&mut table, 2, Some(Kind::Read), 20, 29).unwrap();
    table.flock(FILE, 0x2000, 1, Some(Kind::Read)).unwrap();
    // Closing a descriptor for FILE: all of pid 1's record locks on it, and nothing else.
    assert!(table.release_file(Owner::Process(1), FILE));
    assert!(held(&table, 1).is_empty());
    assert_eq!(held(&table, 2), [(Kind::Read, 20, 29)]);
    assert_eq!(table.len(), 3);
    assert!(!table.release_file(Owner::Process(1), FILE));
    // Its end: the rest of its record locks; the description's `flock` stays.
    assert!(table.release_owner(Owner::Process(1)));
    assert_eq!(table.len(), 2);
    assert!(table.locks().iter().any(|lock| lock.owner == Owner::Description(0x2000)));
}

#[test]
fn flock_follows_ofd() {
    let mut table = table();
    let (first, second) = (0x1000, 0x2000);
    assert_eq!(table.flock(FILE, first, 1, Some(Kind::Write)), Ok(false));
    // Another description conflicts, whichever process holds it; the same one doesn't.
    let refused = table.flock(FILE, second, 1, Some(Kind::Read)).unwrap_err();
    assert!(matches!(refused.why, LockError::Conflict(Conflict { pid: 1, kind: Kind::Write, .. })));
    assert!(!refused.released);
    assert_eq!(table.flock(FILE, first, 2, Some(Kind::Write)), Ok(false));
    assert_eq!(table.len(), 1);
    // The description's last close.
    assert!(table.release_owner(Owner::Description(first)));
    assert_eq!(table.flock(FILE, second, 2, Some(Kind::Read)), Ok(false));
    assert_eq!(table.flock(FILE, first, 1, Some(Kind::Read)), Ok(false));
    // Shared to exclusive isn't atomic: the old lock goes, and the new one can't be had.
    let refused = table.flock(FILE, first, 1, Some(Kind::Write)).unwrap_err();
    assert!(refused.released);
    assert!(matches!(refused.why, LockError::Conflict(Conflict { pid: 2, .. })));
    assert_eq!(table.len(), 1);
    assert_eq!(table.flock(FILE, second, 2, None), Ok(true));
    assert_eq!(table.flock(FILE, second, 2, None), Ok(false));
    assert_eq!(table.flock(FILE, first, 1, Some(Kind::Write)), Ok(false));
    assert_eq!(table.flock(OTHER, second, 2, Some(Kind::Write)), Ok(false));
}

#[test]
fn per_owner_cap() {
    let mut table = LockTable::new(600, 256);
    for index in 0..256 {
        posix(&mut table, 1, Some(Kind::Write), index * 2, index * 2).unwrap();
    }
    assert_eq!(posix(&mut table, 1, Some(Kind::Write), 1000, 1000), Err(LockError::NoLocks));
    // `flock` locks it takes are charged to it too.
    assert_eq!(table.flock(OTHER, 0x1000, 1, Some(Kind::Read)).unwrap_err().why, LockError::NoLocks);
    // What doesn't add a record is never refused for the cap: a merge, an unlock at an end.
    assert_eq!(posix(&mut table, 1, Some(Kind::Write), 1, 1), Ok(false));
    assert_eq!(table.held_by(1), 255);
    posix(&mut table, 1, Some(Kind::Write), 1000, 1000).unwrap();
    assert_eq!(posix(&mut table, 1, None, 1000, 1000), Ok(true));
    // Another process can still lock.
    for index in 0..256 {
        posix(&mut table, 2, Some(Kind::Read), 2000 + index * 2, 2000 + index * 2).unwrap();
    }
    assert_eq!(table.len(), 511);
    // And the machine's cap holds for everyone.
    for index in 0..89 {
        posix(&mut table, 3, Some(Kind::Read), 5000 + index * 2, 5000 + index * 2).unwrap();
    }
    assert_eq!(table.len(), 600);
    assert_eq!(posix(&mut table, 4, Some(Kind::Read), 9000, 9000), Err(LockError::NoLocks));
    // An unlock that would split at the cap is refused too, changing nothing.
    let mut table = LockTable::new(1, 1);
    posix(&mut table, 1, Some(Kind::Write), 0, 9).unwrap();
    assert_eq!(posix(&mut table, 1, None, 4, 5), Err(LockError::NoLocks));
    assert_eq!(held(&table, 1), [(Kind::Write, 0, 9)]);
    assert_eq!(posix(&mut table, 1, None, 0, 5), Ok(true));
}

#[test]
fn range_of_whence_and_lengths() {
    assert_eq!(range_of(0, 0, 0), Ok(Range::WHOLE));
    assert_eq!(range_of(100, -10, 5), Ok(range(90, 94)));
    assert_eq!(range_of(100, 0, -10), Ok(range(90, 99)));
    assert_eq!(range_of(0, 5, -6), Err(RangeError::Invalid));
    assert_eq!(range_of(0, -1, 1), Err(RangeError::Invalid));
    assert_eq!(range_of(10, -10, -1), Err(RangeError::Invalid));
    // The last byte there is runs to the end, as all that follows.
    assert_eq!(range_of(0, i64::MAX, 1), Ok(range(MAX_OFFSET, Range::END)));
    assert_eq!(range_of(1, 0, i64::MAX), Ok(range(1, Range::END)));
    assert_eq!(range_of(0, 0, i64::MAX), Ok(range(0, MAX_OFFSET - 1)));
    assert_eq!(range(0, MAX_OFFSET - 1).length(), i64::MAX as u64);
    assert_eq!(range_of(0, i64::MAX, 2), Err(RangeError::Overflow));
    assert_eq!(range_of(1, i64::MAX, 0), Err(RangeError::Overflow));
    assert_eq!(range_of(u64::MAX, 0, 0), Err(RangeError::Overflow));
    assert_eq!(range_of(0, 0, i64::MIN), Err(RangeError::Invalid));
    assert_eq!(range(90, 94).length(), 5);
    assert_eq!(Range::WHOLE.length(), 0);
}

/// Ranges `range_of` makes, merged or split at the largest offset, still report a start
/// and length that fit a `struct flock`: `[1, i64::MAX]` and `[0, 0]` merge to the whole
/// file, not a length of 2^63, and unlocking up to the last byte leaves nothing past it.
#[test]
fn ranges_at_the_largest_offset_fit_a_flock() {
    let fits = |range: Range| i64::try_from(range.start).is_ok() && i64::try_from(range.length()).is_ok();
    let mut table = table();
    let pid = 1;
    let set = |table: &mut LockTable, kind, start, length| {
        table.set(FILE, Owner::Process(pid), pid, kind, range_of(0, start, length).unwrap())
    };
    set(&mut table, Some(Kind::Read), 1, i64::MAX).unwrap();
    set(&mut table, Some(Kind::Read), 0, 1).unwrap();
    assert_eq!(held(&table, pid), [(Kind::Read, 0, Range::END)]);
    let conflict = table.conflict(FILE, Owner::Process(2), Kind::Write, Range::WHOLE).unwrap();
    assert!(fits(conflict.range));
    assert_eq!(conflict.range.length(), 0);

    let mut table = self::table();
    set(&mut table, Some(Kind::Write), 0, 0).unwrap();
    set(&mut table, None, 1, i64::MAX).unwrap();
    assert_eq!(held(&table, pid), [(Kind::Write, 0, 0)]);
    assert!(table.locks().iter().all(|lock| fits(lock.range)));
    assert_eq!(posix(&mut table, 2, Some(Kind::Write), 1, Range::END), Ok(false));
}

/// Random record locks by three processes on 64 bytes, against a byte-by-byte model of
/// who holds what: every answer and every resulting table the same, and each process's
/// locks kept disjoint and merged.
#[test]
fn matches_byte_model() {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound
    };
    let mut table = table();
    let mut model = [[None::<Kind>; 64]; 3];
    for _ in 0..20_000 {
        let pid = next(3) as usize;
        let start = next(64);
        let end = (start + next(8)).min(63);
        let kind = match next(3) {
            0 => None,
            1 => Some(Kind::Read),
            _ => Some(Kind::Write),
        };
        let conflict = kind.is_some_and(|kind| {
            (0..3).filter(|&other| other != pid).any(|other| {
                (start..=end).any(|byte| match model[other][byte as usize] {
                    Some(Kind::Write) => true,
                    Some(Kind::Read) => kind == Kind::Write,
                    None => false,
                })
            })
        });
        let result = table.set(FILE, Owner::Process(pid as u32), pid as u32, kind, range(start, end));
        assert_eq!(result.is_err(), conflict, "{pid} {kind:?} {start}..={end}");
        if !conflict {
            for byte in start..=end {
                model[pid][byte as usize] = kind;
            }
        }
        for (pid, bytes) in model.iter().enumerate() {
            // The model's runs are what the table holds, exactly.
            let mut runs = Vec::new();
            let mut byte = 0;
            while byte < 64 {
                if let Some(kind) = bytes[byte] {
                    let first = byte;
                    while byte + 1 < 64 && bytes[byte + 1] == Some(kind) {
                        byte += 1;
                    }
                    runs.push((kind, first as u64, byte as u64));
                }
                byte += 1;
            }
            assert_eq!(held(&table, pid as u32), runs);
        }
    }
}
