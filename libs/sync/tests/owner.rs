//! Taking a lock you hold is a report, not a hang.
//!
//! The hooks are process-wide, so these tests get a process of their own. Each
//! thread stands in for one CPU and one task.

use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};

use feos_sync::{install_hooks, IrqSpinLock, SchedHooks, SpinLock};

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

std::thread_local! {
    static ID: Cell<u32> = const { Cell::new(0) };
}

fn this_id() -> u32 {
    ID.with(|id| {
        if id.get() == 0 {
            id.set(NEXT_ID.fetch_add(1, Ordering::Relaxed));
        }
        id.get()
    })
}

static HOOKS: SchedHooks = SchedHooks {
    lock_enter: || {},
    lock_exit: || {},
    relax: std::thread::yield_now,
    wait: |_, _| std::thread::yield_now(),
    wait_ms: |_, _, _| std::thread::yield_now(),
    wake: |_, _| {},
    cpu_id: this_id,
    task_id: this_id,
};

#[test]
#[should_panic(expected = "a CPU took a lock it already holds")]
fn a_cpu_taking_its_own_irq_lock_panics() {
    install_hooks(&HOOKS);
    let lock = IrqSpinLock::new(());
    let _first = lock.lock();
    let _second = lock.lock();
}

#[test]
#[should_panic(expected = "a task took a spin lock it already holds")]
fn a_task_taking_its_own_spin_lock_panics() {
    install_hooks(&HOOKS);
    let lock = SpinLock::new(());
    let _first = lock.lock();
    let _second = lock.lock();
}

#[test]
fn taking_turns_is_fine() {
    install_hooks(&HOOKS);
    static SPIN: SpinLock<u32> = SpinLock::new(0);
    static IRQ: IrqSpinLock<u32> = IrqSpinLock::new(0);
    let threads: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                for _ in 0..1000 {
                    *SPIN.lock() += 1;
                    *IRQ.lock() += 1;
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(*SPIN.lock(), 4000);
    assert_eq!(*IRQ.lock(), 4000);
}
