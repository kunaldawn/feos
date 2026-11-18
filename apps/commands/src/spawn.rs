//! `spawn`: run a registered app as a background process.

use feos_app::{App, Context, SpawnError, SpawnIo};
use feos_console::{eprintln, println};

/// Starts another app in its own process and returns to the prompt.
pub struct Spawn;

impl App for Spawn {
    fn name(&self) -> &'static str {
        "spawn"
    }

    fn usage(&self) -> &'static str {
        "spawn [count] <command> [args]"
    }

    fn description(&self) -> &'static str {
        "run a command in the background"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        // A number first is how many to start, as long as a command follows it.
        let (count, argv) = match context.argv.split_first() {
            Some((first, rest)) if !rest.is_empty() => match first.parse::<usize>() {
                Ok(count) => (count, rest),
                Err(_) => (1, context.argv),
            },
            _ => (1, context.argv),
        };
        let Some((command, args)) = argv.split_first() else {
            eprintln!("usage: {}", self.usage());
            eprintln!("type 'help' to list commands");
            return 2;
        };

        let Some(app) = context.platform.apps.find(command) else {
            eprintln!("unknown command: {command} (type 'help')");
            return 127;
        };

        for _ in 0..count {
            let status = spawn_one(context, app, command, args);
            if status != 0 {
                return status;
            }
        }
        0
    }
}

/// Starts one copy of `app` in the background.
fn spawn_one(context: &Context<'_>, app: &'static dyn App, command: &str, args: &[&str]) -> i32 {
    let io = SpawnIo {
        background: true,
        ..SpawnIo::default()
    };
    match context.platform.processes.spawn_argv(app, args, io) {
        Ok(pid) => {
            println!("started pid {pid}: {command}");
            0
        }
        Err(error) => {
            match error {
                SpawnError::NoFreeSlot => eprintln!(
                    "no free process slot (at most {} background processes)",
                    feos_app::MAX_PROCESSES - 1
                ),
                SpawnError::ArgsTooLong => {
                    eprintln!("arguments too long (max {} bytes)", feos_app::PROCESS_ARGS_MAX)
                }
                SpawnError::Unsupported => eprintln!("spawn: not supported here"),
                SpawnError::NoMemory => eprintln!("spawn: out of memory"),
            }
            1
        }
    }
}
