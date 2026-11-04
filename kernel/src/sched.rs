//! Preemptive round-robin process scheduler.
//!
//! `feos` processes are kernel threads: they share the address space and all
//! run at the kernel's privilege level, but each has its own 128 KiB stack and
//! a saved register frame. Every online CPU runs its architecture timer at
//! 100 Hz; an IRQ saves the interrupted frame, this module chooses the next
//! runnable process, and the stub returns to that process instead.
//!
//! The process table is shared by every core and lives in `.bss`. Each CPU
//! remembers which slot it is currently running, so a process is never
//! executing on two cores at once. Idle cores run a tiny idle loop on a
//! private stack. PID 1 is the shell and is never scheduled away permanently;
//! every other slot can be filled by [`Processes::spawn`](feos_app::Processes::spawn).

use core::mem::MaybeUninit;
use core::ptr::addr_of;
use core::ptr::addr_of_mut;

use feos_app::{
    App, Context, Platform, ProcessInfo, ProcessState, Processes, SpawnError, SystemInfo,
    MAX_PROCESSES, PROCESS_ARGS_MAX, PROCESS_NAME_MAX,
};
use crate::arch::{cpu, irq, timer};
use feos_sync::SpinLock;

/// Number of process slots, including the shell.
const MAX_TASKS: usize = MAX_PROCESSES;

/// Number of CPUs the scheduler can run on.
const MAX_CPUS: usize = crate::arch::smp::MAX_CPUS;

// `SystemInfo` fixes the public array size; a port with more private CPUs
// would need to widen `feos_app::MAX_CPUS` as well.
const _: () = assert!(MAX_CPUS <= feos_app::MAX_CPUS);

/// Sentinel stored in [`Scheduler::current`] when a CPU has no task.
const NO_TASK: usize = usize::MAX;

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

/// Scheduler-visible lifecycle state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Free,
    Ready,
    Running,
    Exited,
}

/// One 16-byte aligned process stack.
#[repr(align(16))]
#[derive(Clone, Copy)]
struct Stack([u8; STACK_SIZE]);

/// One private idle stack, one per CPU.
#[repr(align(16))]
#[derive(Clone, Copy)]
struct IdleStack([u8; IDLE_STACK_SIZE]);

static IDLE_STACKS: [IdleStack; MAX_CPUS] = [IdleStack([0; IDLE_STACK_SIZE]); MAX_CPUS];

/// One process table entry.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Task {
    state: State,
    pid: u32,
    name: [u8; PROCESS_NAME_MAX],
    name_len: usize,
    args: [u8; ARG_MAX],
    args_len: usize,
    app: Option<&'static dyn App>,
    context: irq::TrapFrame,
    stack: Stack,
    /// Scheduler ticks consumed by this task across all CPUs.
    cpu_ticks: u64,
    /// Uptime when the task was created.
    started_ms: u64,
}

impl Task {
    const EMPTY: Self = Self {
        state: State::Free,
        pid: 0,
        name: [0; PROCESS_NAME_MAX],
        name_len: 0,
        args: [0; ARG_MAX],
        args_len: 0,
        app: None,
        context: irq::TrapFrame::ZERO,
        stack: Stack([0; STACK_SIZE]),
        cpu_ticks: 0,
        started_ms: 0,
    };

    fn set_name(&mut self, name: &str) {
        let bytes = name.as_bytes();
        let len = bytes.len().min(PROCESS_NAME_MAX);
        self.name[..len].copy_from_slice(&bytes[..len]);
        self.name_len = len;
    }
}

struct Scheduler {
    tasks: [Task; MAX_TASKS],
    /// Slot running on each CPU, or [`NO_TASK`] while that CPU is idle.
    current: [usize; MAX_CPUS],
    next_pid: u32,
    /// Timer ticks accounted since boot, summed over all CPUs.
    total_ticks: u64,
    /// Ticks a CPU spent waiting in an idle instruction.
    idle_ticks: u64,
    /// Timer ticks accounted per CPU, for per-core graphs.
    cpu_total_ticks: [u64; MAX_CPUS],
    /// Idle ticks accounted per CPU.
    cpu_idle_ticks: [u64; MAX_CPUS],
    /// Scheduler frequency in Hz (the same on every CPU).
    ticks_per_second: u64,
    /// CPUs that have joined the scheduler.
    online_cpus: usize,
}

impl Scheduler {
    const fn new() -> Self {
        Self {
            tasks: [Task::EMPTY; MAX_TASKS],
            current: [NO_TASK; MAX_CPUS],
            next_pid: 0,
            total_ticks: 0,
            idle_ticks: 0,
            cpu_total_ticks: [0; MAX_CPUS],
            cpu_idle_ticks: [0; MAX_CPUS],
            ticks_per_second: 100,
            online_cpus: 1,
        }
    }

    /// Finds the next `Ready` slot after `current`, wrapping around.
    fn next_runnable(&self, current: usize) -> Option<usize> {
        let start = if current < MAX_TASKS {
            current
        } else {
            MAX_TASKS - 1
        };

        for step in 1..=MAX_TASKS {
            let index = (start + step) % MAX_TASKS;
            if self.tasks[index].state == State::Ready {
                return Some(index);
            }
        }

        None
    }

    fn find_pid(&self, pid: u32) -> Option<usize> {
        self.tasks
            .iter()
            .position(|task| task.state != State::Free && task.pid == pid)
    }

    /// Returns `true` when `slot` is the current task on any CPU.
    ///
    /// A slot in that state may still be using its stack (for example while it
    /// waits in `wfi` after exiting), so it must not be freed or reused.
    fn is_current(&self, slot: usize) -> bool {
        self.current.iter().any(|&current| current == slot)
    }
}

static SCHEDULER: SpinLock<Scheduler> = SpinLock::new(Scheduler::new());
static mut PLATFORM: MaybeUninit<Platform> = MaybeUninit::uninit();

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

/// Creates the shell's process-table entry. Call once during boot.
pub fn init() {
    let mut scheduler = SCHEDULER.lock();
    let shell = &mut scheduler.tasks[0];
    shell.state = State::Running;
    shell.pid = 1;
    shell.set_name("shell");
    shell.cpu_ticks = 0;
    shell.started_ms = 0;

    for current in &mut scheduler.current {
        *current = NO_TASK;
    }
    scheduler.current[0] = 0;
    scheduler.next_pid = 2;
    scheduler.online_cpus = 1;
}

/// Stack top for a CPU's idle loop.
fn idle_stack_top(cpu: usize) -> usize {
    let stack = &IDLE_STACKS[cpu.min(MAX_CPUS - 1)];
    // Taking the address of the static does not create a reference to its
    // contents, and each CPU only ever hands out its own stack.
    addr_of!(stack.0) as usize + IDLE_STACK_SIZE
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

/// Saves the interrupted process and switches to the next runnable one.
///
/// Called from each CPU's IRQ handler with interrupts masked. `frame` is
/// replaced in place; the assembly stub restores it and returns to the selected
/// process (or to the idle loop).
pub fn preempt(frame: &mut irq::TrapFrame) {
    let cpu = cpu::index().min(MAX_CPUS - 1);
    let mut scheduler = SCHEDULER.lock();
    let current = scheduler.current[cpu];

    // A tick spent inside `WFI` is idle, not CPU time for the current task.
    scheduler.total_ticks = scheduler.total_ticks.wrapping_add(1);
    scheduler.cpu_total_ticks[cpu] = scheduler.cpu_total_ticks[cpu].wrapping_add(1);
    let idle_hint = timer::take_idle_hint();

    if current < MAX_TASKS {
        let task = &mut scheduler.tasks[current];
        task.context = *frame;
        match task.state {
            State::Running => task.state = State::Ready,
            State::Exited => task.state = State::Free,
            State::Free | State::Ready => {}
        }

        if idle_hint {
            scheduler.idle_ticks = scheduler.idle_ticks.wrapping_add(1);
            scheduler.cpu_idle_ticks[cpu] = scheduler.cpu_idle_ticks[cpu].wrapping_add(1);
        } else {
            task.cpu_ticks = task.cpu_ticks.wrapping_add(1);
        }
    } else {
        scheduler.idle_ticks = scheduler.idle_ticks.wrapping_add(1);
        scheduler.cpu_idle_ticks[cpu] = scheduler.cpu_idle_ticks[cpu].wrapping_add(1);
    }

    match scheduler.next_runnable(current) {
        Some(next) => {
            scheduler.tasks[next].state = State::Running;
            scheduler.current[cpu] = next;
            *frame = scheduler.tasks[next].context;
        }
        None => {
            // No runnable process: park this CPU on its private idle stack.
            scheduler.current[cpu] = NO_TASK;
            *frame = idle_frame(cpu);
        }
    }
}

/// Marks the calling process as finished and waits to be switched away.
fn exit_current() -> ! {
    let flags = irq::save_and_disable();
    {
        let mut scheduler = SCHEDULER.lock();
        let cpu = cpu::index().min(MAX_CPUS - 1);
        let current = scheduler.current[cpu];
        if current < MAX_TASKS {
            scheduler.tasks[current].state = State::Exited;
        }
    }
    irq::restore(flags);

    loop {
        irq::wait_for_interrupt();
    }
}

/// Return address installed in a new process's link register.
extern "C" fn task_exit() -> ! {
    exit_current()
}

/// Entry point for a spawned app.
///
/// The scheduler passes the slot index in `x0`. Arguments are copied onto the
/// process stack before the app runs, so the slot can be reused after an exit
/// without invalidating the app's `&str`.
extern "C" fn app_entry(slot: usize) -> ! {
    let mut args_buffer = [0u8; ARG_MAX];

    let (app, args_len) = {
        let flags = irq::save_and_disable();
        let scheduler = SCHEDULER.lock();
        let task = &scheduler.tasks[slot];
        let args_len = task.args_len.min(ARG_MAX);
        args_buffer[..args_len].copy_from_slice(&task.args[..args_len]);
        let app = task.app;
        drop(scheduler);
        irq::restore(flags);
        (app, args_len)
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

    let flags = irq::save_and_disable();
    let result = {
        let mut scheduler = SCHEDULER.lock();

        // Slot 0 belongs to the shell and is never reused. `Exited` slots stay
        // pinned to the CPU that owns them until its next tick frees them.
        let slot = (1..MAX_TASKS).find(|&index| scheduler.tasks[index].state == State::Free);

        match slot {
            Some(slot) => {
                let pid = scheduler.next_pid;
                scheduler.next_pid = scheduler.next_pid.wrapping_add(1).max(2);
                let task = &mut scheduler.tasks[slot];
                let stack_top = task.stack.0.as_ptr() as usize + STACK_SIZE;

                task.state = State::Ready;
                task.pid = pid;
                task.set_name(app.name());
                task.args[..args.len()].copy_from_slice(args.as_bytes());
                task.args_len = args.len();
                task.app = Some(app);
                task.cpu_ticks = 0;
                task.started_ms = platform().clock.uptime_ms();
                task.context = irq::TrapFrame::new_task(
                    app_entry as *const () as usize,
                    slot,
                    stack_top,
                    task_exit as *const () as usize,
                );

                Ok(pid)
            }
            None => Err(SpawnError::NoFreeSlot),
        }
    };
    irq::restore(flags);
    result
}

fn kill_process(pid: u32) -> bool {
    let flags = irq::save_and_disable();
    let killed = {
        let mut scheduler = SCHEDULER.lock();

        // PID 1 is the shell, and a slot must not be freed while any CPU is
        // still running or exiting on it.
        if pid == 1 {
            false
        } else {
            match scheduler.find_pid(pid) {
                Some(slot) if !scheduler.is_current(slot) => {
                    scheduler.tasks[slot].state = State::Free;
                    scheduler.tasks[slot].app = None;
                    true
                }
                _ => false,
            }
        }
    };
    irq::restore(flags);
    killed
}

fn current_pid() -> u32 {
    let flags = irq::save_and_disable();
    let cpu = cpu::index().min(MAX_CPUS - 1);
    let pid = {
        let scheduler = SCHEDULER.lock();
        let current = scheduler.current[cpu];
        if current < MAX_TASKS {
            scheduler.tasks[current].pid
        } else {
            0
        }
    };
    irq::restore(flags);
    pid
}

fn snapshot(out: &mut [ProcessInfo]) -> usize {
    let flags = irq::save_and_disable();
    let mut count = 0;
    {
        let scheduler = SCHEDULER.lock();
        for task in &scheduler.tasks {
            if task.state == State::Free || count == out.len() {
                continue;
            }

            out[count] = ProcessInfo {
                pid: task.pid,
                state: match task.state {
                    State::Ready => ProcessState::Ready,
                    State::Running => ProcessState::Running,
                    State::Exited | State::Free => ProcessState::Exited,
                },
                name: task.name,
                name_len: task.name_len as u8,
                cpu_ticks: task.cpu_ticks,
                started_ms: task.started_ms,
                memory_bytes: STACK_SIZE,
                threads: 1,
                priority: NORMAL_PRIORITY,
            };
            count += 1;
        }
    }
    irq::restore(flags);
    count
}

fn system_info() -> SystemInfo {
    let flags = irq::save_and_disable();
    let info = {
        let scheduler = SCHEDULER.lock();
        let mut processes = 0;
        let mut threads = 0;
        for task in &scheduler.tasks {
            if matches!(task.state, State::Ready | State::Running) {
                processes += 1;
                threads += 1;
            }
        }
        let mut cpu_ticks = [0u64; feos_app::MAX_CPUS];
        let mut cpu_idle_ticks = [0u64; feos_app::MAX_CPUS];
        for cpu in 0..MAX_CPUS.min(feos_app::MAX_CPUS) {
            cpu_ticks[cpu] = scheduler.cpu_total_ticks[cpu];
            cpu_idle_ticks[cpu] = scheduler.cpu_idle_ticks[cpu];
        }

        SystemInfo {
            uptime_ms: platform().clock.uptime_ms(),
            total_ticks: scheduler.total_ticks,
            idle_ticks: scheduler.idle_ticks,
            ticks_per_second: scheduler.ticks_per_second,
            processes,
            threads,
            cpus: scheduler.online_cpus,
            cpu_ticks,
            cpu_idle_ticks,
        }
    };
    irq::restore(flags);
    info
}

/// Kernel implementation of the process-management platform service.
pub struct ProcessManager;

impl Processes for ProcessManager {
    fn spawn(&self, app: &'static dyn App, args: &str) -> Result<u32, SpawnError> {
        spawn_app(app, args)
    }

    fn kill(&self, pid: u32) -> bool {
        kill_process(pid)
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

/// Starts the scheduler's periodic timer on the boot CPU and unmasks IRQs.
///
/// The interrupt controller and IRQ handler must already be installed.
pub fn start(tick_hz: u64) {
    SCHEDULER.lock().ticks_per_second = tick_hz;
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
        scheduler.online_cpus = (scheduler.online_cpus + 1).min(MAX_CPUS);
    }

    timer::start_periodic(tick_hz);
    irq::enable();

    loop {
        irq::wait_for_interrupt();
    }
}
