//! `tick`: print periodic progress, useful for demonstrating scheduling.
//!
//! Run it in the background with `spawn tick 10 250`, then keep using the
//! shell: it sleeps between ticks, so both processes make progress. A third
//! argument of `busy` spins instead, which keeps a CPU occupied and shows the
//! timer preempting it.

use feos_app::{App, Context};
use feos_console::println;

const DEFAULT_COUNT: u64 = 8;
const DEFAULT_DELAY_MS: u64 = 250;

/// Prints `count` ticks separated by a delay.
pub struct Tick;

impl App for Tick {
    fn name(&self) -> &'static str {
        "tick"
    }

    fn usage(&self) -> &'static str {
        "tick [count] [ms] [busy]"
    }

    fn description(&self) -> &'static str {
        "print periodic ticks (try: spawn tick 10 250)"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let mut parts = context.argv.iter().copied();
        let count = parts
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_COUNT)
            .clamp(1, 1000);
        let delay = parts
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_DELAY_MS)
            .clamp(10, 10_000);
        let busy = parts.next() == Some("busy");

        let pid = context.platform.processes.current();
        for tick in 1..=count {
            if context.platform.processes.kill_requested() {
                return feos_app::EXIT_KILLED;
            }
            println!("[pid {pid}] tick {tick}/{count}");
            if busy {
                context.platform.clock.spin_ms(delay);
            } else {
                context.platform.clock.idle_ms(delay);
            }
        }
        println!("[pid {pid}] done");
        0
    }
}
