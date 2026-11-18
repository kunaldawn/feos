//! `fsflush`: writes the shared tree's changes to disk about once a second.
//!
//! It starts at boot, with a writable tree: it only writes when something changed, so a
//! session that writes nothing has no disk traffic. (It used to start with the first
//! change, to save a slot of 32; now a task's stack is frames, and one that came and
//! stayed partway through would look like a leak to everything counting frames.) If it's
//! killed, the next change starts another. A crash loses what changed since the last
//! pass, but the filesystem stays consistent.
//!
//! Whether there's a flusher is asked of the scheduler, by its PID: a heartbeat alone
//! can't tell a killed flusher from one whose pass is slow (a sync waiting its turn at
//! the tree while a dying program's 64 MiB of mapped pages are written back, say), and
//! taking the second for the first started another flusher each time, which stayed.

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

use feos_app::{App, Context, Platform, WaitStatus};

const PERIOD_MS: u64 = 1000;
/// Beaten within this long, the flusher is certainly running, without asking.
const STALE_AFTER_MS: u64 = 5000;
/// Heartbeat value before any flusher has started.
const NEVER: u64 = u64::MAX;

/// No flusher has been started, or the last couldn't be.
const NO_FLUSHER: u32 = 0;
/// One is being started. The shell's PID, which is never a flusher's.
const STARTING: u32 = 1;

static PLATFORM: AtomicPtr<Platform> = AtomicPtr::new(ptr::null_mut());
static HEARTBEAT_MS: AtomicU64 = AtomicU64::new(NEVER);
/// The flusher's PID, [`NO_FLUSHER`] or [`STARTING`].
static FLUSHER_PID: AtomicU32 = AtomicU32::new(NO_FLUSHER);

/// The flusher process.
pub struct FsFlush;

static FLUSHER: FsFlush = FsFlush;

impl App for FsFlush {
    fn name(&self) -> &'static str {
        "fsflush"
    }

    fn usage(&self) -> &'static str {
        "fsflush"
    }

    fn description(&self) -> &'static str {
        "write filesystem changes to disk periodically"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let platform = context.platform;
        let Some(tree) = platform.filesystem.and_then(|fs| fs.tree()) else {
            return 0;
        };
        loop {
            HEARTBEAT_MS.store(platform.clock.uptime_ms(), Ordering::Release);
            platform.processes.sleep_ms(PERIOD_MS);
            // An error stays flagged in the tree, and the next pass tries again.
            let _ = tree.sync_if_dirty();
            if platform.processes.kill_requested() {
                HEARTBEAT_MS.store(NEVER, Ordering::Release);
                let _ = tree.sync();
                return feos_app::EXIT_KILLED;
            }
        }
    }
}

fn on_change() {
    let platform = PLATFORM.load(Ordering::Acquire);
    if platform.is_null() {
        return;
    }
    // SAFETY: only `install` stores this, from a `'static` reference.
    let platform: &'static Platform = unsafe { &*platform };
    let now = platform.clock.uptime_ms();
    let beat = HEARTBEAT_MS.load(Ordering::Acquire);
    if beat != NEVER && now.saturating_sub(beat) < STALE_AFTER_MS {
        return;
    }
    // Silent a while: gone, or only slow. A flusher still alive (or still being torn
    // down) is left to it.
    let pid = FLUSHER_PID.load(Ordering::Acquire);
    if pid == STARTING || (pid != NO_FLUSHER && platform.processes.try_wait(pid) == WaitStatus::Running) {
        return;
    }
    // Claim it, so that a burst of changes, or changes on several CPUs, start one.
    if FLUSHER_PID.compare_exchange(pid, STARTING, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    HEARTBEAT_MS.store(now, Ordering::Release);
    match platform.processes.spawn(&FLUSHER, "") {
        Ok(pid) => FLUSHER_PID.store(pid, Ordering::Release),
        Err(_) => {
            // No free slot, so nothing would ever flush this. Do it here, and try
            // for a flusher again with the next change.
            HEARTBEAT_MS.store(NEVER, Ordering::Release);
            FLUSHER_PID.store(NO_FLUSHER, Ordering::Release);
            if let Some(tree) = platform.filesystem.and_then(|fs| fs.tree()) {
                let _ = tree.sync();
            }
        }
    }
}

/// Starts the flusher, and arranges for another to start with a change if it's gone.
pub fn install(platform: &'static Platform) {
    PLATFORM.store(ptr::from_ref(platform).cast_mut(), Ordering::Release);
    if let Some(tree) = platform.filesystem.and_then(|fs| fs.tree()) {
        tree.set_change_hook(on_change);
        on_change();
    }
}

/// Writes out everything pending. For the moment before the power goes off.
pub fn sync_all(platform: &Platform) {
    if let Some(tree) = platform.filesystem.and_then(|fs| fs.tree()) {
        let _ = tree.sync();
    }
}
