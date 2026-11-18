//! Scheduler policy: the task state machine, run-queue choice and kill rules.
//!
//! The kernel owns register frames, stacks and locks; this crate owns every
//! decision about them, so the decisions can be tested on the host. All state
//! lives in a [`Table`] the kernel keeps behind its scheduler lock. A zeroed
//! table is valid, so the kernel's copy lives in `.bss`.
//!
//! Sleeping tasks are the table's [`Blocked`](State::Blocked) entries with a
//! deadline, which a bitmap of them finds on each tick: there are few, and that's
//! simpler than keeping a sorted queue.
//!
//! # Bitmaps
//!
//! The table has up to [`MAX_SLOTS`] slots, too many to scan on every switch, tick and
//! wake. So beside the slots it keeps bitmaps of them, changed with every state change
//! (see [`Table::update`]): the ready tasks at each priority, the sleepers with a
//! deadline, the waiters on each futex hash bucket (and on any user word), the tasks
//! passed over too often, and the slots that have left the table since the kernel last
//! looked. Each decision walks only the bits it needs, in slot order, so it comes out
//! exactly as a scan of every slot would: host tests check the two against each other.
//!
//! # Choosing what runs
//!
//! Each task has a home CPU, and a CPU prefers its own tasks, so a task stays where
//! its data is cached: these are per-CPU run queues, in policy. They share the
//! table and the one lock around it, which is fine for a table this small, and
//! means there's no queue to get out of step with the states.
//!
//! The highest priority wins, round-robin among equals. A CPU takes another's task
//! when it has nothing of its own at that priority, or when the other's queue is
//! longer, which is how load evens out. A task can name the CPUs it may use. A task
//! passed over too many times gets one turn at the top, so a low priority means
//! slow, not stopped.
//!
//! # User programs in the kernel
//!
//! A user program's task spends part of its life in the kernel, carrying out its
//! system calls and resolving its page faults, and that kernel work holds things a
//! kill can't take back: references, buffers and promises on its stack. So a task
//! whose [in-call count](TaskFacts::in_call) is nonzero is never reaped and never
//! held off the CPU by a stop. A kill wakes it, its call unwinds, and it ends itself;
//! a stop waits for it to be about to go back to the program. Kernel-linked apps keep
//! the older rules: they're reaped at any point they hold no lock, and stop at once.
//!
//! # Threads
//!
//! Every task has a thread id, [`Task::pid`], unique among tasks, and a thread group
//! id, [`Task::tgid`]: the process it belongs to, which is its first thread's id. A
//! kernel-linked app, and a program's first thread, is its own group. Everything that
//! names a process by its pid (killing, stopping, priority, affinity, waking, finding)
//! acts on every thread of the group, and a process is there for as long as any of
//! its threads is: see [`Table::find_pid`] and [`Table::last_of_group`].

#![cfg_attr(not(test), no_std)]

/// The priority of a task nobody has said anything about.
pub const PRIORITY_NORMAL: u8 = 8;
/// The highest priority. Larger numbers are more urgent.
pub const PRIORITY_MAX: u8 = 15;
/// How many times a ready task can be passed over before it gets a turn anyway.
const STARVATION_LIMIT: u16 = 64;
/// Most slots a table can have: what its bitmaps are sized for.
pub const MAX_SLOTS: usize = 1024;
/// Words in a bitmap of every slot.
const WORDS: usize = MAX_SLOTS / 64;
/// Priorities, each with a ready bitmap of its own.
const LEVELS: usize = PRIORITY_MAX as usize + 1;
/// Futex hash buckets, for kernel words and user words each.
const BUCKETS: usize = 64;

/// A set of slots, a bit each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slots([u64; WORDS]);

impl Slots {
    /// No slots.
    pub const EMPTY: Self = Self([0; WORDS]);

    fn insert(&mut self, slot: usize) {
        self.0[slot / 64] |= 1 << (slot % 64);
    }

    fn remove(&mut self, slot: usize) {
        self.0[slot / 64] &= !(1 << (slot % 64));
    }

    /// Whether `slot` is in the set.
    pub fn contains(&self, slot: usize) -> bool {
        slot < MAX_SLOTS && self.0[slot / 64] >> (slot % 64) & 1 != 0
    }

    /// Whether the set has no slots.
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&word| word == 0)
    }

    fn and(&self, other: &Self) -> Self {
        Self(core::array::from_fn(|index| self.0[index] & other.0[index]))
    }

    fn and_not(&self, other: &Self) -> Self {
        Self(core::array::from_fn(|index| self.0[index] & !other.0[index]))
    }

    /// How many slots are in the set.
    pub fn len(&self) -> usize {
        self.0.iter().map(|word| word.count_ones() as usize).sum()
    }

    /// The slots in the set, lowest first.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.from(0)
    }

    /// The slots in the set from `first` up, lowest first.
    fn from(&self, first: usize) -> impl Iterator<Item = usize> + '_ {
        let mut word = first / 64;
        let mut bits = if word < WORDS { self.0[word] & (!0u64 << (first % 64)) } else { 0 };
        core::iter::from_fn(move || loop {
            if bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                return Some(word * 64 + bit);
            }
            word += 1;
            if word >= WORDS {
                return None;
            }
            bits = self.0[word];
        })
    }

    /// The slots in the set from `first` up, then those below it: the order equals take
    /// turns in.
    fn cyclic(&self, first: usize) -> impl Iterator<Item = usize> + '_ {
        self.from(first).chain(self.iter().take_while(move |&slot| slot < first))
    }
}

/// The bucket a kernel futex word's waiters are in.
fn key_bucket(key: usize) -> usize {
    // Words are at least 4-byte aligned, so the low bits say nothing.
    ((key as u64 >> 2).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (64 - BUCKETS.trailing_zeros())) as usize
}

/// The bucket a user futex word's waiters are in.
fn user_bucket(key: UserKey) -> usize {
    let mixed = (key.space as u64).rotate_left(29) ^ (key.offset >> 2);
    (mixed.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (64 - BUCKETS.trailing_zeros())) as usize
}

/// Scheduler-visible lifecycle state. The zero value is [`State::Free`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum State {
    /// The slot is unused.
    Free = 0,
    /// Reserved by a spawn that hasn't finished preparing the stack.
    Spawning,
    /// Runnable, waiting for a CPU.
    Ready,
    /// Executing on a CPU.
    Running,
    /// Not runnable until a deadline passes or a wake arrives.
    Blocked,
}

/// What a [`Blocked`](State::Blocked) task is waiting for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Wait {
    /// Not waiting.
    Nothing = 0,
    /// A counter deadline.
    Until(u64),
    /// A wake on this address.
    Key(usize),
    /// A wake on this address, or a counter deadline, whichever comes first.
    KeyUntil(usize, u64),
    /// A wake on a user program's futex word.
    UserKey(UserKey),
    /// A wake on a user program's futex word, or a counter deadline.
    UserKeyUntil(UserKey, u64),
}

/// A user program's futex word, by what it is rather than where its frame is: a
/// kernel waiter's key is its word's kernel address, which a user word's frame could
/// share, so the two are different kinds of wait and never match each other.
///
/// `space` names the memory: a process's address space, for a word in its private
/// memory, or a shared object or file every process mapping it agrees on, for a word in
/// shared memory. `offset` is the word's address in the space, or its byte offset in
/// the object. The kernel keeps the two kinds of `space` from colliding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UserKey {
    pub space: usize,
    pub offset: u64,
}

/// Why a CPU is choosing what to run next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request {
    /// The timer fired, or another CPU asked for a reschedule.
    Preempt,
    /// The current task offers the CPU and stays runnable.
    Yield,
    /// The current task sleeps until this counter value.
    Sleep(u64),
    /// The current task blocks until [`Table::wake_key`] names this key.
    Wait(usize),
    /// As `Wait`, but not past this counter value.
    WaitUntil(usize, u64),
    /// The current task blocks until [`Table::wake_user_key`] names this key, or a
    /// [`Table::wake_user_range`] covers it.
    WaitUser(UserKey),
    /// As `WaitUser`, but not past this counter value.
    WaitUserUntil(UserKey, u64),
    /// The current task has finished.
    Exit,
}

/// Facts about a task that only the kernel can measure.
pub trait TaskFacts {
    /// Spin locks and mutexes the task in `slot` holds.
    fn lock_depth(&self, slot: usize) -> u32;
    /// Live heap bytes owned by the task in `slot`.
    fn heap_bytes(&self, slot: usize) -> usize;
    /// How deep the user program in `slot` is in the kernel: nonzero from when a
    /// system call or fault takes it there (or it starts, loading) until it's about to
    /// resume the program. Always zero for a kernel-linked app.
    fn in_call(&self, slot: usize) -> u32;
}

/// The result of asking to kill a process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KillOutcome {
    /// The task is gone.
    Killed,
    /// The task is flagged and will be reaped at its next safe point.
    Pending,
    /// A kill was already pending.
    AlreadyPending,
    /// There is no such process.
    NotFound,
    /// The process may not be killed.
    Protected,
}

/// What [`Table::switch`] decided.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Switch {
    /// Slot whose register frame must be saved, if the outgoing task lives on.
    pub save: Option<usize>,
    /// Slot to run next, or `None` for the CPU's idle loop.
    pub next: Option<usize>,
    /// The outgoing task was reaped by a kill.
    pub reaped: bool,
}

/// The deadline of a wait that has one.
fn deadline_of(wait: Wait) -> Option<u64> {
    match wait {
        Wait::Until(deadline) | Wait::KeyUntil(_, deadline) | Wait::UserKeyUntil(_, deadline) => Some(deadline),
        Wait::Nothing | Wait::Key(_) | Wait::UserKey(_) => None,
    }
}

/// One process table entry, as far as policy is concerned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Task {
    /// Lifecycle state.
    pub state: State,
    /// Thread identifier, unique among tasks: for a process's first thread, and for a
    /// kernel-linked app, the process's pid too.
    pub pid: u32,
    /// The process this thread belongs to: its first thread's id.
    pub tgid: u32,
    /// What a blocked task is waiting for.
    pub wait: Wait,
    /// A kill is waiting for this task's next safe point.
    pub kill_pending: bool,
    /// After this counter value, a task with live heap is reaped regardless.
    pub kill_deadline: u64,
    /// Scheduler ticks consumed by this task across all CPUs.
    pub cpu_ticks: u64,
    /// Held off the CPU by job control until it is resumed. Only a pending kill
    /// overrides this, because a task must run to reach a safe point. A user
    /// program in a call isn't held until it has left it.
    pub stopped: bool,
    /// The system needs this task: it can't be killed or stopped.
    pub protected: bool,
    /// How urgent the task is, up to [`PRIORITY_MAX`].
    pub priority: u8,
    /// The CPU whose queue the task is on.
    pub home: u8,
    /// The CPUs this task may run on, as a bit each. Zero means any.
    pub affinity: u32,
    /// Who the task runs as, for the per-user caps of [`UserLimits`].
    pub uid: u32,
    /// Times this task was ready and something else was chosen.
    passed_over: u16,
}

/// What a user program's new task may not take: the slots kept for the kernel's own
/// work, so a thread or fork bomb can't stop a shell from running `kill`, and how many
/// tasks and processes one user other than root may have, so one user's bomb leaves
/// room for others'. Root is capped only by `keep`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UserLimits {
    /// Free slots to leave.
    pub keep: usize,
    /// Most live tasks, threads included, one user may have.
    pub tasks_per_uid: usize,
    /// Most live processes one user may have.
    pub processes_per_uid: usize,
}

impl UserLimits {
    /// Only `keep` free slots kept: no per-user caps.
    pub const fn keeping(keep: usize) -> Self {
        Self { keep, tasks_per_uid: usize::MAX, processes_per_uid: usize::MAX }
    }
}

/// The process table and accounting.
pub struct Table<const TASKS: usize, const CPUS: usize> {
    tasks: [Task; TASKS],
    /// Slot running on each CPU plus one, or zero while that CPU is idle.
    current: [u16; CPUS],
    next_pid: u32,
    online: usize,
    /// Timer ticks accounted since boot, summed over all CPUs.
    pub total_ticks: u64,
    /// Ticks a CPU spent waiting in an idle instruction.
    pub idle_ticks: u64,
    /// Timer ticks accounted per CPU, for per-core graphs.
    pub cpu_total_ticks: [u64; CPUS],
    /// Idle ticks accounted per CPU.
    pub cpu_idle_ticks: [u64; CPUS],
    /// Heap bytes left behind by tasks that were killed before freeing them.
    pub leaked_bytes: usize,
    /// Counter value each CPU's time was last accounted up to.
    accounted: [u64; CPUS],
    /// The `Ready` slots at each priority, and at any.
    ready: [Slots; LEVELS],
    ready_any: Slots,
    /// Slots passed over [`STARVATION_LIMIT`] times or more, whatever their state: when
    /// ready, they come before any priority.
    starved: Slots,
    /// `Blocked` slots with a deadline.
    timed: Slots,
    /// `Blocked` slots waiting on a kernel word, by its bucket.
    keys: [Slots; BUCKETS],
    /// `Blocked` slots waiting on a user word, by its bucket, and all of them.
    user_keys: [Slots; BUCKETS],
    user_waiters: Slots,
    /// Slots that have become `Free` since the kernel last [forgot](Self::forget_left)
    /// them: the exits it has to collect.
    left: Slots,
    /// Each slot's neighbours in a ring of its process's members: the slots of its
    /// thread group that are live or have left without being forgotten, so whether a
    /// thread is its process's last is one look rather than a scan. Stored as the
    /// neighbour XOR the slot, so a zeroed table has every slot a ring of its own.
    group_next: [u16; TASKS],
    group_prev: [u16; TASKS],
    /// Decide by scanning every slot, as before the bitmaps: the reference the host tests
    /// check them against.
    #[cfg(test)]
    scan: bool,
}

impl<const TASKS: usize, const CPUS: usize> Table<TASKS, CPUS> {
    /// An all-zero table: every slot free and every CPU idle.
    pub const fn zeroed() -> Self {
        const {
            assert!(TASKS >= 1 && TASKS <= MAX_SLOTS);
            assert!(CPUS >= 1 && CPUS <= 32);
        }
        const TASK: Task = Task {
            state: State::Free,
            pid: 0,
            tgid: 0,
            wait: Wait::Nothing,
            kill_pending: false,
            kill_deadline: 0,
            cpu_ticks: 0,
            stopped: false,
            protected: false,
            priority: 0,
            home: 0,
            affinity: 0,
            uid: 0,
            passed_over: 0,
        };
        Self {
            tasks: [TASK; TASKS],
            current: [0; CPUS],
            next_pid: 0,
            online: 0,
            total_ticks: 0,
            idle_ticks: 0,
            cpu_total_ticks: [0; CPUS],
            cpu_idle_ticks: [0; CPUS],
            leaked_bytes: 0,
            accounted: [0; CPUS],
            ready: [Slots::EMPTY; LEVELS],
            ready_any: Slots::EMPTY,
            starved: Slots::EMPTY,
            timed: Slots::EMPTY,
            keys: [Slots::EMPTY; BUCKETS],
            user_keys: [Slots::EMPTY; BUCKETS],
            user_waiters: Slots::EMPTY,
            left: Slots::EMPTY,
            group_next: [0; TASKS],
            group_prev: [0; TASKS],
            #[cfg(test)]
            scan: false,
        }
    }

    /// Adds the slot to, or takes it out of, every bitmap its state, wait and priority
    /// put it in.
    fn index(&mut self, slot: usize, on: bool) {
        fn mark(set: &mut Slots, slot: usize, on: bool) {
            if on {
                set.insert(slot);
            } else {
                set.remove(slot);
            }
        }
        let task = self.tasks[slot];
        match task.state {
            State::Ready => {
                mark(&mut self.ready[(task.priority as usize).min(LEVELS - 1)], slot, on);
                mark(&mut self.ready_any, slot, on);
            }
            State::Blocked => {
                match task.wait {
                    Wait::Key(key) | Wait::KeyUntil(key, _) => mark(&mut self.keys[key_bucket(key)], slot, on),
                    Wait::UserKey(key) | Wait::UserKeyUntil(key, _) => {
                        mark(&mut self.user_keys[user_bucket(key)], slot, on);
                        mark(&mut self.user_waiters, slot, on);
                    }
                    Wait::Nothing | Wait::Until(_) => {}
                }
                if let Wait::Until(_) | Wait::KeyUntil(..) | Wait::UserKeyUntil(..) = task.wait {
                    mark(&mut self.timed, slot, on);
                }
            }
            State::Free | State::Spawning | State::Running => {}
        }
    }

    /// Changes the task in `slot` with `change`, keeping the bitmaps right: the only way
    /// its state, wait or priority changes.
    fn update(&mut self, slot: usize, change: impl FnOnce(&mut Task)) {
        self.index(slot, false);
        change(&mut self.tasks[slot]);
        self.index(slot, true);
    }

    /// Makes the task in `slot` runnable, waiting for nothing.
    fn make_ready(&mut self, slot: usize) {
        self.update(slot, |task| {
            task.state = State::Ready;
            task.wait = Wait::Nothing;
        });
    }

    /// Slots that have left the table since the kernel last [forgot](Self::forget_left)
    /// them, so that collecting exits needn't look at every slot.
    pub fn left(&self) -> Slots {
        self.left
    }

    /// The kernel has collected what the slot left behind, or has nothing to. It stops
    /// counting as one of its process's threads.
    pub fn forget_left(&mut self, slot: usize) {
        if self.left.contains(slot) {
            self.left.remove(slot);
            if self.tasks[slot].state == State::Free {
                self.leave_group(slot);
            }
        }
    }

    fn group_next(&self, slot: usize) -> usize {
        usize::from(self.group_next[slot]) ^ slot
    }

    fn group_prev(&self, slot: usize) -> usize {
        usize::from(self.group_prev[slot]) ^ slot
    }

    fn set_group_links(&mut self, slot: usize, prev: usize, next: usize) {
        // Slots fit in a u16: `MAX_SLOTS` says so.
        self.group_prev[slot] = (prev ^ slot) as u16;
        self.group_next[slot] = (next ^ slot) as u16;
    }

    /// Takes `slot` out of its process's ring, leaving it a ring of its own.
    fn leave_group(&mut self, slot: usize) {
        let (prev, next) = (self.group_prev(slot), self.group_next(slot));
        self.group_next[prev] = (next ^ prev) as u16;
        self.group_prev[next] = (prev ^ next) as u16;
        self.set_group_links(slot, slot, slot);
    }

    /// Puts `slot`, a ring of its own, into the ring `member` is in.
    fn join_group(&mut self, slot: usize, member: usize) {
        let next = self.group_next(member);
        self.set_group_links(slot, member, next);
        self.group_next[member] = (slot ^ member) as u16;
        self.group_prev[next] = (slot ^ next) as u16;
    }

    /// Creates the boot task as slot 0, PID 1, running on CPU 0.
    pub fn init(&mut self) {
        self.tasks[0].state = State::Running;
        self.tasks[0].pid = 1;
        self.tasks[0].tgid = 1;
        self.tasks[0].priority = PRIORITY_NORMAL;
        self.current = [0; CPUS];
        self.current[0] = 1;
        self.next_pid = 2;
        self.online = 1;
    }

    /// Records one more CPU joining the scheduler.
    pub fn cpu_online(&mut self) -> usize {
        self.online = (self.online + 1).min(CPUS);
        self.online
    }

    /// CPUs that have joined the scheduler.
    pub const fn online(&self) -> usize {
        self.online
    }

    /// All slots.
    pub const fn tasks(&self) -> &[Task; TASKS] {
        &self.tasks
    }

    /// The slot running on `cpu`.
    pub fn current(&self, cpu: usize) -> Option<usize> {
        (self.current[cpu] as usize).checked_sub(1)
    }

    /// Finds a live slot of the process `pid`: its first thread while that's there, and
    /// otherwise any other, so a process whose first thread has ended is still found.
    pub fn find_pid(&self, pid: u32) -> Option<usize> {
        self.find_tid(pid)
            .filter(|&slot| self.tasks[slot].tgid == pid)
            .or_else(|| self.tasks.iter().position(|task| task.state != State::Free && task.tgid == pid))
    }

    /// Finds the live slot of the thread `tid`.
    pub fn find_tid(&self, tid: u32) -> Option<usize> {
        self.tasks
            .iter()
            .position(|task| task.state != State::Free && task.pid == tid)
    }

    /// Live processes and threads: tasks that are ready, running or blocked, and the
    /// processes they're threads of. A process is counted at its first thread while
    /// that's live, and otherwise at its lowest live one, which only a process whose
    /// first thread has ended has to search for.
    pub fn counts(&self) -> (usize, usize) {
        let live = |task: &Task| matches!(task.state, State::Ready | State::Running | State::Blocked);
        let mut processes = 0;
        let mut threads = 0;
        for (slot, task) in self.tasks.iter().enumerate() {
            if !live(task) {
                continue;
            }
            threads += 1;
            let first = task.pid == task.tgid
                || (self.find_tid(task.tgid).is_none()
                    && !self.tasks[..slot].iter().any(|other| live(other) && other.tgid == task.tgid));
            processes += usize::from(first);
        }
        (processes, threads)
    }

    /// Whether the slot is a live thread of the process `pid`.
    fn in_group(&self, slot: usize, pid: u32) -> bool {
        let task = &self.tasks[slot];
        task.state != State::Free && task.tgid == pid
    }

    /// The live threads of the process `pid`.
    pub fn threads(&self, pid: u32) -> usize {
        (0..TASKS).filter(|&slot| self.in_group(slot, pid)).count()
    }

    /// Whether the slot, which has left the table, was the last thread of its process:
    /// no other thread of it is live, or has left without the kernel's having
    /// [forgotten](Self::forget_left) it yet. Its process's exit is recorded then, and
    /// only then. One look at the slot's ring of its process's members, so collecting
    /// a thousand threads' exits doesn't scan the table a thousand times.
    pub fn last_of_group(&self, slot: usize) -> bool {
        #[cfg(test)]
        if self.scan {
            let tgid = self.tasks[slot].tgid;
            return !(0..TASKS).any(|other| {
                other != slot
                    && self.tasks[other].tgid == tgid
                    && (self.tasks[other].state != State::Free || self.left.contains(other))
            });
        }
        self.group_next(slot) == slot
    }

    /// The slots of `slot`'s process, `slot` first, from its ring of members: whatever
    /// state each is in, those that have left the table and aren't yet forgotten among
    /// them, so a caller filters by state. One walk of the ring, for whatever has to visit
    /// each process's threads without searching the table for each.
    pub fn group_members(&self, slot: usize) -> impl Iterator<Item = usize> + '_ {
        let mut next = Some(slot);
        // A ring holds each slot once, so it can't be longer than the table.
        core::iter::from_fn(move || {
            let member = next?;
            let after = self.group_next(member);
            next = (after != slot).then_some(after);
            Some(member)
        })
        .take(TASKS)
    }

    /// Returns a CPU in `0..online` other than `except` that is idle.
    pub fn idle_cpu(&self, except: usize) -> Option<usize> {
        (0..self.online).find(|&cpu| cpu != except && self.current[cpu] == 0)
    }

    /// Reserves a slot for a new task, a process of its own. Slot 0 belongs to the boot
    /// task.
    pub fn reserve(&mut self) -> Option<(usize, u32)> {
        self.reserve_keeping(0)
    }

    /// Reserves a slot for a new process as [`reserve`](Self::reserve) does, unless that
    /// would leave fewer than `keep` free: what a user program may take, so that however
    /// many it starts there's room left for the kernel's own work, a shell's `kill` among
    /// it.
    pub fn reserve_keeping(&mut self, keep: usize) -> Option<(usize, u32)> {
        self.reserve_user(0, &UserLimits::keeping(keep))
    }

    /// Reserves a slot for a new user program's process, run as `uid`, within `limits`:
    /// `None` if that would leave fewer than `limits.keep` free, or, for a user other
    /// than root, give `uid` more tasks or processes than its caps.
    pub fn reserve_user(&mut self, uid: u32, limits: &UserLimits) -> Option<(usize, u32)> {
        if uid != 0 && (self.uid_tasks(uid) >= limits.tasks_per_uid || self.uid_processes(uid) >= limits.processes_per_uid) {
            return None;
        }
        let (slot, pid) = self.reserve_slot(limits.keep)?;
        self.tasks[slot].uid = uid;
        Some((slot, pid))
    }

    /// Live tasks running as `uid`.
    pub fn uid_tasks(&self, uid: u32) -> usize {
        self.tasks.iter().filter(|task| task.state != State::Free && task.uid == uid).count()
    }

    /// Live processes running as `uid`: each counted at its lowest slot. Only asked for a
    /// user other than root, which has few.
    pub fn uid_processes(&self, uid: u32) -> usize {
        let mine = |task: &Task| task.state != State::Free && task.uid == uid;
        (0..TASKS)
            .filter(|&slot| {
                let task = &self.tasks[slot];
                mine(task) && !self.tasks[..slot].iter().any(|other| mine(other) && other.tgid == task.tgid)
            })
            .count()
    }

    /// Sets who every thread of the process `pid` runs as.
    pub fn set_uid(&mut self, pid: u32, uid: u32) {
        for slot in 0..TASKS {
            if self.in_group(slot, pid) {
                self.tasks[slot].uid = uid;
            }
        }
    }

    /// Reserves a slot for a new thread of the process the task in `creator` belongs
    /// to, keeping `keep` free as [`reserve_keeping`](Self::reserve_keeping) does. The
    /// thread takes a new id, never a process's, and the creator's priority and CPUs,
    /// and its stop: a process stopped while one of its threads is starting another is
    /// all stopped, the new thread too, which starts at its program and so is held at
    /// once. `None` too if the creator is being killed: a thread started then would miss
    /// the kill its process is getting.
    pub fn reserve_thread(&mut self, creator: usize, keep: usize) -> Option<(usize, u32)> {
        self.reserve_user_thread(creator, &UserLimits::keeping(keep))
    }

    /// Reserves a slot for a new thread as [`reserve_thread`](Self::reserve_thread) does,
    /// within `limits`: the thread runs as its creator, and a user other than root may
    /// not have more tasks than its cap.
    pub fn reserve_user_thread(&mut self, creator: usize, limits: &UserLimits) -> Option<(usize, u32)> {
        let parent = self.tasks[creator];
        if parent.state == State::Free || parent.kill_pending {
            return None;
        }
        if parent.uid != 0 && self.uid_tasks(parent.uid) >= limits.tasks_per_uid {
            return None;
        }
        let (slot, tid) = self.reserve_slot(limits.keep)?;
        self.join_group(slot, creator);
        let task = &mut self.tasks[slot];
        task.tgid = parent.tgid;
        task.uid = parent.uid;
        task.priority = parent.priority;
        task.affinity = parent.affinity;
        task.stopped = parent.stopped;
        if parent.affinity != 0 && parent.affinity >> task.home & 1 == 0 {
            task.home = parent.affinity.trailing_zeros() as u8;
        }
        Some((slot, tid))
    }

    fn reserve_slot(&mut self, keep: usize) -> Option<(usize, u32)> {
        let free = (1..TASKS).filter(|&slot| self.tasks[slot].state == State::Free).count();
        if free <= keep {
            return None;
        }
        let slot = (1..TASKS).find(|&slot| self.tasks[slot].state == State::Free)?;
        // A slot that left and wasn't forgotten stops being its old process's.
        if self.left.contains(slot) {
            self.leave_group(slot);
        }
        let pid = self.next_pid;
        let home = self.least_loaded();
        self.next_pid = self.next_pid.wrapping_add(1).max(2);
        self.tasks[slot] = Task {
            state: State::Spawning,
            pid,
            tgid: pid,
            wait: Wait::Nothing,
            kill_pending: false,
            kill_deadline: 0,
            cpu_ticks: 0,
            stopped: false,
            protected: false,
            priority: PRIORITY_NORMAL,
            home,
            affinity: 0,
            uid: 0,
            passed_over: 0,
        };
        // It was free, so in no bitmap but these.
        self.starved.remove(slot);
        self.left.remove(slot);
        Some((slot, pid))
    }

    /// The online CPU with the fewest tasks homed on it that aren't blocked.
    fn least_loaded(&self) -> u8 {
        let mut load = [0usize; CPUS];
        for task in &self.tasks {
            if matches!(task.state, State::Ready | State::Running | State::Spawning) {
                if let Some(count) = load.get_mut(task.home as usize) {
                    *count += 1;
                }
            }
        }
        let online = self.online.clamp(1, CPUS);
        (0..online).min_by_key(|&cpu| load[cpu]).unwrap_or(0) as u8
    }

    /// Sets a task's priority. Returns `false` when there's no such task or the
    /// priority is out of range.
    pub fn set_priority(&mut self, pid: u32, priority: u8) -> bool {
        if priority > PRIORITY_MAX || self.find_pid(pid).is_none() {
            return false;
        }
        for slot in 0..TASKS {
            if self.in_group(slot, pid) {
                self.update(slot, |task| task.priority = priority);
            }
        }
        true
    }

    /// Sets the CPUs a task may run on, as a bit each. Returns `false` when there's
    /// no such task or the mask names no online CPU. A running task moves at its
    /// CPU's next tick.
    pub fn set_affinity(&mut self, pid: u32, mask: u32) -> bool {
        let online = if self.online >= 32 { u32::MAX } else { (1u32 << self.online) - 1 };
        if mask & online == 0 || self.find_pid(pid).is_none() {
            return false;
        }
        for slot in 0..TASKS {
            if self.in_group(slot, pid) {
                let task = &mut self.tasks[slot];
                task.affinity = mask;
                if mask >> task.home & 1 == 0 {
                    task.home = mask.trailing_zeros() as u8;
                }
            }
        }
        true
    }

    /// Makes a reserved slot runnable.
    pub fn commit(&mut self, slot: usize) {
        if self.tasks[slot].state == State::Spawning {
            self.make_ready(slot);
        }
    }

    /// Gives a reserved slot back without its having run: a spawn that couldn't finish.
    /// It hasn't left anything for the kernel to collect.
    pub fn cancel(&mut self, slot: usize) {
        if slot != 0 && self.tasks[slot].state == State::Spawning {
            self.tasks[slot].state = State::Free;
            self.leave_group(slot);
        }
    }

    fn free(&mut self, slot: usize) {
        self.update(slot, |task| {
            task.state = State::Free;
            task.wait = Wait::Nothing;
            task.kill_pending = false;
            task.stopped = false;
            task.protected = false;
        });
        self.left.insert(slot);
    }

    /// Marks `pid` as one the system needs, like the boot task: it can't be killed or
    /// stopped. Returns `false` when there's no such task.
    pub fn set_protected(&mut self, pid: u32) -> bool {
        match self.find_pid(pid) {
            Some(slot) => {
                self.tasks[slot].protected = true;
                true
            }
            None => false,
        }
    }

    /// Whether any thread of the process `pid` is one the system needs.
    fn group_protected(&self, pid: u32) -> bool {
        pid == self.tasks[0].tgid || (0..TASKS).any(|slot| self.in_group(slot, pid) && self.tasks[slot].protected)
    }

    /// Holds or releases `pid`. Returns `false` when it can't be: no such
    /// process, or the boot task or another the system needs running.
    ///
    /// A kernel-linked app is held from its CPU's next tick. A user program in a
    /// call carries on until it's about to resume the program, so that it's never
    /// held with a kernel lock or a half-made change: see [`TaskFacts::in_call`].
    pub fn set_stopped(&mut self, pid: u32, stopped: bool) -> bool {
        if self.find_pid(pid).is_none() || self.group_protected(pid) {
            return false;
        }
        for slot in 0..TASKS {
            if self.in_group(slot, pid) {
                self.tasks[slot].stopped = stopped;
            }
        }
        true
    }

    /// Frees a killed task, charging any heap it still owns as leaked.
    fn reap(&mut self, slot: usize, facts: &impl TaskFacts) {
        self.leaked_bytes = self.leaked_bytes.saturating_add(facts.heap_bytes(slot));
        self.free(slot);
    }

    /// Returns `true` when the flagged task in `slot` can be reaped now.
    ///
    /// Holding a lock always defers: reaping then would strand the lock. So does
    /// a user program's being in a call, which ends itself instead once the call has
    /// unwound. Live heap defers only until the deadline, which gives a task that
    /// polls `kill_requested` time to unwind and free it.
    fn reapable(&self, slot: usize, now: u64, facts: &impl TaskFacts) -> bool {
        let task = &self.tasks[slot];
        task.kill_pending
            && facts.lock_depth(slot) == 0
            && facts.in_call(slot) == 0
            && (facts.heap_bytes(slot) == 0 || now >= task.kill_deadline)
    }

    /// Whether job control is holding the task in `slot` off the CPU now.
    fn held(&self, slot: usize, facts: &impl TaskFacts) -> bool {
        let task = &self.tasks[slot];
        task.stopped && !task.kill_pending && facts.in_call(slot) == 0
    }

    /// Whether the task in `slot` should stop before it next resumes its program: a
    /// stop, or a kill, came while it was in a call. The kernel asks on the way back to
    /// the program, and switches instead of going straight there.
    pub fn stop_or_kill_pending(&self, slot: usize) -> bool {
        let task = &self.tasks[slot];
        task.stopped || task.kill_pending
    }

    /// Asks for `pid` to die.
    ///
    /// `grace` is how long, in counter ticks, a task with live heap may keep
    /// running to free it. A task that isn't running and is safe to reap goes
    /// at once. Otherwise the flag is honoured by [`switch`](Self::switch) at
    /// the owner's next tick. A task that must run to release something is
    /// woken so that it can.
    ///
    /// Every thread of the process is flagged: `Killed` if each went at once,
    /// `AlreadyPending` if each was flagged already, and `Pending` otherwise.
    pub fn kill(&mut self, pid: u32, now: u64, grace: u64, facts: &impl TaskFacts) -> KillOutcome {
        if pid == self.tasks[0].tgid {
            return KillOutcome::Protected;
        }
        if self.find_pid(pid).is_none() {
            return KillOutcome::NotFound;
        }
        if self.group_protected(pid) {
            return KillOutcome::Protected;
        }
        let (mut killed, mut already, mut pending) = (0, 0, 0);
        for slot in 0..TASKS {
            if !self.in_group(slot, pid) {
                continue;
            }
            match self.kill_slot(slot, now, grace, facts) {
                KillOutcome::Killed => killed += 1,
                KillOutcome::AlreadyPending => already += 1,
                _ => pending += 1,
            }
        }
        if pending == 0 && killed == 0 {
            KillOutcome::AlreadyPending
        } else if pending == 0 && already == 0 {
            KillOutcome::Killed
        } else {
            KillOutcome::Pending
        }
    }

    /// Asks for the one thread in `slot` to die, as [`kill`](Self::kill) does for each
    /// of a process's: for a process ending itself, whose other threads go with it.
    pub fn kill_slot(&mut self, slot: usize, now: u64, grace: u64, facts: &impl TaskFacts) -> KillOutcome {
        if slot == 0 || self.tasks[slot].state == State::Free {
            return KillOutcome::NotFound;
        }
        if self.tasks[slot].protected {
            return KillOutcome::Protected;
        }
        if self.tasks[slot].kill_pending {
            return KillOutcome::AlreadyPending;
        }

        let task = &mut self.tasks[slot];
        task.kill_pending = true;
        task.kill_deadline = now.saturating_add(grace);

        match self.tasks[slot].state {
            State::Ready | State::Blocked if self.reapable(slot, now, facts) => {
                self.reap(slot, facts);
                KillOutcome::Killed
            }
            State::Blocked => {
                self.make_ready(slot);
                KillOutcome::Pending
            }
            _ => KillOutcome::Pending,
        }
    }

    /// Accounts one timer tick on `cpu`.
    pub fn account_tick(&mut self, cpu: usize, idle_hint: bool) {
        self.total_ticks = self.total_ticks.wrapping_add(1);
        self.cpu_total_ticks[cpu] = self.cpu_total_ticks[cpu].wrapping_add(1);

        match self.current(cpu) {
            Some(slot) if !idle_hint => {
                self.tasks[slot].cpu_ticks = self.tasks[slot].cpu_ticks.wrapping_add(1);
            }
            // A tick spent inside `WFI` is idle, not CPU time for the task.
            _ => {
                self.idle_ticks = self.idle_ticks.wrapping_add(1);
                self.cpu_idle_ticks[cpu] = self.cpu_idle_ticks[cpu].wrapping_add(1);
            }
        }
    }

    /// Accounts the time on `cpu` since the last call, in whole ticks of
    /// `tick_length` counter units. A CPU with nothing to do takes no timer
    /// interrupts, so this is how the time it spent that way gets counted.
    pub fn account_elapsed(&mut self, cpu: usize, now: u64, tick_length: u64, idle_hint: bool) {
        if tick_length == 0 {
            return;
        }
        if self.accounted[cpu] == 0 {
            self.accounted[cpu] = now;
            return;
        }
        let ticks = now.wrapping_sub(self.accounted[cpu]) / tick_length;
        self.accounted[cpu] = self.accounted[cpu].wrapping_add(ticks * tick_length);
        self.total_ticks = self.total_ticks.wrapping_add(ticks);
        self.cpu_total_ticks[cpu] = self.cpu_total_ticks[cpu].wrapping_add(ticks);
        match self.current(cpu) {
            Some(slot) if !idle_hint => {
                self.tasks[slot].cpu_ticks = self.tasks[slot].cpu_ticks.wrapping_add(ticks);
            }
            _ => {
                self.idle_ticks = self.idle_ticks.wrapping_add(ticks);
                self.cpu_idle_ticks[cpu] = self.cpu_idle_ticks[cpu].wrapping_add(ticks);
            }
        }
    }

    /// When `cpu`'s timer should next fire: the end of a time slice of `slice`, or
    /// sooner, for a sleeper on this CPU's queue. A CPU that's idle has no slice to
    /// end, so it can go as long as `idle_limit`.
    pub fn next_deadline(&self, cpu: usize, now: u64, slice: u64, idle_limit: u64) -> u64 {
        let wait = if self.current(cpu).is_some() { slice } else { idle_limit };
        let mut deadline = now.saturating_add(wait);
        for slot in self.among(&self.timed).iter() {
            let task = &self.tasks[slot];
            if task.state == State::Blocked && task.home as usize == cpu {
                if let Some(until) = deadline_of(task.wait) {
                    deadline = deadline.min(until);
                }
            }
        }
        deadline
    }

    /// The slots in `set`, or, in the host tests' reference mode, every slot. Each use
    /// checks a slot's state for itself, so the two decide alike exactly when the bitmap
    /// holds every slot it should.
    fn among(&self, set: &Slots) -> Slots {
        #[cfg(test)]
        if self.scan {
            let mut all = Slots::EMPTY;
            (0..TASKS).for_each(|slot| all.insert(slot));
            return all;
        }
        *set
    }

    /// Makes every sleeper whose deadline has passed runnable.
    fn wake_expired(&mut self, now: u64) {
        for slot in self.among(&self.timed).iter() {
            let task = &self.tasks[slot];
            if task.state == State::Blocked && deadline_of(task.wait).is_some_and(|deadline| now >= deadline) {
                self.make_ready(slot);
            }
        }
    }

    /// Wakes every thread of `pid` that's blocked, whatever for: waits may always wake
    /// spuriously. Whether any was.
    pub fn wake_pid(&mut self, pid: u32) -> bool {
        let mut woken = false;
        for slot in 0..TASKS {
            if self.in_group(slot, pid) && self.tasks[slot].state == State::Blocked {
                self.make_ready(slot);
                woken = true;
            }
        }
        woken
    }

    /// Wakes up to `count` tasks blocked on `key` and returns how many woke. Never a
    /// user program's futex wait, whatever its word's frame.
    pub fn wake_key(&mut self, key: usize, count: usize) -> usize {
        let bucket = self.keys[key_bucket(key)];
        self.wake_matching(&bucket, count, |wait| match wait {
            Wait::Key(waited) | Wait::KeyUntil(waited, _) => waited == key,
            _ => false,
        })
    }

    /// Wakes up to `count` tasks blocked on the user futex word `key`, and returns how
    /// many woke.
    pub fn wake_user_key(&mut self, key: UserKey, count: usize) -> usize {
        let bucket = self.user_keys[user_bucket(key)];
        self.wake_matching(&bucket, count, |wait| match wait {
            Wait::UserKey(waited) | Wait::UserKeyUntil(waited, _) => waited == key,
            _ => false,
        })
    }

    /// Wakes every task blocked on a user futex word of `space` in `[low, high)`: the
    /// memory has gone from under them, and a wait must never outlive its word.
    pub fn wake_user_range(&mut self, space: usize, low: u64, high: u64) -> usize {
        let waiters = self.user_waiters;
        self.wake_matching(&waiters, usize::MAX, |wait| match wait {
            Wait::UserKey(key) | Wait::UserKeyUntil(key, _) => key.space == space && (low..high).contains(&key.offset),
            _ => false,
        })
    }

    /// Wakes up to `count` of the slots in `waiters` blocked on a wait that `matches`,
    /// lowest first.
    fn wake_matching(&mut self, waiters: &Slots, count: usize, matches: impl Fn(Wait) -> bool) -> usize {
        let mut woken = 0;
        for slot in self.among(waiters).iter() {
            if woken == count {
                break;
            }
            let task = &self.tasks[slot];
            if task.state == State::Blocked && matches(task.wait) {
                self.make_ready(slot);
                woken += 1;
            }
        }
        woken
    }

    /// Whether `slot` may run on `cpu`.
    fn allowed(&self, slot: usize, cpu: usize) -> bool {
        let affinity = self.tasks[slot].affinity;
        affinity == 0 || affinity >> cpu & 1 != 0
    }

    /// Takes what `cpu` should run next, looking from the slot after `after` so
    /// that equals take turns. See the module's documentation for the rules.
    ///
    /// A flagged task found on the way is reaped instead of being run. Only the ready
    /// tasks are looked at: the starved first, then each priority from the top, and the
    /// first that has a task this CPU may run decides.
    fn take_next(
        &mut self,
        cpu: usize,
        after: Option<usize>,
        now: u64,
        facts: &impl TaskFacts,
    ) -> Option<usize> {
        #[cfg(test)]
        if self.scan {
            return self.take_next_scan(cpu, after, now, facts);
        }
        let first = (after.unwrap_or(TASKS - 1) + 1) % TASKS;

        // Everything this CPU could run, and how long each CPU's queue is.
        let mut queued = [0usize; CPUS];
        let ready = self.ready_any;
        for slot in ready.iter() {
            if self.reapable(slot, now, facts) {
                self.reap(slot, facts);
                continue;
            }
            if self.held(slot, facts) {
                continue;
            }
            if let Some(count) = queued.get_mut(self.tasks[slot].home as usize) {
                *count += 1;
            }
        }

        let starved = self.ready_any.and(&self.starved);
        let mut chosen = self.best_of(&starved, first, cpu, &queued, facts);
        for level in (0..LEVELS).rev() {
            if chosen.is_some() {
                break;
            }
            let set = self.ready[level].and_not(&self.starved);
            if !set.is_empty() {
                chosen = self.best_of(&set, first, cpu, &queued, facts);
            }
        }

        let chosen = chosen?;
        let ready = self.ready_any;
        for slot in ready.iter() {
            if slot != chosen && self.allowed(slot, cpu) {
                self.pass_over(slot);
            }
        }
        self.choose(chosen, cpu);
        Some(chosen)
    }

    /// Of the ready slots in `set`, all equally urgent, the one `cpu` should take: from
    /// a longer queue elsewhere first, then from this CPU's own, then whatever's left so
    /// that the CPU isn't idle; among equals, the first from `first` on.
    fn best_of(&self, set: &Slots, first: usize, cpu: usize, queued: &[usize; CPUS], facts: &impl TaskFacts) -> Option<usize> {
        let mine = queued[cpu];
        let mut best: Option<(usize, u8)> = None;
        for slot in set.cyclic(first) {
            if self.held(slot, facts) || !self.allowed(slot, cpu) {
                continue;
            }
            let home = self.tasks[slot].home as usize;
            let preference = if home == cpu {
                1
            } else if queued.get(home).is_some_and(|&theirs| theirs > mine) {
                2
            } else {
                0
            };
            if best.is_none_or(|(_, held)| preference > held) {
                best = Some((slot, preference));
                if preference == 2 {
                    break;
                }
            }
        }
        best.map(|(slot, _)| slot)
    }

    /// Counts a turn the ready task in `slot` didn't get.
    fn pass_over(&mut self, slot: usize) {
        let task = &mut self.tasks[slot];
        task.passed_over = task.passed_over.saturating_add(1);
        if task.passed_over >= STARVATION_LIMIT {
            self.starved.insert(slot);
        }
    }

    /// The task in `slot` is to run on `cpu`: it's had its turn, and it's that CPU's now.
    fn choose(&mut self, slot: usize, cpu: usize) {
        let task = &mut self.tasks[slot];
        task.passed_over = 0;
        task.home = cpu as u8;
        self.starved.remove(slot);
    }

    /// [`take_next`](Self::take_next) as it was before the bitmaps, scanning every slot:
    /// the reference `bitmap_pick_matches_scan` checks it against.
    #[cfg(test)]
    fn take_next_scan(
        &mut self,
        cpu: usize,
        after: Option<usize>,
        now: u64,
        facts: &impl TaskFacts,
    ) -> Option<usize> {
        let start = after.unwrap_or(TASKS - 1);

        let mut queued = [0usize; CPUS];
        for slot in 0..TASKS {
            if self.tasks[slot].state != State::Ready {
                continue;
            }
            if self.reapable(slot, now, facts) {
                self.reap(slot, facts);
                continue;
            }
            if self.held(slot, facts) {
                continue;
            }
            if let Some(count) = queued.get_mut(self.tasks[slot].home as usize) {
                *count += 1;
            }
        }
        let mine = queued[cpu];

        let mut best = None;
        let mut best_rank = (0u8, 0u8);
        for step in 1..=TASKS {
            let slot = (start + step) % TASKS;
            if self.tasks[slot].state != State::Ready || self.held(slot, facts) || !self.allowed(slot, cpu) {
                continue;
            }
            let task = &self.tasks[slot];
            let urgency = if task.passed_over >= STARVATION_LIMIT {
                PRIORITY_MAX + 1
            } else {
                task.priority
            };
            let home = task.home as usize;
            let preference = if home == cpu {
                1
            } else if queued.get(home).is_some_and(|&theirs| theirs > mine) {
                2
            } else {
                0
            };
            let rank = (urgency + 1, preference);
            if rank > best_rank {
                best_rank = rank;
                best = Some(slot);
            }
        }

        let chosen = best?;
        for slot in 0..TASKS {
            if slot != chosen && self.tasks[slot].state == State::Ready && self.allowed(slot, cpu) {
                self.pass_over(slot);
            }
        }
        self.choose(chosen, cpu);
        Some(chosen)
    }

    /// Retires the current task on `cpu` according to `request` and picks what
    /// runs next.
    ///
    /// The caller holds the scheduler lock and owns the CPU's register frame:
    /// it saves the frame to [`Switch::save`] and loads [`Switch::next`].
    pub fn switch(
        &mut self,
        cpu: usize,
        request: Request,
        now: u64,
        facts: &impl TaskFacts,
    ) -> Switch {
        let current = self.current(cpu);
        let mut save = None;
        let mut reaped = false;

        if let Some(slot) = current {
            if request == Request::Exit {
                self.free(slot);
            } else if self.reapable(slot, now, facts) {
                self.reap(slot, facts);
                reaped = true;
            } else {
                let task = &mut self.tasks[slot];
                // A flagged task must not block: it has to run to reach a safe
                // point.
                let (state, wait) = match request {
                    Request::Sleep(deadline) if !task.kill_pending && now < deadline => {
                        (State::Blocked, Wait::Until(deadline))
                    }
                    Request::Wait(key) if !task.kill_pending => (State::Blocked, Wait::Key(key)),
                    Request::WaitUntil(key, deadline) if !task.kill_pending && now < deadline => {
                        (State::Blocked, Wait::KeyUntil(key, deadline))
                    }
                    Request::WaitUser(key) if !task.kill_pending => (State::Blocked, Wait::UserKey(key)),
                    Request::WaitUserUntil(key, deadline) if !task.kill_pending && now < deadline => {
                        (State::Blocked, Wait::UserKeyUntil(key, deadline))
                    }
                    _ => (State::Ready, Wait::Nothing),
                };
                self.update(slot, |task| {
                    task.state = state;
                    task.wait = wait;
                });
                save = Some(slot);
            }
        }

        if request == Request::Preempt {
            self.wake_expired(now);
        }

        let next = self.take_next(cpu, current, now, facts);
        if let Some(slot) = next {
            self.update(slot, |task| task.state = State::Running);
        }
        self.current[cpu] = next.map_or(0, |slot| slot as u16 + 1);

        Switch { save, next, reaped }
    }
}

/// Adjusts the heap bytes charged to the thread `owner`: `bytes[slot]` for the slot
/// whose `ids[slot]` is `owner`, if any. Lock-free, for the heap, which may run under
/// the scheduler's lock or none.
///
/// The heap records each block's owner by thread id, which names one slot, so a
/// thread's charges are its own even though its process's pid names another slot (or,
/// once the first thread has ended, none). Blocks from a thread that has gone are
/// charged to nobody. The caller serialises charges (the heap lock), so they can't race.
pub fn charge_heap(ids: &[core::sync::atomic::AtomicU32], bytes: &[core::sync::atomic::AtomicUsize], owner: u32, delta: isize) {
    charge_heap_hinted(ids, bytes, owner, delta, None);
}

/// [`charge_heap`], looking first at `hint`, the slot the owner most likely is: the
/// calling task's, for an allocation and most frees. With a table of a thousand slots,
/// that saves a search on nearly every heap call.
pub fn charge_heap_hinted(
    ids: &[core::sync::atomic::AtomicU32],
    bytes: &[core::sync::atomic::AtomicUsize],
    owner: u32,
    delta: isize,
    hint: Option<usize>,
) {
    use core::sync::atomic::Ordering;
    if owner == 0 || delta == 0 {
        return;
    }
    let hinted = hint.filter(|&slot| ids.get(slot).is_some_and(|id| id.load(Ordering::Relaxed) == owner));
    let Some(slot) = hinted.or_else(|| ids.iter().position(|id| id.load(Ordering::Relaxed) == owner)) else {
        return;
    };
    let Some(bytes) = bytes.get(slot) else {
        return;
    };
    let current = bytes.load(Ordering::Relaxed);
    bytes.store(current.saturating_add_signed(delta), Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASKS: usize = 4;
    const CPUS: usize = 2;
    const GRACE: u64 = 100;

    type TestTable = Table<TASKS, CPUS>;

    #[derive(Default)]
    struct Facts {
        depth: [u32; TASKS],
        heap: [usize; TASKS],
        calls: [u32; TASKS],
    }

    impl TaskFacts for Facts {
        fn lock_depth(&self, slot: usize) -> u32 {
            self.depth[slot]
        }

        fn heap_bytes(&self, slot: usize) -> usize {
            self.heap[slot]
        }

        fn in_call(&self, slot: usize) -> u32 {
            self.calls[slot]
        }
    }

    fn table() -> TestTable {
        let mut table = TestTable::zeroed();
        table.init();
        table
    }

    fn spawn(table: &mut TestTable) -> (usize, u32) {
        let (slot, pid) = table.reserve().expect("free slot");
        table.commit(slot);
        (slot, pid)
    }

    fn state(table: &TestTable, slot: usize) -> State {
        table.tasks()[slot].state
    }

    #[test]
    fn zeroed_table_has_no_tasks() {
        let table = TestTable::zeroed();
        assert!(table.tasks().iter().all(|task| task.state == State::Free));
        assert_eq!(table.current(0), None);
    }

    #[test]
    fn slot_zero_is_never_reserved_and_pids_increase() {
        let mut table = table();
        assert_eq!(table.reserve(), Some((1, 2)));
        assert_eq!(table.reserve(), Some((2, 3)));
        assert_eq!(table.reserve(), Some((3, 4)));
        assert_eq!(table.reserve(), None);
    }

    #[test]
    fn a_reserved_slot_is_not_runnable_until_committed() {
        let mut table = table();
        let facts = Facts::default();
        let (slot, _) = table.reserve().unwrap();
        let switch = table.switch(1, Request::Preempt, 0, &facts);
        assert_eq!(switch.next, None);
        table.commit(slot);
        let switch = table.switch(1, Request::Preempt, 0, &facts);
        assert_eq!(switch.next, Some(slot));
    }

    #[test]
    fn round_robin_rotates_and_saves_the_outgoing_task() {
        let mut table = table();
        let facts = Facts::default();
        let (first, _) = spawn(&mut table);
        let (second, _) = spawn(&mut table);

        let switch = table.switch(0, Request::Preempt, 0, &facts);
        assert_eq!(switch.save, Some(0));
        assert_eq!(switch.next, Some(first));
        assert_eq!(state(&table, 0), State::Ready);
        assert_eq!(state(&table, first), State::Running);

        let switch = table.switch(0, Request::Preempt, 0, &facts);
        assert_eq!(switch.next, Some(second));
        let switch = table.switch(0, Request::Preempt, 0, &facts);
        assert_eq!(switch.next, Some(0));
    }

    #[test]
    fn a_lone_task_keeps_running() {
        let mut table = table();
        let facts = Facts::default();
        let switch = table.switch(0, Request::Preempt, 0, &facts);
        assert_eq!(switch.next, Some(0));
        assert_eq!(state(&table, 0), State::Running);
    }

    #[test]
    fn two_cpus_never_share_a_task() {
        let mut table = table();
        table.cpu_online();
        let facts = Facts::default();
        spawn(&mut table);
        let one = table.switch(1, Request::Preempt, 0, &facts).next;
        let zero = table.switch(0, Request::Preempt, 0, &facts).next;
        assert_ne!(one, zero);
        // Nothing else is runnable, so CPU 1 keeps its task after a tick.
        assert_eq!(table.switch(1, Request::Preempt, 0, &facts).next, one);
    }

    #[test]
    fn an_idle_cpu_is_found_only_among_online_cpus() {
        let mut table = table();
        assert_eq!(table.idle_cpu(0), None);
        table.cpu_online();
        assert_eq!(table.idle_cpu(0), Some(1));
        assert_eq!(table.idle_cpu(1), None);
    }

    #[test]
    fn exit_frees_the_slot_and_saves_nothing() {
        let mut table = table();
        let facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        table.switch(1, Request::Preempt, 0, &facts);
        let switch = table.switch(1, Request::Exit, 0, &facts);
        assert_eq!(switch.save, None);
        assert_eq!(switch.next, None);
        assert_eq!(state(&table, slot), State::Free);
        assert_eq!(table.find_pid(pid), None);
    }

    #[test]
    fn kill_refuses_the_boot_task_and_unknown_pids() {
        let mut table = table();
        let facts = Facts::default();
        assert_eq!(table.kill(1, 0, GRACE, &facts), KillOutcome::Protected);
        assert_eq!(table.kill(9, 0, GRACE, &facts), KillOutcome::NotFound);
    }

    #[test]
    fn a_running_task_dies_at_its_owners_next_tick() {
        // KRN-3: a running target used to be refused outright.
        let mut table = table();
        let facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        assert_eq!(table.switch(1, Request::Preempt, 0, &facts).next, Some(slot));

        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Pending);
        assert_eq!(state(&table, slot), State::Running);
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::AlreadyPending);

        let switch = table.switch(1, Request::Preempt, 1, &facts);
        assert!(switch.reaped);
        assert_eq!(switch.save, None);
        assert_eq!(switch.next, None);
        assert_eq!(state(&table, slot), State::Free);
        assert_eq!(table.kill(pid, 2, GRACE, &facts), KillOutcome::NotFound);
    }

    #[test]
    fn a_ready_task_with_nothing_held_dies_at_once() {
        let mut table = table();
        let facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Killed);
        assert_eq!(state(&table, slot), State::Free);
    }

    #[test]
    fn a_task_holding_a_lock_is_only_reaped_once_it_lets_go() {
        // KRN-2: reaping a lock holder wedged every later user of that lock.
        let mut table = table();
        let mut facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        facts.depth[slot] = 1;

        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Pending);
        assert_eq!(state(&table, slot), State::Ready);

        // It keeps being scheduled, even well past the heap grace period.
        assert_eq!(table.switch(1, Request::Preempt, 1, &facts).next, Some(slot));
        assert_eq!(
            table.switch(1, Request::Preempt, GRACE * 10, &facts).next,
            Some(slot)
        );

        facts.depth[slot] = 0;
        let switch = table.switch(1, Request::Preempt, GRACE * 10 + 1, &facts);
        assert!(switch.reaped);
        assert_eq!(state(&table, slot), State::Free);
    }

    #[test]
    fn a_blocked_lock_holder_is_woken_so_it_can_release() {
        let mut table = table();
        let mut facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        facts.depth[slot] = 1;
        table.switch(1, Request::Preempt, 0, &facts);
        table.switch(1, Request::Sleep(1_000), 0, &facts);
        assert_eq!(state(&table, slot), State::Blocked);

        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Pending);
        assert_eq!(state(&table, slot), State::Ready);
    }

    #[test]
    fn live_heap_earns_a_grace_period_then_counts_as_leaked() {
        // KRN-7: the kill has no way to free what the task owns, so it gets a
        // chance to unwind; if it doesn't take it, the loss is on record.
        let mut table = table();
        let mut facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        facts.heap[slot] = 4096;
        table.switch(1, Request::Preempt, 0, &facts);

        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Pending);
        let switch = table.switch(1, Request::Preempt, GRACE - 1, &facts);
        assert!(!switch.reaped);
        assert_eq!(switch.next, Some(slot));
        assert_eq!(table.leaked_bytes, 0);

        let switch = table.switch(1, Request::Preempt, GRACE, &facts);
        assert!(switch.reaped);
        assert_eq!(table.leaked_bytes, 4096);
    }

    #[test]
    fn a_task_that_frees_its_heap_in_the_grace_period_leaks_nothing() {
        let mut table = table();
        let mut facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        facts.heap[slot] = 4096;
        table.switch(1, Request::Preempt, 0, &facts);
        table.kill(pid, 0, GRACE, &facts);

        facts.heap[slot] = 0;
        let switch = table.switch(1, Request::Preempt, 1, &facts);
        assert!(switch.reaped);
        assert_eq!(table.leaked_bytes, 0);
    }

    #[test]
    fn normal_exit_is_not_counted_as_a_leak() {
        let mut table = table();
        let mut facts = Facts::default();
        let (slot, _) = spawn(&mut table);
        facts.heap[slot] = 4096;
        table.switch(1, Request::Preempt, 0, &facts);
        table.switch(1, Request::Exit, 1, &facts);
        assert_eq!(table.leaked_bytes, 0);
    }

    #[test]
    fn a_flagged_task_cannot_block() {
        let mut table = table();
        let mut facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        facts.heap[slot] = 16;
        table.switch(1, Request::Preempt, 0, &facts);
        table.kill(pid, 0, GRACE, &facts);

        table.switch(1, Request::Sleep(1_000), 1, &facts);
        assert_eq!(state(&table, slot), State::Running, "it must keep running");
        table.switch(1, Request::Wait(7), 2, &facts);
        assert_eq!(state(&table, slot), State::Running);
    }

    #[test]
    fn a_sleeper_wakes_at_its_deadline_and_not_before() {
        let mut table = table();
        let facts = Facts::default();
        let (slot, _) = spawn(&mut table);
        table.switch(1, Request::Preempt, 0, &facts);

        let switch = table.switch(1, Request::Sleep(100), 10, &facts);
        assert_eq!(switch.save, Some(slot));
        assert_eq!(switch.next, None);
        assert_eq!(state(&table, slot), State::Blocked);

        assert_eq!(table.switch(1, Request::Preempt, 99, &facts).next, None);
        assert_eq!(table.switch(1, Request::Preempt, 100, &facts).next, Some(slot));
    }

    #[test]
    fn a_sleep_that_has_already_expired_stays_runnable() {
        let mut table = table();
        let facts = Facts::default();
        let switch = table.switch(0, Request::Sleep(5), 10, &facts);
        assert_eq!(switch.next, Some(0));
    }

    #[test]
    fn sleeping_lets_another_task_use_the_cpu() {
        // KRN-5: an idle waiter used to hold its CPU until the next tick.
        let mut table = table();
        let facts = Facts::default();
        let (slot, _) = spawn(&mut table);
        let switch = table.switch(0, Request::Sleep(100), 0, &facts);
        assert_eq!(switch.next, Some(slot));
    }

    #[test]
    fn a_key_wake_targets_matching_waiters_only() {
        let mut table = table();
        let facts = Facts::default();
        let (first, _) = spawn(&mut table);
        let (second, _) = spawn(&mut table);
        table.switch(1, Request::Preempt, 0, &facts);
        table.switch(1, Request::Wait(0xa), 0, &facts);
        table.switch(1, Request::Wait(0xb), 0, &facts);
        assert_eq!(state(&table, first), State::Blocked);
        assert_eq!(state(&table, second), State::Blocked);

        assert_eq!(table.wake_key(0xc, 1), 0);
        assert_eq!(table.wake_key(0xb, 1), 1);
        assert_eq!(state(&table, first), State::Blocked);
        assert_eq!(state(&table, second), State::Ready);
    }

    #[test]
    fn a_key_wake_respects_the_count() {
        let mut table = table();
        let facts = Facts::default();
        spawn(&mut table);
        spawn(&mut table);
        table.switch(1, Request::Preempt, 0, &facts);
        table.switch(1, Request::Wait(0xa), 0, &facts);
        table.switch(1, Request::Wait(0xa), 0, &facts);
        assert_eq!(table.wake_key(0xa, 1), 1);
        assert_eq!(table.wake_key(0xa, 8), 1);
        assert_eq!(table.wake_key(0xa, 8), 0);
    }

    /// Makes `slot` the task on CPU 1, and has it make `request`.
    fn block_with(table: &mut TestTable, facts: &Facts, slot: usize, request: Request, now: u64) {
        for _ in 0..16 {
            if table.current(1) == Some(slot) {
                table.switch(1, request, now, facts);
                return;
            }
            table.switch(1, Request::Preempt, now, facts);
        }
        panic!("slot {slot} never ran on CPU 1");
    }

    #[test]
    fn user_key_never_matches_kernel_key() {
        // A kernel waiter on a word whose kernel address is 0x1000, and a user waiter on
        // a word whose frame is the same: neither wake reaches the other.
        let mut table = table();
        let facts = Facts::default();
        let (kernel, _) = spawn(&mut table);
        let (user, _) = spawn(&mut table);
        let key = UserKey { space: 0x1000, offset: 0x1000 };
        block_with(&mut table, &facts, kernel, Request::Wait(0x1000), 0);
        block_with(&mut table, &facts, user, Request::WaitUser(key), 0);
        assert_eq!(state(&table, kernel), State::Blocked);
        assert_eq!(state(&table, user), State::Blocked);
        assert_eq!(table.wake_key(0x1000, usize::MAX), 1);
        assert_eq!(state(&table, kernel), State::Ready);
        assert_eq!(state(&table, user), State::Blocked);
        assert_eq!(table.wake_key(0x1000, usize::MAX), 0);
        // And the other way.
        block_with(&mut table, &facts, kernel, Request::Wait(0x1000), 0);
        assert_eq!(table.wake_user_range(0x1000, 0, 0x1000), 0);
        assert_eq!(table.wake_user_key(key, usize::MAX), 1);
        assert_eq!(state(&table, user), State::Ready);
        assert_eq!(state(&table, kernel), State::Blocked);
        assert_eq!(table.wake_user_range(0x1000, 0, u64::MAX), 0);
        assert_eq!(state(&table, kernel), State::Blocked);
    }

    #[test]
    fn user_key_by_space_and_address() {
        let mut table = table();
        let facts = Facts::default();
        let (first, _) = spawn(&mut table);
        let (second, _) = spawn(&mut table);
        let (third, _) = spawn(&mut table);
        let mine = UserKey { space: 4, offset: 0x2000 };
        // The same address in another space, and another address in the same one.
        let theirs = UserKey { space: 8, offset: 0x2000 };
        let next_word = UserKey { space: 4, offset: 0x2004 };
        block_with(&mut table, &facts, first, Request::WaitUser(mine), 0);
        block_with(&mut table, &facts, second, Request::WaitUser(theirs), 0);
        block_with(&mut table, &facts, third, Request::WaitUserUntil(next_word, 100), 0);
        assert_eq!(table.wake_user_key(UserKey { space: 4, offset: 0x2008 }, usize::MAX), 0);
        assert_eq!(table.wake_user_key(mine, usize::MAX), 1);
        assert_eq!(state(&table, first), State::Ready);
        assert_eq!(state(&table, second), State::Blocked);
        assert_eq!(state(&table, third), State::Blocked);
        // An unmap of the space's range wakes what's in it and nothing else.
        assert_eq!(table.wake_user_range(4, 0x2000, 0x3000), 1);
        assert_eq!(state(&table, third), State::Ready);
        assert_eq!(state(&table, second), State::Blocked);
        assert_eq!(table.wake_user_range(8, 0x3000, 0x4000), 0);
        assert_eq!(table.wake_user_range(8, 0x1000, 0x2001), 1);
        assert_eq!(state(&table, second), State::Ready);
        // A timed user wait ends at its deadline, and not before.
        block_with(&mut table, &facts, first, Request::WaitUserUntil(mine, 50), 10);
        assert_eq!(state(&table, first), State::Blocked);
        table.switch(1, Request::Preempt, 49, &facts);
        assert_eq!(state(&table, first), State::Blocked);
        table.switch(1, Request::Preempt, 50, &facts);
        assert_ne!(state(&table, first), State::Blocked);
    }

    #[test]
    fn deadline_wakes_at_deadline() {
        // A user futex wait with an absolute deadline, as `FUTEX_WAIT2` makes one: the
        // timer is armed for it, and it wakes on the first reschedule at or after it,
        // with no wake on its word.
        let mut table = table();
        let facts = Facts::default();
        let (slot, _) = spawn(&mut table);
        let key = UserKey { space: 6, offset: 0x4000 };
        block_with(&mut table, &facts, slot, Request::WaitUserUntil(key, 1_000), 10);
        assert_eq!(state(&table, slot), State::Blocked);
        assert_eq!(table.tasks()[slot].wait, Wait::UserKeyUntil(key, 1_000));
        // Its CPU's next timer is no later than the deadline, however long the CPU could
        // otherwise idle.
        let home = table.tasks()[slot].home as usize;
        assert_eq!(table.next_deadline(home, 10, 5_000, u64::MAX / 2), 1_000);
        assert_eq!(table.switch(1, Request::Preempt, 999, &facts).next, None);
        assert_eq!(state(&table, slot), State::Blocked);
        assert_eq!(table.switch(1, Request::Preempt, 1_000, &facts).next, Some(slot));
        // Woken by time, it's no longer on the word.
        assert_eq!(table.wake_user_key(key, usize::MAX), 0);
        // A deadline already past never blocks.
        table.switch(1, Request::WaitUserUntil(key, 500), 2_000, &facts);
        assert_ne!(state(&table, slot), State::Blocked);
    }

    #[test]
    fn wake_before_deadline() {
        let mut table = table();
        let facts = Facts::default();
        let (slot, _) = spawn(&mut table);
        let (other, _) = spawn(&mut table);
        let key = UserKey { space: 6, offset: 0x4000 };
        block_with(&mut table, &facts, slot, Request::WaitUserUntil(key, 1_000), 10);
        block_with(&mut table, &facts, other, Request::WaitUserUntil(key, 50_000), 10);
        // One wake takes the first waiter, well before its deadline; the other still waits.
        assert_eq!(table.wake_user_key(key, 1), 1);
        assert_ne!(state(&table, slot), State::Blocked);
        assert_eq!(state(&table, other), State::Blocked);
        // The woken one's deadline went with its wait: passing it wakes nothing more.
        table.switch(1, Request::Preempt, 1_000, &facts);
        assert_eq!(state(&table, other), State::Blocked);
        assert_eq!(table.wake_user_key(key, usize::MAX), 1);
        assert_ne!(state(&table, other), State::Blocked);
    }

    #[test]
    fn a_timed_wait_ends_at_a_wake_or_its_deadline() {
        let mut table = table();
        let facts = Facts::default();
        let (slot, _) = spawn(&mut table);
        table.switch(1, Request::Preempt, 0, &facts);

        table.switch(1, Request::WaitUntil(0xa, 100), 10, &facts);
        assert_eq!(state(&table, slot), State::Blocked);
        assert_eq!(table.switch(1, Request::Preempt, 99, &facts).next, None);
        assert_eq!(table.switch(1, Request::Preempt, 100, &facts).next, Some(slot));

        table.switch(1, Request::WaitUntil(0xa, 500), 200, &facts);
        assert_eq!(table.wake_key(0xa, 1), 1);
        assert_eq!(state(&table, slot), State::Ready);

        // Already past its deadline, so it never blocks.
        table.switch(1, Request::Preempt, 300, &facts);
        table.switch(1, Request::WaitUntil(0xa, 100), 300, &facts);
        assert_ne!(state(&table, slot), State::Blocked);
    }

    #[test]
    fn higher_priority_runs_first_and_equals_take_turns() {
        let mut table = table();
        let facts = Facts::default();
        let (low, low_pid) = spawn(&mut table);
        let (high, high_pid) = spawn(&mut table);
        let (other, other_pid) = spawn(&mut table);
        assert!(table.set_priority(low_pid, 1));
        assert!(table.set_priority(high_pid, 12));
        assert!(table.set_priority(other_pid, 12));
        assert_eq!(table.tasks()[high].priority, 12);

        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(table.switch(0, Request::Preempt, 0, &facts).next.unwrap());
        }
        assert_eq!(seen, [high, other, high, other]);
        assert_ne!(seen[0], low);
    }

    #[test]
    fn a_priority_must_be_in_range_and_the_task_must_exist() {
        let mut table = table();
        let (_, pid) = spawn(&mut table);
        assert!(!table.set_priority(pid, PRIORITY_MAX + 1));
        assert!(table.set_priority(pid, PRIORITY_MAX));
        assert!(!table.set_priority(999, 1));
    }

    #[test]
    fn a_low_priority_task_is_slowed_not_stopped() {
        let mut table = table();
        let facts = Facts::default();
        let (low, low_pid) = spawn(&mut table);
        assert!(table.set_priority(low_pid, 0));

        let mut turns = 0;
        for _ in 0..(STARVATION_LIMIT as usize + 1) * 3 {
            if table.switch(0, Request::Preempt, 0, &facts).next == Some(low) {
                turns += 1;
            }
        }
        assert!(turns >= 2, "it got {turns} turns");
        assert!(turns <= 4, "it got {turns} turns");
    }

    /// A table with this many CPUs online and CPU 0 idle.
    fn smp_table() -> TestTable {
        let mut table = table();
        table.cpu_online();
        table
    }

    #[test]
    fn affinity_keeps_a_task_off_other_cpus() {
        let mut table = smp_table();
        let facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        assert!(table.set_affinity(pid, 1 << 1));
        assert_eq!(table.tasks()[slot].home, 1);

        // CPU 0 keeps its own task even though this one's waiting.
        assert_eq!(table.switch(0, Request::Preempt, 0, &facts).next, Some(0));
        assert_eq!(table.switch(1, Request::Preempt, 0, &facts).next, Some(slot));
        // It may not be stolen back either.
        table.switch(1, Request::Yield, 0, &facts);
        table.switch(0, Request::Exit, 0, &facts);
        assert_ne!(table.switch(0, Request::Preempt, 0, &facts).next, Some(slot));
    }

    #[test]
    fn a_mask_must_name_an_online_cpu() {
        let mut table = table();
        let (_, pid) = spawn(&mut table);
        assert!(!table.set_affinity(pid, 0));
        assert!(!table.set_affinity(pid, 1 << 1), "CPU 1 isn't online");
        assert!(table.set_affinity(pid, 1));
        assert!(!table.set_affinity(999, 1));
    }

    #[test]
    fn a_new_task_goes_to_the_quietest_cpu() {
        let mut table = smp_table();
        // CPU 0 has the boot task, so CPU 1 gets the first new one.
        let (first, _) = spawn(&mut table);
        assert_eq!(table.tasks()[first].home, 1);
        let (second, _) = spawn(&mut table);
        assert_eq!(table.tasks()[second].home, 0);
    }

    #[test]
    fn a_cpu_prefers_its_own_queue() {
        let mut table = smp_table();
        let facts = Facts::default();
        let (first, _) = spawn(&mut table);
        let (second, _) = spawn(&mut table);
        assert_eq!(table.tasks()[first].home, 1);
        assert_eq!(table.tasks()[second].home, 0);

        // CPU 0 alternates between the boot task and its own second task.
        assert_eq!(table.switch(0, Request::Preempt, 0, &facts).next, Some(second));
        assert_eq!(table.switch(1, Request::Preempt, 0, &facts).next, Some(first));
        assert_eq!(table.switch(0, Request::Preempt, 0, &facts).next, Some(0));
        assert_eq!(table.switch(1, Request::Preempt, 0, &facts).next, Some(first));
    }

    #[test]
    fn an_idle_cpu_takes_work_from_a_busy_one_and_keeps_it() {
        let mut table = table();
        let facts = Facts::default();
        let (slot, _) = spawn(&mut table);
        assert_eq!(table.tasks()[slot].home, 0);
        table.cpu_online();
        assert_eq!(table.switch(1, Request::Preempt, 0, &facts).next, Some(slot));
        assert_eq!(table.tasks()[slot].home, 1);
    }

    #[test]
    fn a_longer_queue_elsewhere_is_evened_out() {
        let mut table = table();
        let facts = Facts::default();
        // All on CPU 0, before CPU 1 came up.
        spawn(&mut table);
        spawn(&mut table);
        table.cpu_online();
        table.switch(0, Request::Preempt, 0, &facts);
        let taken = table.switch(1, Request::Preempt, 0, &facts).next.unwrap();
        assert_eq!(table.tasks()[taken].home, 1);

        // Three on CPU 0's queue against CPU 1's one: at its next tick, CPU 1
        // has its own task ready, and still takes another.
        let (extra, _) = spawn(&mut table);
        table.tasks[extra].home = 0;
        let next = table.switch(1, Request::Preempt, 0, &facts).next.unwrap();
        assert_ne!(next, taken);
        assert_eq!(table.tasks()[next].home, 1);
    }

    #[test]
    fn the_timer_is_set_for_the_slice_or_a_sooner_sleeper_at_home() {
        let mut table = smp_table();
        let facts = Facts::default();
        let (slot, _) = spawn(&mut table);
        let home = table.tasks()[slot].home as usize;
        assert_eq!(table.switch(home, Request::Preempt, 0, &facts).next, Some(slot));

        assert_eq!(table.next_deadline(home, 100, 50, 1000), 150);
        table.switch(home, Request::Sleep(120), 100, &facts);
        // Idle now, with the sleeper's deadline first.
        assert_eq!(table.next_deadline(home, 100, 50, 1000), 120);
        // It's nobody else's to wake.
        assert_eq!(table.next_deadline(1 - home, 100, 50, 1000), 150);
        table.switch(home, Request::Preempt, 120, &facts);
        // Awake and running, so there's a slice to end.
        assert_eq!(table.next_deadline(home, 120, 50, 1000), 170);
    }

    #[test]
    fn an_idle_cpu_can_wait_as_long_as_it_likes() {
        let table = smp_table();
        assert_eq!(table.next_deadline(1, 100, 50, 1000), 1100);
    }

    #[test]
    fn elapsed_time_is_accounted_in_whole_ticks() {
        let mut table = smp_table();
        // The first call only sets the starting point.
        table.account_elapsed(0, 1000, 100, false);
        assert_eq!(table.total_ticks, 0);

        table.account_elapsed(0, 1250, 100, false);
        assert_eq!(table.cpu_total_ticks[0], 2);
        assert_eq!(table.tasks()[0].cpu_ticks, 2);
        // The rest carries over to the next call.
        table.account_elapsed(0, 1300, 100, false);
        assert_eq!(table.cpu_total_ticks[0], 3);

        // Time with nothing to do is idle, in one go, however long it was.
        table.account_elapsed(1, 1000, 100, false);
        table.account_elapsed(1, 11_000, 100, false);
        assert_eq!(table.cpu_idle_ticks[1], 100);
        assert_eq!(table.total_ticks, 103);
    }

    #[test]
    fn idle_ticks_are_not_charged_to_a_task() {
        let mut table = table();
        table.account_tick(0, false);
        assert_eq!(table.tasks()[0].cpu_ticks, 1);
        assert_eq!(table.idle_ticks, 0);
        table.account_tick(0, true);
        assert_eq!(table.tasks()[0].cpu_ticks, 1);
        assert_eq!(table.idle_ticks, 1);
        table.account_tick(1, false);
        assert_eq!(table.cpu_idle_ticks, [1, 1]);
        assert_eq!(table.cpu_total_ticks, [2, 1]);
        assert_eq!(table.total_ticks, 3);
    }

    #[test]
    fn a_stopped_task_is_not_chosen_until_resumed() {
        let facts = Facts::default();
        let mut table = TestTable::zeroed();
        table.init();
        let (slot, pid) = table.reserve().unwrap();
        table.commit(slot);
        assert!(table.set_stopped(pid, true));
        assert!(!table.set_stopped(1, true));

        let switch = table.switch(0, Request::Yield, 0, &facts);
        assert_eq!(switch.next, Some(0));
        assert!(table.set_stopped(pid, false));
        let switch = table.switch(0, Request::Yield, 0, &facts);
        assert_eq!(switch.next, Some(slot));
    }

    #[test]
    fn a_stopped_task_can_still_be_killed() {
        let mut facts = Facts::default();
        let mut table = TestTable::zeroed();
        table.init();
        let (slot, pid) = table.reserve().unwrap();
        table.commit(slot);
        table.set_stopped(pid, true);
        facts.depth[slot] = 1;
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Pending);
        // It holds a lock, so it has to run to release it.
        let switch = table.switch(0, Request::Yield, 0, &facts);
        assert_eq!(switch.next, Some(slot));
    }

    #[test]
    fn user_slot_in_call_not_reapable() {
        // Its call holds what a reap would abandon, so it has to unwind instead,
        // however long that takes and whatever heap it has.
        let mut table = table();
        let mut facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        facts.calls[slot] = 1;
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Pending);
        assert_eq!(state(&table, slot), State::Ready);
        assert_eq!(table.switch(1, Request::Preempt, 1, &facts).next, Some(slot));
        let switch = table.switch(1, Request::Preempt, GRACE * 10, &facts);
        assert!(!switch.reaped);
        assert_eq!(switch.next, Some(slot));

        // Blocked in its call, a kill wakes it rather than reaping it.
        let mut table = self::table();
        let mut facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        facts.calls[slot] = 1;
        assert_eq!(table.switch(1, Request::Preempt, 0, &facts).next, Some(slot));
        table.switch(1, Request::Wait(0x10), 0, &facts);
        assert_eq!(state(&table, slot), State::Blocked);
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Pending);
        assert_eq!(state(&table, slot), State::Ready);
        assert_eq!(table.leaked_bytes, 0);
    }

    #[test]
    fn user_slot_in_user_reapable() {
        // Back in its program, it holds nothing of the kernel's, so it goes at once.
        let mut table = table();
        let mut facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        facts.calls[slot] = 1;
        assert_eq!(table.switch(1, Request::Preempt, 0, &facts).next, Some(slot));
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Pending);
        assert!(!table.switch(1, Request::Preempt, 1, &facts).reaped);

        facts.calls[slot] = 0;
        let switch = table.switch(1, Request::Yield, 2, &facts);
        assert!(switch.reaped);
        assert_eq!(state(&table, slot), State::Free);

        // And one that's off the CPU, in its program, dies on the spot.
        let (ready, ready_pid) = spawn(&mut table);
        assert_eq!(table.kill(ready_pid, 3, GRACE, &facts), KillOutcome::Killed);
        assert_eq!(state(&table, ready), State::Free);
    }

    #[test]
    fn kernel_app_reaping_unchanged() {
        // No call count: reaped when it holds no lock and no heap, or past the grace.
        let mut table = table();
        let mut facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        facts.heap[slot] = 64;
        table.switch(1, Request::Preempt, 0, &facts);
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Pending);
        assert!(!table.switch(1, Request::Preempt, GRACE - 1, &facts).reaped);
        let switch = table.switch(1, Request::Preempt, GRACE, &facts);
        assert!(switch.reaped);
        assert_eq!(table.leaked_bytes, 64);

        let (locked, locked_pid) = spawn(&mut table);
        facts.depth[locked] = 1;
        facts.heap[locked] = 0;
        assert_eq!(table.kill(locked_pid, 0, GRACE, &facts), KillOutcome::Pending);
        assert_eq!(state(&table, locked), State::Ready);
        facts.depth[locked] = 0;
        table.switch(1, Request::Preempt, 1, &facts);
        assert_eq!(state(&table, locked), State::Free);
    }

    #[test]
    fn user_stop_deferred_in_call() {
        let mut table = table();
        let mut facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        facts.calls[slot] = 1;
        assert!(table.set_stopped(pid, true));
        assert!(table.stop_or_kill_pending(slot));
        // Still in its call, so it keeps being chosen until it's done with it.
        assert_eq!(table.switch(0, Request::Yield, 0, &facts).next, Some(slot));
        assert_eq!(table.switch(0, Request::Yield, 0, &facts).next, Some(0));
        // Out of the call, at the boundary: now it's held.
        facts.calls[slot] = 0;
        for _ in 0..4 {
            assert_eq!(table.switch(0, Request::Yield, 0, &facts).next, Some(0));
        }
        assert!(table.set_stopped(pid, false));
        assert!(!table.stop_or_kill_pending(slot));
        assert_eq!(table.switch(0, Request::Yield, 0, &facts).next, Some(slot));
    }

    #[test]
    fn kernel_app_stop_immediate() {
        let mut table = table();
        let facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        assert_eq!(table.switch(1, Request::Preempt, 0, &facts).next, Some(slot));
        assert!(table.set_stopped(pid, true));
        // Off its CPU at the next tick, and not chosen again.
        let switch = table.switch(1, Request::Preempt, 0, &facts);
        assert_eq!(switch.save, Some(slot));
        assert_eq!(switch.next, None);
        assert_eq!(table.switch(0, Request::Yield, 0, &facts).next, Some(0));
    }

    #[test]
    fn a_protected_task_cannot_be_killed_or_stopped() {
        let mut table = table();
        let facts = Facts::default();
        let (slot, pid) = spawn(&mut table);
        assert!(table.set_protected(pid));
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Protected);
        assert!(!table.set_stopped(pid, true));
        assert_eq!(state(&table, slot), State::Ready);
        assert!(!table.set_protected(999));
    }

    /// A second thread of the process in `of`, made runnable.
    fn thread_of(table: &mut TestTable, of: usize) -> (usize, u32) {
        let (slot, tid) = table.reserve_thread(of, 0).expect("free slot");
        table.commit(slot);
        (slot, tid)
    }

    #[test]
    fn tasks_share_tgid() {
        let mut table = table();
        let (leader, pid) = spawn(&mut table);
        let (thread, tid) = thread_of(&mut table, leader);
        assert_ne!(tid, pid);
        assert_eq!(table.tasks()[leader].tgid, pid);
        assert_eq!(table.tasks()[thread].tgid, pid);
        assert_eq!(table.threads(pid), 2);
        // A thread's id is no process's: nothing finds a process by it.
        assert_eq!(table.find_pid(tid), None);
        assert_eq!(table.find_tid(tid), Some(thread));
        assert_eq!(table.find_pid(pid), Some(leader));
        // The next process gets an id no thread has.
        let (_, next) = spawn(&mut table);
        assert!(next != pid && next != tid);
    }

    #[test]
    fn group_members_walks_each_process_once() {
        let mut table = table();
        let (leader, pid) = spawn(&mut table);
        let (first, _) = thread_of(&mut table, leader);
        let (second, _) = thread_of(&mut table, first);
        let mut members: Vec<usize> = table.group_members(first).collect();
        assert_eq!(members[0], first, "the slot asked about comes first");
        members.sort_unstable();
        assert_eq!(members, [leader, first, second]);
        assert!(members.iter().all(|&slot| table.tasks()[slot].tgid == pid));
        // The shell is a process of one thread.
        assert_eq!(table.group_members(0).collect::<Vec<_>>(), [0]);
    }

    #[test]
    fn kill_tgid_flags_every_thread() {
        let mut table = table();
        table.cpu_online();
        let mut facts = Facts::default();
        let (leader, pid) = spawn(&mut table);
        let (thread, _) = thread_of(&mut table, leader);
        // One in a call, which can't be reaped there; the other at its program.
        facts.calls[thread] = 1;
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Pending);
        assert_eq!(state(&table, leader), State::Free);
        assert!(table.tasks()[thread].kill_pending);
        assert!(table.stop_or_kill_pending(thread));
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::AlreadyPending);
        // A killed process starts no threads.
        assert_eq!(table.reserve_thread(thread, 0), None);
        facts.calls[thread] = 0;
        table.switch(1, Request::Preempt, 1, &facts);
        assert_eq!(state(&table, thread), State::Free);
        assert_eq!(table.kill(pid, 2, GRACE, &facts), KillOutcome::NotFound);

        // Stopping is the whole process too, and so are priority and CPUs.
        let (leader, pid) = spawn(&mut table);
        let (thread, _) = thread_of(&mut table, leader);
        assert!(table.set_stopped(pid, true));
        assert!(table.tasks()[leader].stopped && table.tasks()[thread].stopped);
        assert!(table.set_priority(pid, 3));
        assert!(table.set_affinity(pid, 2));
        assert_eq!((table.tasks()[thread].priority, table.tasks()[thread].affinity), (3, 2));
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Killed);
        assert_eq!(table.threads(pid), 0);

        // A thread started by one that's stopped in its call starts stopped, and isn't
        // run until the process is resumed.
        let mut table = super::tests::table();
        table.cpu_online();
        let mut facts = Facts::default();
        let (leader, pid) = spawn(&mut table);
        assert!(table.set_stopped(pid, true));
        facts.calls[leader] = 1;
        let (thread, _) = thread_of(&mut table, leader);
        assert!(table.tasks()[thread].stopped);
        facts.calls[leader] = 0;
        for cpu in 0..CPUS {
            assert_ne!(table.switch(cpu, Request::Yield, 0, &facts).next, Some(thread));
        }
        assert!(table.set_stopped(pid, false));
        assert!(!table.tasks()[thread].stopped);
        let chosen: Vec<_> = (0..CPUS).map(|cpu| table.switch(cpu, Request::Yield, 0, &facts).next).collect();
        assert!(chosen.contains(&Some(thread)));
    }

    #[test]
    fn find_pid_survives_leader_exit() {
        let mut table = table();
        table.cpu_online();
        let facts = Facts::default();
        let (leader, pid) = spawn(&mut table);
        let (thread, _) = thread_of(&mut table, leader);
        assert_eq!(table.switch(1, Request::Preempt, 0, &facts).next, Some(leader));
        table.switch(1, Request::Exit, 0, &facts);
        assert_eq!(state(&table, leader), State::Free);
        assert_eq!(table.find_pid(pid), Some(thread));
        assert_eq!(table.threads(pid), 1);
        // The process can still be killed, through the thread that's left: running on
        // CPU 1 now, so at its next switch.
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Pending);
        table.switch(1, Request::Preempt, 1, &facts);
        assert_eq!(table.find_pid(pid), None);
    }

    #[test]
    fn exit_record_once_at_last_thread() {
        let mut table = table();
        table.cpu_online();
        let facts = Facts::default();
        let (leader, pid) = spawn(&mut table);
        let (thread, _) = thread_of(&mut table, leader);
        // Both gone before either is collected: only the second collected is the last.
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Killed);
        assert!(!table.last_of_group(leader));
        assert!(!table.last_of_group(thread));
        table.forget_left(leader);
        assert!(table.last_of_group(thread));
        table.forget_left(thread);

        // One gone while the other runs on: not the last, whatever was collected.
        let (leader, _) = spawn(&mut table);
        let (thread, _) = thread_of(&mut table, leader);
        assert_eq!(table.switch(1, Request::Preempt, 0, &facts).next, Some(leader));
        table.switch(1, Request::Exit, 0, &facts);
        assert!(!table.last_of_group(leader));
        table.forget_left(leader);
        assert_eq!(table.switch(1, Request::Preempt, 0, &facts).next, Some(thread));
        table.switch(1, Request::Exit, 0, &facts);
        assert!(table.last_of_group(thread));
        table.forget_left(thread);
        // A process of one thread is its own last.
        let (single, _) = spawn(&mut table);
        assert_eq!(table.kill(table.tasks()[single].pid, 0, GRACE, &facts), KillOutcome::Killed);
        assert!(table.last_of_group(single));
        table.forget_left(single);

        // A thread whose start was cancelled was never one of them.
        let (leader, pid) = spawn(&mut table);
        let (cancelled, _) = table.reserve_thread(leader, 0).unwrap();
        table.cancel(cancelled);
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Killed);
        assert!(table.last_of_group(leader));
        table.forget_left(leader);
        // Nor is a slot that left and is taken again before it was forgotten: its new
        // process is its own.
        let (leader, pid) = spawn(&mut table);
        let (thread, _) = thread_of(&mut table, leader);
        assert_eq!(table.kill(pid, 0, GRACE, &facts), KillOutcome::Killed);
        let (again, _) = table.reserve().unwrap();
        assert_eq!(again, leader);
        assert!(table.last_of_group(thread));
    }

    #[test]
    fn last_of_group_after_1024_threads() {
        // One process of every slot but the boot task's, killed at once: each exit is
        // collected in one look, and only the last says so.
        let mut table = Box::new(Table::<1024, 1>::zeroed());
        table.init();
        let facts = ManyFacts::new(1024);
        let (leader, pid) = table.reserve().unwrap();
        table.commit(leader);
        while let Some((slot, _)) = table.reserve_thread(leader, 0) {
            table.commit(slot);
        }
        assert_eq!(table.threads(pid), 1023);
        assert_eq!(table.kill(pid, 0, 0, &facts), KillOutcome::Killed);
        let left: Vec<usize> = table.left().iter().collect();
        assert_eq!(left.len(), 1023);
        for (index, &slot) in left.iter().enumerate() {
            assert_eq!(table.last_of_group(slot), index == left.len() - 1);
            table.forget_left(slot);
        }
    }

    #[test]
    fn heap_charged_by_slot() {
        use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
        // The kernel's per-slot ids, as it publishes them: each slot's thread id.
        fn ids(table: &TestTable) -> [AtomicU32; TASKS] {
            core::array::from_fn(|slot| {
                let task = table.tasks()[slot];
                AtomicU32::new(if task.state == State::Free { 0 } else { task.pid })
            })
        }
        let mut table = table();
        let (leader, pid) = spawn(&mut table);
        let (first, one) = thread_of(&mut table, leader);
        let (second, two) = thread_of(&mut table, leader);
        let bytes: [AtomicUsize; TASKS] = core::array::from_fn(|_| AtomicUsize::new(0));
        let published = ids(&table);
        for (owner, delta) in [(pid, 100), (one, 20), (two, 3), (one, 20), (two, -1)] {
            charge_heap(&published, &bytes, owner, delta);
        }
        let held = |slot: usize| bytes[slot].load(Ordering::Relaxed);
        // Per thread, never pooled on the process's pid.
        assert_eq!((held(leader), held(first), held(second)), (100, 40, 2));
        // A free never goes below nothing, and 0 is nobody's.
        charge_heap(&published, &bytes, two, -10);
        charge_heap(&published, &bytes, 0, 7);
        assert_eq!(held(second), 0);
        assert_eq!((0..TASKS).map(held).sum::<usize>(), 140);
        // The leader gone, its id names nothing to charge, whatever the process does.
        let facts = Facts::default();
        assert_eq!(table.kill_slot(leader, 0, GRACE, &facts), KillOutcome::Killed);
        let published = ids(&table);
        charge_heap(&published, &bytes, pid, 50);
        assert_eq!((0..TASKS).map(held).sum::<usize>(), 140);
        charge_heap(&published, &bytes, one, -40);
        assert_eq!(held(first), 0);
    }

    #[test]
    fn wake_key_unaffected_by_threads() {
        let mut table = table();
        table.cpu_online();
        let facts = Facts::default();
        let (leader, _) = spawn(&mut table);
        let (thread, _) = thread_of(&mut table, leader);
        for _ in 0..2 {
            table.switch(1, Request::Preempt, 0, &facts);
            table.switch(1, Request::Wait(0x1000), 0, &facts);
        }
        assert_eq!(state(&table, leader), State::Blocked);
        assert_eq!(state(&table, thread), State::Blocked);
        // One wake is one thread, whichever process they're in.
        assert_eq!(table.wake_key(0x1000, 1), 1);
        assert_eq!(table.wake_key(0x1000, usize::MAX), 1);
        assert_eq!(table.wake_key(0x1000, usize::MAX), 0);
    }

    #[test]
    fn user_reservations_keep_slots_for_the_kernel() {
        let mut table = table();
        // Three free slots: keeping two, one can be had; then none.
        assert!(table.reserve_keeping(2).is_some());
        assert_eq!(table.reserve_keeping(2), None);
        let (leader, _) = spawn(&mut table);
        assert_eq!(table.reserve_thread(leader, 1), None);
        assert!(table.reserve_thread(leader, 0).is_some());
    }

    #[test]
    fn heap_charge_hint_is_only_a_hint() {
        use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
        let ids: [AtomicU32; 4] = core::array::from_fn(|slot| AtomicU32::new(slot as u32 * 10));
        let bytes: [AtomicUsize; 4] = core::array::from_fn(|_| AtomicUsize::new(0));
        charge_heap_hinted(&ids, &bytes, 20, 5, Some(2));
        // A wrong hint, or one past the table, still finds the owner.
        charge_heap_hinted(&ids, &bytes, 30, 7, Some(1));
        charge_heap_hinted(&ids, &bytes, 30, 1, Some(99));
        assert_eq!(bytes[2].load(Ordering::Relaxed), 5);
        assert_eq!(bytes[3].load(Ordering::Relaxed), 8);
        assert_eq!(bytes[1].load(Ordering::Relaxed), 0);
    }

    /// Facts for tables of any size.
    struct ManyFacts {
        depth: Vec<u32>,
        heap: Vec<usize>,
        calls: Vec<u32>,
    }

    impl ManyFacts {
        fn new(slots: usize) -> Self {
            Self { depth: vec![0; slots], heap: vec![0; slots], calls: vec![0; slots] }
        }
    }

    impl TaskFacts for ManyFacts {
        fn lock_depth(&self, slot: usize) -> u32 {
            self.depth[slot]
        }

        fn heap_bytes(&self, slot: usize) -> usize {
            self.heap[slot]
        }

        fn in_call(&self, slot: usize) -> u32 {
            self.calls[slot]
        }
    }

    /// xorshift64*: enough randomness for traces, and the same every run.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, limit: usize) -> usize {
            (self.next() % limit as u64) as usize
        }
    }

    /// What the traces mostly do.
    #[derive(Clone, Copy, PartialEq)]
    enum Mix {
        Everything,
        Futexes,
        Exits,
    }

    /// Two tables put through the same random trace: one deciding by its bitmaps, the
    /// other by scanning every slot as the table did before them. After every step they
    /// must agree on everything.
    struct Twin<const N: usize, const C: usize> {
        fast: Box<Table<N, C>>,
        slow: Box<Table<N, C>>,
        facts: ManyFacts,
        rng: Rng,
        now: u64,
        /// The kernel's `live`: a slot committed whose exit hasn't been collected.
        live: Vec<bool>,
    }

    impl<const N: usize, const C: usize> Twin<N, C> {
        fn new(seed: u64) -> Self {
            let mut fast = Box::new(Table::<N, C>::zeroed());
            let mut slow = Box::new(Table::<N, C>::zeroed());
            slow.scan = true;
            for table in [&mut fast, &mut slow] {
                table.init();
                for _ in 1..C {
                    table.cpu_online();
                }
            }
            let mut live = vec![false; N];
            live[0] = true;
            Self { fast, slow, facts: ManyFacts::new(N), rng: Rng(seed | 1), now: 1, live }
        }

        fn key(&mut self) -> usize {
            0x1000 + 4 * self.rng.below(150)
        }

        fn user_key(&mut self) -> UserKey {
            UserKey { space: 1 + self.rng.below(3), offset: 4 * self.rng.below(60) as u64 }
        }

        fn live_slot(&mut self) -> Option<usize> {
            let live: Vec<usize> = (1..N).filter(|&slot| self.fast.tasks()[slot].state != State::Free).collect();
            (!live.is_empty()).then(|| live[self.rng.below(live.len())])
        }

        /// Does the same to both, and checks they answered alike.
        fn both<R: PartialEq + core::fmt::Debug>(&mut self, mut step: impl FnMut(&mut Table<N, C>, &ManyFacts) -> R) -> R {
            let fast = step(&mut self.fast, &self.facts);
            let slow = step(&mut self.slow, &self.facts);
            assert_eq!(fast, slow, "the bitmaps and the scan answered differently");
            fast
        }

        fn step(&mut self, mix: Mix) {
            let now = self.now;
            let roll = self.rng.below(100);
            let roll = match mix {
                Mix::Everything => roll,
                // Mostly waits and wakes on a few keys, so the buckets collide.
                Mix::Futexes => [40, 41, 42, 43, 44, 45, 46, 47, 48, 49][roll % 10],
                // Mostly starting and ending.
                Mix::Exits => [0, 1, 2, 20, 21, 22, 23, 30, 31, 32][roll % 10],
            };
            match roll {
                0..=9 => {
                    let priority = self.rng.below(LEVELS) as u8;
                    let affinity = if self.rng.below(4) == 0 { 1 + self.rng.below((1 << C) - 1) as u32 } else { 0 };
                    if let Some((slot, pid)) = self.both(|table, _| table.reserve()) {
                        self.both(|table, _| {
                            table.commit(slot);
                            (table.set_priority(pid, priority), affinity == 0 || table.set_affinity(pid, affinity))
                        });
                        self.live[slot] = true;
                    }
                }
                10..=14 => {
                    if let Some(creator) = self.live_slot() {
                        if let Some((slot, _)) = self.both(|table, _| table.reserve_thread(creator, 0)) {
                            self.both(|table, _| table.commit(slot));
                            self.live[slot] = true;
                        }
                    }
                }
                15..=19 => {
                    let slot = self.rng.below(N);
                    match self.rng.below(3) {
                        0 => self.facts.depth[slot] = self.rng.below(2) as u32,
                        1 => self.facts.heap[slot] = self.rng.below(2) * 64,
                        _ => self.facts.calls[slot] = self.rng.below(2) as u32,
                    }
                }
                20..=39 => {
                    let cpu = self.rng.below(C);
                    let deadline = now + self.rng.below(50) as u64;
                    let (key, user) = (self.key(), self.user_key());
                    let request = match self.rng.below(10) {
                        0 => Request::Yield,
                        1 => Request::Sleep(deadline),
                        2 => Request::Wait(key),
                        3 => Request::WaitUntil(key, deadline),
                        4 => Request::WaitUser(user),
                        5 => Request::WaitUserUntil(user, deadline),
                        6 if mix != Mix::Futexes => Request::Exit,
                        _ => Request::Preempt,
                    };
                    // Slot 0 is the boot task, which never exits.
                    let request = if request == Request::Exit && self.fast.current(cpu).is_none_or(|slot| slot == 0) {
                        Request::Preempt
                    } else {
                        request
                    };
                    self.both(|table, facts| table.switch(cpu, request, now, facts));
                }
                40..=49 => {
                    let (key, user) = (self.key(), self.user_key());
                    let count = [1, 2, usize::MAX][self.rng.below(3)];
                    let low = 4 * self.rng.below(60) as u64;
                    let high = low + 4 * self.rng.below(20) as u64;
                    match self.rng.below(3) {
                        0 => self.both(|table, _| table.wake_key(key, count)),
                        1 => self.both(|table, _| table.wake_user_key(user, count)),
                        _ => self.both(|table, _| table.wake_user_range(user.space, low, high)),
                    };
                }
                50..=54 => {
                    if let Some(slot) = self.live_slot() {
                        let pid = self.fast.tasks()[slot].tgid;
                        self.both(|table, facts| table.kill(pid, now, 20, facts));
                    }
                }
                55..=59 => {
                    if let Some(slot) = self.live_slot() {
                        let pid = self.fast.tasks()[slot].tgid;
                        let stop = self.rng.below(2) == 0;
                        self.both(|table, _| table.set_stopped(pid, stop));
                    }
                }
                60..=64 => {
                    if let Some(slot) = self.live_slot() {
                        let pid = self.fast.tasks()[slot].tgid;
                        let priority = self.rng.below(LEVELS + 1) as u8;
                        self.both(|table, _| (table.set_priority(pid, priority), table.wake_pid(pid)));
                    }
                }
                _ => self.now += 1 + self.rng.below(5) as u64,
            }
            self.check();
        }

        fn check(&mut self) {
            assert_eq!(self.fast.tasks(), self.slow.tasks());
            assert_eq!(self.fast.current, self.slow.current);
            assert_eq!(self.fast.leaked_bytes, self.slow.leaked_bytes);
            for cpu in 0..C {
                assert_eq!(
                    self.fast.next_deadline(cpu, self.now, 10, 1000),
                    self.slow.next_deadline(cpu, self.now, 10, 1000)
                );
            }
            // The exits the kernel would collect: by the bitmap, and by looking at every slot.
            let scanned: Vec<usize> =
                (1..N).filter(|&slot| self.live[slot] && self.fast.tasks()[slot].state == State::Free).collect();
            let left = self.fast.left();
            let listed: Vec<usize> =
                left.iter().filter(|&slot| self.live[slot] && self.fast.tasks()[slot].state == State::Free).collect();
            assert_eq!(listed, scanned, "the exit bitmap missed an exit");
            // Whether each is its process's last: by the rings, and by a scan.
            for slot in left.iter() {
                assert_eq!(self.fast.last_of_group(slot), self.slow.last_of_group(slot), "slot {slot}'s ring is wrong");
            }
            if self.rng.below(3) == 0 {
                for slot in left.iter() {
                    self.live[slot] = false;
                    self.fast.forget_left(slot);
                    self.slow.forget_left(slot);
                }
            }
        }
    }

    #[test]
    fn bitmap_pick_matches_scan() {
        for seed in 1..=40 {
            let mut twin = Twin::<130, 4>::new(seed * 7919);
            for _ in 0..3000 {
                twin.step(Mix::Everything);
            }
        }
        // One CPU, and a table that's one word.
        for seed in 1..=20 {
            let mut twin = Twin::<64, 1>::new(seed * 104_729);
            for _ in 0..3000 {
                twin.step(Mix::Everything);
            }
        }
    }

    #[test]
    fn exit_bitmap_matches_scan() {
        for seed in 1..=40 {
            let mut twin = Twin::<200, 2>::new(seed * 31);
            for _ in 0..3000 {
                twin.step(Mix::Exits);
            }
        }
    }

    #[test]
    fn futex_buckets_match_scan() {
        for seed in 1..=40 {
            let mut twin = Twin::<150, 4>::new(seed * 65_537);
            // Fill the table first, so there are many waiters to wake.
            for _ in 0..200 {
                twin.step(Mix::Exits);
            }
            for _ in 0..3000 {
                twin.step(Mix::Futexes);
            }
        }
    }

    #[test]
    fn table_of_1024() {
        let mut table = Box::new(Table::<1024, 8>::zeroed());
        table.init();
        let mut seen = vec![false; 1024];
        let mut pids = std::collections::HashSet::new();
        while let Some((slot, pid)) = table.reserve() {
            assert!(!seen[slot] && slot != 0 && pids.insert(pid));
            seen[slot] = true;
            table.commit(slot);
        }
        assert_eq!(seen.iter().filter(|&&taken| taken).count(), 1023);
        // Equals take turns: every slot runs, each once, before any runs again.
        let facts = ManyFacts::new(1024);
        let mut ran = vec![0; 1024];
        for _ in 0..1024 {
            let slot = table.switch(0, Request::Preempt, 0, &facts).next.expect("something runs");
            ran[slot] += 1;
            assert_eq!(table.current(0), Some(slot));
        }
        assert!(ran.iter().all(|&turns| turns == 1), "a slot was skipped or ran twice");
        assert_eq!(table.left().len(), 0);
        // And they all go.
        for slot in 1..1024 {
            let pid = table.tasks()[slot].pid;
            assert_ne!(table.kill(pid, 0, 0, &facts), KillOutcome::NotFound);
        }
        table.switch(0, Request::Preempt, 1, &facts);
        assert_eq!(table.left().len(), 1023);
        assert_eq!(table.tasks().iter().filter(|task| task.state != State::Free).count(), 1);
    }

    #[test]
    fn current_beyond_255() {
        let mut table = Box::new(Table::<1024, 2>::zeroed());
        table.init();
        table.cpu_online();
        let facts = ManyFacts::new(1024);
        let mut last = 0;
        for _ in 0..300 {
            last = table.reserve().expect("free slot").0;
        }
        assert_eq!(last, 300);
        // Only the last is runnable: slot 300, which a byte would have made 44.
        table.commit(last);
        let switch = table.switch(1, Request::Preempt, 0, &facts);
        assert_eq!(switch.next, Some(300));
        assert_eq!(table.current(1), Some(300));
        assert_eq!(table.current(0), Some(0));
        let switch = table.switch(1, Request::Exit, 0, &facts);
        assert_eq!((switch.save, switch.next), (None, None));
        assert_eq!(table.current(1), None);
        assert!(table.left().contains(300));
    }

    #[test]
    fn kernel_reserve_kept() {
        let mut table = Box::new(Table::<1024, 1>::zeroed());
        table.init();
        let limits = UserLimits::keeping(64);
        let mut users = 0;
        while let Some((slot, _)) = table.reserve_user(0, &limits) {
            table.commit(slot);
            users += 1;
        }
        // 960 tasks in all, the boot task among them, and 64 slots left.
        assert_eq!(users, 959);
        let leader = (1..1024).find(|&slot| table.tasks()[slot].state != State::Free).unwrap();
        assert_eq!(table.reserve_user_thread(leader, &limits), None);
        // The kernel's own work still gets every one of those.
        for _ in 0..64 {
            assert!(table.reserve().is_some());
        }
        assert_eq!(table.reserve(), None);
    }

    #[test]
    fn per_uid_caps() {
        let mut table = table_of::<64>();
        let limits = UserLimits { keep: 4, tasks_per_uid: 10, processes_per_uid: 3 };
        let mut leaders = Vec::new();
        for _ in 0..3 {
            let (slot, _) = table.reserve_user(1000, &limits).expect("under the caps");
            table.commit(slot);
            leaders.push(slot);
        }
        // A fourth process is one too many for uid 1000, but not for anyone else's.
        assert_eq!(table.reserve_user(1000, &limits), None);
        assert!(table.reserve_user(1001, &limits).is_some());
        // Threads count against the task cap, not the process cap.
        for _ in 0..7 {
            let (slot, _) = table.reserve_user_thread(leaders[0], &limits).expect("under the task cap");
            assert_eq!(table.tasks()[slot].uid, 1000);
            table.commit(slot);
        }
        assert_eq!(table.uid_tasks(1000), 10);
        assert_eq!(table.uid_processes(1000), 3);
        assert_eq!(table.reserve_user_thread(leaders[1], &limits), None);
        // Root has no caps but the slots kept for the kernel.
        let root = table.reserve_user(0, &limits).expect("root").0;
        table.commit(root);
        assert!(table.reserve_user_thread(root, &limits).is_some());
        // A process that becomes someone else takes its threads with it.
        let pid = table.tasks()[leaders[0]].tgid;
        table.set_uid(pid, 7);
        assert_eq!((table.uid_tasks(1000), table.uid_processes(1000)), (2, 2));
        assert_eq!(table.uid_tasks(7), 8);
        assert!(table.reserve_user(1000, &limits).is_some());
    }

    fn table_of<const N: usize>() -> Box<Table<N, 1>> {
        let mut table = Box::new(Table::<N, 1>::zeroed());
        table.init();
        table
    }
}
