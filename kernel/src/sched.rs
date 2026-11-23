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
//! There are 1024 slots. Each but the shell's gets a stack of its own from `kstack` when
//! it's taken, and `feos-sched` keeps bitmaps of them, so that nothing that happens
//! often (a switch, a tick, a wake, collecting exits) looks at every slot.
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
//! # Threads
//!
//! A user program's process can have many tasks, its threads: each a slot of its own,
//! with its own kernel stack, frames and thread pointer, sharing the process (and so its
//! space and files) through the slot's reference to it. A thread's id is its slot's
//! [`SLOT_PID`]; its process's pid, the first thread's id, is [`SLOT_TGID`]. Killing,
//! stopping and waiting name the process and reach every thread (`feos-sched` has the
//! rules). A process's exit status is decided once, by `EXIT` or a fault in any of its
//! threads, which then has the others killed; its exit is recorded when its last thread
//! is collected, and not before.
//!
//! # Exit status and a task's console
//!
//! A task's exit status is a [`Status`]: an exit code, or the signal that ended it, which
//! each caller is told in its own encoding (v1 and kernel apps the code, or 128 plus the
//! signal). It goes into a small ring of recent exits once it has left the table and
//! been torn down, so a slot never waits for a parent to collect it: that's what kernel
//! apps wait on. A program started by a program has an exit record in its parent too
//! ([`crate::user::ChildRecord`]), which its last thread's exit fills in, here, without
//! allocating, and which keeps its pid from being given again until it's reaped. A task
//! can have pipes as its console: everything it prints goes there. Pipes need no heap and
//! record who holds them, so whatever a task held closes when it goes, even if a
//! kill gave it no chance to run its destructors.
//!
//! # Signals and stops
//!
//! A signal a program sends (`KILL`), or the shell does, is decided here, under the lock:
//! ignored, made pending for the process's handler (see [`crate::user::signal`], which
//! delivers it on the thread's way back to its program), or its default action: a
//! terminating one ends the process through the kill path, its status the signal; a
//! stop holds each thread at its next way back to its program, never in a call, and is
//! recorded for the parent once every thread has parked; a continue lets them go. A
//! default that ends or stops the process waits pending instead while every thread
//! blocks the signal, and `THREAD_KILL`'s go on the one thread.
//!
//! A signal made pending asks each thread that may take it to: one running on another
//! CPU by an IPI, one blocked by a wake. A wait in a call that a signal may end looks
//! for one with [`interrupted`], which claims a process signal for the thread that
//! looks, so of several woken exactly one leaves its call for it; and no program's
//! thread blocks while it has a signal due, so one that comes just before the block
//! isn't missed. `ITIMER_REAL`s go off in the `alarms` task.

extern crate alloc;

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr::addr_of;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use feos_app::pipe::{self, PipeHooks, PipeId};
use feos_app::{
    App, ArgBlock, Context, KillResult, Platform, ProcessDetail, ProcessInfo, ProcessState,
    Processes, SpawnError, SpawnIo, SystemInfo, WaitStatus, MAX_ARGC, MAX_PROCESSES,
    PROCESS_ARGS_MAX, PROCESS_NAME_MAX,
};
use feos_hal::{Arch, Board, Clock, IpiTarget, TrapContext};
use feos_sched::{KillOutcome, PidHolds, PidWindow, Request, State, Table, TaskFacts};
use feos_signal::{Action, Status};
use feos_sync::{IrqSpinLock, SchedHooks, TryArc};

use crate::{kstack, Frame, Machine, Port};

/// Number of process slots, including the shell.
const MAX_TASKS: usize = MAX_PROCESSES;

/// Number of CPUs the scheduler can run on.
const MAX_CPUS: usize = Port::MAX_CPUS;

// `SystemInfo` fixes the public array size; a port with more private CPUs
// would need to widen `feos_app::MAX_CPUS` as well.
const _: () = assert!(MAX_CPUS <= feos_app::MAX_CPUS);

/// Stack size used by a CPU's idle loop.
const IDLE_STACK_SIZE: usize = 16 * 1024;

/// How long a killed process with live heap may keep running to free it.
const KILL_GRACE_MS: u64 = 200;

/// Unused memory the linker script leaves below the boot stack, as a guard.
const BOOT_STACK_GAP: usize = 0x40000;

/// Exit status of a task ended for a CPU fault or a panic: `SIGSEGV`, which a shell
/// reports as 139.
const EXIT_FAULTED: Status = feos_signal::FAULTED;

/// What a killed task ends with, unless a signal decided otherwise: `SIGKILL`, 137.
const KILLED: Status = feos_signal::KILLED;

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

/// The TLB bookkeeping of the space each slot's task runs in, beside [`ROOT`]: which
/// CPUs have it, and its ASID. Null for the kernel's tables. It lives as long as the
/// slot's process does, which outlives the slot's having it here.
static TLBS: [AtomicPtr<crate::user::tlb::Tlb>; MAX_TASKS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_TASKS];

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
    /// A fault the page tables might answer, and what was tried.
    pub memory: Option<feos_hal::MemoryFault>,
    /// What kind of fault it was.
    pub name: &'static str,
    /// The instruction that faulted.
    pub instruction: u64,
    /// The signal the port says it raises, and its code, for a fault that isn't one the
    /// page tables might answer.
    pub signal: u32,
    pub code: i32,
}

struct FaultNotes([UnsafeCell<UserFault>; MAX_TASKS]);

// SAFETY: a slot's note is written by the trap that takes its task out of EL0 or ring 3
// and read by that task in `fault_entry`, which the write happens before.
unsafe impl Sync for FaultNotes {}

static FAULT_NOTES: FaultNotes = FaultNotes(
    [const { UnsafeCell::new(UserFault { address: None, memory: None, name: "", instruction: 0, signal: 0, code: 0 }) }; MAX_TASKS],
);

/// The task in each slot gave back what it held itself, on its way out, so there's
/// nothing left to queue for the reaper when it leaves the table.
static TORN_DOWN: [AtomicBool; MAX_TASKS] = [const { AtomicBool::new(false) }; MAX_TASKS];

/// Each slot's user thread pointer: what `SET_TLS` set, and on AArch64 whatever the
/// program left in `TPIDR_EL0`, saved every time the thread leaves its CPU, in a call
/// or not. Written back at every switch in, so no CPU caches whose it holds. Zero for
/// kernel-linked apps, which never use it.
static TLS: [AtomicUsize; MAX_TASKS] = [const { AtomicUsize::new(0) }; MAX_TASKS];

/// The word each user thread's end clears, for `pthread_join`: zero for none.
static CLEAR_WORD: [AtomicUsize; MAX_TASKS] = [const { AtomicUsize::new(0) }; MAX_TASKS];

/// Free slots a user program can't take, by starting threads or programs: room for the
/// kernel's own work, and for a shell to `kill` a thread bomb with. User programs stop at
/// 960 tasks machine-wide.
const KEEP_FOR_KERNEL: usize = 64;

/// Most threads one process may have, its first included: as many as user programs can
/// have at all, so the machine's limit is the one a bomb meets.
pub const MAX_THREADS: usize = MAX_TASKS - KEEP_FOR_KERNEL;

/// What a user program's new task may take: the slots above, and for a user other than
/// root, at most half of what's left as tasks, and a quarter as processes.
const USER_LIMITS: feos_sched::UserLimits = feos_sched::UserLimits {
    keep: KEEP_FOR_KERNEL,
    tasks_per_uid: 512,
    processes_per_uid: 256,
};

/// What the kernel keeps about a task beyond scheduling policy.
struct Task {
    name: [u8; PROCESS_NAME_MAX],
    name_len: usize,
    args: ArgBlock,
    app: Option<&'static dyn App>,
    /// What a task started by [`Processes::spawn_task`] runs, until it starts.
    entry: Option<Box<dyn FnOnce() -> i32 + Send>>,
    /// The program a user task loads and where it starts, on the heap since a path may
    /// be `PATH_MAX` long. Put here before the task is committed, and taken out by the
    /// task as it starts, both under the lock and so neither allocating nor freeing
    /// there: whatever was left in the slot is dropped once the lock is let go.
    user_start: Option<Box<crate::user::UserStart>>,
    /// Or, for a forked child, the copy of its parent it starts from, the same way. Two
    /// boxes rather than one of an enum: the table starts zeroed, which is `None` for both.
    forked: Option<Box<crate::user::Forked>>,
    is_user: bool,
    /// Lowest and highest stack addresses, set when the task is created.
    stack: (usize, usize),
    /// Uptime when the task was created.
    started_ms: u64,
    /// PID of the task that started this one.
    parent: u32,
    /// The slot of a thread of the parent, the one that started this task, where
    /// [`collect_exits`] looks for it first. It may have gone, and its slot been reused.
    parent_slot: u16,
    /// Spared by an interrupt to the parent.
    background: bool,
    /// The slot's PID hasn't had its exit recorded yet.
    live: bool,
    /// What [`collect_exits`] records. A kill leaves [`KILLED`] here.
    exit_code: Status,
    /// The status its process was given, by `EXIT` or a failure in any of its threads,
    /// which every thread of it keeps: what its exit is recorded with, whichever thread
    /// is last and however that one ended. Written under the scheduler lock for the
    /// whole process at once, so the record can't miss it.
    group_code: Option<Status>,
    /// Its process has run `EXEC`: a parent's `SETPGID` of it is `EACCES` from then on.
    /// The same in every thread of the process.
    execed: bool,
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
    status: Status,
    parent: u32,
    /// The process's last thread: its exit is the process's. A thread before it leaves
    /// only its teardown, which a wait still waits for.
    last: bool,
    /// The process's exit record in its parent, as [`RECORD`] had it, for its last thread
    /// only: filled in and given up as the exit is recorded. Zero for none.
    child: usize,
    /// Who it ran as, for its parent's `SIGCHLD`.
    uid: u32,
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
        // SAFETY: integers, arrays of those, null pointers, `None`, and the zero `State`,
        // `Wait` and `Status` variants are all valid as zero bytes.
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

/// Thread id in each slot, for attributing heap frees without the scheduler lock: the
/// heap records a block's owner by it, so a block is charged to the thread, which is one
/// slot, whatever its process.
static SLOT_PID: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];

/// The process of the thread in each slot: its first thread's id. The same as
/// [`SLOT_PID`] for kernel-linked apps and first threads. What pipes record a holder
/// by, since the files a thread holds are its process's.
static SLOT_TGID: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];

/// The pipes behind each slot's standard output and error, or zero for the
/// registered console. Written while the slot is reserved, read by its task.
static STDOUT: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];
/// The pipe behind each slot's standard input, or zero for none. Only user programs
/// read through this; kernel-linked apps are handed their streams.
static STDIN: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];
/// Who each task is. Everything starts as root, and only a user program can stop being.
/// Each slot's process group and session, the same in every thread of a process, and
/// changed only under the scheduler's lock (`SETPGID`, `SETSID`). The kernel's own tasks
/// are in group 1 and session 1, the shell's, which is the terminal's session too.
static SLOT_PGID: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];
static SLOT_SID: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];
/// Each thread's signal mask, bit `n - 1` for signal `n`: inherited by a new thread and
/// by a spawned program, or set by `SPAWN2`'s `SETSIGMASK`; a handler's entry adds to it
/// and its `SIGRETURN` restores it (stage 1c.9). Only the thread itself changes its own,
/// in a call, so the scheduler reads it lock-free.
static SIGNAL_MASK: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];

/// Signals pending for each thread alone (section 8.1 of the ABI design): `SIGPIPE` from
/// its own write, and what `THREAD_KILL` sends it. Cleared as the slot is reserved.
static THREAD_PENDING: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];

/// Of each thread's [`THREAD_PENDING`], the signals `THREAD_KILL` sent, which a handler
/// is told came from its own process (`SI_TKILL`), rather than the kernel's `SIGPIPE`.
static TKILLED: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];

/// Signals of its process's that each thread has claimed for itself as a wait it was
/// in was interrupted for them ([`interrupted`]), so of several threads woken for one
/// process signal exactly one leaves its call for it. Taken as the call returns,
/// whatever the mask says by then; given back to the process if the thread ends first.
static CLAIMED: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];

/// Each thread's alternate signal stack (`SIGALTSTACK`): its lowest address, and its
/// size, 0 for none. Only the thread itself changes or reads its own, in the kernel.
static ALT_BASE: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];
static ALT_SIZE: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];

/// The mask a thread's `SIGSUSPEND` stood in for, to go back as the handler that ends
/// the wait is entered (in its frame, so its `SIGRETURN` restores it), or after delivery
/// if none is: [`NO_MASK`] when there's none. Only the thread itself uses its own.
static SUSPENDED_MASK: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(NO_MASK) }; MAX_TASKS];

/// In [`SUSPENDED_MASK`]: no mask, which no real one can be, as it would block `SIGKILL`.
const NO_MASK: u64 = u64::MAX;

/// Each slot's process's signal state ([`crate::user::signal::Signals`]), or null: set
/// as the slot is given its process, and cleared under this module's lock before the
/// slot lets its reference to the process go, so whatever sees it under the lock sees a
/// process that's alive. What generation (`KILL`, `SIGCHLD`) and the resume-time check
/// read without the process's signal lock.
static SIGNALS: [AtomicPtr<crate::user::signal::Signals>; MAX_TASKS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_TASKS];

/// The signals each new program's process starts out ignoring: its spawner's ignored set,
/// less `SPAWN2`'s `SETSIGDEF`. Written as the slot is reserved; also what a signal sent
/// to the program before its process is made goes by ([`act_on`]).
static INHERIT_IGNORED: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];

/// The session and group the terminal's programs are in: the shell's.
const TERMINAL_SESSION: u32 = 1;

/// The exit record each slot's process has in a parent program (see
/// [`crate::user::ChildRecord`]), or null: one reference for the whole thread group,
/// which every thread's slot points at without counting, given up by the last thread's
/// exit ([`record_exit`]). Set as a slot's task is committed, under the scheduler's lock.
static RECORD: [AtomicPtr<crate::user::ChildRecord>; MAX_TASKS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_TASKS];

/// Where each slot's process is in a stop by a signal, the same in every thread of it:
/// 0 for none, the signal while the stop waits for every thread to park, and the signal
/// with [`STOP_REPORTED`] once it has, and the stop is recorded for the parent. Changed
/// under the scheduler's lock.
static STOP_STATE: [AtomicU8; MAX_TASKS] = [const { AtomicU8::new(0) }; MAX_TASKS];

/// In [`STOP_STATE`]: every thread has parked, and the stop is reported.
const STOP_REPORTED: u8 = 0x80;

static TASK_UID: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];
static TASK_GID: [AtomicU32; MAX_TASKS] = [const { AtomicU32::new(0) }; MAX_TASKS];

/// Who the task in `slot` is.
pub fn credentials(slot: usize) -> (u32, u32) {
    (TASK_UID[slot].load(Ordering::Relaxed), TASK_GID[slot].load(Ordering::Relaxed))
}

/// Sets who the process of the task in `slot` is, every thread of it, and what it starts
/// from now on. Under the scheduler lock, so a thread being started takes either the old
/// or the new, and is counted here if it took the old.
pub fn set_credentials(slot: usize, uid: u32, gid: u32) {
    let mut scheduler = SCHEDULER.lock();
    let tgid = SLOT_TGID[slot].load(Ordering::Relaxed);
    // The caps count by the table's.
    scheduler.table.set_uid(tgid, uid);
    for other in 0..MAX_TASKS {
        let task = &scheduler.table.tasks()[other];
        if other == slot || (task.state != State::Free && task.tgid == tgid) {
            TASK_UID[other].store(uid, Ordering::Relaxed);
            TASK_GID[other].store(gid, Ordering::Relaxed);
        }
    }
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
    /// Block on a user program's futex word.
    WaitUser(UserWait),
    Exit,
    /// Continue at EL0 or ring 3, from the slot's user frame.
    ToUser,
}

/// A user futex wait, for the scheduler to check and carry out under its lock.
#[derive(Clone, Copy)]
struct UserWait {
    /// The kernel's address of the word, whose frame the waiter has pinned.
    word: usize,
    key: feos_sched::UserKey,
    expected: u32,
    /// The space's unmap count, and what the waiter saw of it with the word's key.
    unmaps: usize,
    seen: u32,
    deadline: Option<u64>,
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

/// Holds off a kill of the calling task for as long as it's alive, as a lock does: for
/// a change that a kill part way through would leave half made, with nothing to finish
/// or undo it. A failure under it stops the machine, as under any lock.
pub(crate) struct NoKill;

impl NoKill {
    pub(crate) fn new() -> Self {
        lock_enter();
        Self
    }
}

impl Drop for NoKill {
    fn drop(&mut self) {
        lock_exit();
        take_kill_now();
    }
}

/// A kill that waited for a [`NoKill`] to go takes effect now, rather than at whatever
/// switch next finds the task outside one: a kernel app that takes one after another
/// (`ps` reading every stack) is almost never outside one when a tick comes, and would
/// otherwise outlive its kill indefinitely. Only a kernel app with no other lock and
/// interrupts on: a program's thread in a call goes at its way back to its program.
fn take_kill_now() {
    if !can_block() {
        return;
    }
    let flags = Port::irq_save_and_disable();
    let due = current_slot().is_some_and(|slot| {
        slot != 0
            && ATTENTION[slot].load(Ordering::Acquire)
            && LOCK_DEPTH[slot].load(Ordering::Acquire) == 0
            && IN_CALL[slot].load(Ordering::Acquire) == 0
    });
    Port::irq_restore(flags);
    // The switch reaps it, if nothing else (live heap in its grace) holds the kill off.
    if due && kill_requested() {
        yield_now();
    }
}

/// The calling task's slot, if it has one: the same whichever CPU it's on.
pub fn caller_slot() -> Option<usize> {
    let flags = Port::irq_save_and_disable();
    let slot = current_slot();
    Port::irq_restore(flags);
    slot
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

/// `ms` milliseconds in timer ticks, saturating: a length that comes from a program can
/// be anything, and one too long to count is forever rather than whatever it wraps to.
fn ms_to_ticks(ms: u64) -> u64 {
    u64::try_from(Port::services().frequency() as u128 * ms as u128 / 1000).unwrap_or(u64::MAX)
}

fn futex_wait_ms(word: &AtomicU32, expected: u32, ms: u64) {
    if can_block() {
        let delta = ms_to_ticks(ms);
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

    let canary_end = bottom + kstack::CANARY_WORDS * core::mem::size_of::<u64>();
    let sp = frame.stack_pointer();
    let stack = bottom as *const u64;
    // SAFETY: the canary words are part of this task's stack, which it has while it runs.
    let intact = (0..kstack::CANARY_WORDS).all(|word| unsafe { stack.add(word).read_volatile() } == kstack::CANARY);
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
            // A thread of a program failing in its call ends the whole process: what the
            // others share with it, its files and its space, may be half changed.
            if ROOT[slot].load(Ordering::Relaxed) != 0 {
                end_group(slot, EXIT_FAULTED);
            }
        }
    }
    contained
}

/// Decides the exit status of the process of the user thread in `slot`, unless a thread
/// of it already has, and asks each of its other threads to end, as a kill does: one in a
/// call unwinds it first, one at its program goes at once. Every thread of it keeps the
/// status, so whichever is last records it. The caller then ends its own thread.
fn end_group(slot: usize, code: Status) {
    let grace = Port::services().frequency() / 1000 * KILL_GRACE_MS;
    let now = Port::services().ticks();
    let idle = {
        let mut scheduler = SCHEDULER.lock();
        let scheduler = &mut *scheduler;
        let tgid = scheduler.table.tasks()[slot].tgid;
        let code = scheduler.tasks[slot].group_code.unwrap_or(code);
        for other in 0..MAX_TASKS {
            let task = scheduler.table.tasks()[other];
            if task.tgid != tgid || (task.state == State::Free && !scheduler.tasks[other].live) {
                continue;
            }
            scheduler.tasks[other].group_code = Some(code);
            if other != slot && scheduler.table.kill_slot(other, now, grace, &Facts) == KillOutcome::Pending {
                ATTENTION[other].store(true, Ordering::Release);
            }
        }
        let collected = collect_exits(scheduler);
        announce(scheduler, collected).or(scheduler.table.idle_cpu(this_cpu()))
    };
    kick(idle);
}

/// If `address` is in the guard page below the current task's stack, the bounds of
/// that stack. For the fault path, so it must not wait.
pub fn stack_overflow_at(address: u64) -> Option<(usize, usize)> {
    let slot = current_slot()?;
    let address = address as usize;
    if slot != 0 {
        return kstack::overflow_of(slot, address);
    }
    let (bottom, top) = Machine::stack_range();
    (bottom.saturating_sub(BOOT_STACK_GAP)..bottom).contains(&address).then_some((bottom, top))
}

/// Ends the task whose fault trapped into `frame`, and swaps in the next task's.
pub fn end_task_from_trap(frame: &mut Frame) {
    reschedule(frame, || Request::Exit, false);
}

/// Ends the calling task, from a panic in it.
pub fn end_task() -> ! {
    exit_task(EXIT_FAULTED, false)
}

/// Takes the unused memory below the boot stack out of the page tables, so that
/// overflowing it is a fault at the time, not corruption found out later. Every other
/// task's stack is in the window `kstack` keeps, with a gap below it that's never mapped.
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
    let mapped = unsafe {
        Port::unmap_range(boot_bottom - BOOT_STACK_GAP, BOOT_STACK_GAP, &mut alloc_table)
    };
    // The stack would still work without its guard, with the canaries as a backstop.
    if !mapped {
        feos_console::println!("[mem] the boot stack's guard couldn't be mapped");
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
    unsafe { kstack::paint(bottom, here.saturating_sub(16 * 1024).max(bottom)) };

    let mut scheduler = SCHEDULER.lock();
    scheduler.table.init();
    scheduler.table.set_pid_holds(PID_HOLDS);
    scheduler.ticks_per_second = 100;
    let shell = &mut scheduler.tasks[0];
    shell.set_name("shell");
    shell.stack = (bottom, top);

    SLOT_PID[0].store(1, Ordering::Release);
    CURRENT[0].store(1, Ordering::Release);
    Port::set_trap_frame(TASK_FRAMES.0[0].get());
    feos_sync::install_hooks(&HOOKS);
    SLOT_TGID[0].store(1, Ordering::Release);
    SLOT_PGID[0].store(TERMINAL_SESSION, Ordering::Release);
    SLOT_SID[0].store(TERMINAL_SESSION, Ordering::Release);
    pipe::install_hooks(PipeHooks {
        current_pid: current_tgid,
        interrupted,
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

/// Loads the tables of the task in `slot` on this CPU, `cpu`, or the kernel's with no
/// slot. With interrupts masked.
fn load_tables(cpu: usize, slot: Option<usize>) {
    match slot {
        Some(slot) => crate::user::tlb::load(cpu, ROOT[slot].load(Ordering::Relaxed), TLBS[slot].load(Ordering::Relaxed)),
        None => crate::user::tlb::load(cpu, 0, core::ptr::null()),
    }
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

        let outgoing = scheduler.table.current(cpu);
        let switch = scheduler.table.switch(cpu, request, now, &Facts);
        // A thread of a process stopping has just left its CPU: perhaps the last that
        // hadn't parked.
        if let Some(slot) = outgoing {
            let stop = STOP_STATE[slot].load(Ordering::Relaxed);
            if stop != 0 && stop & STOP_REPORTED == 0 {
                check_parked(scheduler, slot);
            }
        }
        // The thread pointer is a thread's: kept as one leaves the CPU, where a program
        // can write it itself, and set as the next comes, always, so nothing a program
        // left in the register (or, on x86_64, a selector load left in the FS base) is
        // ever another's. Before the lock goes, so no other CPU runs the outgoing thread
        // with what it had not yet saved.
        if switch.next != outgoing {
            if let (Some(slot), true) = (outgoing, Port::USER_TLS_WRITABLE) {
                TLS[slot].store(Port::user_tls(), Ordering::Relaxed);
            }
            if let Some(slot) = switch.next {
                Port::set_user_tls(TLS[slot].load(Ordering::Relaxed));
            }
        }

        match switch.next {
            // A program's thread resumed at its program with a signal due for it takes it
            // first, in the kernel: whether it was preempted there or carries on.
            Some(slot) if IN_USER[slot].load(Ordering::Relaxed) && signal_due(slot) => {
                load_tables(cpu, Some(slot));
                Port::resume(divert_to_signals(slot, false));
                if switch.save != Some(slot) {
                    CURRENT[cpu].store(slot + 1, Ordering::Relaxed);
                }
            }
            // The outgoing task carries on, from the frame the trap saved: unless that
            // was its kernel half, on its way back to its program.
            Some(slot) if switch.save == Some(slot) => {
                let resume = resume_frame(slot);
                if !core::ptr::eq(frame as *const Frame, resume) {
                    // Its tables too, as on the way out of `on_switch_request`: a
                    // program's first entry comes here from a task that loaded it on
                    // the kernel's tables.
                    load_tables(cpu, Some(slot));
                    Port::resume(resume);
                }
            }
            Some(slot) => {
                load_tables(cpu, Some(slot));
                Port::resume(resume_frame(slot));
                CURRENT[cpu].store(slot + 1, Ordering::Relaxed);
            }
            None => {
                // No runnable process: park this CPU on its private idle stack.
                load_tables(cpu, None);
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
    /// The task's reference to its process, dropped in task context.
    process: UnsafeCell<Option<alloc::sync::Arc<crate::user::Process>>>,
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
        record: UnsafeCell::new(ExitRecord { pid: 0, status: Status::Exited(0), parent: 0, last: false, child: 0, uid: 0 }),
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
    /// A teardown was queued, or a stack left for the reaper to give back.
    queued: bool,
}

/// Records the exit of every task that has left the table and has given back what it
/// held, and queues the rest for teardown. Runs under the scheduler lock, often in a
/// trap, so it only moves things: see [`drain_teardowns`]. The table's bitmap of slots
/// that have left says which to look at, so this costs next to nothing when none have.
fn collect_exits(scheduler: &mut Scheduler) -> Collected {
    let mut collected = Collected { complete: true, ..Collected::default() };
    let left = scheduler.table.left();
    for slot in left.iter() {
        if slot == 0 || !scheduler.tasks[slot].live || scheduler.table.tasks()[slot].state != State::Free {
            scheduler.table.forget_left(slot);
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
        // the slot is next reserved. A thread's parent is its process's.
        let spent = scheduler.table.tasks()[slot].cpu_ticks.wrapping_add(CHILD_TICKS[slot].load(Ordering::Relaxed));
        // The parent is looked for in the slot of the task that started this one first,
        // so collecting a thousand threads' exits doesn't scan the table for each. Only
        // when that task has gone, and its slot with it, are the parent's other threads
        // searched for.
        let parent = scheduler.tasks[slot].parent;
        let is_parent = |other: usize| {
            other != slot && (other == 0 || scheduler.tasks[other].live) && SLOT_TGID[other].load(Ordering::Relaxed) == parent
        };
        let starter = usize::from(scheduler.tasks[slot].parent_slot);
        let parent_slot = if is_parent(starter) { Some(starter) } else { (0..MAX_TASKS).find(|&other| is_parent(other)) };
        if let Some(parent_slot) = parent_slot {
            CHILD_TICKS[parent_slot].fetch_add(spent, Ordering::Relaxed);
        }
        // Only the last of a process's threads to be collected records its exit: before
        // that, the process is still there.
        let last = scheduler.table.last_of_group(slot);
        let task = &mut scheduler.tasks[slot];
        task.live = false;
        STOP_STATE[slot].store(0, Ordering::Relaxed);
        let own = if FAULTED[slot].load(Ordering::Acquire) { EXIT_FAULTED } else { task.exit_code };
        // Its process's record in a parent program: every thread's time goes into it, and
        // the last thread's end makes it whole.
        let child = RECORD[slot].swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !child.is_null() {
            // SAFETY: the record's thread group holds it until its last thread is
            // recorded, which is this one or one after it.
            unsafe { (*child).add_ticks(scheduler.table.tasks()[slot].cpu_ticks) };
        }
        let record = ExitRecord {
            pid: SLOT_TGID[slot].load(Ordering::Relaxed),
            status: task.group_code.unwrap_or(own),
            parent: task.parent,
            last,
            child: if last { child as usize } else { 0 },
            uid: TASK_UID[slot].load(Ordering::Relaxed),
        };
        // Nothing runs on these tables now: every CPU that did changed tables before
        // it let the slot go. The space's ASID is flushed when the space goes.
        ROOT[slot].store(0, Ordering::Relaxed);
        TLBS[slot].store(core::ptr::null_mut(), Ordering::Relaxed);
        IN_USER[slot].store(false, Ordering::Relaxed);
        // Nothing runs on its stack either: the reaper gives it back.
        kstack::idle(slot);
        scheduler.table.forget_left(slot);
        collected.queued = true;
        match entry {
            None if last => {
                record_exit(scheduler, record);
                collected.recorded = true;
            }
            None => {}
            Some(entry) => {
                // SAFETY: the entry is empty, and only this, under the scheduler lock,
                // fills one.
                let process = crate::user::take_process(slot);
                SIGNALS[slot].store(core::ptr::null_mut(), Ordering::Release);
                // Its CPU time passes to its process as it stops being live.
                if let Some(process) = &process {
                    process.fold_ticks(scheduler.table.tasks()[slot].cpu_ticks);
                }
                unsafe {
                    *entry.record.get() = record;
                    *entry.process.get() = process;
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
    scheduler.exits[next] = ExitRecord { child: 0, ..record };
    scheduler.next_exit = (next + 1) % EXIT_RECORDS;
    let child = record.child as *const crate::user::ChildRecord;
    if !child.is_null() {
        // SAFETY: the thread group's reference, which this, its last thread's exit, gives
        // up: onto a list, since nothing may be freed under this lock.
        unsafe {
            (*child).ended(record.status);
            // A parent that ignores `SIGCHLD`, or has `SA_NOCLDWAIT`, never waits for it:
            // the record is dropped from its list as it next looks, not kept for `WAIT4`.
            let parent = program_slot(scheduler, record.parent);
            if parent.and_then(signals_of).is_some_and(|signals| {
                signals.child_flags.load(Ordering::Acquire) & crate::user::signal::NOCLDWAIT != 0
            }) {
                (*child).detach();
            }
            // Raised once the record is complete, so the handler's `WAIT4` finds it.
            let ticks = (*child).usage().ticks;
            raise_child(scheduler, record.parent, record.status, record.pid, record.uid, ticks);
            crate::user::release_record(child);
        }
    }
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
        give_back(process, record.pid);
        finish_teardown(entry, record);
    }
    // The exit records those let go of, if they were the last to hold them.
    crate::user::drain_records();
}

/// What a task gives back as it goes: its reference to its process, whose last gives
/// back the process's files, pipes and, with its space's last, its memory; or, for a
/// task with no process, the pipes it held as `pid`. In task context, with interrupts on.
fn give_back(process: Option<alloc::sync::Arc<crate::user::Process>>, pid: u32) {
    match process {
        Some(process) => drop(process),
        None => pipe::release_pid(pid),
    }
}

/// Records the exit of a task whose teardown is done, if it was its process's last, and
/// empties its queue entry. Waiters are woken either way: a wait for a process waits for
/// every thread's teardown.
fn finish_teardown(entry: &Teardown, record: ExitRecord) {
    let idle = {
        let mut scheduler = SCHEDULER.lock();
        if record.last {
            record_exit(&mut scheduler, record);
        }
        entry.state.store(EMPTY, Ordering::Release);
        announce(&mut scheduler, Collected { complete: true, recorded: true, queued: false })
    };
    kick(idle);
}

/// What the kernel holds pids by, besides live tasks' own, for the table, which asks
/// under the scheduler's lock and only once its pid counter has wrapped. An exit record
/// nobody has reaped or dropped holds its pid and its group's, by a lookup (whose
/// buckets are fewer than [`feos_sched::PID_TRIES`], so a pid is found while one is
/// free); a teardown not yet done, and a live group or session whose leader has gone,
/// are scanned for once for each window of pids, not for each pid tried.
const PID_HOLDS: PidHolds = PidHolds { maybe: crate::user::pid_held, mark: mark_held };

fn mark_held(window: &mut PidWindow, tasks: &[feos_sched::Task]) {
    for entry in &TEARDOWNS {
        if entry.state.load(Ordering::Acquire) != EMPTY {
            // SAFETY: see `Teardown`: the caller holds the scheduler lock.
            window.mark(unsafe { (*entry.record.get()).pid });
        }
    }
    for (slot, task) in tasks.iter().enumerate() {
        if task.state != State::Free {
            window.mark(SLOT_PGID[slot].load(Ordering::Relaxed));
            window.mark(SLOT_SID[slot].load(Ordering::Relaxed));
        }
    }
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
        TLBS[slot].store(core::ptr::null_mut(), Ordering::Relaxed);
        load_tables(this_cpu(), None);
    }
    Port::irq_restore(flags);
    note_teardown(&TEARDOWNS_SELF);
    let process = crate::user::take_process(slot);
    {
        // Its CPU time so far passes to its process, and from now on it isn't counted as
        // live (see `process_cpu_ns`): both under the lock the count is read under. What it
        // runs from here to its end is too little to matter.
        let scheduler = SCHEDULER.lock();
        if let Some(process) = &process {
            process.fold_ticks(scheduler.table.tasks()[slot].cpu_ticks);
        }
        // Nothing under the lock looks at its process from here, before it's let go.
        SIGNALS[slot].store(core::ptr::null_mut(), Ordering::Release);
        TORN_DOWN[slot].store(true, Ordering::Release);
    }
    give_back(process, SLOT_TGID[slot].load(Ordering::Acquire));
}

/// The reaper: a kernel task that gives back what killed and failed tasks left behind,
/// and the stacks of every task that has gone.
fn reaper() -> i32 {
    loop {
        let seen = TEARDOWN_SEQUENCE.load(Ordering::Acquire);
        drain_teardowns();
        crate::user::drain_records();
        kstack::trim();
        futex_wait(&TEARDOWN_SEQUENCE, seen);
    }
}

/// Gives back the stacks of tasks that have gone, now: for counting free frames, which
/// shouldn't depend on whether the reaper has had its turn yet.
pub fn trim_stacks() {
    kstack::trim();
}

/// Whether this CPU is in its idle loop. Call with interrupts masked.
pub fn is_idle() -> bool {
    current_slot().is_none()
}

/// The signal state of the process of the task in `slot`, if it's a program's. Only under
/// the scheduler's lock, or from the slot's own task: see [`SIGNALS`].
fn signals_of(slot: usize) -> Option<&'static crate::user::signal::Signals> {
    // SAFETY: see `SIGNALS`: what it points at lives while the slot holds its process,
    // which it does until the pointer is cleared under the lock the caller holds (or the
    // caller is the slot's own task, which holds it).
    unsafe { SIGNALS[slot].load(Ordering::Acquire).as_ref() }
}

/// Points `slot` at its process's signal state, or at nothing: as it's given its process,
/// by the task itself or while the slot is reserved.
pub fn publish_signals(slot: usize, signals: *const crate::user::signal::Signals) {
    SIGNALS[slot].store(signals.cast_mut(), Ordering::Release);
}

/// The signals the new program in `slot` starts out ignoring.
pub fn inherited_ignored(slot: usize) -> u64 {
    INHERIT_IGNORED[slot].load(Ordering::Acquire)
}

/// The signal mask of the thread in `slot`.
pub fn signal_mask(slot: usize) -> u64 {
    SIGNAL_MASK[slot].load(Ordering::Acquire)
}

/// Sets the signal mask of the thread in `slot`, the caller: never `SIGKILL` or `SIGSTOP`.
pub fn set_signal_mask(slot: usize, mask: u64) {
    SIGNAL_MASK[slot].store(mask & !crate::user::signal::UNBLOCKABLE, Ordering::Release);
}

/// The signals pending for the thread in `slot` alone.
pub fn thread_pending(slot: usize) -> &'static AtomicU64 {
    &THREAD_PENDING[slot]
}

/// Of those, the ones `THREAD_KILL` sent.
pub fn tkilled(slot: usize) -> &'static AtomicU64 {
    &TKILLED[slot]
}

/// The process signals the thread in `slot` has claimed for itself: see [`CLAIMED`].
pub fn claimed(slot: usize) -> &'static AtomicU64 {
    &CLAIMED[slot]
}

/// The alternate signal stack of the thread in `slot`, the caller: its lowest address
/// and its size, 0 for none.
pub fn alt_stack(slot: usize) -> (u64, u64) {
    (ALT_BASE[slot].load(Ordering::Relaxed), ALT_SIZE[slot].load(Ordering::Relaxed))
}

/// Sets the alternate signal stack of the thread in `slot`, the caller; a size of 0 for
/// none.
pub fn set_alt_stack(slot: usize, base: u64, size: u64) {
    ALT_BASE[slot].store(if size == 0 { 0 } else { base }, Ordering::Relaxed);
    ALT_SIZE[slot].store(size, Ordering::Relaxed);
}

/// Notes the mask a `SIGSUSPEND` of the thread in `slot`, the caller, stands in for.
pub fn set_suspended_mask(slot: usize, mask: u64) {
    SUSPENDED_MASK[slot].store(mask & !crate::user::signal::UNBLOCKABLE, Ordering::Relaxed);
}

/// The mask a `SIGSUSPEND` of the thread in `slot`, the caller, stood in for, if one is
/// still to go back.
pub fn take_suspended_mask(slot: usize) -> Option<u64> {
    Some(SUSPENDED_MASK[slot].swap(NO_MASK, Ordering::Relaxed)).filter(|&mask| mask != NO_MASK)
}

/// A new thread's or program's signal state in `slot`, as it's reserved: `mask`, and
/// nothing pending, claimed or suspended, and no alternate stack.
fn reset_signal_slot(slot: usize, mask: u64) {
    SIGNAL_MASK[slot].store(mask & !crate::user::signal::UNBLOCKABLE, Ordering::Release);
    THREAD_PENDING[slot].store(0, Ordering::Release);
    TKILLED[slot].store(0, Ordering::Release);
    CLAIMED[slot].store(0, Ordering::Release);
    ALT_BASE[slot].store(0, Ordering::Release);
    ALT_SIZE[slot].store(0, Ordering::Release);
    SUSPENDED_MASK[slot].store(NO_MASK, Ordering::Release);
}

/// The user frame of the thread in `slot`, the caller, which is in the kernel: nothing
/// else reads or writes it until the thread goes back to its program.
pub fn user_frame(slot: usize) -> *mut Frame {
    USER_FRAMES.0[slot].get()
}

/// Whether the thread in `slot` has a signal it doesn't block pending, for itself or its
/// process: lock-free, from the resume paths.
fn signal_due(slot: usize) -> bool {
    let Some(signals) = signals_of(slot) else {
        return false;
    };
    if CLAIMED[slot].load(Ordering::Acquire) != 0 {
        return true;
    }
    let pending = THREAD_PENDING[slot].load(Ordering::Acquire) | signals.pending.load(Ordering::Acquire);
    pending & !SIGNAL_MASK[slot].load(Ordering::Acquire) != 0
}

/// Whether the user thread in `slot`, the caller, in a call, has a signal to take that
/// a wait in that call should end for (section 8.3 of the ABI design): one of its own
/// it doesn't block, one it has claimed already, or one of its process's it doesn't
/// block, which it then claims, so that of several threads looking only one leaves its
/// wait for it. Lock-free.
fn signal_interrupts(slot: usize) -> bool {
    let Some(signals) = signals_of(slot) else {
        return false;
    };
    let mask = SIGNAL_MASK[slot].load(Ordering::Acquire);
    if CLAIMED[slot].load(Ordering::Acquire) != 0 || THREAD_PENDING[slot].load(Ordering::Acquire) & !mask != 0 {
        return true;
    }
    match feos_signal::claim(&signals.pending, mask) {
        Some(signal) => {
            CLAIMED[slot].fetch_or(feos_signal::bit(signal), Ordering::AcqRel);
            true
        }
        None => false,
    }
}

/// Whether the calling task's wait should end: it's being killed, or it's a program's
/// thread with a signal to take (stage 1c.10). Kernel apps have only the first. The
/// `interrupted` hook of every wait a call can make that a signal may cut short; a call
/// that gives up for it returns [`interrupted_result`].
pub(crate) fn interrupted() -> bool {
    kill_requested() || caller_slot().is_some_and(signal_interrupts)
}

/// What a call whose wait was [interrupted](interrupted) with nothing done returns:
/// `EINTR` for a signal (or the call made again, as its handler says), and for a kill
/// anything, as the task ends on its way out and nobody sees it.
pub fn interrupted_result() -> isize {
    -feos_abi::errno::EINTR
}

/// A wait that only a signal (or a kill) ends, for `SIGSUSPEND`: on a word nothing
/// wakes, so only [`notify_signal`]'s wake of the thread, or a kill's, ends it.
pub fn wait_for_signal() {
    static NEVER: AtomicU32 = AtomicU32::new(0);
    while !interrupted() {
        futex_wait(&NEVER, 0);
    }
}

/// Hands the process signals the thread in `slot`, the caller, claimed back to its
/// process, as the thread ends without having taken them, and asks the others to take
/// them.
fn give_back_claimed(slot: usize) {
    let claimed = CLAIMED[slot].swap(0, Ordering::AcqRel);
    if claimed == 0 {
        return;
    }
    let Some(signals) = signals_of(slot) else {
        return;
    };
    signals.pending.fetch_or(claimed, Ordering::AcqRel);
    let cpus = {
        let mut scheduler = SCHEDULER.lock();
        notify_signal(&mut scheduler, slot, claimed)
    };
    reschedule_cpus(cpus);
}

/// Diverts the resume of the user thread in `slot`, about to go back to its program from
/// its user frame, into [`signal_entry`], a kernel thread at the top of its kernel stack,
/// which delivers what's due and then goes back. On the CPU about to run it, with
/// interrupts masked; the kernel stack is free, as the thread is at its program.
/// `in_call` says the thread is still counted in a call (the way out of one).
fn divert_to_signals(slot: usize, in_call: bool) -> *mut Frame {
    if !in_call {
        IN_USER[slot].store(false, Ordering::Relaxed);
        IN_CALL[slot].fetch_add(1, Ordering::AcqRel);
    }
    let thread = Frame::new_task(signal_entry as *const () as usize, slot, kstack::top(slot), task_exit as *const () as usize);
    // SAFETY: the task is this CPU's to run, and nothing is using its kernel frame: it's
    // either at its program or on its way back to it from a call that has finished.
    unsafe { TASK_FRAMES.0[slot].get().write(thread) };
    TASK_FRAMES.0[slot].get()
}

/// Delivers the signals due for a user thread that was on its way back to its program,
/// then goes back.
extern "C" fn signal_entry(slot: usize) -> ! {
    end_if_killed(slot);
    return_to_user(slot)
}

/// Ends the process of the thread in `slot`, the caller, with `signal`, as a signal's
/// default action or a fault it can't be given a handler for does: every other thread is
/// killed, and the status is the signal's (unless the process already had one). A
/// `SIGSEGV` counts as a failure, as a fault always did.
pub fn end_by_signal(slot: usize, signal: u32) -> ! {
    let status = Status::signaled(signal);
    if signal == feos_abi::sig::SEGV {
        FAULTED[slot].store(true, Ordering::Release);
    }
    end_group(slot, status);
    exit_task(status, true)
}

/// Stops the caller's own process for `signal`, a stop signal taken with its default
/// action: the caller parks on its way back to its program, with the others.
pub fn stop_own_process(slot: usize, signal: u32) {
    let cpus = {
        let mut scheduler = SCHEDULER.lock();
        stop_process(&mut scheduler, slot, signal)
    };
    reschedule_cpus(cpus);
}

/// Discards `bits` from what every thread of the process of the thread in `slot` has
/// pending, as `SIGACTION` sets them ignored. Under the process's signal lock.
pub fn discard_thread_signal(slot: usize, bits: u64) {
    let scheduler = SCHEDULER.lock();
    for member in live_members(&scheduler, slot) {
        THREAD_PENDING[member].fetch_and(!bits, Ordering::AcqRel);
        TKILLED[member].fetch_and(!bits, Ordering::AcqRel);
    }
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
            // A signal that came after the call's last look at them (section 8.3 of the
            // ABI design: a call's return never skips one): back into the kernel to take
            // it, still in the call.
            if signal_due(slot) {
                Port::resume(divert_to_signals(slot, true));
                return;
            }
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
            load_tables(this_cpu(), Some(slot));
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
        PendingRequest::WaitUser(wait) => {
            // SAFETY: the word's frame is pinned, and the count is the waiter's space's,
            // which its blocked task keeps. An unmap moves the count and then takes this
            // lock to wake the range, so it can't fall between this check and the block.
            let (word, unmaps) = unsafe { (&*(wait.word as *const AtomicU32), &*(wait.unmaps as *const AtomicU32)) };
            if word.load(Ordering::Acquire) != wait.expected || unmaps.load(Ordering::Acquire) != wait.seen {
                Request::Yield
            } else if let Some(deadline) = wait.deadline {
                Request::WaitUserUntil(wait.key, deadline)
            } else {
                Request::WaitUser(wait.key)
            }
        }
        PendingRequest::Exit | PendingRequest::ToUser => Request::Exit,
    };
    // A program's thread with a signal to take doesn't block, as a thread being killed
    // doesn't: one that looked for it just before it came would otherwise wait on, with
    // nothing to wake it. It goes round its wait again, and a wait a signal may end sees
    // it there ([`interrupted`]); one it may not just waits again after. Under the lock
    // a signal is made pending and its threads woken under.
    let request = move || match request() {
        Request::Sleep(_) | Request::Wait(_) | Request::WaitUntil(..) | Request::WaitUser(_) | Request::WaitUserUntil(..)
            if current_slot().is_some_and(signal_due) =>
        {
            Request::Yield
        }
        other => other,
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
    let thread = Frame::new_task(entry, slot, kstack::top(slot), task_exit as *const () as usize);
    // SAFETY: this is the running task's frame, and nothing is using it: it was at EL0.
    unsafe { TASK_FRAMES.0[slot].get().write(thread) };
    Port::resume(TASK_FRAMES.0[slot].get());
}

/// A system call's kernel half.
extern "C" fn syscall_entry(slot: usize) -> ! {
    let user = USER_FRAMES.0[slot].get();
    // SAFETY: the task is here, not at EL0, so nothing else is using its user frame.
    let (number, args) = unsafe { ((*user).syscall_number(), (*user).syscall_args()) };
    // `SIGRETURN` sets every register, its result's among them.
    if number == feos_abi::nr::SIGRETURN {
        crate::user::signal::sigreturn(slot, args[0]);
        end_if_killed(slot);
        return_to_user(slot);
    }
    let result = crate::user::syscall(slot, number, args);
    // A kill gave the call reason to stop waiting, and it has unwound: what it held
    // has gone with it, so the task can end here.
    end_if_killed(slot);
    // SAFETY: as above.
    unsafe { (*user).set_syscall_result(result as usize) };
    // A call a signal interrupted with nothing done says `EINTR`, or is made again, as
    // the handler that runs now says (section 8.3 of the ABI design): with its number
    // and first argument as they came in, which the result has overwritten.
    let interrupted = (result == -feos_abi::errno::EINTR)
        .then(|| crate::user::signal::Interrupted { restart: crate::user::signal::restart_of(number, args), number, first: args[0] });
    crate::user::signal::deliver_after(slot, interrupted);
    switch_with(PendingRequest::ToUser);
    unreachable!("a task came back from EL0 without a trap");
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

/// A user fault's kernel half: gives the program the page its mapping says it should
/// have, or ends it. The single place a fault from user mode is decided, which is where
/// a signal frame will be built.
extern "C" fn fault_entry(slot: usize) -> ! {
    use crate::user::FaultOutcome;
    // SAFETY: see `FaultNotes`.
    let fault = unsafe { *FAULT_NOTES.0[slot].get() };
    let outcome = match fault.memory {
        Some(memory) => crate::user::resolve_fault(slot, memory),
        None => FaultOutcome::Segv,
    };
    // The fault's signal (section 8.2 of the ABI design): the port's for what wasn't a
    // page fault, a bad address or one not allowed for a page fault the tables couldn't
    // answer, and the bus for a mapped page with no frame.
    let (signal, code, address) = match (outcome, fault.memory) {
        (FaultOutcome::Retry, _) => (0, 0, 0),
        (FaultOutcome::Bus, memory) => (feos_abi::sig::BUS, feos_abi::si::BUS_ADRERR, memory.map_or(fault.instruction, |memory| memory.address)),
        (FaultOutcome::Segv, Some(memory)) => (feos_abi::sig::SEGV, crate::user::segv_code(slot, memory.address), memory.address),
        (FaultOutcome::Segv, None) => (fault.signal, fault.code, fault.address.unwrap_or(fault.instruction)),
    };
    // A handler the program has for it runs now; otherwise the whole process goes, with
    // the signal as its status, as its default action would take it.
    if signal != 0 && !crate::user::signal::fault(slot, signal, code, address) {
        crate::fatal::report_user_fault(&fault, outcome == FaultOutcome::Bus);
        end_by_signal(slot, signal);
    }
    end_if_killed(slot);
    return_to_user(slot)
}

/// Ends the calling user task if a kill is waiting for it. Its call has unwound, so
/// nothing of the kernel's is held on its stack.
fn end_if_killed(slot: usize) {
    if ATTENTION[slot].load(Ordering::Acquire) && kill_requested() {
        exit_task(KILLED, true);
    }
}

/// Goes back to the calling task's program, from its user frame, having delivered the
/// signals due for it: the single place a handler is entered from (section 8.3 of the ABI
/// design), with nothing of a call held.
fn return_to_user(slot: usize) -> ! {
    crate::user::signal::deliver_after(slot, None);
    switch_with(PendingRequest::ToUser);
    unreachable!("a task came back from EL0 without a trap");
}

/// Starts the calling task's program. It never comes back here: its system calls run
/// in [`syscall_entry`].
pub fn enter_user(slot: usize, root: usize, tlb: &crate::user::tlb::Tlb, frame: Frame) -> ! {
    // SAFETY: the task is running here, so nothing else is using its user frame.
    unsafe { USER_FRAMES.0[slot].get().write(frame) };
    // The space first: a switch that sees the root loads it with this.
    TLBS[slot].store(core::ptr::from_ref(tlb).cast_mut(), Ordering::Relaxed);
    ROOT[slot].store(root, Ordering::Relaxed);
    end_if_killed(slot);
    return_to_user(slot)
}

/// Runs the calling kernel task on the tables `root` of the space `tlb` keeps, wherever
/// it's switched in, until [`return_tables`]: for self-tests that look at a space from
/// a CPU. The space must outlive that.
pub fn borrow_tables(root: usize, tlb: &crate::user::tlb::Tlb) {
    let flags = Port::irq_save_and_disable();
    if let Some(slot) = current_slot() {
        TLBS[slot].store(core::ptr::from_ref(tlb).cast_mut(), Ordering::Relaxed);
        ROOT[slot].store(root, Ordering::Relaxed);
        load_tables(this_cpu(), Some(slot));
    }
    Port::irq_restore(flags);
}

/// Back to the kernel's tables, after [`borrow_tables`].
pub fn return_tables() {
    let flags = Port::irq_save_and_disable();
    if let Some(slot) = current_slot() {
        ROOT[slot].store(0, Ordering::Relaxed);
        TLBS[slot].store(core::ptr::null_mut(), Ordering::Relaxed);
        load_tables(this_cpu(), None);
    }
    Port::irq_restore(flags);
}

/// Ends the calling thread's whole process, for a user program's `exit`: every other
/// thread of it is killed, and the process ends with `code` whichever thread is last.
pub fn exit_user(status: Status) -> ! {
    if let Some(slot) = with_current_slot() {
        end_group(slot, status);
    }
    exit_task(status, true)
}

/// Ends the calling user thread alone, for `THREAD_EXIT`. With its process's last
/// thread, the process ends, with status 0 unless something decided otherwise.
pub fn exit_thread() -> ! {
    exit_current(0)
}

/// For `EXEC` from the thread in `slot`, the caller, holding its process's creation lock:
/// asks every other thread of its process to end, through the cooperative path a kill
/// takes (one in a call unwinds it first, one at its program goes at once), and waits
/// until each has left the table, not just its CPU: a thread blocked in a call still
/// walks the old tables and holds what the call took. `false` if the caller is itself
/// being killed meanwhile, which ends the whole process anyway.
pub fn end_other_threads(slot: usize) -> bool {
    let grace = Port::services().frequency() / 1000 * KILL_GRACE_MS;
    let now = Port::services().ticks();
    let (tgid, idle) = {
        let mut scheduler = SCHEDULER.lock();
        let scheduler = &mut *scheduler;
        let tgid = scheduler.table.tasks()[slot].tgid;
        for other in 0..MAX_TASKS {
            let task = scheduler.table.tasks()[other];
            if other == slot || task.tgid != tgid || task.state == State::Free {
                continue;
            }
            if scheduler.table.kill_slot(other, now, grace, &Facts) == KillOutcome::Pending {
                ATTENTION[other].store(true, Ordering::Release);
            }
        }
        let collected = collect_exits(scheduler);
        (tgid, announce(scheduler, collected).or(scheduler.table.idle_cpu(this_cpu())))
    };
    kick(idle);
    loop {
        // Every slot that leaves the table is collected in the same pass, which moves this
        // on and wakes its waiters.
        let seen = TEARDOWN_SEQUENCE.load(Ordering::Acquire);
        if SCHEDULER.lock().table.threads(tgid) <= 1 {
            return true;
        }
        if kill_requested() {
            return false;
        }
        futex_wait(&TEARDOWN_SEQUENCE, seen);
    }
}

/// For `EXEC`: the thread in `slot`, the caller and its process's only thread, runs on
/// the new space whose tables are `root` from here, on this CPU and wherever it's switched
/// in, so the old space can go.
pub fn switch_space(slot: usize, root: usize, tlb: &crate::user::tlb::Tlb) {
    let flags = Port::irq_save_and_disable();
    TLBS[slot].store(core::ptr::from_ref(tlb).cast_mut(), Ordering::Relaxed);
    ROOT[slot].store(root, Ordering::Relaxed);
    load_tables(this_cpu(), Some(slot));
    Port::irq_restore(flags);
}

/// For `EXEC`: the thread in `slot`, the caller, now runs the program `name` with the
/// arguments `shown`, as `ps` sees it; its thread pointer and clear word are the new
/// image's, none; and its process has run another program, for `SETPGID`.
pub fn exec_done(slot: usize, name: &str, shown: ArgBlock) {
    let flags = Port::irq_save_and_disable();
    TLS[slot].store(0, Ordering::Relaxed);
    Port::set_user_tls(0);
    Port::irq_restore(flags);
    CLEAR_WORD[slot].store(0, Ordering::Release);
    // The alternate stack was in the old image.
    set_alt_stack(slot, 0, 0);
    let mut scheduler = SCHEDULER.lock();
    let task = &mut scheduler.tasks[slot];
    task.set_name(name);
    task.args = shown;
    task.execed = true;
}

/// The thread id of the task in `slot`, the caller.
pub fn tid(slot: usize) -> u32 {
    SLOT_PID[slot].load(Ordering::Relaxed)
}

/// The word the end of the thread in `slot`, the caller, clears.
pub fn clear_word(slot: usize) -> usize {
    CLEAR_WORD[slot].load(Ordering::Relaxed)
}

/// Sets the thread pointer of the calling thread, in `slot`: its own copy and this
/// CPU's register at once, with interrupts masked, so a switch can't save the old one
/// over the new.
pub fn set_tls(slot: usize, value: usize) {
    let flags = Port::irq_save_and_disable();
    TLS[slot].store(value, Ordering::Relaxed);
    Port::set_user_tls(value);
    Port::irq_restore(flags);
}

/// A process whose next `GETTID` from a thread that isn't its first faults in the kernel,
/// for `ktest thread-fault`: zero for none. Nothing else arms it.
static INJECT_FAULT: AtomicU32 = AtomicU32::new(0);

/// The next thread a program starts is refused as if no frame were left for its kernel
/// stack: for a self-test of what a refused thread leaves in its slot.
static FAIL_THREAD_STACK: AtomicBool = AtomicBool::new(false);

/// Arms (or disarms) [`FAIL_THREAD_STACK`], saying whether it was armed.
pub fn fail_next_thread_stack(armed: bool) -> bool {
    FAIL_THREAD_STACK.swap(armed, Ordering::AcqRel)
}

/// Free slots, with no exit left in them to collect, that still point at an exit record:
/// there should never be one, since the next task in the slot would take it as its own.
pub fn stray_records() -> usize {
    let scheduler = SCHEDULER.lock();
    let left = scheduler.table.left();
    (1..MAX_TASKS)
        .filter(|&slot| {
            scheduler.table.tasks()[slot].state == State::Free
                && !left.contains(slot)
                && !RECORD[slot].load(Ordering::Relaxed).is_null()
        })
        .count()
}

/// Puts the pid counter at `pid`, wrapped or not, giving back where it was: for a
/// self-test of what holds a pid once the counter has wrapped.
pub fn swap_pid_counter(pid: u32, wrapped: bool) -> (u32, bool) {
    let mut scheduler = SCHEDULER.lock();
    let was = scheduler.table.pid_counter();
    scheduler.table.set_pid_counter(pid, wrapped);
    was
}

/// Arms [`injected_fault`] for the process `pid`.
pub fn arm_thread_fault(pid: u32) {
    INJECT_FAULT.store(pid, Ordering::Release);
}

/// Faults, once, if a self-test armed a fault for this thread's process and it isn't the
/// process's first thread: a real CPU fault in the middle of a call with nothing held, a
/// read of the guard below this task's kernel stack, which ends the whole process.
pub fn injected_fault(slot: usize) {
    let tgid = SLOT_TGID[slot].load(Ordering::Relaxed);
    if tgid == 0 || SLOT_PID[slot].load(Ordering::Relaxed) == tgid {
        return;
    }
    if INJECT_FAULT.compare_exchange(tgid, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        let guard = kstack::range(slot).0 - 2 * 4096;
        // SAFETY: not at all, on purpose: the guard is never mapped, so this faults, and
        // the fault path ends the task.
        let _ = unsafe { core::ptr::read_volatile(guard as *const u64) };
    }
}

/// The calling task's process, its first thread's id, for the pipes it holds.
fn current_tgid() -> u32 {
    let flags = Port::irq_save_and_disable();
    let pid = current_slot().map_or(0, |slot| SLOT_TGID[slot].load(Ordering::Relaxed));
    Port::irq_restore(flags);
    pid
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
        let Some(slot) = scheduler.table.find_pid(pid) else {
            return false;
        };
        if scheduler.table.tasks()[slot].state == State::Spawning {
            return false;
        }
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

/// A user program's standard stream, 0 to 2: the pipe its task was given or inherited,
/// or `None` for the console itself (stdin then reads as at end of file, and stdout and
/// stderr go wherever a kernel-linked app's print would).
pub fn stream_pipe(slot: usize, stream: usize) -> Option<PipeId> {
    let streams = match stream {
        0 => &STDIN,
        1 => &STDOUT,
        _ => &STDERR,
    };
    PipeId::from_raw(streams[slot].load(Ordering::Acquire))
}

/// Blocks a user program on its futex word `word` (the kernel's address of its pinned
/// frame), named by `key`, while it holds `expected` and `unmaps` is still the count the
/// caller saw, and not past the counter value `deadline`, and says whether it ran out of
/// time. As with any futex, a wake can be spurious.
pub fn futex_wait_user(
    word: &AtomicU32,
    key: feos_sched::UserKey,
    expected: u32,
    unmaps: (&AtomicU32, u32),
    deadline: Option<u64>,
) -> bool {
    switch_with(PendingRequest::WaitUser(UserWait {
        word: word as *const AtomicU32 as usize,
        key,
        expected,
        unmaps: unmaps.0 as *const AtomicU32 as usize,
        seen: unmaps.1,
        deadline,
    }));
    deadline.is_some_and(|deadline| Port::services().ticks() >= deadline)
}

/// Wakes up to `count` tasks blocked on a user futex word. Returns how many.
pub fn futex_wake_user(key: feos_sched::UserKey, count: usize) -> usize {
    let (woken, idle) = {
        let mut scheduler = SCHEDULER.lock();
        let woken = scheduler.table.wake_user_key(key, count);
        (woken, (woken > 0).then(|| scheduler.table.idle_cpu(this_cpu())).flatten())
    };
    kick(idle);
    woken
}

/// Wakes every task blocked on a user futex word of `space` in `[low, high)`, which has
/// just been unmapped.
pub fn futex_wake_user_range(space: usize, low: u64, high: u64) -> usize {
    let (woken, idle) = {
        let mut scheduler = SCHEDULER.lock();
        let woken = scheduler.table.wake_user_range(space, low, high);
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
    let delta = ms_to_ticks(ms);
    switch_with(PendingRequest::Sleep(Port::services().ticks().saturating_add(delta)));
}

/// The counter value `timeout_ms` from now, for a timed wait.
pub fn deadline_in_ms(timeout_ms: u64) -> u64 {
    Port::services().ticks().saturating_add(ms_to_ticks(timeout_ms))
}

/// Blocks the calling task until the counter reaches `deadline`. `false` if it was
/// [interrupted](interrupted) first, by a kill or a signal to take: either wakes a
/// sleeper, and a woken sleeper whose time hasn't come sleeps again unless that's why.
pub fn sleep_until(deadline: u64) -> bool {
    loop {
        if Port::services().ticks() >= deadline {
            return true;
        }
        if interrupted() {
            return false;
        }
        if !can_block() {
            core::hint::spin_loop();
            continue;
        }
        switch_with(PendingRequest::Sleep(deadline));
    }
}

/// Nanoseconds of CPU time the thread in `slot` has used, as the scheduler accounts it.
pub fn thread_cpu_ns(slot: usize) -> u64 {
    let scheduler = SCHEDULER.lock();
    ticks_as_ns(scheduler.table.tasks()[slot].cpu_ticks, scheduler.ticks_per_second)
}

/// Nanoseconds of CPU time every thread of `process` has used, live and ended. A thread
/// is counted live until its time is folded into the process (as it tears itself down,
/// or as it's collected), and both happen under this lock, so the sum never goes back.
pub fn process_cpu_ns(process: &crate::user::Process) -> u64 {
    let scheduler = SCHEDULER.lock();
    let pid = process.pid();
    let mut ticks = process.ended_ticks();
    for slot in 1..MAX_TASKS {
        if scheduler.tasks[slot].live
            && !TORN_DOWN[slot].load(Ordering::Acquire)
            && SLOT_TGID[slot].load(Ordering::Relaxed) == pid
        {
            ticks = ticks.wrapping_add(scheduler.table.tasks()[slot].cpu_ticks);
        }
    }
    ticks_as_ns(ticks, scheduler.ticks_per_second)
}

/// How finely CPU time is accounted: one scheduler tick, in nanoseconds.
pub fn cpu_resolution_ns() -> u64 {
    ticks_as_ns(1, SCHEDULER.lock().ticks_per_second)
}

fn ticks_as_ns(ticks: u64, per_second: u64) -> u64 {
    u64::try_from(ticks as u128 * feos_abi::NANOS_PER_SECOND as u128 / per_second.max(1) as u128).unwrap_or(u64::MAX)
}

/// Marks the calling process as finished and switches away for good.
fn exit_current(code: i32) -> ! {
    exit_task(Status::Exited(code), true)
}

/// Marks the calling process as finished and switches away for good, having given back
/// what it holds if `teardown` and it can. A task ending for a failure leaves that to
/// the reaper: what it was in the middle of may not be in a state to look at.
fn exit_task(status: Status, teardown: bool) -> ! {
    // A program's thread going with a process signal it claimed and didn't take gives it
    // back, for another thread of the process.
    if let Some(slot) = with_current_slot() {
        give_back_claimed(slot);
    }
    {
        let mut scheduler = SCHEDULER.lock();
        if let Some(slot) = scheduler.table.current(this_cpu()) {
            scheduler.tasks[slot].exit_code = status;
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
    let (app, entry, block, user_start) = {
        let mut scheduler = SCHEDULER.lock();
        let task = &mut scheduler.tasks[slot];
        let user_start = match task.is_user {
            true => task.user_start.take().map(crate::user::Start::Load).or_else(|| task.forked.take().map(crate::user::Start::Fork)),
            false => None,
        };
        (task.app, task.entry.take(), task.args, user_start)
    };

    if let Some(start) = user_start {
        exit_current(crate::user::start(slot, start));
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
    /// A program, with its arguments as far as they fit in the table's block, for
    /// showing: what it loads and starts with is the `UserStart` beside it.
    User(ArgBlock, core::marker::PhantomData<&'a ()>),
}

/// Streams for a user program, where a parent program names them, and what else it
/// inherits from one.
#[derive(Clone, Copy, Default)]
pub struct RawStreams {
    /// A pipe, or zero to inherit.
    pub stdin: u32,
    /// A pipe, or zero to inherit.
    pub stdout: u32,
    /// The parent program's umask, or `None` for [`DEFAULT_UMASK`](crate::user::DEFAULT_UMASK).
    pub umask: Option<u32>,
    /// The process group and session it starts in.
    pub group: SpawnGroup,
    /// Its signal mask, or `None` for its creator's.
    pub sigmask: Option<u64>,
    /// Signals its creator ignores that it starts at their defaults: `SETSIGDEF`.
    pub sigdefault: u64,
    /// A terminal's foreground group word, to be set to the new process's group as it's
    /// made, before it can run: `SPAWN2`'s `TCSETPGRP`.
    pub foreground: Option<&'static AtomicU32>,
}

/// Where a new program's process goes, among process groups and sessions. A program a
/// kernel app starts always gets a group of its own in the terminal's session.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum SpawnGroup {
    /// Its creator's group and session.
    #[default]
    Inherit,
    /// A new group, its own pid, in its creator's session.
    Own,
    /// The group with this id, which has to be in its creator's session (`EPERM`).
    Join(u32),
    /// A new session and group, both its own pid.
    NewSession,
}

/// Why a program's task couldn't be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpawnFail {
    Spawn(SpawnError),
    /// An error number for the program that asked: a group it can't join.
    Errno(isize),
}

impl From<SpawnError> for SpawnFail {
    fn from(error: SpawnError) -> Self {
        Self::Spawn(error)
    }
}

fn spawn_entry(
    name: &str,
    entry: Entry<'_>,
    io: SpawnIo<'_>,
    raw: RawStreams,
) -> Result<u32, SpawnError> {
    spawn_entry_with(name, entry, io, raw, None, None).map_err(|fail| match fail {
        SpawnFail::Spawn(error) => error,
        // Only a program's own group request can be refused, which kernel apps never make.
        SpawnFail::Errno(_) => SpawnError::Unsupported,
    })
}

/// The process group and session of a new task in `slot`, with pid `pid`, that the task
/// in `creator` is starting: as `group` asks, if a program is starting it. Under the
/// scheduler's lock.
fn new_group(scheduler: &Scheduler, creator: Option<usize>, pid: u32, group: SpawnGroup) -> Result<(u32, u32), isize> {
    let Some(creator) = creator.filter(|&creator| scheduler.tasks[creator].is_user) else {
        return Ok((pid, TERMINAL_SESSION));
    };
    let (pgid, sid) = (SLOT_PGID[creator].load(Ordering::Relaxed), SLOT_SID[creator].load(Ordering::Relaxed));
    match group {
        SpawnGroup::Inherit => Ok((pgid, sid)),
        SpawnGroup::Own => Ok((pid, sid)),
        SpawnGroup::NewSession => Ok((pid, pid)),
        SpawnGroup::Join(wanted) if group_in_session(scheduler, wanted, sid) => Ok((wanted, sid)),
        SpawnGroup::Join(_) => Err(-feos_abi::errno::EPERM),
    }
}

/// Whether a live program is in the process group `pgid`, in the session `sid`. Under the
/// scheduler's lock.
fn group_in_session(scheduler: &Scheduler, pgid: u32, sid: u32) -> bool {
    (1..MAX_TASKS).any(|slot| {
        let task = &scheduler.tasks[slot];
        task.live
            && task.is_user
            && scheduler.table.tasks()[slot].state != State::Free
            && SLOT_PGID[slot].load(Ordering::Relaxed) == pgid
            && SLOT_SID[slot].load(Ordering::Relaxed) == sid
    })
}

/// Starts a task: `entry`, and for a program `user_start`, what it loads and starts with,
/// and `record`, its exit record in the program starting it.
fn spawn_entry_with(
    name: &str,
    entry: Entry<'_>,
    io: SpawnIo<'_>,
    raw: RawStreams,
    mut user_start: Option<crate::user::Start>,
    record: Option<TryArc<crate::user::ChildRecord>>,
) -> Result<u32, SpawnFail> {
    // Slot 0 belongs to the shell and is never reused. A slot that's free but hasn't
    // had its exit collected still holds its address space, so everything gone has to
    // be collected before one can be taken. If the teardown queue is full, this task
    // empties it.
    // What a program needs to start is on the heap already: nothing may be allocated
    // under the scheduler's lock, where it's handed over.
    let user = matches!(entry, Entry::User(..));
    if user != user_start.is_some() {
        return Err(SpawnFail::Spawn(SpawnError::Unsupported));
    }
    let mut refused = None;
    let (slot, pid, (uid, gid), _no_kill) = loop {
        let (collected, reserved, idle) = {
            let mut scheduler = SCHEDULER.lock();
            let collected = collect_exits(&mut scheduler);
            // A new task is whoever started it, as of now: read under the lock that
            // `set_credentials` takes, so the table's uid, which the caps count by, and
            // the task's own can't differ however a `SETUID` races this. A program takes
            // a slot only while that leaves some for the kernel, and its user has room
            // for another.
            let who = current_slot().map_or((0, 0), credentials);
            let reserved = match (collected.complete, user) {
                (false, _) => None,
                (true, true) => scheduler.table.reserve_user(who.0, &USER_LIMITS),
                (true, false) => scheduler.table.reserve(),
            };
            // From the reservation until the task is committed or the slot given back,
            // the spawner can't be reaped: it would leave the slot reserved, and its
            // stack half made (see `kstack::take`). A user program's spawn is in a call,
            // which a kill waits for already; a kernel app's is held off here, under the
            // lock a kill takes, so no tick comes between.
            let mut reserved = reserved.map(|(slot, pid)| (slot, pid, who, NoKill::new()));
            // Its group and session, decided as it's made, under the lock that changes
            // them: a group it's to join is there now, or the spawn is refused. Its signal
            // mask with them.
            if let Some((slot, pid, ..)) = reserved {
                let creator = current_slot();
                match new_group(&scheduler, creator, pid, raw.group) {
                    Ok((pgid, sid)) => {
                        SLOT_PGID[slot].store(pgid, Ordering::Release);
                        SLOT_SID[slot].store(sid, Ordering::Release);
                        let inherited = creator.map_or(0, |creator| SIGNAL_MASK[creator].load(Ordering::Relaxed));
                        reset_signal_slot(slot, raw.sigmask.unwrap_or(inherited));
                        // What its spawner ignores, it ignores, but what `SETSIGDEF` names.
                        let ignored = creator.map_or(0, |creator| {
                            // SAFETY: a live slot's, under the lock: see `SIGNALS`.
                            signals_of(creator).map_or(0, |signals| signals.inherited())
                        });
                        INHERIT_IGNORED[slot].store(ignored & !raw.sigdefault, Ordering::Release);
                    }
                    Err(error) => {
                        scheduler.table.cancel(slot);
                        reserved = None;
                        refused = Some(error);
                    }
                }
            }
            // What a kill or a stop looks at is reset before the lock goes, and so
            // before the PID can be found: one that comes while the stack is being
            // painted has its hint kept.
            if let Some((slot, ..)) = reserved {
                LOCK_DEPTH[slot].store(0, Ordering::Release);
                // A program starts in the kernel, loading itself.
                IN_CALL[slot].store(u32::from(user), Ordering::Release);
                ATTENTION[slot].store(false, Ordering::Release);
                TORN_DOWN[slot].store(false, Ordering::Release);
                HEAP_BYTES[slot].store(0, Ordering::Release);
                STOP_STATE[slot].store(0, Ordering::Release);
                // A slot has a record only as its task is committed with one: whatever
                // path freed it last, it never passes one on.
                RECORD[slot].store(core::ptr::null_mut(), Ordering::Release);
                crate::user::forget_fault(slot);
            }
            let idle = announce(&mut scheduler, collected);
            (collected, reserved, idle)
        };
        kick(idle);
        if let Some(error) = refused {
            return Err(SpawnFail::Errno(error));
        }
        if collected.complete {
            break reserved.ok_or(SpawnError::NoFreeSlot)?;
        }
        // Whatever this can't take is being torn down by someone else already.
        drain_teardowns();
        yield_now();
    };

    // The stack is made, and painted, with interrupts on. The reservation keeps the slot
    // from being scheduled or reused meanwhile.
    let size = if user { kstack::USER_STACK } else { kstack::KERNEL_STACK };
    let Some((bottom, top)) = kstack::take(slot, size) else {
        SCHEDULER.lock().table.cancel(slot);
        return Err(SpawnError::NoFreeSlot.into());
    };
    // SAFETY: the slot is reserved and its stack just taken, so nothing is using it.
    unsafe { kstack::paint(bottom, top) };
    let started_ms = CLOCK.uptime_ms();

    FAULTED[slot].store(false, Ordering::Release);
    CAUGHT[slot].store(0, Ordering::Release);
    PENDING_SIGNALS[slot].store(0, Ordering::Release);
    CHILD_TICKS[slot].store(0, Ordering::Release);
    TLS[slot].store(0, Ordering::Release);
    CLEAR_WORD[slot].store(0, Ordering::Release);
    SLOT_PID[slot].store(pid, Ordering::Release);
    SLOT_TGID[slot].store(pid, Ordering::Release);

    TASK_UID[slot].store(uid, Ordering::Release);
    TASK_GID[slot].store(gid, Ordering::Release);
    let flags = Port::irq_save_and_disable();
    let creator = current_slot();
    let (parent, mut inherited_out, mut inherited_err, inherited_console, inherited_in) = match current_slot() {
        Some(parent) => (
            SLOT_TGID[parent].load(Ordering::Relaxed),
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
    let writer = |given: Option<&pipe::PipeWriter>, inherited: u32| {
        given.map(|writer| writer.id()).or(PipeId::from_raw(inherited))
    };
    if raw.stdout != 0 {
        inherited_out = raw.stdout;
    }
    let stdin_id = io
        .stdin
        .map(|reader| reader.id())
        .or(PipeId::from_raw(if raw.stdin != 0 { raw.stdin } else { inherited_in }));
    // Stdin, stdout, stderr and the console, and whether each is a read end. A pipe that
    // has gone leaves its stream on the console; one there's no heap to record the child
    // as holding fails the spawn, with what it took given back, rather than quietly send
    // the child's output, or take its input, somewhere nobody asked.
    let wanted = [
        (stdin_id, true),
        (writer(io.stdout, inherited_out), false),
        (writer(io.stderr, inherited_err), false),
        (writer(io.console, inherited_console), false),
    ];
    let mut held = [0u32; 4];
    for (index, &(id, read)) in wanted.iter().enumerate() {
        match pipe::attach_stream(id, read, pid) {
            Ok(raw) => held[index] = raw,
            Err(_) => {
                for (&(_, read), &raw) in wanted.iter().zip(&held[..index]) {
                    match PipeId::from_raw(raw) {
                        Some(id) if read => pipe::close_reader_id(id, pid),
                        Some(id) => pipe::close_writer_id(id, pid),
                        None => {}
                    }
                }
                let mut scheduler = SCHEDULER.lock();
                kstack::idle(slot);
                scheduler.table.cancel(slot);
                return Err(SpawnError::NoMemory.into());
            }
        }
    }
    let [stdin, stdout, stderr, console] = held;
    STDIN[slot].store(stdin, Ordering::Release);
    STDOUT[slot].store(stdout, Ordering::Release);
    STDERR[slot].store(stderr, Ordering::Release);
    CONSOLE[slot].store(console, Ordering::Release);

    // Whatever a task before left in the slot goes once the lock has.
    let (idle, left) = {
        let mut scheduler = SCHEDULER.lock();
        let scheduler = &mut *scheduler;
        let task = &mut scheduler.tasks[slot];
        task.set_name(name);
        task.is_user = false;
        let (start, forked) = match user_start.take() {
            Some(crate::user::Start::Load(start)) => (Some(start), None),
            Some(crate::user::Start::Fork(forked)) => (None, Some(forked)),
            None => (None, None),
        };
        let left = (core::mem::replace(&mut task.user_start, start), core::mem::replace(&mut task.forked, forked));
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
            Entry::User(args, _) => {
                task.is_user = true;
                task.args = args;
                task.app = None;
                task.entry = None;
            }
        }
        task.stack = (bottom, top);
        task.started_ms = started_ms;
        task.parent = parent;
        task.parent_slot = creator.unwrap_or(0) as u16;
        task.background = io.background;
        task.live = true;
        task.exit_code = KILLED;
        task.group_code = None;
        task.execed = false;
        // Its parent program's record of it has its pid from now on, which it holds until
        // it's gone; the thread group's reference to it is the slot's. Neither allocates.
        if let Some(record) = record {
            record.made(pid, SLOT_PGID[slot].load(Ordering::Relaxed));
            RECORD[slot].store(TryArc::into_raw(record).cast_mut(), Ordering::Release);
        }
        let frame = Frame::new_task(
            app_entry as *const () as usize,
            slot,
            top,
            task_exit as *const () as usize,
        );
        // SAFETY: the slot is reserved, so nothing is running it or about to.
        unsafe { TASK_FRAMES.0[slot].get().write(frame) };
        // The terminal goes to its group only now, when nothing is left that can fail the
        // spawn, and still before it can run: set any earlier, a spawn that fails after
        // would leave the terminal to a group that never existed, and its creator's group
        // in the background, to be stopped by its next read.
        if let Some(foreground) = raw.foreground {
            foreground.store(SLOT_PGID[slot].load(Ordering::Relaxed), Ordering::Release);
        }
        scheduler.table.commit(slot);
        (scheduler.table.idle_cpu(this_cpu()), left)
    };
    kick(idle);
    drop(left);

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
    spawn_user_env(path, argv, &[], cwd, io, raw)
}

/// [`spawn_user`] with an environment, `KEY=VALUE` strings.
pub fn spawn_user_env(
    path: &str,
    argv: ArgBlock,
    env: &[&str],
    cwd: &str,
    io: SpawnIo<'_>,
    raw: RawStreams,
) -> Result<u32, SpawnError> {
    if path.len() > feos_abi::PATH_MAX {
        return Err(SpawnError::ArgsTooLong);
    }
    let umask = raw.umask.unwrap_or(crate::user::DEFAULT_UMASK);
    // The staged strings are frames, which nothing counts as a kernel app's: reaped while
    // it held them, it would lose them for good. So no kill from before they're taken
    // until the new task has them, or they've gone back.
    let _no_kill = NoKill::new();
    let mut start = crate::user::UserStart::new(path, &argv, env, cwd, umask)?;
    // Its streams on a terminal, those the kernel app says are: a reference to that
    // terminal for the process to make its descriptors from. One closed since is none.
    if let Some(terminal) = io.terminal {
        start.set_terminal(crate::user::tty::from_host(terminal.terminal), terminal.streams);
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    spawn_entry_with(name, Entry::User(argv, core::marker::PhantomData), io, raw, Some(crate::user::Start::Load(start)), None).map_err(|fail| match fail {
        SpawnFail::Spawn(error) => error,
        SpawnFail::Errno(_) => SpawnError::Unsupported,
    })
}

/// Starts a program a program asked for (`SPAWN2`, v1's `SPAWN`), with what it loads
/// and starts with made already: its strings staged, and its descriptor table and
/// image too for `SPAWN2`. `shown` is its arguments as far as the table shows them, and
/// `record` the exit record its parent took for it.
pub fn spawn_program(
    name: &str,
    start: Box<crate::user::UserStart>,
    shown: ArgBlock,
    raw: RawStreams,
    record: TryArc<crate::user::ChildRecord>,
) -> Result<u32, SpawnFail> {
    spawn_entry_with(name, Entry::User(shown, core::marker::PhantomData), SpawnIo::default(), raw, Some(crate::user::Start::Load(start)), Some(record))
}

/// Starts the child a `FORK` from the calling thread made, in a new process: with the
/// caller's name and arguments as `ps` shows them, in its group and session, with its
/// signal mask, and `record` the exit record its parent took for it. `forked` is what
/// the child starts from (`user::proc::start_forked`).
pub fn spawn_fork(
    forked: Box<crate::user::Forked>,
    raw: RawStreams,
    record: TryArc<crate::user::ChildRecord>,
) -> Result<u32, SpawnFail> {
    let Some(creator) = with_current_slot() else {
        return Err(SpawnFail::Spawn(SpawnError::Unsupported));
    };
    let (name, name_len, args) = {
        let scheduler = SCHEDULER.lock();
        let task = &scheduler.tasks[creator];
        (task.name, task.name_len, task.args)
    };
    let name = core::str::from_utf8(&name[..name_len]).unwrap_or("forked");
    spawn_entry_with(name, Entry::User(args, core::marker::PhantomData), SpawnIo::default(), raw, Some(crate::user::Start::Fork(forked)), Some(record))
}

/// The thread pointer of the calling thread, in `slot`, as its register has it now.
/// Where a program can write the register itself, `TLS[slot]` only catches up when the
/// thread leaves its CPU, so it's read from the register (and the copy brought up to
/// date), with interrupts masked so a switch can't come between.
pub fn current_tls(slot: usize) -> usize {
    if !Port::USER_TLS_WRITABLE {
        return TLS[slot].load(Ordering::Relaxed);
    }
    let flags = Port::irq_save_and_disable();
    let value = Port::user_tls();
    TLS[slot].store(value, Ordering::Relaxed);
    Port::irq_restore(flags);
    value
}

/// The exit record of the calling program's process in its parent, which is in `slot`:
/// another reference to it, for the process to keep. `None` if a kernel app started it.
pub fn own_record(slot: usize) -> Option<TryArc<crate::user::ChildRecord>> {
    let record = RECORD[slot].load(Ordering::Acquire);
    // SAFETY: the thread group's reference, which lasts while any thread of it, the caller
    // among them, is in the table.
    (!record.is_null()).then(|| unsafe { TryArc::clone_raw(record.cast_const()) })
}

/// Why a thread couldn't be started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadError {
    /// The process has [`MAX_THREADS`], or no slot is free to user programs, or the
    /// process is ending.
    Again,
}

/// Starts a thread of the calling user thread's process, in a slot of its own, at
/// `frame` in its program: on the space whose tables are `root` and TLB bookkeeping
/// `tlb`, with thread pointer `tls` and `clear_word` for its end. `adopt` gives the new
/// slot its reference to the process, before anything can run it. The thread starts at
/// its program, never in the kernel, so there's no call for a kill to unwind: until it
/// first traps it can be reaped like any program at EL0 or ring 3.
///
/// The caller holds its process's creation lock, so the thread count can't change but
/// by threads ending, and a fork or exec never sees half a thread.
pub fn spawn_thread(
    frame: Frame,
    root: usize,
    tlb: &crate::user::tlb::Tlb,
    tls: usize,
    clear_word: usize,
    adopt: impl FnOnce(usize),
) -> Result<u32, ThreadError> {
    let Some(creator) = with_current_slot() else {
        return Err(ThreadError::Again);
    };
    let (slot, tid) = loop {
        let (collected, reserved, idle) = {
            let mut scheduler = SCHEDULER.lock();
            let collected = collect_exits(&mut scheduler);
            let tgid = scheduler.table.tasks()[creator].tgid;
            // A process whose status is decided is ending: its threads are being killed,
            // and one started now would outlive the sweep. A kill that's under way is
            // `reserve_thread`'s to refuse.
            let ending = scheduler.tasks[creator].group_code.is_some();
            let room = scheduler.table.threads(tgid) < MAX_THREADS;
            let reserved = if collected.complete && room && !ending {
                scheduler.table.reserve_user_thread(creator, &USER_LIMITS)
            } else {
                None
            };
            if let Some((slot, _)) = reserved {
                LOCK_DEPTH[slot].store(0, Ordering::Release);
                IN_CALL[slot].store(0, Ordering::Release);
                ATTENTION[slot].store(false, Ordering::Release);
                TORN_DOWN[slot].store(false, Ordering::Release);
                HEAP_BYTES[slot].store(0, Ordering::Release);
                IN_USER[slot].store(false, Ordering::Release);
                // Its process's record, which the thread group holds, and its stop: one
                // starting as its process stops is stopped with it.
                RECORD[slot].store(RECORD[creator].load(Ordering::Relaxed), Ordering::Release);
                STOP_STATE[slot].store(STOP_STATE[creator].load(Ordering::Relaxed), Ordering::Release);
                crate::user::forget_fault(slot);
            }
            let idle = announce(&mut scheduler, collected);
            (collected, reserved, idle)
        };
        kick(idle);
        if collected.complete {
            break reserved.ok_or(ThreadError::Again)?;
        }
        drain_teardowns();
        yield_now();
    };

    // Its kernel stack, for its calls and faults, as any user task's.
    let refused = FAIL_THREAD_STACK.load(Ordering::Relaxed) && FAIL_THREAD_STACK.swap(false, Ordering::AcqRel);
    let stack = if refused { None } else { kstack::take(slot, kstack::USER_STACK) };
    let Some((bottom, top)) = stack else {
        // The slot gives back the record pointer it was lent, under the lock a stop
        // reads it by: a cancelled slot is never collected, so nothing else would, and
        // the next task in it would take the record as its own.
        let mut scheduler = SCHEDULER.lock();
        RECORD[slot].store(core::ptr::null_mut(), Ordering::Release);
        STOP_STATE[slot].store(0, Ordering::Release);
        scheduler.table.cancel(slot);
        return Err(ThreadError::Again);
    };
    // SAFETY: the slot is reserved and its stack just taken, so nothing is using it.
    unsafe { kstack::paint(bottom, top) };
    let started_ms = CLOCK.uptime_ms();

    FAULTED[slot].store(false, Ordering::Release);
    CAUGHT[slot].store(0, Ordering::Release);
    PENDING_SIGNALS[slot].store(0, Ordering::Release);
    CHILD_TICKS[slot].store(0, Ordering::Release);
    TLS[slot].store(tls, Ordering::Release);
    CLEAR_WORD[slot].store(clear_word, Ordering::Release);
    SLOT_PID[slot].store(tid, Ordering::Release);
    SLOT_TGID[slot].store(SLOT_TGID[creator].load(Ordering::Relaxed), Ordering::Release);
    // Its creator's mask, and nothing pending; no alternate stack, as on Linux, where
    // a thread shares its creator's memory and so can't share the stack.
    reset_signal_slot(slot, SIGNAL_MASK[creator].load(Ordering::Relaxed));
    // The process's streams: its pipes are held by the process, not by a thread, so the
    // thread only names them.
    for streams in [&STDIN, &STDOUT, &STDERR, &CONSOLE] {
        streams[slot].store(streams[creator].load(Ordering::Relaxed), Ordering::Release);
    }
    adopt(slot);
    // SAFETY: the slot is reserved, so nothing is running it or about to.
    unsafe { USER_FRAMES.0[slot].get().write(frame) };
    TLBS[slot].store(core::ptr::from_ref(tlb).cast_mut(), Ordering::Relaxed);
    ROOT[slot].store(root, Ordering::Relaxed);
    IN_USER[slot].store(true, Ordering::Release);

    let (idle, left) = {
        let mut scheduler = SCHEDULER.lock();
        let scheduler = &mut *scheduler;
        // Who it is, as of now: a `SETUID` that came since the reservation is in the
        // creator's by the time this lock is had.
        TASK_UID[slot].store(TASK_UID[creator].load(Ordering::Relaxed), Ordering::Release);
        TASK_GID[slot].store(TASK_GID[creator].load(Ordering::Relaxed), Ordering::Release);
        // Its group and session are its process's, which change under this lock.
        SLOT_PGID[slot].store(SLOT_PGID[creator].load(Ordering::Relaxed), Ordering::Release);
        SLOT_SID[slot].store(SLOT_SID[creator].load(Ordering::Relaxed), Ordering::Release);
        let (name, name_len, parent, parent_slot, background, group_code, execed) = {
            let task = &scheduler.tasks[creator];
            (task.name, task.name_len, task.parent, task.parent_slot, task.background, task.group_code, task.execed)
        };
        let task = &mut scheduler.tasks[slot];
        task.name = name;
        task.name_len = name_len;
        task.is_user = true;
        // A thread starts at its program: nothing to load. Nothing is freed here either.
        let left = (task.user_start.take(), task.forked.take());
        task.args = ArgBlock::new();
        task.app = None;
        task.entry = None;
        task.stack = (bottom, top);
        task.started_ms = started_ms;
        task.parent = parent;
        task.parent_slot = parent_slot;
        task.background = background;
        task.live = true;
        task.exit_code = KILLED;
        task.group_code = group_code;
        task.execed = execed;
        scheduler.table.commit(slot);
        (scheduler.table.idle_cpu(this_cpu()), left)
    };
    kick(idle);
    drop(left);
    Ok(tid)
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
            return WaitStatus::Exited(record.status.v1());
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

/// Whether the task in `slot` is a live thread of a program. Under the scheduler's lock.
fn is_live_program(scheduler: &Scheduler, slot: usize) -> bool {
    slot != 0 && scheduler.tasks[slot].live && scheduler.tasks[slot].is_user && scheduler.table.tasks()[slot].state != State::Free
}

/// `GETPPID` for the program in `slot`: its parent, or 1 (the shell, which doesn't reap
/// it) once its parent has gone. Pids aren't reused until the counter wraps, so a parent
/// that's there is the one that started it.
pub fn parent_pid(slot: usize) -> u32 {
    let scheduler = SCHEDULER.lock();
    let parent = scheduler.tasks[slot].parent;
    if parent != 0 && scheduler.table.find_pid(parent).is_some() {
        parent
    } else {
        1
    }
}

/// The process group and session of the process `pid`, or of the caller in `slot` for
/// 0: `GETPGID` and `GETSID`. `ESRCH` if there's no such process.
pub fn group_of(slot: usize, pid: u32) -> Result<(u32, u32), isize> {
    let scheduler = SCHEDULER.lock();
    let target = if pid == 0 { Some(slot) } else { scheduler.table.find_pid(pid) };
    let target = target.ok_or(-feos_abi::errno::ESRCH)?;
    Ok((SLOT_PGID[target].load(Ordering::Relaxed), SLOT_SID[target].load(Ordering::Relaxed)))
}

/// Gives every thread of the process in `slot` this group and session. Under the
/// scheduler's lock, which is where they change.
fn set_group_of(scheduler: &Scheduler, slot: usize, pgid: u32, sid: u32) {
    for member in scheduler.table.group_members(slot) {
        if scheduler.table.tasks()[member].state != State::Free {
            SLOT_PGID[member].store(pgid, Ordering::Release);
            SLOT_SID[member].store(sid, Ordering::Release);
        }
    }
    // A parent's `WAIT4` of a group looks in its record.
    let record = RECORD[slot].load(Ordering::Relaxed);
    if !record.is_null() {
        // SAFETY: the thread group's reference, which a live thread of it keeps.
        unsafe { (*record).set_pgid(pgid) };
    }
}

/// `SETPGID` from the program in `slot`, with `pid` and `pgid` already 0 or positive.
pub fn set_process_group(slot: usize, pid: u32, pgid: u32) -> Result<(), isize> {
    use feos_abi::errno::{EPERM, ESRCH};
    let scheduler = SCHEDULER.lock();
    let me = SLOT_TGID[slot].load(Ordering::Relaxed);
    let session = SLOT_SID[slot].load(Ordering::Relaxed);
    let pid = if pid == 0 { me } else { pid };
    // The caller, or a child of it; one in another session is there but out of reach.
    let target = if pid == me {
        slot
    } else {
        let child = scheduler.table.find_pid(pid).filter(|&child| is_live_program(&scheduler, child) && scheduler.tasks[child].parent == me);
        let child = child.ok_or(-ESRCH)?;
        if SLOT_SID[child].load(Ordering::Relaxed) != session {
            return Err(-EPERM);
        }
        // A child that has run another program is out of its parent's hands, as POSIX says.
        if scheduler.tasks[child].execed {
            return Err(-feos_abi::errno::EACCES);
        }
        child
    };
    // A session leader stays where it is.
    if SLOT_SID[target].load(Ordering::Relaxed) == pid {
        return Err(-EPERM);
    }
    let pgid = if pgid == 0 { pid } else { pgid };
    if pgid != pid && !group_in_session(&scheduler, pgid, session) {
        return Err(-EPERM);
    }
    set_group_of(&scheduler, target, pgid, session);
    Ok(())
}

/// Whether a live program is in the process group `pgid` of the session `sid`: a group
/// a terminal's foreground can be made.
pub fn group_exists(pgid: u32, sid: u32) -> bool {
    let scheduler = SCHEDULER.lock();
    group_in_session(&scheduler, pgid, sid)
}

/// Whether the process group `pgid` of the session `sid` is orphaned, as POSIX has it: no
/// member has a parent in another group of the same session, so nobody is left to
/// continue it if a terminal stops it. The kernel app a program was started by counts as
/// such a parent: it's in the session, in a group of its own. A group with no members
/// isn't orphaned; it's nothing.
pub fn group_orphaned(pgid: u32, sid: u32) -> bool {
    let scheduler = SCHEDULER.lock();
    let mut members = false;
    for slot in 1..MAX_TASKS {
        if !is_live_program(&scheduler, slot) || SLOT_PGID[slot].load(Ordering::Relaxed) != pgid {
            continue;
        }
        members = true;
        let parent = scheduler.tasks[slot].parent;
        let held = scheduler.table.find_pid(parent).filter(|&found| scheduler.tasks[found].live || found == 0);
        if let Some(found) = held {
            let (group, session) = (SLOT_PGID[found].load(Ordering::Relaxed), SLOT_SID[found].load(Ordering::Relaxed));
            if group != pgid && session == sid {
                return false;
            }
        }
    }
    members
}

/// Raises `signal` on every program in the process group `pgid`, as a terminal does:
/// its line discipline's `ISIG`, a background read's `SIGTTIN`, a resize's `SIGWINCH`.
/// From the kernel, so nothing is refused. Whether any program was there.
pub fn signal_group(pgid: u32, signal: u32) -> bool {
    if pgid == 0 {
        return false;
    }
    let info = feos_abi::SigInfo::sender(signal, feos_abi::si::KERNEL, 0, 0);
    let (found, cpus, idle) = {
        let mut scheduler = SCHEDULER.lock();
        let scheduler = &mut *scheduler;
        let mut seen = [0u64; MAX_TASKS / 64];
        let (mut found, mut cpus) = (false, 0);
        for other in 1..MAX_TASKS {
            if seen[other / 64] >> (other % 64) & 1 != 0 || !is_live_program(scheduler, other) {
                continue;
            }
            if SLOT_PGID[other].load(Ordering::Relaxed) != pgid {
                continue;
            }
            for member in scheduler.table.group_members(other) {
                seen[member / 64] |= 1 << (member % 64);
            }
            found = true;
            cpus |= act_on(scheduler, other, signal, &info);
        }
        let collected = collect_exits(scheduler);
        (found, cpus, announce(scheduler, collected).or(scheduler.table.idle_cpu(this_cpu())))
    };
    reschedule_cpus(cpus);
    kick(idle);
    found
}

/// Blocks the calling task while `word` holds `seen`, until the counter reaches
/// `deadline` if there is one. As with any futex, a wake can be spurious, and a signal's
/// or a kill's wake of the task ends it too: the caller looks at
/// [`interrupted`] and the time itself.
pub fn wait_word_until(word: &AtomicU32, seen: u32, deadline: Option<u64>) {
    if !can_block() {
        core::hint::spin_loop();
        return;
    }
    let key = word as *const AtomicU32 as usize;
    match deadline {
        Some(deadline) => switch_with(PendingRequest::WaitUntil(key, seen, deadline)),
        None => switch_with(PendingRequest::Wait(key, seen)),
    }
}

/// `SETSID` from the program in `slot`: a new session and group, both its pid. `EPERM`
/// if a group already has its pid, its own or one another process joined.
pub fn new_session(slot: usize) -> Result<u32, isize> {
    let scheduler = SCHEDULER.lock();
    let me = SLOT_TGID[slot].load(Ordering::Relaxed);
    if (1..MAX_TASKS).any(|other| is_live_program(&scheduler, other) && SLOT_PGID[other].load(Ordering::Relaxed) == me) {
        return Err(-feos_abi::errno::EPERM);
    }
    set_group_of(&scheduler, slot, me, me);
    Ok(me)
}

/// Task slots, threads included: what `SYSINFO` says.
pub const fn task_slots() -> usize {
    MAX_TASKS
}


fn kill_process(pid: u32) -> KillResult {
    let grace = Port::services().frequency() / 1000 * KILL_GRACE_MS;
    let now = Port::services().ticks();
    let (outcome, idle) = {
        let mut scheduler = SCHEDULER.lock();
        let outcome = scheduler.table.kill(pid, now, grace, &Facts);
        // Every thread of it that has to get to its boundary to go looks there.
        if outcome == KillOutcome::Pending {
            note_attention(&scheduler.table, pid);
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

/// Has every live thread of `pid` that a kill or a stop is waiting for look for it on
/// its way back to its program. Under the scheduler lock.
fn note_attention(table: &Table<MAX_TASKS, MAX_CPUS>, pid: u32) {
    for (slot, task) in table.tasks().iter().enumerate() {
        if task.state != State::Free && task.tgid == pid && (task.kill_pending || task.stopped) {
            ATTENTION[slot].store(true, Ordering::Release);
        }
    }
}

/// The live threads of the process the task in `slot` belongs to, `slot` among them if it
/// is one. Under the scheduler lock.
fn live_members(scheduler: &Scheduler, slot: usize) -> impl Iterator<Item = usize> + '_ {
    let tgid = scheduler.table.tasks()[slot].tgid;
    scheduler
        .table
        .group_members(slot)
        .filter(move |&member| scheduler.table.tasks()[member].state != State::Free && scheduler.table.tasks()[member].tgid == tgid)
}

/// Records, in the parent program's record of the process in `slot`, that it stopped or
/// continued, and wakes whoever waits for that. Under the scheduler lock.
fn note_change(scheduler: &mut Scheduler, slot: usize, status: Status) {
    let record = RECORD[slot].load(Ordering::Relaxed);
    if record.is_null() {
        return;
    }
    // SAFETY: the thread group's reference, which a live thread of it keeps.
    let ticks = unsafe {
        (*record).changed(status);
        (*record).usage().ticks
    };
    EXIT_SEQUENCE.fetch_add(1, Ordering::Release);
    scheduler.table.wake_key(&EXIT_SEQUENCE as *const AtomicU32 as usize, usize::MAX);
    // The parent's `SIGCHLD`, unless it said `SA_NOCLDSTOP`. A parent running on another
    // CPU takes it at its next switch or call: nothing here waits on it.
    let (parent, pid, uid) = (scheduler.tasks[slot].parent, SLOT_TGID[slot].load(Ordering::Relaxed), TASK_UID[slot].load(Ordering::Relaxed));
    // A parent blocked in a call is woken for it, and one on another CPU asked to look.
    reschedule_cpus(raise_child(scheduler, parent, status, pid, uid, ticks));
}

/// The stop of the process in `slot` is recorded for its parent once every thread of it
/// has parked: none is running, and each is either blocked or back at its program, held
/// there. A thread blocked in a call counts, since it can't go on without a wake, and
/// when it has one it finishes the call and parks at its boundary (2.1 of the ABI
/// design): nothing of its program runs meanwhile. Under the scheduler lock, wherever a
/// thread of a stopping process leaves its CPU, and when the stop is asked for.
fn check_parked(scheduler: &mut Scheduler, slot: usize) {
    let stop = STOP_STATE[slot].load(Ordering::Relaxed);
    if stop == 0 || stop & STOP_REPORTED != 0 {
        return;
    }
    let tasks = scheduler.table.tasks();
    let parked = live_members(scheduler, slot).all(|member| match tasks[member].state {
        State::Running => false,
        State::Ready => IN_CALL[member].load(Ordering::Acquire) == 0,
        State::Blocked | State::Spawning | State::Free => true,
    });
    if !parked {
        return;
    }
    for member in scheduler.table.group_members(slot) {
        if STOP_STATE[member].load(Ordering::Relaxed) == stop {
            STOP_STATE[member].store(stop | STOP_REPORTED, Ordering::Relaxed);
        }
    }
    note_change(scheduler, slot, Status::Stopped(stop));
}

/// Stops the process of the user thread in `slot` for `signal`, as a stop signal's
/// default action does: each thread is held at its next way back to its program, never
/// inside a call. A process that's stopping or stopped already stays as it is. Returns
/// the CPUs running its threads, which have to be asked to reschedule so they park soon.
/// Under the scheduler lock.
fn stop_process(scheduler: &mut Scheduler, slot: usize, signal: u32) -> u32 {
    let tgid = scheduler.table.tasks()[slot].tgid;
    if !scheduler.table.set_stopped(tgid, true) {
        return 0;
    }
    note_attention(&scheduler.table, tgid);
    let mut running = 0;
    let me = this_cpu();
    for member in live_members(scheduler, slot) {
        if STOP_STATE[member].load(Ordering::Relaxed) == 0 {
            STOP_STATE[member].store(signal as u8, Ordering::Relaxed);
        }
        if scheduler.table.tasks()[member].state == State::Running {
            if let Some(cpu) = (0..MAX_CPUS).find(|&cpu| scheduler.table.current(cpu) == Some(member)) {
                if cpu != me {
                    running |= 1 << cpu;
                }
            }
        }
    }
    // A stop that comes again while the first stands changes nothing.
    check_parked(scheduler, slot);
    running
}

/// Continues the process of the user thread in `slot`, as `SIGCONT` does, and records it
/// for its parent if it was stopped. Under the scheduler lock.
fn continue_process(scheduler: &mut Scheduler, slot: usize) {
    let tgid = scheduler.table.tasks()[slot].tgid;
    let stopped = live_members(scheduler, slot).any(|member| {
        scheduler.table.tasks()[member].stopped || STOP_STATE[member].load(Ordering::Relaxed) != 0
    });
    if !stopped {
        return;
    }
    scheduler.table.set_stopped(tgid, false);
    for member in scheduler.table.group_members(slot) {
        STOP_STATE[member].store(0, Ordering::Relaxed);
    }
    note_change(scheduler, slot, Status::Continued);
}

/// Ends the process of the user thread in `slot` with `status`, unless something decided
/// its status already, as a terminating signal's default action does: every thread is
/// killed through the cooperative path (one in a call unwinds it first), a stopped one
/// too. Under the scheduler lock.
fn end_process(scheduler: &mut Scheduler, slot: usize, status: Status) {
    let grace = Port::services().frequency() / 1000 * KILL_GRACE_MS;
    let now = Port::services().ticks();
    let tgid = scheduler.table.tasks()[slot].tgid;
    let status = live_members(scheduler, slot).find_map(|member| scheduler.tasks[member].group_code).unwrap_or(status);
    for member in scheduler.table.group_members(slot) {
        if scheduler.table.tasks()[member].tgid == tgid && scheduler.tasks[member].live {
            scheduler.tasks[member].group_code = Some(status);
        }
    }
    if scheduler.table.kill(tgid, now, grace, &Facts) == KillOutcome::Pending {
        note_attention(&scheduler.table, tgid);
    }
}

/// Carries out `signal`'s default action on the process of the user thread in `slot`
/// (signal 0 does nothing). Returns the CPUs to ask to reschedule. Under the scheduler
/// lock; the caller collects exits after.
fn act_on(scheduler: &mut Scheduler, slot: usize, signal: u32, info: &feos_abi::SigInfo) -> u32 {
    if signal == 0 {
        return 0;
    }
    let action = feos_signal::default_action(signal);
    // What the process has said about it, read without its signal lock: a handler makes
    // it pending, `SIG_IGN` drops it. `SIGKILL` and `SIGSTOP` can have neither, and
    // `SIGCONT` continues the process whatever its disposition.
    if !feos_signal::is_unstoppable(signal) {
        let wanted = feos_signal::bit(signal);
        let signals = signals_of(slot);
        // A new program can be found from its commit, but has its process, and so its
        // signal state, only once its image is resolved, which can wait on the disk.
        // Until then what it inherited is what it ignores: a signal its spawner ignored
        // mustn't end it meanwhile. It has no handlers yet.
        let ignored = match signals {
            Some(signals) => signals.ignored.load(Ordering::Acquire),
            None if scheduler.tasks[slot].is_user => INHERIT_IGNORED[slot].load(Ordering::Acquire),
            None => 0,
        };
        if action == Action::Continue {
            continue_process(scheduler, slot);
        }
        if ignored & wanted != 0 {
            return 0;
        }
        if let Some(signals) = signals {
            // A handler takes it as a thread goes back to its program; so does a default
            // that ends or stops the process, while every thread blocks the signal: it
            // stays pending for the process until one unblocks it (8.3 of the design).
            let caught = signals.caught.load(Ordering::Acquire) & wanted != 0;
            let held = matches!(action, Action::Terminate | Action::Stop)
                && live_members(scheduler, slot).all(|member| SIGNAL_MASK[member].load(Ordering::Acquire) & wanted != 0);
            if caught || held {
                signals.note_sender(info);
                signals.pending.fetch_or(wanted, Ordering::AcqRel);
                return notify_signal(scheduler, slot, wanted);
            }
        }
    }
    default_now(scheduler, slot, signal, action)
}

/// `signal`'s default action, `action`, on the process of the user thread in `slot`, now.
/// Returns the CPUs to ask to reschedule. Under the scheduler lock.
fn default_now(scheduler: &mut Scheduler, slot: usize, signal: u32, action: Action) -> u32 {
    match action {
        Action::Terminate => {
            end_process(scheduler, slot, Status::signaled(signal));
            0
        }
        Action::Stop => stop_process(scheduler, slot, signal),
        Action::Continue => {
            continue_process(scheduler, slot);
            0
        }
        Action::Ignore => 0,
    }
}

/// Has the threads of the process of the thread in `slot` that don't block `wanted`, a
/// set just made pending for the process, take it: returns the CPUs, other than this
/// one, running one of them, to ask to reschedule (the IPI of 8.3 of the design), and
/// wakes each blocked one, whose wait, if a signal may end it, claims the signal for
/// that thread alone ([`interrupted`]). Under the lock.
///
/// A thread in a call is asked too, though its way out looks at its signals. That look
/// comes before it marks itself at its program ([`IN_USER`]), so one that read nothing
/// pending, and is marked only after the signal was made pending here, would otherwise
/// go back to its program with no request on the way and take the signal only at its
/// next tick. An extra reschedule of a thread still in its call costs a pass of the
/// scheduler, nothing more; so does a wake of a thread whose wait isn't one a signal
/// ends, which waits again.
fn notify_signal(scheduler: &mut Scheduler, slot: usize, wanted: u64) -> u32 {
    let mut blocked = [0u64; MAX_TASKS / 64];
    let mut cpus = 0;
    for member in live_members(scheduler, slot) {
        if SIGNAL_MASK[member].load(Ordering::Acquire) & wanted == wanted {
            continue;
        }
        match scheduler.table.tasks()[member].state {
            State::Running => cpus |= running_cpu(scheduler, member),
            State::Blocked => blocked[member / 64] |= 1 << (member % 64),
            _ => {}
        }
    }
    let mut woken = false;
    for member in 0..MAX_TASKS {
        if blocked[member / 64] >> (member % 64) & 1 != 0 {
            woken |= scheduler.table.wake_slot(member);
        }
    }
    cpus | if woken { idle_cpus(scheduler) } else { 0 }
}

/// Has the thread in `slot` take a signal just made pending for it alone: the CPU it's
/// running on, if that isn't this one, to ask to reschedule, or a wake if it's blocked.
/// Under the lock.
fn notify_thread(scheduler: &mut Scheduler, slot: usize) -> u32 {
    let state = scheduler.table.tasks()[slot].state;
    match state {
        State::Running => running_cpu(scheduler, slot),
        State::Blocked if scheduler.table.wake_slot(slot) => idle_cpus(scheduler),
        _ => 0,
    }
}

/// The CPU, as a bit, the running task in `slot` is on, unless it's this one.
fn running_cpu(scheduler: &Scheduler, slot: usize) -> u32 {
    let me = this_cpu();
    (0..MAX_CPUS).find(|&cpu| scheduler.table.current(cpu) == Some(slot)).filter(|&cpu| cpu != me).map_or(0, |cpu| 1 << cpu)
}

/// An idle CPU, as a bit, to pick up a task just woken.
fn idle_cpus(scheduler: &Scheduler) -> u32 {
    scheduler.table.idle_cpu(this_cpu()).map_or(0, |cpu| 1 << cpu)
}

/// Raises `SIGCHLD` on the program `parent` for its child's change, `status`, unless it
/// doesn't want one: a stop or continue with `SA_NOCLDSTOP`. Under the lock, after the
/// change is in the child's record, so a handler's `WAIT4` finds it. Returns the CPUs to
/// ask to reschedule.
fn raise_child(scheduler: &mut Scheduler, parent: u32, status: Status, pid: u32, uid: u32, ticks: u64) -> u32 {
    let Some(parent_slot) = program_slot(scheduler, parent) else {
        return 0;
    };
    let Some(signals) = signals_of(parent_slot) else {
        return 0;
    };
    let flags = signals.child_flags.load(Ordering::Acquire);
    if !status.is_end() && flags & crate::user::signal::NOCLDSTOP != 0 {
        return 0;
    }
    let info = crate::user::signal::child_info(status, pid, uid, ticks);
    act_on(scheduler, parent_slot, feos_abi::sig::CHLD, &info)
}

/// A live slot of the program `tgid`: its first thread's, or any other's once that's
/// gone. Under the lock.
fn program_slot(scheduler: &Scheduler, tgid: u32) -> Option<usize> {
    let live = |slot: usize| {
        slot != 0 && scheduler.tasks[slot].live && scheduler.tasks[slot].is_user && SLOT_TGID[slot].load(Ordering::Relaxed) == tgid
    };
    scheduler.table.find_pid(tgid).filter(|&slot| live(slot)).or_else(|| (1..MAX_TASKS).find(|&slot| live(slot)))
}

/// Asks each CPU in `cpus` to reschedule.
fn reschedule_cpus(cpus: u32) {
    for cpu in 0..MAX_CPUS {
        if cpus >> cpu & 1 != 0 {
            Machine::interrupts().send_ipi(IpiTarget::Cpu(cpu), Machine::IPI_RESCHEDULE);
        }
    }
}

/// Who a program's `KILL` is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KillTarget {
    /// The process with this pid.
    Process(u32),
    /// Every process in this group.
    Group(u32),
    /// Every process the caller may signal but its own.
    All,
}

/// `KILL` from the program in `slot`: `signal` (0 to check only, or a signal) to
/// `target`, with its default action. Root may signal any program, and anyone else
/// those of its own user (`EPERM`); kernel tasks can't be signalled (`EPERM`). `ESRCH` if
/// nothing matches. A zombie is the caller's to look for: it isn't in the table.
pub fn kill_from(slot: usize, target: KillTarget, signal: u32) -> Result<(), isize> {
    use feos_abi::errno::{EPERM, ESRCH};
    let (uid, _) = credentials(slot);
    let mut cpus = 0;
    let me = SLOT_TGID[slot].load(Ordering::Relaxed);
    let info = feos_abi::SigInfo::sender(signal, feos_abi::si::USER, me, uid);
    let (result, idle) = {
        let mut scheduler = SCHEDULER.lock();
        let scheduler = &mut *scheduler;
        let may = |other: usize| uid == 0 || TASK_UID[other].load(Ordering::Relaxed) == uid;
        let result = match target {
            KillTarget::Process(pid) => match scheduler.table.find_pid(pid) {
                // The shell.
                Some(0) => Err(-EPERM),
                // One being made isn't there yet; nor is a thread that isn't a process.
                Some(found) if scheduler.tasks[found].live => {
                    if !scheduler.tasks[found].is_user || !may(found) {
                        Err(-EPERM)
                    } else {
                        cpus |= act_on(scheduler, found, signal, &info);
                        Ok(())
                    }
                }
                _ => Err(-ESRCH),
            },
            KillTarget::Group(_) | KillTarget::All => {
                // Each process once, at the first of its threads found: a bit per slot
                // for those already seen.
                let mut seen = [0u64; MAX_TASKS / 64];
                let (mut found, mut done) = (false, false);
                for other in 1..MAX_TASKS {
                    if seen[other / 64] >> (other % 64) & 1 != 0 || !is_live_program(scheduler, other) {
                        continue;
                    }
                    let tgid = scheduler.table.tasks()[other].tgid;
                    let wanted = match target {
                        KillTarget::Group(pgid) => SLOT_PGID[other].load(Ordering::Relaxed) == pgid,
                        _ => tgid != me,
                    };
                    if !wanted {
                        continue;
                    }
                    for member in scheduler.table.group_members(other) {
                        seen[member / 64] |= 1 << (member % 64);
                    }
                    found = true;
                    if may(other) {
                        cpus |= act_on(scheduler, other, signal, &info);
                        done = true;
                    }
                }
                match (found, done) {
                    (_, true) => Ok(()),
                    (true, false) => Err(-EPERM),
                    (false, _) => Err(-ESRCH),
                }
            }
        };
        let collected = collect_exits(scheduler);
        (result, announce(scheduler, collected).or(scheduler.table.idle_cpu(this_cpu())))
    };
    reschedule_cpus(cpus);
    kick(idle);
    result
}

/// `THREAD_KILL` from the program in `slot`: `signal` (0 to check only, or a signal) to
/// its own process's thread `tid` (`ESRCH` for any other), pending on that thread alone
/// if it has a handler for it or blocks it, and otherwise with its default action on the
/// whole process, as a signal to the process would have.
pub fn thread_kill_from(slot: usize, tid: u32, signal: u32) -> Result<(), isize> {
    let (uid, _) = credentials(slot);
    let me = SLOT_TGID[slot].load(Ordering::Relaxed);
    let info = feos_abi::SigInfo::sender(signal, feos_abi::si::TKILL, me, uid);
    let (cpus, idle) = {
        let mut scheduler = SCHEDULER.lock();
        let scheduler = &mut *scheduler;
        let Some(target) = scheduler
            .table
            .find_tid(tid)
            .filter(|&target| target != 0 && scheduler.tasks[target].live && scheduler.table.tasks()[target].tgid == me)
        else {
            return Err(-feos_abi::errno::ESRCH);
        };
        let cpus = signal_thread(scheduler, target, signal, &info);
        let collected = collect_exits(scheduler);
        (cpus, announce(scheduler, collected).or(scheduler.table.idle_cpu(this_cpu())))
    };
    reschedule_cpus(cpus);
    kick(idle);
    Ok(())
}

/// Carries out `signal` (0 does nothing) sent to the user thread in `slot` alone: dropped
/// if its process ignores it, pending on the thread if the process has a handler for it
/// or the thread blocks it (a stop or an end it would bring held off as for a process
/// signal), and otherwise its default action on the process. Returns the CPUs to ask to
/// reschedule. Under the scheduler lock.
fn signal_thread(scheduler: &mut Scheduler, slot: usize, signal: u32, info: &feos_abi::SigInfo) -> u32 {
    if signal == 0 {
        return 0;
    }
    let action = feos_signal::default_action(signal);
    if !feos_signal::is_unstoppable(signal) {
        let wanted = feos_signal::bit(signal);
        if action == Action::Continue {
            continue_process(scheduler, slot);
        }
        if let Some(signals) = signals_of(slot) {
            if signals.ignored.load(Ordering::Acquire) & wanted != 0 {
                return 0;
            }
            let caught = signals.caught.load(Ordering::Acquire) & wanted != 0;
            let held = matches!(action, Action::Terminate | Action::Stop) && SIGNAL_MASK[slot].load(Ordering::Acquire) & wanted != 0;
            if caught || held {
                // `TKILLED` first, so the signal is never taken as pending with it unset.
                if info.code == feos_abi::si::TKILL {
                    TKILLED[slot].fetch_or(wanted, Ordering::AcqRel);
                }
                THREAD_PENDING[slot].fetch_or(wanted, Ordering::AcqRel);
                return notify_thread(scheduler, slot);
            }
        }
    }
    default_now(scheduler, slot, signal, action)
}

/// [`Processes::send_signal`]: `number`'s default action on `pid`, from a kernel app,
/// which may signal any program. A kernel app has no signals but `SIGKILL`: anything that
/// would end it kills it, and a stop or continue holds or lets it go.
fn send_signal(pid: u32, number: u32) -> KillResult {
    if !feos_signal::is_signal(number) {
        return KillResult::NotFound;
    }
    let action = feos_signal::default_action(number);
    let (user, protected) = {
        let scheduler = SCHEDULER.lock();
        match scheduler.table.find_pid(pid) {
            Some(0) => (false, true),
            Some(slot) if scheduler.tasks[slot].live => (scheduler.tasks[slot].is_user, false),
            _ => return KillResult::NotFound,
        }
    };
    if protected {
        return KillResult::Protected;
    }
    if !user {
        return match action {
            Action::Terminate => kill_process(pid),
            Action::Stop | Action::Continue => match set_stopped(pid, action == Action::Stop) {
                true => KillResult::Killed,
                false => KillResult::Protected,
            },
            Action::Ignore => KillResult::Killed,
        };
    }
    let mut cpus = 0;
    let (outcome, idle) = {
        let mut scheduler = SCHEDULER.lock();
        let scheduler = &mut *scheduler;
        let Some(slot) = scheduler.table.find_pid(pid).filter(|&slot| scheduler.tasks[slot].live) else {
            return KillResult::NotFound;
        };
        let info = feos_abi::SigInfo::sender(number, feos_abi::si::USER, current_tgid(), 0);
        cpus |= act_on(scheduler, slot, number, &info);
        let outcome = match (action, scheduler.table.find_pid(pid)) {
            (Action::Terminate, None) => KillResult::Killed,
            (Action::Terminate, Some(_)) => KillResult::Pending,
            _ => KillResult::Killed,
        };
        let collected = collect_exits(scheduler);
        (outcome, announce(scheduler, collected).or(scheduler.table.idle_cpu(this_cpu())))
    };
    reschedule_cpus(cpus);
    kick(idle);
    outcome
}

/// [`Processes::set_stopped`]: job control, the shell's Ctrl-Z and `fg`. A program stops
/// as for `SIGTSTP`, which its parent sees as a stop, and continues as for `SIGCONT`; a
/// kernel app is held off the CPU from its next tick.
fn set_stopped(pid: u32, stopped: bool) -> bool {
    let mut cpus = 0;
    let (done, idle) = {
        let mut scheduler = SCHEDULER.lock();
        let scheduler = &mut *scheduler;
        let program = scheduler.table.find_pid(pid).filter(|&slot| is_live_program(scheduler, slot));
        let done = match program {
            Some(slot) if stopped => {
                cpus = stop_process(scheduler, slot, feos_abi::sig::TSTP);
                scheduler.table.tasks()[slot].stopped
            }
            Some(slot) => {
                continue_process(scheduler, slot);
                true
            }
            None => {
                let done = scheduler.table.set_stopped(pid, stopped);
                // A program in a call stops on its way back to the program, which looks.
                if done && stopped {
                    note_attention(&scheduler.table, pid);
                }
                done
            }
        };
        (done, scheduler.table.idle_cpu(this_cpu()))
    };
    reschedule_cpus(cpus);
    kick(idle);
    done
}

/// The status, as v1 sees it, of `parent`'s child `pid` among the recent exits the ring
/// keeps: for v1's `WAIT` of a child it has reaped already, which v1 answered again from
/// there.
pub fn recent_exit(pid: u32, parent: u32) -> Option<i32> {
    let scheduler = SCHEDULER.lock();
    (1..=EXIT_RECORDS).find_map(|back| {
        let record = scheduler.exits[(scheduler.next_exit + EXIT_RECORDS - back) % EXIT_RECORDS];
        (record.pid == pid && pid != 0).then_some(record)
    })
    .filter(|record| record.parent == parent)
    .map(|record| record.status.v1())
}

/// The current value of the counter every recorded exit, stop and continue moves on, for
/// a `WAIT4` to wait on with [`wait_for_exits`].
pub fn exit_sequence() -> u32 {
    EXIT_SEQUENCE.load(Ordering::Acquire)
}

/// Waits until the counter [`exit_sequence`] read as `seen` moves, which may be at once.
/// `false` if the caller is being killed, and has to unwind, or, if a signal may end
/// the wait (`by_signal`), has a signal to take.
pub fn wait_for_exits(seen: u32, by_signal: bool) -> bool {
    let stop = if by_signal { interrupted } else { kill_requested };
    if stop() {
        return false;
    }
    futex_wait(&EXIT_SEQUENCE, seen);
    !stop()
}

/// Whether the process `pid`, which has ended, has also given back everything it held:
/// no teardown of any thread of it is still under way. A parent reaps it only then, so a
/// `WAIT4` that returns means its frames and pipes are free.
pub fn settled(pid: u32) -> bool {
    let _scheduler = SCHEDULER.lock();
    tearing_down(pid).is_none()
}

/// `ticks` of the scheduler's CPU time accounting, in nanoseconds.
pub fn ticks_ns(ticks: u64) -> u64 {
    let per_second = SCHEDULER.lock().ticks_per_second;
    ticks_as_ns(ticks, per_second)
}

/// Thread id of the calling task, or 0 outside any task. Call with interrupts masked.
pub fn current_pid_fast() -> u32 {
    current_slot().map_or(0, |slot| SLOT_PID[slot].load(Ordering::Relaxed))
}

/// Adjusts the heap bytes charged to the thread `pid`, which may have exited since.
///
/// A process freeing another's block is normal, so this searches by thread id, looking
/// first at the calling task's slot, whose block it nearly always is (the heap calls with
/// interrupts masked, so the slot is still this CPU's). Blocks from a thread that has
/// gone are charged to nobody. The caller holds the heap lock, so charges can't race
/// each other.
pub fn charge_heap(pid: u32, delta: isize) {
    feos_sched::charge_heap_hinted(&SLOT_PID, &HEAP_BYTES, pid, delta, current_slot());
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

/// One entry per process, its threads counted in it: the first thread found speaks for
/// it, and the CPU time is all of theirs.
///
/// Under the innermost lock, so it's one pass over the table: a process is entered at
/// its first slot found, and the rest of its threads are added from its ring of members
/// there and marked, so no thread looks for its process among the entries made already.
fn snapshot(out: &mut [ProcessInfo]) -> usize {
    /// A slot of a process with no room in `out`.
    const LEFT_OUT: u16 = u16::MAX;
    let shown = |state: State| !matches!(state, State::Free | State::Spawning);
    // Which entry each slot has been counted in, plus one; zero for none yet.
    let mut counted = [0u16; MAX_TASKS];
    let mut count = 0;
    let scheduler = SCHEDULER.lock();
    let tasks = scheduler.table.tasks();
    for (slot, policy) in tasks.iter().enumerate() {
        if !shown(policy.state) || counted[slot] != 0 {
            continue;
        }
        let members = scheduler
            .table
            .group_members(slot)
            .filter(|&member| shown(tasks[member].state) && tasks[member].tgid == policy.tgid);
        if count == out.len() {
            for member in members {
                counted[member] = LEFT_OUT;
            }
            continue;
        }

        let task = &scheduler.tasks[slot];
        let (bottom, top) = task.stack;
        let entry = &mut out[count];
        *entry = ProcessInfo {
            pid: policy.tgid,
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
            memory_bytes: top - bottom,
            threads: 1,
            priority: policy.priority,
            parent: task.parent,
            background: task.background,
        };
        count += 1;
        // Slots before this one were all looked at already, and none was this process's.
        for member in members {
            counted[member] = count as u16;
            if member == slot {
                continue;
            }
            let (bottom, top) = scheduler.tasks[member].stack;
            entry.threads = entry.threads.saturating_add(1);
            entry.cpu_ticks = entry.cpu_ticks.wrapping_add(tasks[member].cpu_ticks);
            entry.memory_bytes += top - bottom;
        }
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
    // exits in the meantime, the figure is merely out of date: a stack being read isn't
    // given back until the read is done.
    let stack_used = if slot == 0 { kstack::measure(bottom, top - bottom) } else { kstack::high_water(slot) };
    Some(ProcessDetail {
        blocked,
        kill_pending,
        stack_size: top - bottom,
        stack_used,
        heap_bytes: HEAP_BYTES[slot].load(Ordering::Acquire),
        stopped,
        cpu,
        affinity,
    })
}

/// Whether the current task has been asked to end.
pub(crate) fn kill_requested() -> bool {
    let scheduler = SCHEDULER.lock();
    scheduler
        .table
        .current(this_cpu())
        .is_some_and(|slot| scheduler.table.tasks()[slot].kill_pending)
}

fn system_info() -> SystemInfo {
    let scheduler = SCHEDULER.lock();
    let table = &scheduler.table;
    let (processes, threads) = table.counts();
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
        threads,
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
        env: &[&str],
        cwd: &str,
        io: SpawnIo<'_>,
    ) -> Result<u32, SpawnError> {
        let mut block = ArgBlock::new();
        if !block.push(path) || !block.push_all(argv) {
            return Err(SpawnError::ArgsTooLong);
        }
        spawn_user_env(path, block, env, cwd, io, RawStreams::default())
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

    fn open_terminal(&self, echo: PipeId, rows: u16, columns: u16) -> Option<u32> {
        crate::user::tty::open(echo, rows, columns)
    }

    fn terminal_input(&self, terminal: u32, bytes: &[u8]) -> usize {
        crate::user::tty::input(terminal, bytes)
    }

    fn resize_terminal(&self, terminal: u32, rows: u16, columns: u16) {
        crate::user::tty::resize(terminal, rows, columns);
    }

    fn close_terminal(&self, terminal: u32) {
        crate::user::tty::close(terminal);
    }

    fn take_terminal_input(&self, terminal: u32, out: &mut [u8]) -> usize {
        crate::user::tty::take_input(terminal, out)
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
        set_stopped(pid, stopped)
    }

    fn is_stopped(&self, pid: u32) -> bool {
        let scheduler = SCHEDULER.lock();
        scheduler.table.find_pid(pid).is_some_and(|slot| scheduler.table.tasks()[slot].stopped)
    }

    fn send_signal(&self, pid: u32, number: u32) -> KillResult {
        send_signal(pid, number)
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

    /// The hardware clock is read at boot and then counted forward (see `time`): the
    /// taskbar asks every frame, and reading a CMOS clock is slow port I/O.
    fn unix_time(&self) -> Option<u64> {
        crate::time::unix_time()
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
    start_alarms();
}

/// Moves on every time a program's `ITIMER_REAL` is set, to wake [`alarms`].
static ALARMS: AtomicU32 = AtomicU32::new(0);

/// A program's `SETITIMER` changed its timer: the alarm task looks again.
pub fn alarms_changed() {
    ALARMS.fetch_add(1, Ordering::AcqRel);
    futex_wake(&ALARMS, 1);
}

/// The task that raises `SIGALRM` for each program whose `ITIMER_REAL` has passed, and
/// re-arms it by its interval, then sleeps until the next one is due or a timer changes.
/// It looks at most once per [`feos_signal::itimer::MIN_PERIOD_NS`] but when a timer is
/// set: as well as each timer's interval having that floor, many programs' timers out of
/// phase with each other then can't between them have it take the scheduler's lock any
/// more often.
fn alarms() -> i32 {
    loop {
        let seen = ALARMS.load(Ordering::Acquire);
        let now = crate::time::monotonic_ns();
        match fire_alarms(now) {
            Some(due) => {
                let due = due.max(now.saturating_add(feos_signal::itimer::MIN_PERIOD_NS));
                if can_block() {
                    let key = &ALARMS as *const AtomicU32 as usize;
                    switch_with(PendingRequest::WaitUntil(key, seen, crate::time::deadline_ticks(due)));
                }
            }
            None => futex_wait(&ALARMS, seen),
        }
    }
}

/// Raises `SIGALRM` for every program whose timer is due by `now` (monotonic
/// nanoseconds) and re-arms each that has an interval, and says when the next is due.
/// A timer is the process's ([`crate::user::signal::Signals`]), seen through each of its
/// threads' slots under the scheduler's lock; it's moved on by a compare-and-swap, so it
/// goes off once whichever slot sees it first, and a `SETITIMER` that changes it
/// meanwhile wins.
fn fire_alarms(now: u64) -> Option<u64> {
    let mut cpus = 0;
    let mut next: Option<u64> = None;
    let idle = {
        let mut scheduler = SCHEDULER.lock();
        let scheduler = &mut *scheduler;
        for slot in 1..MAX_TASKS {
            if !is_live_program(scheduler, slot) {
                continue;
            }
            let Some(signals) = signals_of(slot) else {
                continue;
            };
            let mut due = signals.alarm_at.load(Ordering::Acquire);
            if due != 0 && due <= now {
                let again = feos_signal::itimer::next(due, signals.alarm_every.load(Ordering::Acquire), now);
                // One still pending from last time takes this one in (signals don't
                // queue), so there's no one to tell again.
                let pending = signals.pending.load(Ordering::Acquire) & feos_signal::bit(feos_abi::sig::ALRM) != 0;
                if signals.alarm_at.compare_exchange(due, again, Ordering::AcqRel, Ordering::Acquire).is_ok() && !pending {
                    let info = feos_abi::SigInfo::sender(feos_abi::sig::ALRM, feos_abi::si::KERNEL, 0, 0);
                    cpus |= act_on(scheduler, slot, feos_abi::sig::ALRM, &info);
                }
                due = signals.alarm_at.load(Ordering::Acquire);
            }
            if due != 0 {
                next = Some(next.map_or(due, |next| next.min(due)));
            }
        }
        let collected = collect_exits(scheduler);
        announce(scheduler, collected).or(scheduler.table.idle_cpu(this_cpu()))
    };
    reschedule_cpus(cpus);
    kick(idle);
    next
}

/// Starts the task that raises programs' `SIGALRM`s. As the reaper, it can't be killed
/// or stopped; it runs above most, so a timer goes off on time.
fn start_alarms() {
    match spawn_entry("alarms", Entry::Task(Box::new(alarms)), SpawnIo::default(), RawStreams::default()) {
        Ok(pid) => {
            let mut scheduler = SCHEDULER.lock();
            scheduler.table.set_protected(pid);
            scheduler.table.set_priority(pid, feos_sched::PRIORITY_NORMAL + 3);
        }
        Err(error) => feos_console::println!("[sched] no alarms: {error:?}"),
    }
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
