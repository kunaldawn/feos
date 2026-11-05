//! Window surfaces: pixel memory a user program draws in and the desktop shows.
//!
//! A surface's frames are mapped into its program and read by the window that hosts it,
//! which may be doing that on another CPU at the moment the program dies. So they aren't
//! the program's: `feos_vm` maps them as shared, and a count of who's using a surface
//! frees it when the last goes, the program or the window, in either order. The copy to
//! the screen takes a while, so it mustn't be under a lock that a dying task's exit
//! would have to wait for, and with a count it doesn't have to be.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use feos_abi::{Event, PAGE_SIZE};
use feos_sync::IrqSpinLock;

use crate::memory;

/// Surfaces there can be at once.
const SURFACES: usize = 8;
/// Input events waiting for a program to ask for them.
const QUEUE: usize = 64;

struct Queue {
    events: [Event; QUEUE],
    head: usize,
    len: usize,
}

struct Surface {
    /// Who's using it: the program, and the window once it has attached. Zero means it's
    /// free. Only [`create`] raises it from there, under `ALLOCATE`.
    users: AtomicU32,
    /// The program's PID, as windows look it up.
    owner: AtomicU32,
    first_frame: AtomicUsize,
    pages: AtomicUsize,
    width: AtomicUsize,
    height: AtomicUsize,
    /// How many times the program has said a frame is ready.
    presented: AtomicU32,
    queue: IrqSpinLock<Queue>,
}

static POOL: [Surface; SURFACES] = [const {
    Surface {
        users: AtomicU32::new(0),
        owner: AtomicU32::new(0),
        first_frame: AtomicUsize::new(0),
        pages: AtomicUsize::new(0),
        width: AtomicUsize::new(0),
        height: AtomicUsize::new(0),
        presented: AtomicU32::new(0),
        queue: IrqSpinLock::new(Queue {
            events: [Event { kind: 0, a: 0, b: 0, c: 0 }; QUEUE],
            head: 0,
            len: 0,
        }),
    }
}; SURFACES];

/// Held while a slot goes from free to in use, and while a window looks for its program's.
static ALLOCATE: IrqSpinLock<()> = IrqSpinLock::new(());

/// Makes a surface for the program `owner`, zeroed. Returns its index and first frame.
pub fn create(owner: u32, width: usize, height: usize) -> Option<(usize, usize)> {
    let bytes = width.checked_mul(height)?.checked_mul(4)?;
    let pages = bytes.div_ceil(PAGE_SIZE);
    let first_frame = memory::alloc_frames(pages, 1)?;
    // SAFETY: these frames were just given to this, and the identity map covers them.
    unsafe { core::ptr::write_bytes(first_frame as *mut u8, 0, pages * PAGE_SIZE) };

    let guard = ALLOCATE.lock();
    let found = POOL.iter().position(|surface| surface.users.load(Ordering::Acquire) == 0);
    let Some(index) = found else {
        drop(guard);
        memory::free_frames(first_frame, pages);
        return None;
    };
    let surface = &POOL[index];
    surface.first_frame.store(first_frame, Ordering::Relaxed);
    surface.pages.store(pages, Ordering::Relaxed);
    surface.width.store(width, Ordering::Relaxed);
    surface.height.store(height, Ordering::Relaxed);
    surface.presented.store(0, Ordering::Relaxed);
    surface.owner.store(owner, Ordering::Relaxed);
    {
        let mut queue = surface.queue.lock();
        queue.head = 0;
        queue.len = 0;
    }
    surface.users.store(1, Ordering::Release);
    drop(guard);
    Some((index, first_frame))
}

/// Gives up one use of a surface. The last frees it.
pub fn release(index: usize) {
    let surface = &POOL[index];
    if surface.users.fetch_sub(1, Ordering::AcqRel) == 1 {
        let first = surface.first_frame.load(Ordering::Relaxed);
        let pages = surface.pages.load(Ordering::Relaxed);
        memory::free_frames(first, pages);
    }
}

/// Finds the surface of the program `owner` and takes a use of it for a window.
pub fn attach(owner: u32) -> Option<usize> {
    let _guard = ALLOCATE.lock();
    let index = POOL.iter().position(|surface| {
        surface.users.load(Ordering::Acquire) != 0 && surface.owner.load(Ordering::Relaxed) == owner
    })?;
    POOL[index].users.fetch_add(1, Ordering::AcqRel);
    Some(index)
}

/// Marks a program's frame as ready.
pub fn present(index: usize) {
    POOL[index].presented.fetch_add(1, Ordering::Release);
}

/// A surface's pixels and size, for a window holding a use of it. A program can be writing
/// them at any moment: that's a torn picture, which is its own problem, so long as
/// nothing is read as anything but numbers.
pub fn pixels(index: usize) -> (*const u32, usize, usize, u32) {
    let surface = &POOL[index];
    (
        surface.first_frame.load(Ordering::Relaxed) as *const u32,
        surface.width.load(Ordering::Relaxed),
        surface.height.load(Ordering::Relaxed),
        surface.presented.load(Ordering::Acquire),
    )
}

/// Queues an event for the program, dropping the oldest if it isn't keeping up.
pub fn push_event(index: usize, event: Event) {
    let mut queue = POOL[index].queue.lock();
    if queue.len == QUEUE {
        queue.head = (queue.head + 1) % QUEUE;
        queue.len -= 1;
    }
    let at = (queue.head + queue.len) % QUEUE;
    queue.events[at] = event;
    queue.len += 1;
}

/// The program's next event.
pub fn pop_event(index: usize) -> Option<Event> {
    let mut queue = POOL[index].queue.lock();
    if queue.len == 0 {
        return None;
    }
    let event = queue.events[queue.head];
    queue.head = (queue.head + 1) % QUEUE;
    queue.len -= 1;
    Some(event)
}
