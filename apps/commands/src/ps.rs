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

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let mut processes = [ProcessInfo::EMPTY; MAX_PROCESSES];
        let count = context.platform.processes.snapshot(&mut processes);
        let current = context.platform.processes.current();

        println!("  PID  PPID  STATE     STACK KiB  HEAP KiB  NAME");
        for process in &processes[..count] {
            let marker = if process.pid == current { '*' } else { ' ' };
            let detail = context.platform.processes.detail(process.pid);
            let state = match detail {
                Some(detail) if detail.kill_pending => "killing",
                Some(detail) if detail.stopped => "stopped",
                Some(detail) if detail.blocked => "sleeping",
                _ => state_name(process.state),
            };
            let detail = detail.unwrap_or_default();
            println!(
                "{marker} {:>4}  {:>4}  {:<8}  {:>4}/{:<4}  {:>8}  {}",
                process.pid,
                process.parent,
                state,
                detail.stack_used / 1024,
                detail.stack_size / 1024,
                detail.heap_bytes / 1024,
                process.name()
            );
        }
        0
    }
}

fn state_name(state: ProcessState) -> &'static str {
    match state {
        ProcessState::Ready => "ready",
        ProcessState::Running => "running",
        ProcessState::Exited => "exited",
    }
}
