//! Promises: frames the machine owes programs it hasn't handed over yet.
//!
//! A charged page is promised when it's mapped and gets its frame when it's first
//! touched, so running out is an answer to `mmap`, not a fault later. A promise is only
//! made if the frames free now cover it and every promise before it, with a spare kept
//! back for the kernel's own frames (page tables, kernel stacks). Every frame taken for
//! user memory goes promise, allocate, then release what wasn't used: never "is there
//! room?" followed by an allocation, which another CPU could get between.

use core::sync::atomic::{AtomicUsize, Ordering};

/// The machine's promises, one value shared by every address space.
#[derive(Debug)]
pub struct Promises {
    held: AtomicUsize,
    /// Frames no promise may count on.
    spare: usize,
    /// Releases of more than was held, clamped. Always zero unless there's a bug.
    underflows: AtomicUsize,
}

impl Promises {
    pub const fn new(spare: usize) -> Self {
        Self {
            held: AtomicUsize::new(0),
            spare,
            underflows: AtomicUsize::new(0),
        }
    }

    /// Promises `pages` if `free`, the frames free right now, covers them, every other
    /// promise and the spare.
    pub fn promise(&self, pages: usize, free: usize) -> bool {
        if pages == 0 {
            return true;
        }
        self.held
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                let wanted = held.checked_add(pages)?;
                (wanted.checked_add(self.spare)? <= free).then_some(wanted)
            })
            .is_ok()
    }

    /// Takes a promise without asking, for a page whose frame is about to be freed and
    /// whose promise has to stay: the frame covers it once it's back.
    pub fn hold(&self, pages: usize) {
        self.held.fetch_add(pages, Ordering::AcqRel);
    }

    /// Gives back promises that were kept, or that nobody needs any more. Releasing more
    /// than is held is a bug: it stops the machine in a debug build, and is clamped,
    /// counted and reported `false` otherwise, since a promise too few is a page that
    /// might not be there.
    pub fn release(&self, pages: usize) -> bool {
        if pages == 0 {
            return true;
        }
        let result = self
            .held
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| held.checked_sub(pages));
        if result.is_ok() {
            return true;
        }
        debug_assert!(false, "released {pages} promises, more than are held");
        self.held.store(0, Ordering::Release);
        self.underflows.fetch_add(1, Ordering::AcqRel);
        false
    }

    /// Pages promised and not yet given their frames.
    pub fn held(&self) -> usize {
        self.held.load(Ordering::Acquire)
    }

    /// How many releases were clamped.
    pub fn underflows(&self) -> usize {
        self.underflows.load(Ordering::Acquire)
    }

    /// Frames no promise may count on.
    pub const fn spare(&self) -> usize {
        self.spare
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promise_then_release() {
        let promises = Promises::new(10);
        assert!(promises.promise(90, 100));
        assert!(!promises.promise(1, 100), "the spare is kept back");
        assert!(promises.promise(0, 0));
        assert!(promises.release(40));
        assert!(promises.promise(40, 100));
        assert!(!promises.promise(usize::MAX, usize::MAX), "no overflow");
        assert_eq!(promises.held(), 90);
        promises.hold(5);
        assert_eq!(promises.held(), 95);
        assert!(promises.release(95));
        assert_eq!((promises.held(), promises.underflows()), (0, 0));
    }

    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "more than are held"))]
    fn release_underflow_detected() {
        let promises = Promises::new(0);
        assert!(promises.promise(3, 10));
        // In a release build the release is clamped and counted.
        assert!(!promises.release(4));
        assert_eq!((promises.held(), promises.underflows()), (0, 1));
    }

    /// Host threads racing to promise against a fixed number of free frames: however
    /// they interleave, no more is promised than there is.
    #[test]
    fn concurrent_promises_never_overcommit() {
        use std::sync::Arc;
        const FREE: usize = 10_000;
        const SPARE: usize = 1000;
        let promises = Arc::new(Promises::new(SPARE));
        let threads: Vec<_> = (0..8)
            .map(|seed| {
                let promises = Arc::clone(&promises);
                std::thread::spawn(move || {
                    let mut got = 0usize;
                    for round in 0..20_000usize {
                        let pages = 1 + (round * 7 + seed) % 5;
                        if promises.promise(pages, FREE) {
                            got += pages;
                            assert!(promises.held() + SPARE <= FREE);
                            // Some of it given back as it goes, as unmaps do.
                            if round % 3 == 0 {
                                assert!(promises.release(pages));
                                got -= pages;
                            }
                        }
                    }
                    got
                })
            })
            .collect();
        let total: usize = threads.into_iter().map(|thread| thread.join().unwrap()).sum();
        assert_eq!(total, promises.held());
        assert!(total <= FREE - SPARE);
        // Full, give or take what the threads' last releases left.
        assert!(total + 8 * 5 > FREE - SPARE);
    }
}
