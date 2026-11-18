//! [`TryArc`]: a counted, shared value like `Arc`, but made fallibly.
//!
//! `Arc::try_new` isn't stable, and a plain `Arc::new` that finds the heap full panics,
//! which in the kernel can't always be contained: a value made for a program's call (an
//! open file description, say) has to fail with `ENOMEM` instead. The room can also be
//! taken first, with [`TryArc::reserve`], before anything is done that the value would
//! have to undo.

use alloc::alloc::{alloc, dealloc};
use core::alloc::Layout;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::ptr::NonNull;
use core::sync::atomic::{fence, AtomicUsize, Ordering};

struct Inner<T> {
    count: AtomicUsize,
    value: T,
}

/// A counted reference to a `T` on the heap, as `Arc<T>` is.
pub struct TryArc<T> {
    inner: NonNull<Inner<T>>,
    owns: PhantomData<Inner<T>>,
}

// SAFETY: as for `Arc<T>`: the value is shared between threads, and dropped by whichever
// lets go last.
unsafe impl<T: Send + Sync> Send for TryArc<T> {}
// SAFETY: as above.
unsafe impl<T: Send + Sync> Sync for TryArc<T> {}

/// Room on the heap for one [`TryArc`], not yet holding a value.
pub struct Reserved<T> {
    inner: NonNull<MaybeUninit<Inner<T>>>,
}

// SAFETY: it holds no value, only memory.
unsafe impl<T> Send for Reserved<T> {}

impl<T> Reserved<T> {
    /// Puts `value` in, as the first reference to it. Allocates nothing.
    pub fn fill(self, value: T) -> TryArc<T> {
        let inner = self.inner.cast::<Inner<T>>();
        core::mem::forget(self);
        // SAFETY: the allocation was made for an `Inner<T>` and holds nothing yet.
        unsafe { inner.as_ptr().write(Inner { count: AtomicUsize::new(1), value }) };
        TryArc { inner, owns: PhantomData }
    }
}

impl<T> Drop for Reserved<T> {
    fn drop(&mut self) {
        // SAFETY: made by `reserve` with this layout, and never filled.
        unsafe { dealloc(self.inner.as_ptr().cast(), Layout::new::<Inner<T>>()) };
    }
}

impl<T> TryArc<T> {
    /// Room for one, or `None` if the heap has none.
    pub fn reserve() -> Option<Reserved<T>> {
        let layout = Layout::new::<Inner<T>>();
        if layout.size() == 0 {
            // A zero-sized `Inner` can't be: it has the count.
            return None;
        }
        // SAFETY: the layout isn't zero sized.
        let raw = unsafe { alloc(layout) }.cast::<MaybeUninit<Inner<T>>>();
        Some(Reserved { inner: NonNull::new(raw)? })
    }

    /// The first reference to `value`, or `value` back if the heap has no room.
    pub fn new(value: T) -> Result<Self, T> {
        match Self::reserve() {
            Some(room) => Ok(room.fill(value)),
            None => Err(value),
        }
    }

    fn inner(&self) -> &Inner<T> {
        // SAFETY: the allocation lives while any reference does, this one included.
        unsafe { self.inner.as_ref() }
    }

    /// Whether two references are to the same value.
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.inner == other.inner
    }

    /// References to the value, this one included.
    pub fn count(this: &Self) -> usize {
        this.inner().count.load(Ordering::Acquire)
    }

    /// What one costs on the heap, before the allocator's own overhead.
    pub const fn heap_size() -> usize {
        core::mem::size_of::<Inner<T>>()
    }
}

impl<T> Clone for TryArc<T> {
    fn clone(&self) -> Self {
        // As `Arc` does: a count that could overflow is a leak gone mad, and stops.
        if self.inner().count.fetch_add(1, Ordering::Relaxed) > isize::MAX as usize {
            panic!("TryArc count overflow");
        }
        Self { inner: self.inner, owns: PhantomData }
    }
}

impl<T> Drop for TryArc<T> {
    fn drop(&mut self) {
        if self.inner().count.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        // Everything any other reference did to the value happens before it goes.
        fence(Ordering::Acquire);
        // SAFETY: this was the last reference, so nothing else can reach the allocation,
        // which `reserve` made with this layout.
        unsafe {
            core::ptr::drop_in_place(self.inner.as_ptr());
            dealloc(self.inner.as_ptr().cast(), Layout::new::<Inner<T>>());
        }
    }
}

impl<T> Deref for TryArc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner().value
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for TryArc<T> {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.inner().value.fmt(out)
    }
}

#[cfg(test)]
mod tests {
    use super::TryArc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Counted(Arc<AtomicUsize>);

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn shared_value_drops_once_with_the_last_reference() {
        let drops = Arc::new(AtomicUsize::new(0));
        let first = TryArc::new(Counted(drops.clone())).ok().unwrap();
        let second = first.clone();
        assert!(TryArc::ptr_eq(&first, &second));
        assert_eq!(TryArc::count(&first), 2);
        drop(first);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(second);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn reserved_room_unfilled_drops_nothing() {
        let drops = Arc::new(AtomicUsize::new(0));
        let room = TryArc::<Counted>::reserve().unwrap();
        drop(room);
        let filled = TryArc::<Counted>::reserve().unwrap().fill(Counted(drops.clone()));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let other = TryArc::new(Counted(drops.clone())).ok().unwrap();
        assert!(!TryArc::ptr_eq(&filled, &other));
        drop((filled, other));
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn shared_across_threads() {
        let value = TryArc::new(AtomicUsize::new(0)).ok().unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let mine = value.clone();
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        let again = mine.clone();
                        again.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(value.load(Ordering::Relaxed), 8000);
        assert_eq!(TryArc::count(&value), 1);
    }
}
