//! How a display that needs telling hears about changes.
//!
//! ramfb shows memory directly, so drawing is enough. virtio-gpu shows it only
//! when asked. Rather than make everything that draws know which it has, the board
//! registers a flush here, and the desktop reports its damage. Whatever draws
//! without reporting (a full-screen app) is caught by a periodic refresh: see
//! [`needs_refresh`].

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

/// Shows a changed rectangle: x, y, width, height.
pub type FlushFn = fn(u32, u32, u32, u32);

static FLUSH: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static WIDTH: AtomicU32 = AtomicU32::new(0);
static HEIGHT: AtomicU32 = AtomicU32::new(0);
static REPORTED: AtomicBool = AtomicBool::new(false);

/// Registers the display's flush and its size.
pub fn set_flush(flush: FlushFn, width: u32, height: u32) {
    WIDTH.store(width, Ordering::Release);
    HEIGHT.store(height, Ordering::Release);
    FLUSH.store(flush as *mut (), Ordering::Release);
}

fn registered() -> Option<FlushFn> {
    let pointer = FLUSH.load(Ordering::Acquire);
    if pointer.is_null() {
        return None;
    }
    // SAFETY: only `set_flush` stores this, from a `FlushFn`.
    Some(unsafe { core::mem::transmute::<*mut (), FlushFn>(pointer) })
}

/// Shows a changed rectangle. Nothing happens on a display that doesn't need it.
pub fn flush(x: u32, y: u32, width: u32, height: u32) {
    if let Some(flush) = registered() {
        REPORTED.store(true, Ordering::Release);
        flush(x, y, width, height);
    }
}

/// Shows the whole screen, if nobody has reported a change since the last call.
/// What's reporting has it covered, and a whole screen is a lot to copy.
pub fn refresh() {
    if REPORTED.swap(false, Ordering::AcqRel) {
        return;
    }
    if let Some(flush) = registered() {
        flush(0, 0, WIDTH.load(Ordering::Acquire), HEIGHT.load(Ordering::Acquire));
    }
}

/// True when something has to refresh the screen for code that draws directly.
pub fn needs_refresh() -> bool {
    !FLUSH.load(Ordering::Acquire).is_null()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32 as Count, Ordering as CountOrdering};

    static CALLS: Count = Count::new(0);
    static LAST_WIDTH: Count = Count::new(0);

    fn record(_x: u32, _y: u32, width: u32, _height: u32) {
        CALLS.fetch_add(1, CountOrdering::SeqCst);
        LAST_WIDTH.store(width, CountOrdering::SeqCst);
    }

    #[test]
    fn nothing_registered_is_nothing_to_do_and_a_flush_reaches_the_board() {
        // One test, as this state is global.
        assert!(!needs_refresh());
        flush(1, 2, 3, 4);
        assert_eq!(CALLS.load(CountOrdering::SeqCst), 0);
        set_flush(record, 640, 480);
        assert!(needs_refresh());
        flush(1, 2, 3, 4);
        assert_eq!(LAST_WIDTH.load(CountOrdering::SeqCst), 3);
        // Someone just reported, so this refresh has nothing to add.
        refresh();
        assert_eq!(LAST_WIDTH.load(CountOrdering::SeqCst), 3);
        refresh();
        assert_eq!(LAST_WIDTH.load(CountOrdering::SeqCst), 640);
    }
}
