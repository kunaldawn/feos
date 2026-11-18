//! Bounded byte pipes, from a pool that grows on demand.
//!
//! Tasks are killed without unwinding, so nothing a killed task holds can rely
//! on `Drop`. A pipe is therefore owned by the pool, not by its handles: the pool's
//! slots are in `.bss`, each pipe's buffer and holder list come from the heap when it's
//! made and go back when its last end closes, and each pipe records which PIDs hold its
//! ends. When a task goes, the kernel calls [`release_pid`] from its teardown, which
//! closes what the task held: its reader sees end of file and its writer's reader stops
//! waiting.
//!
//! A pipe's memory is taken and given back with the slot's lock held, which masks
//! interrupts, so a kernel app killed at a tick can't leave a buffer allocated and not
//! yet the slot's, or the other way round.
//!
//! What a user program can make is capped: [`PIPES_PER_PROCESS`] alive at once of the
//! ones its process created, wherever their ends have gone since, and none of the last
//! [`KEPT_FOR_KERNEL`] of the pool, so neither one program nor several can stop the
//! shell from making a pipeline. Kernel apps, which are trusted, have the whole pool.
//!
//! A handle's owner is the PID that created it, cloned it or last adopted it.
//! Moving a handle into a new task is finished by [`PipeReader::adopt`] or
//! [`PipeWriter::adopt`] on that task, or by the kernel for a task's stdio.
//!
//! A user program's pipes are different: the kernel's open file descriptions own their
//! ends, and close them as the last descriptor referring to one goes (the kernel drops
//! them in task context, with interrupts on, whichever way the process ended). So those
//! ends are counted, as any other, but no holder is recorded for them:
//! [`create_owned_for_program`] makes them, and [`close_owned`] closes them. A program's
//! standard streams are still held by PID, by the task they were given to.
//!
//! Without installed hooks (host tests), holders are not tracked and handles
//! only close on `Drop`.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use feos_sync::{futex_wait, futex_wake, IrqSpinLock};

/// Pipes the pool can have at once: slots, each a few words until it's used.
pub const MAX_PIPES: usize = 1024;

/// Pipes a user program's process may have alive at once, of the ones it created.
pub const PIPES_PER_PROCESS: usize = 64;

/// Pipes a user program can't take, by making them: room for the kernel's own apps,
/// the shell's pipelines among them.
pub const KEPT_FOR_KERNEL: usize = 64;

/// Bytes each pipe can hold before a writer has to wait.
pub const PIPE_BYTES: usize = 16 * 1024;

/// Most bytes [`write_some`] writes in one piece, never interleaved with another
/// writer's: POSIX's `PIPE_BUF`.
pub const PIPE_BUF: usize = 4096;

const _: () = assert!(PIPE_BUF <= PIPE_BYTES);

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
    readers: u16,
    writers: u16,
}

struct Ring {
    /// `PIPE_BYTES`, from the heap while the pipe is open; empty while the slot is free.
    bytes: Vec<u8>,
    head: usize,
    len: usize,
    readers: u16,
    writers: u16,
    /// Bumped each time the slot is reused, so a stale raw id can't touch it.
    generation: u16,
    /// Every PID holding an end, as many as there are: grown from the heap, so that a
    /// pipe given to a thousand tasks can still be undone for each of them.
    holders: Vec<Holder>,
}

impl Ring {
    /// Makes room to record one more holder, so that [`hold`](Self::hold) with a positive
    /// `delta` can't fail. `false` when the heap has none.
    fn room_for_holder(&mut self, pid: u32) -> bool {
        #[cfg(test)]
        if tests::NO_HEAP.with(|none| none.get()) && pid != 0 && !self.holders.iter().any(|holder| holder.pid == pid) {
            return false;
        }
        pid == 0 || self.holders.iter().any(|holder| holder.pid == pid) || self.holders.try_reserve(1).is_ok()
    }

    /// Records `pid`'s holding one end more or less. A positive `delta` for a PID not
    /// yet recorded needs [`room_for_holder`](Self::room_for_holder) first, or it goes
    /// untracked.
    fn hold(&mut self, pid: u32, end: End, delta: i16) {
        let tracked = pid != 0;
        let mut found = if tracked { self.holders.iter().position(|holder| holder.pid == pid) } else { None };
        if found.is_none() && tracked && delta > 0 && self.holders.len() < self.holders.capacity() {
            self.holders.push(Holder { pid, readers: 0, writers: 0 });
            found = Some(self.holders.len() - 1);
        }
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
        match end {
            End::Read => holder.readers = holder.readers.saturating_add_signed(delta),
            End::Write => holder.writers = holder.writers.saturating_add_signed(delta),
        }
        if holder.readers == 0 && holder.writers == 0 {
            self.holders.swap_remove(index);
        }
    }

    fn is_free(&self) -> bool {
        self.readers == 0 && self.writers == 0
    }

    /// Gives the pipe's memory back, as its last end closes.
    fn empty(&mut self) {
        self.head = 0;
        self.len = 0;
        self.bytes = Vec::new();
        self.holders = Vec::new();
    }
}

struct Slot {
    ring: IrqSpinLock<Ring>,
    /// Whose the pipe is, for [`PIPES_PER_PROCESS`]: the PID that created it, zero while
    /// the slot is free or for a pipe made outside any task. Read without the lock.
    creator: AtomicU32,
    /// The slot holds a pipe. Read without the lock, to skip free slots.
    used: AtomicBool,
    /// A reader found the pipe empty and nothing has reached it since. Lets a feeder
    /// tell a program that wants input from one that isn't asking, so that keys typed
    /// ahead of a program that never reads them stay for whoever does.
    wanted: AtomicBool,
    /// Changes whenever a blocked reader or writer should look again.
    wakes: AtomicU32,
}

static POOL: [Slot; MAX_PIPES] = [const {
    Slot {
        ring: IrqSpinLock::new(Ring {
            bytes: Vec::new(),
            head: 0,
            len: 0,
            readers: 0,
            writers: 0,
            generation: 0,
            holders: Vec::new(),
        }),
        creator: AtomicU32::new(0),
        used: AtomicBool::new(false),
        wanted: AtomicBool::new(false),
        wakes: AtomicU32::new(0),
    }
}; MAX_PIPES];

/// Held while a pipe is made, so that counting a process's pipes and taking a slot are
/// one step: two threads of a process can't both pass the cap with one pipe to go.
static CREATING: IrqSpinLock<()> = IrqSpinLock::new(());

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
        ((self.0 & 0xffff) as usize).saturating_sub(1).min(MAX_PIPES - 1)
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
    close_end(id, end, Some(pid));
}

/// Closes one end, and takes it off `holder`'s record if there is one to take it off.
fn close_end(id: PipeId, end: End, holder: Option<u32>) {
    let slot = id.slot();
    let mut ring = slot.ring.lock();
    if ring.generation != id.generation() {
        return;
    }
    if let Some(pid) = holder {
        ring.hold(pid, end, -1);
    }
    match end {
        End::Read => ring.readers = ring.readers.saturating_sub(1),
        End::Write => ring.writers = ring.writers.saturating_sub(1),
    }
    if ring.is_free() {
        ring.generation = ring.generation.wrapping_add(1);
        ring.empty();
        slot.creator.store(0, Ordering::Release);
        slot.used.store(false, Ordering::Release);
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
            slot.wanted.store(true, Ordering::Release);
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
    slot.wanted.store(false, Ordering::Release);
    drop(ring);
    notify(slot);
    Transfer::Moved(count)
}

/// A wait in a pipe ended because the calling task is being killed. What was asked
/// for may have partly happened, which nobody will look at: the task is going.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Killed;

/// Writes everything to a pipe by id, blocking while it is full. `Ok(false)` when
/// the readers have gone, and [`Killed`] when the caller is being killed: a user
/// program's call has to tell that apart from a broken pipe, so it can unwind.
pub fn write_wait(id: PipeId, mut bytes: &[u8]) -> Result<bool, Killed> {
    let slot = id.slot();
    while !bytes.is_empty() {
        let seen = slot.wakes.load(Ordering::Acquire);
        match try_write_id(id, bytes) {
            Transfer::Moved(count) => bytes = &bytes[count..],
            Transfer::Closed => return Ok(false),
            Transfer::WouldBlock => {
                if interrupted() {
                    return Err(Killed);
                }
                futex_wait(&slot.wakes, seen);
            }
        }
    }
    Ok(true)
}

/// Reads at least one byte from a pipe by id, blocking while it is empty. `Ok(0)`
/// at end of file, and [`Killed`] when the caller is being killed.
pub fn read_wait(id: PipeId, out: &mut [u8]) -> Result<usize, Killed> {
    if out.is_empty() {
        return Ok(0);
    }
    let slot = id.slot();
    loop {
        let seen = slot.wakes.load(Ordering::Acquire);
        match try_read_id(id, out) {
            Transfer::Moved(count) => return Ok(count),
            Transfer::Closed => return Ok(0),
            Transfer::WouldBlock => {
                if interrupted() {
                    return Err(Killed);
                }
                futex_wait(&slot.wakes, seen);
            }
        }
    }
}

/// Writes everything to a pipe by id, blocking while it is full. Returns
/// `false` when the readers have gone or the caller is being killed.
pub fn write_id(id: PipeId, bytes: &[u8]) -> bool {
    write_wait(id, bytes).unwrap_or(false)
}

/// Reads at least one byte from a pipe by id, blocking while it is empty.
/// Returns zero at end of file, or when the caller is being killed.
pub fn read_id(id: PipeId, out: &mut [u8]) -> usize {
    read_wait(id, out).unwrap_or(0)
}

/// Why an end of a pipe couldn't be added for a task.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttachError {
    /// The pipe has gone, or the end has.
    Gone,
    /// No heap to record the task as a holder with.
    NoMemory,
}

/// Adds a reference to one end of a pipe on behalf of `pid`.
fn attach(id: PipeId, end: End, pid: u32) -> Result<(), AttachError> {
    let mut ring = id.slot().ring.lock();
    let open = match end {
        End::Read => ring.readers,
        End::Write => ring.writers,
    };
    if ring.generation != id.generation() || open == 0 {
        return Err(AttachError::Gone);
    }
    if !ring.room_for_holder(pid) {
        return Err(AttachError::NoMemory);
    }
    match end {
        End::Read => ring.readers = ring.readers.saturating_add(1),
        End::Write => ring.writers = ring.writers.saturating_add(1),
    }
    ring.hold(pid, end, 1);
    Ok(())
}

/// Adds a reference to a pipe's write end on behalf of `pid`. The kernel uses
/// this to give a new task its stdio.
pub fn attach_writer(id: PipeId, pid: u32) -> Result<(), AttachError> {
    attach(id, End::Write, pid)
}

/// Adds a reference to a pipe's read end on behalf of `pid`, as [`attach_writer`] does.
pub fn attach_reader(id: PipeId, pid: u32) -> Result<(), AttachError> {
    attach(id, End::Read, pid)
}

/// A new task `pid`'s stream from `id`, a pipe given to it or inherited: the raw id of
/// the end (read end when `read`) it now holds, or zero, the console, when there's no
/// pipe or it has gone, as there always was. No heap to record the holder is an error,
/// which the spawn fails on: quietly giving the task the console instead would send
/// its output, or take its input, where nobody asked, and leave the pipe's other end
/// at end of file.
pub fn attach_stream(id: Option<PipeId>, read: bool, pid: u32) -> Result<u32, AttachError> {
    let Some(id) = id else {
        return Ok(0);
    };
    let attached = if read { attach_reader(id, pid) } else { attach_writer(id, pid) };
    match attached {
        Ok(()) => Ok(id.raw()),
        Err(AttachError::Gone) => Ok(0),
        Err(AttachError::NoMemory) => Err(AttachError::NoMemory),
    }
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
        if !slot.used.load(Ordering::Acquire) {
            continue;
        }
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
    POOL.iter().filter(|slot| slot.used.load(Ordering::Acquire)).count()
}

/// Pipes alive that `pid` created.
pub fn created_by(pid: u32) -> usize {
    if pid == 0 {
        return 0;
    }
    POOL.iter()
        .filter(|slot| slot.used.load(Ordering::Acquire) && slot.creator.load(Ordering::Acquire) == pid)
        .count()
}

/// Why a pipe couldn't be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CreateError {
    /// The calling process has [`PIPES_PER_PROCESS`] alive already.
    ProcessCap,
    /// The pool is full, or as full as a user program may make it.
    PoolFull,
    /// No heap for the buffer.
    NoMemory,
}

/// Whether a pipe may be made with `used` of the pool taken, `mine` of them by the
/// caller's process (`tracked` when it has one), by a user program (`program`) or a
/// kernel app.
fn admit(used: usize, mine: usize, tracked: bool, program: bool) -> Result<(), CreateError> {
    if program && tracked && mine >= PIPES_PER_PROCESS {
        return Err(CreateError::ProcessCap);
    }
    let limit = if program { MAX_PIPES - KEPT_FOR_KERNEL } else { MAX_PIPES };
    if used >= limit {
        return Err(CreateError::PoolFull);
    }
    Ok(())
}

/// Makes a pipe for the calling task, its buffer from the heap, and its ends held in the
/// task's name if `tracked`.
fn make(program: bool, tracked: bool) -> Result<PipeId, CreateError> {
    let pid = current_pid();
    let holder = if tracked { pid } else { 0 };
    let _creating = CREATING.lock();
    let (mut used, mut mine) = (0, 0);
    for slot in &POOL {
        if slot.used.load(Ordering::Acquire) {
            used += 1;
            mine += usize::from(pid != 0 && slot.creator.load(Ordering::Acquire) == pid);
        }
    }
    admit(used, mine, pid != 0, program)?;
    // A slot only becomes used under `CREATING`, so one that's free now stays free until
    // it's this one's.
    for (index, slot) in POOL.iter().enumerate() {
        if slot.used.load(Ordering::Acquire) {
            continue;
        }
        let mut ring = slot.ring.lock();
        if !ring.is_free() {
            continue;
        }
        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(PIPE_BYTES).is_err() || !ring.room_for_holder(holder) {
            ring.empty();
            return Err(CreateError::NoMemory);
        }
        bytes.resize(PIPE_BYTES, 0);
        ring.bytes = bytes;
        ring.generation = ring.generation.wrapping_add(1);
        ring.head = 0;
        ring.len = 0;
        ring.readers = 1;
        ring.writers = 1;
        // Holding nothing in nobody's name (zero) records nothing.
        ring.hold(holder, End::Read, 1);
        ring.hold(holder, End::Write, 1);
        // The creator is counted against the cap however the ends are held.
        slot.creator.store(pid, Ordering::Release);
        slot.used.store(true, Ordering::Release);
        slot.wanted.store(false, Ordering::Release);
        return Ok(PipeId::new(index, ring.generation));
    }
    Err(CreateError::PoolFull)
}

/// Creates a pipe for a kernel app, which may use the whole pool. `None` when it's
/// full or the heap is short.
pub fn create() -> Option<(PipeReader, PipeWriter)> {
    make(false, true).ok().map(|id| (PipeReader { id }, PipeWriter { id }))
}

/// Creates a pipe for a user program's call, held to [`PIPES_PER_PROCESS`] and leaving
/// [`KEPT_FOR_KERNEL`] of the pool alone, and says why if it can't.
pub fn create_for_program() -> Result<(PipeReader, PipeWriter), CreateError> {
    make(true, true).map(|id| (PipeReader { id }, PipeWriter { id }))
}

/// Creates a pipe for a user program's call as [`create_for_program`] does, with its two
/// ends owned by the caller rather than held in the task's name: an open file
/// description each, which [`close_owned`] closes. One id names both ends.
pub fn create_owned_for_program() -> Result<PipeId, CreateError> {
    make(true, false)
}

/// Closes an end made by [`create_owned_for_program`]: the read end if `read`. No holder
/// is touched, so a task that also holds the pipe in its name (a program's standard
/// stream) keeps what it holds.
pub fn close_owned(id: PipeId, read: bool) {
    close_end(id, if read { End::Read } else { End::Write }, None);
}

/// Writes all of `bytes` in one piece, or nothing: [`Transfer::WouldBlock`] while there
/// isn't room for all of it. At most [`PIPE_BUF`] bytes, so the copy under the lock
/// stays short.
fn try_write_whole(id: PipeId, bytes: &[u8]) -> Transfer {
    if bytes.is_empty() {
        return Transfer::Moved(0);
    }
    let slot = id.slot();
    let mut ring = slot.ring.lock();
    if ring.generation != id.generation() || ring.readers == 0 {
        return Transfer::Closed;
    }
    let count = bytes.len().min(PIPE_BUF);
    if PIPE_BYTES - ring.len < count {
        return Transfer::WouldBlock;
    }
    let tail = (ring.head + ring.len) % PIPE_BYTES;
    let first = count.min(PIPE_BYTES - tail);
    ring.bytes[tail..tail + first].copy_from_slice(&bytes[..first]);
    ring.bytes[..count - first].copy_from_slice(&bytes[first..count]);
    ring.len += count;
    slot.wanted.store(false, Ordering::Release);
    drop(ring);
    notify(slot);
    Transfer::Moved(count)
}

/// Writes to a pipe by id, as a program's `write` does.
///
/// With `atomic`, `bytes` (at most [`PIPE_BUF`]) goes in one piece, never interleaved
/// with another writer's: a blocking write waits for room for all of it, and a
/// non-blocking one is [`Transfer::WouldBlock`] until there is. Without, a blocking write
/// waits until it has all gone in, and a non-blocking one writes what fits now.
///
/// [`Transfer::Moved`] says how much went: everything, unless the readers went part way
/// or a non-blocking write filled the pipe. [`Transfer::Closed`] is a broken pipe with
/// nothing written, and [`Killed`] a caller being killed, whatever was written.
pub fn write_some(id: PipeId, bytes: &[u8], atomic: bool, nonblocking: bool) -> Result<Transfer, Killed> {
    if bytes.is_empty() {
        return Ok(Transfer::Moved(0));
    }
    let slot = id.slot();
    let mut done = 0;
    while done < bytes.len() {
        let seen = slot.wakes.load(Ordering::Acquire);
        let result = if atomic { try_write_whole(id, &bytes[done..]) } else { try_write_id(id, &bytes[done..]) };
        match result {
            Transfer::Moved(count) => done += count,
            Transfer::Closed if done == 0 => return Ok(Transfer::Closed),
            Transfer::Closed => break,
            Transfer::WouldBlock if nonblocking => break,
            Transfer::WouldBlock => {
                if interrupted() {
                    return Err(Killed);
                }
                futex_wait(&slot.wakes, seen);
            }
        }
    }
    Ok(if done == 0 { Transfer::WouldBlock } else { Transfer::Moved(done) })
}

/// Reads from a pipe by id, as a program's `read` does: what's there, at most `out`'s
/// length, as soon as there's anything. A blocking read waits for it; a non-blocking one
/// is [`Transfer::WouldBlock`] while the pipe is empty with a writer left.
/// [`Transfer::Closed`] is end of file.
pub fn read_some(id: PipeId, out: &mut [u8], nonblocking: bool) -> Result<Transfer, Killed> {
    if out.is_empty() {
        return Ok(Transfer::Moved(0));
    }
    let slot = id.slot();
    let mut done = 0;
    loop {
        let seen = slot.wakes.load(Ordering::Acquire);
        match try_read_id(id, &mut out[done..]) {
            // What's there now, all of it that fits, but no waiting for more.
            Transfer::Moved(count) => {
                done += count;
                if done == out.len() {
                    return Ok(Transfer::Moved(done));
                }
            }
            _ if done > 0 => return Ok(Transfer::Moved(done)),
            Transfer::Closed => return Ok(Transfer::Closed),
            Transfer::WouldBlock if nonblocking => return Ok(Transfer::WouldBlock),
            Transfer::WouldBlock => {
                if interrupted() {
                    return Err(Killed);
                }
                futex_wait(&slot.wakes, seen);
            }
        }
    }
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
    // has this end when the current PID has no record yet. With no heap to record the
    // new owner, the old record stays, and is released when the old owner goes.
    let has = ring.holders.iter().any(|holder| {
        holder.pid == pid
            && match end {
                End::Read => holder.readers > 0,
                End::Write => holder.writers > 0,
            }
    });
    if !has && ring.room_for_holder(pid) {
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

    /// Another handle on this end. `None` if the pipe has gone, or there is no heap to
    /// record the caller as holding it with.
    pub fn try_clone(&self) -> Option<Self> {
        attach(self.id, End::Read, current_pid()).ok().map(|()| Self { id: self.id })
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

    /// Whether a reader has come up empty since anything was last written: that is,
    /// it's waiting for input, or polling for some.
    pub fn reader_wants(&self) -> bool {
        self.id.slot().wanted.load(Ordering::Acquire)
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

    /// Another handle on this end. `None` if the pipe has gone, or there is no heap to
    /// record the caller as holding it with.
    pub fn try_clone(&self) -> Option<Self> {
        attach(self.id, End::Write, current_pid()).ok().map(|()| Self { id: self.id })
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        close(self.id, End::Write, current_pid());
    }
}

#[cfg(test)]
mod tests {
    std::thread_local! {
        /// The PID the calling test thread runs as: zero, untracked, unless it says.
        static PID: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
        static KILLED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
        /// Whether recording a new holder finds the heap full, on this test thread.
        pub(super) static NO_HEAP: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    }

    /// Hooks that answer per thread, so each test is its own task and the others
    /// running alongside see none of it.
    fn per_thread_hooks() {
        install_hooks(PipeHooks {
            current_pid: || PID.with(|pid| pid.get()),
            interrupted: || KILLED.with(|killed| killed.get()),
        });
    }

    #[test]
    fn holders_beyond_32() {
        // Far more holders than the 32 a pool slot once had, and than there are tasks:
        // each is recorded, so each can be undone, and only the last one closes it.
        let (reader, writer) = create().unwrap();
        let first = 1_000_000;
        let count = 2 * crate::MAX_PROCESSES as u32;
        for pid in first..first + count {
            assert!(attach_writer(writer.id(), pid).is_ok());
        }
        assert_eq!(writer.id().slot().ring.lock().holders.len(), count as usize);
        for pid in first..first + count {
            release_pid(pid);
        }
        assert!(writer.id().slot().ring.lock().holders.is_empty());
        assert_eq!(reader.try_read(&mut [0u8; 4]), Transfer::WouldBlock);
        drop(writer);
        assert_eq!(reader.try_read(&mut [0u8; 4]), Transfer::Closed);
    }

    #[test]
    fn pipe_pool_grows() {
        // Well past the 32 pipes the pool used to be: four processes each make as many
        // as one may, and the next of each is refused, with the others unaffected.
        per_thread_hooks();
        let first = 2_000_000;
        let mut pipes = Vec::new();
        for pid in first..first + 4 {
            PID.with(|current| current.set(pid));
            for _ in 0..PIPES_PER_PROCESS {
                pipes.push(create_for_program().unwrap());
            }
            assert_eq!(create_for_program().err(), Some(CreateError::ProcessCap));
            assert_eq!(created_by(pid), PIPES_PER_PROCESS);
            // A kernel app, trusted, isn't held to it.
            assert!(create().is_some());
        }
        assert!(in_use() >= 4 * PIPES_PER_PROCESS);
        for (reader, writer) in &pipes {
            assert_eq!(writer.id().slot().ring.lock().bytes.len(), PIPE_BYTES);
            assert_eq!(writer.try_write(b"x"), Transfer::Moved(1));
            assert_eq!(reader.try_read(&mut [0u8; 4]), Transfer::Moved(1));
        }
        // Their memory goes with their last ends, and so does the count against the cap.
        let ids: Vec<PipeId> = pipes.iter().map(|(_, writer)| writer.id()).collect();
        PID.with(|current| current.set(first));
        drop(pipes);
        for id in ids {
            let ring = id.slot().ring.lock();
            if ring.generation == id.generation().wrapping_add(1) {
                assert_eq!(ring.bytes.capacity(), 0);
                assert_eq!(ring.holders.capacity(), 0);
            }
        }
        for pid in first..first + 4 {
            assert_eq!(created_by(pid), 0);
        }
        assert!(create_for_program().is_ok());
        PID.with(|current| current.set(0));
    }

    #[test]
    fn a_pipe_given_away_still_counts_for_its_creator() {
        per_thread_hooks();
        let (creator, other) = (3_000_000, 3_000_001);
        PID.with(|current| current.set(creator));
        let (reader, writer) = create_for_program().unwrap();
        assert!(attach_reader(reader.id(), other).is_ok());
        assert!(attach_writer(writer.id(), other).is_ok());
        // Held as raw ids from here, as the kernel holds a program's.
        let id = reader.id();
        core::mem::forget(reader);
        core::mem::forget(writer);
        release_pid(creator);
        assert_eq!(created_by(creator), 1, "the other holder keeps it open");
        release_pid(other);
        assert_eq!(created_by(creator), 0);
        assert_eq!(try_write_id(id, b"x"), Transfer::Closed);
        PID.with(|current| current.set(0));
    }

    #[test]
    fn programs_leave_the_kernel_its_pipes() {
        assert_eq!(admit(0, 0, true, true), Ok(()));
        assert_eq!(admit(0, PIPES_PER_PROCESS - 1, true, true), Ok(()));
        assert_eq!(admit(0, PIPES_PER_PROCESS, true, true), Err(CreateError::ProcessCap));
        assert_eq!(admit(0, PIPES_PER_PROCESS, true, false), Ok(()), "a kernel app has no cap");
        assert_eq!(admit(0, PIPES_PER_PROCESS, false, true), Ok(()), "outside a task is untracked");
        let kept = MAX_PIPES - KEPT_FOR_KERNEL;
        assert_eq!(admit(kept - 1, 0, true, true), Ok(()));
        assert_eq!(admit(kept, 0, true, true), Err(CreateError::PoolFull));
        assert_eq!(admit(kept, 0, true, false), Ok(()));
        assert_eq!(admit(MAX_PIPES - 1, 0, true, false), Ok(()));
        assert_eq!(admit(MAX_PIPES, 0, true, false), Err(CreateError::PoolFull));
    }

    #[test]
    fn a_stream_with_no_heap_for_its_holder_fails_rather_than_falling_back() {
        let (reader, writer) = create().unwrap();
        let (child, other) = (4_000_000, 4_000_001);
        assert_eq!(attach_stream(None, false, child), Ok(0), "no pipe is the console");
        NO_HEAP.with(|none| none.set(true));
        assert_eq!(attach_writer(writer.id(), child), Err(AttachError::NoMemory));
        assert_eq!(attach_stream(Some(writer.id()), false, child), Err(AttachError::NoMemory));
        assert_eq!(attach_stream(Some(reader.id()), true, child), Err(AttachError::NoMemory));
        NO_HEAP.with(|none| none.set(false));
        {
            // Nothing was counted for the failures.
            let ring = writer.id().slot().ring.lock();
            assert_eq!((ring.readers, ring.writers), (1, 1));
            assert!(ring.holders.iter().all(|holder| holder.pid != child));
        }
        assert_eq!(attach_stream(Some(writer.id()), false, other), Ok(writer.id().raw()));
        release_pid(other);
        // A pipe whose end has gone is still the console, as it always was.
        let id = reader.id();
        drop(reader);
        assert_eq!(attach_stream(Some(id), true, child), Ok(0));
        assert_eq!(attach_reader(id, child), Err(AttachError::Gone));
        drop(writer);
        assert_eq!(attach_stream(Some(id), false, child), Ok(0));
    }

    use super::*;

    #[test]
    fn pipe_buf_writes_atomic() {
        // Writers racing with blocks of PIPE_BUF bytes, each all one byte, into a pipe a
        // reader drains in odd-sized pieces: every block comes out whole.
        let id = create_owned_for_program().unwrap();
        const WRITERS: usize = 4;
        const BLOCKS: usize = 64;
        let writers: Vec<_> = (0..WRITERS)
            .map(|writer| {
                std::thread::spawn(move || {
                    let block = [b'a' + writer as u8; PIPE_BUF];
                    for _ in 0..BLOCKS {
                        assert_eq!(write_some(id, &block, true, false), Ok(Transfer::Moved(PIPE_BUF)));
                    }
                })
            })
            .collect();
        let mut current = Vec::new();
        let mut blocks = [0usize; WRITERS];
        let mut out = [0u8; 1000];
        while blocks.iter().sum::<usize>() < WRITERS * BLOCKS {
            let Ok(Transfer::Moved(count)) = read_some(id, &mut out, false) else {
                panic!("the pipe ended early");
            };
            for &byte in &out[..count] {
                if let Some(&first) = current.first() {
                    assert_eq!(byte, first, "a block was interleaved with another");
                }
                current.push(byte);
                if current.len() == PIPE_BUF {
                    blocks[(current[0] - b'a') as usize] += 1;
                    current.clear();
                }
            }
        }
        for writer in writers {
            writer.join().unwrap();
        }
        assert_eq!(blocks, [BLOCKS; WRITERS]);
        // A whole block that doesn't fit doesn't go in part.
        let block = [0u8; PIPE_BUF];
        let mut fill = 0;
        while let Ok(Transfer::Moved(count)) = write_some(id, &block[..1000], true, true) {
            fill += count;
        }
        assert_eq!(fill, 16 * 1000);
        assert_eq!(write_some(id, &block, true, true), Ok(Transfer::WouldBlock));
        assert_eq!(write_some(id, &block, false, true), Ok(Transfer::Moved(PIPE_BYTES - fill)));
        close_owned(id, true);
        assert_eq!(write_some(id, &block, true, false), Ok(Transfer::Closed));
        close_owned(id, false);
        assert_eq!(try_write_id(id, b"x"), Transfer::Closed, "the last end closed it");
    }

    #[test]
    fn nonblocking_read_eagain() {
        let id = create_owned_for_program().unwrap();
        let mut out = [0u8; 8];
        // Empty with a writer: would block, and says so rather than waiting.
        assert_eq!(read_some(id, &mut out, true), Ok(Transfer::WouldBlock));
        assert_eq!(write_some(id, b"abc", true, true), Ok(Transfer::Moved(3)));
        assert_eq!(read_some(id, &mut out, true), Ok(Transfer::Moved(3)));
        assert_eq!(&out[..3], b"abc");
        assert_eq!(read_some(id, &mut out, true), Ok(Transfer::WouldBlock));
        // Once the writer's gone, it's end of file, blocking or not.
        close_owned(id, false);
        assert_eq!(read_some(id, &mut out, true), Ok(Transfer::Closed));
        assert_eq!(read_some(id, &mut out, false), Ok(Transfer::Closed));
        close_owned(id, true);
    }

    #[test]
    fn large_write_completes() {
        // A megabyte in one blocking write, read in small pieces by another thread: the
        // write waits for room as often as it takes, and returns only when all is in.
        let id = create_owned_for_program().unwrap();
        const TOTAL: usize = 1024 * 1024;
        let reader = std::thread::spawn(move || {
            let mut out = [0u8; 100];
            let mut next = 0usize;
            loop {
                match read_some(id, &mut out, false) {
                    Ok(Transfer::Moved(count)) => {
                        for &byte in &out[..count] {
                            assert_eq!(byte, (next % 251) as u8);
                            next += 1;
                        }
                    }
                    Ok(Transfer::Closed) => return next,
                    other => panic!("{other:?}"),
                }
            }
        });
        let data: Vec<u8> = (0..TOTAL).map(|index| (index % 251) as u8).collect();
        assert_eq!(write_some(id, &data, false, false), Ok(Transfer::Moved(TOTAL)));
        close_owned(id, false);
        assert_eq!(reader.join().unwrap(), TOTAL);
        // With the reader gone, a write is a broken pipe.
        let id2 = create_owned_for_program().unwrap();
        close_owned(id2, true);
        assert_eq!(write_some(id2, &data, false, false), Ok(Transfer::Closed));
        close_owned(id2, false);
        close_owned(id, true);
    }

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
    fn a_writer_can_see_that_its_reader_wants_input() {
        let (reader, writer) = create().unwrap();
        assert!(!writer.reader_wants());
        let mut out = [0u8; 4];
        assert_eq!(reader.try_read(&mut out), Transfer::WouldBlock);
        assert!(writer.reader_wants());
        assert_eq!(writer.try_write(b"x"), Transfer::Moved(1));
        assert!(!writer.reader_wants());
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

    #[test]
    fn pipe_wait_returns_killed() {
        // Only this thread is "being killed": the hook answers per thread, so the
        // other tests running alongside don't see it. No PID, so nothing's tracked.
        per_thread_hooks();
        let (reader, writer) = create().unwrap();
        let mut out = [0u8; 8];

        // An empty pipe with a writer: a read would block, so a killed caller is told.
        KILLED.with(|killed| killed.set(true));
        assert_eq!(read_wait(reader.id(), &mut out), Err(Killed));
        assert_eq!(read_id(reader.id(), &mut out), 0);
        // What's there is still read: the kill only ends a wait.
        assert!(writer.try_write(b"ab") == Transfer::Moved(2));
        assert_eq!(read_wait(reader.id(), &mut out), Ok(2));

        // A full pipe: the writer would block.
        let data = [3u8; COPY_CHUNK];
        while writer.try_write(&data) != Transfer::WouldBlock {}
        assert_eq!(write_wait(writer.id(), b"x"), Err(Killed));
        assert!(!writer.write(b"x"));

        // Not being killed, a broken pipe and end of file are what they always were.
        KILLED.with(|killed| killed.set(false));
        drop(reader);
        assert_eq!(write_wait(writer.id(), b"x"), Ok(false));
        let (reader, writer) = create().unwrap();
        drop(writer);
        assert_eq!(read_wait(reader.id(), &mut out), Ok(0));
    }
}
