//! Preemptive round-robin process scheduler.
//!
//! `feos` processes are kernel threads: they share the address space and all
//! run at the kernel's privilege level, but each has its own stack and a saved
//! register frame. Every online CPU runs its architecture timer at 100 Hz. A
//! trap saves the interrupted frame on the CPU's private exception stack, this
//! module chooses the next runnable process, and the stub returns to that
//! process instead. A task can also trap on purpose, through
//! [`irq::request_switch`], to yield, sleep, wait or exit.
//!
//! Decisions live in `feos-sched`, which is tested on the host. This file owns
//! what they act on: register frames, stacks, the lock, and the per-CPU state.
//!
//! The process table is shared by every core and lives in `.bss`. Each CPU
//! remembers which slot it is currently running, so a process is never
//! executing on two cores at once. Idle cores run a tiny idle loop on a
//! private stack. PID 1 is the shell and is never scheduled away permanently;
//! every other slot can be filled by [`Processes::spawn`](feos_app::Processes::spawn).
//!
//! # Killing a process
//!
//! A kill only frees a process at a safe point: not running, holding no locks,
//! and either owning no heap or past a short grace period. A running process is
//! flagged and reaped at its CPU's next tick; its locks are counted through
//! [`feos_sync::SchedHooks`]. There's no ownership record for what a process
//! shares with others, so memory can't be taken back from a killed process.
//! The grace period lets a process that polls `kill_requested` unwind and free
//! it, and whatever it still owns after that is reported as leaked.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr::addr_of;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use feos_app::{
    App, Context, KillResult, Platform, ProcessDetail, ProcessInfo, ProcessState, Processes,
    SpawnError, SystemInfo, MAX_PROCESSES, PROCESS_ARGS_MAX, PROCESS_NAME_MAX,
};
use feos_hal::{Board, Clock, IpiTarget};
use feos_sched::{KillOutcome, Request, State, Table, TaskFacts};
use feos_sync::{IrqSpinLock, SchedHooks};

use crate::arch::{self, cpu, irq, timer};
use crate::Machine;

/// Number of process slots, including the shell.
const MAX_TASKS: usize = MAX_PROCESSES;

/// Number of CPUs the scheduler can run on.
const MAX_CPUS: usize = crate::arch::smp::MAX_CPUS;

// `SystemInfo` fixes the public array size; a port with more private CPUs
// would need to widen `feos_app::MAX_CPUS` as well.
const _: () = assert!(MAX_CPUS <= feos_app::MAX_CPUS);

/// Stack size reserved for each spawned process.
///
/// The shell keeps using the linker-provided kernel stack, so this is only
/// allocated for the remaining slots.
const STACK_SIZE: usize = 128 * 1024;

/// Stack size used by a CPU's idle loop.
const IDLE_STACK_SIZE: usize = 16 * 1024;

/// Maximum argument string length copied into the process table.
const ARG_MAX: usize = PROCESS_ARGS_MAX;

/// The single scheduler priority used by the round-robin policy.
const NORMAL_PRIORITY: u8 = 8;

/// How long a killed process with live heap may keep running to free it.
const KILL_GRACE_MS: u64 = 200;

/// Words at the bottom of a stack that must never change.
const CANARY_WORDS: usize = 8;
const CANARY: u64 = 0xdead_57ac_dead_57ac;
/// Fills the rest of a stack, so the high-water mark can be measured.
const STACK_FILL: u64 = 0x5a5a_5a5a_5a5a_5a5a;

/// Unused memory below each process stack. An overflow lands here first, which
/// leaves the scheduler's tick time to notice before another task is hurt.
const STACK_GAP: usize = 16 * 1024;
const STACK_STRIDE: usize = STACK_SIZE + STACK_GAP;

/// Process stacks, in `.bss` and outside the table, so an overflow can't reach
/// the task's own saved state.
#[repr(C, align(16))]
struct Stacks(UnsafeCell<[[u8; STACK_STRIDE]; MAX_TASKS - 1]>);

// SAFETY: each stack is used by one task at a time, and the table says which.
unsafe impl Sync for Stacks {}

static STACKS: Stacks = Stacks(UnsafeCell::new([[0; STACK_STRIDE]; MAX_TASKS - 1]));

/// Private idle stacks, one per CPU.
#[repr(C, align(16))]
struct IdleStacks(UnsafeCell<[[u8; IDLE_STACK_SIZE]; MAX_CPUS]>);

// SAFETY: each CPU only ever uses its own stack.
unsafe impl Sync for IdleStacks {}

static IDLE_STACKS: IdleStacks = IdleStacks(UnsafeCell::new([[0; IDLE_STACK_SIZE]; MAX_CPUS]));

/// What the kernel keeps about a task beyond scheduling policy.
struct Task {
    name: [u8; PROCESS_NAME_MAX],
    name_len: usize,
    args: [u8; ARG_MAX],
    args_len: usize,
    app: Option<&'static dyn App>,
    context: irq::TrapFrame,
    /// Lowest and highest stack addresses, set when the task is created.
    stack: (usize, usize),
    /// Uptime when the task was created.
    started_ms: u64,
}

impl Task {
    fn set_name(&mut self, name: &str) {
        let bytes = name.as_bytes();
        let len = bytes.len().min(PROCESS_NAME_MAX);
        self.name[..len].copy_from_slice(&bytes[..len]);
        self.name_len = len;
    }
}

struct Scheduler {
    table: Table<MAX_TASKS, MAX_CPUS>,
    tasks: [Task; MAX_TASKS],
    /// Scheduler frequency in Hz (the same on every CPU).
    ticks_per_second: u64,
}

impl Scheduler {
    /// All zeroes, valid for every field, so this static stays in `.bss`.
    const fn zeroed() -> Self {
        // SAFETY: integers, arrays of those, `None`, and the zero `State`
        // and `Wait` variants are all valid as zero bytes.
        unsafe { MaybeUninit::zeroed().assume_init() }
    }
}

static SCHEDULER: IrqSpinLock<Scheduler> = IrqSpinLock::new(Scheduler::zeroed());
static mut PLATFORM: MaybeUninit<Platform> = MaybeUninit::uninit();

/// Tasks can block and yield. False during early boot, when there's nothing to
/// switch to and the trap handlers may not be installed.
static STARTED: AtomicBool = AtomicBool::new(false);

/// CPUs that have joined the scheduler, for the fatal path.
static ONLINE_CPUS: AtomicUsize = AtomicUsize::new(1);

/// Slot running on each CPU plus one, or zero while idle. Only its own CPU
/// writes an entry, with interrupts masked.
static CURRENT: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];

/// Locks held by the task in each slot. Only that task changes its own count.
static LOCK_DEPTH: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];

/// Live heap bytes owned by the task in each slot.
static HEAP_BYTES: [AtomicUsize; MAX_TASKS] = [const { AtomicUsize::new(0) }; MAX_TASKS];

/// PID in each slot, for attributing heap frees without the scheduler lock.
static SLOT_PID: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];

/// A switch a task has asked for, handed to the trap handler on the same CPU.
///
/// The task sets this with interrupts masked and then traps, so nothing else on
/// this CPU runs in between. Marking the task blocked before the trap wouldn't
/// work: another CPU could wake and resume it before its frame had been saved.
struct PendingSwitch(UnsafeCell<PendingRequest>);

// SAFETY: each CPU only accesses its own entry, with interrupts masked.
unsafe impl Sync for PendingSwitch {}

#[derive(Clone, Copy)]
enum PendingRequest {
    Yield,
    Sleep(u64),
    /// Block on this word while it still holds this value.
    Wait(usize, u32),
    Exit,
}

static PENDING: [PendingSwitch; MAX_CPUS] =
    [const { PendingSwitch(UnsafeCell::new(PendingRequest::Yield)) }; MAX_CPUS];

/// The kernel's view of what `feos-sched` can't measure.
struct Facts;

impl TaskFacts for Facts {
    fn lock_depth(&self, slot: usize) -> u32 {
        LOCK_DEPTH[slot].load(Ordering::Acquire)
    }

    fn heap_bytes(&self, slot: usize) -> usize {
        HEAP_BYTES[slot].load(Ordering::Acquire)
    }
}

fn this_cpu() -> usize {
    cpu::index().min(MAX_CPUS - 1)
}

/// The current task's slot. Call with interrupts masked, so it can't migrate.
fn current_slot() -> Option<usize> {
    CURRENT[this_cpu()].load(Ordering::Relaxed).checked_sub(1)
}

fn lock_enter() {
    let flags = irq::save_and_disable();
    if let Some(slot) = current_slot() {
        let depth = &LOCK_DEPTH[slot];
        depth.store(depth.load(Ordering::Relaxed) + 1, Ordering::Release);
    }
    irq::restore(flags);
}

fn lock_exit() {
    let flags = irq::save_and_disable();
    if let Some(slot) = current_slot() {
        // A lock taken before the boot task had a slot wasn't counted.
        let depth = &LOCK_DEPTH[slot];
        depth.store(depth.load(Ordering::Relaxed).saturating_sub(1), Ordering::Release);
    }
    irq::restore(flags);
}

fn futex_wait(word: &AtomicU32, expected: u32) {
    if can_block() {
        switch_with(PendingRequest::Wait(word as *const AtomicU32 as usize, expected));
    } else {
        core::hint::spin_loop();
    }
}

fn futex_wake(word: &AtomicU32, count: usize) {
    let key = word as *const AtomicU32 as usize;
    let idle = {
        let mut scheduler = SCHEDULER.lock();
        let woken = scheduler.table.wake_key(key, count);
        (woken > 0).then(|| scheduler.table.idle_cpu(this_cpu())).flatten()
    };
    kick(idle);
}

static HOOKS: SchedHooks = SchedHooks {
    lock_enter,
    lock_exit,
    relax: yield_now,
    wait: futex_wait,
    wake: futex_wake,
};

/// Asks an idle CPU to look for work now, rather than at its next tick.
fn kick(idle: Option<usize>) {
    if let Some(cpu) = idle {
        Machine::interrupts().send_ipi(IpiTarget::Cpu(cpu), Machine::IPI_RESCHEDULE);
    }
}

/// Installs the platform services used by background processes.
///
/// Must be called once during boot before the shell starts, before any process
/// is spawned, and before secondary CPUs are brought online.
pub fn set_platform(platform: Platform) {
    // SAFETY: boot writes this once; every later access is a shared read.
    unsafe { addr_of_mut!(PLATFORM).write(MaybeUninit::new(platform)) }
}

/// Returns the platform services installed by [`set_platform`].
pub fn platform() -> &'static Platform {
    // SAFETY: `set_platform` has run during boot and never writes again.
    unsafe { (&*addr_of!(PLATFORM)).assume_init_ref() }
}

/// Fills the words `[bottom, end)` of a stack, with the canary at the bottom.
///
/// # Safety
/// The range must be unused stack memory.
unsafe fn paint_stack(bottom: usize, end: usize) {
    let words = (end - bottom) / core::mem::size_of::<u64>();
    let stack = bottom as *mut u64;
    for word in 0..words {
        let value = if word < CANARY_WORDS { CANARY } else { STACK_FILL };
        // SAFETY: the caller owns the range, and stacks are 16-byte aligned.
        unsafe { stack.add(word).write_volatile(value) };
    }
}

/// Bytes of the stack starting at `bottom` that have ever been used.
fn stack_high_water(bottom: usize, size: usize) -> usize {
    if bottom == 0 {
        return 0;
    }
    let total = size / core::mem::size_of::<u64>();
    let stack = bottom as *const u64;
    for word in CANARY_WORDS..total {
        // SAFETY: the range is one of the kernel's stacks. Its owner may be
        // writing to it, which at worst makes this estimate slightly stale.
        if unsafe { stack.add(word).read_volatile() } != STACK_FILL {
            return (total - word) * core::mem::size_of::<u64>();
        }
    }
    0
}

/// Stops the machine if the task's stack has overflowed or is about to.
///
/// By the time this fires, the task has overwritten the top of another stack or
/// `.bss`, so nothing downstream can be trusted.
fn check_stack(scheduler: &Scheduler, slot: usize, frame: &irq::TrapFrame) {
    let task = &scheduler.tasks[slot];
    let (bottom, top) = task.stack;
    if bottom == 0 {
        return;
    }

    let canary_end = bottom + CANARY_WORDS * core::mem::size_of::<u64>();
    let sp = frame.stack_pointer();
    let stack = bottom as *const u64;
    // SAFETY: the canary words are part of this task's stack.
    let intact = (0..CANARY_WORDS).all(|word| unsafe { stack.add(word).read_volatile() } == CANARY);

    if !intact || sp < canary_end || sp > top {
        let pid = scheduler.table.tasks()[slot].pid;
        let name = core::str::from_utf8(&task.name[..task.name_len]).unwrap_or("?");
        panic!("stack overflow in pid {pid} ({name}): sp={sp:#x}, stack {bottom:#x}..{top:#x}");
    }
}

/// Creates the shell's process-table entry and starts counting locks.
/// Call once during boot.
pub fn init() {
    let (bottom, top) = Machine::stack_range();
    let here = addr_of!(bottom) as usize;
    // SAFETY: everything this far below the current frame is unused. Later
    // calls may go deeper than this margin, but they haven't happened yet.
    unsafe { paint_stack(bottom, here.saturating_sub(16 * 1024).max(bottom)) };

    let mut scheduler = SCHEDULER.lock();
    scheduler.table.init();
    scheduler.ticks_per_second = 100;
    let shell = &mut scheduler.tasks[0];
    shell.set_name("shell");
    shell.stack = (bottom, top);

    SLOT_PID[0].store(1, Ordering::Release);
    CURRENT[0].store(1, Ordering::Release);
    feos_sync::install_hooks(&HOOKS);
}

/// Stack top for a CPU's idle loop.
fn idle_stack_top(cpu: usize) -> usize {
    // Taking the address of the array does not create a reference to its
    // contents, and each CPU only ever hands out its own stack.
    IDLE_STACKS.0.get() as usize + (cpu.min(MAX_CPUS - 1) + 1) * IDLE_STACK_SIZE
}

/// Entry point for an idle CPU. The scheduler's tick wakes it again.
extern "C" fn idle_entry() -> ! {
    loop {
        irq::wait_for_interrupt();
    }
}

fn idle_frame(cpu: usize) -> irq::TrapFrame {
    irq::TrapFrame::new_task(idle_entry as *const () as usize, 0, idle_stack_top(cpu), 0)
}

/// Applies `request` and swaps `frame` for the next task's.
///
/// Called on the CPU's exception stack with interrupts masked. `frame` is
/// replaced in place; the assembly stub restores it and returns to the selected
/// process (or to the idle loop). `request` runs under the scheduler lock.
fn reschedule(frame: &mut irq::TrapFrame, request: impl FnOnce() -> Request, tick: bool) {
    let cpu = this_cpu();
    let now = timer::ticks();
    let idle_hint = timer::take_idle_hint();
    let mut scheduler = SCHEDULER.lock();
    let scheduler = &mut *scheduler;
    let request = request();

    if let Some(slot) = scheduler.table.current(cpu) {
        check_stack(scheduler, slot, frame);
    }
    if tick {
        scheduler.table.account_tick(cpu, idle_hint);
    }

    let switch = scheduler.table.switch(cpu, request, now, &Facts);

    if switch.save == switch.next && switch.save.is_some() {
        return;
    }
    if let Some(slot) = switch.save {
        scheduler.tasks[slot].context = *frame;
    }
    match switch.next {
        Some(slot) => {
            *frame = scheduler.tasks[slot].context;
            CURRENT[cpu].store(slot + 1, Ordering::Relaxed);
        }
        None => {
            // No runnable process: park this CPU on its private idle stack.
            *frame = idle_frame(cpu);
            CURRENT[cpu].store(0, Ordering::Relaxed);
        }
    }
}

/// Handles a timer tick on this CPU.
pub fn on_tick(frame: &mut irq::TrapFrame) {
    reschedule(frame, || Request::Preempt, true);
}

/// Handles a request from another CPU to look for work.
pub fn on_reschedule(frame: &mut irq::TrapFrame) {
    reschedule(frame, || Request::Preempt, false);
}

/// Handles a task's [`switch_with`] trap.
pub fn on_switch_request(frame: &mut irq::TrapFrame) {
    // SAFETY: only this CPU touches its entry, and interrupts are masked here.
    let pending = unsafe { *PENDING[this_cpu()].0.get() };
    let request = move || match pending {
        PendingRequest::Yield => Request::Yield,
        PendingRequest::Sleep(deadline) => Request::Sleep(deadline),
        PendingRequest::Wait(key, expected) => {
            // SAFETY: the word outlives the wait: its owner is blocked in this
            // call. A wake changes the word and then takes the scheduler lock,
            // and this runs under that lock, so no wake can fall between this
            // check and the task being marked blocked.
            let word = unsafe { &*(key as *const AtomicU32) };
            if word.load(Ordering::Acquire) == expected {
                Request::Wait(key)
            } else {
                Request::Yield
            }
        }
        PendingRequest::Exit => Request::Exit,
    };
    reschedule(frame, request, false);
}

/// Traps to the scheduler to carry out `request`. The caller resumes when the
/// scheduler next chooses it, possibly on another CPU.
fn switch_with(request: PendingRequest) {
    let flags = irq::save_and_disable();
    // SAFETY: only this CPU touches its entry, and interrupts are masked.
    unsafe { *PENDING[this_cpu()].0.get() = request };
    irq::request_switch();
    irq::restore(flags);
}

/// Returns `true` when the caller is a task that can give up the CPU.
fn can_block() -> bool {
    STARTED.load(Ordering::Acquire) && irq::is_enabled()
}

/// Offers the CPU to another runnable task.
pub fn yield_now() {
    if can_block() {
        switch_with(PendingRequest::Yield);
    }
}

/// Blocks the calling task for at least `ms` milliseconds.
///
/// Too early in boot, or with interrupts masked, there's no way to block, so
/// this waits in place instead.
pub fn sleep_ms(ms: u64) {
    if !can_block() {
        timer::idle_ms(ms);
        return;
    }
    if ms == 0 {
        yield_now();
        return;
    }
    let delta = (timer::frequency() as u128 * ms as u128 / 1000) as u64;
    switch_with(PendingRequest::Sleep(timer::ticks().saturating_add(delta)));
}

/// Marks the calling process as finished and switches away for good.
fn exit_current() -> ! {
    switch_with(PendingRequest::Exit);
    unreachable!("the scheduler resumed a task that exited");
}

/// Return address installed in a new process's link register.
extern "C" fn task_exit() -> ! {
    exit_current()
}

/// Entry point for a spawned app.
///
/// The scheduler passes the slot index in the first argument register. Arguments
/// are copied onto the process stack before the app runs, so the slot can be
/// reused after an exit without invalidating the app's `&str`.
extern "C" fn app_entry(slot: usize) -> ! {
    let mut args_buffer = [0u8; ARG_MAX];

    let (app, args_len) = {
        let scheduler = SCHEDULER.lock();
        let task = &scheduler.tasks[slot];
        let args_len = task.args_len.min(ARG_MAX);
        args_buffer[..args_len].copy_from_slice(&task.args[..args_len]);
        (task.app, args_len)
    };

    let Some(app) = app else {
        exit_current();
    };

    let args = core::str::from_utf8(&args_buffer[..args_len]).unwrap_or("");
    let mut display = None;
    let mut context = Context {
        args,
        display: &mut display,
        viewport: None,
        suspend: None,
        suspend_area: None,
        platform: platform(),
    };

    app.run(&mut context);
    exit_current()
}

fn spawn_app(app: &'static dyn App, args: &str) -> Result<u32, SpawnError> {
    if args.len() > ARG_MAX {
        return Err(SpawnError::ArgsTooLong);
    }

    // Slot 0 belongs to the shell and is never reused.
    let (slot, pid) = SCHEDULER.lock().table.reserve().ok_or(SpawnError::NoFreeSlot)?;

    // Painting the stack takes a while, so it happens with interrupts on. The
    // reservation keeps the slot from being scheduled or reused meanwhile.
    let bottom = STACKS.0.get() as usize + (slot - 1) * STACK_STRIDE + STACK_GAP;
    let top = bottom + STACK_SIZE;
    // SAFETY: the slot is reserved, so nothing is using its stack.
    unsafe { paint_stack(bottom, top) };
    let started_ms = CLOCK.uptime_ms();

    LOCK_DEPTH[slot].store(0, Ordering::Release);
    HEAP_BYTES[slot].store(0, Ordering::Release);
    SLOT_PID[slot].store(pid, Ordering::Release);

    let idle = {
        let mut scheduler = SCHEDULER.lock();
        let scheduler = &mut *scheduler;
        let task = &mut scheduler.tasks[slot];
        task.set_name(app.name());
        task.args[..args.len()].copy_from_slice(args.as_bytes());
        task.args_len = args.len();
        task.app = Some(app);
        task.stack = (bottom, top);
        task.started_ms = started_ms;
        task.context = irq::TrapFrame::new_task(
            app_entry as *const () as usize,
            slot,
            top,
            task_exit as *const () as usize,
        );
        scheduler.table.commit(slot);
        scheduler.table.idle_cpu(this_cpu())
    };
    kick(idle);

    Ok(pid)
}

fn kill_process(pid: u32) -> KillResult {
    let grace = timer::frequency() / 1000 * KILL_GRACE_MS;
    let now = timer::ticks();
    let outcome = SCHEDULER.lock().table.kill(pid, now, grace, &Facts);
    match outcome {
        KillOutcome::Killed => KillResult::Killed,
        KillOutcome::Pending => KillResult::Pending,
        KillOutcome::AlreadyPending => KillResult::AlreadyPending,
        KillOutcome::NotFound => KillResult::NotFound,
        KillOutcome::Protected => KillResult::Protected,
    }
}

/// PID of the calling task, or 0 outside any task. Call with interrupts masked.
pub fn current_pid_fast() -> u32 {
    current_slot().map_or(0, |slot| SLOT_PID[slot].load(Ordering::Relaxed))
}

/// Adjusts the heap bytes charged to `pid`, which may have exited since.
///
/// A process freeing another's block is normal, so this searches by PID; there
/// are few slots. Blocks from a PID that has gone are charged to nobody. The
/// caller holds the heap lock, so charges can't race each other.
pub fn charge_heap(pid: u32, delta: isize) {
    if pid == 0 || delta == 0 {
        return;
    }
    let Some(slot) = SLOT_PID.iter().position(|slot| slot.load(Ordering::Relaxed) == pid) else {
        return;
    };
    let bytes = &HEAP_BYTES[slot];
    let current = bytes.load(Ordering::Relaxed);
    bytes.store(current.saturating_add_signed(delta), Ordering::Release);
}

/// CPUs that have joined the scheduler.
pub fn online_cpus() -> usize {
    ONLINE_CPUS.load(Ordering::Acquire)
}

/// Copies the current task's name and returns its PID and the name's length.
/// For the fatal path, so it must not wait: the failure may hold the lock.
pub fn current_identity(name: &mut [u8; PROCESS_NAME_MAX]) -> (u32, usize) {
    let Some(slot) = current_slot() else {
        return (0, 0);
    };
    let pid = SLOT_PID[slot].load(Ordering::Relaxed);
    let Some(scheduler) = SCHEDULER.try_lock() else {
        return (pid, 0);
    };
    let task = &scheduler.tasks[slot];
    let len = task.name_len.min(PROCESS_NAME_MAX);
    name[..len].copy_from_slice(&task.name[..len]);
    (pid, len)
}

fn current_pid() -> u32 {
    let flags = irq::save_and_disable();
    let pid = current_pid_fast();
    irq::restore(flags);
    pid
}

fn snapshot(out: &mut [ProcessInfo]) -> usize {
    let mut count = 0;
    let scheduler = SCHEDULER.lock();
    for (slot, policy) in scheduler.table.tasks().iter().enumerate() {
        if matches!(policy.state, State::Free | State::Spawning) || count == out.len() {
            continue;
        }

        let task = &scheduler.tasks[slot];
        out[count] = ProcessInfo {
            pid: policy.pid,
            state: match policy.state {
                State::Running => ProcessState::Running,
                State::Free | State::Spawning => ProcessState::Exited,
                // `ProcessState` has no blocked variant: see `Processes::detail`.
                State::Ready | State::Blocked => ProcessState::Ready,
            },
            name: task.name,
            name_len: task.name_len as u8,
            cpu_ticks: policy.cpu_ticks,
            started_ms: task.started_ms,
            memory_bytes: STACK_SIZE,
            threads: 1,
            priority: NORMAL_PRIORITY,
        };
        count += 1;
    }
    count
}

fn detail(pid: u32) -> Option<ProcessDetail> {
    let (slot, blocked, kill_pending, bottom, top) = {
        let scheduler = SCHEDULER.lock();
        let slot = scheduler.table.find_pid(pid)?;
        let policy = &scheduler.table.tasks()[slot];
        let (bottom, top) = scheduler.tasks[slot].stack;
        (slot, policy.state == State::Blocked, policy.kill_pending, bottom, top)
    };
    // The scan walks the whole stack, so it runs with interrupts on. If the task
    // exits in the meantime, the figure is merely out of date.
    Some(ProcessDetail {
        blocked,
        kill_pending,
        stack_size: top - bottom,
        stack_used: stack_high_water(bottom, top - bottom),
        heap_bytes: HEAP_BYTES[slot].load(Ordering::Acquire),
    })
}

fn kill_requested() -> bool {
    let scheduler = SCHEDULER.lock();
    scheduler
        .table
        .current(this_cpu())
        .is_some_and(|slot| scheduler.table.tasks()[slot].kill_pending)
}

fn system_info() -> SystemInfo {
    let scheduler = SCHEDULER.lock();
    let table = &scheduler.table;
    let mut processes = 0;
    for task in table.tasks() {
        if matches!(task.state, State::Ready | State::Running | State::Blocked) {
            processes += 1;
        }
    }
    let mut cpu_ticks = [0u64; feos_app::MAX_CPUS];
    let mut cpu_idle_ticks = [0u64; feos_app::MAX_CPUS];
    for cpu in 0..MAX_CPUS.min(feos_app::MAX_CPUS) {
        cpu_ticks[cpu] = table.cpu_total_ticks[cpu];
        cpu_idle_ticks[cpu] = table.cpu_idle_ticks[cpu];
    }

    SystemInfo {
        uptime_ms: CLOCK.uptime_ms(),
        total_ticks: table.total_ticks,
        idle_ticks: table.idle_ticks,
        ticks_per_second: scheduler.ticks_per_second,
        processes,
        threads: processes,
        cpus: table.online(),
        cpu_ticks,
        cpu_idle_ticks,
    }
}

/// Kernel implementation of the process-management platform service.
pub struct ProcessManager;

impl Processes for ProcessManager {
    fn spawn(&self, app: &'static dyn App, args: &str) -> Result<u32, SpawnError> {
        spawn_app(app, args)
    }

    fn kill(&self, pid: u32) -> bool {
        kill_process(pid).accepted()
    }

    fn try_kill(&self, pid: u32) -> KillResult {
        kill_process(pid)
    }

    fn kill_requested(&self) -> bool {
        kill_requested()
    }

    fn yield_now(&self) {
        yield_now();
    }

    fn sleep_ms(&self, ms: u64) {
        sleep_ms(ms);
    }

    fn detail(&self, pid: u32) -> Option<ProcessDetail> {
        detail(pid)
    }

    fn leaked_heap_bytes(&self) -> usize {
        SCHEDULER.lock().table.leaked_bytes
    }

    fn current(&self) -> u32 {
        current_pid()
    }

    fn snapshot(&self, out: &mut [ProcessInfo]) -> usize {
        snapshot(out)
    }

    fn system(&self) -> SystemInfo {
        system_info()
    }
}

/// The process manager installed in [`Platform`].
pub static PROCESSES: ProcessManager = ProcessManager;

/// The clock apps and drivers use: the architecture's, except that waiting
/// blocks the process instead of holding the CPU.
pub struct KernelClock;

impl Clock for KernelClock {
    fn frequency(&self) -> u64 {
        arch::ARCH.frequency()
    }

    fn ticks(&self) -> u64 {
        arch::ARCH.ticks()
    }

    fn name(&self) -> &'static str {
        arch::ARCH.name()
    }

    fn spin_ms(&self, ms: u64) {
        arch::ARCH.spin_ms(ms);
    }

    fn idle_ms(&self, ms: u64) {
        sleep_ms(ms);
    }
}

/// The clock installed in [`Platform`].
pub static CLOCK: KernelClock = KernelClock;

/// Starts the scheduler's periodic timer on the boot CPU and unmasks IRQs.
///
/// The interrupt controller and IRQ handler must already be installed.
pub fn start(tick_hz: u64) {
    SCHEDULER.lock().ticks_per_second = tick_hz;
    STARTED.store(true, Ordering::Release);
    timer::start_periodic(tick_hz);
    irq::enable();
}

/// Joins the scheduler from a secondary CPU.
///
/// The architecture entry stub has already installed the exception vectors and
/// set this CPU's logical index. This function starts its timer, unmasks IRQs,
/// and parks it in the idle loop until the scheduler hands it a process.
pub fn start_cpu(tick_hz: u64) -> ! {
    {
        let mut scheduler = SCHEDULER.lock();
        scheduler.ticks_per_second = tick_hz;
        let online = scheduler.table.cpu_online();
        ONLINE_CPUS.store(online, Ordering::Release);
    }

    timer::start_periodic(tick_hz);
    irq::enable();

    loop {
        irq::wait_for_interrupt();
    }
}
