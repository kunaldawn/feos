//! `ps`: list processes.

use feos_app::{App, Context, ProcessSnapshot, ProcessState};
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
        // On the heap: a full table is 72 KiB, too much for a task's stack.
        let Some(processes) = ProcessSnapshot::take(context.platform.processes) else {
            println!("ps: out of memory");
            return 1;
        };
        let current = context.platform.processes.current();

        println!("  PID  PPID  STATE     THR  STACK KiB  HEAP KiB  NAME");
        for process in processes.iter() {
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
                "{marker} {:>4}  {:>4}  {:<8}  {:>3}  {:>4}/{:<4}  {:>8}  {}",
                process.pid,
                process.parent,
                state,
                process.threads,
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
