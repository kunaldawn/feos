//! `uptime`: milliseconds since the generic counter started.

use feos_app::{App, Context};
use feos_console::println;

/// Prints the kernel uptime.
pub struct Uptime;

impl App for Uptime {
    fn name(&self) -> &'static str {
        "uptime"
    }

    fn usage(&self) -> &'static str {
        "uptime"
    }

    fn description(&self) -> &'static str {
        "milliseconds since boot"
    }

    fn run(&self, context: &mut Context<'_>) {
        println!("uptime: {} ms", context.platform.clock.uptime_ms());
    }
}
