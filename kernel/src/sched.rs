//! Preemptive round-robin process scheduler.
//!
//! `feos` processes are kernel threads: they share the address space and all
//! run at EL1, but each has its own 16 KiB stack and a saved register frame.
//! The generic timer raises an IRQ every 10 ms; the architecture IRQ stub
//! saves the interrupted frame, this module picks the next runnable process,
//! and the stub returns to that process instead.
//!
//! The process table is fixed-size and lives in `.bss`. PID 1 is the shell and
//! is never scheduled away permanently. Every other slot can be filled by
//! [`Processes::spawn`](feos_app::Processes::spawn).

use core::mem::MaybeUninit;
use core::ptr::addr_of;
use core::ptr::addr_of_mut;

use feos_app::{
    App, Context, Platform, ProcessInfo, ProcessState, Processes, SpawnError, SystemInfo,
    MAX_PROCESSES, PROCESS_ARGS_MAX, PROCESS_NAME_MAX,
};
use feos_arch_aarch64::{irq, timer};

/// Number of process slots, including the shell.
const MAX_TASKS: usize = MAX_PROCESSES;

/// Stack size reserved for each spawned process.
///
/// The shell keeps using the linker-provided kernel stack, so this is only
/// allocated for the remaining slots.
const STACK_SIZE: usize = 16 * 1024;

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
    /// Scheduler ticks consumed by this task.
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
    current: usize,
    next_pid: u32,
    /// Timer ticks accounted since boot, including idle ticks.
    total_ticks: u64,
    /// Ticks spent in an architecture idle wait.
    idle_ticks: u64,
    /// Scheduler frequency in Hz.
    ticks_per_second: u64,
}

impl Scheduler {
    const fn new() -> Self {
        // Keep the whole table zero-initialised so it lands in `.bss`; `init`
        // fills in the shell's PID and the first spawned PID.
        Self {
            tasks: [Task::EMPTY; MAX_TASKS],
            current: 0,
            next_pid: 0,
            total_ticks: 0,
            idle_ticks: 0,
            ticks_per_second: 100,
        }
    }

    /// Finds the next runnable slot after `current`, wrapping around.
    fn next_runnable(&self, current: usize) -> Option<usize> {
        for step in 1..=MAX_TASKS {
            let index = (current + step) % MAX_TASKS;
            if self.tasks[index].state == State::Ready {
                return Some(index);
            }
        }

        if self.tasks[current].state == State::Ready {
            Some(current)
        } else {
            None
        }
    }

    fn find_pid(&self, pid: u32) -> Option<usize> {
        self.tasks
            .iter()
            .position(|task| task.state != State::Free && task.pid == pid)
    }
}

static mut SCHEDULER: Scheduler = Scheduler::new();
static mut PLATFORM: MaybeUninit<Platform> = MaybeUninit::uninit();

/// Installs the platform services used by background processes.
///
/// Must be called once during boot before the shell starts and before any
/// process is spawned.
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
    // SAFETY: called once from boot before interrupts are enabled.
    let scheduler = unsafe { &mut *addr_of_mut!(SCHEDULER) };
    let shell = &mut scheduler.tasks[0];
    shell.state = State::Running;
    shell.pid = 1;
    shell.set_name("shell");
    shell.cpu_ticks = 0;
    shell.started_ms = 0;
    scheduler.current = 0;
    scheduler.next_pid = 2;
}

/// Saves the interrupted process and switches to the next runnable one.
///
/// Called from the IRQ handler with interrupts masked. `frame` is replaced in
/// place; the assembly stub restores it and returns to the selected process.
pub fn preempt(frame: &mut irq::TrapFrame) {
    // SAFETY: interrupts are masked, so this is the only writer.
    let scheduler = unsafe { &mut *addr_of_mut!(SCHEDULER) };
    let current = scheduler.current;

    // A tick spent inside `WFI` is idle, not CPU time for the current task.
    scheduler.total_ticks = scheduler.total_ticks.wrapping_add(1);
    if timer::take_idle_hint() {
        scheduler.idle_ticks = scheduler.idle_ticks.wrapping_add(1);
    } else {
        scheduler.tasks[current].cpu_ticks = scheduler.tasks[current].cpu_ticks.wrapping_add(1);
    }

    scheduler.tasks[current].context = *frame;
    match scheduler.tasks[current].state {
        State::Running => scheduler.tasks[current].state = State::Ready,
        State::Exited => scheduler.tasks[current].state = State::Free,
        State::Free | State::Ready => {}
    }

    let next = scheduler.next_runnable(current).unwrap_or(current);

    scheduler.tasks[next].state = State::Running;
    scheduler.current = next;
    *frame = scheduler.tasks[next].context;
}

/// Marks the calling process as finished and waits to be switched away.
fn exit_current() -> ! {
    let flags = irq::save_and_disable();
    // SAFETY: interrupts are masked while the table is modified.
    unsafe {
        let scheduler = &mut *addr_of_mut!(SCHEDULER);
        scheduler.tasks[scheduler.current].state = State::Exited;
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
        // SAFETY: interrupts are masked while the slot is inspected.
        let scheduler = unsafe { &*addr_of!(SCHEDULER) };
        let task = &scheduler.tasks[slot];
        let args_len = task.args_len.min(ARG_MAX);
        args_buffer[..args_len].copy_from_slice(&task.args[..args_len]);
        let app = task.app;
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
        // SAFETY: interrupts are masked while the table is modified.
        let scheduler = unsafe { &mut *addr_of_mut!(SCHEDULER) };

        // Slot 0 belongs to the shell and is never reused.
        let slot = (1..MAX_TASKS)
            .find(|&index| matches!(scheduler.tasks[index].state, State::Free | State::Exited));

        match slot {
            Some(slot) => {
                let task = &mut scheduler.tasks[slot];
                let stack_top = task.stack.0.as_ptr() as usize + STACK_SIZE;
                let pid = scheduler.next_pid;
                scheduler.next_pid = scheduler.next_pid.wrapping_add(1).max(2);

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
        // SAFETY: interrupts are masked while the table is modified.
        let scheduler = unsafe { &mut *addr_of_mut!(SCHEDULER) };

        // PID 1 is the shell, and a process must not free the slot it is
        // currently running on.
        if pid == 1 || pid == scheduler.tasks[scheduler.current].pid {
            false
        } else {
            match scheduler.find_pid(pid) {
                Some(slot) => {
                    scheduler.tasks[slot].state = State::Free;
                    scheduler.tasks[slot].app = None;
                    true
                }
                None => false,
            }
        }
    };
    irq::restore(flags);
    killed
}

fn current_pid() -> u32 {
    let flags = irq::save_and_disable();
    // SAFETY: interrupts are masked for the duration of the read.
    let pid = unsafe {
        let scheduler = &*addr_of!(SCHEDULER);
        scheduler.tasks[scheduler.current].pid
    };
    irq::restore(flags);
    pid
}

fn snapshot(out: &mut [ProcessInfo]) -> usize {
    let flags = irq::save_and_disable();
    let mut count = 0;
    // SAFETY: interrupts are masked for the duration of the copy.
    unsafe {
        let scheduler = &*addr_of!(SCHEDULER);
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
    // SAFETY: interrupts are masked for the duration of the read.
    let info = unsafe {
        let scheduler = &*addr_of!(SCHEDULER);
        let mut processes = 0;
        let mut threads = 0;
        for task in &scheduler.tasks {
            if matches!(task.state, State::Ready | State::Running) {
                processes += 1;
                threads += 1;
            }
        }
        SystemInfo {
            uptime_ms: platform().clock.uptime_ms(),
            total_ticks: scheduler.total_ticks,
            idle_ticks: scheduler.idle_ticks,
            ticks_per_second: scheduler.ticks_per_second,
            processes,
            threads,
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

/// Starts the scheduler's periodic timer and unmasks IRQs.
///
/// The interrupt controller and IRQ handler must already be installed.
pub fn start(tick_hz: u64) {
    // SAFETY: called once during boot before interrupts are enabled.
    unsafe {
        let scheduler = &mut *addr_of_mut!(SCHEDULER);
        scheduler.ticks_per_second = tick_hz;
    }
    timer::start_periodic(tick_hz);
    irq::enable();
}
