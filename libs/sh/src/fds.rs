//! The shell's file-descriptor table.
//!
//! Descriptors 0 to 9 name open files, and several descriptors can share one,
//! which is what makes `2>&1 > f` and `exec 3> f` behave. Commands see the table
//! through [`FdInput`] and [`FdOutput`] adapters, which keeps the commands
//! themselves unaware of it, and the filesystem stays borrowable by the command
//! because a file's writes are buffered here and flushed when the command ends.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use feos_coreutils::io::MAX_STREAM_BYTES;
use feos_coreutils::{FsError, Input, Output, System};

use crate::{CancelToken, Host, Io};

/// Number of descriptors the shell tracks.
pub(crate) const MAX_FDS: usize = 10;

/// An entry in the open-file table.
pub(crate) enum OpenFile {
    /// One of the streams the embedding application passed in.
    Base(u8),
    /// Reads as empty and discards writes.
    Null,
    /// Reads as endless zeros.
    Zero,
    /// A file, here-document or pipe being read.
    Reader { data: Vec<u8>, position: usize },
    /// A file being written, with bytes waiting for the next flush.
    Writer {
        path: String,
        pending: Vec<u8>,
        overflow: bool,
    },
    /// Output kept in memory for a pipe or command substitution.
    Capture { data: Vec<u8>, overflow: bool },
    /// A live stream from another task.
    Stream(Box<dyn Input + Send>),
}

struct Entry {
    file: OpenFile,
    refs: u16,
}

/// The descriptor numbers in use, saved so a redirection can be undone.
pub(crate) type SavedFds = [Option<u16>; MAX_FDS];

/// Bytes on their way to a file once the command that wrote them has ended.
pub(crate) type Leftovers = Vec<(String, Vec<u8>)>;

/// Open files and the descriptors that refer to them.
pub(crate) struct FdTable {
    slots: SavedFds,
    files: Vec<Option<Entry>>,
}

impl FdTable {
    pub(crate) fn new() -> Self {
        let mut table = Self {
            slots: [None; MAX_FDS],
            files: Vec::new(),
        };
        for fd in 0..3u8 {
            let index = table.open(OpenFile::Base(fd));
            table.slots[usize::from(fd)] = Some(index);
        }
        table
    }

    /// Adds an open file with one reference, not yet bound to a descriptor.
    pub(crate) fn open(&mut self, file: OpenFile) -> u16 {
        let entry = Entry { file, refs: 1 };
        if let Some(index) = self.files.iter().position(Option::is_none) {
            self.files[index] = Some(entry);
            return index as u16;
        }
        self.files.push(Some(entry));
        (self.files.len() - 1) as u16
    }

    fn add_ref(&mut self, index: u16) {
        if let Some(Some(entry)) = self.files.get_mut(usize::from(index)) {
            entry.refs += 1;
        }
    }

    /// Drops one reference. The last writer's pending bytes are returned so the
    /// caller can write them out.
    fn drop_ref(&mut self, index: u16) -> Option<(String, Vec<u8>)> {
        let slot = self.files.get_mut(usize::from(index))?;
        let entry = slot.as_mut()?;
        entry.refs -= 1;
        if entry.refs > 0 {
            return None;
        }
        match slot.take()?.file {
            OpenFile::Writer { path, pending, .. } if !pending.is_empty() => Some((path, pending)),
            _ => None,
        }
    }

    /// Takes a copy of the descriptor numbers, holding their files open.
    pub(crate) fn save(&mut self) -> SavedFds {
        let saved = self.slots;
        for index in saved.iter().flatten() {
            self.add_ref(*index);
        }
        saved
    }

    /// Puts saved descriptors back, closing what replaced them.
    pub(crate) fn restore(&mut self, saved: SavedFds, leftovers: &mut Leftovers) {
        let current = core::mem::replace(&mut self.slots, saved);
        for index in current.iter().flatten() {
            leftovers.extend(self.drop_ref(*index));
        }
    }

    /// Forgets saved descriptors, keeping the current ones (`exec > f`).
    pub(crate) fn discard(&mut self, saved: SavedFds, leftovers: &mut Leftovers) {
        for index in saved.iter().flatten() {
            leftovers.extend(self.drop_ref(*index));
        }
    }

    /// Binds `fd` to an already-opened file, consuming the caller's reference.
    pub(crate) fn bind(&mut self, fd: usize, index: u16, leftovers: &mut Leftovers) {
        if let Some(old) = self.slots.get_mut(fd).and_then(|slot| slot.replace(index)) {
            leftovers.extend(self.drop_ref(old));
        }
    }

    /// Makes `fd` share `target`'s file. Fails when `target` is closed.
    pub(crate) fn dup(&mut self, fd: usize, target: usize, leftovers: &mut Leftovers) -> bool {
        let Some(index) = self.slots.get(target).copied().flatten() else {
            return false;
        };
        self.add_ref(index);
        self.bind(fd, index, leftovers);
        true
    }

    /// Closes `fd`.
    pub(crate) fn close(&mut self, fd: usize, leftovers: &mut Leftovers) {
        if let Some(old) = self.slots.get_mut(fd).and_then(Option::take) {
            leftovers.extend(self.drop_ref(old));
        }
    }

    /// Holds a capture open past its descriptor, so its data survives a close.
    pub(crate) fn hold(&mut self, index: u16) {
        self.add_ref(index);
    }

    /// Takes the data out of a capture and drops the caller's hold on it. The
    /// second value is true when bytes were lost to the stream budget.
    pub(crate) fn take_capture(&mut self, index: u16) -> (Vec<u8>, bool) {
        let result = match self.files.get_mut(usize::from(index)) {
            Some(Some(Entry {
                file: OpenFile::Capture { data, overflow },
                ..
            })) => (core::mem::take(data), core::mem::take(overflow)),
            _ => (Vec::new(), false),
        };
        let _ = self.drop_ref(index);
        result
    }

    /// Takes the pending bytes of every writer, for flushing to the filesystem.
    pub(crate) fn take_pending(&mut self, out: &mut Leftovers) {
        for entry in self.files.iter_mut().flatten() {
            if let OpenFile::Writer { path, pending, .. } = &mut entry.file {
                if !pending.is_empty() {
                    out.push((path.clone(), core::mem::take(pending)));
                }
            }
        }
    }

    /// Reports a file that lost output to the stream budget, once.
    pub(crate) fn take_overflowed_writer(&mut self) -> Option<String> {
        for entry in self.files.iter_mut().flatten() {
            if let OpenFile::Writer { path, overflow, .. } = &mut entry.file {
                if core::mem::take(overflow) {
                    return Some(path.clone());
                }
            }
        }
        None
    }

    /// True when a descriptor above 2 is open, which a forked shell can't inherit.
    pub(crate) fn has_extra_descriptors(&self) -> bool {
        self.slots[3..].iter().any(Option::is_some)
    }

    pub(crate) fn resolve(&mut self, fd: usize) -> Option<&mut OpenFile> {
        let index = self.slots.get(fd).copied().flatten()?;
        self.files
            .get_mut(usize::from(index))?
            .as_mut()
            .map(|entry| &mut entry.file)
    }
}

/// How often the streams look for cancellation, in calls. Polling is cheap, but
/// a command writing one byte at a time should not pay for it on every byte.
const POLL_INTERVAL: u32 = 64;

/// Everything a running command's streams can reach.
pub(crate) struct StreamView<'x, 'y> {
    pub(crate) fds: &'x mut FdTable,
    pub(crate) io: &'x mut Io<'y>,
    pub(crate) cancel: &'x CancelToken,
    pub(crate) host: Option<&'x mut dyn Host>,
    pub(crate) system: &'x dyn System,
    pub(crate) deadline: Option<u64>,
    pub(crate) timed_out: &'x mut bool,
    pub(crate) calls: u32,
}

impl StreamView<'_, '_> {
    fn poll(&mut self) -> bool {
        self.calls = self.calls.wrapping_add(1);
        if self.calls % POLL_INTERVAL == 0 {
            if let Some(host) = self.host.as_deref_mut() {
                if host.poll_cancel() {
                    self.cancel.cancel();
                }
            }
            if let Some(deadline) = self.deadline {
                if self.system.uptime_ms() >= deadline {
                    *self.timed_out = true;
                    self.cancel.cancel();
                }
            }
        }
        self.cancel.is_cancelled()
    }

    pub(crate) fn read(&mut self, fd: usize, out: &mut [u8]) -> usize {
        if self.poll() {
            return 0;
        }
        let Self { fds, io, .. } = self;
        match fds.resolve(fd) {
            Some(OpenFile::Base(0)) => io.stdin.read(out),
            Some(OpenFile::Reader { data, position }) => {
                let rest = data.get(*position..).unwrap_or(&[]);
                let count = rest.len().min(out.len());
                out[..count].copy_from_slice(&rest[..count]);
                *position += count;
                count
            }
            Some(OpenFile::Zero) => {
                out.fill(0);
                out.len()
            }
            Some(OpenFile::Stream(reader)) => reader.read(out),
            _ => 0,
        }
    }

    /// What's already here, without waiting.
    pub(crate) fn try_read(&mut self, fd: usize, out: &mut [u8]) -> feos_coreutils::Available {
        use feos_coreutils::Available;
        if self.poll() {
            return Available::End;
        }
        let Self { fds, io, .. } = self;
        match fds.resolve(fd) {
            Some(OpenFile::Base(0)) => io.stdin.try_read(out),
            Some(OpenFile::Stream(reader)) => reader.try_read(out),
            _ => match self.read(fd, out) {
                0 => Available::End,
                count => Available::Bytes(count),
            },
        }
    }

    /// A terminal's keys, as they are. Anything else reads as it always does.
    pub(crate) fn read_raw(&mut self, fd: usize, out: &mut [u8]) -> usize {
        let Self { fds, io, .. } = self;
        match fds.resolve(fd) {
            Some(OpenFile::Base(0)) => io.stdin.read_raw(out),
            _ => self.read(fd, out),
        }
    }

    /// Keys that have already arrived, without waiting for more.
    pub(crate) fn read_pending(&mut self, fd: usize, out: &mut [u8]) -> usize {
        let Self { fds, io, .. } = self;
        match fds.resolve(fd) {
            Some(OpenFile::Base(0)) => io.stdin.read_pending(out),
            _ => self.read(fd, out),
        }
    }

    /// Raw mode is the terminal's, whatever this descriptor reads from.
    pub(crate) fn set_raw(&mut self, raw: bool) {
        self.io.stdin.set_raw(raw);
    }

    pub(crate) fn read_all(&mut self, fd: usize, limit: usize) -> Result<Vec<u8>, FsError> {
        let Self { fds, io, .. } = self;
        match fds.resolve(fd) {
            Some(OpenFile::Reader { data, position }) => {
                // Move the remainder out: a pipeline stage never needs a copy.
                let mut all = core::mem::take(data);
                if *position > 0 {
                    all.drain(..(*position).min(all.len()));
                    *position = 0;
                }
                if all.len() > limit {
                    return Err(FsError::NoSpace);
                }
                Ok(all)
            }
            Some(OpenFile::Base(0)) => io.stdin.try_read_to_end(limit),
            Some(OpenFile::Stream(reader)) => reader.try_read_to_end(limit),
            Some(OpenFile::Zero) => Err(FsError::NoSpace),
            _ => Ok(Vec::new()),
        }
    }

    pub(crate) fn write(&mut self, fd: usize, bytes: &[u8]) {
        if self.poll() {
            return;
        }
        let Self { fds, io, .. } = self;
        match fds.resolve(fd) {
            Some(OpenFile::Base(1)) => io.stdout.write(bytes),
            Some(OpenFile::Base(2)) => io.stderr.write(bytes),
            Some(OpenFile::Writer {
                pending, overflow, ..
            })
            | Some(OpenFile::Capture {
                data: pending,
                overflow,
            }) => {
                if *overflow
                    || pending.len().saturating_add(bytes.len()) > MAX_STREAM_BYTES
                    || pending.try_reserve(bytes.len()).is_err()
                {
                    *overflow = true;
                    return;
                }
                pending.extend_from_slice(bytes);
            }
            _ => {}
        }
    }

    pub(crate) fn is_broken(&mut self, fd: usize) -> bool {
        // A command asking this is at a safe stopping point, so always look.
        self.calls |= POLL_INTERVAL - 1;
        if self.poll() {
            return true;
        }
        let Self { fds, io, .. } = self;
        match fds.resolve(fd) {
            Some(OpenFile::Base(1)) => io.stdout.is_broken(),
            Some(OpenFile::Base(2)) => io.stderr.is_broken(),
            Some(OpenFile::Writer { overflow, .. } | OpenFile::Capture { overflow, .. }) => {
                *overflow
            }
            _ => false,
        }
    }

    pub(crate) fn is_terminal(&mut self, fd: usize) -> bool {
        let Self { fds, io, .. } = self;
        match fds.resolve(fd) {
            Some(OpenFile::Base(0)) => io.stdin.is_terminal(),
            Some(OpenFile::Base(1)) => io.stdout.is_terminal(),
            Some(OpenFile::Base(2)) => io.stderr.is_terminal(),
            _ => false,
        }
    }
}

/// A command's view of an input descriptor.
pub(crate) struct FdInput<'v, 'x, 'y> {
    pub(crate) view: &'v RefCell<StreamView<'x, 'y>>,
    pub(crate) fd: usize,
}

impl Input for FdInput<'_, '_, '_> {
    fn read(&mut self, out: &mut [u8]) -> usize {
        self.view.borrow_mut().read(self.fd, out)
    }

    fn try_read(&mut self, out: &mut [u8]) -> feos_coreutils::Available {
        self.view.borrow_mut().try_read(self.fd, out)
    }

    fn is_terminal(&self) -> bool {
        self.view.borrow_mut().is_terminal(self.fd)
    }

    fn read_raw(&mut self, out: &mut [u8]) -> usize {
        self.view.borrow_mut().read_raw(self.fd, out)
    }

    fn read_pending(&mut self, out: &mut [u8]) -> usize {
        self.view.borrow_mut().read_pending(self.fd, out)
    }

    fn set_raw(&mut self, raw: bool) {
        self.view.borrow_mut().set_raw(raw);
    }

    fn read_tty(&mut self, out: &mut [u8]) -> usize {
        self.view.borrow_mut().io.stdin.read_raw(out)
    }

    fn read_tty_pending(&mut self, out: &mut [u8]) -> usize {
        self.view.borrow_mut().io.stdin.read_pending(out)
    }

    fn tty_is_terminal(&self) -> bool {
        self.view.borrow_mut().io.stdin.is_terminal()
    }

    fn try_read_to_end(&mut self, limit: usize) -> Result<Vec<u8>, FsError> {
        self.view.borrow_mut().read_all(self.fd, limit)
    }
}

/// A command's view of an output descriptor.
pub(crate) struct FdOutput<'v, 'x, 'y> {
    pub(crate) view: &'v RefCell<StreamView<'x, 'y>>,
    pub(crate) fd: usize,
}

impl Output for FdOutput<'_, '_, '_> {
    fn write(&mut self, bytes: &[u8]) {
        self.view.borrow_mut().write(self.fd, bytes);
    }

    fn is_terminal(&self) -> bool {
        self.view.borrow_mut().is_terminal(self.fd)
    }

    fn is_broken(&self) -> bool {
        self.view.borrow_mut().is_broken(self.fd)
    }
}
