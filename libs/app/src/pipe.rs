//! Bounded byte pipes, from a fixed pool.
//!
//! Tasks are killed without unwinding, so nothing a killed task holds can rely
//! on `Drop`. Pipes therefore live in `.bss` and use no heap, and each pipe
//! records which PIDs hold its ends. When a task goes, the kernel calls
//! [`release_pid`] from its scheduling path, which closes what the task held:
//! its reader sees end of file and its writer's reader stops waiting.
//!
//! A handle's owner is the PID that created it, cloned it or last adopted it.
//! Moving a handle into a new task is finished by [`PipeReader::adopt`] or
//! [`PipeWriter::adopt`] on that task, or by the kernel for a task's stdio.
//!
//! Without installed hooks (host tests), holders are not tracked and handles
//! only close on `Drop`.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use feos_sync::{futex_wait, futex_wake, IrqSpinLock};

/// Pipes in the pool.
pub const PIPE_COUNT: usize = 32;

/// Bytes each pipe can hold before a writer has to wait.
pub const PIPE_BYTES: usize = 16 * 1024;

/// PIDs recorded per pipe: every task there can be, so that one's holding a pipe can
/// always be undone. Past this, a holder goes untracked, and only closes on drop.
const HOLDERS: usize = crate::MAX_PROCESSES;

/// Most bytes copied per lock hold, which bounds how long interrupts stay masked.
const COPY_CHUNK: usize = 2048;

/// Scheduler facts the pool needs, which it can't reach on its own.
#[derive(Clone, Copy)]
pub struct PipeHooks {
    /// PID of the calling task, or zero outside any task.
    pub current_pid: fn() -> u32,
    /// True when the calling task should stop waiting because it is being killed.
    pub interrupted: fn() -> bool,
}

static CURRENT_PID: AtomicUsize = AtomicUsize::new(0);
static INTERRUPTED: AtomicUsize = AtomicUsize::new(0);

/// Installs the scheduler facts. Call once during boot.
pub fn install_hooks(hooks: PipeHooks) {
    CURRENT_PID.store(hooks.current_pid as usize, Ordering::Release);
    INTERRUPTED.store(hooks.interrupted as usize, Ordering::Release);
}

fn current_pid() -> u32 {
    let address = CURRENT_PID.load(Ordering::Acquire);
    if address == 0 {
        return 0;
    }
    // SAFETY: only `install_hooks` stores this, from a valid function pointer.
    let hook: fn() -> u32 = unsafe { core::mem::transmute(address) };
    hook()
}

fn interrupted() -> bool {
    let address = INTERRUPTED.load(Ordering::Acquire);
    if address == 0 {
        return false;
    }
    // SAFETY: only `install_hooks` stores this, from a valid function pointer.
    let hook: fn() -> bool = unsafe { core::mem::transmute(address) };
    hook()
}

#[derive(Clone, Copy)]
struct Holder {
    pid: u32,
    readers: u8,
    writers: u8,
}

const NO_HOLDER: Holder = Holder {
    pid: 0,
    readers: 0,
    writers: 0,
};

struct Ring {
    bytes: [u8; PIPE_BYTES],
    head: usize,
    len: usize,
    readers: u16,
    writers: u16,
    /// Bumped each time the slot is reused, so a stale raw id can't touch it.
    generation: u16,
    holders: [Holder; HOLDERS],
}

impl Ring {
    fn hold(&mut self, pid: u32, end: End, delta: i8) {
        let tracked = pid != 0;
        let found = if tracked {
            self.holders
                .iter()
                .position(|holder| holder.pid == pid)
                .or_else(|| (delta > 0).then(|| self.holders.iter().position(|h| h.pid == 0)).flatten())
        } else {
            None
        };
        // A handle moved to a task that never adopted it: charge whoever has one.
        let found = found.or_else(|| {
            (delta < 0)
                .then(|| {
                    self.holders.iter().position(|holder| match end {
                        End::Read => holder.readers > 0,
                        End::Write => holder.writers > 0,
                    })
                })
                .flatten()
        });
        let Some(index) = found else {
            return;
        };
        let holder = &mut self.holders[index];
        if delta > 0 {
            holder.pid = pid;
        }
        match end {
            End::Read => holder.readers = holder.readers.saturating_add_signed(delta),
            End::Write => holder.writers = holder.writers.saturating_add_signed(delta),
        }
        if holder.readers == 0 && holder.writers == 0 {
            *holder = NO_HOLDER;
        }
    }

    fn is_free(&self) -> bool {
        self.readers == 0 && self.writers == 0
    }
}

struct Slot {
    ring: IrqSpinLock<Ring>,
    /// Changes whenever a blocked reader or writer should look again.
    wakes: AtomicU32,
}

static POOL: [Slot; PIPE_COUNT] = [const {
    Slot {
        ring: IrqSpinLock::new(Ring {
            bytes: [0; PIPE_BYTES],
            head: 0,
            len: 0,
            readers: 0,
            writers: 0,
            generation: 0,
            holders: [NO_HOLDER; HOLDERS],
        }),
        wakes: AtomicU32::new(0),
    }
}; PIPE_COUNT];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum End {
    Read,
    Write,
}

/// Names one pipe. Plain data, for the kernel's task table: nonzero when valid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PipeId(u32);

impl PipeId {
    /// The raw form, for storing in an atomic. Never zero.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Rebuilds an id from [`raw`](Self::raw). Zero means none.
    pub const fn from_raw(raw: u32) -> Option<Self> {
        if raw == 0 {
            None
        } else {
            Some(Self(raw))
        }
    }

    const fn new(index: usize, generation: u16) -> Self {
        Self(((generation as u32) << 16) | (index as u32 + 1))
    }

    fn index(self) -> usize {
        ((self.0 & 0xffff) as usize - 1).min(PIPE_COUNT - 1)
    }

    fn generation(self) -> u16 {
        (self.0 >> 16) as u16
    }

    fn slot(self) -> &'static Slot {
        &POOL[self.index()]
    }
}

fn notify(slot: &Slot) {
    slot.wakes.fetch_add(1, Ordering::Release);
    futex_wake(&slot.wakes, usize::MAX);
}

fn close(id: PipeId, end: End, pid: u32) {
    let slot = id.slot();
    let mut ring = slot.ring.lock();
    if ring.generation != id.generation() {
        return;
    }
    ring.hold(pid, end, -1);
    match end {
        End::Read => ring.readers = ring.readers.saturating_sub(1),
        End::Write => ring.writers = ring.writers.saturating_sub(1),
    }
    if ring.is_free() {
        ring.head = 0;
        ring.len = 0;
        ring.generation = ring.generation.wrapping_add(1);
        ring.holders = [NO_HOLDER; HOLDERS];
    }
    drop(ring);
    notify(slot);
}

/// What a non-blocking transfer found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transfer {
    /// This many bytes moved. Never zero.
    Moved(usize),
    /// Nothing can move yet.
    WouldBlock,
    /// Reading: every writer has gone and the pipe is empty. Writing: every
    /// reader has gone.
    Closed,
}

/// Reads what is available from a pipe by id, without blocking.
pub fn try_read_id(id: PipeId, out: &mut [u8]) -> Transfer {
    if out.is_empty() {
        return Transfer::WouldBlock;
    }
    let slot = id.slot();
    let mut ring = slot.ring.lock();
    if ring.generation != id.generation() {
        return Transfer::Closed;
    }
    if ring.len == 0 {
        return if ring.writers == 0 {
            Transfer::Closed
        } else {
            Transfer::WouldBlock
        };
    }
    let count = ring.len.min(out.len()).min(COPY_CHUNK);
    let first = count.min(PIPE_BYTES - ring.head);
    let head = ring.head;
    out[..first].copy_from_slice(&ring.bytes[head..head + first]);
    out[first..count].copy_from_slice(&ring.bytes[..count - first]);
    ring.head = (ring.head + count) % PIPE_BYTES;
    ring.len -= count;
    drop(ring);
    notify(slot);
    Transfer::Moved(count)
}

/// Writes what fits into a pipe by id, without blocking.
pub fn try_write_id(id: PipeId, bytes: &[u8]) -> Transfer {
    if bytes.is_empty() {
        return Transfer::Moved(0);
    }
    let slot = id.slot();
    let mut ring = slot.ring.lock();
    if ring.generation != id.generation() || ring.readers == 0 {
        return Transfer::Closed;
    }
    let room = PIPE_BYTES - ring.len;
    if room == 0 {
        return Transfer::WouldBlock;
    }
    let count = room.min(bytes.len()).min(COPY_CHUNK);
    let tail = (ring.head + ring.len) % PIPE_BYTES;
    let first = count.min(PIPE_BYTES - tail);
    ring.bytes[tail..tail + first].copy_from_slice(&bytes[..first]);
    ring.bytes[..count - first].copy_from_slice(&bytes[first..count]);
    ring.len += count;
    drop(ring);
    notify(slot);
    Transfer::Moved(count)
}

/// Writes everything to a pipe by id, blocking while it is full. Returns
/// `false` when the readers have gone or the caller is being killed.
pub fn write_id(id: PipeId, mut bytes: &[u8]) -> bool {
    let slot = id.slot();
    while !bytes.is_empty() {
        let seen = slot.wakes.load(Ordering::Acquire);
        match try_write_id(id, bytes) {
            Transfer::Moved(count) => bytes = &bytes[count..],
            Transfer::Closed => return false,
            Transfer::WouldBlock => {
                if interrupted() {
                    return false;
                }
                futex_wait(&slot.wakes, seen);
            }
        }
    }
    true
}

/// Reads at least one byte from a pipe by id, blocking while it is empty.
/// Returns zero at end of file, or when the caller is being killed.
pub fn read_id(id: PipeId, out: &mut [u8]) -> usize {
    if out.is_empty() {
        return 0;
    }
    let slot = id.slot();
    loop {
        let seen = slot.wakes.load(Ordering::Acquire);
        match try_read_id(id, out) {
            Transfer::Moved(count) => return count,
            Transfer::Closed => return 0,
            Transfer::WouldBlock => {
                if interrupted() {
                    return 0;
                }
                futex_wait(&slot.wakes, seen);
            }
        }
    }
}

/// Adds a reference to a pipe's write end on behalf of `pid`. The kernel uses
/// this to give a new task its stdio.
pub fn attach_writer(id: PipeId, pid: u32) -> bool {
    let mut ring = id.slot().ring.lock();
    if ring.generation != id.generation() || ring.writers == 0 {
        return false;
    }
    ring.writers = ring.writers.saturating_add(1);
    ring.hold(pid, End::Write, 1);
    true
}

/// Adds a reference to a pipe's read end on behalf of `pid`, as [`attach_writer`] does.
pub fn attach_reader(id: PipeId, pid: u32) -> bool {
    let mut ring = id.slot().ring.lock();
    if ring.generation != id.generation() || ring.readers == 0 {
        return false;
    }
    ring.readers = ring.readers.saturating_add(1);
    ring.hold(pid, End::Read, 1);
    true
}

/// Closes `pid`'s reference to a pipe's read end, for a handle held as a raw id.
pub fn close_reader_id(id: PipeId, pid: u32) {
    close(id, End::Read, pid);
}

/// Closes `pid`'s reference to a pipe's write end, for a handle held as a raw id.
pub fn close_writer_id(id: PipeId, pid: u32) {
    close(id, End::Write, pid);
}

/// Closes everything `pid` holds. The kernel calls this when a task has gone.
pub fn release_pid(pid: u32) {
    if pid == 0 {
        return;
    }
    for (index, slot) in POOL.iter().enumerate() {
        let (generation, readers, writers) = {
            let ring = slot.ring.lock();
            match ring.holders.iter().find(|holder| holder.pid == pid) {
                Some(holder) => (ring.generation, holder.readers, holder.writers),
                None => continue,
            }
        };
        let id = PipeId::new(index, generation);
        for _ in 0..readers {
            close(id, End::Read, pid);
        }
        for _ in 0..writers {
            close(id, End::Write, pid);
        }
    }
}

/// Pipes currently in use.
pub fn in_use() -> usize {
    POOL.iter().filter(|slot| !slot.ring.lock().is_free()).count()
}

/// Creates a pipe. `None` when the pool is exhausted.
pub fn create() -> Option<(PipeReader, PipeWriter)> {
    let pid = current_pid();
    for (index, slot) in POOL.iter().enumerate() {
        let mut ring = slot.ring.lock();
        if !ring.is_free() {
            continue;
        }
        ring.generation = ring.generation.wrapping_add(1);
        ring.head = 0;
        ring.len = 0;
        ring.readers = 1;
        ring.writers = 1;
        ring.holders = [NO_HOLDER; HOLDERS];
        ring.hold(pid, End::Read, 1);
        ring.hold(pid, End::Write, 1);
        let id = PipeId::new(index, ring.generation);
        return Some((PipeReader { id }, PipeWriter { id }));
    }
    None
}

fn adopt(id: PipeId, end: End) {
    let pid = current_pid();
    if pid == 0 {
        return;
    }
    let mut ring = id.slot().ring.lock();
    if ring.generation != id.generation() {
        return;
    }
    // The previous owner's record goes with the handle: `hold` charges whoever
    // has this end when the current PID has no record yet.
    let has = ring.holders.iter().any(|holder| {
        holder.pid == pid
            && match end {
                End::Read => holder.readers > 0,
                End::Write => holder.writers > 0,
            }
    });
    if !has {
        ring.hold(0, end, -1);
        ring.hold(pid, end, 1);
    }
}

/// The read end of a pipe. Dropping it closes the end.
#[derive(Debug)]
pub struct PipeReader {
    id: PipeId,
}

impl PipeReader {
    /// This end's id.
    pub const fn id(&self) -> PipeId {
        self.id
    }

    /// Reads at least one byte, blocking while the pipe is empty. Returns zero
    /// at end of file.
    pub fn read(&self, out: &mut [u8]) -> usize {
        read_id(self.id, out)
    }

    /// Reads what is available, without blocking.
    pub fn try_read(&self, out: &mut [u8]) -> Transfer {
        try_read_id(self.id, out)
    }

    /// Records the calling task as this handle's owner, after a move.
    pub fn adopt(&self) {
        adopt(self.id, End::Read);
    }

    /// Whether bytes are waiting: [`Transfer::Moved`] with how many, or what
    /// [`try_read`](Self::try_read) would otherwise have said.
    pub fn peek(&self) -> Transfer {
        let ring = self.id.slot().ring.lock();
        if ring.generation != self.id.generation() {
            Transfer::Closed
        } else if ring.len > 0 {
            Transfer::Moved(ring.len)
        } else if ring.writers == 0 {
            Transfer::Closed
        } else {
            Transfer::WouldBlock
        }
    }

    /// A count that moves whenever this pipe changes. Read it before looking at
    /// the pipe, and pass it to [`wait_for_change`](Self::wait_for_change).
    pub fn change_count(&self) -> u32 {
        self.id.slot().wakes.load(Ordering::Acquire)
    }

    /// Blocks while the pipe hasn't changed since `seen`. A spurious return is
    /// always possible.
    pub fn wait_for_change(&self, seen: u32) {
        futex_wait(&self.id.slot().wakes, seen);
    }

    /// Wakes tasks blocked on this pipe, so they can look at something else.
    pub fn wake(&self) {
        notify(self.id.slot());
    }

    /// Another handle on this end. `None` if the pipe has gone.
    pub fn try_clone(&self) -> Option<Self> {
        let mut ring = self.id.slot().ring.lock();
        if ring.generation != self.id.generation() {
            return None;
        }
        ring.readers = ring.readers.saturating_add(1);
        ring.hold(current_pid(), End::Read, 1);
        Some(Self { id: self.id })
    }
}

impl Drop for PipeReader {
    fn drop(&mut self) {
        close(self.id, End::Read, current_pid());
    }
}

/// The write end of a pipe. Dropping it closes the end.
#[derive(Debug)]
pub struct PipeWriter {
    id: PipeId,
}

impl PipeWriter {
    /// This end's id.
    pub const fn id(&self) -> PipeId {
        self.id
    }

    /// Writes everything, blocking while the pipe is full. Returns `false` when
    /// the readers have gone.
    pub fn write(&self, bytes: &[u8]) -> bool {
        write_id(self.id, bytes)
    }

    /// Writes what fits, without blocking.
    pub fn try_write(&self, bytes: &[u8]) -> Transfer {
        try_write_id(self.id, bytes)
    }

    /// True when nobody can ever read what this end writes.
    pub fn is_broken(&self) -> bool {
        let ring = self.id.slot().ring.lock();
        ring.generation != self.id.generation() || ring.readers == 0
    }

    /// Records the calling task as this handle's owner, after a move.
    pub fn adopt(&self) {
        adopt(self.id, End::Write);
    }

    /// Another handle on this end. `None` if the pipe has gone.
    pub fn try_clone(&self) -> Option<Self> {
        let mut ring = self.id.slot().ring.lock();
        if ring.generation != self.id.generation() {
            return None;
        }
        ring.writers = ring.writers.saturating_add(1);
        ring.hold(current_pid(), End::Write, 1);
        Some(Self { id: self.id })
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        close(self.id, End::Write, current_pid());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_task_there_can_be_can_hold_a_pipe_and_let_go() {
        let (reader, writer) = create().unwrap();
        let first = 1_000_000;
        for pid in first..first + crate::MAX_PROCESSES as u32 - 1 {
            assert!(attach_writer(writer.id(), pid));
        }
        for pid in first..first + crate::MAX_PROCESSES as u32 - 1 {
            release_pid(pid);
        }
        assert_eq!(reader.try_read(&mut [0u8; 4]), Transfer::WouldBlock);
        drop(writer);
        assert_eq!(reader.try_read(&mut [0u8; 4]), Transfer::Closed);
    }

    use super::*;

    #[test]
    fn bytes_cross_a_pipe_in_order_and_wrap_around() {
        let (reader, writer) = create().unwrap();
        let mut out = [0u8; 64];
        let mut next = 0u8;
        let mut expect = 0u8;
        for _ in 0..2000 {
            let chunk: [u8; 37] = core::array::from_fn(|_| {
                next = next.wrapping_add(1);
                next
            });
            assert_eq!(writer.try_write(&chunk), Transfer::Moved(37));
            let mut got = 0;
            while got < 37 {
                match reader.try_read(&mut out[got..37]) {
                    Transfer::Moved(count) => got += count,
                    other => panic!("{other:?}"),
                }
            }
            for byte in &out[..37] {
                expect = expect.wrapping_add(1);
                assert_eq!(*byte, expect);
            }
        }
    }

    #[test]
    fn a_full_pipe_would_block_and_a_closed_reader_breaks_it() {
        let (reader, writer) = create().unwrap();
        let data = [7u8; COPY_CHUNK];
        let mut total = 0;
        loop {
            match writer.try_write(&data) {
                Transfer::Moved(count) => total += count,
                Transfer::WouldBlock => break,
                Transfer::Closed => panic!("closed"),
            }
        }
        assert_eq!(total, PIPE_BYTES);
        assert!(!writer.is_broken());
        drop(reader);
        assert!(writer.is_broken());
        assert_eq!(writer.try_write(&data), Transfer::Closed);
        assert!(!writer.write(&data));
    }

    #[test]
    fn end_of_file_arrives_after_the_buffered_bytes() {
        let (reader, writer) = create().unwrap();
        assert!(writer.write(b"abc"));
        let second = writer.try_clone().unwrap();
        drop(writer);
        let mut out = [0u8; 8];
        assert_eq!(reader.read(&mut out), 3);
        assert_eq!(reader.try_read(&mut out), Transfer::WouldBlock);
        drop(second);
        assert_eq!(reader.read(&mut out), 0);
        assert_eq!(reader.try_read(&mut out), Transfer::Closed);
    }

    #[test]
    fn a_stale_id_cannot_touch_the_slot_after_reuse() {
        // Hold many at once so this test's slot can't be taken by another test
        // between the close and the check.
        let (reader, writer) = create().unwrap();
        let stale = writer.id();
        drop(writer);
        drop(reader);
        assert_eq!(try_write_id(stale, b"x"), Transfer::Closed);
    }

    #[test]
    fn a_blocked_reader_wakes_across_threads() {
        let (reader, writer) = create().unwrap();
        let thread = std::thread::spawn(move || {
            let mut total = 0usize;
            let mut buffer = [0u8; 1024];
            loop {
                let read = reader.read(&mut buffer);
                if read == 0 {
                    return total;
                }
                total += read;
            }
        });
        let data = [1u8; 4096];
        for _ in 0..64 {
            assert!(writer.write(&data));
        }
        drop(writer);
        assert_eq!(thread.join().unwrap(), 64 * 4096);
    }
}
