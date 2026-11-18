//! Runs through threads: starting, joining, thread pointers, floating point, shared
//! memory, futexes, and how a process ends with several. Reports each check, and exits
//! zero if all passed.
//!
//! Also the children some checks need a process of their own for, and kernel tests run:
//!     ttest --exit-with-threads       four threads busy, then `exit(42)` from the first
//!     ttest --leader-exits            the first thread ends, the other ends the process
//!     ttest --last-thread-zero        every thread ends with `THREAD_EXIT`: status 0
//!     ttest --tls-watch MS            checks its thread pointer for MS milliseconds
//!     ttest --migrate MS              four threads check their thread pointers for MS
//!                                     milliseconds while something moves them about
//!     ttest --purge-race MS           a thread writes a page another discards, for MS
//!                                     milliseconds: never a fault that ends it
//!     ttest --fault-victim            three threads waiting, one of which makes the call
//!                                     `ktest thread-fault` injects a fault into once
//!                                     a byte comes on standard input

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use feos_user::abi::{errno, map, prot, PAGE_SIZE};
use feos_user::thread::{self, Thread, STACK};
use feos_user::{
    entry, futex_wait, futex_wake, getcpu, getpid, gettid, mmap2, munmap, parse_number, pipe, println, read, set_tls,
    sleep_ms, spawn, tls, uptime_ms, wait, write, yield_now, Args,
};

struct Report {
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, good: bool, detail: core::fmt::Arguments<'_>) {
        if good {
            println!("ttest {name}: ok ({detail})");
        } else {
            println!("ttest {name}: FAILED ({detail})");
            self.failed += 1;
        }
    }
}

/// The program, for children.
const PROGRAM: &str = "/bin/ttest";

/// A thread control block, as libc lays one out: its first word is its own address,
/// which is how x86_64 code reads its thread pointer (`fs:0`).
#[repr(C, align(64))]
struct Tcb([AtomicUsize; 8]);

static TCBS: [Tcb; 32] = [const { Tcb([const { AtomicUsize::new(0) }; 8]) }; 32];

/// Points this thread at control block `index`, and says where it is.
fn take_tcb(index: usize) -> usize {
    let tcb = &TCBS[index];
    let address = tcb as *const Tcb as usize;
    tcb.0[0].store(address, Ordering::Relaxed);
    set_tls(address);
    address
}

/// Waits on `word` until it's no longer `value`.
fn wait_while(word: &AtomicU32, value: u32) {
    while word.load(Ordering::Acquire) == value {
        futex_wait(word, value, 0);
    }
}

fn run_flag(arg: usize) -> usize {
    // SAFETY: the caller passes a static's address.
    let flag = unsafe { &*(arg as *const AtomicUsize) };
    flag.store(0x5eed, Ordering::Release);
    0
}

fn run_double(arg: usize) -> usize {
    arg * 2
}

fn run_ids(_: usize) -> usize {
    (gettid() as usize) << 32 | getpid() as usize
}

fn test_basics(report: &mut Report) {
    static FLAG: AtomicUsize = AtomicUsize::new(0);
    let ran = thread::spawn(run_flag, &FLAG as *const AtomicUsize as usize).map(Thread::join);
    report.check("thread-runs", ran == Ok(0) && FLAG.load(Ordering::Acquire) == 0x5eed, format_args!("{ran:?}"));

    let doubled = thread::spawn(run_double, 21).map(Thread::join);
    report.check("thread-arg", doubled == Ok(42), format_args!("{doubled:?}"));

    let (me, pid) = (gettid(), getpid());
    let ids = thread::spawn(run_ids, 0).map(|thread| (thread.tid, thread.join()));
    let (tid, other, its_pid) = match ids {
        Ok((tid, ids)) => (tid as isize, (ids >> 32) as isize, (ids & 0xffff_ffff) as isize),
        Err(error) => (error, error, error),
    };
    report.check(
        "gettid-differs",
        me == pid && other == tid && other != me && other > 0,
        format_args!("main={me} pid={pid} thread={other} created={tid}"),
    );
    report.check("getpid-same", its_pid == pid, format_args!("main={pid} thread={its_pid}"));
}

/// Sets control block `arg`, then yields 1000 times checking it's still there.
fn run_tls(arg: usize) -> usize {
    let address = take_tcb(arg);
    let mut bad = 0;
    for _ in 0..1000 {
        yield_now();
        if tls() != address {
            bad += 1;
        }
    }
    bad
}

fn test_tls(report: &mut Report) {
    let mut threads: [Option<Thread>; 8] = [const { None }; 8];
    for (index, slot) in threads.iter_mut().enumerate() {
        *slot = thread::spawn(run_tls, index + 1).ok();
    }
    let started = threads.iter().filter(|thread| thread.is_some()).count();
    let bad: usize = threads.into_iter().flatten().map(Thread::join).sum();
    // The first thread's own, untouched by the others'.
    let mine = take_tcb(0);
    yield_now();
    report.check(
        "tls-per-thread",
        started == 8 && bad == 0 && tls() == mine,
        format_args!("threads={started} wrong={bad}"),
    );
}

/// The end of a migration run, in uptime milliseconds.
static UNTIL: AtomicUsize = AtomicUsize::new(0);

/// Checks control block `arg` until [`UNTIL`], spinning a little and yielding between,
/// and says how many times it was wrong, with the CPUs it saw in the high bits.
fn run_migrating(arg: usize) -> usize {
    let address = take_tcb(arg);
    let mut bad = 0usize;
    let mut cpus = 0usize;
    let mut rounds = 0usize;
    while (uptime_ms() as usize) < UNTIL.load(Ordering::Relaxed) || rounds < 1000 {
        cpus |= 1 << (getcpu() as usize & 31);
        for _ in 0..2000 {
            core::hint::spin_loop();
        }
        if tls() != address {
            bad += 1;
        }
        if rounds.is_multiple_of(4) {
            yield_now();
        }
        rounds += 1;
    }
    bad.min(0xffff) | cpus << 16
}

/// Four threads check their thread pointers for `ms`, and say which CPUs they saw.
fn migrate(ms: usize) -> (usize, usize) {
    UNTIL.store(uptime_ms() as usize + ms, Ordering::Relaxed);
    let mut threads: [Option<Thread>; 4] = [const { None }; 4];
    for (index, slot) in threads.iter_mut().enumerate() {
        *slot = thread::spawn(run_migrating, 10 + index).ok();
    }
    let mut bad = 0;
    let mut cpus = 0;
    for thread in threads.into_iter() {
        match thread {
            Some(thread) => {
                let result = thread.join();
                bad += result & 0xffff;
                cpus |= result >> 16;
            }
            None => bad += 1,
        }
    }
    (bad, cpus)
}

#[cfg(target_arch = "aarch64")]
fn run_msr_direct(_: usize) -> usize {
    const VALUE: usize = 0x1234_5678_9abc_def0;
    // SAFETY: TPIDR_EL0 is the program's to write.
    unsafe { core::arch::asm!("msr tpidr_el0, {}", in(reg) VALUE, options(nomem, nostack, preserves_flags)) };
    let mut bad = 0;
    let mut cpus = 0usize;
    for round in 0..1000 {
        cpus |= 1 << (getcpu() as usize & 31);
        if round % 2 == 0 {
            yield_now();
        } else {
            sleep_ms(0);
        }
        if tls() != VALUE {
            bad += 1;
        }
    }
    bad | cpus << 16
}

#[cfg(target_arch = "aarch64")]
fn test_arch_tls(report: &mut Report) {
    // Busy threads beside it, so it's moved about and preempted.
    UNTIL.store(uptime_ms() as usize + 300, Ordering::Relaxed);
    let busy = [thread::spawn(run_migrating, 20).ok(), thread::spawn(run_migrating, 21).ok()];
    let result = thread::spawn(run_msr_direct, 0).map(Thread::join);
    for thread in busy.into_iter().flatten() {
        thread.join();
    }
    let (bad, cpus) = match result {
        Ok(result) => (result & 0xffff, result >> 16),
        Err(error) => (error as usize, 0),
    };
    report.check("tls-msr-direct", result.is_ok() && bad == 0, format_args!("wrong={bad} cpus={cpus:#x}"));
}

/// Loads a selector into `fs`, null and the data one in turn, and yields, over and over,
/// for 2 seconds. Each load moves the FS base off the thread's saved pointer: to 0 for the
/// data selector on any CPU, and for the null one where loading it clears the base.
#[cfg(target_arch = "x86_64")]
fn run_selector_reload(_: usize) -> usize {
    let until = uptime_ms() as usize + 2000;
    let mut null = true;
    while (uptime_ms() as usize) < until {
        if null {
            // SAFETY: a null selector is always loadable into `fs`, and this thread
            // never uses its thread pointer.
            unsafe { core::arch::asm!("xor eax, eax", "mov fs, ax", out("eax") _, options(nomem, nostack)) };
        } else {
            // SAFETY: `ss` holds the program's data selector, which `fs` can take, and
            // whose base is 0.
            unsafe { core::arch::asm!("mov ax, ss", "mov fs, ax", out("eax") _, options(nomem, nostack)) };
        }
        null = !null;
        yield_now();
    }
    0
}

#[cfg(target_arch = "x86_64")]
fn test_arch_tls(report: &mut Report) {
    // Another process checking its own thread pointer meanwhile, on the same CPUs: what
    // a selector load leaves in the FS base must never reach it. The reloader's own
    // thread pointer is the watcher's, the same static in the same binary, so the base
    // the kernel last loaded for it is the one the watcher needs: a CPU that skipped the
    // write, or cached the base, would leave the watcher the reloader's 0.
    let child = spawn(PROGRAM, &[PROGRAM, "--tls-watch", "2000"]);
    let shared_tcb = &TCBS[0] as *const Tcb as usize;
    TCBS[0].0[0].store(shared_tcb, Ordering::Relaxed);
    let reloader = thread::spawn_tls(run_selector_reload, 0, shared_tcb);
    let ours = run_tls(31);
    let reloaded = reloader.map(Thread::join);
    let status = if child > 0 { wait(child as usize) } else { child };
    report.check(
        "fs-selector-reload",
        status == 0 && reloaded == Ok(0) && ours == 0,
        format_args!("watcher={status} reloader={reloaded:?} own-wrong={ours}"),
    );
}

/// For `--tls-watch`: checks this thread's pointer for `ms`, yielding, and fails if it
/// ever changes. On x86_64 a lost FS base faults instead.
fn tls_watch(ms: usize) -> i32 {
    let address = take_tcb(0);
    let until = uptime_ms() as usize + ms;
    let mut bad = 0;
    while (uptime_ms() as usize) < until {
        yield_now();
        if tls() != address {
            bad += 1;
        }
    }
    println!("ttest tls-watch: wrong={bad}");
    i32::from(bad != 0)
}

/// For `--migrate`.
fn migrate_main(ms: usize) -> i32 {
    let (bad, cpus) = migrate(ms);
    println!("ttest migrate: wrong={bad} cpus={cpus:#x}");
    i32::from(bad != 0)
}

/// The word a hand-made thread's end clears, and whether it has ended.
static CLEARED: AtomicU32 = AtomicU32::new(1);

fn run_and_end(_: usize) -> usize {
    sleep_ms(20);
    0
}

fn test_clear_word(report: &mut Report) {
    // A word of its own, not the start block's: made by hand.
    let stack = mmap2(0, STACK, prot::READ | prot::WRITE, map::PRIVATE);
    let good = stack > 0 && {
        let stack = stack as usize;
        CLEARED.store(1, Ordering::Release);
        // The trampoline's start block, at the top, with no clear word of its own use.
        let tid = make_thread(stack, run_and_end, &CLEARED);
        let start = uptime_ms();
        wait_while(&CLEARED, 1);
        let waited = uptime_ms() - start;
        munmap(stack, STACK);
        println!("ttest clear-word-wakes: tid={tid} waited={waited}ms");
        tid > 0 && CLEARED.load(Ordering::Acquire) == 0
    };
    report.check("clear-word-wakes", good, format_args!("stack={stack:#x}"));
}

/// Starts `run` on a stack mapped at `stack`, `STACK` long, clearing `word` at its end.
fn make_thread(stack: usize, run: fn(usize) -> usize, word: &AtomicU32) -> isize {
    // A start block like `thread::spawn`'s, whose own word nobody waits on.
    #[repr(C, align(16))]
    struct Start {
        done: AtomicU32,
        run: fn(usize) -> usize,
        arg: usize,
        result: usize,
    }
    let start = (stack + STACK - core::mem::size_of::<Start>()) as *mut Start;
    // SAFETY: the top of a stack just mapped, which nothing else uses.
    unsafe { start.write(Start { done: AtomicU32::new(1), run, arg: 0, result: 0 }) };
    let word = word as *const AtomicU32 as usize;
    feos_user::thread_create(thread::entry(), start as usize, 0, start as usize, word)
}

/// Where a thread that unmaps its own stack finds it: set once it's started.
static OWN_STACK: AtomicUsize = AtomicUsize::new(0);

fn run_unmapping_own_stack(_: usize) -> usize {
    let mut stack = OWN_STACK.load(Ordering::Acquire);
    while stack == 0 {
        yield_now();
        stack = OWN_STACK.load(Ordering::Acquire);
    }
    sleep_ms(10);
    feos_user::thread_exit(stack, STACK)
}

fn test_exit_unmaps(report: &mut Report) {
    OWN_STACK.store(0, Ordering::Release);
    // The clear word is the start block's, inside the stack the thread unmaps.
    let Ok((thread, stack, done)) = thread::spawn_with_stack(run_unmapping_own_stack, 0) else {
        report.check("clear-word-inside-stack", false, format_args!("no thread"));
        return;
    };
    // Not joined: its stack goes with it.
    let tid = thread.tid;
    // SAFETY: the start block's word, mapped until the thread unmaps it.
    let word = unsafe { &*(done as *const AtomicU32) };
    OWN_STACK.store(stack, Ordering::Release);
    let start = uptime_ms();
    // Woken by the end: by then the word is zero and the stack gone, or the wait finds
    // the word already unmapped. Either way it never hangs. The unmap wakes the range's
    // waiters too, so this can't tell that the word was written first: `ktest
    // thread-exit-clear` checks that, through the frame.
    let waited = futex_wait(word, 1, 5000);
    let elapsed = uptime_ms() - start;
    // The range is free: a mapping of exactly it can be had, and nobody takes it after.
    let again = mmap2(stack, STACK, prot::READ | prot::WRITE, map::PRIVATE | map::FIXED_NOREPLACE);
    let reused = again == stack as isize;
    if reused {
        // SAFETY: just mapped.
        unsafe { core::ptr::write_volatile((stack + STACK - 8) as *mut u64, 0xfeed_f00d) };
    }
    report.check(
        "clear-word-inside-stack",
        waited != -errno::ETIMEDOUT && elapsed < 5000,
        format_args!("tid={tid} wait={waited} in {elapsed}ms"),
    );
    report.check("thread-exit-unmaps-stack", reused, format_args!("map-again={again:#x}"));
    sleep_ms(50);
    // SAFETY: mapped just above, if `reused`.
    let kept = reused && unsafe { core::ptr::read_volatile((stack + STACK - 8) as *const u64) } == 0xfeed_f00d;
    report.check("joiner-reuses-stack-range", kept, format_args!("after 50ms"));
    if reused {
        munmap(stack, STACK);
    }
}

fn run_forever(_: usize) -> usize {
    static NEVER: AtomicU32 = AtomicU32::new(0);
    loop {
        futex_wait(&NEVER, 0, 0);
    }
}

fn run_spinning(_: usize) -> usize {
    loop {
        core::hint::spin_loop();
    }
}

/// For `--exit-with-threads`.
fn exit_with_threads() -> i32 {
    for index in 0..4 {
        let _ = thread::spawn(if index % 2 == 0 { run_forever } else { run_spinning }, 0).map(|_| ());
    }
    sleep_ms(50);
    42
}

/// The first thread's pid, for `--leader-exits`.
static FIRST_PID: AtomicUsize = AtomicUsize::new(0);

fn run_outliving(_: usize) -> usize {
    sleep_ms(300);
    let same = getpid() as usize == FIRST_PID.load(Ordering::Acquire);
    feos_user::exit(if same { 7 } else { 8 })
}

/// For `--leader-exits`.
fn leader_exits() -> i32 {
    FIRST_PID.store(getpid() as usize, Ordering::Release);
    match thread::spawn(run_outliving, 0) {
        // Never joined: the process goes on in it.
        Ok(_) => feos_user::thread_exit(0, 0),
        Err(_) => 3,
    }
}

fn run_then_end(_: usize) -> usize {
    sleep_ms(100);
    feos_user::thread_exit(0, 0)
}

/// For `--last-thread-zero`.
fn last_thread_zero() -> i32 {
    for _ in 0..2 {
        let _ = thread::spawn(run_then_end, 0).map(|_| ());
    }
    feos_user::thread_exit(0, 0)
}

fn test_endings(report: &mut Report) {
    let start = uptime_ms();
    let child = spawn(PROGRAM, &[PROGRAM, "--exit-with-threads"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    let took = uptime_ms() - start;
    report.check("exit-ends-all-threads", status == 42 && took < 3000, format_args!("status={status} in {took}ms"));

    let start = uptime_ms();
    let child = spawn(PROGRAM, &[PROGRAM, "--leader-exits"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    let took = uptime_ms() - start;
    report.check("leader-exits-first", status == 7 && took >= 250, format_args!("status={status} after {took}ms"));

    let child = spawn(PROGRAM, &[PROGRAM, "--last-thread-zero"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    report.check("last-thread-exit-status-0", status == 0, format_args!("status={status}"));
}

/// A float sum that depends on every step, so a register another thread's value got
/// into would show.
#[inline(never)]
fn float_sum(seed: usize, rounds: usize) -> u64 {
    let mut value = seed as f64 * 0.5 + 1.0;
    let mut sum = 0.0f64;
    for round in 0..rounds {
        value = value * 1.000_001 + (round % 7) as f64 * 0.25;
        sum += value / (seed as f64 + 1.0) + value * value * 1e-9;
        if round % 512 == 0 {
            yield_now();
        }
    }
    sum.to_bits()
}

fn run_float(seed: usize) -> usize {
    float_sum(seed, 20_000) as usize
}

fn test_fp(report: &mut Report) {
    let mut expected = [0u64; 6];
    for (seed, value) in expected.iter_mut().enumerate() {
        *value = float_sum(seed, 20_000);
    }
    let mut threads: [Option<Thread>; 6] = [const { None }; 6];
    for (seed, slot) in threads.iter_mut().enumerate() {
        *slot = thread::spawn(run_float, seed).ok();
    }
    let mut wrong = 0;
    for (seed, thread) in threads.into_iter().enumerate() {
        if thread.map(Thread::join) != Some(expected[seed] as usize) {
            wrong += 1;
        }
    }
    report.check("fp-per-thread", wrong == 0, format_args!("wrong={wrong} of 6"));
}

static SHARED: AtomicUsize = AtomicUsize::new(0);

fn run_writer(page: usize) -> usize {
    SHARED.store(0xabcd, Ordering::Relaxed);
    // SAFETY: the page the caller mapped.
    unsafe { core::ptr::write_volatile(page as *mut u64, 0x1122_3344) };
    0
}

fn test_shared(report: &mut Report) {
    let page = mmap2(0, PAGE_SIZE, prot::READ | prot::WRITE, map::PRIVATE);
    let good = page > 0 && {
        let joined = thread::spawn(run_writer, page as usize).map(Thread::join);
        // SAFETY: mapped just above.
        let seen = unsafe { core::ptr::read_volatile(page as usize as *const u64) };
        munmap(page as usize, PAGE_SIZE);
        joined.is_ok() && seen == 0x1122_3344 && SHARED.load(Ordering::Relaxed) == 0xabcd
    };
    report.check("shared-memory-visible", good, format_args!("page={page:#x}"));
}

static PING: AtomicU32 = AtomicU32::new(0);
const ROUNDS: u32 = 1000;

/// Answers each odd value the first thread puts in [`PING`] with the even one after it.
fn run_pong(_: usize) -> usize {
    for round in 0..ROUNDS {
        wait_while(&PING, 2 * round);
        if PING.load(Ordering::Acquire) != 2 * round + 1 {
            return 1;
        }
        PING.store(2 * round + 2, Ordering::Release);
        futex_wake(&PING, 1);
    }
    0
}

fn test_futex(report: &mut Report) {
    PING.store(0, Ordering::Release);
    let pong = thread::spawn(run_pong, 0);
    let mut rounds = 0;
    while rounds < ROUNDS && pong.is_ok() {
        PING.store(2 * rounds + 1, Ordering::Release);
        futex_wake(&PING, 1);
        wait_while(&PING, 2 * rounds + 1);
        rounds += 1;
    }
    let joined = pong.map(Thread::join);
    let value = PING.load(Ordering::Acquire);
    report.check("futex-between-threads", joined == Ok(0) && value == 2 * ROUNDS, format_args!("value={value}"));
}

/// Tells [`run_purged_writer`] to stop.
static PURGE_STOP: AtomicU32 = AtomicU32::new(0);
/// Its writes, for the report.
static PURGE_WRITES: AtomicUsize = AtomicUsize::new(0);

/// Writes to the page at `page` until told to stop, with no call between writes: each of
/// the other thread's discards makes the next write fault, on the same page, the same
/// way, with nothing of its own between.
fn run_purged_writer(page: usize) -> usize {
    let mut value = 0u64;
    while PURGE_STOP.load(Ordering::Relaxed) == 0 {
        value += 1;
        // SAFETY: the page the caller mapped, kept until this thread is joined.
        unsafe { core::ptr::write_volatile(page as *mut u64, value) };
        PURGE_WRITES.fetch_add(1, Ordering::Relaxed);
    }
    0
}

/// For `--purge-race`: one thread writing a page while another discards it, as purging
/// allocators do, for `ms`. Every fault is one the tables answer, so it must never end
/// the process: status 0, or 139 from a fault taken for one that can't be fixed.
fn purge_race(ms: usize) -> i32 {
    let page = mmap2(0, PAGE_SIZE, prot::READ | prot::WRITE, map::PRIVATE);
    if page <= 0 {
        return 2;
    }
    let page = page as usize;
    let Ok(writer) = thread::spawn(run_purged_writer, page) else {
        return 3;
    };
    let until = uptime_ms() as usize + ms;
    let mut discards = 0usize;
    while (uptime_ms() as usize) < until {
        if feos_user::madvise(page, PAGE_SIZE, feos_user::abi::madv::DONTNEED) == 0 {
            discards += 1;
        }
        yield_now();
    }
    PURGE_STOP.store(1, Ordering::Relaxed);
    let joined = writer.join();
    munmap(page, PAGE_SIZE);
    println!("ttest purge-race: discards={discards} writes={}", PURGE_WRITES.load(Ordering::Relaxed));
    i32::from(joined != 0 || discards == 0)
}

fn test_purge_race(report: &mut Report) {
    let child = spawn(PROGRAM, &[PROGRAM, "--purge-race", "1000"]);
    let status = if child > 0 { wait(child as usize) } else { child };
    report.check("fault-again-after-other-threads-discard", status == 0, format_args!("status={status}"));
}

/// Reads the pipe `arg` names into a buffer that may be unmapped meanwhile. The result
/// of the read, as a word.
fn run_reader(arg: usize) -> usize {
    let (fd, buffer) = (arg & 0xff, arg >> 8 << 8);
    let got = feos_user::syscall(feos_user::abi::nr::READ, [fd, buffer, 4096, 0, 0, 0]);
    got as usize
}

fn test_munmap_while_reading(report: &mut Report) {
    let Ok((reader, writer)) = pipe() else {
        report.check("munmap-while-other-thread-reads", false, format_args!("no pipe"));
        return;
    };
    let buffer = mmap2(0, PAGE_SIZE, prot::READ | prot::WRITE, map::PRIVATE);
    if buffer <= 0 {
        report.check("munmap-while-other-thread-reads", false, format_args!("no buffer"));
        return;
    }
    let buffer = buffer as usize;
    let thread = thread::spawn(run_reader, buffer | reader);
    // Let it block in the read, then take its buffer away and give it something.
    sleep_ms(50);
    munmap(buffer, PAGE_SIZE);
    write(writer, b"data for a buffer that's gone");
    let got = thread.map(Thread::join).map(|got| got as isize);
    feos_user::close(reader);
    feos_user::close(writer);
    let good = matches!(got, Ok(result) if result == -errno::EFAULT || result > 0);
    report.check("munmap-while-other-thread-reads", good, format_args!("read={got:?}"));
}

#[cfg(target_arch = "aarch64")]
fn test_user_mode_controls(report: &mut Report) {
    let (ctr, dczid, frequency, first): (u64, u64, u64, u64);
    // SAFETY: each is readable at EL0, as the kernel sets it up.
    unsafe {
        core::arch::asm!(
            "mrs {ctr}, ctr_el0",
            "mrs {dczid}, dczid_el0",
            "mrs {frequency}, cntfrq_el0",
            "mrs {first}, cntvct_el0",
            ctr = out(reg) ctr,
            dczid = out(reg) dczid,
            frequency = out(reg) frequency,
            first = out(reg) first,
            options(nomem, nostack, preserves_flags)
        );
    }
    sleep_ms(5);
    let second: u64;
    // SAFETY: as above.
    unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) second, options(nomem, nostack, preserves_flags)) };
    // `dc zva` zeroes a block of `4 << BS` bytes, aligned, unless DZP says it's off.
    let block = 4usize << (dczid & 0xf);
    let allowed = dczid & 0x10 == 0;
    let page = mmap2(0, PAGE_SIZE, prot::READ | prot::WRITE, map::PRIVATE);
    let zeroed = page > 0 && allowed && block <= PAGE_SIZE && {
        let page = page as usize;
        // SAFETY: a page just mapped, and a block aligned within it.
        unsafe {
            core::ptr::write_bytes(page as *mut u8, 0xff, PAGE_SIZE);
            core::arch::asm!("dc zva, {}", in(reg) page + block, options(nostack, preserves_flags));
            let inside = (0..block).all(|at| core::ptr::read_volatile((page + block + at) as *const u8) == 0);
            let outside = core::ptr::read_volatile((page + block - 1) as *const u8) == 0xff
                && core::ptr::read_volatile((page + 2 * block) as *const u8) == 0xff;
            inside && outside
        }
    };
    if page > 0 {
        munmap(page as usize, PAGE_SIZE);
    }
    report.check(
        "user-mode-controls",
        ctr != 0 && zeroed && frequency != 0 && second > first,
        format_args!("ctr={ctr:#x} dczid={dczid:#x} zva-block={block} counter {first}->{second} at {frequency}Hz"),
    );
}

#[cfg(target_arch = "x86_64")]
fn test_user_mode_controls(report: &mut Report) {
    // SAFETY: `rdtsc` is allowed at ring 3 here, as `CR4.TSD` is clear.
    let first = unsafe { core::arch::x86_64::_rdtsc() };
    sleep_ms(5);
    // SAFETY: as above.
    let second = unsafe { core::arch::x86_64::_rdtsc() };
    let features = core::arch::x86_64::__cpuid(1);
    // AVX stays off: the trap frame doesn't hold it, so `cpuid` says the OS hasn't
    // enabled `xsave`.
    let osxsave = features.ecx & (1 << 27) != 0;
    report.check(
        "user-mode-controls",
        second > first && !osxsave,
        format_args!("tsc {first}->{second} osxsave={osxsave}"),
    );
}

/// The barrier the threads of `test_many` wait at.
static GATE: AtomicU32 = AtomicU32::new(0);
static ARRIVED: AtomicU32 = AtomicU32::new(0);

fn run_at_gate(index: usize) -> usize {
    ARRIVED.fetch_add(1, Ordering::AcqRel);
    wait_while(&GATE, 0);
    index * 3 + gettid() as usize % 2
}

/// `N` threads at once, all waiting at a gate until every one has started, then let go
/// together: each has its own id and returns what it was given. With 500, the table is
/// well past the 255 slots a byte could name, and the futex wake reaches them all.
fn test_many<const N: usize>(report: &mut Report, name: &str) {
    GATE.store(0, Ordering::Release);
    ARRIVED.store(0, Ordering::Release);
    let mut threads: [Option<Thread>; N] = [const { None }; N];
    let mut tids = [0usize; N];
    let mut refused = 0;
    for (index, slot) in threads.iter_mut().enumerate() {
        *slot = match thread::spawn(run_at_gate, index) {
            Ok(thread) => Some(thread),
            Err(error) => {
                refused = error;
                None
            }
        };
        tids[index] = slot.as_ref().map_or(0, |thread| thread.tid);
    }
    let started = threads.iter().filter(|thread| thread.is_some()).count();
    let start = uptime_ms();
    while ARRIVED.load(Ordering::Acquire) < started as u32 && uptime_ms() - start < 20_000 {
        sleep_ms(1);
    }
    let arrived = ARRIVED.load(Ordering::Acquire);
    GATE.store(1, Ordering::Release);
    futex_wake(&GATE, usize::MAX);
    let mut wrong = 0;
    for (index, thread) in threads.into_iter().enumerate() {
        if thread.map(Thread::join) != Some(index * 3 + tids[index] % 2) {
            wrong += 1;
        }
    }
    let mut sorted = tids;
    sorted.sort_unstable();
    let distinct = sorted.windows(2).all(|pair| pair[0] != pair[1]);
    report.check(
        name,
        started == N && arrived == N as u32 && wrong == 0 && distinct,
        format_args!("started={started} arrived={arrived} wrong={wrong} refused={refused}"),
    );
}

/// For `--fault-victim`: two threads waiting for good, one waiting for a byte on standard
/// input and then making the call the kernel test faults, and the first thread waiting
/// for good too. Nothing here ends by itself.
fn fault_victim() -> i32 {
    for _ in 0..2 {
        let _ = thread::spawn(run_forever, 0).map(|_| ());
    }
    let _ = thread::spawn(run_victim, 0).map(|_| ());
    run_forever(0);
    0
}

fn run_victim(_: usize) -> usize {
    let mut byte = [0u8; 1];
    read(0, &mut byte);
    // The kernel test has armed a fault for this process's next `gettid` from a thread
    // that isn't the first: this one.
    gettid();
    println!("ttest fault-victim: the call came back");
    feos_user::exit(1)
}

fn main(args: Args) -> i32 {
    if args.has("--exit-with-threads") {
        return exit_with_threads();
    }
    if args.has("--leader-exits") {
        return leader_exits();
    }
    if args.has("--last-thread-zero") {
        return last_thread_zero();
    }
    if args.has("--fault-victim") {
        return fault_victim();
    }
    if let Some(ms) = args.value_of("--tls-watch").and_then(parse_number) {
        return tls_watch(ms);
    }
    if let Some(ms) = args.value_of("--migrate").and_then(parse_number) {
        return migrate_main(ms);
    }
    if let Some(ms) = args.value_of("--purge-race").and_then(parse_number) {
        return purge_race(ms);
    }

    let mut report = Report { failed: 0 };
    test_basics(&mut report);
    test_tls(&mut report);
    let (bad, cpus) = migrate(500);
    report.check("tls-survives-migration", bad == 0, format_args!("wrong={bad} cpus={cpus:#x}"));
    test_arch_tls(&mut report);
    test_clear_word(&mut report);
    test_exit_unmaps(&mut report);
    test_endings(&mut report);
    test_fp(&mut report);
    test_shared(&mut report);
    test_futex(&mut report);
    test_munmap_while_reading(&mut report);
    test_purge_race(&mut report);
    test_user_mode_controls(&mut report);
    test_many::<16>(&mut report, "16-threads");
    test_many::<64>(&mut report, "64-threads");
    test_many::<500>(&mut report, "500-threads");
    if report.failed == 0 {
        println!("ttest: all ok");
        0
    } else {
        println!("ttest: {} FAILED", report.failed);
        1
    }
}

entry!(main);
