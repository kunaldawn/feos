//! Signals, as far as stage 1c.10 of `docs/feos-abi-v2.md` has them: what a signal's
//! default action is, how a process's end, stop or continue is told to each caller, how
//! a pending signal is claimed by one thread ([`claim`]), the frames a handler is
//! entered and left by ([`frame`], both ports' on any host), masks ([`change_mask`]),
//! whether an interrupted call is made again ([`Restart`]), and `ITIMER_REAL`'s
//! arithmetic ([`itimer`]).
//!
//! The kernel keeps a process's status as a [`Status`] and encodes it per call: `WAIT4`
//! writes the POSIX word ([`Status::wait_status`]), while v1's `WAIT` and the shell see
//! the exit code, or 128 plus the signal that ended it ([`Status::v1`]), as v1 always
//! did (137 for a kill, 139 for a fault). [`Status::to_bits`] packs one into a word an
//! atomic can hold, for records filled in under the scheduler's lock.

#![cfg_attr(not(test), no_std)]

use core::sync::atomic::{AtomicU64, Ordering};

use feos_abi::sig;

pub mod frame;

#[cfg(test)]
mod frame_tests;

/// A signal's bit in a set: bit `n - 1` for signal `n`.
pub const fn bit(signal: u32) -> u64 {
    1 << (signal - 1)
}

/// Claims the lowest-numbered signal in `pending` that `mask` doesn't block, by clearing
/// its bit with a compare-and-swap, so that of two threads looking at one process's
/// pending set at once only one takes each signal, and one whose mask changes while it
/// looks just fails to. `None` if nothing there is unblocked.
pub fn claim(pending: &AtomicU64, mask: u64) -> Option<u32> {
    let mut seen = pending.load(Ordering::Acquire);
    loop {
        let open = seen & !mask;
        if open == 0 {
            return None;
        }
        let low = open.isolate_lowest_one();
        match pending.compare_exchange_weak(seen, seen & !low, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(low.trailing_zeros() + 1),
            Err(now) => seen = now,
        }
    }
}

/// What a signal does to a process with no handler for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// The process ends, with the signal as its status.
    Terminate,
    /// Nothing happens.
    Ignore,
    /// The process stops until it's continued.
    Stop,
    /// A stopped process goes on; one that isn't, nothing.
    Continue,
}

/// Whether `signal` is a signal number at all: `1..=64`.
pub const fn is_signal(signal: u32) -> bool {
    signal >= 1 && signal <= sig::MAX
}

/// `signal`'s default action (section 8.3 of the ABI design, as POSIX has them). There
/// are no core dumps, so the signals that would make one just end the process. Real-time
/// signals end it too. Only meaningful for a [signal](is_signal).
pub const fn default_action(signal: u32) -> Action {
    match signal {
        sig::CHLD | sig::URG | sig::WINCH => Action::Ignore,
        sig::STOP | sig::TSTP | sig::TTIN | sig::TTOU => Action::Stop,
        sig::CONT => Action::Continue,
        _ => Action::Terminate,
    }
}

/// The signals no mask holds: `SIGKILL` and `SIGSTOP`.
pub const UNBLOCKABLE: u64 = bit(sig::KILL) | bit(sig::STOP);

/// A thread's mask after `SIGPROCMASK`'s `how` with `set`: `None` for a `how` that isn't
/// one. Whatever `set` says, `SIGKILL` and `SIGSTOP` are never blocked.
pub const fn change_mask(how: i32, current: u64, set: u64) -> Option<u64> {
    let mask = match how {
        feos_abi::how::BLOCK => current | set,
        feos_abi::how::UNBLOCK => current & !set,
        feos_abi::how::SETMASK => set,
        _ => return None,
    };
    Some(mask & !UNBLOCKABLE)
}

/// What becomes of a call a signal interrupted with nothing done, which returns `EINTR`
/// unless it's made again (section 8.3 of the ABI design).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restart {
    /// Never made again: `POLL`, `NANOSLEEP`, `FUTEX_WAIT2`, `CONNECT`.
    Never,
    /// Made again unless a handler runs: `SIGSUSPEND`, which so goes on waiting after a
    /// stop and a continue, and says `EINTR` only for a handler.
    NoHandler,
    /// Made again unless a handler without `SA_RESTART` runs: `READ`, `WRITE`, `WAIT4`
    /// and the rest of the restartable calls.
    Restartable,
}

impl Restart {
    /// Whether the call is made again, given the flags of the first handler entered as it
    /// returns, or `None` if no handler was.
    pub const fn again(self, handler_flags: Option<u32>) -> bool {
        match (self, handler_flags) {
            (Self::Never, _) => false,
            (_, None) => true,
            (Self::NoHandler, Some(_)) => false,
            (Self::Restartable, Some(flags)) => flags & feos_abi::sa::RESTART != 0,
        }
    }
}

/// `ITIMER_REAL`'s arithmetic, on the monotonic clock in nanoseconds.
pub mod itimer {
    use feos_abi::{Timeval, NANOS_PER_SECOND};

    /// A `Timeval` from a program as nanoseconds: `None` for a negative `sec` or a `usec`
    /// outside `0..1_000_000`. One too long to count is as long as can be.
    pub const fn to_nanos(value: Timeval) -> Option<u64> {
        if value.sec < 0 || value.usec < 0 || value.usec >= 1_000_000 {
            return None;
        }
        Some((value.sec as u64).saturating_mul(NANOS_PER_SECOND).saturating_add(value.usec as u64 * 1000))
    }

    /// Nanoseconds as a `Timeval`, rounded up to the microsecond: a timer that's armed
    /// never reports a zero time left, which would read as disarmed.
    pub const fn from_nanos(ns: u64) -> Timeval {
        let micros = ns.div_ceil(1000);
        Timeval { sec: (micros / 1_000_000) as i64, usec: (micros % 1_000_000) as i64 }
    }

    /// The shortest period a timer is re-armed by. Each time one goes off the kernel's
    /// alarm task takes the scheduler's lock, so an interval of a microsecond, which any
    /// program may ask for, would have that task, which runs above every program, do so
    /// a million times a second, whether or not the program ever takes its `SIGALRM`s.
    /// A shorter interval still reads back as set, as on Linux.
    pub const MIN_PERIOD_NS: u64 = 1_000_000;

    /// The deadline after `at`, which has passed (`now >= at`), for a timer re-armed by
    /// `every` (at least [`MIN_PERIOD_NS`]): the first `at + k * every` past `now`, so a
    /// timer whose process wasn't run for a while goes off once and then keeps its
    /// phase, not once per period missed. 0, disarmed, without an interval.
    pub const fn next(at: u64, every: u64, now: u64) -> u64 {
        if every == 0 {
            return 0;
        }
        let every = if every < MIN_PERIOD_NS { MIN_PERIOD_NS } else { every };
        let missed = now.saturating_sub(at) / every;
        at.saturating_add(every.saturating_mul(missed + 1))
    }

    /// What's left of a timer due at `at` (0 disarmed), as of `now`: at least a
    /// nanosecond for one that's due but hasn't been seen to go off yet.
    pub const fn remaining(at: u64, now: u64) -> u64 {
        if at == 0 {
            0
        } else if at > now {
            at - now
        } else {
            1
        }
    }
}

/// Whether `signal` can be neither caught nor ignored nor blocked: `SIGKILL` and
/// `SIGSTOP`.
pub const fn is_unstoppable(signal: u32) -> bool {
    signal == sig::KILL || signal == sig::STOP
}

/// What became of a process, or what it did last. A primitive representation, so all
/// zero bytes are a valid one (`Exited(0)`), for tables kept in `.bss`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    /// It ended by `exit`, or its task returned, with this code. POSIX keeps only its low
    /// eight bits; v1 keeps all of it.
    Exited(i32),
    /// A signal ended it.
    Signaled(u8),
    /// A signal stopped it.
    Stopped(u8),
    /// It was continued after a stop.
    Continued,
}

/// What a killed process ends with: `SIGKILL`, v1's 137.
pub const KILLED: Status = Status::Signaled(sig::KILL as u8);
/// What a process that faulted ends with: `SIGSEGV`, v1's 139.
pub const FAULTED: Status = Status::Signaled(sig::SEGV as u8);

impl Status {
    /// A process ended by `signal`, a [signal](is_signal).
    pub const fn signaled(signal: u32) -> Self {
        Self::Signaled(signal as u8)
    }

    /// Whether this is the end of the process, not a stop or continue.
    pub const fn is_end(self) -> bool {
        matches!(self, Self::Exited(_) | Self::Signaled(_))
    }

    /// The POSIX status word, as Linux writes it and glibc's and relibc's `W*` macros read
    /// it: an exit with code `c` is `(c & 0xff) << 8`, an end by signal `s` is `s` (never
    /// with the core flag: there are no core dumps), a stop by `s` is `0x7f | s << 8`, a
    /// continue `0xffff`.
    pub const fn wait_status(self) -> i32 {
        match self {
            Self::Exited(code) => (code & 0xff) << 8,
            Self::Signaled(signal) => signal as i32 & 0x7f,
            Self::Stopped(signal) => 0x7f | (signal as i32) << 8,
            Self::Continued => 0xffff,
        }
    }

    /// The POSIX status word read back, as the `W*` macros would: `None` for one no
    /// kernel writes (the core flag, a signal 0 or past 64).
    pub const fn from_wait_status(word: i32) -> Option<Self> {
        if word == 0xffff {
            return Some(Self::Continued);
        }
        if word & !0xffff != 0 {
            return None;
        }
        let low = word & 0x7f;
        let high = (word >> 8) & 0xff;
        if word & 0x80 != 0 {
            return None;
        }
        match low {
            0 => Some(Self::Exited(high)),
            0x7f if high != 0 && high as u32 <= sig::MAX => Some(Self::Stopped(high as u8)),
            signal if high == 0 && signal != 0x7f && signal as u32 <= sig::MAX => Some(Self::Signaled(signal as u8)),
            _ => None,
        }
    }

    /// What v1's `WAIT`, the shell and kernel apps see: an exit's code, or 128 plus the
    /// signal for an end by one. A stop or continue is never what a v1 caller is given;
    /// as a number it's 128 plus the signal, and 128 plus `SIGCONT`.
    pub const fn v1(self) -> i32 {
        match self {
            Self::Exited(code) => code,
            Self::Signaled(signal) | Self::Stopped(signal) => 128 + signal as i32,
            Self::Continued => 128 + sig::CONT as i32,
        }
    }

    /// Packed into one nonzero word, for an atomic: the kind in bits 32 to 39, the code or
    /// signal below.
    pub const fn to_bits(self) -> u64 {
        let (kind, value) = match self {
            Self::Exited(code) => (1u64, code as u32),
            Self::Signaled(signal) => (2, signal as u32),
            Self::Stopped(signal) => (3, signal as u32),
            Self::Continued => (4, 0),
        };
        kind << 32 | value as u64
    }

    /// Unpacked from [`to_bits`](Self::to_bits): `None` for 0, or anything else it never
    /// makes.
    pub const fn from_bits(bits: u64) -> Option<Self> {
        let value = bits as u32;
        match bits >> 32 {
            1 => Some(Self::Exited(value as i32)),
            2 if value <= 0xff => Some(Self::Signaled(value as u8)),
            3 if value <= 0xff => Some(Self::Stopped(value as u8)),
            4 if value == 0 => Some(Self::Continued),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_action_table() {
        // POSIX's table, every standard signal by name.
        let terminate = [
            sig::HUP, sig::INT, sig::QUIT, sig::ILL, sig::TRAP, sig::ABRT, sig::BUS, sig::FPE, sig::KILL, sig::USR1,
            sig::SEGV, sig::USR2, sig::PIPE, sig::ALRM, sig::TERM, sig::STKFLT, sig::XCPU, sig::XFSZ, sig::VTALRM,
            sig::PROF, sig::IO, sig::PWR, sig::SYS,
        ];
        for signal in terminate {
            assert_eq!(default_action(signal), Action::Terminate, "signal {signal}");
        }
        for signal in [sig::CHLD, sig::URG, sig::WINCH] {
            assert_eq!(default_action(signal), Action::Ignore, "signal {signal}");
        }
        for signal in [sig::STOP, sig::TSTP, sig::TTIN, sig::TTOU] {
            assert_eq!(default_action(signal), Action::Stop, "signal {signal}");
        }
        assert_eq!(default_action(sig::CONT), Action::Continue);
        // Real-time signals end a process that doesn't handle them.
        for signal in sig::RTMIN..=sig::MAX {
            assert_eq!(default_action(signal), Action::Terminate, "signal {signal}");
        }
        // Every one of 1 to 31 is named above, once.
        let named = terminate.len() + 3 + 4 + 1;
        assert_eq!(named, 31);
        assert!(!is_signal(0) && is_signal(1) && is_signal(64) && !is_signal(65));
        assert!(is_unstoppable(sig::KILL) && is_unstoppable(sig::STOP) && !is_unstoppable(sig::TSTP) && !is_unstoppable(sig::TERM));
    }

    #[test]
    fn mask_never_blocks_kill() {
        let all = u64::MAX;
        for how in [feos_abi::how::BLOCK, feos_abi::how::SETMASK] {
            let mask = change_mask(how, 0, all).unwrap();
            assert_eq!(mask, all & !UNBLOCKABLE, "how {how}");
            assert_eq!(mask & bit(sig::KILL), 0);
            assert_eq!(mask & bit(sig::STOP), 0);
        }
        // Even from a mask that somehow held them.
        assert_eq!(change_mask(feos_abi::how::UNBLOCK, all, bit(sig::USR1)), Some(all & !UNBLOCKABLE & !bit(sig::USR1)));
        assert_eq!(change_mask(feos_abi::how::BLOCK, bit(sig::USR1), bit(sig::USR2)), Some(bit(sig::USR1) | bit(sig::USR2)));
        assert_eq!(change_mask(feos_abi::how::SETMASK, all, 0), Some(0));
        assert_eq!(change_mask(3, 0, 0), None);
        assert_eq!(change_mask(-1, 0, 0), None);
        assert_eq!(UNBLOCKABLE, 1 << 8 | 1 << 18);
    }

    #[test]
    fn restart_decided_by_handler() {
        use feos_abi::sa;
        assert!(Restart::Restartable.again(None));
        assert!(Restart::Restartable.again(Some(sa::RESTART | sa::RESTORER)));
        assert!(!Restart::Restartable.again(Some(sa::RESTORER)));
        assert!(Restart::NoHandler.again(None));
        assert!(!Restart::NoHandler.again(Some(sa::RESTART)));
        assert!(!Restart::Never.again(None));
        assert!(!Restart::Never.again(Some(sa::RESTART)));
    }

    #[test]
    fn itimer_rearms_by_interval() {
        use feos_abi::Timeval;
        const MS: u64 = 1_000_000;
        // Once, then disarmed.
        assert_eq!(itimer::next(100 * MS, 0, 100 * MS), 0);
        // On time: the next period.
        assert_eq!(itimer::next(100 * MS, 20 * MS, 100 * MS), 120 * MS);
        assert_eq!(itimer::next(100 * MS, 20 * MS, 119 * MS), 120 * MS);
        // Late by several periods: once, and the phase kept.
        assert_eq!(itimer::next(100 * MS, 20 * MS, 175 * MS), 180 * MS);
        assert_eq!(itimer::next(100 * MS, 20 * MS, 180 * MS), 200 * MS);
        // Every deadline it gives is past `now`, on the grid of `at`.
        for now in (100 * MS..400 * MS).step_by(MS as usize / 3) {
            let next = itimer::next(100 * MS, 7 * MS, now);
            assert!(next > now && next - now <= 7 * MS && (next - 100 * MS) % (7 * MS) == 0, "now {now}");
        }
        // No overflow at the far end.
        assert_eq!(itimer::next(u64::MAX - 5, u64::MAX, u64::MAX), u64::MAX);
        // An interval under the floor re-arms by the floor, on its grid, however late.
        assert_eq!(itimer::next(100 * MS, 1000, 100 * MS), 100 * MS + itimer::MIN_PERIOD_NS);
        assert_eq!(itimer::next(100 * MS, 1, 100 * MS + 2_500_000), 100 * MS + 3 * itimer::MIN_PERIOD_NS);
        // Values: checked, and read back.
        assert_eq!(itimer::to_nanos(Timeval { sec: 1, usec: 500_000 }), Some(1_500_000_000));
        assert_eq!(itimer::to_nanos(Timeval { sec: 0, usec: 999_999 }), Some(999_999_000));
        assert_eq!(itimer::to_nanos(Timeval { sec: 0, usec: 1_000_000 }), None);
        assert_eq!(itimer::to_nanos(Timeval { sec: 0, usec: -1 }), None);
        assert_eq!(itimer::to_nanos(Timeval { sec: -1, usec: 0 }), None);
        assert_eq!(itimer::to_nanos(Timeval { sec: i64::MAX, usec: 0 }), Some(u64::MAX));
        assert_eq!(itimer::from_nanos(1_500_000_000), Timeval { sec: 1, usec: 500_000 });
        assert_eq!(itimer::from_nanos(1), Timeval { sec: 0, usec: 1 });
        assert_eq!(itimer::from_nanos(0), Timeval { sec: 0, usec: 0 });
        assert_eq!(itimer::remaining(0, 5), 0);
        assert_eq!(itimer::remaining(10, 4), 6);
        assert_eq!(itimer::remaining(10, 10), 1);
        assert_eq!(itimer::remaining(10, 11), 1);
    }

    #[test]
    fn status_encoding_roundtrip() {
        // Linux's words for each kind, as a C program's `W*` macros read them.
        assert_eq!(Status::Exited(0).wait_status(), 0);
        assert_eq!(Status::Exited(42).wait_status(), 0x2a00);
        assert_eq!(Status::Exited(300).wait_status(), 0x2c00, "only the low eight bits");
        assert_eq!(Status::Signaled(15).wait_status(), 15);
        assert_eq!(Status::Signaled(9).wait_status(), 9);
        assert_eq!(Status::Stopped(19).wait_status(), 0x137f);
        assert_eq!(Status::Stopped(20).wait_status(), 0x147f);
        assert_eq!(Status::Continued.wait_status(), 0xffff);
        for code in 0..=255 {
            let status = Status::Exited(code);
            assert_eq!(Status::from_wait_status(status.wait_status()), Some(status));
            assert!(feos_abi::wait::exited(status.wait_status()));
            assert_eq!(feos_abi::wait::exit_code(status.wait_status()), code);
        }
        for signal in 1..=sig::MAX {
            let ended = Status::signaled(signal);
            assert_eq!(Status::from_wait_status(ended.wait_status()), Some(ended));
            assert!(feos_abi::wait::signaled(ended.wait_status()) && !feos_abi::wait::exited(ended.wait_status()));
            assert_eq!(feos_abi::wait::term_signal(ended.wait_status()), signal as i32);
            let stopped = Status::Stopped(signal as u8);
            assert_eq!(Status::from_wait_status(stopped.wait_status()), Some(stopped));
            assert!(feos_abi::wait::stopped(stopped.wait_status()) && !feos_abi::wait::signaled(stopped.wait_status()));
            assert_eq!(feos_abi::wait::stop_signal(stopped.wait_status()), signal as i32);
        }
        assert_eq!(Status::from_wait_status(0xffff), Some(Status::Continued));
        assert!(feos_abi::wait::continued(Status::Continued.wait_status()));
        // The core flag, and words no kernel writes.
        assert_eq!(Status::from_wait_status(0x8b), None);
        assert_eq!(Status::from_wait_status(0x10000), None);
        assert_eq!(Status::from_wait_status(0x7f), None);
        // And through the packed word an atomic keeps.
        let all = [Status::Exited(0), Status::Exited(-1), Status::Exited(i32::MAX), Status::Signaled(64), Status::Stopped(21), Status::Continued];
        for status in all {
            assert_ne!(status.to_bits(), 0);
            assert_eq!(Status::from_bits(status.to_bits()), Some(status));
        }
        assert_eq!(Status::from_bits(0), None);
        assert_eq!(Status::from_bits(5 << 32), None);
        assert!(Status::Exited(3).is_end() && Status::Signaled(9).is_end());
        assert!(!Status::Stopped(19).is_end() && !Status::Continued.is_end());
    }

    #[test]
    fn v1_status_is_128_plus() {
        // As v1's `WAIT` and the shell always reported them.
        assert_eq!(KILLED.v1(), 137);
        assert_eq!(FAULTED.v1(), 139);
        assert_eq!(Status::signaled(sig::BUS).v1(), feos_abi::EXIT_BUS);
        assert_eq!(FAULTED.v1(), feos_abi::EXIT_FAULTED);
        assert_eq!(Status::signaled(sig::INT).v1(), 130);
        assert_eq!(Status::signaled(sig::TERM).v1(), 143);
        for signal in 1..=sig::MAX {
            assert_eq!(Status::signaled(signal).v1(), 128 + signal as i32);
        }
        // An exit's code is kept whole, as v1 kept it.
        assert_eq!(Status::Exited(7).v1(), 7);
        assert_eq!(Status::Exited(126).v1(), 126);
        assert_eq!(Status::Exited(300).v1(), 300);
    }
}
