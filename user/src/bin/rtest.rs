//! Runs through the clocks and deadlines: `CLOCK_GETTIME` and `CLOCK_GETRES` on every
//! clock, `NANOSLEEP` relative and absolute, `FUTEX_WAIT2` timing out, and the CPU-time
//! clocks. Reports each check, and exits zero if all passed.
//!
//!     rtest [--now SECONDS]       SECONDS is the host's UTC time as the boot test typed
//!                                 the command: the wall clock must be within 2 s of it

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use feos_user::abi::{clock, errno, timer, Timespec, NANOS_PER_SECOND};
use feos_user::thread::{self, Thread};
use feos_user::{
    clock_gettime, entry, futex_wait2, futex_wake, nanosleep, now_ns, parse_number, println, sleep_ms, syscall, yield_now,
    Args,
};

const MS: u64 = 1_000_000;

struct Report {
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, good: bool, detail: core::fmt::Arguments<'_>) {
        if good {
            println!("rtest {name}: ok ({detail})");
        } else {
            println!("rtest {name}: FAILED ({detail})");
            self.failed += 1;
        }
    }
}

/// The latest monotonic reading any thread has had back.
static LATEST: AtomicU64 = AtomicU64::new(0);
/// Reads each thread of the check makes.
const READS: usize = 500_000;

/// Reads the monotonic clock `READS` times. Each reading must be no earlier than the
/// thread's last, and no earlier than the latest any thread had back before it asked.
/// Returns how many weren't.
fn run_monotonic(_: usize) -> usize {
    let mut last = 0;
    let mut backwards = 0;
    for _ in 0..READS {
        let before = LATEST.load(Ordering::Acquire);
        let now = now_ns(clock::MONOTONIC);
        if now < last || now < before || now == 0 {
            backwards += 1;
        }
        last = now;
        LATEST.fetch_max(now, Ordering::AcqRel);
    }
    backwards
}

fn test_monotonic(report: &mut Report) {
    let started = now_ns(clock::MONOTONIC);
    let other = thread::spawn(run_monotonic, 0);
    let mine = run_monotonic(0);
    let theirs = other.map(Thread::join);
    let took = (now_ns(clock::MONOTONIC) - started) / MS;
    report.check(
        "monotonic-never-backwards",
        mine == 0 && theirs == Ok(0),
        format_args!("2x{READS} reads in {took}ms, backwards: {mine} and {theirs:?}"),
    );
}

fn test_realtime(report: &mut Report, reference: Option<usize>) {
    let wall = clock_gettime(clock::REALTIME);
    let coarse = clock_gettime(clock::REALTIME_COARSE);
    let seconds = wall.map_or(-1, |time| time.sec);
    match reference {
        Some(host) => {
            let off = seconds - host as i64;
            report.check(
                "realtime-near-rtc",
                off.abs() <= 2 && coarse.is_ok(),
                format_args!("guest={seconds} host={host} off={off}s"),
            );
        }
        // Run by hand: no reference, so only that it's a date after the clock was set.
        None => report.check(
            "realtime-near-rtc",
            seconds > 1_000_000_000 && coarse.is_ok(),
            format_args!("guest={seconds}, no reference given"),
        ),
    }
}

fn test_getres(report: &mut Report) {
    let mut good = true;
    let mut seen = [0i64; 8];
    for (id, seen) in seen.iter_mut().enumerate() {
        match feos_user::clock_getres(id) {
            Ok(res) => {
                *seen = res.nsec;
                good &= res.sec == 0 && res.nsec > 0;
            }
            Err(_) => good = false,
        }
    }
    // `out` may be zero: only whether the clock exists.
    let none = syscall(feos_user::abi::nr::CLOCK_GETRES, [clock::MONOTONIC, 0, 0, 0, 0, 0]);
    report.check("getres-positive", good && none == 0, format_args!("ns={seen:?} null-out={none}"));
}

fn test_nanosleep(report: &mut Report) {
    // Relative, on each clock that sleeps.
    let mut good = true;
    let mut slept = [0u64; 3];
    for (index, id) in [clock::MONOTONIC, clock::REALTIME, clock::BOOTTIME].into_iter().enumerate() {
        let before = now_ns(clock::MONOTONIC);
        let result = nanosleep(id, 0, &Timespec::from_nanos(20 * MS), None);
        slept[index] = now_ns(clock::MONOTONIC) - before;
        good &= result == 0 && slept[index] >= 20 * MS;
    }
    report.check("nanosleep-relative", good, format_args!("asked 20ms, slept {slept:?}ns"));

    // Absolute, on the monotonic clock and on the wall clock, and one already past.
    let deadline = now_ns(clock::MONOTONIC) + 30 * MS;
    let monotonic = nanosleep(clock::MONOTONIC, timer::ABSTIME, &Timespec::from_nanos(deadline), None);
    let late = now_ns(clock::MONOTONIC) as i64 - deadline as i64;
    let wall_deadline = now_ns(clock::REALTIME) + 30 * MS;
    let wall = nanosleep(clock::REALTIME, timer::ABSTIME, &Timespec::from_nanos(wall_deadline), None);
    let wall_late = now_ns(clock::REALTIME) as i64 - wall_deadline as i64;
    let before = now_ns(clock::MONOTONIC);
    let past = nanosleep(clock::MONOTONIC, timer::ABSTIME, &Timespec::from_nanos(1), None);
    let past_took = now_ns(clock::MONOTONIC) - before;
    report.check(
        "nanosleep-absolute",
        monotonic == 0 && late >= 0 && wall == 0 && wall_late >= 0 && past == 0 && past_took < 500 * MS,
        format_args!("late by {late}ns, on the wall clock {wall_late}ns; a past deadline took {past_took}ns"),
    );
}

static WORD: AtomicU32 = AtomicU32::new(0);
static WAKER_READY: AtomicUsize = AtomicUsize::new(0);
/// How long the waker waits, once running, before it wakes: long enough for the waiter to
/// be blocked by then even on a busy machine.
const WAKER_DELAY_MS: u64 = 50;

fn run_waker(_: usize) -> usize {
    WAKER_READY.store(1, Ordering::Release);
    sleep_ms(WAKER_DELAY_MS as usize);
    WORD.store(1, Ordering::Release);
    futex_wake(&WORD, 1) as usize
}

fn test_futex_deadlines(report: &mut Report) {
    // Nobody wakes it: it times out at its deadline, on either clock, and not before.
    WORD.store(0, Ordering::Release);
    let deadline = now_ns(clock::MONOTONIC) + 30 * MS;
    let result = futex_wait2(&WORD, 0, Some(&Timespec::from_nanos(deadline)), clock::MONOTONIC);
    let late = now_ns(clock::MONOTONIC) as i64 - deadline as i64;
    let wall_deadline = now_ns(clock::REALTIME) + 30 * MS;
    let wall = futex_wait2(&WORD, 0, Some(&Timespec::from_nanos(wall_deadline)), clock::REALTIME);
    let wall_late = now_ns(clock::REALTIME) as i64 - wall_deadline as i64;
    report.check(
        "futex-deadline-times-out",
        result == -errno::ETIMEDOUT && late >= 0 && wall == -errno::ETIMEDOUT && wall_late >= 0,
        format_args!("{result} late by {late}ns; on the wall clock {wall} late by {wall_late}ns"),
    );

    // A deadline already gone: at once, unless the word has changed, which says so first.
    let before = now_ns(clock::MONOTONIC);
    let past = futex_wait2(&WORD, 0, Some(&Timespec::from_nanos(1)), clock::MONOTONIC);
    let wall_past = futex_wait2(&WORD, 0, Some(&Timespec { sec: 0, nsec: 5 }), clock::REALTIME);
    let changed = futex_wait2(&WORD, 7, Some(&Timespec::from_nanos(1)), clock::MONOTONIC);
    let took = now_ns(clock::MONOTONIC) - before;
    report.check(
        "futex-deadline-in-past",
        past == -errno::ETIMEDOUT && wall_past == -errno::ETIMEDOUT && changed == -errno::EAGAIN && took < 500 * MS,
        format_args!("{past} {wall_past}, changed word {changed}, in {took}ns"),
    );

    // A wake well before a distant deadline ends the wait then, with 0. One call, which
    // the waker's wake found blocked: a wait that came back at once (spuriously, or with
    // `EAGAIN` while the deadline is far) would need more than one call, or would have
    // gone before the waker's 50 ms, and the waker would find nobody to wake.
    WORD.store(0, Ordering::Release);
    WAKER_READY.store(0, Ordering::Release);
    let before = now_ns(clock::MONOTONIC);
    let waker = thread::spawn(run_waker, 0);
    while WAKER_READY.load(Ordering::Acquire) == 0 && now_ns(clock::MONOTONIC) - before < 5 * NANOS_PER_SECOND {
        core::hint::spin_loop();
    }
    let far = Timespec::from_nanos(before + 10 * NANOS_PER_SECOND);
    let mut result = 0;
    let mut calls = 0;
    while WORD.load(Ordering::Acquire) == 0 && calls < 1_000 {
        calls += 1;
        result = futex_wait2(&WORD, 0, Some(&far), clock::MONOTONIC);
        if result != 0 && result != -errno::EAGAIN {
            break;
        }
    }
    let took = (now_ns(clock::MONOTONIC) - before) / MS;
    let woke = waker.map(Thread::join);
    report.check(
        "futex-wake-before-deadline",
        result == 0 && calls == 1 && woke == Ok(1) && (WAKER_DELAY_MS..5_000).contains(&took),
        format_args!("{result} after {took}ms in {calls} calls, waker woke {woke:?}"),
    );
}

/// Spins until this thread has used `ns` of CPU time, or `limit_ms` of wall time has gone.
fn spin_cpu(ns: u64, limit_ms: u64) -> u64 {
    let start = now_ns(clock::THREAD_CPUTIME_ID);
    let wall = now_ns(clock::MONOTONIC);
    while now_ns(clock::THREAD_CPUTIME_ID) - start < ns && now_ns(clock::MONOTONIC) - wall < limit_ms * MS {
        core::hint::spin_loop();
    }
    now_ns(clock::THREAD_CPUTIME_ID) - start
}

fn run_spinner(_: usize) -> usize {
    spin_cpu(200 * MS, 5_000) as usize
}

fn run_sleeper(_: usize) -> usize {
    let start = now_ns(clock::THREAD_CPUTIME_ID);
    nanosleep(clock::MONOTONIC, 0, &Timespec::from_nanos(300 * MS), None);
    (now_ns(clock::THREAD_CPUTIME_ID) - start) as usize
}

fn test_cputime(report: &mut Report) {
    // This thread spinning shows in the process's time; so does a thread that spun and
    // has ended.
    let before = now_ns(clock::PROCESS_CPUTIME_ID);
    let spun = spin_cpu(150 * MS, 5_000);
    let middle = now_ns(clock::PROCESS_CPUTIME_ID);
    let ended = thread::spawn(run_spinner, 0).map(Thread::join).unwrap_or(0) as u64;
    // Its end is folded in once it has gone, which the join can see a moment before.
    let mut after = now_ns(clock::PROCESS_CPUTIME_ID);
    for _ in 0..100 {
        if after >= middle + ended {
            break;
        }
        sleep_ms(10);
        after = now_ns(clock::PROCESS_CPUTIME_ID);
    }
    report.check(
        "process-cputime-grows",
        spun >= 150 * MS && middle >= before + 100 * MS && ended >= 150 * MS && after + 20 * MS >= middle + ended,
        format_args!("{}ms, +{}ms spinning, +{}ms after a thread spun {}ms and ended", before / MS, (middle - before) / MS, (after - middle) / MS, ended / MS),
    );

    // Each thread's own time is its own: a sleeper uses next to none while a spinner
    // uses plenty.
    let spinner = thread::spawn(run_spinner, 0);
    let sleeper = thread::spawn(run_sleeper, 0);
    let spinner = spinner.map(Thread::join).unwrap_or(0) as u64;
    let sleeper = sleeper.map(Thread::join).unwrap_or(usize::MAX) as u64;
    let mine = now_ns(clock::THREAD_CPUTIME_ID);
    let process = now_ns(clock::PROCESS_CPUTIME_ID);
    report.check(
        "thread-cputime-per-thread",
        spinner >= 150 * MS && sleeper < 50 * MS && mine <= process && mine > 0,
        format_args!("spinner={}ms sleeper={}ms main={}ms process={}ms", spinner / MS, sleeper / MS, mine / MS, process / MS),
    );
}

fn main(args: Args) -> i32 {
    let reference = args.value_of("--now").and_then(parse_number);
    let mut report = Report { failed: 0 };
    // The wall clock first, nearest the moment the reference was taken.
    test_realtime(&mut report, reference);
    test_monotonic(&mut report);
    test_getres(&mut report);
    test_nanosleep(&mut report);
    test_futex_deadlines(&mut report);
    test_cputime(&mut report);
    yield_now();
    if report.failed == 0 {
        println!("rtest: all ok");
        0
    } else {
        println!("rtest: {} FAILED", report.failed);
        1
    }
}

entry!(main);
