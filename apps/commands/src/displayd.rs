//! `displayd`: keeps a display that needs telling up to date.
//!
//! The desktop reports its damage directly. This catches what draws without
//! reporting: a full-screen app, or the splash. It only runs on a display that
//! needs it, so ramfb boots never see it.

use feos_app::{App, Context, Platform};

/// A full refresh ten times a second: fine for a text app, if not a video.
const PERIOD_MS: u64 = 100;

/// The refresh process.
pub struct DisplayRefresh;

static REFRESH: DisplayRefresh = DisplayRefresh;

impl App for DisplayRefresh {
    fn name(&self) -> &'static str {
        "displayd"
    }

    fn usage(&self) -> &'static str {
        "displayd"
    }

    fn description(&self) -> &'static str {
        "refresh a display that shows nothing until told"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let platform = context.platform;
        loop {
            platform.processes.sleep_ms(PERIOD_MS);
            if platform.processes.kill_requested() {
                return feos_app::EXIT_KILLED;
            }
            feos_hal::display::refresh();
        }
    }
}

/// Starts the refresh, on a display that needs one.
pub fn install(platform: &'static Platform) {
    if feos_hal::display::needs_refresh() {
        let _ = platform.processes.spawn(&REFRESH, "");
    }
}
