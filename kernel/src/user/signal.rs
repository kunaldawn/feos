//! Signal handlers (stage 1c.9): dispositions, delivery, frames and `SIGRETURN`
//! (section 8 of the ABI design).
//!
//! A process's dispositions are behind its signal lock, [`Signals::actions`]. What the
//! scheduler has to look at without that lock, under its own with interrupts masked, is
//! in atomics beside them: the process-pending set, which signals have a handler, which
//! are ignored, and `SIGCHLD`'s flags. Each slot points at its process's [`Signals`]
//! (`sched`'s `SIGNALS`), so a signal sent by `KILL` is made pending, or ignored, or takes
//! its default action, where it's sent, under the scheduler's lock. The lock order is the
//! signal lock, then `SCHEDULER`; nothing under `SCHEDULER` takes the signal lock.
//!
//! A handler is entered only as a thread goes back to its program: [`deliver`] runs in
//! task context on the thread's kernel stack, at the end of a call or fault, or in a
//! kernel thread the scheduler diverts a resume into when it finds a signal due for a
//! thread interrupted at its program. Either way it can fault the user stack in and wait.
//! It builds the frame from the thread's saved user frame, in a zeroed buffer, through
//! the port's [`UserFrame`], copies it out, and points the user frame at the handler.
//! `SIGRETURN` copies the context in once and has the port rebuild the frame from a
//! whitelist of its fields ([`sigreturn`]).
//!
//! A process signal is taken by one thread: the first that doesn't block it and claims
//! its bit ([`feos_signal::claim`]), as it goes back to its program, or as a wait it's in
//! is interrupted for it (`sched::interrupted`, which keeps what it claims in the
//! thread's own claimed word until the call returns). Thread signals (`SIGPIPE`,
//! `THREAD_KILL`) are pending in the thread's own word.
//!
//! Stage 1c.10 adds masks (`SIGPROCMASK`, `SIGPENDING`, `SIGSUSPEND`), alternate stacks,
//! calls a signal interrupts (`EINTR`, or the call made again for `SA_RESTART`: see
//! [`deliver_after`]), `THREAD_KILL` and `ITIMER_REAL` (`SETITIMER`; the deadline is
//! here, the task that raises `SIGALRM` for it is `sched`'s).

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use feos_abi::{errno, int_arg, itimer, nr, sa, si, sig, ss, ITimerVal, SigAction, SigInfo, SigStack};
use feos_signal::frame::{Entry, UserFrame, Window, CONTEXT_MAX, FRAME_MAX};
use feos_signal::{bit, Action, Restart, Status};
use feos_sync::Mutex;

use super::Process;
use crate::{sched, Frame};

/// Signals that can be neither caught, ignored nor blocked.
pub const UNBLOCKABLE: u64 = feos_signal::UNBLOCKABLE;

/// Signals whose default action ignores them: setting `SIG_DFL` on one pending discards
/// it, as `SIG_IGN` does.
const DEFAULT_IGNORED: u64 = bit(sig::CHLD) | bit(sig::URG) | bit(sig::WINCH);

/// `SIGCHLD`'s `SA_NOCLDSTOP` and `SA_NOCLDWAIT`, in [`Signals::child_flags`].
pub const NOCLDSTOP: u32 = 1;
pub const NOCLDWAIT: u32 = 2;

/// Who raised a process signal, kept until it's taken. Standard signals don't queue: a
/// second one pending replaces the first's sender.
struct Sender {
    /// The sender's pid in the high half, its uid in the low.
    who: AtomicU64,
    /// The code in the high half, a `SIGCHLD`'s status in the low.
    code: AtomicU64,
    /// A `SIGCHLD`'s user time, in clock ticks.
    ticks: AtomicU64,
}

/// A process's signal state.
pub struct Signals {
    /// The dispositions: the process's signal lock.
    actions: Mutex<[SigAction; 64]>,
    /// Signals pending for the process, for any thread that doesn't block them.
    pub pending: AtomicU64,
    /// Signals with a handler, and those set to `SIG_IGN`: written under [`Self::actions`],
    /// read under the scheduler's lock.
    pub caught: AtomicU64,
    pub ignored: AtomicU64,
    /// [`NOCLDSTOP`] and [`NOCLDWAIT`], from `SIGCHLD`'s disposition.
    pub child_flags: AtomicU32,
    senders: [Sender; 64],
    /// `ITIMER_REAL`: when it's next due, on the monotonic clock in nanoseconds (0 for
    /// disarmed), and what it's re-armed by (0 for once). Set by `SETITIMER`, moved on by
    /// `sched`'s alarm task with a compare-and-swap on `alarm_at`. Kept across `EXEC`,
    /// since the process is; a spawned program's starts disarmed.
    pub alarm_at: AtomicU64,
    pub alarm_every: AtomicU64,
}

impl Signals {
    /// Every signal at its default, but those in `ignored`, which a spawn kept.
    pub fn new(ignored: u64) -> Self {
        let ignored = ignored & !UNBLOCKABLE;
        let mut actions = [SigAction::default(); 64];
        for (index, action) in actions.iter_mut().enumerate() {
            if ignored & 1 << index != 0 {
                action.handler = sa::IGN;
            }
        }
        // A `SIGCHLD` ignored by inheritance reaps as one ignored here would.
        let (caught, ignored, child_flags) = Self::derive(&actions);
        Self {
            actions: Mutex::new(actions),
            pending: AtomicU64::new(0),
            caught: AtomicU64::new(caught),
            ignored: AtomicU64::new(ignored),
            child_flags: AtomicU32::new(child_flags),
            senders: [const { Sender { who: AtomicU64::new(0), code: AtomicU64::new(0), ticks: AtomicU64::new(0) } }; 64],
            alarm_at: AtomicU64::new(0),
            alarm_every: AtomicU64::new(0),
        }
    }

    /// Notes what a process signal about to be made pending carries. Under the
    /// scheduler's lock: lock-free.
    pub fn note_sender(&self, info: &SigInfo) {
        let Some(sender) = (info.signo as usize).checked_sub(1).and_then(|index| self.senders.get(index)) else {
            return;
        };
        sender.who.store(u64::from(info.pid() as u32) << 32 | u64::from(info.uid()), Ordering::Relaxed);
        sender.code.store(u64::from(info.code as u32) << 32 | u64::from(info.status() as u32), Ordering::Relaxed);
        let ticks = i64::from_ne_bytes(info.fields[16..24].try_into().unwrap_or([0; 8]));
        sender.ticks.store(ticks as u64, Ordering::Release);
    }

    /// What the process signal `signal`, just taken, carries.
    fn sender(&self, signal: u32) -> SigInfo {
        let sender = &self.senders[signal as usize - 1];
        let who = sender.who.load(Ordering::Acquire);
        let code = sender.code.load(Ordering::Acquire);
        let (pid, uid) = ((who >> 32) as u32, who as u32);
        let (code, status) = ((code >> 32) as u32 as i32, code as u32 as i32);
        if signal == sig::CHLD {
            SigInfo::child(code, pid, uid, status, sender.ticks.load(Ordering::Acquire) as i64, 0)
        } else {
            SigInfo::sender(signal, code, pid, uid)
        }
    }

    /// The ignored set a program this one spawns starts with: what's ignored stays
    /// ignored across a spawn, as across a fork and an exec.
    pub fn inherited(&self) -> u64 {
        self.ignored.load(Ordering::Acquire)
    }

    /// A forked child's: every disposition as this process has it, handlers and their
    /// flags and masks included (the child runs the same code), and nothing else. Nothing
    /// pending (a `SIGCHLD` pending here stays this process's) and no interval timer.
    pub fn fork_copy(&self) -> Self {
        let actions = *self.actions.lock();
        let (caught, ignored, child_flags) = Self::derive(&actions);
        Self {
            actions: Mutex::new(actions),
            pending: AtomicU64::new(0),
            caught: AtomicU64::new(caught),
            ignored: AtomicU64::new(ignored),
            child_flags: AtomicU32::new(child_flags),
            senders: [const { Sender { who: AtomicU64::new(0), code: AtomicU64::new(0), ticks: AtomicU64::new(0) } }; 64],
            alarm_at: AtomicU64::new(0),
            alarm_every: AtomicU64::new(0),
        }
    }

    /// For `EXEC`: handlers aren't kept by a new image, so each goes back to its default.
    /// What's ignored stays ignored, and the mask and pending signals stay as they are.
    pub fn exec_reset(&self) {
        let mut actions = self.actions.lock();
        // Flags and masks go too, as Linux's exec clears them: an ignored signal keeps
        // only its `SIG_IGN`.
        for action in actions.iter_mut() {
            let ignored = action.handler == sa::IGN;
            *action = SigAction::default();
            if ignored {
                action.handler = sa::IGN;
            }
        }
        // Rederived from what's kept, so a `SIGCHLD` still ignored keeps reaping.
        self.publish(&actions);
    }

    /// The atomics for `actions`, under its lock.
    fn publish(&self, actions: &[SigAction; 64]) {
        let (caught, ignored, flags) = Self::derive(actions);
        self.caught.store(caught, Ordering::Release);
        self.ignored.store(ignored, Ordering::Release);
        self.child_flags.store(flags, Ordering::Release);
    }

    /// The caught and ignored sets and `SIGCHLD`'s flags that `actions` give.
    fn derive(actions: &[SigAction; 64]) -> (u64, u64, u32) {
        let (mut caught, mut ignored) = (0u64, 0u64);
        for (index, action) in actions.iter().enumerate() {
            match action.handler {
                sa::DFL => {}
                sa::IGN => ignored |= 1 << index,
                _ => caught |= 1 << index,
            }
        }
        let chld = actions[sig::CHLD as usize - 1];
        let mut flags = 0;
        if chld.flags & sa::NOCLDSTOP != 0 {
            flags |= NOCLDSTOP;
        }
        // An explicit `SIG_IGN` of `SIGCHLD` reaps as `SA_NOCLDWAIT` does.
        if chld.flags & sa::NOCLDWAIT != 0 || chld.handler == sa::IGN {
            flags |= NOCLDWAIT;
        }
        (caught, ignored, flags)
    }
}

/// Whether `[address, address + 1)` is in the process's window.
fn in_window(process: &Process, address: u64) -> bool {
    usize::try_from(address).is_ok_and(|address| process.mm().in_window(address, 1))
}

/// `sigaction`.
pub fn sys_sigaction(slot: usize, process: &Process, signal: usize, new: usize, old: usize) -> isize {
    let Some(signal) = int_arg(signal).and_then(|signal| u32::try_from(signal).ok()).filter(|&signal| feos_signal::is_signal(signal))
    else {
        return -errno::EINVAL;
    };
    // Read once, and checked from the copy.
    let wanted = if new != 0 {
        let mut bytes = [0u8; SigAction::SIZE];
        if let Err(error) = process.mm().copy_in(new, &mut bytes) {
            return error;
        }
        let action = SigAction::from_bytes(bytes);
        if feos_signal::is_unstoppable(signal) || action.flags & !sa::ALL != 0 {
            return -errno::EINVAL;
        }
        if action.handler > sa::IGN {
            if action.flags & sa::RESTORER == 0 {
                return -errno::EINVAL;
            }
            if !in_window(process, action.handler) || !in_window(process, action.restorer) {
                return -errno::EFAULT;
            }
        }
        Some(SigAction { mask: action.mask & !UNBLOCKABLE, reserved: 0, ..action })
    } else {
        None
    };
    // The old one's room, readied before anything changes.
    if old != 0 {
        if let Err(error) = process.mm().fault_in(old, SigAction::SIZE, true) {
            return error;
        }
    }
    let signals = process.signals();
    let previous = {
        let mut actions = signals.actions.lock();
        let index = signal as usize - 1;
        let previous = actions[index];
        if let Some(action) = wanted {
            actions[index] = action;
            signals.publish(&actions);
            // Pending and now ignored, explicitly or by default: discarded, from the process
            // and every thread of it. Under the signal lock, so no delivery decides on the
            // old disposition meanwhile.
            if action.handler == sa::IGN || (action.handler == sa::DFL && DEFAULT_IGNORED & bit(signal) != 0) {
                signals.pending.fetch_and(!bit(signal), Ordering::AcqRel);
                sched::discard_thread_signal(slot, bit(signal));
            }
        }
        previous
    };
    if old != 0 {
        if let Err(error) = process.mm().copy_out(old, &previous.to_bytes()) {
            return error;
        }
    }
    0
}

/// The alternate stack of the thread in `slot`, the caller, as `SIGALTSTACK` and a
/// frame report it, against the stack pointer `sp` its program is at: `SS_DISABLE` for
/// none, `SS_ONSTACK` while `sp` is on it. And whether it is.
fn alt_stack_at(slot: usize, sp: u64) -> (SigStack, bool) {
    let (base, size) = sched::alt_stack(slot);
    if size == 0 {
        return (SigStack { sp: 0, flags: ss::DISABLE, reserved: 0, size: 0 }, false);
    }
    // As Linux's `on_sig_stack`: the stack grows down, so its top is on it and its base
    // isn't.
    let on = sp > base && sp - base <= size;
    (SigStack { sp: base, flags: if on { ss::ONSTACK } else { 0 }, reserved: 0, size }, on)
}

/// The user window, as the frame checker takes it.
fn window(process: &Process) -> Window {
    let (base, end) = process.mm().window();
    Window { base: base as u64, end: end as u64 }
}

/// Enters the handler `action` for `signal` in the thread in `slot`, the caller, which is
/// in the kernel: the frame goes on its stack, or its alternate stack for `SA_ONSTACK`
/// if it has one and isn't on it already, and its user frame is pointed at the handler
/// with the handler's mask. The frame's mask, what the handler's `SIGRETURN` restores, is
/// `restore`, or the thread's own. `false` if the frame couldn't be written (a stack
/// pointer that's bad, or a stack that's full, as after an overflow without an alternate
/// stack): the caller ends the process.
fn enter(
    slot: usize,
    process: &Process,
    signal: u32,
    action: &SigAction,
    info: &SigInfo,
    fault_address: u64,
    restore: Option<u64>,
) -> bool {
    let mask = sched::signal_mask(slot);
    let user = sched::user_frame(slot);
    // SAFETY: the thread is the caller, in the kernel, so nothing else uses its user
    // frame: see `sched::user_frame`.
    let sp = unsafe { (*user).stack_pointer() } as u64;
    let (stack, on) = alt_stack_at(slot, sp);
    let alt_top = (action.flags & sa::ONSTACK != 0 && stack.size != 0 && !on).then(|| stack.sp.wrapping_add(stack.size));
    let entry = Entry {
        signal,
        handler: action.handler,
        restorer: action.restorer,
        info: *info,
        mask: restore.unwrap_or(mask),
        stack,
        fault_address,
        alt_top,
    };
    let mut out = [0u8; FRAME_MAX];
    // SAFETY: as above.
    let built = unsafe { (*user).enter_handler(&entry, &mut out) };
    let Some((address, handler)) = built else {
        return false;
    };
    let Ok(address) = usize::try_from(address) else {
        return false;
    };
    if process.mm().copy_out(address, &out[..Frame::FRAME_SIZE]).is_err() {
        return false;
    }
    // SAFETY: as above.
    unsafe { user.write(handler) };
    let mut blocked = mask | action.mask;
    if action.flags & sa::NODEFER == 0 {
        blocked |= bit(signal);
    }
    sched::set_signal_mask(slot, blocked);
    true
}

/// The disposition of `signal` as it's taken, reset to the default first if it's a
/// handler with `SA_RESETHAND`.
fn take_action(process: &Process, signal: u32) -> SigAction {
    let signals = process.signals();
    let mut actions = signals.actions.lock();
    let index = signal as usize - 1;
    let action = actions[index];
    if action.handler > sa::IGN && action.flags & sa::RESETHAND != 0 {
        actions[index] = SigAction::default();
        signals.publish(&actions);
    }
    action
}

/// Takes the next signal the thread in `slot` doesn't block: its own first, then those of
/// its process's it claimed in a wait (whatever its mask is by now: it didn't block them
/// then), then its process's, claimed now.
fn take(slot: usize, process: &Process) -> Option<(u32, SigInfo)> {
    let mask = sched::signal_mask(slot);
    let signals = process.signals();
    if let Some(signal) = feos_signal::claim(sched::thread_pending(slot), mask) {
        // One `THREAD_KILL` sent comes from the thread's own process.
        if sched::tkilled(slot).fetch_and(!bit(signal), Ordering::AcqRel) & bit(signal) != 0 {
            let (uid, _) = sched::credentials(slot);
            return Some((signal, SigInfo::sender(signal, si::TKILL, process.pid(), uid)));
        }
        return Some((signal, SigInfo::sender(signal, si::KERNEL, 0, 0)));
    }
    if let Some(signal) = feos_signal::claim(sched::claimed(slot), 0) {
        return Some((signal, signals.sender(signal)));
    }
    let signal = feos_signal::claim(&signals.pending, mask)?;
    Some((signal, signals.sender(signal)))
}

/// A call a signal interrupted with nothing done, which returned `EINTR`: whether it may
/// be made again, and what it came in with that its result overwrote.
#[derive(Clone, Copy)]
pub struct Interrupted {
    pub restart: Restart,
    pub number: usize,
    pub first: usize,
}

/// Whether the call `number` with `args` is made again after a signal interrupts it
/// (section 8.3 of the ABI design): the reads and writes, the waits for children and
/// locks, unless a handler without `SA_RESTART` runs; the timed waits never.
/// `SIGSUSPEND` is made again when the signal that woke it runs no handler (ignored by
/// then, or a stop and continue), as Linux's `ERESTARTNOHAND`: it returns only once a
/// handler has run (section 3's `SIGSUSPEND`, and 1c.10's notes).
pub fn restart_of(number: usize, args: [usize; 6]) -> Restart {
    match number {
        nr::READ | nr::WRITE | nr::PREAD | nr::PWRITE | nr::WAIT | nr::WAIT4 | nr::FLOCK | nr::OPENAT | nr::SPAWN2 | nr::IOCTL => {
            Restart::Restartable
        }
        nr::FCNTL if int_arg(args[1]).is_some_and(|command| command as usize == feos_abi::f::SETLKW) => Restart::Restartable,
        nr::SIGSUSPEND => Restart::NoHandler,
        _ => Restart::Never,
    }
}

/// Puts the caller's user frame back to the call `interrupted` was, to make it again.
fn rewind(slot: usize, interrupted: &Interrupted) {
    // SAFETY: the caller's own user frame, which it's in the kernel for.
    unsafe { (*sched::user_frame(slot)).rewind_syscall(interrupted.number as u64, interrupted.first as u64) };
}

/// Delivers every signal due for the thread in `slot`, the caller, on its way back to its
/// program: each handler's frame on top of the last, so the first to run is the last
/// taken. One whose disposition went back to its default meanwhile takes that action;
/// one ignored by now is dropped. Only returns if the thread goes back to its program.
///
/// `interrupted` is the call it's on its way back from, if a signal cut it short: made
/// again or not, as [`Restart::again`] says for the first handler entered (or none), by
/// rewinding the user frame before that handler's frame is built, so the handler's
/// `SIGRETURN` comes back to the call. A `SIGSUSPEND`'s mask goes back here too: in the
/// first handler's frame, or at once if none runs.
pub fn deliver_after(slot: usize, interrupted: Option<Interrupted>) {
    let Some(process) = super::process(slot) else {
        return;
    };
    let mut interrupted = interrupted;
    let mut restore = sched::take_suspended_mask(slot);
    while let Some((signal, info)) = take(slot, process) {
        let action = take_action(process, signal);
        match action.handler {
            sa::IGN => {}
            sa::DFL => default_action(slot, signal),
            _ => {
                if let Some(call) = interrupted.take() {
                    if call.restart.again(Some(action.flags)) {
                        rewind(slot, &call);
                    }
                }
                if !enter(slot, process, signal, &action, &info, 0, restore.take()) {
                    sched::end_by_signal(slot, sig::SEGV);
                }
            }
        }
    }
    if let Some(call) = interrupted.filter(|call| call.restart.again(None)) {
        rewind(slot, &call);
    }
    if let Some(mask) = restore {
        sched::set_signal_mask(slot, mask);
    }
}

/// A signal's default action on the caller's own process, as it's taken.
fn default_action(slot: usize, signal: u32) {
    match feos_signal::default_action(signal) {
        Action::Terminate => sched::end_by_signal(slot, signal),
        Action::Stop => sched::stop_own_process(slot, signal),
        Action::Continue | Action::Ignore => {}
    }
}

/// A synchronous fault's `signal` in the thread in `slot`, the caller: its handler is
/// entered at once, if it has one it doesn't block. `false` if it hasn't (blocked,
/// ignored or default), or the frame couldn't be written: the fault then ends the
/// process with the signal.
pub fn fault(slot: usize, signal: u32, code: i32, address: u64) -> bool {
    let Some(process) = super::process(slot) else {
        return false;
    };
    let signals = process.signals();
    if signals.caught.load(Ordering::Acquire) & bit(signal) == 0 || sched::signal_mask(slot) & bit(signal) != 0 {
        return false;
    }
    let action = take_action(process, signal);
    if action.handler <= sa::IGN {
        return false;
    }
    let info = SigInfo::fault(signal, code, address);
    enter(slot, process, signal, &action, &info, address, None)
}

/// `SIGPIPE` for the thread in `slot`, the caller, whose write found no reader: ignored,
/// pending for its handler, or the end of its process. The write returns `EPIPE` unless
/// the process ends.
pub fn broken_pipe(slot: usize, process: &Process) {
    let signals = process.signals();
    let pipe = bit(sig::PIPE);
    if signals.ignored.load(Ordering::Acquire) & pipe != 0 {
        return;
    }
    // Caught, or blocked: pending, for the way back or until it's unblocked.
    if signals.caught.load(Ordering::Acquire) & pipe != 0 || sched::signal_mask(slot) & pipe != 0 {
        sched::thread_pending(slot).fetch_or(pipe, Ordering::AcqRel);
        return;
    }
    sched::end_by_signal(slot, sig::PIPE);
}

/// `sigreturn`, for the thread in `slot`, the caller: its user frame from the context at
/// `context`, read once and rebuilt from a whitelist, with the context's mask. A context
/// that can't be read, or fails a check, ends the process with `SIGSEGV`.
pub fn sigreturn(slot: usize, context: usize) {
    let Some(process) = super::process(slot) else {
        sched::end_by_signal(slot, sig::SEGV);
    };
    let mut bytes = [0u8; CONTEXT_MAX];
    if process.mm().copy_in(context, &mut bytes[..Frame::CONTEXT_SIZE]).is_err() {
        sched::end_by_signal(slot, sig::SEGV);
    }
    let user = sched::user_frame(slot);
    // SAFETY: as in `enter`: the caller's own user frame, which it's in the kernel for.
    let rebuilt = unsafe { (*user).sigreturn(&bytes[..Frame::CONTEXT_SIZE], window(process)) };
    match rebuilt {
        Ok((frame, mask)) => {
            // SAFETY: as above.
            unsafe { user.write(frame) };
            sched::set_signal_mask(slot, mask);
        }
        Err(_) => sched::end_by_signal(slot, sig::SEGV),
    }
}

/// What a `SIGCHLD` for a child's change carries, and whether its parent wants one: not
/// for a stop or continue with `SA_NOCLDSTOP`.
pub fn child_info(status: Status, pid: u32, uid: u32, ticks: u64) -> SigInfo {
    let (code, value) = match status {
        Status::Exited(code) => (si::CLD_EXITED, code & 0xff),
        Status::Signaled(signal) => (si::CLD_KILLED, i32::from(signal)),
        Status::Stopped(signal) => (si::CLD_STOPPED, i32::from(signal)),
        Status::Continued => (si::CLD_CONTINUED, sig::CONT as i32),
    };
    SigInfo::child(code, pid, uid, value, ticks as i64, 0)
}

/// A signal number from a program: `1..=64`, or 0 too where `zero` says (a check only).
fn signal_arg(signal: usize, zero: bool) -> Option<u32> {
    int_arg(signal).and_then(|signal| u32::try_from(signal).ok()).filter(|&signal| (zero && signal == 0) || feos_signal::is_signal(signal))
}

/// Copies a signal set in, once.
fn read_set(process: &Process, address: usize) -> Result<u64, isize> {
    let mut bytes = [0u8; 8];
    process.mm().copy_in(address, &mut bytes)?;
    Ok(u64::from_ne_bytes(bytes))
}

/// `sigprocmask`. The old mask's room is readied first, so nothing changes if it can't
/// be written. A pending signal it unblocks is delivered on the way back, before the
/// program goes on.
pub fn sys_sigprocmask(slot: usize, process: &Process, how: usize, set: usize, old: usize) -> isize {
    let change = if set != 0 {
        let Some(how) = int_arg(how).filter(|&how| feos_signal::change_mask(how, 0, 0).is_some()) else {
            return -errno::EINVAL;
        };
        match read_set(process, set) {
            Ok(set) => Some((how, set)),
            Err(error) => return error,
        }
    } else {
        None
    };
    if old != 0 {
        if let Err(error) = process.mm().fault_in(old, 8, true) {
            return error;
        }
    }
    let previous = sched::signal_mask(slot);
    if let Some((how, set)) = change {
        if let Some(mask) = feos_signal::change_mask(how, previous, set) {
            sched::set_signal_mask(slot, mask);
        }
    }
    if old != 0 {
        if let Err(error) = process.mm().copy_out(old, &previous.to_ne_bytes()) {
            return error;
        }
    }
    0
}

/// `sigpending`: what's pending for the thread or its process that the thread blocks.
pub fn sys_sigpending(slot: usize, process: &Process, out: usize) -> isize {
    let pending = sched::thread_pending(slot).load(Ordering::Acquire)
        | sched::claimed(slot).load(Ordering::Acquire)
        | process.signals().pending.load(Ordering::Acquire);
    match process.mm().copy_out(out, &(pending & sched::signal_mask(slot)).to_ne_bytes()) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

/// `sigsuspend`: the mask `*mask` until a signal it doesn't block is to be taken, then
/// `EINTR`. The mask it replaced goes back as the handler is entered, in its frame, or
/// after delivery if there's no handler (see [`deliver_after`]), when the call is made
/// again instead.
pub fn sys_sigsuspend(slot: usize, process: &Process, mask: usize) -> isize {
    let wanted = match read_set(process, mask) {
        Ok(wanted) => wanted,
        Err(error) => return error,
    };
    sched::set_suspended_mask(slot, sched::signal_mask(slot));
    sched::set_signal_mask(slot, wanted);
    sched::wait_for_signal();
    -errno::EINTR
}

/// `sigaltstack`. Everything is checked from one copy before anything changes; the old
/// stack's room is readied first.
pub fn sys_sigaltstack(slot: usize, process: &Process, new: usize, old: usize) -> isize {
    // SAFETY: the caller's own user frame, which it's in the kernel for.
    let sp = unsafe { (*sched::user_frame(slot)).stack_pointer() } as u64;
    let (current, on) = alt_stack_at(slot, sp);
    let wanted = if new != 0 {
        let mut bytes = [0u8; SigStack::SIZE];
        if let Err(error) = process.mm().copy_in(new, &mut bytes) {
            return error;
        }
        let stack = SigStack::from_bytes(bytes);
        if stack.flags != 0 && stack.flags != ss::DISABLE {
            return -errno::EINVAL;
        }
        if on {
            return -errno::EPERM;
        }
        if stack.flags == ss::DISABLE {
            Some((0, 0))
        } else {
            if stack.size < ss::MINSIGSTKSZ as u64 {
                return -errno::ENOMEM;
            }
            let in_window = usize::try_from(stack.sp)
                .ok()
                .zip(usize::try_from(stack.size).ok())
                .is_some_and(|(base, size)| process.mm().in_window(base, size));
            if !in_window {
                return -errno::EFAULT;
            }
            Some((stack.sp, stack.size))
        }
    } else {
        None
    };
    if old != 0 {
        if let Err(error) = process.mm().fault_in(old, SigStack::SIZE, true) {
            return error;
        }
    }
    if let Some((base, size)) = wanted {
        sched::set_alt_stack(slot, base, size);
    }
    if old != 0 {
        if let Err(error) = process.mm().copy_out(old, &current.to_bytes()) {
            return error;
        }
    }
    0
}

/// `thread_kill`.
pub fn sys_thread_kill(slot: usize, tid: usize, signal: usize) -> isize {
    let (Some(tid), Some(signal)) = (int_arg(tid), signal_arg(signal, true)) else {
        return -errno::EINVAL;
    };
    let Some(tid) = u32::try_from(tid).ok().filter(|&tid| tid != 0) else {
        return -errno::ESRCH;
    };
    match sched::thread_kill_from(slot, tid, signal) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

/// A timer setting from a program, once checked: its interval and value in nanoseconds.
fn timer_of(value: ITimerVal) -> Option<(u64, u64)> {
    Some((feos_signal::itimer::to_nanos(value.interval)?, feos_signal::itimer::to_nanos(value.value)?))
}

/// `setitimer`, and with `new` zero `getitimer`: `ITIMER_REAL`, the process's, only. The
/// request is read once and checked from the copy; the old setting's room is readied
/// before anything changes.
pub fn sys_setitimer(process: &Process, which: usize, new: usize, old: usize) -> isize {
    if int_arg(which) != Some(itimer::REAL) {
        return -errno::EINVAL;
    }
    let wanted = if new != 0 {
        let mut bytes = [0u8; ITimerVal::SIZE];
        if let Err(error) = process.mm().copy_in(new, &mut bytes) {
            return error;
        }
        match timer_of(ITimerVal::from_bytes(bytes)) {
            Some(wanted) => Some(wanted),
            None => return -errno::EINVAL,
        }
    } else {
        None
    };
    if old != 0 {
        if let Err(error) = process.mm().fault_in(old, ITimerVal::SIZE, true) {
            return error;
        }
    }
    let signals = process.signals();
    let now = crate::time::monotonic_ns();
    let (at, every) = match wanted {
        // The interval first: the alarm task reads it once it has seen the deadline.
        Some((interval, value)) => {
            let previous_every = signals.alarm_every.swap(interval, Ordering::AcqRel);
            let due = if value == 0 { 0 } else { now.saturating_add(value).max(1) };
            let previous_at = signals.alarm_at.swap(due, Ordering::AcqRel);
            sched::alarms_changed();
            (previous_at, previous_every)
        }
        None => (signals.alarm_at.load(Ordering::Acquire), signals.alarm_every.load(Ordering::Acquire)),
    };
    if old != 0 {
        let left = feos_signal::itimer::remaining(at, now);
        let report = ITimerVal {
            interval: feos_signal::itimer::from_nanos(every),
            value: feos_signal::itimer::from_nanos(left),
        };
        if let Err(error) = process.mm().copy_out(old, &report.to_bytes()) {
            return error;
        }
    }
    0
}
