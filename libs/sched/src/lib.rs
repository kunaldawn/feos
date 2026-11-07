//! Scheduler policy: the task state machine, run-queue choice and kill rules.
//!
//! The kernel owns register frames, stacks and locks; this crate owns every
//! decision about them, so the decisions can be tested on the host. All state
//! lives in a [`Table`] the kernel keeps behind its scheduler lock. A zeroed
//! table is valid, so the kernel's copy lives in `.bss`.
//!
//! Sleeping tasks are the table's [`Blocked`](State::Blocked) entries with a
//! deadline. With a fixed, small table, scanning them on each tick is cheaper
//! and simpler than keeping a sorted queue.
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

#![cfg_attr(not(test), no_std)]

/// The priority of a task nobody has said anything about.
pub const PRIORITY_NORMAL: u8 = 8;
/// The highest priority. Larger numbers are more urgent.
pub const PRIORITY_MAX: u8 = 15;
/// How many times a ready task can be passed over before it gets a turn anyway.
const STARVATION_LIMIT: u16 = 64;

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
    /// The current task has finished.
    Exit,
}

/// Facts about a task that only the kernel can measure.
pub trait TaskFacts {
    /// Spin locks and mutexes the task in `slot` holds.
    fn lock_depth(&self, slot: usize) -> u32;
    /// Live heap bytes owned by the task in `slot`.
    fn heap_bytes(&self, slot: usize) -> usize;
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

/// One process table entry, as far as policy is concerned.
#[derive(Clone, Copy)]
pub struct Task {
    /// Lifecycle state.
    pub state: State,
    /// Process identifier.
    pub pid: u32,
    /// What a blocked task is waiting for.
    pub wait: Wait,
    /// A kill is waiting for this task's next safe point.
    pub kill_pending: bool,
    /// After this counter value, a task with live heap is reaped regardless.
    pub kill_deadline: u64,
    /// Scheduler ticks consumed by this task across all CPUs.
    pub cpu_ticks: u64,
    /// Held off the CPU by job control until it is resumed. Only a pending kill
    /// overrides this, because a task must run to reach a safe point.
    pub stopped: bool,
    /// How urgent the task is, up to [`PRIORITY_MAX`].
    pub priority: u8,
    /// The CPU whose queue the task is on.
    pub home: u8,
    /// The CPUs this task may run on, as a bit each. Zero means any.
    pub affinity: u32,
    /// Times this task was ready and something else was chosen.
    passed_over: u16,
}

/// The process table and accounting.
pub struct Table<const TASKS: usize, const CPUS: usize> {
    tasks: [Task; TASKS],
    /// Slot running on each CPU plus one, or zero while that CPU is idle.
    current: [u8; CPUS],
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
}

impl<const TASKS: usize, const CPUS: usize> Table<TASKS, CPUS> {
    /// An all-zero table: every slot free and every CPU idle.
    pub const fn zeroed() -> Self {
        const {
            assert!(TASKS >= 1 && TASKS <= u8::MAX as usize);
            assert!(CPUS >= 1);
        }
        const TASK: Task = Task {
            state: State::Free,
            pid: 0,
            wait: Wait::Nothing,
            kill_pending: false,
            kill_deadline: 0,
            cpu_ticks: 0,
            stopped: false,
            priority: 0,
            home: 0,
            affinity: 0,
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
        }
    }

    /// Creates the boot task as slot 0, PID 1, running on CPU 0.
    pub fn init(&mut self) {
        self.tasks[0].state = State::Running;
        self.tasks[0].pid = 1;
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

    /// Finds the live slot with `pid`.
    pub fn find_pid(&self, pid: u32) -> Option<usize> {
        self.tasks
            .iter()
            .position(|task| task.state != State::Free && task.pid == pid)
    }

    /// Returns a CPU in `0..online` other than `except` that is idle.
    pub fn idle_cpu(&self, except: usize) -> Option<usize> {
        (0..self.online).find(|&cpu| cpu != except && self.current[cpu] == 0)
    }

    /// Reserves a slot for a new task. Slot 0 belongs to the boot task.
    pub fn reserve(&mut self) -> Option<(usize, u32)> {
        let slot = (1..TASKS).find(|&slot| self.tasks[slot].state == State::Free)?;
        let pid = self.next_pid;
        let home = self.least_loaded();
        self.next_pid = self.next_pid.wrapping_add(1).max(2);
        self.tasks[slot] = Task {
            state: State::Spawning,
            pid,
            wait: Wait::Nothing,
            kill_pending: false,
            kill_deadline: 0,
            cpu_ticks: 0,
            stopped: false,
            priority: PRIORITY_NORMAL,
            home,
            affinity: 0,
            passed_over: 0,
        };
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
        if priority > PRIORITY_MAX {
            return false;
        }
        match self.find_pid(pid) {
            Some(slot) => {
                self.tasks[slot].priority = priority;
                true
            }
            None => false,
        }
    }

    /// Sets the CPUs a task may run on, as a bit each. Returns `false` when there's
    /// no such task or the mask names no online CPU. A running task moves at its
    /// CPU's next tick.
    pub fn set_affinity(&mut self, pid: u32, mask: u32) -> bool {
        let online = if self.online >= 32 { u32::MAX } else { (1u32 << self.online) - 1 };
        if mask & online == 0 {
            return false;
        }
        match self.find_pid(pid) {
            Some(slot) => {
                let task = &mut self.tasks[slot];
                task.affinity = mask;
                if mask >> task.home & 1 == 0 {
                    task.home = mask.trailing_zeros() as u8;
                }
                true
            }
            None => false,
        }
    }

    /// Makes a reserved slot runnable.
    pub fn commit(&mut self, slot: usize) {
        if self.tasks[slot].state == State::Spawning {
            self.tasks[slot].state = State::Ready;
        }
    }

    fn free(&mut self, slot: usize) {
        self.tasks[slot].state = State::Free;
        self.tasks[slot].wait = Wait::Nothing;
        self.tasks[slot].kill_pending = false;
        self.tasks[slot].stopped = false;
    }

    /// Holds or releases `pid`. Returns `false` when it can't be: no such
    /// process, or the boot task, which the system needs running.
    pub fn set_stopped(&mut self, pid: u32, stopped: bool) -> bool {
        if pid == self.tasks[0].pid {
            return false;
        }
        match self.find_pid(pid) {
            Some(slot) => {
                self.tasks[slot].stopped = stopped;
                true
            }
            None => false,
        }
    }

    /// Frees a killed task, charging any heap it still owns as leaked.
    fn reap(&mut self, slot: usize, facts: &impl TaskFacts) {
        self.leaked_bytes = self.leaked_bytes.saturating_add(facts.heap_bytes(slot));
        self.free(slot);
    }

    /// Returns `true` when the flagged task in `slot` can be reaped now.
    ///
    /// Holding a lock always defers: reaping then would strand the lock. Live
    /// heap defers only until the deadline, which gives a task that polls
    /// `kill_requested` time to unwind and free it.
    fn reapable(&self, slot: usize, now: u64, facts: &impl TaskFacts) -> bool {
        let task = &self.tasks[slot];
        task.kill_pending
            && facts.lock_depth(slot) == 0
            && (facts.heap_bytes(slot) == 0 || now >= task.kill_deadline)
    }

    /// Asks for `pid` to die.
    ///
    /// `grace` is how long, in counter ticks, a task with live heap may keep
    /// running to free it. A task that isn't running and is safe to reap goes
    /// at once. Otherwise the flag is honoured by [`switch`](Self::switch) at
    /// the owner's next tick. A task that must run to release something is
    /// woken so that it can.
    pub fn kill(&mut self, pid: u32, now: u64, grace: u64, facts: &impl TaskFacts) -> KillOutcome {
        if pid == self.tasks[0].pid {
            return KillOutcome::Protected;
        }
        let Some(slot) = self.find_pid(pid) else {
            return KillOutcome::NotFound;
        };
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
                self.tasks[slot].state = State::Ready;
                self.tasks[slot].wait = Wait::Nothing;
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
        for task in &self.tasks {
            if task.state == State::Blocked && task.home as usize == cpu {
                if let Wait::Until(until) | Wait::KeyUntil(_, until) = task.wait {
                    deadline = deadline.min(until);
                }
            }
        }
        deadline
    }

    /// Makes every sleeper whose deadline has passed runnable.
    fn wake_expired(&mut self, now: u64) {
        for task in &mut self.tasks {
            if task.state == State::Blocked {
                if let Wait::Until(deadline) | Wait::KeyUntil(_, deadline) = task.wait {
                    if now >= deadline {
                        task.state = State::Ready;
                        task.wait = Wait::Nothing;
                    }
                }
            }
        }
    }

    /// Wakes `pid` if it's blocked, whatever for: waits may always wake spuriously.
    pub fn wake_pid(&mut self, pid: u32) -> bool {
        let Some(slot) = self.find_pid(pid) else {
            return false;
        };
        let task = &mut self.tasks[slot];
        if task.state != State::Blocked {
            return false;
        }
        task.state = State::Ready;
        task.wait = Wait::Nothing;
        true
    }

    /// Wakes up to `count` tasks blocked on `key` and returns how many woke.
    pub fn wake_key(&mut self, key: usize, count: usize) -> usize {
        let mut woken = 0;
        for task in &mut self.tasks {
            if woken == count {
                break;
            }
            let waiting = match task.wait {
                Wait::Key(waited) | Wait::KeyUntil(waited, _) => waited == key,
                _ => false,
            };
            if task.state == State::Blocked && waiting {
                task.state = State::Ready;
                task.wait = Wait::Nothing;
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
    /// A flagged task found on the way is reaped instead of being run.
    fn take_next(
        &mut self,
        cpu: usize,
        after: Option<usize>,
        now: u64,
        facts: &impl TaskFacts,
    ) -> Option<usize> {
        let start = after.unwrap_or(TASKS - 1);

        // Everything this CPU could run, and how long each CPU's queue is.
        let mut queued = [0usize; CPUS];
        for slot in 0..TASKS {
            if self.tasks[slot].state != State::Ready {
                continue;
            }
            if self.reapable(slot, now, facts) {
                self.reap(slot, facts);
                continue;
            }
            if self.tasks[slot].stopped && !self.tasks[slot].kill_pending {
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
            let task = &self.tasks[slot];
            if task.state != State::Ready
                || (task.stopped && !task.kill_pending)
                || !self.allowed(slot, cpu)
            {
                continue;
            }
            let urgency = if task.passed_over >= STARVATION_LIMIT {
                PRIORITY_MAX + 1
            } else {
                task.priority
            };
            // Of equal urgency: a longer queue elsewhere first, then this CPU's
            // own, then whatever's left, so that the CPU isn't idle.
            let home = task.home as usize;
            let preference = if home == cpu {
                1
            } else if queued.get(home).is_some_and(|&theirs| theirs > mine) {
                2
            } else {
                0
            };
            // One past any real rank, so the first candidate always counts.
            let rank = (urgency + 1, preference);
            if rank > best_rank {
                best_rank = rank;
                best = Some(slot);
            }
        }

        let chosen = best?;
        for slot in 0..TASKS {
            if slot != chosen && self.tasks[slot].state == State::Ready && self.allowed(slot, cpu) {
                let task = &mut self.tasks[slot];
                task.passed_over = task.passed_over.saturating_add(1);
            }
        }
        let task = &mut self.tasks[chosen];
        task.passed_over = 0;
        task.home = cpu as u8;
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
                    _ => (State::Ready, Wait::Nothing),
                };
                task.state = state;
                task.wait = wait;
                save = Some(slot);
            }
        }

        if request == Request::Preempt {
            self.wake_expired(now);
        }

        let next = self.take_next(cpu, current, now, facts);
        if let Some(slot) = next {
            self.tasks[slot].state = State::Running;
        }
        self.current[cpu] = next.map_or(0, |slot| slot as u8 + 1);

        Switch { save, next, reaped }
    }
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
    }

    impl TaskFacts for Facts {
        fn lock_depth(&self, slot: usize) -> u32 {
            self.depth[slot]
        }

        fn heap_bytes(&self, slot: usize) -> usize {
            self.heap[slot]
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
}
