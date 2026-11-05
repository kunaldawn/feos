//! Runs a hosted app's calls on a task of its own.
//!
//! The desktop's session is the boot task, and a failure there stops the machine. So
//! each window's app runs on a task of the window's, one call at a time, while the desktop
//! waits. A fault or a panic there ends that task, which ends that window, and the
//! desktop carries on. The app can't tell: it's still called once at a time, in the
//! same order, with the same arguments.
//!
//! # What this doesn't do
//!
//! These apps are linked into the kernel and share its memory and privilege. An app that
//! scribbles on the desktop's memory before it faults has done so, and a fault with
//! a lock held stops the machine, as it would from any task. This contains failures. It
//! isn't isolation in the sense a user program has it, which is what a real wall takes:
//! see `kernel/src/user.rs`.
//!
//! A host with no way to start tasks, like the tests', gets calls run in place.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::{Platform, SpawnIo, WaitStatus};
use feos_sync::{futex_wait, futex_wait_ms, futex_wake, SpinLock};

/// How long an app can go without returning before it's stopped. Long enough for what
/// a real app might do in a frame, like read a big file from a slow disk.
const HANG_MS: u64 = 30_000;
/// How often a waiting desktop looks to see whether the app has died. A dead task
/// can't say it's done, and its exit doesn't change the word being waited on.
const POLL_MS: u64 = 10;
/// Windows with a task. A window past this has its app called in place.
const WORKERS: usize = 32;

/// What a call turned out to be.
pub struct Died {
    /// The task's exit status.
    pub status: i32,
}

struct Shared {
    /// Bumped by the desktop for each call.
    request: AtomicU32,
    /// Set to `request`'s value once that call has returned.
    done: AtomicU32,
    /// The call to make. Written only while the worker is waiting for `request` to
    /// change, and read by it only after, so never at the same time.
    job: UnsafeCell<Option<*mut dyn FnMut()>>,
    quit: AtomicBool,
}

// SAFETY: see `job`.
unsafe impl Send for Shared {}
// SAFETY: see `job`.
unsafe impl Sync for Shared {}

#[derive(Clone)]
struct Worker {
    shared: Arc<Shared>,
    pid: u32,
}

static SLOTS: [SpinLock<Option<Worker>>; WORKERS] = [const { SpinLock::new(None) }; WORKERS];

fn body(shared: Arc<Shared>) -> i32 {
    let mut seen = 0;
    loop {
        if shared.quit.load(Ordering::Acquire) {
            return 0;
        }
        let request = shared.request.load(Ordering::Acquire);
        if request == seen {
            futex_wait(&shared.request, seen);
            continue;
        }
        seen = request;
        // SAFETY: the desktop filled this in before it changed `request`, and it
        // doesn't touch it, or what it points at, until `done` says so or this has died.
        if let Some(job) = unsafe { *shared.job.get() } {
            // SAFETY: as above: it's a pointer to something on the stack of `call`,
            // which stays where it is until this call is over.
            unsafe { (*job)() };
        }
        shared.done.store(seen, Ordering::Release);
        futex_wake(&shared.done, usize::MAX);
    }
}

fn start(platform: &Platform, name: &str) -> Option<Worker> {
    let shared = Arc::new(Shared {
        request: AtomicU32::new(0),
        done: AtomicU32::new(0),
        job: UnsafeCell::new(None),
        quit: AtomicBool::new(false),
    });
    let theirs = shared.clone();
    let io = SpawnIo {
        background: true,
        ..SpawnIo::default()
    };
    let pid = platform
        .processes
        .spawn_task(name, io, Box::new(move || body(theirs)))
        .ok()?;
    Some(Worker { shared, pid })
}

/// Has the window's task run `call`, and waits. `Err` when the task died doing it.
///
/// With nowhere to run it, `call` runs right here.
pub fn call<R>(
    platform: &Platform,
    window: usize,
    name: &str,
    call: impl FnOnce() -> R,
) -> Result<R, Died> {
    let Some(slot) = SLOTS.get(window) else {
        return Ok(call());
    };
    let existing = slot.lock().clone();
    let worker = match existing {
        Some(worker) => worker,
        None => match start(platform, name) {
            Some(worker) => {
                *slot.lock() = Some(worker.clone());
                worker
            }
            None => return Ok(call()),
        },
    };
    run(platform, slot, &worker, call)
}

fn run<R>(
    platform: &Platform,
    slot: &SpinLock<Option<Worker>>,
    worker: &Worker,
    call: impl FnOnce() -> R,
) -> Result<R, Died> {
    let mut call = Some(call);
    let mut result = None;
    let mut thunk = || {
        if let Some(call) = call.take() {
            result = Some(call());
        }
    };
    let job: *mut (dyn FnMut() + '_) = &mut thunk;
    // SAFETY: only the lifetime changes. The pointer isn't used past the end of this
    // function: it either sees the worker finish or sees it dead, before returning.
    let job: *mut (dyn FnMut() + 'static) = unsafe { core::mem::transmute(job) };
    // SAFETY: see `Shared::job`.
    unsafe { *worker.shared.job.get() = Some(job) };

    let shared = &worker.shared;
    let request = shared.request.load(Ordering::Relaxed).wrapping_add(1);
    shared.request.store(request, Ordering::Release);
    futex_wake(&shared.request, 1);

    let processes = platform.processes;
    let started = platform.clock.uptime_ms();
    let mut stopping = false;
    loop {
        let done = shared.done.load(Ordering::Acquire);
        if done == request {
            break;
        }
        if processes.try_wait(worker.pid) != WaitStatus::Running {
            // Finished between those two looks?
            if shared.done.load(Ordering::Acquire) == request {
                break;
            }
            *slot.lock() = None;
            let status = processes.wait(worker.pid).unwrap_or(crate::EXIT_KILLED);
            return Err(Died { status });
        }
        if !stopping && platform.clock.uptime_ms().wrapping_sub(started) >= HANG_MS {
            stopping = true;
            let _ = processes.try_kill(worker.pid);
        }
        futex_wait_ms(&shared.done, done, POLL_MS);
    }
    // The worker has finished with `thunk`. Nothing may find this pointer again: a
    // request can also change for `finish`.
    // SAFETY: see `Shared::job`.
    unsafe { *shared.job.get() = None };
    match result {
        Some(value) => Ok(value),
        // It only returns without a result if the call was somehow never made.
        None => Err(Died { status: crate::EXIT_KILLED }),
    }
}

/// Ends a window's task, once it has nothing left to do for the window.
pub fn finish(window: usize) {
    let Some(slot) = SLOTS.get(window) else {
        return;
    };
    let worker = slot.lock().take();
    if let Some(worker) = worker {
        worker.shared.quit.store(true, Ordering::Release);
        // A change to the word, so that a task just about to wait on it doesn't. It finds
        // no job to do: `run` took that away when the last call ended.
        worker.shared.request.fetch_add(1, Ordering::Release);
        futex_wake(&worker.shared.request, usize::MAX);
    }
}
