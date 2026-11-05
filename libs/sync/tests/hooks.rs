//! Lock-depth accounting through the scheduler hooks.
//!
//! The hooks are process-wide, so this file holds one test: integration tests
//! each get their own process, which keeps it away from the crate's unit tests.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use feos_sync::{install_hooks, IrqSpinLock, Mutex, SchedHooks, SpinLock};

static DEPTH: AtomicU32 = AtomicU32::new(0);
static RELAX: AtomicUsize = AtomicUsize::new(0);
static WAITS: AtomicUsize = AtomicUsize::new(0);
static WAKES: AtomicUsize = AtomicUsize::new(0);

static HOOKS: SchedHooks = SchedHooks {
    lock_enter: || {
        DEPTH.fetch_add(1, Ordering::Relaxed);
    },
    lock_exit: || {
        DEPTH.fetch_sub(1, Ordering::Relaxed);
    },
    relax: || {
        RELAX.fetch_add(1, Ordering::Relaxed);
    },
    wait: |_, _| {
        WAITS.fetch_add(1, Ordering::Relaxed);
        std::thread::yield_now();
    },
    wait_ms: |_, _, _| {
        WAITS.fetch_add(1, Ordering::Relaxed);
        std::thread::yield_now();
    },
    wake: |_, _| {
        WAKES.fetch_add(1, Ordering::Relaxed);
    },
    // No IDs: nothing is recorded, as before the scheduler starts.
    cpu_id: || 0,
    task_id: || 0,
};

fn depth() -> u32 {
    DEPTH.load(Ordering::Relaxed)
}

#[test]
fn locks_keep_the_task_lock_depth_balanced() {
    install_hooks(&HOOKS);

    // A held spin lock counts; releasing it restores the depth.
    let spin = SpinLock::new(());
    {
        let _first = spin.lock();
        assert_eq!(depth(), 1);
        // A failed attempt holds nothing, so the depth must not leak.
        assert!(spin.try_lock().is_none());
        assert_eq!(depth(), 1);
    }
    assert_eq!(depth(), 0);

    // The IRQ lock's holder cannot be preempted, so it is not counted.
    let irq = IrqSpinLock::new(());
    {
        let _guard = irq.lock();
        assert_eq!(depth(), 0);
    }

    // A mutex counts while held.
    let mutex = Mutex::new(());
    {
        let _guard = mutex.lock();
        assert_eq!(depth(), 1);
        assert!(mutex.try_lock().is_none());
        assert_eq!(depth(), 1);
    }
    assert_eq!(depth(), 0);

    // A task spinning on a held lock holds nothing, so it stays killable.
    let shared = std::sync::Arc::new(SpinLock::new(()));
    let guard = shared.lock();
    assert_eq!(depth(), 1);
    let thread = {
        let shared = std::sync::Arc::clone(&shared);
        std::thread::spawn(move || drop(shared.lock()))
    };
    while RELAX.load(Ordering::Relaxed) == 0 {
        std::thread::yield_now();
    }
    assert_eq!(depth(), 1, "a spinning waiter must not count as a holder");
    drop(guard);
    thread.join().unwrap();
    assert_eq!(depth(), 0);

    // A contended mutex waits through the hook and wakes on release.
    let shared = std::sync::Arc::new(Mutex::new(0u32));
    let guard = shared.lock();
    let thread = {
        let shared = std::sync::Arc::clone(&shared);
        std::thread::spawn(move || *shared.lock() += 1)
    };
    while WAITS.load(Ordering::Relaxed) == 0 {
        std::thread::yield_now();
    }
    drop(guard);
    thread.join().unwrap();
    assert_eq!(*shared.lock(), 1);
    assert!(WAKES.load(Ordering::Relaxed) >= 1);
    assert_eq!(depth(), 0);
}
