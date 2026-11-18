//! `spin`: keep a task busy until it's killed, for filling the scheduler's table.
//!
//! `spawn 200 spin` starts two hundred of them. Each gives up its CPU at every turn, so
//! however many there are, the shell's turn comes round quickly.

use feos_app::{App, Context};

/// Runs, offering the CPU at every turn, until killed or for the milliseconds given.
pub struct Spin;

impl App for Spin {
    fn name(&self) -> &'static str {
        "spin"
    }

    fn usage(&self) -> &'static str {
        "spin [ms]"
    }

    fn description(&self) -> &'static str {
        "stay runnable until killed (try: spawn 200 spin)"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let limit = context.argv.first().and_then(|value| value.parse::<u64>().ok());
        let clock = context.platform.clock;
        let start = clock.ticks();
        let per_ms = (clock.frequency() / 1000).max(1);
        loop {
            if context.platform.processes.kill_requested() {
                return feos_app::EXIT_KILLED;
            }
            if limit.is_some_and(|ms| clock.ticks().wrapping_sub(start) / per_ms >= ms) {
                return 0;
            }
            context.platform.processes.yield_now();
        }
    }
}
