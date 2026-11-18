//! The time calls: `CLOCK_GETTIME`, `CLOCK_GETRES`, `NANOSLEEP`, and `FUTEX_WAIT2`'s
//! deadline.
//!
//! Every deadline becomes a counter value, which is what the scheduler waits for: a
//! `CLOCK_REALTIME` one as the wall clock stands when the call starts, so a later change
//! to the wall clock doesn't move it.

use core::sync::atomic::{AtomicU32, Ordering};

use feos_abi::{clock, errno, int_arg, timer, Timespec};

use super::memory_calls::KernelFrames;
use super::process::{Process, UserMm};
use crate::{sched, time};

/// What a clock id names, once checked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Clock {
    Realtime,
    Monotonic,
    ProcessCpu,
    ThreadCpu,
}

fn clock_of(id: usize) -> Option<Clock> {
    match id {
        clock::REALTIME | clock::REALTIME_COARSE => Some(Clock::Realtime),
        clock::MONOTONIC | clock::MONOTONIC_RAW | clock::MONOTONIC_COARSE | clock::BOOTTIME => Some(Clock::Monotonic),
        clock::PROCESS_CPUTIME_ID => Some(Clock::ProcessCpu),
        clock::THREAD_CPUTIME_ID => Some(Clock::ThreadCpu),
        _ => None,
    }
}

/// Copies a `Timespec` in, once, and checks it: the copy is all that's used after.
fn read_timespec(user: &UserMm, address: usize) -> Result<u64, isize> {
    let mut bytes = [0u8; Timespec::SIZE];
    user.copy_in(address, &mut bytes)?;
    Timespec::from_bytes(bytes).to_nanos().ok_or(-errno::EINVAL)
}

fn write_timespec(user: &UserMm, address: usize, ns: u64) -> isize {
    match user.copy_out(address, &Timespec::from_nanos(ns).to_bytes()) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

/// `clock_gettime`.
pub(super) fn sys_clock_gettime(slot: usize, process: &Process, id: usize, out: usize) -> isize {
    let Some(clock) = clock_of(id) else {
        return -errno::EINVAL;
    };
    let ns = match clock {
        Clock::Realtime => time::realtime_ns(),
        Clock::Monotonic => time::monotonic_ns(),
        Clock::ProcessCpu => sched::process_cpu_ns(process),
        Clock::ThreadCpu => sched::thread_cpu_ns(slot),
    };
    write_timespec(process.mm(), out, ns)
}

/// `clock_getres`: a period of the counter for the clocks read from it, a scheduler tick
/// for the CPU clocks.
pub(super) fn sys_clock_getres(process: &Process, id: usize, out: usize) -> isize {
    let Some(clock) = clock_of(id) else {
        return -errno::EINVAL;
    };
    if out == 0 {
        return 0;
    }
    let ns = match clock {
        Clock::Realtime | Clock::Monotonic => time::resolution_ns(),
        Clock::ProcessCpu | Clock::ThreadCpu => sched::cpu_resolution_ns(),
    };
    write_timespec(process.mm(), out, ns)
}

/// `nanosleep`, which is `clock_nanosleep`. Nothing interrupts it yet but a kill, which
/// ends the thread on its way out, so it never says `EINTR` and never writes `remain`
/// until signal handlers exist; `remain` is checked all the same.
pub(super) fn sys_nanosleep(process: &Process, args: [usize; 6]) -> isize {
    let [id, flags, request, remain, _, _] = args;
    let user = process.mm();
    // Linux's three, and not the coarse or raw variants, which it refuses too.
    let realtime = match id {
        clock::REALTIME => true,
        clock::MONOTONIC | clock::BOOTTIME => false,
        // Linux sleeps on it; nothing here would.
        clock::PROCESS_CPUTIME_ID => return -errno::EOPNOTSUPP,
        _ => return -errno::EINVAL,
    };
    if flags & !timer::ABSTIME != 0 {
        return -errno::EINVAL;
    }
    if remain != 0 && !user.in_window(remain, Timespec::SIZE) {
        return -errno::EFAULT;
    }
    let ns = match read_timespec(user, request) {
        Ok(ns) => ns,
        Err(error) => return error,
    };
    let deadline = if flags & timer::ABSTIME == 0 {
        time::monotonic_ns().saturating_add(ns)
    } else if realtime {
        time::realtime_to_monotonic(ns)
    } else {
        ns
    };
    // A kill ends the thread as the call unwinds, whatever this returns.
    sched::sleep_until(time::deadline_ticks(deadline));
    0
}

/// The counter value a `FUTEX_WAIT2` waits until, if it has a deadline: checked, then
/// made monotonic.
fn futex_deadline(user: &UserMm, deadline: usize, id: usize) -> Result<Option<u64>, isize> {
    let realtime = match id {
        clock::REALTIME => true,
        clock::MONOTONIC => false,
        _ => return Err(-errno::EINVAL),
    };
    if deadline == 0 {
        return Ok(None);
    }
    let ns = read_timespec(user, deadline)?;
    let ns = if realtime { time::realtime_to_monotonic(ns) } else { ns };
    Ok(Some(time::deadline_ticks(ns)))
}

/// `futex_wait2`. Every argument is checked before the word is looked at.
pub(super) fn sys_futex_wait2(user: &UserMm, args: [usize; 6]) -> isize {
    let [address, expected, deadline, id, flags, _] = args;
    if flags != 0 {
        return -errno::EINVAL;
    }
    let Some(expected) = int_arg(expected) else {
        return -errno::EINVAL;
    };
    match futex_deadline(user, deadline, id) {
        Ok(deadline) => futex_wait_until(user, address, expected as u32, deadline),
        Err(error) => error,
    }
}

/// A user futex wait, untimed or until a counter value. The word is named by what maps
/// it, not by its frame: see `UserMm::futex_word`. Its frame is pinned from the check to
/// the resume, so the check the scheduler makes under its lock reads the word itself, and
/// the space's unmap count is checked with it, so an unmap that woke the word's waiters
/// before this one blocked keeps it from blocking.
pub(super) fn futex_wait_until(user: &UserMm, address: usize, expected: u32, deadline: Option<u64>) -> isize {
    let (key, pinned, unmaps) = match user.futex_word(address) {
        Ok(word) => word,
        Err(error) => return error,
    };
    // SAFETY: the word is aligned, and its frame pinned until below; the kernel's
    // mapping of a frame is its address.
    let word = unsafe { &*(pinned.address as *const AtomicU32) };
    let result = if word.load(Ordering::Acquire) != expected {
        -errno::EAGAIN
    } else if sched::futex_wait_user(word, key, expected, (user.unmap_count(), unmaps), deadline) {
        -errno::ETIMEDOUT
    } else {
        0
    };
    pinned.unpin(&mut KernelFrames);
    result
}
