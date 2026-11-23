//! Terminals (stage 1c.11 of the ABI design): what a program's terminal-marked
//! descriptors refer to, the line discipline its reads go through, its settings, size and
//! foreground group, and the calls on it.
//!
//! A terminal is a kernel app's: the shell host opens one for each program it runs in the
//! foreground on a console it reads keys from ([`open`]), names it in the program's
//! spawn, feeds it what's typed ([`input`]) and closes its own hold once the program has
//! ended ([`close`]). The program's standard streams that are on the console are marked
//! with it (see [`files`](super::files)'s `Object::Terminal`): a read of one goes through
//! the terminal's line discipline (`feos-tty`), a write through its output processing to
//! the pipe the stream always had, and its `IOCTL`s are answered here. Descriptors are
//! inherited, so a program's children share its terminal, and `/dev/tty` opens the one a
//! caller's marked descriptors refer to.
//!
//! Typed keys reach the discipline only when somebody wants them: while a reader waits
//! (or a poller looks), or found nothing since the last offer; otherwise only up to a
//! character that raises a signal. So keys typed ahead of a program that never reads them
//! stay the shell's, for whoever reads next, while Ctrl-C still reaches the foreground
//! group at once, through `ISIG`.
//!
//! Terminals live in a static table, each counted: the host's hold and every description
//! marked with one. The last to let go clears it. Nothing here allocates.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use feos_abi::{errno, ioctl, sig, tc, Termios, WinSize};
use feos_app::pipe::{self, PipeId, Transfer};
use feos_sync::SpinLock;
use feos_tty::{Background, Tty, Wait};

use super::process::Process;
use super::KILLED;
use crate::{sched, time};

/// Terminals that can be open at once: one per foreground program on a console, which a
/// handful of Terminal windows and the serial shell have.
pub const MAX_TERMINALS: usize = 16;

/// What the terminal's lock covers.
struct Inner {
    tty: Tty,
    size: WinSize,
    /// Where echo, and writes to `/dev/tty`, go: an end of the pipe the program's output
    /// is shown from, held by the terminal itself.
    echo: Option<PipeId>,
}

pub struct Terminal {
    /// The slot is taken: set as it's opened, cleared once its last hold has gone.
    used: AtomicBool,
    /// Holds: the host's, and each description marked with it.
    holds: AtomicU32,
    /// Bumped as the slot is opened, so a stale id from a host names nothing.
    generation: AtomicU32,
    /// The host hasn't closed it: there's somebody to type.
    host: AtomicBool,
    state: SpinLock<Option<Inner>>,
    /// The foreground process group, 0 until a program is started on it.
    foreground: AtomicU32,
    session: AtomicU32,
    /// Moves when input arrives, settings change, output resumes or the host goes: what
    /// a reader, a writer held by `TCOOFF` and a drain wait on.
    wakes: AtomicU32,
    /// A reader came up empty and nothing has been given it since.
    wanted: AtomicBool,
    /// Output suspended by `TCOOFF`.
    stopped: AtomicBool,
}

static TERMINALS: [Terminal; MAX_TERMINALS] = [const {
    Terminal {
        used: AtomicBool::new(false),
        holds: AtomicU32::new(0),
        generation: AtomicU32::new(0),
        host: AtomicBool::new(false),
        state: SpinLock::new(None),
        foreground: AtomicU32::new(0),
        session: AtomicU32::new(0),
        wakes: AtomicU32::new(0),
        wanted: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
    }
}; MAX_TERMINALS];

/// A hold on a terminal, as a marked description keeps one.
pub struct TerminalRef {
    index: usize,
}

impl TerminalRef {
    fn get(&self) -> &'static Terminal {
        &TERMINALS[self.index]
    }

    /// The word its foreground group is in, for `SPAWN2`'s `TCSETPGRP` to set as the
    /// child is made.
    pub fn foreground_word(&self) -> &'static AtomicU32 {
        &self.get().foreground
    }

    pub fn session(&self) -> u32 {
        self.get().session.load(Ordering::Acquire)
    }

    /// Whether both are the same terminal.
    pub fn same(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl Clone for TerminalRef {
    fn clone(&self) -> Self {
        self.get().holds.fetch_add(1, Ordering::AcqRel);
        Self { index: self.index }
    }
}

impl Drop for TerminalRef {
    fn drop(&mut self) {
        release(self.index);
    }
}

fn release(index: usize) {
    let terminal = &TERMINALS[index];
    if terminal.holds.fetch_sub(1, Ordering::AcqRel) != 1 {
        return;
    }
    // The last hold: nothing can reach it now but a host's stale id, which the
    // generation turns away.
    let gone = terminal.state.lock().take();
    if let Some(echo) = gone.and_then(|inner| inner.echo) {
        pipe::close_owned(echo, false);
    }
    terminal.foreground.store(0, Ordering::Release);
    terminal.session.store(0, Ordering::Release);
    terminal.stopped.store(false, Ordering::Release);
    terminal.wanted.store(false, Ordering::Release);
    terminal.used.store(false, Ordering::Release);
}

/// A host's id for the terminal in `index`.
fn host_id(index: usize, generation: u32) -> u32 {
    (generation & 0x00ff_ffff) << 8 | (index as u32 + 1)
}

/// The terminal a host's id names, if it's still the host's.
fn by_host(id: u32) -> Option<(usize, &'static Terminal)> {
    let index = ((id & 0xff) as usize).checked_sub(1).filter(|&index| index < MAX_TERMINALS)?;
    let terminal = &TERMINALS[index];
    let current = terminal.generation.load(Ordering::Acquire) & 0x00ff_ffff;
    (terminal.used.load(Ordering::Acquire) && terminal.host.load(Ordering::Acquire) && id >> 8 == current).then_some((index, terminal))
}

/// Opens a terminal for a kernel app: its echo going to the pipe `echo`, which it
/// holds a write end of from now on, its size `rows` by `columns`.
pub fn open(echo: PipeId, rows: u16, columns: u16) -> Option<u32> {
    let index = TERMINALS.iter().position(|terminal| terminal.used.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok())?;
    let terminal = &TERMINALS[index];
    if pipe::attach_writer(echo, 0).is_err() {
        terminal.used.store(false, Ordering::Release);
        return None;
    }
    *terminal.state.lock() = Some(Inner { tty: Tty::new(), size: WinSize { rows, columns, x_pixels: 0, y_pixels: 0 }, echo: Some(echo) });
    let generation = terminal.generation.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    terminal.foreground.store(0, Ordering::Release);
    terminal.session.store(0, Ordering::Release);
    terminal.holds.store(1, Ordering::Release);
    terminal.host.store(true, Ordering::Release);
    Some(host_id(index, generation))
}

/// The host lets go of its terminal: nobody types on it any more. Readers left, a
/// program's children, see end of file once what was typed is read.
pub fn close(id: u32) {
    let Some((index, terminal)) = by_host(id) else {
        return;
    };
    if terminal.host.swap(false, Ordering::AcqRel) {
        changed(terminal);
        release(index);
    }
}

/// Another hold on the terminal a host's id names, for a program started on it.
pub fn from_host(id: u32) -> Option<TerminalRef> {
    let (index, terminal) = by_host(id)?;
    terminal.holds.fetch_add(1, Ordering::AcqRel);
    Some(TerminalRef { index })
}

/// A program has been made on `terminal`, as the first of its job: its group is the
/// foreground one, and its session the terminal's.
pub fn adopt(terminal: &TerminalRef, pgid: u32, sid: u32) {
    let terminal = terminal.get();
    if terminal.foreground.compare_exchange(0, pgid, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        terminal.session.store(sid, Ordering::Release);
    }
}

/// Wakes whoever waits on the terminal, and pollers.
fn changed(terminal: &Terminal) {
    terminal.wakes.fetch_add(1, Ordering::Release);
    feos_sync::futex_wake(&terminal.wakes, usize::MAX);
    pipe::advance_readiness();
}

/// Writes what the discipline echoed, as far as the pipe has room: a full one loses it
/// rather than holding up the host's typing.
fn echo_out(echo: Option<PipeId>, bytes: &[u8]) {
    if let Some(echo) = echo {
        if !bytes.is_empty() {
            let _ = pipe::try_write_id(echo, bytes);
        }
    }
}

/// Typed bytes from the host: see the module's description of what's taken. How many.
pub fn input(id: u32, bytes: &[u8]) -> usize {
    let Some((_, terminal)) = by_host(id) else {
        return 0;
    };
    let wanted = terminal.wanted.load(Ordering::Acquire);
    let now = time::monotonic_ns();
    let mut taken = 0;
    let mut raised = None;
    for &byte in bytes {
        let (received, echo) = {
            let mut state = terminal.state.lock();
            let Some(inner) = state.as_mut() else {
                break;
            };
            // Nobody asking, or a reader that has what it waits for already: only a
            // signal's character goes in, and what's before it, which that character
            // takes away anyway. The rest is offered again when it's wanted.
            let asked = wanted && !inner.tty.satisfied();
            if !asked && !bytes[taken..].iter().any(|&later| inner.tty.signal_of(later).is_some()) {
                break;
            }
            (inner.tty.receive(byte, now), inner.echo)
        };
        taken += 1;
        for _ in 0..received.rub_out {
            echo_out(echo, b"\x08 \x08");
        }
        echo_out(echo, received.echo());
        if let Some(signal) = received.signal {
            raised = Some(signal);
            break;
        }
    }
    if taken > 0 {
        let satisfied = terminal.state.lock().as_ref().is_some_and(|inner| inner.tty.satisfied());
        if satisfied {
            terminal.wanted.store(false, Ordering::Release);
        }
        changed(terminal);
    }
    if let Some(signal) = raised {
        sched::signal_group(terminal.foreground.load(Ordering::Acquire), signal.number());
    }
    taken
}

/// Takes back what was typed on the host's terminal and not read, into `out`: for the
/// host to give its console back once the program has ended, as keys typed ahead.
pub fn take_input(id: u32, out: &mut [u8]) -> usize {
    let Some((_, terminal)) = by_host(id) else {
        return 0;
    };
    let mut state = terminal.state.lock();
    state.as_mut().map_or(0, |inner| inner.tty.take_unread(out))
}

/// The host's terminal changed size: stored, and `SIGWINCH` for the foreground group if
/// that's a change.
pub fn resize(id: u32, rows: u16, columns: u16) {
    let Some((_, terminal)) = by_host(id) else {
        return;
    };
    set_size(terminal, WinSize { rows, columns, x_pixels: 0, y_pixels: 0 });
}

fn set_size(terminal: &Terminal, size: WinSize) {
    let moved = {
        let mut state = terminal.state.lock();
        match state.as_mut() {
            Some(inner) if inner.size != size => {
                inner.size = size;
                true
            }
            _ => false,
        }
    };
    if moved {
        sched::signal_group(terminal.foreground.load(Ordering::Acquire), sig::WINCH);
    }
}

/// What `POLL` sees of the terminal: readable (a whole line in canonical mode), and
/// whether the host has gone. A poller that finds nothing wants input, as a reader does.
pub fn poll_state(terminal: &TerminalRef) -> (bool, bool) {
    let terminal = terminal.get();
    let readable = terminal.state.lock().as_ref().is_some_and(|inner| inner.tty.readable());
    let hung_up = !terminal.host.load(Ordering::Acquire);
    if !readable {
        terminal.wanted.store(true, Ordering::Release);
    }
    (readable, hung_up)
}

/// Bytes a read would take now: `FD_NREAD`.
pub fn available(terminal: &TerminalRef) -> usize {
    terminal.get().state.lock().as_ref().map_or(0, |inner| inner.tty.available())
}

/// Whether the reader in `slot`, of `process`, may read the terminal, or what instead.
fn may_read(slot: usize, process: &Process, terminal: &Terminal) -> Result<(), isize> {
    let Ok((pgid, sid)) = sched::group_of(slot, 0) else {
        return Ok(());
    };
    let foreground = terminal.foreground.load(Ordering::Acquire);
    let session = terminal.session.load(Ordering::Acquire);
    let bit = 1u64 << (sig::TTIN - 1);
    let ignores = process.signals().ignored.load(Ordering::Acquire) & bit != 0 || sched::signal_mask(slot) & bit != 0;
    // Asked about only if it would matter: it looks at every task.
    let orphaned = || sched::group_orphaned(pgid, sid);
    match feos_tty::background_read(pgid, sid, foreground, session, ignores, false) {
        Background::Read => Ok(()),
        Background::Fail => Err(-errno::EIO),
        Background::Signal if orphaned() => Err(-errno::EIO),
        Background::Signal => {
            sched::signal_group(pgid, sig::TTIN);
            Err(-errno::EINTR)
        }
    }
}

/// What a read found: bytes taken, or a wait, until a deadline if there is one.
enum Found {
    Took(usize),
    Wait(Option<u64>),
}

/// `read` of the terminal: through the line discipline, as its settings say.
pub fn read(slot: usize, process: &Process, terminal: &TerminalRef, address: usize, length: usize, nonblocking: bool) -> isize {
    let terminal = terminal.get();
    if let Err(error) = super::files::ready(process, address, length, true) {
        return error;
    }
    if let Err(error) = may_read(slot, process, terminal) {
        return error;
    }
    if length == 0 {
        return 0;
    }
    let mut bounce = [0u8; feos_tty::INPUT_MAX];
    let wanted = length.min(bounce.len());
    let started = time::monotonic_ns();
    loop {
        let seen = terminal.wakes.load(Ordering::Acquire);
        let found = {
            let mut state = terminal.state.lock();
            let Some(inner) = state.as_mut() else {
                return 0;
            };
            let termios = *inner.tty.termios();
            if termios.lflag & tc::ICANON != 0 {
                match inner.tty.read(&mut bounce[..wanted]) {
                    Some(count) => Found::Took(count),
                    None => Found::Wait(None),
                }
            } else {
                let (min, time) = (termios.cc[tc::VMIN], termios.cc[tc::VTIME]);
                match feos_tty::noncanonical(min, time, inner.tty.available(), wanted, time::monotonic_ns(), started, inner.tty.last_input()) {
                    Wait::Now(count) => Found::Took(inner.tty.read(&mut bounce[..count]).unwrap_or(0)),
                    Wait::Until(deadline) => Found::Wait(deadline),
                }
            }
        };
        match found {
            Found::Took(count) => {
                if let Err(error) = process.mm().copy_out(address, &bounce[..count]) {
                    return error;
                }
                return count as isize;
            }
            Found::Wait(deadline) => {
                // Nobody left to type: end of file, as a hung-up terminal's.
                if !terminal.host.load(Ordering::Acquire) {
                    return 0;
                }
                if nonblocking {
                    terminal.wanted.store(true, Ordering::Release);
                    return -errno::EAGAIN;
                }
                terminal.wanted.store(true, Ordering::Release);
                if sched::interrupted() {
                    return sched::interrupted_result();
                }
                sched::wait_word_until(&terminal.wakes, seen, deadline.map(time::deadline_ticks));
            }
        }
    }
}

/// Waits while `TCOOFF` holds the terminal's output. `Err` with the call's result if a
/// signal or a kill comes first.
fn wait_for_output(terminal: &Terminal, nonblocking: bool) -> Result<(), isize> {
    loop {
        let seen = terminal.wakes.load(Ordering::Acquire);
        if !terminal.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        if nonblocking {
            return Err(-errno::EAGAIN);
        }
        if sched::interrupted() {
            return Err(sched::interrupted_result());
        }
        sched::wait_word_until(&terminal.wakes, seen, None);
    }
}

/// A piece of a write, at most, before output processing: with what it becomes, a page
/// of the kernel stack.
const PIECE: usize = 1024;

/// `write` to the terminal: through its output processing (`OPOST | ONLCR`), to `pipe`,
/// the stream the descriptor is, or for `/dev/tty` (`None`) the terminal's own echo pipe.
pub fn write(process: &Process, terminal: &TerminalRef, pipe: Option<PipeId>, address: usize, length: usize, nonblocking: bool) -> isize {
    let terminal = terminal.get();
    if length == 0 {
        return 0;
    }
    if !process.mm().in_window(address, length) {
        return -errno::EFAULT;
    }
    let (oflag, echo) = match terminal.state.lock().as_ref() {
        Some(inner) => (inner.tty.termios().oflag, inner.echo),
        None => return -errno::EIO,
    };
    let Some(target) = pipe.or(echo) else {
        // A stream with no pipe is the console, which a program's write always reached.
        return super::files::console_write(process, address, length);
    };
    let mut piece = [0u8; PIECE];
    let mut out = [0u8; 2 * PIECE];
    let mut done = 0;
    while done < length {
        if done > 0 && sched::kill_requested() {
            return KILLED;
        }
        if let Err(error) = wait_for_output(terminal, nonblocking) {
            return if done == 0 { error } else { done as isize };
        }
        let count = (length - done).min(PIECE);
        if let Err(error) = process.mm().copy_in(address + done, &mut piece[..count]) {
            return if done == 0 { error } else { done as isize };
        }
        let (taken, filled) = feos_tty::output(oflag, &piece[..count], &mut out);
        match pipe::write_some(target, &out[..filled], false, nonblocking) {
            Err(pipe::Killed) if done == 0 => return sched::interrupted_result(),
            Err(pipe::Killed) => break,
            Ok(Transfer::Moved(moved)) if moved == filled => done += taken,
            // Part of it: what went, counted in the program's bytes, translated again
            // into as much room as went.
            Ok(Transfer::Moved(moved)) => {
                done += feos_tty::output(oflag, &piece[..count], &mut out[..moved]).0;
                break;
            }
            Ok(Transfer::Closed) if done == 0 => return -errno::EPIPE,
            Ok(Transfer::WouldBlock) if done == 0 => return -errno::EAGAIN,
            Ok(_) => break,
        }
    }
    done as isize
}

/// Waits until what was written to `pipe`, and what the terminal echoed, has gone out.
fn drain(terminal: &Terminal, pipe: Option<PipeId>) -> Result<(), isize> {
    let echo = terminal.state.lock().as_ref().and_then(|inner| inner.echo);
    for id in [pipe, echo].into_iter().flatten() {
        if pipe::wait_empty(id).is_err() {
            return Err(sched::interrupted_result());
        }
    }
    Ok(())
}

/// Reads an `i32` argument from `arg`.
fn read_int(process: &Process, arg: usize) -> Result<i32, isize> {
    let mut bytes = [0u8; 4];
    process.mm().copy_in(arg, &mut bytes)?;
    Ok(i32::from_ne_bytes(bytes))
}

/// The terminal's `IOCTL`s, `request` one of them, on a descriptor of the caller in
/// `slot` whose writes go to `pipe` (or the echo pipe, for `None`).
pub fn ioctl(slot: usize, process: &Process, terminal_ref: &TerminalRef, pipe: Option<PipeId>, request: usize, arg: usize) -> isize {
    let terminal = terminal_ref.get();
    let give = |bytes: &[u8]| process.mm().copy_out(arg, bytes).map_or_else(|error| error, |()| 0);
    let result: Result<isize, isize> = (|| match request {
        ioctl::TTY_GET_ATTR => {
            let termios = terminal.state.lock().as_ref().map(|inner| *inner.tty.termios()).ok_or(-errno::EIO)?;
            Ok(give(&termios.to_bytes()))
        }
        ioctl::TTY_SET_ATTR | ioctl::TTY_SET_ATTR_DRAIN | ioctl::TTY_SET_ATTR_FLUSH => {
            // Read once, before anything waits or changes.
            let mut bytes = [0u8; Termios::SIZE];
            process.mm().copy_in(arg, &mut bytes)?;
            let termios = Termios::from_bytes(bytes);
            if request != ioctl::TTY_SET_ATTR {
                drain(terminal, pipe)?;
            }
            {
                let mut state = terminal.state.lock();
                let inner = state.as_mut().ok_or(-errno::EIO)?;
                if request == ioctl::TTY_SET_ATTR_FLUSH {
                    inner.tty.flush_input();
                }
                inner.tty.set_termios(termios);
            }
            // A reader may be satisfied now: out of canonical mode, a partial line is.
            changed(terminal);
            Ok(0)
        }
        ioctl::TTY_GET_WINSIZE => {
            let size = terminal.state.lock().as_ref().map(|inner| inner.size).ok_or(-errno::EIO)?;
            Ok(give(&size.to_bytes()))
        }
        ioctl::TTY_SET_WINSIZE => {
            let mut bytes = [0u8; WinSize::SIZE];
            process.mm().copy_in(arg, &mut bytes)?;
            set_size(terminal, WinSize::from_bytes(bytes));
            Ok(0)
        }
        ioctl::TTY_GET_PGRP => Ok(give(&(terminal.foreground.load(Ordering::Acquire) as i32).to_ne_bytes())),
        ioctl::TTY_SET_PGRP => {
            let pgid = read_int(process, arg)?;
            let Ok(pgid) = u32::try_from(pgid).map_err(|_| ()).and_then(|pgid| if pgid == 0 { Err(()) } else { Ok(pgid) }) else {
                return Err(-errno::EINVAL);
            };
            let (_, sid) = sched::group_of(slot, 0)?;
            // Only a terminal of the caller's own session is its to change, as Linux's
            // controlling terminal is; and only to a group there.
            if sid != terminal.session.load(Ordering::Acquire) {
                return Err(-errno::ENOTTY);
            }
            if !sched::group_exists(pgid, sid) {
                return Err(-errno::EPERM);
            }
            terminal.foreground.store(pgid, Ordering::Release);
            changed(terminal);
            Ok(0)
        }
        ioctl::TTY_DRAIN => drain(terminal, pipe).map(|()| 0),
        ioctl::TTY_FLUSH => {
            let (input, output) = match arg {
                tc::IFLUSH => (true, false),
                tc::OFLUSH => (false, true),
                tc::IOFLUSH => (true, true),
                _ => return Err(-errno::EINVAL),
            };
            let echo = {
                let mut state = terminal.state.lock();
                let inner = state.as_mut().ok_or(-errno::EIO)?;
                if input {
                    inner.tty.flush_input();
                }
                inner.echo
            };
            if output {
                for id in [pipe, echo].into_iter().flatten() {
                    pipe::discard(id);
                }
            }
            changed(terminal);
            Ok(0)
        }
        ioctl::TTY_FLOW => {
            match arg {
                tc::OOFF => terminal.stopped.store(true, Ordering::Release),
                tc::OON => terminal.stopped.store(false, Ordering::Release),
                // No input flow control to suspend: a host only feeds what's asked for.
                tc::IOFF | tc::ION => {}
                _ => return Err(-errno::EINVAL),
            }
            changed(terminal);
            Ok(0)
        }
        ioctl::TTY_GET_SID => Ok(give(&(terminal.session.load(Ordering::Acquire) as i32).to_ne_bytes())),
        _ => Err(-errno::EINVAL),
    })();
    result.unwrap_or_else(|error| error)
}
