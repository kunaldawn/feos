//! `spawn`: run a registered app as a background process.

use feos_app::{App, Context};
use feos_console::println;

/// Starts another app in its own process and returns to the prompt.
pub struct Spawn;

impl App for Spawn {
    fn name(&self) -> &'static str {
        "spawn"
    }

    fn usage(&self) -> &'static str {
        "spawn <command> [args]"
    }

    fn description(&self) -> &'static str {
        "run a command in the background"
    }

    fn run(&self, context: &mut Context<'_>) {
        let text = context.args.trim();
        if text.is_empty() {
            println!("usage: {}", self.usage());
            println!("type 'help' to list commands");
            return;
        }

        let (command, args) = match text.split_once(' ') {
            Some((command, args)) => (command, args.trim()),
            None => (text, ""),
        };

        let Some(app) = crate::find(command) else {
            println!("unknown command: {command} (type 'help')");
            return;
        };

        match context.platform.processes.spawn(app, args) {
            Ok(pid) => println!("started pid {pid}: {command}"),
            Err(feos_app::SpawnError::NoFreeSlot) => {
                println!(
                    "no free process slot (at most {} background processes)",
                    feos_app::MAX_PROCESSES - 1
                );
            }
            Err(feos_app::SpawnError::ArgsTooLong) => {
                println!(
                    "arguments too long (max {} bytes)",
                    feos_app::PROCESS_ARGS_MAX
                );
            }
        }
    }
}
