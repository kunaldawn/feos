//! A small dependency-free, thread-safe lazy cell for no_std lookup tables.

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU8, Ordering};

const UNINIT: u8 = 0;
const INITIALIZING: u8 = 1;
const READY: u8 = 2;

/// Single-assignment cell usable in a static without `std::sync::OnceLock`.
///
/// The initialized value is intentionally never dropped; this type is for
/// process-lifetime lookup tables. The initializer must be panic-safe: a
/// panic resets the state so another caller can retry initialization.
pub(crate) struct OnceCell<T> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<T>>,
}

// The state machine makes access to the value synchronized. The public API
// never exposes mutable aliases and initialization is performed once.
unsafe impl<T: Send + Sync> Sync for OnceCell<T> {}

impl<T> OnceCell<T> {
    pub(crate) const fn new() -> Self {
        Self {
            state: AtomicU8::new(UNINIT),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    pub(crate) fn get_or_init(&self, init: impl FnOnce() -> T) -> &T {
        loop {
            if self.state.load(Ordering::Acquire) == READY {
                // SAFETY: READY is only published after write initialization.
                return unsafe { (*self.value.get()).assume_init_ref() };
            }

            if self
                .state
                .compare_exchange(UNINIT, INITIALIZING, Ordering::Acquire, Ordering::Acquire)
                .is_ok()
            {
                let mut guard = InitGuard {
                    state: &self.state,
                    committed: false,
                };
                let value = init();
                // SAFETY: this thread owns the INITIALIZING state exclusively.
                unsafe { (*self.value.get()).write(value) };
                self.state.store(READY, Ordering::Release);
                guard.committed = true;
                return unsafe { (*self.value.get()).assume_init_ref() };
            }

            while self.state.load(Ordering::Acquire) == INITIALIZING {
                spin_loop();
            }
        }
    }
}

struct InitGuard<'a> {
    state: &'a AtomicU8,
    committed: bool,
}

impl Drop for InitGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.state.store(UNINIT, Ordering::Release);
        }
    }
}
