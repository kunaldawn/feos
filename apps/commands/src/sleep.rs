//! `sleep`: block for a number of milliseconds.

use feos_app::{App, Context};
use feos_console::eprintln;

/// How long one wait lasts between looks for a kill.
const SLICE_MS: u64 = 100;

/// Sleeps for the duration given in its argument.
pub struct Sleep;

impl App for Sleep {
    fn name(&self) -> &'static str {
        "sleep"
    }

    fn usage(&self) -> &'static str {
        "sleep <ms>"
    }

    fn description(&self) -> &'static str {
        "wait for the given time"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let Some(mut remaining) = context.argv.first().and_then(|text| text.parse::<u64>().ok())
        else {
            eprintln!("usage: {}", self.usage());
            return 2;
        };
        let processes = context.platform.processes;
        while remaining > 0 {
            if processes.kill_requested() {
                return feos_app::EXIT_KILLED;
            }
            let slice = remaining.min(SLICE_MS);
            context.platform.clock.idle_ms(slice);
            remaining -= slice;
        }
        0
    }
}
