//! `ktest`: kernel self-tests the boot tests drive through the shell.
//!
//! What's tested here can only be seen from inside the kernel: which CPU a task
//! ran on, what a register held across a preemption, how the machine takes a
//! fatal fault. Everything else is tested on the host. Some of these stop the
//! machine on purpose, so nothing but a test should run them.

extern crate alloc;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use feos_app::{App, AppRegistry, Context, Processes, SpawnIo, MAX_CPUS};
use feos_console::println;
use feos_sync::SpinLock;

use feos_hal::{Arch, Clock};

use feos_hal::Board;

use crate::{memory, sched, Machine, Port};

/// The apps the kernel provides itself, in front of the regular registry.
pub struct Registry;

/// The registry installed in `Platform`.
pub static REGISTRY: Registry = Registry;

impl AppRegistry for Registry {
    fn find(&self, name: &str) -> Option<&'static dyn App> {
        if KernelTest.matches(name) {
            return Some(&KernelTest);
        }
        if crate::user_window::WDEMO.matches(name) {
            return Some(&crate::user_window::WDEMO);
        }
        feos_apps::find(name)
    }
}

/// What USB's been up to.
fn test_usb() -> i32 {
    match Machine::usb_stats() {
        Some(stats) => println!(
            "[ktest] usb: devices={} keyboard={} pointer={} storage={}",
            stats.devices, stats.keyboard_reports, stats.pointer_reports, stats.storage_commands
        ),
        None => println!("[ktest] usb: no controller"),
    }
    0
}

/// Shows what the input devices report for three seconds, as apps would see it.
fn test_input(context: &Context<'_>) -> i32 {
    let platform = context.platform;
    let deadline = platform.clock.uptime_ms() + 3000;
    let start = platform.pointer.map(|pointer| pointer.position());
    println!("[ktest] input: start {start:?}");
    while platform.clock.uptime_ms() < deadline {
        if let Some(event) = platform.pointer.and_then(|pointer| pointer.poll()) {
            println!(
                "[ktest] input: pointer {} {} left={:?} right={:?} wheel={}",
                event.x, event.y, event.left, event.right, event.wheel
            );
        }
        if let Some(event) = platform.keyboard.and_then(|keyboard| keyboard.poll()) {
            println!("[ktest] input: key {} pressed={}", event.code, event.pressed);
        }
        sched::sleep_ms(5);
    }
    println!("[ktest] input: done");
    0
}

/// Runs a task for each CPU the machine could have, for `run` to spin on.
fn spawn_workers(count: usize, name: &str, run: fn(usize)) -> Option<usize> {
    let processes = &sched::PROCESSES;
    let mut pids = [0u32; 16];
    let mut spawned = 0;
    for index in 0..count.min(pids.len()) {
        let task = Box::new(move || {
            run(index);
            0
        });
        match processes.spawn_task(name, SpawnIo::default(), task) {
            Ok(pid) => {
                pids[spawned] = pid;
                spawned += 1;
            }
            Err(error) => {
                println!("[ktest] {name}: can't spawn: {error:?}");
                return None;
            }
        }
    }
    for pid in &pids[..spawned] {
        processes.wait(*pid);
    }
    Some(spawned)
}

/// Spins for about `ms`, through ticks, so the scheduler moves things around.
fn spin(ms: u64) {
    Port::services().spin_ms(ms);
}

/// Tasks record every CPU they're seen on, and each CPU's tick count is read.
fn test_cpus() -> i32 {
    static SEEN: AtomicU32 = AtomicU32::new(0);
    SEEN.store(0, Ordering::Relaxed);

    fn worker(_index: usize) {
        for _ in 0..30 {
            let flags = Port::irq_save_and_disable();
            SEEN.fetch_or(1 << Port::cpu_index(), Ordering::Relaxed);
            Port::irq_restore(flags);
            spin(10);
        }
    }

    let before = sched::PROCESSES.system();
    let online = sched::online_cpus();
    if spawn_workers(online * 2, "ktest-cpu", worker).is_none() {
        return 1;
    }
    let after = sched::PROCESSES.system();
    let seen = SEEN.load(Ordering::Relaxed);
    let mut ticking = 0;
    for cpu in 0..online.min(MAX_CPUS) {
        if after.cpu_ticks[cpu] > before.cpu_ticks[cpu] {
            ticking += 1;
        }
    }
    println!(
        "[ktest] cpus: online={online} ran-on={} ticking={ticking} mask={seen:#x}",
        seen.count_ones()
    );
    i32::from(seen.count_ones() as usize != online || ticking != online)
}

/// Loads a pattern, spins through preemption in assembly with no Rust to disturb
/// the registers, and checks the pattern is still there. Returns the failures.
#[cfg(target_arch = "x86_64")]
fn registers_survive(seed: u64, spins: u64) -> u64 {
    let mode = (seed & 3) as u32;
    let mut failed = 0u64;
    let wanted_mxcsr = 0x1f80u32 | (mode << 13);
    let mut saved = 0u32;
    let mut after = 0u32;
    let mut read = [[0u64; 2]; 2];
    // SAFETY: the block changes only the registers it declares, and puts MXCSR
    // back to its default before it ends.
    unsafe {
        core::arch::asm!(
            "stmxcsr [{saved}]",
            "ldmxcsr [{wanted}]",
            "movq xmm7, {seed}",
            "movq xmm15, {seed}",
            "pslldq xmm15, 8",
            "2:",
            "pause",
            "dec {spins}",
            "jnz 2b",
            "movdqu [{read}], xmm7",
            "movdqu [{read} + 16], xmm15",
            "stmxcsr [{after}]",
            "ldmxcsr [{saved}]",
            saved = in(reg) &mut saved,
            wanted = in(reg) &wanted_mxcsr,
            after = in(reg) &mut after,
            read = in(reg) &mut read,
            seed = in(reg) seed,
            spins = inout(reg) spins => _,
            out("xmm7") _,
            out("xmm15") _,
        );
    }
    failed += u64::from(read[0][0] != seed || read[0][1] != 0);
    failed += u64::from(read[1][0] != 0 || read[1][1] != seed);
    failed += u64::from(after != wanted_mxcsr);
    failed
}

#[cfg(target_arch = "aarch64")]
fn registers_survive(seed: u64, spins: u64) -> u64 {
    let mode = (seed & 3) << 22;
    let mut low = 0u64;
    let mut high = 0u64;
    let mut after = 0u64;
    // SAFETY: the block changes only the registers it declares, and puts FPCR
    // back before it ends.
    unsafe {
        core::arch::asm!(
            "mrs {saved}, fpcr",
            "msr fpcr, {mode}",
            "fmov d7, {seed}",
            "mov v31.d[0], xzr",
            "mov v31.d[1], {seed}",
            "2:",
            "yield",
            "subs {spins}, {spins}, #1",
            "b.ne 2b",
            "fmov {low}, d7",
            "mov {high}, v31.d[1]",
            "mrs {after}, fpcr",
            "msr fpcr, {saved}",
            saved = out(reg) _,
            mode = in(reg) mode,
            seed = in(reg) seed,
            spins = inout(reg) spins => _,
            low = out(reg) low,
            high = out(reg) high,
            after = out(reg) after,
            out("v7") _,
            out("v31") _,
        );
    }
    u64::from(low != seed) + u64::from(high != seed) + u64::from(after != mode)
}

/// Every task keeps its own floating-point state across preemption, whichever
/// CPU it comes back on, and real float work agrees with a known answer.
fn test_fp() -> i32 {
    static FAILURES: AtomicUsize = AtomicUsize::new(0);
    FAILURES.store(0, Ordering::Relaxed);

    fn worker(index: usize) {
        let seed = 0x1234_5678_9abc_def0u64.rotate_left(index as u32 * 7) | index as u64;
        for round in 0..20u64 {
            let failed = registers_survive(seed ^ (round << 2), 200_000);
            FAILURES.fetch_add(failed as usize, Ordering::Relaxed);

            // A decoder's kind of arithmetic, with a known sum.
            let mut sum = 0.0f64;
            for term in 1..=2000u32 {
                let term = f64::from(term);
                sum += core::hint::black_box(1.0) / (term * term);
            }
            if !(1.6444 < sum && sum < 1.6445) {
                FAILURES.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let workers = sched::online_cpus() * 2 + 2;
    if spawn_workers(workers, "ktest-fp", worker).is_none() {
        return 1;
    }
    let failures = FAILURES.load(Ordering::Relaxed);
    println!("[ktest] fp: workers={workers} failures={failures}");
    i32::from(failures != 0)
}

/// Panics while every other CPU is busy, some with interrupts masked, and holding
/// a lock, so the panic can't be put down to one task. The fatal path reports how
/// many CPUs it stopped.
fn test_panic() -> i32 {
    static HELD: SpinLock<()> = SpinLock::new(());

    fn worker(index: usize) {
        if index.is_multiple_of(2) {
            Port::irq_disable();
        }
        loop {
            core::hint::spin_loop();
        }
    }

    let processes = &sched::PROCESSES;
    for index in 0..sched::online_cpus().saturating_sub(1) {
        let task = Box::new(move || {
            worker(index);
            0
        });
        if processes.spawn_task("ktest-spin", SpawnIo::default(), task).is_err() {
            return 1;
        }
    }
    // Let them all get onto a CPU.
    spin(300);
    let _guard = HELD.lock();
    panic!("ktest: deliberate panic");
}

/// Runs off the bottom of the stack. There are no base cases: the guard page is.
#[inline(never)]
#[allow(unconditional_recursion)]
fn recurse(depth: usize) -> usize {
    let mut frame = [0u8; 512];
    frame[depth % 512] = depth as u8;
    core::hint::black_box(&mut frame);
    recurse(depth + 1) + frame[(depth + 1) % 512] as usize
}

/// What can go wrong in a task: run through a child, so the machine's response is
/// the only thing being tested.
fn test_failure(kind: &str) -> i32 {
    static HELD: SpinLock<()> = SpinLock::new(());

    let result = match kind {
        "overflow" => recurse(0),
        "null" => {
            // SAFETY: nothing about this is safe, which is the test. Address zero
            // is mapped on AArch64's first block only if someone mapped it: this
            // address is in neither port's map.
            unsafe { core::ptr::read_volatile(core::hint::black_box(0x70_0000_0000usize) as *const usize) }
        }
        "panic" => panic!("ktest: a task's panic"),
        "overflow-locked" => {
            let _guard = HELD.lock();
            recurse(0)
        }
        _ => 0,
    };
    println!("[ktest] failure {kind}: survived with {result}");
    1
}

/// Two tasks pinned to one CPU, spinning: the more urgent one gets far more of it.
fn test_priority() -> i32 {
    static PROGRESS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
    static RUN: AtomicU32 = AtomicU32::new(0);
    PROGRESS[0].store(0, Ordering::Relaxed);
    PROGRESS[1].store(0, Ordering::Relaxed);
    RUN.store(1, Ordering::Release);

    fn worker(index: usize) {
        while RUN.load(Ordering::Acquire) != 0 {
            PROGRESS[index].fetch_add(1, Ordering::Relaxed);
        }
    }

    let processes = &sched::PROCESSES;
    let last = sched::online_cpus() as u32 - 1;
    let mut pids = [0u32; 2];
    for (index, pid) in pids.iter_mut().enumerate() {
        let task = Box::new(move || {
            worker(index);
            0
        });
        let Ok(spawned) = processes.spawn_task("ktest-prio", SpawnIo::default(), task) else {
            return 1;
        };
        *pid = spawned;
        processes.set_affinity(spawned, 1 << last);
    }
    processes.set_priority(pids[0], 12);
    processes.set_priority(pids[1], 3);
    processes.sleep_ms(2000);
    let (high, low) = (PROGRESS[0].load(Ordering::Relaxed), PROGRESS[1].load(Ordering::Relaxed));
    RUN.store(0, Ordering::Release);
    processes.wait(pids[0]);
    processes.wait(pids[1]);
    println!("[ktest] priority: high={high} low={low} favoured={}", high > low * 4 && low > 0);
    i32::from(high <= low * 4 || low == 0)
}

/// A task pinned to one CPU is seen on no other, and one moved to another CPU goes
/// there.
fn test_affinity() -> i32 {
    static SEEN: AtomicU32 = AtomicU32::new(0);
    SEEN.store(0, Ordering::Relaxed);

    let processes = &sched::PROCESSES;
    let online = sched::online_cpus() as u32;
    let target = online - 1;
    let task = Box::new(|| {
        for _ in 0..50 {
            let flags = Port::irq_save_and_disable();
            SEEN.fetch_or(1 << Port::cpu_index(), Ordering::Relaxed);
            Port::irq_restore(flags);
            Port::services().spin_ms(5);
        }
        0
    });
    let Ok(pid) = processes.spawn_task("ktest-pin", SpawnIo::default(), task) else {
        return 1;
    };
    let pinned = processes.set_affinity(pid, 1 << target);
    // It may have started elsewhere the moment before it was pinned, and moves at
    // that CPU's next tick. From then on it should be seen nowhere else.
    processes.sleep_ms(50);
    SEEN.store(0, Ordering::Relaxed);
    // Every CPU busy, so it would move if it were free to.
    let others = spawn_workers(online as usize, "ktest-load", |_| spin(150));
    processes.wait(pid);
    let seen = SEEN.load(Ordering::Relaxed);
    let refused = !processes.set_affinity(processes.current(), 0);
    let ok = pinned && others.is_some() && refused && seen == 1 << target;
    println!("[ktest] affinity: target={target} seen={seen:#x} ok={ok}");
    i32::from(!ok)
}

/// Sleeps wake close to when they asked to, not on some tick, and a CPU with nothing
/// to do doesn't take timer interrupts.
fn test_timer() -> i32 {
    let processes = &sched::PROCESSES;
    let online = sched::online_cpus();

    // Off the desktop's way, on the last CPU.
    let start = Port::services().ticks();
    let rounds = 200u64;
    for _ in 0..rounds {
        processes.sleep_ms(1);
    }
    let elapsed = Port::services().ticks() - start;
    let average_us = elapsed * 1_000_000 / Port::services().frequency() / rounds;

    let before: [u64; MAX_CPUS] = core::array::from_fn(sched::timer_interrupts);
    let wall = Port::services().ticks();
    processes.sleep_ms(1000);
    let wall_ms = (Port::services().ticks() - wall) * 1000 / Port::services().frequency();
    let mut fewest = u64::MAX;
    for (cpu, count) in before.iter().enumerate().take(online) {
        fewest = fewest.min(sched::timer_interrupts(cpu) - count);
    }

    // A periodic 100 Hz tick would make a 1 ms sleep take 10 ms, and give every CPU
    // about 100 interrupts in this second.
    let precise = average_us < 5000;
    let quiet = online == 1 || fewest < 50;
    println!(
        "[ktest] timer: sleep(1)={average_us}us quietest-cpu={fewest}/s wall={wall_ms}ms precise={precise} quiet={quiet}"
    );
    i32::from(!(precise && quiet))
}

/// Reports what's known of RAM, and runs frames through the allocator.
fn test_mem() -> i32 {
    let before = memory::free_bytes();
    let Some(run) = memory::alloc_frames(2048, 512) else {
        println!("[ktest] mem: can't allocate frames");
        return 1;
    };
    let aligned = run % (2 * 1024 * 1024) == 0;
    let words = 2048 * 4096 / 8;
    let base = run as *mut u64;
    for word in 0..words {
        // SAFETY: the run was just allocated, is mapped, and is nobody else's.
        unsafe { base.add(word).write_volatile(word as u64 ^ 0x5a5a) };
    }
    // SAFETY: as above.
    let intact = (0..words).all(|word| unsafe { base.add(word).read_volatile() } == word as u64 ^ 0x5a5a);
    let during = memory::free_bytes();
    memory::free_frames(run, 2048);
    let restored = memory::free_bytes() == before && during + 2048 * 4096 == before;
    println!(
        "[ktest] mem: total={}MiB frames-free={}MiB heap={}MiB aligned={aligned} intact={intact} restored={restored}",
        memory::total_bytes() / (1024 * 1024),
        before / (1024 * 1024),
        crate::allocator::stats().total_bytes / (1024 * 1024),
    );
    i32::from(!(aligned && intact && restored))
}

/// Lists the device interrupts and how many of each have arrived.
fn test_irqs() -> i32 {
    feos_irq::TABLE.for_each(|row| {
        println!("[ktest] irq {:4} {:20} count={}", row.id, row.name, row.count);
    });
    println!("[ktest] irqs: unclaimed={}", feos_irq::TABLE.unclaimed());
    0
}

/// Starts and ends user programs over and over, in every way one can end, and says
/// whether what they used came back: page tables and pages, pipes and heap.
fn test_uspawn(rounds: usize) -> i32 {
    use feos_app::ArgBlock;

    use crate::sched::RawStreams;

    let spawn = |argv: &[&str]| {
        let mut block = ArgBlock::new();
        if !block.push_all(argv) {
            return None;
        }
        sched::spawn_user(argv[0], block, "/", SpawnIo::default(), RawStreams::default()).ok()
    };
    let wait_for_frames = |target: usize| {
        for _ in 0..1000 {
            if memory::free_bytes() >= target {
                return;
            }
            sched::sleep_ms(1);
        }
    };

    // Whatever else is running settles first, and is counted the same way after.
    sched::sleep_ms(200);
    let frames_before = memory::free_bytes();
    let heap_before = crate::allocator::stats().used_bytes;
    let pipes_before = feos_app::pipe::in_use();

    let mut exited = 0;
    let mut faulted = 0;
    let mut killed = 0;
    let mut failed = 0;
    for round in 0..rounds {
        match round % 16 {
            // A fault with the page tables at their most used. The report of it each
            // time would be a lot of output, so most rounds exit normally.
            7 => {
                let Some(pid) = spawn(&["/bin/malicious", "--probe", "null-write"]) else {
                    failed += 1;
                    continue;
                };
                if sched::PROCESSES.wait(pid) == Some(feos_abi::EXIT_FAULTED) {
                    faulted += 1;
                } else {
                    failed += 1;
                }
            }
            // A kill, at whatever point it lands.
            11 => {
                let Some(pid) = spawn(&["/bin/spin", "3", "0"]) else {
                    failed += 1;
                    continue;
                };
                sched::sleep_ms(1 + (round as u64 % 5));
                let _ = sched::PROCESSES.try_kill(pid);
                let _ = sched::PROCESSES.wait(pid);
                killed += 1;
            }
            // A program that isn't one: what it had built by then goes back too.
            13 => {
                let Some(pid) = spawn(&["/no/such/program"]) else {
                    failed += 1;
                    continue;
                };
                if sched::PROCESSES.wait(pid) != Some(feos_abi::EXIT_NOT_FOUND) {
                    failed += 1;
                }
            }
            _ => {
                let Some(pid) = spawn(&["/bin/hello", "--quiet", "--exit", "9"]) else {
                    failed += 1;
                    continue;
                };
                if sched::PROCESSES.wait(pid) == Some(9) {
                    exited += 1;
                } else {
                    failed += 1;
                }
            }
        }
    }

    wait_for_frames(frames_before);
    let frames_after = memory::free_bytes();
    let heap_after = crate::allocator::stats().used_bytes;
    let leaked_frames = frames_before.saturating_sub(frames_after) / 4096;
    let leaked_heap = heap_after.saturating_sub(heap_before);
    let pipes_after = feos_app::pipe::in_use();
    println!(
        "[ktest] uspawn: rounds={rounds} exited={exited} faulted={faulted} killed={killed} failed={failed} \
         frames-free={frames_before}->{frames_after} leaked-frames={leaked_frames} \
         leaked-heap={leaked_heap} pipes={pipes_before}->{pipes_after}"
    );
    i32::from(failed != 0 || leaked_frames != 0 || pipes_after > pipes_before)
}

/// Starts a user program, with `stdin` as its standard input if given.
fn spawn_program(argv: &[&str], stdin: Option<&feos_app::pipe::PipeReader>) -> Option<u32> {
    let mut block = feos_app::ArgBlock::new();
    if !block.push_all(argv) {
        return None;
    }
    let io = SpawnIo { stdin, ..SpawnIo::default() };
    sched::spawn_user(argv[0], block, "/", io, sched::RawStreams::default()).ok()
}

/// Waits up to a few seconds for the free frames to be back to `target`, as what's being
/// given back is given back with interrupts on, by tasks that have to be scheduled.
fn wait_for_frames(target: usize) {
    for _ in 0..3000 {
        if memory::free_bytes() >= target {
            return;
        }
        sched::sleep_ms(1);
    }
}

/// Waits up to two seconds for `pid` to be blocked: in a call, waiting.
fn wait_until_blocked(pid: u32) -> bool {
    for _ in 0..2000 {
        if sched::PROCESSES.detail(pid).is_some_and(|detail| detail.blocked) {
            return true;
        }
        sched::sleep_ms(1);
    }
    false
}

/// What's in use now: free frames, heap and pipes.
fn baseline() -> (usize, usize, usize) {
    sched::sleep_ms(200);
    (memory::free_bytes(), crate::allocator::stats().used_bytes, feos_app::pipe::in_use())
}

/// Whether what was in use at `before` is again: every frame back, the heap within what
/// caches may keep, and no more pipes.
fn back_to(before: (usize, usize, usize), label: &str) -> bool {
    wait_for_frames(before.0);
    let frames = memory::free_bytes();
    let heap = crate::allocator::stats().used_bytes;
    let pipes = feos_app::pipe::in_use();
    let leaked_frames = before.0.saturating_sub(frames) / 4096;
    let leaked_heap = heap.saturating_sub(before.1);
    println!(
        "[ktest] {label}: frames-free={}->{frames} leaked-frames={leaked_frames} leaked-heap={leaked_heap} pipes={}->{pipes}",
        before.0, before.2
    );
    leaked_frames == 0 && leaked_heap <= 64 * 1024 && pipes <= before.2
}

/// User programs killed in the middle of system calls and faults: each call unwinds
/// and its task ends itself, and what it held comes back. The kernel is the killer
/// here because a program can't kill anything until stage 1c.8 gives it a call to.
fn test_ukill() -> i32 {
    let processes = &sched::PROCESSES;
    let mut failed = 0;
    let mut verdict = |name: &str, good: bool, detail: core::fmt::Arguments<'_>| {
        println!("[ktest] ukill {name}: {} ({detail})", if good { "ok" } else { "FAILED" });
        failed += usize::from(!good);
    };

    // Blocked in a read of a pipe: killed, the read gives up, and the pipe's last
    // reader goes with the task, so the writer sees a broken pipe.
    let before = baseline();
    match feos_app::pipe::create() {
        Some((reader, writer)) => {
            let pid = spawn_program(&["/bin/utest", "--block-read"], Some(&reader));
            drop(reader);
            let blocked = pid.is_some_and(wait_until_blocked);
            let status = pid.and_then(|pid| {
                let _ = processes.try_kill(pid);
                processes.wait(pid)
            });
            let broken = !writer.write(b"x");
            drop(writer);
            let good = blocked && status == Some(feos_app::EXIT_KILLED) && broken;
            verdict(
                "kill-during-blocked-read",
                good && back_to(before, "ukill kill-during-blocked-read"),
                format_args!("blocked={blocked} status={status:?} epipe={broken}"),
            );
        }
        None => verdict("kill-during-blocked-read", false, format_args!("no pipe")),
    }

    // A MiB per call to a file: the kill lands in one, or between.
    let before = baseline();
    let path = "/tmp/ktest-long-write";
    let pid = spawn_program(&["/bin/utest", "--long-write", path, "0"], None);
    processes.sleep_ms(500);
    let status = pid.and_then(|pid| {
        let _ = processes.try_kill(pid);
        processes.wait(pid)
    });
    if let Some(tree) = sched::platform().filesystem.and_then(|filesystem| filesystem.tree()) {
        let _ = tree.remove_file(path);
    }
    verdict(
        "kill-during-long-write",
        status == Some(feos_app::EXIT_KILLED) && back_to(before, "ukill kill-during-long-write"),
        format_args!("status={status:?}"),
    );

    // A child forever spawning, writing and reading, killed and started again.
    let before = baseline();
    let mut killed = 0;
    let mut odd = 0;
    for round in 0..200u64 {
        let Some(pid) = spawn_program(&["/bin/malicious", "--call-loop"], None) else {
            odd += 1;
            continue;
        };
        processes.sleep_ms(1 + round % 7);
        let _ = processes.try_kill(pid);
        match processes.wait(pid) {
            Some(feos_app::EXIT_KILLED) => killed += 1,
            _ => odd += 1,
        }
    }
    // Its own children end by themselves, killed parent or not.
    processes.sleep_ms(500);
    verdict(
        "kill-mid-call-storm",
        killed == 200 && back_to(before, "ukill kill-mid-call-storm"),
        format_args!("killed={killed} other={odd}"),
    );

    // A child taking page faults as fast as it can still dies promptly when killed.
    let before = baseline();
    let mut slowest = 0;
    let mut killed = 0;
    for _ in 0..3 {
        let Some(pid) = spawn_program(&["/bin/malicious", "--fault-loop"], None) else {
            continue;
        };
        processes.sleep_ms(700);
        let start = sched::CLOCK.uptime_ms();
        let _ = processes.try_kill(pid);
        if processes.wait(pid) == Some(feos_app::EXIT_KILLED) {
            killed += 1;
        }
        slowest = slowest.max(sched::CLOCK.uptime_ms() - start);
    }
    verdict(
        "fault-storm",
        killed == 3 && slowest < 2000 && back_to(before, "ukill fault-storm"),
        format_args!("killed={killed} slowest-kill={slowest}ms"),
    );

    // Killed by another task while it's being spawned, before its PID is even handed
    // back: the kill is kept, and the program ends at its first boundary as killed,
    // never seeing the kill as an error from a call.
    let before = baseline();
    let (killed, early, odd) = kill_while_spawning();
    verdict(
        "kill-while-spawning",
        killed == KILL_WHILE_SPAWNING_ROUNDS && back_to(before, "ukill kill-while-spawning"),
        format_args!("killed={killed} during-spawn={early} other={odd}"),
    );

    println!("[ktest] ukill: {failed} failed");
    i32::from(failed != 0)
}

/// Spawns a program blocking on a pipe over and over, while a kernel task tries to
/// kill each one by the PID it's about to get. Says how many ended killed, how many of
/// the kills landed before the spawn had returned, and how many went otherwise.
fn kill_while_spawning() -> (usize, usize, usize) {
    use core::sync::atomic::AtomicBool;

    static TARGET: AtomicU32 = AtomicU32::new(0);
    static SPAWNED: AtomicBool = AtomicBool::new(false);
    static STOP: AtomicBool = AtomicBool::new(false);
    static EARLY: AtomicUsize = AtomicUsize::new(0);
    TARGET.store(0, Ordering::Release);
    STOP.store(false, Ordering::Release);
    EARLY.store(0, Ordering::Relaxed);
    let mut killed = 0;
    let mut odd = 0;

    let processes = &sched::PROCESSES;
    let Some((reader, writer)) = feos_app::pipe::create() else {
        return (0, 0, 1);
    };
    // It spins rather than sleeps, so on another CPU its kill can land while the
    // spawner is painting the new task's stack.
    let killer = Box::new(|| {
        while !STOP.load(Ordering::Acquire) {
            let target = TARGET.load(Ordering::Acquire);
            if target == 0 || sched::PROCESSES.try_kill(target) == feos_app::KillResult::NotFound {
                core::hint::spin_loop();
                continue;
            }
            if !SPAWNED.load(Ordering::Acquire) {
                EARLY.fetch_add(1, Ordering::Relaxed);
            }
            let _ = TARGET.compare_exchange(target, 0, Ordering::AcqRel, Ordering::Acquire);
        }
        0
    });
    let Ok(killer_pid) = processes.spawn_task("ktest-killer", SpawnIo::default(), killer) else {
        return (0, 0, 1);
    };
    // PIDs are handed out in order, and nothing else is spawning.
    let mut next = killer_pid.wrapping_add(1).max(2);
    for _ in 0..KILL_WHILE_SPAWNING_ROUNDS {
        SPAWNED.store(false, Ordering::Release);
        TARGET.store(next, Ordering::Release);
        let pid = spawn_program(&["/bin/utest", "--block-read"], Some(&reader));
        SPAWNED.store(true, Ordering::Release);
        let Some(pid) = pid else {
            TARGET.store(0, Ordering::Release);
            odd += 1;
            continue;
        };
        if pid != next {
            // Something else took the PID: kill this one here instead.
            TARGET.store(0, Ordering::Release);
            let _ = processes.try_kill(pid);
        }
        // Killed in its spawn, its load or its read, it must end killed: a read that
        // gave up with an error would let it end by itself, with status 0.
        match processes.wait(pid) {
            Some(feos_app::EXIT_KILLED) if pid == next => killed += 1,
            _ => odd += 1,
        }
        next = pid.wrapping_add(1).max(2);
    }
    STOP.store(true, Ordering::Release);
    processes.wait(killer_pid);
    drop(reader);
    drop(writer);
    (killed, EARLY.load(Ordering::Relaxed), odd)
}

const KILL_WHILE_SPAWNING_ROUNDS: usize = 50;

/// Kills 100 user programs in the middle of system calls, and 20 more at EL0 or ring 3
/// beside them for the reaper: every teardown runs in task context with interrupts on,
/// and everything comes back.
fn test_teardown() -> i32 {
    const BATCHES: usize = 10;
    const IN_CALL: usize = 10;
    const IN_USER: usize = 2;
    let processes = &sched::PROCESSES;
    let before = baseline();
    let (own_before, queued_before, unsafe_before) = sched::teardown_counts();
    let mut mid_call = 0;
    let mut at_user = 0;
    let mut odd = 0;
    for _ in 0..BATCHES {
        let Some((reader, writer)) = feos_app::pipe::create() else {
            odd += IN_CALL + IN_USER;
            continue;
        };
        // Each with whether it's to be killed in a call, and whether it was.
        let mut pids = [(0u32, false, false); IN_CALL + IN_USER];
        let mut count = 0;
        for index in 0..pids.len() {
            let call = index < IN_CALL;
            let argv: &[&str] = if call { &["/bin/utest", "--block-read"] } else { &["/bin/spin", "5", "0"] };
            match spawn_program(argv, Some(&reader)) {
                Some(pid) => {
                    pids[count] = (pid, call, false);
                    count += 1;
                }
                None => odd += 1,
            }
        }
        drop(reader);
        for (pid, call, blocked) in &mut pids[..count] {
            *blocked = *call && wait_until_blocked(*pid);
        }
        for (pid, ..) in &pids[..count] {
            let _ = processes.try_kill(*pid);
        }
        for (pid, call, blocked) in &pids[..count] {
            match (processes.wait(*pid), *call, *blocked) {
                (Some(feos_app::EXIT_KILLED), true, true) => mid_call += 1,
                (Some(feos_app::EXIT_KILLED), false, _) => at_user += 1,
                _ => odd += 1,
            }
        }
        drop(writer);
    }
    let killed = mid_call + at_user;
    let (own_after, queued_after, unsafe_after) = sched::teardown_counts();
    let own = own_after - own_before;
    let queued = queued_after - queued_before;
    let wrong = unsafe_after - unsafe_before;
    let whole = back_to(before, "teardown");
    let all = BATCHES * (IN_CALL + IN_USER);
    let good = mid_call == BATCHES * IN_CALL
        && at_user == BATCHES * IN_USER
        && own + queued >= all
        && wrong == 0
        && whole;
    println!(
        "[ktest] teardown: killed={killed} mid-call={mid_call} at-user={at_user} other={odd} by-self={own} \
         by-queue={queued} outside-task-context={wrong} ok={good}"
    );
    i32::from(!good)
}

/// Says what the frames and memory look like right now.
fn test_user_memory() -> i32 {
    println!(
        "[ktest] frames-free={} heap-used={} pipes={}",
        memory::free_bytes(),
        crate::allocator::stats().used_bytes,
        feos_app::pipe::in_use()
    );
    0
}

struct KernelTest;

impl App for KernelTest {
    fn name(&self) -> &'static str {
        "ktest"
    }

    fn usage(&self) -> &'static str {
        "ktest <uspawn [n]|ukill|teardown|user-memory|cpus|fp|irqs|mem|priority|affinity|timer|panic|overflow|overflow-locked|null|task-panic>"
    }

    fn description(&self) -> &'static str {
        "kernel self-tests, some of which stop the machine"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        match context.args.trim() {
            "user-memory" => test_user_memory(),
            "ukill" => test_ukill(),
            "teardown" => test_teardown(),
            uspawn if uspawn.starts_with("uspawn") => {
                let rounds = uspawn["uspawn".len()..].trim().parse().unwrap_or(320);
                test_uspawn(rounds)
            }
            "cpus" => test_cpus(),
            "fp" => test_fp(),
            "irqs" => test_irqs(),
            "usb" => test_usb(),
            "input" => test_input(context),
            "mem" => test_mem(),
            "priority" => test_priority(),
            "affinity" => test_affinity(),
            "timer" => test_timer(),
            "panic" => test_panic(),
            "overflow" | "overflow-locked" | "null" => test_failure(context.args.trim()),
            "task-panic" => test_failure("panic"),
            _ => {
                println!("usage: {}", self.usage());
                2
            }
        }
    }
}
