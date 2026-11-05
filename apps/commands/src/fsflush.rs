//! `fsflush`: writes the shared tree's changes to disk about once a second.
//!
//! It starts with the first change, not at boot, so a session that writes nothing
//! has one more free process slot and no disk traffic. A crash loses what changed
//! since the last pass, but the filesystem stays consistent.

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use feos_app::{App, Context, Platform};

const PERIOD_MS: u64 = 1000;
/// Silent for this long, the flusher isn't running: someone killed it.
const STALE_AFTER_MS: u64 = 5000;
/// Heartbeat value before any flusher has started.
const NEVER: u64 = u64::MAX;

static PLATFORM: AtomicPtr<Platform> = AtomicPtr::new(ptr::null_mut());
static HEARTBEAT_MS: AtomicU64 = AtomicU64::new(NEVER);

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
    // Claim it now, so that a burst of changes doesn't start several.
    HEARTBEAT_MS.store(now, Ordering::Release);
    if platform.processes.spawn(&FLUSHER, "").is_err() {
        // No free slot, so nothing would ever flush this. Do it here, and try
        // for a flusher again with the next change.
        HEARTBEAT_MS.store(NEVER, Ordering::Release);
        if let Some(tree) = platform.filesystem.and_then(|fs| fs.tree()) {
            let _ = tree.sync();
        }
    }
}

/// Arranges for the flusher to start with the tree's first change.
pub fn install(platform: &'static Platform) {
    PLATFORM.store(ptr::from_ref(platform).cast_mut(), Ordering::Release);
    if let Some(tree) = platform.filesystem.and_then(|fs| fs.tree()) {
        tree.set_change_hook(on_change);
    }
}

/// Writes out everything pending. For the moment before the power goes off.
pub fn sync_all(platform: &Platform) {
    if let Some(tree) = platform.filesystem.and_then(|fs| fs.tree()) {
        let _ = tree.sync();
    }
}
