//! Small synchronization primitives for a single-core bare-metal kernel.
//!
//! `feos` currently runs with interrupts disabled and on one CPU, but the
//! kernel still needs to exclude re-entrant use of shared state (for example a
//! console used by both normal code and a panic path). [`SpinLock`] is the
//! building block for that.
//!
//! The types here deliberately do not implement any interrupt masking: an
//! interrupt handler that takes the same lock as the code it interrupted will
//! deadlock. Once the kernel grows interrupts, the lock should be combined
//! with an interrupt-disable guard at the call site.

#![no_std]

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A mutual-exclusion lock that busy-waits until the value is available.
///
/// The lock is not re-entrant. Lock acquisitions should be short: copying a
/// pointer, updating a counter, or writing to a device register.
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
        loop {
            if !self.locked.swap(true, Ordering::Acquire) {
                return SpinLockGuard { lock: self };
            }

            // Contended: spin on a relaxed load so we do not keep hammering
            // the cache line with read-modify-write operations.
            while self.locked.load(Ordering::Relaxed) {
                spin_loop();
            }
        }
    }

    /// Tries to acquire the lock without spinning.
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        if self.locked.swap(true, Ordering::Acquire) {
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
}
