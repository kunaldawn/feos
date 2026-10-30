//! `kill`: stop a background process.

use feos_app::{App, Context};
use feos_console::println;

/// Stops a process by PID.
pub struct Kill;

impl App for Kill {
    fn name(&self) -> &'static str {
        "kill"
    }

    fn usage(&self) -> &'static str {
        "kill <pid>"
    }

    fn description(&self) -> &'static str {
        "stop a background process"
    }

    fn run(&self, context: &mut Context<'_>) {
        let Ok(pid) = context.args.trim().parse::<u32>() else {
            println!("usage: {}", self.usage());
            return;
        };

        if context.platform.processes.kill(pid) {
            println!("killed pid {pid}");
        } else {
            println!("cannot kill pid {pid} (missing, or it is the shell)");
        }
    }
}
