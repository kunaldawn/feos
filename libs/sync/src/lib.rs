//! Small synchronization primitives for a bare-metal multi-core kernel.
//!
//! The kernel runs on several CPUs and with preemption enabled, so shared state
//! such as the process table, console and heap needs real mutual exclusion.
//!
//! - [`SpinLock`] busy-waits and stays preemptible. A task can be preempted
//!   holding one, so every acquisition is counted in the task's lock depth
//!   (see [`SchedHooks`]) and the kernel never reaps a task that holds one.
//! - [`IrqSpinLock`] also masks interrupts on this CPU while held, so it is
//!   safe to share with interrupt handlers and its holder cannot be preempted.
//!   Use it for short critical sections.
//! - [`Mutex`] blocks in the scheduler instead of spinning. Use it for anything
//!   that may be held for a long time.
//!
//! Without installed hooks (host tests, early boot) a contended [`Mutex`]
//! falls back to spinning.

#![cfg_attr(not(test), no_std)]

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

pub mod irq;

/// Spins on a contended [`SpinLock`] before offering the CPU to other tasks.
const RELAX_SPINS: u32 = 4096;

/// Scheduler services the primitives in this crate call.
///
/// The crate cannot depend on the kernel, so the kernel installs these once
/// during boot with [`install_hooks`].
pub struct SchedHooks {
    /// A task is about to hold a [`SpinLock`] or [`Mutex`].
    pub lock_enter: fn(),
    /// A task released a [`SpinLock`] or [`Mutex`].
    pub lock_exit: fn(),
    /// A task is waiting and could let another one run.
    pub relax: fn(),
    /// Blocks the caller while `word` still holds `expected`.
    pub wait: fn(word: &AtomicU32, expected: u32),
    /// Wakes up to `count` tasks blocked on `word`.
    pub wake: fn(word: &AtomicU32, count: usize),
}

static HOOKS: AtomicPtr<SchedHooks> = AtomicPtr::new(core::ptr::null_mut());

/// Installs the scheduler services. Call once, before any task can be preempted.
pub fn install_hooks(hooks: &'static SchedHooks) {
    HOOKS.store(hooks as *const SchedHooks as *mut SchedHooks, Ordering::Release);
}

#[inline]
fn hooks() -> Option<&'static SchedHooks> {
    let hooks = HOOKS.load(Ordering::Acquire);
    // SAFETY: only `install_hooks` writes this pointer, from a `'static` value.
    unsafe { hooks.as_ref() }
}

#[inline]
fn lock_enter() {
    if let Some(hooks) = hooks() {
        (hooks.lock_enter)();
    }
}

#[inline]
fn lock_exit() {
    if let Some(hooks) = hooks() {
        (hooks.lock_exit)();
    }
}

/// Offers the CPU to another runnable task, when a scheduler is installed.
pub fn yield_now() {
    if let Some(hooks) = hooks() {
        (hooks.relax)();
    }
}

/// A mutual-exclusion lock that busy-waits until the value is available.
///
/// The lock is not re-entrant. Lock acquisitions should be short: copying a
/// pointer, updating a counter, or writing to a device register.
///
/// A holder can be preempted but is never killed: see [`SchedHooks::lock_enter`].
/// A task still waiting for the lock holds nothing, so it can be killed.
pub struct SpinLock<T: ?Sized> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: access to `value` is serialized by `locked`.
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Creates an unlocked spin lock containing `value`.
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Consumes the lock and returns the inner value.
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: ?Sized> SpinLock<T> {
    /// Acquires the lock, spinning until it becomes available.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let mut spins = 0u32;
        loop {
            // Count the attempt first: preemption between a successful swap
            // and the count would let the kernel reap a task holding the lock.
            lock_enter();
            if !self.locked.swap(true, Ordering::Acquire) {
                return SpinLockGuard { lock: self };
            }
            lock_exit();

            // Contended: spin on a relaxed load so we do not keep hammering
            // the cache line with read-modify-write operations. The holder may
            // have been preempted, so let others run after a while.
            while self.locked.load(Ordering::Relaxed) {
                spin_loop();
                spins = spins.wrapping_add(1);
                if spins % RELAX_SPINS == 0 {
                    yield_now();
                }
            }
        }
    }

    /// Tries to acquire the lock without spinning.
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        lock_enter();
        if self.locked.swap(true, Ordering::Acquire) {
            lock_exit();
            None
        } else {
            Some(SpinLockGuard { lock: self })
        }
    }

    /// Returns `true` if the lock is currently held.
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    /// Returns a shared reference without locking.
    ///
    /// # Safety
    /// The caller must ensure no mutable access can happen for as long as the
    /// returned reference is alive. Intended for panic and fault paths.
    pub unsafe fn get_unchecked(&self) -> &T {
        unsafe { &*self.value.get() }
    }
}

impl<T: Default> Default for SpinLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// RAII guard returned by [`SpinLock::lock`].
pub struct SpinLockGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
}

impl<T: ?Sized> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: holding the guard grants exclusive access.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T: ?Sized> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: holding the guard grants exclusive access.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T: ?Sized> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        lock_exit();
    }
}

/// A spin lock that keeps interrupts masked on this CPU while it is held.
///
/// The guard restores the previous interrupt state on drop, so critical
/// sections nest and work from interrupt handlers. The holder cannot be
/// preempted or reaped, so these acquisitions are not part of the task's lock
/// depth. Keep the critical section short: it delays this CPU's interrupts.
pub struct IrqSpinLock<T: ?Sized> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: access to `value` is serialized by `locked`.
unsafe impl<T: ?Sized + Send> Sync for IrqSpinLock<T> {}
unsafe impl<T: ?Sized + Send> Send for IrqSpinLock<T> {}

impl<T> IrqSpinLock<T> {
    /// Creates an unlocked lock containing `value`.
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }
}

impl<T: ?Sized> IrqSpinLock<T> {
    /// Masks interrupts and acquires the lock.
    pub fn lock(&self) -> IrqSpinLockGuard<'_, T> {
        let flags = irq::save_and_disable();
        while self.locked.swap(true, Ordering::Acquire) {
            while self.locked.load(Ordering::Relaxed) {
                spin_loop();
            }
        }
        IrqSpinLockGuard { lock: self, flags }
    }

    /// Tries to acquire the lock; the interrupt state is unchanged on failure.
    pub fn try_lock(&self) -> Option<IrqSpinLockGuard<'_, T>> {
        let flags = irq::save_and_disable();
        if self.locked.swap(true, Ordering::Acquire) {
            irq::restore(flags);
            None
        } else {
            Some(IrqSpinLockGuard { lock: self, flags })
        }
    }

    /// Returns `true` if the lock is currently held.
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }
}

/// RAII guard returned by [`IrqSpinLock::lock`].
pub struct IrqSpinLockGuard<'a, T: ?Sized> {
    lock: &'a IrqSpinLock<T>,
    flags: u64,
}

impl<T: ?Sized> Deref for IrqSpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: holding the guard grants exclusive access.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T: ?Sized> DerefMut for IrqSpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: holding the guard grants exclusive access.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T: ?Sized> Drop for IrqSpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        irq::restore(self.flags);
    }
}

const MUTEX_FREE: u32 = 0;
const MUTEX_HELD: u32 = 1;
const MUTEX_CONTENDED: u32 = 2;

/// A mutual-exclusion lock that blocks in the scheduler while contended.
///
/// The holder may sleep. Like a [`SpinLock`], holding it defers a kill of the
/// holder until it is released; a task blocked waiting holds nothing and can
/// be killed. Never take one with interrupts masked or from an interrupt
/// handler.
pub struct Mutex<T: ?Sized> {
    state: AtomicU32,
    value: UnsafeCell<T>,
}

// SAFETY: access to `value` is serialized by `state`.
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}

impl<T> Mutex<T> {
    /// Creates an unlocked mutex containing `value`.
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(MUTEX_FREE),
            value: UnsafeCell::new(value),
        }
    }

    /// Consumes the mutex and returns the inner value.
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    fn acquire(&self, desired: u32) -> Result<MutexGuard<'_, T>, u32> {
        lock_enter();
        match self.state.compare_exchange(
            MUTEX_FREE,
            desired,
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(MutexGuard { lock: self }),
            Err(state) => {
                lock_exit();
                Err(state)
            }
        }
    }

    /// Acquires the mutex, blocking until it becomes available.
    pub fn lock(&self) -> MutexGuard<'_, T> {
        let mut desired = MUTEX_HELD;
        loop {
            let state = match self.acquire(desired) {
                Ok(guard) => return guard,
                Err(state) => state,
            };

            // Advertise a waiter so the holder wakes someone on release. Once
            // this task has waited, it must keep the flag when it acquires,
            // because other waiters may still be queued behind it.
            if state == MUTEX_CONTENDED
                || self
                    .state
                    .compare_exchange(
                        MUTEX_HELD,
                        MUTEX_CONTENDED,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                match hooks() {
                    Some(hooks) => (hooks.wait)(&self.state, MUTEX_CONTENDED),
                    None => spin_loop(),
                }
            }
            desired = MUTEX_CONTENDED;
        }
    }

    /// Tries to acquire the mutex without blocking.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.acquire(MUTEX_HELD).ok()
    }

    /// Returns `true` if the mutex is currently held.
    pub fn is_locked(&self) -> bool {
        self.state.load(Ordering::Relaxed) != MUTEX_FREE
    }
}

/// RAII guard returned by [`Mutex::lock`].
pub struct MutexGuard<'a, T: ?Sized> {
    lock: &'a Mutex<T>,
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: holding the guard grants exclusive access.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: holding the guard grants exclusive access.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        let previous = self.lock.state.swap(MUTEX_FREE, Ordering::Release);
        // Wake before leaving the lock depth: a task reaped between the two
        // would strand the waiter it had not yet woken.
        if previous == MUTEX_CONTENDED {
            if let Some(hooks) = hooks() {
                (hooks.wake)(&self.lock.state, 1);
            }
        }
        lock_exit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_exposes_and_releases_value() {
        let lock = SpinLock::new(41);

        {
            let mut guard = lock.lock();
            *guard += 1;
            assert!(lock.is_locked());
        }

        assert!(!lock.is_locked());
        assert_eq!(*lock.lock(), 42);
    }

    #[test]
    fn try_lock_fails_while_held() {
        let lock = SpinLock::new(());
        let _guard = lock.lock();
        assert!(lock.try_lock().is_none());
    }

    #[test]
    fn into_inner_returns_value() {
        assert_eq!(SpinLock::new(7u32).into_inner(), 7);
    }

    #[test]
    fn irq_lock_masks_while_held_and_restores_on_drop() {
        let lock = IrqSpinLock::new(1);
        assert!(irq::is_enabled());
        {
            let mut guard = lock.lock();
            assert!(!irq::is_enabled());
            *guard = 2;
        }
        assert!(irq::is_enabled());
        assert_eq!(*lock.lock(), 2);
    }

    #[test]
    fn irq_lock_nests_and_keeps_the_outer_mask() {
        let outer = IrqSpinLock::new(());
        let inner = IrqSpinLock::new(());
        let first = outer.lock();
        {
            let _second = inner.lock();
            assert!(!irq::is_enabled());
        }
        assert!(!irq::is_enabled(), "inner guard must restore, not unmask");
        drop(first);
        assert!(irq::is_enabled());
    }

    #[test]
    fn irq_try_lock_leaves_interrupts_alone_on_failure() {
        let lock = IrqSpinLock::new(());
        let guard = lock.lock();
        assert!(lock.try_lock().is_none());
        assert!(!irq::is_enabled());
        drop(guard);
        assert!(irq::is_enabled());
        assert!(lock.try_lock().is_some());
    }

    #[test]
    fn mutex_is_exclusive_across_threads() {
        extern crate std;
        use std::sync::Arc;

        let mutex = Arc::new(Mutex::new(0u32));
        let threads: std::vec::Vec<_> = (0..4)
            .map(|_| {
                let mutex = Arc::clone(&mutex);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        *mutex.lock() += 1;
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(*mutex.lock(), 4000);
        assert!(!mutex.is_locked() || mutex.try_lock().is_none());
    }

    #[test]
    fn mutex_try_lock_fails_while_held() {
        let mutex = Mutex::new(5);
        let guard = mutex.lock();
        assert!(mutex.try_lock().is_none());
        drop(guard);
        assert_eq!(mutex.try_lock().map(|guard| *guard), Some(5));
    }
}
