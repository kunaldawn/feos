//! `sleep`: busy-wait for a number of milliseconds.

use feos_app::{App, Context};
use feos_console::println;

/// Spins for the duration given in its argument.
pub struct Sleep;

impl App for Sleep {
    fn name(&self) -> &'static str {
        "sleep"
    }

    fn usage(&self) -> &'static str {
        "sleep <ms>"
    }

    fn description(&self) -> &'static str {
        "busy-wait for the given time"
    }

    fn run(&self, context: &mut Context<'_>) {
        match context.args.parse::<u64>() {
            Ok(ms) => {
                context.platform.clock.spin_ms(ms);
                println!("slept for {ms} ms");
            }
            Err(_) => println!("usage: {}", self.usage()),
        }
    }
}
