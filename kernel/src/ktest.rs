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
            if free_bytes_settled() >= target {
                return;
            }
            sched::sleep_ms(1);
        }
    };

    // Whatever else is running settles first, and is counted the same way after.
    sched::sleep_ms(200);
    let frames_before = free_bytes_settled();
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
    let frames_after = free_bytes_settled();
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

/// Free bytes, once the page cache has let go of every page nothing maps and the stacks
/// of tasks that have gone are back: what's left in use is what's in use, whatever files
/// were read lately and whether the reaper has had its turn.
fn free_bytes_settled() -> usize {
    crate::pagecache::drop_idle();
    sched::trim_stacks();
    memory::free_bytes()
}

/// Waits up to a few seconds for the free frames to be back to `target`, as what's being
/// given back is given back with interrupts on, by tasks that have to be scheduled.
fn wait_for_frames(target: usize) {
    for _ in 0..3000 {
        if free_bytes_settled() >= target {
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
    (free_bytes_settled(), crate::allocator::stats().used_bytes, feos_app::pipe::in_use())
}

/// Whether what was in use at `before` is again: every frame back, the heap within what
/// caches may keep, and no more pipes.
fn back_to(before: (usize, usize, usize), label: &str) -> bool {
    wait_for_frames(before.0);
    let frames = free_bytes_settled();
    let heap = crate::allocator::stats().used_bytes;
    let pipes = feos_app::pipe::in_use();
    let leaked_frames = before.0.saturating_sub(frames) / 4096;
    let leaked_heap = heap.saturating_sub(before.1);
    println!(
        "[ktest] {label}: frames-free={}->{frames} leaked-frames={leaked_frames} leaked-heap={leaked_heap} pipes={}->{pipes}",
        before.0, before.2
    );
    let good = leaked_frames == 0 && leaked_heap <= 64 * 1024 && pipes <= before.2;
    if !good {
        // Where the frames are, to tell a leak from something still being given back.
        let (cached, idle) = crate::pagecache::counts();
        println!(
            "[ktest] {label}: cache-pages={cached} cache-idle={idle} cache-dirty={} stack-bytes={} promised={} orphans={}",
            crate::pagecache::dirty(),
            crate::kstack::bytes(),
            crate::user::promised(),
            crate::user::orphans()
        );
        sched::sleep_ms(5000);
        println!("[ktest] {label}: 5 s later frames-free={}", free_bytes_settled());
    }
    good
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

/// The threads the process `pid` has, as the process table counts them.
fn threads_of(pid: u32) -> usize {
    feos_app::ProcessSnapshot::take(&sched::PROCESSES)
        .and_then(|processes| processes.find(pid).map(|process| usize::from(process.threads)))
        .unwrap_or(0)
}

/// Runs a user program with its output in a pipe, printing only its last line and any
/// that says something failed, and returns its status.
fn run_quietly(argv: &[&str]) -> Option<i32> {
    let (out_reader, out_writer) = feos_app::pipe::create()?;
    let io = SpawnIo { stdout: Some(&out_writer), stderr: Some(&out_writer), ..SpawnIo::default() };
    let mut block = feos_app::ArgBlock::new();
    if !block.push_all(argv) {
        return None;
    }
    let pid = sched::spawn_user(argv[0], block, "/", io, sched::RawStreams::default()).ok();
    drop(out_writer);
    let pid = pid?;
    let mut line = [0u8; 160];
    let mut length = 0;
    let mut last = [0u8; 160];
    let mut last_length = 0;
    let mut chunk = [0u8; 256];
    // Until the program has ended, not until its output does: a child of it may hold the
    // pipe for longer.
    let mut ended = None;
    loop {
        let got = match out_reader.try_read(&mut chunk) {
            feos_app::pipe::Transfer::Moved(got) => got,
            feos_app::pipe::Transfer::Closed => break,
            feos_app::pipe::Transfer::WouldBlock => {
                if ended.is_some() {
                    break;
                }
                if let feos_app::WaitStatus::Exited(status) = sched::PROCESSES.try_wait(pid) {
                    ended = Some(status);
                    continue;
                }
                sched::sleep_ms(5);
                continue;
            }
        };
        for &byte in &chunk[..got] {
            if byte != b'\n' {
                if length < line.len() {
                    line[length] = byte;
                    length += 1;
                }
                continue;
            }
            let text = core::str::from_utf8(&line[..length]).unwrap_or("?");
            if text.contains("FAILED") {
                println!("{text}");
            }
            last = line;
            last_length = length;
            length = 0;
        }
    }
    let status = ended.or_else(|| sched::PROCESSES.wait(pid));
    println!("[ktest] kstack: {}: {}", argv[0], core::str::from_utf8(&last[..last_length]).unwrap_or("?"));
    status
}

/// User threads' kernel stacks are 64 KiB: this drives the deepest calls and faults
/// there are (every call `utest` makes, file mappings and write-back on the filesystem
/// at `dir`, which on the native one goes through its journal, and threads by the
/// hundred), then checks that no thread used more than half its stack. Every stack is
/// painted when it's taken and measured as it's given back.
fn test_kstack(dir: &str) -> i32 {
    sched::trim_stacks();
    crate::kstack::reset_peaks();
    println!("[ktest] kstack: driving utest, mtest on {dir} and ttest");
    let programs: [&[&str]; 3] = [&["/bin/utest"], &["/bin/mtest", "--dir", dir], &["/bin/ttest"]];
    let mut failed = 0;
    for argv in programs {
        if run_quietly(argv) != Some(0) {
            println!("[ktest] kstack: {} failed", argv[0]);
            failed += 1;
        }
    }
    sched::trim_stacks();
    let (user, kernel) = crate::kstack::peaks();
    let half = crate::kstack::USER_STACK / 2;
    let ok = failed == 0 && user > 0 && user <= half;
    println!(
        "[ktest] kstack: user-peak={user} of {} (half {half}) kernel-peak={kernel} of {} dir={dir} \
         stacks-held={} failed={failed} ok={ok}",
        crate::kstack::USER_STACK,
        crate::kstack::KERNEL_STACK,
        crate::kstack::bytes()
    );
    i32::from(!ok)
}

/// Kernel apps killed over and over while they take stacks (spawning) and read others'
/// (as `ps` does): a kill lands only once they're done, so no stack is left `TRIMMING`,
/// no reader is left counted, and no slot is left reserved. `trim_stacks` returning at
/// all says the first; the counts and a spawn after say the rest.
fn test_kstack_kill() -> i32 {
    const CHURNERS: usize = 6;
    const ROUNDS: usize = 40;
    let processes = &sched::PROCESSES;
    sched::trim_stacks();
    let before = crate::kstack::bytes();
    let mut killed = 0;
    let mut odd = 0;
    let mut seed = CLOCK_SEED.fetch_add(0x9e37_79b9, Ordering::Relaxed) | 1;
    for _ in 0..ROUNDS {
        let mut pids = [0u32; CHURNERS];
        for pid in pids.iter_mut() {
            let task = Box::new(|| -> i32 {
                loop {
                    let child = Box::new(|| 0);
                    let _ = sched::PROCESSES.spawn_task("ktest-kchild", SpawnIo::default(), child);
                    for slot in 1..64 {
                        core::hint::black_box(crate::kstack::high_water(slot));
                    }
                }
            });
            match processes.spawn_task("ktest-churn", SpawnIo::default(), task) {
                Ok(spawned) => *pid = spawned,
                Err(_) => odd += 1,
            }
        }
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        sched::sleep_ms(1 + seed % 15);
        for &pid in pids.iter().filter(|&&pid| pid != 0) {
            let _ = processes.try_kill(pid);
            match processes.wait(pid) {
                Some(feos_app::EXIT_KILLED) => killed += 1,
                _ => odd += 1,
            }
        }
    }
    // Its children end by themselves: give them a moment to be collected.
    sched::sleep_ms(200);
    sched::trim_stacks();
    let (readers, trimming) = crate::kstack::in_flight();
    let after = crate::kstack::bytes();
    let spawned = processes.spawn_task("ktest-after", SpawnIo::default(), Box::new(|| 7));
    let after_status = spawned.ok().and_then(|pid| processes.wait(pid));
    let ok = odd == 0
        && killed == ROUNDS * CHURNERS
        && readers == 0
        && trimming == 0
        && after <= before
        && after_status == Some(7);
    println!(
        "[ktest] kstack-kill: killed={killed} odd={odd} readers={readers} trimming={trimming} \
         stacks={before}->{after} spawn-after={after_status:?} ok={ok}"
    );
    i32::from(!ok)
}

static CLOCK_SEED: AtomicU64 = AtomicU64::new(0x2545_f491_4f6c_dd1d);

/// A fault in the kernel, in the middle of one thread's call, ends that thread's whole
/// process with status 139: its other threads, all waiting in calls, are killed through
/// the cooperative path, and everything the process had comes back.
fn test_thread_fault() -> i32 {
    let processes = &sched::PROCESSES;
    let before = baseline();
    let Some((reader, writer)) = feos_app::pipe::create() else {
        println!("[ktest] thread-fault: no pipe");
        return 1;
    };
    let pid = spawn_program(&["/bin/ttest", "--fault-victim"], Some(&reader));
    drop(reader);
    let Some(pid) = pid else {
        println!("[ktest] thread-fault: /bin/ttest didn't start");
        return 1;
    };
    // Every thread started, and each waiting.
    let mut threads = 0;
    for _ in 0..2000 {
        threads = threads_of(pid);
        if threads == 4 {
            break;
        }
        sched::sleep_ms(1);
    }
    sched::sleep_ms(50);
    sched::arm_thread_fault(pid);
    let _ = writer.write(b"x");
    let status = processes.wait(pid);
    drop(writer);
    sched::arm_thread_fault(0);
    let left = threads_of(pid);
    let whole = back_to(before, "thread-fault");
    let good = threads == 4 && status == Some(139) && left == 0 && whole;
    println!("[ktest] thread-fault: threads={threads} status={status:?} left={left} ok={good}");
    i32::from(!good)
}

/// A line a program wrote to `reader`, waiting up to `ms` for it.
fn read_line<'a>(reader: &feos_app::pipe::PipeReader, line: &'a mut [u8; 64], ms: u64) -> &'a str {
    let mut length = 0;
    let mut byte = [0u8; 1];
    for _ in 0..ms {
        loop {
            match reader.try_read(&mut byte) {
                feos_app::pipe::Transfer::Moved(_) if byte[0] == b'\n' => {
                    return core::str::from_utf8(&line[..length]).unwrap_or("");
                }
                feos_app::pipe::Transfer::Moved(_) => {
                    if length < line.len() {
                        line[length] = byte[0];
                        length += 1;
                    }
                }
                feos_app::pipe::Transfer::Closed => return core::str::from_utf8(&line[..length]).unwrap_or(""),
                feos_app::pipe::Transfer::WouldBlock => break,
            }
        }
        sched::sleep_ms(1);
    }
    core::str::from_utf8(&line[..length]).unwrap_or("")
}

/// Exit records (stage 1c.8). A thread refused its kernel stack leaves its process's
/// record in no slot, for the next task there to take as its own. And once the pid
/// counter has wrapped, which `ptest` can't wait for, an unreaped child's pid and the
/// pid of a group whose leader has gone but which still has a member are given to
/// nothing new until they're let go.
fn test_records() -> i32 {
    let processes = &sched::PROCESSES;
    let mut failed = 0;
    let mut verdict = |name: &str, good: bool, detail: core::fmt::Arguments<'_>| {
        println!("[ktest] records {name}: {} ({detail})", if good { "ok" } else { "FAILED" });
        failed += usize::from(!good);
    };

    // The refused thread's process is a user program's child, so it has a record; the
    // programs after it are the kernel's, with none, in whatever slots are free.
    let before = baseline();
    sched::fail_next_thread_stack(true);
    let status = spawn_program(&["/bin/ptest", "--refused-thread-parent"], None).and_then(|pid| processes.wait(pid));
    let armed = sched::fail_next_thread_stack(false);
    let stray = sched::stray_records();
    let mut others = 0;
    for _ in 0..8 {
        let pid = spawn_program(&["/bin/hello", "--quiet"], None);
        others += usize::from(pid.and_then(|pid| processes.wait(pid)) == Some(0));
    }
    let stray_after = sched::stray_records();
    let whole = back_to(before, "records");
    verdict(
        "refused-thread",
        status == Some(0) && !armed && stray == 0 && stray_after == 0 && others == 8 && whole,
        format_args!("status {status:?}, still armed {armed}, stray records {stray} then {stray_after}, others {others}/8"),
    );

    // An unreaped child and a group with a member but no leader, from a program that
    // says their pids and reaps the child when told.
    let (Some((in_reader, in_writer)), Some((out_reader, out_writer))) = (feos_app::pipe::create(), feos_app::pipe::create()) else {
        println!("[ktest] records: no pipe");
        return 1;
    };
    let mut block = feos_app::ArgBlock::new();
    let _ = block.push_all(&["/bin/ptest", "--hold-pids"]);
    let io = SpawnIo { stdin: Some(&in_reader), stdout: Some(&out_writer), ..SpawnIo::default() };
    let holder = sched::spawn_user("/bin/ptest", block, "/", io, sched::RawStreams::default()).ok();
    drop(in_reader);
    drop(out_writer);
    let mut line = [0u8; 64];
    let said = read_line(&out_reader, &mut line, 5000);
    let mut words = said.strip_prefix("hold ").unwrap_or("").split(' ').map(|word| word.parse::<u32>().ok());
    let (Some(Some(zombie)), Some(Some(leader))) = (words.next(), words.next()) else {
        println!("[ktest] records: --hold-pids said {said:?}");
        drop(in_writer);
        let _ = holder.map(|pid| processes.wait(pid));
        return 1;
    };
    // What a program is given with the counter at `at`, wrapped: the first pid from there
    // that nothing holds.
    let given = |at: u32| {
        sched::swap_pid_counter(at, true);
        let pid = spawn_program(&["/bin/hello", "--quiet"], None)?;
        processes.wait(pid);
        Some(pid)
    };
    // What may take a moment to be let go (a record drops as its last reference does, a
    // member ends as it reads its input's end) is asked for again until it is.
    let given_soon = |at: u32| {
        let mut pid = None;
        for _ in 0..200 {
            pid = given(at);
            if pid == Some(at) {
                break;
            }
            sched::sleep_ms(10);
        }
        pid
    };
    let saved = sched::swap_pid_counter(zombie, true);
    let past_zombie = given(zombie);
    let past_leader = given(leader);
    let _ = in_writer.write(b"x");
    let reaped = read_line(&out_reader, &mut line, 5000) == "reaped true";
    let zombie_again = given_soon(zombie);
    drop(in_writer);
    let holder_status = holder.and_then(|pid| processes.wait(pid));
    let leader_again = given_soon(leader);
    // Back where it was, unwrapped: every pid given meanwhile is below it.
    sched::swap_pid_counter(saved.0, saved.1);
    verdict(
        "pid-held-after-wrap",
        past_zombie.is_some_and(|pid| pid != zombie && pid != leader)
            && past_leader.is_some_and(|pid| pid != zombie && pid != leader)
            && reaped
            && zombie_again == Some(zombie)
            && holder_status == Some(0)
            && leader_again == Some(leader),
        format_args!(
            "zombie {zombie}: past it {past_zombie:?}, reaped {reaped}, then {zombie_again:?}; group {leader}: past it {past_leader:?}, then {leader_again:?}; holder {holder_status:?}"
        ),
    );
    println!("[ktest] records: ok={}", failed == 0);
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

/// Most words `ktest drive` passes on to its program.
const DRIVE_WORDS: usize = 16;

/// Runs a user program with its standard streams on pipes, passes on what it prints,
/// and answers each `?frames` line it prints with the free frame count, which a program
/// can't see for itself until `SYSINFO`. The program is told with `--frames`. Its status
/// is the result.
fn test_drive(words: &str) -> i32 {
    let mut argv = [""; DRIVE_WORDS + 1];
    let mut count = 0;
    for word in words.split_whitespace().take(DRIVE_WORDS) {
        argv[count] = word;
        count += 1;
    }
    if count == 0 {
        println!("usage: ktest drive PROGRAM [ARGS...]");
        return 2;
    }
    argv[count] = "--frames";
    let mut block = feos_app::ArgBlock::new();
    if !block.push_all(&argv[..=count]) {
        println!("[ktest] drive: too many arguments");
        return 2;
    }
    let (Some((stdin_reader, stdin_writer)), Some((out_reader, out_writer))) =
        (feos_app::pipe::create(), feos_app::pipe::create())
    else {
        println!("[ktest] drive: no pipes");
        return 1;
    };
    let io = SpawnIo {
        stdin: Some(&stdin_reader),
        stdout: Some(&out_writer),
        stderr: Some(&out_writer),
        ..SpawnIo::default()
    };
    let pid = sched::spawn_user(argv[0], block, "/", io, sched::RawStreams::default());
    // The program and its children have the other ends now: the output ends when the
    // last of them goes.
    drop(stdin_reader);
    drop(out_writer);
    let Ok(pid) = pid else {
        println!("[ktest] drive: {} didn't start", argv[0]);
        return 1;
    };
    let mut line = [0u8; 512];
    let mut length = 0;
    let mut chunk = [0u8; 256];
    let mut answered = 0usize;
    loop {
        let got = out_reader.read(&mut chunk);
        if got == 0 {
            break;
        }
        for &byte in &chunk[..got] {
            if byte != b'\n' && length < line.len() {
                line[length] = byte;
                length += 1;
                continue;
            }
            let text = core::str::from_utf8(&line[..length]).unwrap_or("?");
            if text == "?frames" {
                let mut number = [0u8; 24];
                let digits = decimal(free_bytes_settled() / 4096, &mut number);
                let _ = stdin_writer.write(digits);
                answered += 1;
            } else {
                println!("{text}");
            }
            length = 0;
        }
    }
    let status = sched::PROCESSES.wait(pid).unwrap_or(feos_app::EXIT_KILLED);
    println!("[ktest] drive: {} ended with status {status}, {answered} frame counts given", argv[0]);
    status
}

/// Writes `value` and a newline into `out`, and returns what was written.
fn decimal(mut value: usize, out: &mut [u8; 24]) -> &[u8] {
    let mut at = out.len() - 1;
    out[at] = b'\n';
    loop {
        at -= 1;
        out[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    &out[at..]
}

/// A program dirtying every page of a shared mapping of a `mib` MiB file at `path` as
/// fast as it can, killed by the kernel (a program can't kill until stage 1c.8): what it
/// dirtied is written back as its space goes, so the file holds, page by page, one whole
/// pass's number or the one before, never a torn page, and every frame comes back once
/// the file has gone.
fn test_dirty_storm(path: &str, mib: u64) -> i32 {
    let Some(tree) = sched::platform().filesystem.and_then(|filesystem| filesystem.tree()) else {
        println!("[ktest] dirty-storm: no filesystem");
        return 1;
    };
    let processes = &sched::PROCESSES;
    let size = mib << 20;
    let before = baseline();
    let _ = tree.remove_file(path);
    if let Err(error) = tree.create_file(path).and_then(|()| tree.truncate(path, size)) {
        println!("[ktest] dirty-storm: can't make {path}: {error:?}");
        return 1;
    }
    let pid = spawn_program(&["/bin/malicious", "--dirty-storm", path], None);
    // Killed once every page is dirty, so its first pass is over, and a while into the
    // next: however slow the filesystem is to fill the pages.
    let pages = (size / 4096) as usize;
    let mut waited = 0;
    while crate::pagecache::dirty() < pages && waited < 120_000 {
        processes.sleep_ms(10);
        waited += 10;
    }
    processes.sleep_ms(500);
    let start = sched::CLOCK.uptime_ms();
    let status = pid.and_then(|pid| {
        let _ = processes.try_kill(pid);
        processes.wait(pid)
    });
    let teardown = sched::CLOCK.uptime_ms() - start;
    let mut page = [0u8; 4096];
    let (mut low, mut high, mut torn) = (u64::MAX, 0u64, 0usize);
    let mut first_error = None;
    for index in 0..size / 4096 {
        let got = tree.read_at(path, index * 4096, &mut page);
        if got != Ok(4096) {
            first_error.get_or_insert((index, got));
            torn += 1;
            continue;
        }
        let mut word = [0u8; 8];
        word.copy_from_slice(&page[..8]);
        let pass = u64::from_ne_bytes(word);
        torn += usize::from(page[8..].iter().any(|&byte| byte != 0));
        low = low.min(pass);
        high = high.max(pass);
    }
    let _ = tree.remove_file(path);
    let whole = torn == 0 && low >= 1 && high - low <= 1;
    let good = status == Some(feos_app::EXIT_KILLED) && whole && back_to(before, "dirty-storm frames");
    println!(
        "[ktest] dirty-storm: {mib} MiB status={status:?} passes={low}..={high} torn-pages={torn}          kill-and-write-back={teardown}ms first-bad-read={first_error:?} ok={good}"
    );
    i32::from(!good)
}

/// Says what the frames and memory look like right now, once the page cache has let go
/// of the pages nothing maps, and what it keeps.
fn test_user_memory() -> i32 {
    sched::trim_stacks();
    let dropped = crate::pagecache::drop_idle();
    let (cached, idle) = crate::pagecache::counts();
    println!(
        "[ktest] frames-free={} heap-used={} pipes={} promised={} cache-pages={cached} cache-idle={idle} cache-dropped={dropped} orphans={} locks={}",
        memory::free_bytes(),
        crate::allocator::stats().used_bytes,
        feos_app::pipe::in_use(),
        crate::user::promised(),
        crate::user::orphans(),
        crate::user::locks()
    );
    0
}

/// A program waiting in `F_SETLKW` for a lock another holds is killed (stage 1c.5): its
/// wait ends, it never gets the lock, and the holder's lock is all the table has; then
/// the holder is killed too, and the table is empty. The kernel kills, as no program can
/// until stage 1c.8.
fn test_setlkw_killed() -> i32 {
    let processes = &sched::PROCESSES;
    let path = "/tmp/ktest-setlkw";
    let before = baseline();
    let locks_before = crate::user::locks();
    let Some((reader, writer)) = feos_app::pipe::create() else {
        println!("[ktest] setlkw-killed: FAILED (no pipe)");
        return 1;
    };
    let holder = spawn_program(&["/bin/malicious", "--lock-hold", path], Some(&reader));
    let held = holder.is_some_and(wait_until_blocked);
    let held_locks = crate::user::locks();
    let waiter = if held { spawn_program(&["/bin/malicious", "--setlkw-wait", path], None) } else { None };
    let waiting = waiter.is_some_and(wait_until_blocked);
    // Long enough that a wrongly granted lock would have been had and reported.
    processes.sleep_ms(200);
    let waiting_locks = crate::user::locks();
    let waiter_status = waiter.and_then(|pid| {
        let _ = processes.try_kill(pid);
        processes.wait(pid)
    });
    let after_waiter = crate::user::locks();
    let holder_status = holder.and_then(|pid| {
        let _ = processes.try_kill(pid);
        processes.wait(pid)
    });
    drop(reader);
    drop(writer);
    let after_holder = crate::user::locks();
    if let Some(tree) = sched::platform().filesystem.and_then(|filesystem| filesystem.tree()) {
        let _ = tree.remove_file(path);
    }
    let good = held
        && waiting
        && held_locks == locks_before + 1
        && waiting_locks == held_locks
        && waiter_status == Some(feos_app::EXIT_KILLED)
        && after_waiter == held_locks
        && holder_status == Some(feos_app::EXIT_KILLED)
        && after_holder == locks_before
        && back_to(before, "setlkw-killed");
    println!(
        "[ktest] setlkw-killed: {} (held={held} waiting={waiting} locks={locks_before}->{held_locks}->{waiting_locks}->{after_waiter}->{after_holder} waiter={waiter_status:?} holder={holder_status:?})",
        if good { "ok" } else { "FAILED" }
    );
    i32::from(!good)
}

/// Where the TLB, ASID and futex tests map their page in a space of their own.
const TEST_PAGE: usize = feos_abi::USER_MMAP_BASE + 0x10_0000;

/// Waits up to `ms` for `stage` to reach `wanted`.
fn wait_stage(stage: &AtomicU32, wanted: u32, ms: u64) -> bool {
    let deadline = sched::CLOCK.uptime_ms() + ms;
    while stage.load(Ordering::Acquire) < wanted {
        if sched::CLOCK.uptime_ms() > deadline {
            return false;
        }
        sched::sleep_ms(1);
    }
    true
}

/// A page mapped in a space, read on CPU 1 so its TLB has it, unmapped from CPU 0 while
/// CPU 1 has interrupts masked: on a port whose flushes don't reach other CPUs the
/// unmap has to wait for CPU 1 to take the shootdown, and the frame isn't freed until
/// it has. Then the page is mapped again with another frame, and CPU 1 must read that,
/// not what a stale entry would give it.
fn test_tlb() -> i32 {
    static STAGE: AtomicU32 = AtomicU32::new(0);
    static FIRST: AtomicU64 = AtomicU64::new(0);
    static SECOND: AtomicU64 = AtomicU64::new(0);
    static HELD: AtomicU32 = AtomicU32::new(0);
    static ON_CPU: AtomicU32 = AtomicU32::new(u32::MAX);
    let processes = &sched::PROCESSES;
    if sched::online_cpus() < 2 {
        println!("[ktest] tlb: skipped=one-cpu ok=true");
        return 0;
    }
    let Some(space) = crate::user::TestSpace::new() else {
        println!("[ktest] tlb: no memory");
        return 1;
    };
    let Some(frame) = space.map_word(TEST_PAGE, 0x1111) else {
        println!("[ktest] tlb: can't map");
        return 1;
    };
    STAGE.store(0, Ordering::Release);
    HELD.store(0, Ordering::Release);
    let me = processes.current();
    processes.set_affinity(me, 1 << 0);
    processes.sleep_ms(20);
    let address = &space as *const crate::user::TestSpace as usize;
    let task = Box::new(move || {
        // On CPU 1 before anything: it was pinned there just after it started.
        processes.sleep_ms(50);
        // SAFETY: the test waits for this task before the space goes.
        let space = unsafe { &*(address as *const crate::user::TestSpace) };
        sched::borrow_tables(space.mm().root(), space.mm().tlb());
        let flags = Port::irq_save_and_disable();
        ON_CPU.store(Port::cpu_index() as u32, Ordering::Release);
        // SAFETY: mapped and readable in the tables just loaded.
        FIRST.store(unsafe { Port::read_user_word(TEST_PAGE) }, Ordering::Release);
        STAGE.store(1, Ordering::Release);
        // With interrupts masked, the shootdown can't be answered: the unmap waits, and
        // the frame stays.
        while STAGE.load(Ordering::Acquire) < 2 {
            core::hint::spin_loop();
        }
        let until = Port::services().ticks() + Port::services().frequency() / 5;
        let mut held = 1;
        while Port::services().ticks() < until {
            if memory::frame_refs(frame) == 0 {
                held = 0;
            }
        }
        HELD.store(held, Ordering::Release);
        Port::irq_restore(flags);
        while STAGE.load(Ordering::Acquire) < 3 {
            core::hint::spin_loop();
        }
        let flags = Port::irq_save_and_disable();
        // SAFETY: mapped again before stage 3.
        SECOND.store(unsafe { Port::read_user_word(TEST_PAGE) }, Ordering::Release);
        Port::irq_restore(flags);
        sched::return_tables();
        STAGE.store(4, Ordering::Release);
        0
    });
    let Ok(pid) = processes.spawn_task("ktest-tlb", SpawnIo::default(), task) else {
        return 1;
    };
    processes.set_affinity(pid, 1 << 1);
    let started = wait_stage(&STAGE, 1, 5000);
    let (shootdowns, held_back) = crate::user::tlb::shootdown_counts();
    let start = Port::services().ticks();
    STAGE.store(2, Ordering::Release);
    let unmapped = space.unmap(TEST_PAGE);
    let waited_ms = (Port::services().ticks() - start) * 1000 / Port::services().frequency();
    let freed = memory::frame_refs(frame) == 0;
    let (shootdowns_after, held_after) = crate::user::tlb::shootdown_counts();
    let remapped = space.map_word(TEST_PAGE, 0x2222).is_some();
    STAGE.store(3, Ordering::Release);
    let finished = wait_stage(&STAGE, 4, 5000);
    processes.wait(pid);
    processes.set_affinity(me, 0);
    drop(space);
    let (first, second) = (FIRST.load(Ordering::Acquire), SECOND.load(Ordering::Acquire));
    let held = HELD.load(Ordering::Acquire) == 1;
    let cpu = ON_CPU.load(Ordering::Acquire);
    let sent = shootdowns_after - shootdowns;
    // Where flushes are broadcast there's nothing to wait for, so nothing is held.
    let shot = if <Port as Arch>::USER_FLUSH_BROADCAST {
        sent == 0
    } else {
        sent >= 1 && held && held_after > held_back && waited_ms >= 150
    };
    let ok = started && finished && unmapped && remapped && cpu == 1 && first == 0x1111 && second == 0x2222 && freed && shot;
    println!(
        "[ktest] tlb: cpu={cpu} first={first:#x} second={second:#x} shootdowns={sent} held-back={} waited-ms={waited_ms} \
         held-until-ack={held} freed-after={freed} broadcast={} ok={ok}",
        held_after - held_back,
        <Port as Arch>::USER_FLUSH_BROADCAST
    );
    i32::from(!ok)
}

/// ASIDs under load: `count` user programs alive at once, more than 8-bit ASIDs can tell
/// apart, with ASIDs forced to 8 bits, each reading back words of its own over and over
/// while they all share every CPU, so tables are loaded on each all the time and the
/// generation rolls over again and again with programs running everywhere. They start
/// together, held by a pipe each reads until it ends. Every one must see only its own,
/// and say so on a pipe this reads to its end, which is when the last has gone (their
/// statuses can't tell: v1 keeps the last 64 exits' only). Everything they used must
/// come back.
fn test_asid_load(count: usize) -> i32 {
    let before = baseline();
    let (generation, bits) = crate::user::tlb::asid_state();
    crate::user::tlb::force_asid_bits(8);
    let mut started = 0;
    let mut good = 0;
    let pipes = feos_app::pipe::create().zip(feos_app::pipe::create());
    if let Some(((gate, open), (said, say))) = pipes {
        for _ in 0..count {
            let mut block = feos_app::ArgBlock::new();
            if !block.push_all(&["/bin/utest", "--asid-check", "200"]) {
                break;
            }
            let io = SpawnIo { stdin: Some(&gate), stdout: Some(&say), stderr: Some(&say), ..SpawnIo::default() };
            if sched::spawn_user("/bin/utest", block, "/", io, sched::RawStreams::default()).is_err() {
                break;
            }
            started += 1;
        }
        drop(gate);
        drop(say);
        // Every one of them holds the gate's reader: closing its writer starts them all.
        drop(open);
        // Each says `asid-ok` and nothing else when all was well: count whole lines.
        let mut line = [0u8; 16];
        let mut length = 0;
        let mut chunk = [0u8; 512];
        loop {
            let got = said.read(&mut chunk);
            if got == 0 {
                break;
            }
            for &byte in &chunk[..got] {
                if byte == b'\n' {
                    good += usize::from(&line[..length.min(line.len())] == b"asid-ok");
                    length = 0;
                } else {
                    if length < line.len() {
                        line[length] = byte;
                    }
                    length += 1;
                }
            }
        }
    }
    let failed = started - good.min(started);
    let (now, _) = crate::user::tlb::asid_state();
    crate::user::tlb::force_asid_bits(bits);
    let rollovers = now - generation;
    let back = back_to(before, "asid-load-back");
    let ok = started == count && failed == 0 && back && (bits == 0 || rollovers >= 2);
    let forced = if bits == 0 { 0 } else { 8 };
    println!(
        "[ktest] asid-load: programs={started}/{count} failed={failed} bits={bits}->{forced} generations={rollovers} ok={ok}"
    );
    i32::from(!ok)
}

/// 1024 spaces at once, as many as there are tasks, each with its own word at the same
/// address, every one loaded in turn and read, twice, on CPU 0: with ASIDs forced to 8
/// bits that runs out of them four times a pass, so
/// the generation rolls over and spaces get new ones, and each must still see only its
/// own. Meanwhile, with two CPUs, CPU 1 keeps a space of its own loaded with interrupts
/// masked, reading its word: every rollover must find it loaded there and reserve its
/// ASID, so no space on CPU 0 is given that ASID in a later generation, and when CPU 1
/// loads it again it keeps it.
fn test_asid_rollover() -> i32 {
    const SPACES: usize = 1024;
    const MAGIC: u64 = 0x5eed_0000_0000;
    const HELPER: u64 = MAGIC | 0xffff;
    static STAGE: AtomicU32 = AtomicU32::new(0);
    static HELPER_CONTEXT: AtomicU64 = AtomicU64::new(0);
    static HELPER_WRONG: AtomicU32 = AtomicU32::new(0);
    static HELPER_KEPT: AtomicU32 = AtomicU32::new(0);
    let processes = &sched::PROCESSES;
    let me = processes.current();
    processes.set_affinity(me, 1 << 0);
    processes.sleep_ms(20);
    let (generation, bits) = crate::user::tlb::asid_state();
    crate::user::tlb::force_asid_bits(8);
    STAGE.store(0, Ordering::Release);
    HELPER_CONTEXT.store(0, Ordering::Release);
    HELPER_WRONG.store(0, Ordering::Release);
    HELPER_KEPT.store(0, Ordering::Release);
    // The space CPU 1 keeps loaded, and the task that does.
    let helper_space = if sched::online_cpus() >= 2 {
        crate::user::TestSpace::new().filter(|space| space.map_word(TEST_PAGE, HELPER).is_some())
    } else {
        None
    };
    let mut helper = None;
    if let Some(space) = &helper_space {
        let address = space as *const crate::user::TestSpace as usize;
        let task = Box::new(move || {
            // On CPU 1 before anything: it was pinned there just after it started.
            processes.sleep_ms(50);
            // SAFETY: the test waits for this task before the space goes.
            let space = unsafe { &*(address as *const crate::user::TestSpace) };
            let flags = Port::irq_save_and_disable();
            sched::borrow_tables(space.mm().root(), space.mm().tlb());
            HELPER_CONTEXT.store(space.mm().tlb().context(), Ordering::Release);
            STAGE.store(1, Ordering::Release);
            // Masked, so this CPU never changes tables while CPU 0 rolls the ASIDs over.
            let mut wrong = 0;
            while STAGE.load(Ordering::Acquire) < 2 {
                // SAFETY: mapped readable in the tables just loaded.
                if unsafe { Port::read_user_word(TEST_PAGE) } != HELPER {
                    wrong += 1;
                }
            }
            Port::irq_restore(flags);
            // Loaded again, in the generation CPU 0 left: the reserved ASID is kept.
            sched::borrow_tables(space.mm().root(), space.mm().tlb());
            let flags = Port::irq_save_and_disable();
            // SAFETY: as above.
            if unsafe { Port::read_user_word(TEST_PAGE) } != HELPER {
                wrong += 1;
            }
            let (before, after) = (HELPER_CONTEXT.load(Ordering::Acquire), space.mm().tlb().context());
            Port::irq_restore(flags);
            sched::return_tables();
            HELPER_WRONG.store(wrong, Ordering::Release);
            HELPER_KEPT.store(u32::from(before as u16 == after as u16), Ordering::Release);
            STAGE.store(3, Ordering::Release);
            0
        });
        if let Ok(pid) = processes.spawn_task("ktest-asid", SpawnIo::default(), task) {
            processes.set_affinity(pid, 1 << 1);
            helper = Some(pid);
        }
    }
    let helper_loaded = helper.is_none() || wait_stage(&STAGE, 1, 5000);
    let helper_context = HELPER_CONTEXT.load(Ordering::Acquire);
    let mut spaces = alloc::vec::Vec::new();
    for index in 0..SPACES {
        let Some(space) = crate::user::TestSpace::new() else {
            break;
        };
        if space.map_word(TEST_PAGE, MAGIC | index as u64).is_none() {
            break;
        }
        spaces.push(space);
    }
    let made = spaces.len();
    let mut wrong = 0;
    // Spaces on CPU 0 given CPU 1's ASID in a generation after it loaded it.
    let mut collided = 0;
    for _ in 0..2 {
        for (index, space) in spaces.iter().enumerate() {
            sched::borrow_tables(space.mm().root(), space.mm().tlb());
            let flags = Port::irq_save_and_disable();
            // SAFETY: mapped readable in the tables just loaded, which stay loaded with
            // interrupts masked.
            let value = unsafe { Port::read_user_word(TEST_PAGE) };
            let context = space.mm().tlb().context();
            Port::irq_restore(flags);
            if value != MAGIC | index as u64 {
                wrong += 1;
            }
            if helper_context != 0 && context >> 16 > helper_context >> 16 && context as u16 == helper_context as u16 {
                collided += 1;
            }
        }
    }
    sched::return_tables();
    let (now, _) = crate::user::tlb::asid_state();
    STAGE.store(2, Ordering::Release);
    let helper_ok = match helper {
        Some(pid) => {
            let finished = wait_stage(&STAGE, 3, 5000);
            processes.wait(pid);
            // Only a port with ASIDs has one to keep.
            let kept = bits == 0 || HELPER_KEPT.load(Ordering::Acquire) == 1;
            finished && kept && HELPER_WRONG.load(Ordering::Acquire) == 0
        }
        None => true,
    };
    processes.set_affinity(me, 0);
    drop(spaces);
    drop(helper_space);
    crate::user::tlb::force_asid_bits(bits);
    let rollovers = now - generation;
    // The forced width is one generation; running out of 255 takes at least another.
    let ok = helper_loaded
        && made == SPACES
        && wrong == 0
        && collided == 0
        && helper_ok
        && (bits == 0 || rollovers >= 2);
    let forced = if bits == 0 { 0 } else { 8 };
    let other_cpu = if helper.is_some() { "cpu1" } else { "none" };
    println!(
        "[ktest] asid-rollover: spaces={made} bits={bits}->{forced} generations={rollovers} wrong={wrong} \
         other-cpu={other_cpu} collided={collided} other-ok={helper_ok} ok={ok}"
    );
    i32::from(!ok)
}

/// `THREAD_EXIT` with its clear word inside the range it unmaps: the word is zeroed while
/// it's still mapped, before the range goes, and the range goes. A program can't see
/// this, since the unmap wakes the word's waiters too, so it's read through the frame.
fn test_thread_exit_clear() -> i32 {
    let Some(space) = crate::user::TestSpace::new() else {
        return 1;
    };
    // A word away from the page's start, as a start block's is.
    let word = TEST_PAGE + 0x80;
    if space.map_word(TEST_PAGE, 0).is_none() || space.mm().copy_out(word, &1u32.to_ne_bytes()).is_err() {
        return 1;
    }
    let result = space.exit_clearing(word);
    let ok = matches!(result, Some((0, true)));
    let (cleared, unmapped) = result.map_or(("no", false), |(value, gone)| (if value == 0 { "yes" } else { "no" }, gone));
    println!("[ktest] thread-exit-clear: cleared={cleared} unmapped={unmapped} ok={ok}");
    i32::from(!ok)
}

/// A kernel waiter on a word and a user waiter on a word whose frame is the same one:
/// neither's wake reaches the other, whichever comes first. Then an unmap wakes the user
/// waiter, whose word has gone.
fn test_futex_namespace() -> i32 {
    static KERNEL_WOKE: AtomicU32 = AtomicU32::new(0);
    static USER_WOKE: AtomicU32 = AtomicU32::new(0);
    let processes = &sched::PROCESSES;
    let Some(space) = crate::user::TestSpace::new() else {
        return 1;
    };
    let Some(frame) = space.map_word(TEST_PAGE, 0) else {
        return 1;
    };
    let Ok(key) = space.mm().futex_key(TEST_PAGE) else {
        return 1;
    };
    KERNEL_WOKE.store(0, Ordering::Release);
    USER_WOKE.store(0, Ordering::Release);
    let address = &space as *const crate::user::TestSpace as usize;
    // SAFETY: the frame is the space's, mapped until the last round, and the identity
    // map makes its address the kernel's.
    let word = unsafe { &*(frame as *const AtomicU32) };
    let spawn_kernel = || {
        processes.spawn_task(
            "ktest-kwait",
            SpawnIo::default(),
            Box::new(move || {
                // SAFETY: as above; the test waits for this task.
                let word = unsafe { &*(frame as *const AtomicU32) };
                feos_sync::futex_wait(word, 0);
                KERNEL_WOKE.fetch_add(1, Ordering::AcqRel);
                0
            }),
        )
    };
    let spawn_user = || {
        processes.spawn_task(
            "ktest-uwait",
            SpawnIo::default(),
            Box::new(move || {
                // SAFETY: as above.
                let space = unsafe { &*(address as *const crate::user::TestSpace) };
                let word = unsafe { &*(frame as *const AtomicU32) };
                let unmaps = space.mm().unmap_count();
                sched::futex_wait_user(word, key, 0, (unmaps, unmaps.load(Ordering::Acquire)), None);
                USER_WOKE.fetch_add(1, Ordering::AcqRel);
                0
            }),
        )
    };
    let counts = || (KERNEL_WOKE.load(Ordering::Acquire), USER_WOKE.load(Ordering::Acquire));
    let mut ok = true;
    let mut report = alloc::string::String::new();
    for kernel_first in [true, false] {
        let (Ok(kernel), Ok(user)) = (spawn_kernel(), spawn_user()) else {
            return 1;
        };
        processes.sleep_ms(100);
        let before = counts();
        let (first_woken, after_first) = if kernel_first {
            feos_sync::futex_wake(word, usize::MAX);
            processes.sleep_ms(100);
            (0, counts())
        } else {
            let woken = sched::futex_wake_user(key, usize::MAX);
            processes.sleep_ms(100);
            (woken, counts())
        };
        let second_woken = if kernel_first {
            sched::futex_wake_user(key, usize::MAX)
        } else {
            feos_sync::futex_wake(word, usize::MAX);
            0
        };
        processes.wait(kernel);
        processes.wait(user);
        let (k, u) = before;
        let round_ok = if kernel_first {
            after_first == (k + 1, u) && second_woken == 1
        } else {
            after_first == (k, u + 1) && first_woken == 1
        };
        ok &= round_ok;
        let _ = core::fmt::Write::write_fmt(
            &mut report,
            format_args!("{}-first={} ", if kernel_first { "kernel" } else { "user" }, if round_ok { "separate" } else { "CROSSED" }),
        );
    }
    // A wait on a word that's unmapped is woken by the unmap.
    let Ok(user) = spawn_user() else {
        return 1;
    };
    processes.sleep_ms(100);
    let before = USER_WOKE.load(Ordering::Acquire);
    space.unmap(TEST_PAGE);
    processes.sleep_ms(100);
    let unmap_wakes = USER_WOKE.load(Ordering::Acquire) == before + 1;
    processes.wait(user);
    // And so is a wait, as a program's, on a word whose page is discarded: its frame is
    // no longer the word's, and the zero page that replaces it still holds what the
    // waiter expected, so nothing but the discard would wake it.
    let remapped = space.map_word(TEST_PAGE, 0).is_some();
    let waiter = processes.spawn_task(
        "ktest-dwait",
        SpawnIo::default(),
        Box::new(move || {
            // SAFETY: as above.
            let space = unsafe { &*(address as *const crate::user::TestSpace) };
            let result = space.wait_word(TEST_PAGE, 0);
            USER_WOKE.fetch_add(1, Ordering::AcqRel);
            i32::from(result != 0)
        }),
    );
    let Ok(waiter) = waiter else {
        return 1;
    };
    processes.sleep_ms(100);
    let before = USER_WOKE.load(Ordering::Acquire);
    let discarded = space.discard(TEST_PAGE);
    processes.sleep_ms(100);
    let discard_wakes = remapped && discarded && USER_WOKE.load(Ordering::Acquire) == before + 1;
    if !discard_wakes {
        // Not to leave the waiter blocked on a test that failed.
        let _ = space.mm().futex_key(TEST_PAGE).map(|key| sched::futex_wake_user(key, usize::MAX));
    }
    processes.wait(waiter);
    drop(space);
    ok &= unmap_wakes && discard_wakes;
    println!("[ktest] futex-namespace: {report}unmap-wakes={unmap_wakes} discard-wakes={discard_wakes} ok={ok}");
    i32::from(!ok)
}

struct KernelTest;

impl App for KernelTest {
    fn name(&self) -> &'static str {
        "ktest"
    }

    fn usage(&self) -> &'static str {
        "ktest <uspawn [n]|ukill|setlkw-killed|teardown|kstack [DIR]|kstack-kill|thread-fault|tlb|asid-rollover|asid-load [n]|futex-namespace|thread-exit-clear|drive PROGRAM [ARGS]|dirty-storm PATH MIB|user-memory|cpus|fp|irqs|mem|priority|affinity|timer|panic|overflow|overflow-locked|null|task-panic>"
    }

    fn description(&self) -> &'static str {
        "kernel self-tests, some of which stop the machine"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        match context.args.trim() {
            "user-memory" => test_user_memory(),
            "tlb" => test_tlb(),
            "asid-rollover" => test_asid_rollover(),
            load if load.starts_with("asid-load") => {
                let count = load["asid-load".len()..].trim().parse().unwrap_or(300);
                test_asid_load(count)
            }
            "futex-namespace" => test_futex_namespace(),
            "thread-exit-clear" => test_thread_exit_clear(),
            "ukill" => test_ukill(),
            "setlkw-killed" => test_setlkw_killed(),
            "thread-fault" => test_thread_fault(),
            "records" => test_records(),
            "teardown" => test_teardown(),
            "kstack-kill" => test_kstack_kill(),
            kstack if kstack.starts_with("kstack") => {
                let dir = kstack["kstack".len()..].trim();
                test_kstack(if dir.is_empty() { "/tmp" } else { dir })
            }
            drive if drive.starts_with("drive ") => test_drive(&drive["drive ".len()..]),
            storm if storm.starts_with("dirty-storm") => {
                let mut words = storm["dirty-storm".len()..].split_whitespace();
                let path = words.next().unwrap_or("/tmp/ktest-dirty-storm");
                let mib = words.next().and_then(|mib| mib.parse().ok()).unwrap_or(8);
                test_dirty_storm(path, mib)
            }
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
