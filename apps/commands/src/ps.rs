//! `ps`: list processes.

use feos_app::{App, Context, ProcessInfo, ProcessState, MAX_PROCESSES};
use feos_console::println;

/// Prints the process table.
pub struct Ps;

impl App for Ps {
    fn name(&self) -> &'static str {
        "ps"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["procs", "processes"]
    }

    fn usage(&self) -> &'static str {
        "ps"
    }

    fn description(&self) -> &'static str {
        "list processes and their state"
    }

    fn run(&self, context: &mut Context<'_>) {
        let mut processes = [ProcessInfo::EMPTY; MAX_PROCESSES];
        let count = context.platform.processes.snapshot(&mut processes);
        let current = context.platform.processes.current();

        println!("  PID  STATE    NAME");
        for process in &processes[..count] {
            let marker = if process.pid == current { '*' } else { ' ' };
            println!(
                "{marker} {:>4}  {:<7}  {}",
                process.pid,
                state_name(process.state),
                process.name()
            );
        }
    }
}

fn state_name(state: ProcessState) -> &'static str {
    match state {
        ProcessState::Ready => "ready",
        ProcessState::Running => "running",
        ProcessState::Exited => "exited",
    }
}
