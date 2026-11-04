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

#![cfg_attr(not(test), no_std)]

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
        }
    }

    /// Creates the boot task as slot 0, PID 1, running on CPU 0.
    pub fn init(&mut self) {
        self.tasks[0].state = State::Running;
        self.tasks[0].pid = 1;
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
        self.next_pid = self.next_pid.wrapping_add(1).max(2);
        self.tasks[slot] = Task {
            state: State::Spawning,
            pid,
            wait: Wait::Nothing,
            kill_pending: false,
            kill_deadline: 0,
            cpu_ticks: 0,
        };
        Some((slot, pid))
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

    /// Makes every sleeper whose deadline has passed runnable.
    fn wake_expired(&mut self, now: u64) {
        for task in &mut self.tasks {
            if task.state == State::Blocked {
                if let Wait::Until(deadline) = task.wait {
                    if now >= deadline {
                        task.state = State::Ready;
                        task.wait = Wait::Nothing;
                    }
                }
            }
        }
    }

    /// Wakes up to `count` tasks blocked on `key` and returns how many woke.
    pub fn wake_key(&mut self, key: usize, count: usize) -> usize {
        let mut woken = 0;
        for task in &mut self.tasks {
            if woken == count {
                break;
            }
            if task.state == State::Blocked && task.wait == Wait::Key(key) {
                task.state = State::Ready;
                task.wait = Wait::Nothing;
                woken += 1;
            }
        }
        woken
    }

    /// Takes the next `Ready` slot after `after`, wrapping around.
    ///
    /// A flagged task found on the way is reaped instead of being run.
    fn take_next(&mut self, after: Option<usize>, now: u64, facts: &impl TaskFacts) -> Option<usize> {
        let start = after.unwrap_or(TASKS - 1);
        for step in 1..=TASKS {
            let slot = (start + step) % TASKS;
            if self.tasks[slot].state != State::Ready {
                continue;
            }
            if self.reapable(slot, now, facts) {
                self.reap(slot, facts);
                continue;
            }
            return Some(slot);
        }
        None
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

        let next = self.take_next(current, now, facts);
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
}
