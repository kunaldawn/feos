//! The preemptive process scheduler.
//!
//! Most `feos` processes are kernel threads: they share the address space and all
//! run at the kernel's privilege level, but each has its own stack and a saved
//! register frame. A user program's task is the same kind of task, with its own page
//! tables and a second frame for when it's at EL0 or ring 3: see [`crate::user`]. Each
//! CPU's timer fires at the end of the running task's time
//! slice, or for a sleeper sooner than that, and not at all while it has nothing to
//! do. A trap saves the interrupted task's registers right in its frame, this module
//! chooses the next runnable process, and the stub returns to that process's frame
//! instead, which is the only thing a switch changes. A task can also trap on
//! purpose, through [`Arch::request_switch`], to yield, sleep, wait or exit.
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
//!
//! A user program is different while it's in the kernel, in a system call or a page
//! fault: what the call holds is on its kernel stack, so the slot's in-call count
//! keeps it from being reaped there at all. A kill wakes it instead, every wait it can
//! be in gives up, and the call unwinds to [`syscall_entry`] (or [`fault_entry`]),
//! which ends the task. A stop waits for the same point, the way back to the
//! program, so a stopped program never holds a kernel lock.
//!
//! # Teardown
//!
//! What a task leaves behind (its address space, the pipes it held) is given back in
//! task context with interrupts on, never in the trap that takes it off the table.
//! A task that ends itself does it on the way out. One that's reaped, by a kill or a
//! failure, is queued by [`collect_exits`] in a fixed queue, and a reaper task drains
//! it. Either way, its exit is only recorded, and `wait` only returns, once that's
//! done.
//!
//! # Exit status and a task's console
//!
//! A task's exit status goes into a small ring of recent exits once it has left the
//! table and been torn down, so a slot never waits for a parent to collect it. A task can have
//! pipes as its console: everything it prints goes there. Pipes need no heap and
//! record who holds them, so whatever a task held closes when it goes, even if a
//! kill gave it no chance to run its destructors.

extern crate alloc;

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr::addr_of;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use feos_app::pipe::{self, PipeHooks, PipeId};
use feos_app::{
    App, ArgBlock, Context, KillResult, Platform, ProcessDetail, ProcessInfo, ProcessState,
    Processes, SpawnError, SpawnIo, SystemInfo, WaitStatus, EXIT_KILLED, MAX_ARGC, MAX_PROCESSES,
    PROCESS_ARGS_MAX, PROCESS_NAME_MAX,
};
use feos_hal::{Arch, Board, Clock, IpiTarget, TrapContext};
use feos_sched::{KillOutcome, Request, State, Table, TaskFacts};
use feos_sync::{IrqSpinLock, SchedHooks};

use crate::{Frame, Machine, Port};

/// Number of process slots, including the shell.
const MAX_TASKS: usize = MAX_PROCESSES;

/// Number of CPUs the scheduler can run on.
const MAX_CPUS: usize = Port::MAX_CPUS;

// `SystemInfo` fixes the public array size; a port with more private CPUs
// would need to widen `feos_app::MAX_CPUS` as well.
const _: () = assert!(MAX_CPUS <= feos_app::MAX_CPUS);

/// Stack size reserved for each spawned process.
///
/// The shell keeps using the linker-provided kernel stack, so this is only
/// allocated for the remaining slots.
const STACK_SIZE: usize = 256 * 1024;

/// Stack size used by a CPU's idle loop.
const IDLE_STACK_SIZE: usize = 16 * 1024;

/// How long a killed process with live heap may keep running to free it.
const KILL_GRACE_MS: u64 = 200;

/// Words at the bottom of a stack that must never change.
const CANARY_WORDS: usize = 8;
const CANARY: u64 = 0xdead_57ac_dead_57ac;
/// Fills the rest of a stack, so the high-water mark can be measured.
const STACK_FILL: u64 = 0x5a5a_5a5a_5a5a_5a5a;

/// Unused memory the linker script leaves below the boot stack, as a guard.
const BOOT_STACK_GAP: usize = 0x40000;

/// Unmapped memory below each process stack, so an overflow faults right there. It's
/// more than one page so that a large frame, or a chain of function calls that
/// doesn't touch every page, can't step over it.
const STACK_GAP: usize = 16 * 1024;
const STACK_STRIDE: usize = STACK_SIZE + STACK_GAP;

/// Exit status of a task ended for a CPU fault or a panic, as a shell would report
/// a segmentation fault.
const EXIT_FAULTED: i32 = 139;

/// Process stacks, in `.bss` and outside the table, so an overflow can't reach
/// the task's own saved state. Page aligned for the sake of the guard pages.
#[repr(C, align(4096))]
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

/// Each slot's register frame: what the slot's task is, when it isn't running.
///
/// Traps on the CPU running a task write here, and the CPU that next runs it reads
/// it, without taking a lock. The table's lock orders those: a task is `Ready` only
/// once its registers are all saved, and nobody else looks until then.
struct Frames<const N: usize>([UnsafeCell<Frame>; N]);

// SAFETY: as above.
unsafe impl<const N: usize> Sync for Frames<N> {}

static TASK_FRAMES: Frames<MAX_TASKS> =
    Frames([const { UnsafeCell::new(Frame::ZERO) }; MAX_TASKS]);
static IDLE_FRAMES: Frames<MAX_CPUS> =
    Frames([const { UnsafeCell::new(Frame::ZERO) }; MAX_CPUS]);

/// A user program's registers while its task is in the kernel, or off the CPU having
/// been interrupted at EL0 or ring 3. The same rules as [`TASK_FRAMES`] hold.
static USER_FRAMES: Frames<MAX_TASKS> =
    Frames([const { UnsafeCell::new(Frame::ZERO) }; MAX_TASKS]);

/// The task in each slot is at EL0 or ring 3 when it next resumes: its registers are in
/// [`USER_FRAMES`], not [`TASK_FRAMES`].
static IN_USER: [AtomicBool; MAX_TASKS] = [const { AtomicBool::new(false) }; MAX_TASKS];

/// What to load for the task in each slot: its page tables, or zero for the kernel's.
static ROOT: [AtomicUsize; MAX_TASKS] = [const { AtomicUsize::new(0) }; MAX_TASKS];

/// How deep the user program in each slot is in the kernel: one from when it starts
/// (loading is the kernel's work) and from each system call or fault, until it's about
/// to resume the program. Zero for kernel-linked apps. Only the slot's own CPU changes
/// it, in a trap. While it's nonzero the task isn't reaped or held by a stop: see
/// `feos-sched`.
static IN_CALL: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];

/// A kill or a stop may be waiting for the task in each slot at the way back to its
/// program. A hint, so that returning from a call needn't take the scheduler's lock:
/// the table says for certain.
static ATTENTION: [AtomicBool; MAX_TASKS] = [const { AtomicBool::new(false) }; MAX_TASKS];

/// What a user program's fault was, kept from the trap for [`fault_entry`].
#[derive(Clone, Copy)]
pub struct UserFault {
    /// The address of a memory fault.
    pub address: Option<u64>,
    /// What kind of fault it was.
    pub name: &'static str,
    /// The instruction that faulted.
    pub instruction: u64,
}

struct FaultNotes([UnsafeCell<UserFault>; MAX_TASKS]);

// SAFETY: a slot's note is written by the trap that takes its task out of EL0 or ring 3
// and read by that task in `fault_entry`, which the write happens before.
unsafe impl Sync for FaultNotes {}

static FAULT_NOTES: FaultNotes = FaultNotes(
    [const { UnsafeCell::new(UserFault { address: None, name: "", instruction: 0 }) }; MAX_TASKS],
);

/// The task in each slot gave back what it held itself, on its way out, so there's
/// nothing left to queue for the reaper when it leaves the table.
static TORN_DOWN: [AtomicBool; MAX_TASKS] = [const { AtomicBool::new(false) }; MAX_TASKS];

/// What the kernel keeps about a task beyond scheduling policy.
struct Task {
    name: [u8; PROCESS_NAME_MAX],
    name_len: usize,
    args: ArgBlock,
    app: Option<&'static dyn App>,
    /// What a task started by [`Processes::spawn_task`] runs, until it starts.
    entry: Option<Box<dyn FnOnce() -> i32 + Send>>,
    /// The program a user task loads: no heap, because a task that goes to EL0 never
    /// comes back to free any.
    user_path: [u8; feos_abi::PATH_MAX],
    user_path_len: usize,
    is_user: bool,
    /// Lowest and highest stack addresses, set when the task is created.
    stack: (usize, usize),
    /// Uptime when the task was created.
    started_ms: u64,
    /// PID of the task that started this one.
    parent: u32,
    /// Spared by an interrupt to the parent.
    background: bool,
    /// The slot's PID hasn't had its exit recorded yet.
    live: bool,
    /// What [`collect_exits`] records. A kill leaves [`EXIT_KILLED`] here.
    exit_code: i32,
}

impl Task {
    fn set_name(&mut self, name: &str) {
        let bytes = name.as_bytes();
        let len = bytes.len().min(PROCESS_NAME_MAX);
        self.name[..len].copy_from_slice(&bytes[..len]);
        self.name_len = len;
    }
}

/// Exits remembered for a later `wait`, oldest overwritten first.
const EXIT_RECORDS: usize = 64;

#[derive(Clone, Copy)]
struct ExitRecord {
    pid: u32,
    code: i32,
    parent: u32,
}

struct Scheduler {
    table: Table<MAX_TASKS, MAX_CPUS>,
    tasks: [Task; MAX_TASKS],
    exits: [ExitRecord; EXIT_RECORDS],
    next_exit: usize,
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

/// The task in each slot failed, so its exit status says so, whatever it was doing.
static FAULTED: [AtomicBool; MAX_TASKS] = [const { AtomicBool::new(false) }; MAX_TASKS];

/// Locks held by the task in each slot. Only that task changes its own count.
static LOCK_DEPTH: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];

/// Live heap bytes owned by the task in each slot.
static HEAP_BYTES: [AtomicUsize; MAX_TASKS] = [const { AtomicUsize::new(0) }; MAX_TASKS];

/// PID in each slot, for attributing heap frees without the scheduler lock.
static SLOT_PID: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];

/// The pipes behind each slot's standard output and error, or zero for the
/// registered console. Written while the slot is reserved, read by its task.
static STDOUT: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];
/// The pipe behind each slot's standard input, or zero for none. Only user programs
/// read through this; kernel-linked apps are handed their streams.
static STDIN: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];
/// Who each task is. Everything starts as root, and only a user program can stop being.
static TASK_UID: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];
static TASK_GID: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];

/// Who the task in `slot` is.
pub fn credentials(slot: usize) -> (u32, u32) {
    (TASK_UID[slot].load(Ordering::Relaxed), TASK_GID[slot].load(Ordering::Relaxed))
}

/// Sets who the task in `slot` is, and what it starts from now on.
pub fn set_credentials(slot: usize, uid: u32, gid: u32) {
    TASK_UID[slot].store(uid, Ordering::Relaxed);
    TASK_GID[slot].store(gid, Ordering::Relaxed);
}

/// Signals each task handles itself, and those waiting for it, as `1 << number`.
static CAUGHT: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];
static PENDING_SIGNALS: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];
/// CPU ticks of each task's children that have gone, and of theirs.
static CHILD_TICKS: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];
static STDERR: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];

/// The terminal each slot belongs to, which a background child prints to even
/// when this task's own output is being captured. Zero for the registered console.
static CONSOLE: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];

/// Changes whenever an exit is recorded, for tasks in [`wait_for`].
static EXIT_SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// How long an idle CPU with nothing due sleeps, in ticks. A wake-up from another
/// CPU comes as an IPI, so this is only a backstop.
const IDLE_TICKS: u64 = 100;

static TIMER_INTERRUPTS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

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
    /// The same, and not past this counter value.
    WaitUntil(usize, u32, u64),
    Exit,
    /// Continue at EL0 or ring 3, from the slot's user frame.
    ToUser,
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

    fn in_call(&self, slot: usize) -> u32 {
        IN_CALL[slot].load(Ordering::Acquire)
    }
}

fn this_cpu() -> usize {
    Port::cpu_index().min(MAX_CPUS - 1)
}

/// The current task's slot. Call with interrupts masked, so it can't migrate.
fn current_slot() -> Option<usize> {
    CURRENT[this_cpu()].load(Ordering::Relaxed).checked_sub(1)
}

pub(crate) fn lock_enter() {
    let flags = Port::irq_save_and_disable();
    if let Some(slot) = current_slot() {
        let depth = &LOCK_DEPTH[slot];
        depth.store(depth.load(Ordering::Relaxed) + 1, Ordering::Release);
    }
    Port::irq_restore(flags);
}

pub(crate) fn lock_exit() {
    let flags = Port::irq_save_and_disable();
    if let Some(slot) = current_slot() {
        // A lock taken before the boot task had a slot wasn't counted.
        let depth = &LOCK_DEPTH[slot];
        depth.store(depth.load(Ordering::Relaxed).saturating_sub(1), Ordering::Release);
    }
    Port::irq_restore(flags);
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

fn futex_wait_ms(word: &AtomicU32, expected: u32, ms: u64) {
    if can_block() {
        let delta = (Port::services().frequency() as u128 * ms as u128 / 1000) as u64;
        let deadline = Port::services().ticks().saturating_add(delta);
        switch_with(PendingRequest::WaitUntil(word as *const AtomicU32 as usize, expected, deadline));
    } else {
        core::hint::spin_loop();
    }
}

static HOOKS: SchedHooks = SchedHooks {
    lock_enter,
    lock_exit,
    relax: yield_now,
    wait: futex_wait,
    wait_ms: futex_wait_ms,
    wake: futex_wake,
    cpu_id: || this_cpu() as u32 + 1,
    task_id: current_pid,
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
    feos_app::publish_platform(self::platform());
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

/// Whether the task's stack still looks right: its canary is intact and its stack
/// pointer is inside it.
///
/// The guard page below normally catches an overflow first, as a fault. What this
/// sees is a stack pointer that has gone astray, or a write that stepped past a
/// frame without touching the guard.
fn stack_is_sound(scheduler: &Scheduler, slot: usize, frame: &Frame) -> bool {
    let task = &scheduler.tasks[slot];
    let (bottom, top) = task.stack;
    if bottom == 0 {
        return true;
    }

    let canary_end = bottom + CANARY_WORDS * core::mem::size_of::<u64>();
    let sp = frame.stack_pointer();
    let stack = bottom as *const u64;
    // SAFETY: the canary words are part of this task's stack.
    let intact = (0..CANARY_WORDS).all(|word| unsafe { stack.add(word).read_volatile() } == CANARY);
    // A program's stack pointer is the program's business.
    intact && (frame.is_user() || (canary_end..=top).contains(&sp))
}

/// Whether a failure in the calling task can be answered by ending just that task.
///
/// It can't when the task is the boot task, which the system needs, or holds a
/// lock, because what the lock protects may be half changed and nobody else could
/// ever take it. The caller has checked that the failure wasn't in an interrupt
/// handler or under a lock that masks interrupts. Marks the task as having failed.
pub fn contain_failure() -> bool {
    let flags = Port::irq_save_and_disable();
    let slot = current_slot();
    Port::irq_restore(flags);
    let contained = !crate::fatal::is_stopping()
        && STARTED.load(Ordering::Acquire)
        && slot.is_some_and(|slot| slot != 0 && LOCK_DEPTH[slot].load(Ordering::Acquire) == 0);
    if contained {
        if let Some(slot) = slot {
            FAULTED[slot].store(true, Ordering::Release);
        }
    }
    contained
}

/// If `address` is in the guard page below the current task's stack, the bounds of
/// that stack. For the fault path, so it must not wait.
pub fn stack_overflow_at(address: u64) -> Option<(usize, usize)> {
    let slot = current_slot()?;
    let (bottom, top) = if slot == 0 {
        Machine::stack_range()
    } else {
        let bottom = stack_bottom(slot);
        (bottom, bottom + STACK_SIZE)
    };
    let gap = if slot == 0 { BOOT_STACK_GAP } else { STACK_GAP };
    let address = address as usize;
    (bottom.saturating_sub(gap)..bottom).contains(&address).then_some((bottom, top))
}

/// Ends the task whose fault trapped into `frame`, and swaps in the next task's.
pub fn end_task_from_trap(frame: &mut Frame) {
    reschedule(frame, || Request::Exit, false);
}

/// Ends the calling task, from a panic in it.
pub fn end_task() -> ! {
    exit_task(EXIT_FAULTED, false)
}

/// Lowest address of a spawned task's stack.
fn stack_bottom(slot: usize) -> usize {
    STACKS.0.get() as usize + (slot - 1) * STACK_STRIDE + STACK_GAP
}

/// Takes the unused memory below every stack out of the page tables, so that
/// overflowing one is a fault at the time, not corruption found out later.
///
/// Call on the boot CPU before others start: changing the tables is only simple
/// with nobody else using them.
fn map_guard_pages() {
    let mut alloc_table = || {
        let table = crate::memory::alloc_frames(1, 1)?;
        Some(table)
    };
    let (boot_bottom, _) = Machine::stack_range();
    // SAFETY: the linker script leaves this below the boot stack and nothing uses it.
    let mut mapped = unsafe {
        Port::unmap_range(boot_bottom - BOOT_STACK_GAP, BOOT_STACK_GAP, &mut alloc_table)
    };
    for slot in 1..MAX_TASKS {
        // SAFETY: nothing ever uses a stack's gap, by that stack or any other.
        mapped &= unsafe {
            Port::unmap_range(stack_bottom(slot) - STACK_GAP, STACK_GAP, &mut alloc_table)
        };
    }
    // Stacks would still work without their guards, with the canaries as a backstop.
    if !mapped {
        feos_console::println!("[mem] guard pages couldn't all be mapped");
    }
}

/// Creates the shell's process-table entry and starts counting locks.
/// Call once during boot.
pub fn init() {
    map_guard_pages();

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
    Port::set_trap_frame(TASK_FRAMES.0[0].get());
    feos_sync::install_hooks(&HOOKS);
    pipe::install_hooks(PipeHooks {
        current_pid,
        interrupted: kill_requested,
    });
    feos_console::set_router(route_console);
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
        Port::wait_for_interrupt();
    }
}

/// Sets up this CPU's idle loop to start over, and returns its frame.
fn idle_frame(cpu: usize) -> *mut Frame {
    let frame = IDLE_FRAMES.0[cpu].get();
    let idle = Frame::new_task(idle_entry as *const () as usize, 0, idle_stack_top(cpu), 0);
    // SAFETY: each CPU only touches its own idle frame, and it isn't resuming it.
    unsafe { frame.write(idle) };
    frame
}

/// The address space ID of the task in a slot. Live spaces each have their own, and
/// [`collect_exits`] clears the TLB of one before it can be used again.
const fn asid(slot: usize) -> u16 {
    slot as u16 + 1
}

/// The frame the task in a slot resumes from.
fn resume_frame(slot: usize) -> *mut Frame {
    if IN_USER[slot].load(Ordering::Relaxed) {
        USER_FRAMES.0[slot].get()
    } else {
        TASK_FRAMES.0[slot].get()
    }
}

/// Applies `request` and asks for the next task's frame to be resumed.
///
/// Called on the CPU's exception stack with interrupts masked. `frame` is where the
/// trap saved what was running. The assembly stub returns to the selected process
/// (or to the idle loop). `request` runs under the scheduler lock.
///
/// Nothing a task leaves behind is given back here: [`collect_exits`] only queues it.
fn reschedule(frame: &mut Frame, request: impl FnOnce() -> Request, tick: bool) {
    let cpu = this_cpu();
    let now = Port::services().ticks();
    let period = Port::timer_period();
    let idle_hint = Port::take_idle_hint();
    let idle = {
        let mut scheduler = SCHEDULER.lock();
        let scheduler = &mut *scheduler;
        let request = request();

        let mut request = request;
        if let Some(slot) = scheduler.table.current(cpu) {
            if request != Request::Exit && !stack_is_sound(scheduler, slot, frame) {
                let pid = scheduler.table.tasks()[slot].pid;
                let task = &scheduler.tasks[slot];
                let name = core::str::from_utf8(&task.name[..task.name_len]).unwrap_or("?");
                let (bottom, top) = task.stack;
                let sp = frame.stack_pointer();
                // Interrupts can't be masked in a task taking a tick, and a request
                // to switch comes with them masked but no lock held.
                if slot == 0 || LOCK_DEPTH[slot].load(Ordering::Acquire) != 0 {
                    panic!("stack overflow in pid {pid} ({name}): sp={sp:#x}, stack {bottom:#x}..{top:#x}");
                }
                crate::fatal::report_stack_overflow(pid, name, sp, bottom, top);
                FAULTED[slot].store(true, Ordering::Release);
                request = Request::Exit;
            }
        }
        // What a failed task owned stays allocated, and is nobody's to free.
        if request == Request::Exit {
            if let Some(slot) = scheduler.table.current(cpu) {
                if FAULTED[slot].load(Ordering::Acquire) {
                    let left = HEAP_BYTES[slot].load(Ordering::Acquire);
                    scheduler.table.leaked_bytes = scheduler.table.leaked_bytes.saturating_add(left);
                }
            }
        }
        scheduler.table.account_elapsed(cpu, now, period, idle_hint);

        let switch = scheduler.table.switch(cpu, request, now, &Facts);

        match switch.next {
            // The outgoing task carries on, from the frame the trap saved: unless that
            // was its kernel half, on its way back to its program.
            Some(slot) if switch.save == Some(slot) => {
                let resume = resume_frame(slot);
                if !core::ptr::eq(frame as *const Frame, resume) {
                    // Its tables too, as on the way out of `on_switch_request`: a
                    // program's first entry comes here from a task that loaded it on
                    // the kernel's tables.
                    Port::activate_user_tables(ROOT[slot].load(Ordering::Relaxed), asid(slot));
                    Port::resume(resume);
                }
            }
            Some(slot) => {
                Port::activate_user_tables(ROOT[slot].load(Ordering::Relaxed), asid(slot));
                Port::resume(resume_frame(slot));
                CURRENT[cpu].store(slot + 1, Ordering::Relaxed);
            }
            None => {
                // No runnable process: park this CPU on its private idle stack.
                Port::activate_user_tables(0, 0);
                Port::resume(idle_frame(cpu));
                CURRENT[cpu].store(0, Ordering::Relaxed);
            }
        }
        let collected = collect_exits(scheduler);
        let idle = announce(scheduler, collected);

        // CPU 0 services the audio stream on its tick, whatever it's doing.
        let idle_limit = if cpu == 0 { period } else { period.saturating_mul(IDLE_TICKS) };
        Port::timer_arm(scheduler.table.next_deadline(cpu, now, period, idle_limit));
        idle
    };
    if tick {
        TIMER_INTERRUPTS[cpu].fetch_add(1, Ordering::Relaxed);
    }
    kick(idle);
}

/// Timer interrupts each CPU has taken. Idle CPUs should take few.
pub fn timer_interrupts(cpu: usize) -> u64 {
    TIMER_INTERRUPTS.get(cpu).map_or(0, |count| count.load(Ordering::Relaxed))
}

/// What a task that has left the table still has to give back, until someone in task
/// context does. Its exit is recorded only after that, so a `wait` that returns means
/// its pipes are closed and its frames are free.
struct Teardown {
    /// [`EMPTY`], [`QUEUED`] or [`TAKEN`]. Only [`collect_exits`] fills an entry and only
    /// [`finish_teardown`] empties one, both under the scheduler lock, so a reader with
    /// that lock sees the record whole. Taking one is a compare-and-swap.
    state: AtomicU8,
    record: UnsafeCell<ExitRecord>,
    process: UnsafeCell<Option<crate::user::Process>>,
}

// SAFETY: the cells belong to whoever moved `state` to its current value: the filler
// until it's `QUEUED`, then the one that took it. The record is only read under the
// scheduler lock, and doesn't change while the entry is in use.
unsafe impl Sync for Teardown {}

const EMPTY: u8 = 0;
const QUEUED: u8 = 1;
const TAKEN: u8 = 2;

/// Room for every slot to have gone at once, and gone again after being reused.
const TEARDOWN_ENTRIES: usize = MAX_TASKS * 2;

static TEARDOWNS: [Teardown; TEARDOWN_ENTRIES] = [const {
    Teardown {
        state: AtomicU8::new(EMPTY),
        record: UnsafeCell::new(ExitRecord { pid: 0, code: 0, parent: 0 }),
        process: UnsafeCell::new(None),
    }
}; TEARDOWN_ENTRIES];

/// Changes whenever a teardown is queued, for the reaper to wait on.
static TEARDOWN_SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// Teardowns done by the task itself and by whoever took them from the queue, and
/// those that, against the rules, ran without a task or with interrupts masked.
static TEARDOWNS_SELF: AtomicUsize = AtomicUsize::new(0);
static TEARDOWNS_QUEUED: AtomicUsize = AtomicUsize::new(0);
static TEARDOWNS_UNSAFE: AtomicUsize = AtomicUsize::new(0);

/// How many teardowns have run: by tasks on their way out, from the queue, and
/// outside task context (which should be none).
pub fn teardown_counts() -> (usize, usize, usize) {
    (
        TEARDOWNS_SELF.load(Ordering::Acquire),
        TEARDOWNS_QUEUED.load(Ordering::Acquire),
        TEARDOWNS_UNSAFE.load(Ordering::Acquire),
    )
}

/// Counts a teardown about to run, and whether it's where teardowns belong.
fn note_teardown(counter: &AtomicUsize) {
    if !(Port::irqs_enabled() && with_current_slot().is_some()) {
        TEARDOWNS_UNSAFE.fetch_add(1, Ordering::AcqRel);
    }
    counter.fetch_add(1, Ordering::AcqRel);
}

/// What [`collect_exits`] did, for [`announce`].
#[derive(Clone, Copy, Default)]
struct Collected {
    /// Every task that has left the table has been collected.
    complete: bool,
    /// An exit was recorded.
    recorded: bool,
    /// A teardown was queued.
    queued: bool,
}

/// Records the exit of every task that has left the table and has given back what it
/// held, and queues the rest for teardown. Runs under the scheduler lock, often in a
/// trap, so it only moves things: see [`drain_teardowns`].
fn collect_exits(scheduler: &mut Scheduler) -> Collected {
    let mut collected = Collected { complete: true, ..Collected::default() };
    for slot in 1..MAX_TASKS {
        if !scheduler.tasks[slot].live || scheduler.table.tasks()[slot].state != State::Free {
            continue;
        }
        let torn_down = TORN_DOWN[slot].load(Ordering::Acquire);
        let entry = if torn_down {
            None
        } else {
            match TEARDOWNS.iter().find(|entry| entry.state.load(Ordering::Acquire) == EMPTY) {
                Some(entry) => Some(entry),
                // The slot stays as it is, uncollected, for a later pass: nobody can
                // reserve it meanwhile, since `spawn_entry` collects everything first.
                None => {
                    collected.complete = false;
                    continue;
                }
            }
        };
        // Its CPU time passes to its parent, for `time`. The table keeps the count until
        // the slot is next reserved.
        let spent = scheduler.table.tasks()[slot].cpu_ticks.wrapping_add(CHILD_TICKS[slot].load(Ordering::Relaxed));
        let parent = scheduler.tasks[slot].parent;
        if let Some(parent_slot) = (0..MAX_TASKS).find(|other| {
            *other != slot && (*other == 0 || scheduler.tasks[*other].live) && SLOT_PID[*other].load(Ordering::Relaxed) == parent
        }) {
            CHILD_TICKS[parent_slot].fetch_add(spent, Ordering::Relaxed);
        }
        let task = &mut scheduler.tasks[slot];
        task.live = false;
        let record = ExitRecord {
            pid: SLOT_PID[slot].load(Ordering::Relaxed),
            code: if FAULTED[slot].load(Ordering::Acquire) {
                EXIT_FAULTED
            } else {
                task.exit_code
            },
            parent: task.parent,
        };
        // Nothing runs on these tables now: every CPU that did changed tables before
        // it let the slot go. After this flush, the slot's ASID is clean for the next.
        if ROOT[slot].swap(0, Ordering::Relaxed) != 0 {
            Port::flush_user_space(asid(slot));
        }
        IN_USER[slot].store(false, Ordering::Relaxed);
        match entry {
            None => {
                record_exit(scheduler, record);
                collected.recorded = true;
            }
            Some(entry) => {
                // SAFETY: the entry is empty, and only this, under the scheduler lock,
                // fills one.
                unsafe {
                    *entry.record.get() = record;
                    *entry.process.get() = crate::user::take_process(slot);
                }
                entry.state.store(QUEUED, Ordering::Release);
                collected.queued = true;
            }
        }
    }
    collected
}

fn record_exit(scheduler: &mut Scheduler, record: ExitRecord) {
    let next = scheduler.next_exit;
    scheduler.exits[next] = record;
    scheduler.next_exit = (next + 1) % EXIT_RECORDS;
}

/// Wakes whoever [`collect_exits`] has news for: tasks waiting for an exit, and the
/// reaper. Under the scheduler lock, as a futex wake would take it. Returns an idle CPU
/// to [`kick`] once the lock is released.
fn announce(scheduler: &mut Scheduler, collected: Collected) -> Option<usize> {
    let mut woken = 0;
    if collected.recorded {
        EXIT_SEQUENCE.fetch_add(1, Ordering::Release);
        woken += scheduler.table.wake_key(&EXIT_SEQUENCE as *const AtomicU32 as usize, usize::MAX);
    }
    if collected.queued {
        TEARDOWN_SEQUENCE.fetch_add(1, Ordering::Release);
        woken += scheduler.table.wake_key(&TEARDOWN_SEQUENCE as *const AtomicU32 as usize, usize::MAX);
    }
    (woken > 0).then(|| scheduler.table.idle_cpu(this_cpu())).flatten()
}

/// Gives back what queued tasks left behind, and records their exits. Call in task
/// context with interrupts on: closing pipes and freeing frames take locks of their own.
fn drain_teardowns() {
    for entry in &TEARDOWNS {
        if entry
            .state
            .compare_exchange(QUEUED, TAKEN, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        // SAFETY: taking the entry made its cells this task's, and the record doesn't
        // change until it's empty again.
        let (record, process) = unsafe { (*entry.record.get(), (*entry.process.get()).take()) };
        note_teardown(&TEARDOWNS_QUEUED);
        pipe::release_pid(record.pid);
        if let Some(process) = process {
            process.destroy();
        }
        finish_teardown(entry, record);
    }
}

/// Records the exit of a task whose teardown is done, and empties its queue entry.
fn finish_teardown(entry: &Teardown, record: ExitRecord) {
    let idle = {
        let mut scheduler = SCHEDULER.lock();
        record_exit(&mut scheduler, record);
        entry.state.store(EMPTY, Ordering::Release);
        announce(&mut scheduler, Collected { complete: true, recorded: true, queued: false })
    };
    kick(idle);
}

/// Whether `pid` has left the table but its teardown hasn't finished, and if so, its
/// parent. Call under the scheduler lock.
fn tearing_down(pid: u32) -> Option<u32> {
    if pid == 0 {
        return None;
    }
    TEARDOWNS.iter().find_map(|entry| {
        if entry.state.load(Ordering::Acquire) == EMPTY {
            return None;
        }
        // SAFETY: see `Teardown`: the caller holds the scheduler lock.
        let record = unsafe { *entry.record.get() };
        (record.pid == pid).then_some(record.parent)
    })
}

/// Gives back what the calling task holds, on its way out: its address space and its
/// pipes, with interrupts on. Then there's nothing for the reaper to do.
fn teardown_self() {
    let Some(slot) = with_current_slot() else {
        return;
    };
    if slot == 0 {
        return;
    }
    // Off its own tables first. A preemption from here on loads the kernel's, since
    // there's no root for it to load.
    let flags = Port::irq_save_and_disable();
    if ROOT[slot].swap(0, Ordering::Relaxed) != 0 {
        Port::activate_user_tables(0, 0);
        Port::flush_user_space(asid(slot));
    }
    Port::irq_restore(flags);
    note_teardown(&TEARDOWNS_SELF);
    let process = crate::user::take_process(slot);
    pipe::release_pid(SLOT_PID[slot].load(Ordering::Acquire));
    if let Some(process) = process {
        process.destroy();
    }
    TORN_DOWN[slot].store(true, Ordering::Release);
}

/// The reaper: a kernel task that gives back what killed and failed tasks left behind.
fn reaper() -> i32 {
    loop {
        let seen = TEARDOWN_SEQUENCE.load(Ordering::Acquire);
        drain_teardowns();
        futex_wait(&TEARDOWN_SEQUENCE, seen);
    }
}

/// Whether this CPU is in its idle loop. Call with interrupts masked.
pub fn is_idle() -> bool {
    current_slot().is_none()
}

/// Handles a timer tick on this CPU.
pub fn on_tick(frame: &mut Frame) {
    reschedule(frame, || Request::Preempt, true);
}

/// Handles a request from another CPU to look for work.
pub fn on_reschedule(frame: &mut Frame) {
    reschedule(frame, || Request::Preempt, false);
}

/// Handles a task's [`switch_with`] trap.
pub fn on_switch_request(frame: &mut Frame) {
    // SAFETY: only this CPU touches its entry, and interrupts are masked here.
    let pending = unsafe { *PENDING[this_cpu()].0.get() };
    if matches!(pending, PendingRequest::ToUser) {
        if let Some(slot) = current_slot() {
            // Out of the call: from here the task is its program's, and can be reaped
            // or held like any other.
            let depth = &IN_CALL[slot];
            depth.store(depth.load(Ordering::Relaxed).saturating_sub(1), Ordering::Release);
            IN_USER[slot].store(true, Ordering::Relaxed);
            // A kill or a stop that came during the call takes effect here, at the
            // boundary, through the scheduler. Otherwise the same task carries on, so
            // there's nothing to decide.
            if ATTENTION[slot].load(Ordering::Acquire) {
                let waiting = {
                    let scheduler = SCHEDULER.lock();
                    let waiting = scheduler.table.stop_or_kill_pending(slot);
                    if !waiting {
                        ATTENTION[slot].store(false, Ordering::Release);
                    }
                    waiting
                };
                if waiting {
                    reschedule(frame, || Request::Yield, false);
                    return;
                }
            }
            Port::activate_user_tables(ROOT[slot].load(Ordering::Relaxed), asid(slot));
            Port::resume(USER_FRAMES.0[slot].get());
        }
        return;
    }
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
        PendingRequest::WaitUntil(key, expected, deadline) => {
            // SAFETY: as for `Wait`.
            let word = unsafe { &*(key as *const AtomicU32) };
            if word.load(Ordering::Acquire) == expected {
                Request::WaitUntil(key, deadline)
            } else {
                Request::Yield
            }
        }
        PendingRequest::Exit | PendingRequest::ToUser => Request::Exit,
    };
    reschedule(frame, request, false);
}

/// Handles a user program's system call, on the CPU's exception stack.
///
/// The program's registers are in its user frame. Carrying out the call can take a
/// long time, block and be preempted, so it runs as an ordinary kernel thread on the
/// task's own kernel stack, starting over from the top each time, with a frame of its
/// own. See [`syscall_entry`].
pub fn on_syscall(_frame: &mut Frame) {
    let Some(slot) = current_slot() else {
        return;
    };
    enter_kernel(slot, syscall_entry as *const () as usize);
}

/// Turns the user program in `slot`, which has just trapped from EL0 or ring 3, into a
/// kernel thread at `entry`, at the top of its kernel stack. That stack is free: a task
/// only uses it while it's in the kernel.
fn enter_kernel(slot: usize, entry: usize) {
    IN_USER[slot].store(false, Ordering::Relaxed);
    IN_CALL[slot].fetch_add(1, Ordering::AcqRel);
    let thread = Frame::new_task(entry, slot, stack_bottom(slot) + STACK_SIZE, task_exit as *const () as usize);
    // SAFETY: this is the running task's frame, and nothing is using it: it was at EL0.
    unsafe { TASK_FRAMES.0[slot].get().write(thread) };
    Port::resume(TASK_FRAMES.0[slot].get());
}

/// A system call's kernel half.
extern "C" fn syscall_entry(slot: usize) -> ! {
    let user = USER_FRAMES.0[slot].get();
    // SAFETY: the task is here, not at EL0, so nothing else is using its user frame.
    let (number, args) = unsafe { ((*user).syscall_number(), (*user).syscall_args()) };
    let result = crate::user::syscall(slot, number, args);
    // A kill gave the call reason to stop waiting, and it has unwound: what it held
    // has gone with it, so the task can end here.
    end_if_killed(slot);
    // SAFETY: as above.
    unsafe { (*user).set_syscall_result(result as usize) };
    return_to_user()
}

/// Handles a fault from a user program, on the CPU's exception stack: hands it to
/// [`fault_entry`], a kernel thread on the task's own stack, which can wait for what
/// it needs with interrupts on. Returns `false`, to stop the machine, if the task
/// wasn't at EL0 or ring 3: the fault's frame would have nowhere to go.
pub fn on_user_fault(fault: UserFault, frame: &mut Frame) -> bool {
    let Some(slot) = current_slot() else {
        return false;
    };
    if !IN_USER[slot].load(Ordering::Relaxed) {
        return false;
    }
    let user = USER_FRAMES.0[slot].get();
    // A port whose faults save their frame somewhere else, on a fault stack, has the
    // program's registers there; they belong in its user frame.
    if !core::ptr::eq(frame as *const Frame, user) {
        // SAFETY: the task is in this trap, so nothing else is using its user frame.
        unsafe { user.write(*frame) };
    }
    // SAFETY: see `FaultNotes`.
    unsafe { FAULT_NOTES.0[slot].get().write(fault) };
    enter_kernel(slot, fault_entry as *const () as usize);
    true
}

/// A user fault's kernel half: gives the program the page it was promised, or ends it.
/// The single place a fault from user mode is decided, which is where a signal frame
/// will be built.
extern "C" fn fault_entry(slot: usize) -> ! {
    // SAFETY: see `FaultNotes`.
    let fault = unsafe { *FAULT_NOTES.0[slot].get() };
    let resolved = fault
        .address
        .is_some_and(|address| crate::user::resolve_fault(slot, address as usize));
    if !resolved {
        crate::fatal::report_user_fault(&fault);
        FAULTED[slot].store(true, Ordering::Release);
        exit_task(EXIT_FAULTED, true);
    }
    end_if_killed(slot);
    return_to_user()
}

/// Ends the calling user task if a kill is waiting for it. Its call has unwound, so
/// nothing of the kernel's is held on its stack.
fn end_if_killed(slot: usize) {
    if ATTENTION[slot].load(Ordering::Acquire) && kill_requested() {
        exit_task(EXIT_KILLED, true);
    }
}

/// Goes back to the calling task's program, from its user frame.
fn return_to_user() -> ! {
    switch_with(PendingRequest::ToUser);
    unreachable!("a task came back from EL0 without a trap");
}

/// Starts the calling task's program. It never comes back here: its system calls run
/// in [`syscall_entry`].
pub fn enter_user(slot: usize, root: usize, frame: Frame) -> ! {
    // SAFETY: the task is running here, so nothing else is using its user frame.
    unsafe { USER_FRAMES.0[slot].get().write(frame) };
    ROOT[slot].store(root, Ordering::Relaxed);
    end_if_killed(slot);
    return_to_user()
}

/// Ends the calling task, for a user program's `exit`.
pub fn exit_user(code: i32) -> ! {
    exit_current(code)
}

/// The CPU this task was on a moment ago.
pub fn current_cpu() -> usize {
    let flags = Port::irq_save_and_disable();
    let cpu = this_cpu();
    Port::irq_restore(flags);
    cpu
}

/// The calling task's slot's standard input.
fn current_stdin() -> Option<PipeId> {
    let slot = with_current_slot()?;
    PipeId::from_raw(STDIN[slot].load(Ordering::Acquire))
}

fn with_current_slot() -> Option<usize> {
    let flags = Port::irq_save_and_disable();
    let slot = current_slot();
    Port::irq_restore(flags);
    slot
}

/// Offers a signal to `pid`: see [`Processes::signal`].
fn offer_signal(pid: u32, number: u32) -> bool {
    if number >= 32 {
        return false;
    }
    let bit = 1u32 << number;
    let idle = {
        let mut scheduler = SCHEDULER.lock();
        let found = scheduler.table.tasks().iter().position(|task| {
            task.pid == pid && !matches!(task.state, State::Free | State::Spawning)
        });
        let Some(slot) = found else {
            return false;
        };
        if CAUGHT[slot].load(Ordering::Relaxed) & bit == 0 {
            return false;
        }
        PENDING_SIGNALS[slot].fetch_or(bit, Ordering::Release);
        let woken = scheduler.table.wake_pid(pid);
        woken.then(|| scheduler.table.idle_cpu(this_cpu())).flatten()
    };
    kick(idle);
    true
}

/// Reads a user program's standard input. Zero at end of file, or if it has none.
pub fn read_stdin(slot: usize, out: &mut [u8]) -> Result<usize, pipe::Killed> {
    match PipeId::from_raw(STDIN[slot].load(Ordering::Acquire)) {
        Some(id) => pipe::read_wait(id, out),
        None => Ok(0),
    }
}

/// Writes to a user program's standard output or error, which go wherever a
/// kernel-linked app's print would. `false` if nobody's left to read it.
pub fn write_stream(slot: usize, error: bool, bytes: &[u8]) -> Result<bool, pipe::Killed> {
    let console = if error { &STDERR[slot] } else { &STDOUT[slot] };
    match PipeId::from_raw(console.load(Ordering::Acquire)) {
        Some(id) => pipe::write_wait(id, bytes),
        None => {
            feos_console::write_bytes(bytes);
            Ok(true)
        }
    }
}

/// Blocks a user program on a word, identified by its kernel address, and says whether
/// it ran out of time. As with any futex, a wake can be spurious.
pub fn futex_wait_user(key: usize, expected: u32, timeout_ms: u64) -> bool {
    if timeout_ms == 0 {
        switch_with(PendingRequest::Wait(key, expected));
        return false;
    }
    let delta = (Port::services().frequency() as u128 * timeout_ms as u128 / 1000) as u64;
    let deadline = Port::services().ticks().saturating_add(delta);
    switch_with(PendingRequest::WaitUntil(key, expected, deadline));
    Port::services().ticks() >= deadline
}

/// Wakes up to `count` tasks blocked on a word. Returns how many.
pub fn futex_wake_user(key: usize, count: usize) -> usize {
    let (woken, idle) = {
        let mut scheduler = SCHEDULER.lock();
        let woken = scheduler.table.wake_key(key, count);
        (woken, (woken > 0).then(|| scheduler.table.idle_cpu(this_cpu())).flatten())
    };
    kick(idle);
    woken
}

/// Traps to the scheduler to carry out `request`. The caller resumes when the
/// scheduler next chooses it, possibly on another CPU.
fn switch_with(request: PendingRequest) {
    let flags = Port::irq_save_and_disable();
    // SAFETY: only this CPU touches its entry, and interrupts are masked.
    unsafe { *PENDING[this_cpu()].0.get() = request };
    Port::request_switch();
    Port::irq_restore(flags);
}

/// Returns `true` when the caller is a task that can give up the CPU.
fn can_block() -> bool {
    STARTED.load(Ordering::Acquire) && Port::irqs_enabled()
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
/// this spins instead: unmasking to wait could preempt a critical section.
pub fn sleep_ms(ms: u64) {
    if !can_block() {
        Port::services().spin_ms(ms);
        return;
    }
    if ms == 0 {
        yield_now();
        return;
    }
    let delta = (Port::services().frequency() as u128 * ms as u128 / 1000) as u64;
    switch_with(PendingRequest::Sleep(Port::services().ticks().saturating_add(delta)));
}

/// Marks the calling process as finished and switches away for good.
fn exit_current(code: i32) -> ! {
    exit_task(code, true)
}

/// Marks the calling process as finished and switches away for good, having given back
/// what it holds if `teardown` and it can. A task ending for a failure leaves that to
/// the reaper: what it was in the middle of may not be in a state to look at.
fn exit_task(code: i32, teardown: bool) -> ! {
    {
        let mut scheduler = SCHEDULER.lock();
        if let Some(slot) = scheduler.table.current(this_cpu()) {
            scheduler.tasks[slot].exit_code = code;
        }
    }
    if teardown && can_block() {
        teardown_self();
    }
    switch_with(PendingRequest::Exit);
    unreachable!("the scheduler resumed a task that exited");
}

/// Return address installed in a new process's link register.
extern "C" fn task_exit() -> ! {
    exit_current(0)
}

/// Entry point for a spawned app or task.
///
/// The scheduler passes the slot index in the first argument register. Arguments
/// are copied onto the process stack before the app runs, so the slot can be
/// reused after an exit without invalidating the app's `&str`.
extern "C" fn app_entry(slot: usize) -> ! {
    let (app, entry, block, user_path) = {
        let mut scheduler = SCHEDULER.lock();
        let task = &mut scheduler.tasks[slot];
        let user_path = task.is_user.then_some((task.user_path, task.user_path_len));
        (task.app, task.entry.take(), task.args, user_path)
    };

    if let Some((path, length)) = user_path {
        let path = core::str::from_utf8(&path[..length]).unwrap_or("");
        exit_current(crate::user::start(slot, path, &block));
    }
    if let Some(entry) = entry {
        exit_current(entry());
    }
    let Some(app) = app else {
        exit_current(0);
    };

    let mut joined = [0u8; PROCESS_ARGS_MAX];
    let args = block.joined(&mut joined);
    let mut slots = [""; MAX_ARGC];
    let mut display = None;
    let mut context = Context {
        args,
        argv: block.argv(&mut slots),
        display: &mut display,
        viewport: None,
        suspend: None,
        suspend_area: None,
        platform: platform(),
    };

    let status = app.run(&mut context);
    exit_current(status)
}

/// What a new task runs.
enum Entry<'a> {
    App(&'static dyn App, ArgBlock),
    Task(Box<dyn FnOnce() -> i32 + Send>),
    /// A program's path, its argv, and the directory relative paths start from.
    User(&'a str, ArgBlock, &'a str),
}

/// Streams for a user program, where a parent program names them.
#[derive(Clone, Copy, Default)]
pub struct RawStreams {
    /// A pipe, or zero to inherit.
    pub stdin: u32,
    /// A pipe, or zero to inherit.
    pub stdout: u32,
}

fn spawn_entry(
    name: &str,
    entry: Entry<'_>,
    io: SpawnIo<'_>,
    raw: RawStreams,
) -> Result<u32, SpawnError> {
    // Slot 0 belongs to the shell and is never reused. A slot that's free but hasn't
    // had its exit collected still holds its address space, so everything gone has to
    // be collected before one can be taken. If the teardown queue is full, this task
    // empties it.
    let user = matches!(entry, Entry::User(..));
    let (slot, pid) = loop {
        let (collected, reserved, idle) = {
            let mut scheduler = SCHEDULER.lock();
            let collected = collect_exits(&mut scheduler);
            let reserved = if collected.complete { scheduler.table.reserve() } else { None };
            // What a kill or a stop looks at is reset before the lock goes, and so
            // before the PID can be found: one that comes while the stack is being
            // painted has its hint kept.
            if let Some((slot, _)) = reserved {
                LOCK_DEPTH[slot].store(0, Ordering::Release);
                // A program starts in the kernel, loading itself.
                IN_CALL[slot].store(u32::from(user), Ordering::Release);
                ATTENTION[slot].store(false, Ordering::Release);
                TORN_DOWN[slot].store(false, Ordering::Release);
                HEAP_BYTES[slot].store(0, Ordering::Release);
            }
            let idle = announce(&mut scheduler, collected);
            (collected, reserved, idle)
        };
        kick(idle);
        if collected.complete {
            break reserved.ok_or(SpawnError::NoFreeSlot)?;
        }
        // Whatever this can't take is being torn down by someone else already.
        drain_teardowns();
        yield_now();
    };

    // Painting the stack takes a while, so it happens with interrupts on. The
    // reservation keeps the slot from being scheduled or reused meanwhile.
    let bottom = stack_bottom(slot);
    let top = bottom + STACK_SIZE;
    // SAFETY: the slot is reserved, so nothing is using its stack.
    unsafe { paint_stack(bottom, top) };
    let started_ms = CLOCK.uptime_ms();

    FAULTED[slot].store(false, Ordering::Release);
    CAUGHT[slot].store(0, Ordering::Release);
    PENDING_SIGNALS[slot].store(0, Ordering::Release);
    CHILD_TICKS[slot].store(0, Ordering::Release);
    SLOT_PID[slot].store(pid, Ordering::Release);

    let flags = Port::irq_save_and_disable();
    // A new task is whoever started it.
    let creator = current_slot();
    let (uid, gid) = creator.map_or((0, 0), credentials);
    TASK_UID[slot].store(uid, Ordering::Release);
    TASK_GID[slot].store(gid, Ordering::Release);
    let (parent, mut inherited_out, mut inherited_err, inherited_console, inherited_in) = match current_slot() {
        Some(parent) => (
            SLOT_PID[parent].load(Ordering::Relaxed),
            STDOUT[parent].load(Ordering::Relaxed),
            STDERR[parent].load(Ordering::Relaxed),
            CONSOLE[parent].load(Ordering::Relaxed),
            STDIN[parent].load(Ordering::Relaxed),
        ),
        None => (0, 0, 0, 0, 0),
    };
    Port::irq_restore(flags);
    // A background child outlives whatever is capturing its parent's output.
    if io.background {
        inherited_out = inherited_console;
        inherited_err = inherited_console;
    }
    let console = |given: Option<&pipe::PipeWriter>, inherited: u32| {
        let id = given.map(|writer| writer.id()).or(PipeId::from_raw(inherited));
        id.filter(|id| pipe::attach_writer(*id, pid)).map_or(0, PipeId::raw)
    };
    if raw.stdout != 0 {
        inherited_out = raw.stdout;
    }
    let stdin_id = io
        .stdin
        .map(|reader| reader.id())
        .or(PipeId::from_raw(if raw.stdin != 0 { raw.stdin } else { inherited_in }));
    STDIN[slot].store(
        stdin_id.filter(|id| pipe::attach_reader(*id, pid)).map_or(0, PipeId::raw),
        Ordering::Release,
    );
    STDOUT[slot].store(console(io.stdout, inherited_out), Ordering::Release);
    STDERR[slot].store(console(io.stderr, inherited_err), Ordering::Release);
    CONSOLE[slot].store(console(io.console, inherited_console), Ordering::Release);

    let idle = {
        let mut scheduler = SCHEDULER.lock();
        let scheduler = &mut *scheduler;
        let task = &mut scheduler.tasks[slot];
        task.set_name(name);
        task.is_user = false;
        match entry {
            Entry::App(app, args) => {
                task.args = args;
                task.app = Some(app);
                task.entry = None;
            }
            Entry::Task(run) => {
                task.args = ArgBlock::new();
                task.app = None;
                task.entry = Some(run);
            }
            Entry::User(path, args, cwd) => {
                let length = path.len().min(task.user_path.len());
                task.user_path[..length].copy_from_slice(&path.as_bytes()[..length]);
                task.user_path_len = length;
                task.is_user = true;
                task.args = args;
                task.app = None;
                task.entry = None;
                crate::user::reset_files(slot, cwd);
            }
        }
        task.stack = (bottom, top);
        task.started_ms = started_ms;
        task.parent = parent;
        task.background = io.background;
        task.live = true;
        task.exit_code = EXIT_KILLED;
        let frame = Frame::new_task(
            app_entry as *const () as usize,
            slot,
            top,
            task_exit as *const () as usize,
        );
        // SAFETY: the slot is reserved, so nothing is running it or about to.
        unsafe { TASK_FRAMES.0[slot].get().write(frame) };
        scheduler.table.commit(slot);
        scheduler.table.idle_cpu(this_cpu())
    };
    kick(idle);

    Ok(pid)
}

fn spawn_app(app: &'static dyn App, args: &str) -> Result<u32, SpawnError> {
    let block = ArgBlock::parse(args).ok_or(SpawnError::ArgsTooLong)?;
    spawn_entry(app.name(), Entry::App(app, block), SpawnIo::default(), RawStreams::default())
}

fn spawn_app_argv(
    app: &'static dyn App,
    argv: &[&str],
    io: SpawnIo<'_>,
) -> Result<u32, SpawnError> {
    let mut block = ArgBlock::new();
    if !block.push_all(argv) {
        return Err(SpawnError::ArgsTooLong);
    }
    spawn_entry(app.name(), Entry::App(app, block), io, RawStreams::default())
}

/// Starts a user program in a new process. `argv` includes its own name, and a
/// program that can't be loaded ends with a status saying why.
pub fn spawn_user(
    path: &str,
    argv: ArgBlock,
    cwd: &str,
    io: SpawnIo<'_>,
    raw: RawStreams,
) -> Result<u32, SpawnError> {
    if path.len() > feos_abi::PATH_MAX {
        return Err(SpawnError::ArgsTooLong);
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    spawn_entry(name, Entry::User(path, argv, cwd), io, raw)
}

/// Sends what the calling task prints to its own console, when it has one.
fn route_console(bytes: &[u8], error: bool) -> bool {
    if !can_block() {
        return false;
    }
    let flags = Port::irq_save_and_disable();
    let slot = current_slot();
    Port::irq_restore(flags);
    let Some(slot) = slot else {
        return false;
    };
    let console = if error { &STDERR[slot] } else { &STDOUT[slot] };
    let Some(id) = PipeId::from_raw(console.load(Ordering::Acquire)) else {
        return false;
    };
    // With nobody left to read it, the output is simply lost.
    let _ = pipe::write_id(id, bytes);
    true
}

fn try_wait(pid: u32) -> WaitStatus {
    let scheduler = SCHEDULER.lock();
    // Until its teardown is done, a task that has gone is as good as running.
    if scheduler.table.find_pid(pid).is_some() || tearing_down(pid).is_some() {
        return WaitStatus::Running;
    }
    for back in 1..=EXIT_RECORDS {
        let index = (scheduler.next_exit + EXIT_RECORDS - back) % EXIT_RECORDS;
        let record = scheduler.exits[index];
        if record.pid == pid && pid != 0 {
            return WaitStatus::Exited(record.code);
        }
    }
    WaitStatus::Unknown
}

fn wait_for(pid: u32) -> Option<i32> {
    loop {
        let sequence = EXIT_SEQUENCE.load(Ordering::Acquire);
        match try_wait(pid) {
            WaitStatus::Exited(code) => return Some(code),
            WaitStatus::Unknown => return None,
            WaitStatus::Running => {}
        }
        if kill_requested() {
            return None;
        }
        futex_wait(&EXIT_SEQUENCE, sequence);
    }
}

/// Waits for a child of `parent`. `None` if it isn't one, or its status has gone.
pub fn wait_child(pid: u32, parent: u32) -> Option<i32> {
    loop {
        let sequence = EXIT_SEQUENCE.load(Ordering::Acquire);
        {
            let scheduler = SCHEDULER.lock();
            if let Some(slot) = scheduler.table.find_pid(pid) {
                if scheduler.tasks[slot].parent != parent {
                    return None;
                }
            } else if let Some(its_parent) = tearing_down(pid) {
                if its_parent != parent {
                    return None;
                }
            } else {
                for back in 1..=EXIT_RECORDS {
                    let index = (scheduler.next_exit + EXIT_RECORDS - back) % EXIT_RECORDS;
                    let record = scheduler.exits[index];
                    if record.pid == pid && pid != 0 {
                        return (record.parent == parent).then_some(record.code);
                    }
                }
                return None;
            }
        }
        if kill_requested() {
            return None;
        }
        futex_wait(&EXIT_SEQUENCE, sequence);
    }
}

fn kill_process(pid: u32) -> KillResult {
    let grace = Port::services().frequency() / 1000 * KILL_GRACE_MS;
    let now = Port::services().ticks();
    let (outcome, idle) = {
        let mut scheduler = SCHEDULER.lock();
        let slot = scheduler.table.find_pid(pid);
        let outcome = scheduler.table.kill(pid, now, grace, &Facts);
        if let (Some(slot), KillOutcome::Pending) = (slot, outcome) {
            ATTENTION[slot].store(true, Ordering::Release);
        }
        let collected = collect_exits(&mut scheduler);
        let woken = announce(&mut scheduler, collected);
        (outcome, woken.or(scheduler.table.idle_cpu(this_cpu())))
    };
    // The task may have to run to reach its safe point.
    kick(idle);
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
    let flags = Port::irq_save_and_disable();
    let pid = current_pid_fast();
    Port::irq_restore(flags);
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
            priority: policy.priority,
            parent: task.parent,
            background: task.background,
        };
        count += 1;
    }
    count
}

fn detail(pid: u32) -> Option<ProcessDetail> {
    let (slot, blocked, kill_pending, stopped, bottom, top, cpu, affinity) = {
        let scheduler = SCHEDULER.lock();
        let slot = scheduler.table.find_pid(pid)?;
        let policy = &scheduler.table.tasks()[slot];
        let (bottom, top) = scheduler.tasks[slot].stack;
        let blocked = policy.state == State::Blocked;
        (slot, blocked, policy.kill_pending, policy.stopped, bottom, top, policy.home, policy.affinity)
    };
    // The scan walks the whole stack, so it runs with interrupts on. If the task
    // exits in the meantime, the figure is merely out of date.
    Some(ProcessDetail {
        blocked,
        kill_pending,
        stack_size: top - bottom,
        stack_used: stack_high_water(bottom, top - bottom),
        heap_bytes: HEAP_BYTES[slot].load(Ordering::Acquire),
        stopped,
        cpu,
        affinity,
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
    let cpus = MAX_CPUS.min(feos_app::MAX_CPUS);
    cpu_ticks[..cpus].copy_from_slice(&table.cpu_total_ticks[..cpus]);
    cpu_idle_ticks[..cpus].copy_from_slice(&table.cpu_idle_ticks[..cpus]);

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

    fn spawn_argv(
        &self,
        app: &'static dyn App,
        argv: &[&str],
        io: SpawnIo<'_>,
    ) -> Result<u32, SpawnError> {
        spawn_app_argv(app, argv, io)
    }

    fn spawn_task(
        &self,
        name: &str,
        io: SpawnIo<'_>,
        task: Box<dyn FnOnce() -> i32 + Send>,
    ) -> Result<u32, SpawnError> {
        spawn_entry(name, Entry::Task(task), io, RawStreams::default())
    }

    fn spawn_user(
        &self,
        path: &str,
        argv: &[&str],
        cwd: &str,
        io: SpawnIo<'_>,
    ) -> Result<u32, SpawnError> {
        let mut block = ArgBlock::new();
        if !block.push(path) || !block.push_all(argv) {
            return Err(SpawnError::ArgsTooLong);
        }
        spawn_user(path, block, cwd, io, RawStreams::default())
    }

    fn wait(&self, pid: u32) -> Option<i32> {
        wait_for(pid)
    }

    fn try_wait(&self, pid: u32) -> WaitStatus {
        try_wait(pid)
    }

    fn stdin(&self) -> Option<PipeId> {
        current_stdin()
    }

    fn catch_signals(&self, mask: u32) {
        if let Some(slot) = with_current_slot() {
            CAUGHT[slot].store(mask, Ordering::Release);
        }
    }

    fn signal(&self, pid: u32, number: u32) -> bool {
        offer_signal(pid, number)
    }

    fn take_signals(&self) -> u32 {
        with_current_slot().map_or(0, |slot| PENDING_SIGNALS[slot].swap(0, Ordering::AcqRel))
    }

    fn cpu_ms(&self) -> u64 {
        let Some(slot) = with_current_slot() else {
            return 0;
        };
        let (own, hz) = {
            let scheduler = SCHEDULER.lock();
            (scheduler.table.tasks()[slot].cpu_ticks, scheduler.ticks_per_second.max(1))
        };
        own.wrapping_add(CHILD_TICKS[slot].load(Ordering::Acquire)).saturating_mul(1000) / hz
    }

    fn set_stopped(&self, pid: u32, stopped: bool) -> bool {
        let (done, idle) = {
            let mut scheduler = SCHEDULER.lock();
            let done = scheduler.table.set_stopped(pid, stopped);
            // A program in a call stops on its way back to the program, which looks.
            if let (true, true, Some(slot)) = (done, stopped, scheduler.table.find_pid(pid)) {
                ATTENTION[slot].store(true, Ordering::Release);
            }
            (done, scheduler.table.idle_cpu(this_cpu()))
        };
        kick(idle);
        done
    }

    fn set_priority(&self, pid: u32, priority: u8) -> bool {
        SCHEDULER.lock().table.set_priority(pid, priority)
    }

    fn set_affinity(&self, pid: u32, mask: u32) -> bool {
        let (done, idle) = {
            let mut scheduler = SCHEDULER.lock();
            let done = scheduler.table.set_affinity(pid, mask);
            (done, scheduler.table.idle_cpu(this_cpu()))
        };
        kick(idle);
        done
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
        Port::services().frequency()
    }

    fn ticks(&self) -> u64 {
        Port::services().ticks()
    }

    fn name(&self) -> &'static str {
        Port::services().name()
    }

    fn spin_ms(&self, ms: u64) {
        Port::services().spin_ms(ms);
    }

    fn idle_ms(&self, ms: u64) {
        sleep_ms(ms);
    }

    /// The hardware clock is read once and then counted forward from uptime: the
    /// taskbar asks every frame, and reading a CMOS clock is slow port I/O.
    fn unix_time(&self) -> Option<u64> {
        static BOOT_TIME: AtomicU64 = AtomicU64::new(0);
        let uptime = self.uptime_ms() / 1000;
        let known = BOOT_TIME.load(Ordering::Relaxed);
        if known != 0 {
            return Some(known + uptime);
        }
        let now = crate::Machine::unix_time()?;
        BOOT_TIME.store(now.saturating_sub(uptime), Ordering::Relaxed);
        Some(now)
    }
}

/// The clock installed in [`Platform`].
pub static CLOCK: KernelClock = KernelClock;

/// Starts the scheduler's timer on the boot CPU and unmasks IRQs.
///
/// The interrupt controller and IRQ handler must already be installed.
pub fn start(tick_hz: u64) {
    SCHEDULER.lock().ticks_per_second = tick_hz;
    STARTED.store(true, Ordering::Release);
    Port::timer_start(tick_hz);
    Port::irq_enable();
    start_reaper();
}

/// Starts the task that gives back what killed and failed tasks left behind. The
/// system needs it, so it can't be killed or stopped.
fn start_reaper() {
    match spawn_entry("reaper", Entry::Task(Box::new(reaper)), SpawnIo::default(), RawStreams::default()) {
        Ok(pid) => {
            let mut scheduler = SCHEDULER.lock();
            scheduler.table.set_protected(pid);
            scheduler.table.set_priority(pid, feos_sched::PRIORITY_NORMAL + 2);
        }
        Err(error) => feos_console::println!("[sched] no reaper: {error:?}"),
    }
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

    Port::timer_start(tick_hz);
    Port::irq_enable();

    loop {
        Port::wait_for_interrupt();
    }
}
