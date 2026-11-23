//! Programs as processes of their own: what a shell needs of the system to start one
//! with its descriptors set up by actions, wait for it and signal it.
//!
//! The kernel's shell host runs a program as a task it feeds through streams, and
//! doesn't offer this. The user-mode `/bin/sh` (`user/src/bin/sh.rs`) does, over
//! `SPAWN2`, `EXEC`, `WAIT4`, `KILL`, `PIPE2` and `POLL`: its standard streams are its
//! own descriptors 0, 1 and 2, so a command whose output goes there needs nothing
//! copied, and a pipeline of programs is real pipes between real processes.

use alloc::string::String;

/// Linux's error numbers that a shell tells apart. Everything else is just a failure.
pub mod errno {
    /// No such file or directory.
    pub const ENOENT: i32 = 2;
    /// A call cut short by a signal.
    pub const EINTR: i32 = 4;
    /// Not a format the system can run.
    pub const ENOEXEC: i32 = 8;
    /// No such child.
    pub const ECHILD: i32 = 10;
    /// Not now: a non-blocking call that would wait.
    pub const EAGAIN: i32 = 11;
    /// Permission denied.
    pub const EACCES: i32 = 13;
    /// A pipe with no reader.
    pub const EPIPE: i32 = 32;
}

/// How an [`Action::Open`] opens its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// For reading.
    Read,
    /// For writing at its end, as a file the shell already created or truncated.
    Append,
    /// For reading and writing.
    ReadWrite,
}

/// One step of setting up a child's descriptors, in order, as `posix_spawn`'s file
/// actions are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// `dup2(from, to)`.
    Dup2 {
        /// Copied from.
        from: i32,
        /// Copied to.
        to: i32,
    },
    /// Opens `path` as descriptor `fd`.
    Open {
        /// Where it goes.
        fd: i32,
        /// An absolute path.
        path: String,
        /// How.
        mode: OpenMode,
    },
    /// Closes `fd`, if it's open.
    Close(i32),
    /// Closes every descriptor from this one up.
    CloseFrom(i32),
}

/// Which process group a child goes in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// The shell's.
    Inherit,
    /// One of its own, led by it: a job's first process.
    New,
    /// This one: a later process of the same job.
    Join(u32),
}

/// A program to start.
#[derive(Clone, Copy, Debug)]
pub struct Spawn<'a> {
    /// The program, an absolute path.
    pub path: &'a str,
    /// Its arguments, its name first.
    pub argv: &'a [String],
    /// Its environment, each `NAME=value`.
    pub env: &'a [String],
    /// Its working directory.
    pub cwd: &'a str,
    /// How its descriptors are made from the shell's.
    pub actions: &'a [Action],
    /// Its process group.
    pub group: Group,
    /// Its group becomes the terminal's foreground group, before it runs.
    pub foreground: bool,
}

/// What became of a child, as `WAIT4` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitStatus {
    /// It ended with this status.
    Exited(i32),
    /// A signal ended it.
    Signaled(u32),
    /// A signal stopped it.
    Stopped(u32),
    /// `SIGCONT` let it go on.
    Continued,
}

impl WaitStatus {
    /// From `WAIT4`'s status word: Linux's encoding (section 5.6 of
    /// `docs/feos-abi-v2.md`).
    pub const fn from_raw(raw: i32) -> Self {
        let low = raw & 0x7f;
        if raw == 0xffff {
            Self::Continued
        } else if raw & 0xff == 0x7f {
            Self::Stopped(((raw >> 8) & 0xff) as u32)
        } else if low == 0 {
            Self::Exited((raw >> 8) & 0xff)
        } else {
            Self::Signaled(low as u32)
        }
    }

    /// What `$?` says: the status, or 128 plus the signal that ended or stopped it.
    pub const fn shell_status(self) -> i32 {
        match self {
            Self::Exited(code) => code,
            Self::Signaled(signal) | Self::Stopped(signal) => 128 + signal as i32,
            Self::Continued => 0,
        }
    }

    /// Whether it has ended, one way or another.
    pub const fn is_final(self) -> bool {
        matches!(self, Self::Exited(_) | Self::Signaled(_))
    }
}

/// A descriptor a [`Processes::poll`] looks at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PollEntry {
    /// The descriptor.
    pub fd: i32,
    /// Waiting to read.
    pub read: bool,
    /// Waiting to write.
    pub write: bool,
    /// Set by the poll: it can be read or written without waiting, or it's at its end.
    pub ready: bool,
}

/// Signal numbers a shell sends, Linux's.
pub mod signal {
    /// Interrupt.
    pub const INT: u32 = 2;
    /// Killed.
    pub const KILL: u32 = 9;
    /// Terminate.
    pub const TERM: u32 = 15;
    /// Continue a stopped process.
    pub const CONT: u32 = 18;
}

/// What the system does for a shell that runs programs as processes. Errors are Linux's
/// error numbers, positive.
pub trait Processes {
    /// Starts a program. Its descriptors are the shell's, with `actions` applied.
    fn spawn(&mut self, request: &Spawn<'_>) -> Result<u32, i32>;

    /// Replaces the shell with a program: `actions` are applied to the shell's own
    /// descriptors first. Returns only with the reason it failed.
    fn exec(&mut self, request: &Spawn<'_>) -> i32;

    /// Waits for a child to end or stop (`block`), or looks (`!block`: `None` when
    /// nothing has happened). `EINTR` when a signal came first.
    fn wait(&mut self, pid: u32, block: bool) -> Result<Option<WaitStatus>, i32>;

    /// A pipe, read end first, both closed in programs the shell starts unless an
    /// action puts them somewhere.
    fn pipe(&mut self) -> Result<(i32, i32), i32>;

    /// Makes reads and writes of `fd` not wait.
    fn set_nonblocking(&mut self, fd: i32);

    /// Closes one of the shell's descriptors.
    fn close(&mut self, fd: i32);

    /// Reads from one of the shell's descriptors; zero at its end.
    fn read(&mut self, fd: i32, out: &mut [u8]) -> Result<usize, i32>;

    /// Writes to one of the shell's descriptors.
    fn write(&mut self, fd: i32, bytes: &[u8]) -> Result<usize, i32>;

    /// Waits until one of `entries` is ready, or `timeout_ms` passes (negative: no
    /// limit), marking those that are. Returns how many.
    fn poll(&mut self, entries: &mut [PollEntry], timeout_ms: i32) -> Result<usize, i32>;

    /// Sends a signal to a process, or with a negative `pid` to the group `-pid`.
    fn signal(&mut self, pid: i32, signal: u32) -> Result<(), i32>;

    /// Whether this shell does job control: it's interactive, on a terminal whose
    /// foreground group it can set.
    fn job_control(&self) -> bool {
        false
    }

    /// Makes `pgid` the terminal's foreground group.
    fn give_terminal(&mut self, _pgid: u32) {}

    /// Makes the shell's own group the terminal's foreground group again.
    fn take_terminal(&mut self) {}

    /// The highest descriptor above the shell's table (0 to 9) that the shell inherited
    /// open without close-on-exec, or `None`. Every program it starts gets those too, as
    /// POSIX has it (a jobserver's pipe handed through `sh -c`), so the descriptors past
    /// the table are closed in a child only above this one.
    fn inherited_above(&self) -> Option<i32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_status_of_signal() {
        // WAIT4's words: an exit is the status in bits 8 to 15, a signal its number in
        // the low seven bits (with 0x80 for a core), a stop 0x7f under the signal.
        assert_eq!(WaitStatus::from_raw(3 << 8), WaitStatus::Exited(3));
        assert_eq!(WaitStatus::from_raw(15), WaitStatus::Signaled(15));
        assert_eq!(WaitStatus::from_raw(0x80 | 11), WaitStatus::Signaled(11));
        assert_eq!(WaitStatus::from_raw(20 << 8 | 0x7f), WaitStatus::Stopped(20));
        assert_eq!(WaitStatus::from_raw(0xffff), WaitStatus::Continued);
        // `$?` is 128 plus the signal, as every shell has it.
        assert_eq!(WaitStatus::from_raw(15).shell_status(), 143);
        assert_eq!(WaitStatus::from_raw(2).shell_status(), 130);
        assert_eq!(WaitStatus::from_raw(0x80 | 11).shell_status(), 139);
        assert_eq!(WaitStatus::from_raw(1 << 8).shell_status(), 1);
        assert_eq!(WaitStatus::from_raw(20 << 8 | 0x7f).shell_status(), 148);
        assert!(WaitStatus::Signaled(9).is_final() && !WaitStatus::Stopped(19).is_final());
    }
}
