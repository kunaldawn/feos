//! `kill`: stop a background process.

use feos_app::{App, Context, KillResult, Processes};
use feos_console::println;

/// How long to wait for a flagged process to reach its next safe point before
/// reporting that it's still going.
const WAIT_MS: u64 = 500;
const POLL_MS: u64 = 10;

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

        let processes = context.platform.processes;
        let leaked_before = processes.leaked_heap_bytes();
        match processes.try_kill(pid) {
            KillResult::Killed => report_killed(processes, pid, leaked_before),
            KillResult::Pending | KillResult::AlreadyPending => {
                if wait_for_exit(processes, pid) {
                    report_killed(processes, pid, leaked_before);
                } else {
                    println!("pid {pid} will exit when it releases its locks");
                }
            }
            KillResult::NotFound => println!("cannot kill pid {pid}: no such process"),
            KillResult::Protected => println!("cannot kill pid {pid}: it is the shell"),
        }
    }
}

/// Waits for `pid` to disappear from the process table.
fn wait_for_exit(processes: &dyn Processes, pid: u32) -> bool {
    let mut waited = 0;
    while waited < WAIT_MS {
        if processes.detail(pid).is_none() {
            return true;
        }
        processes.sleep_ms(POLL_MS);
        waited += POLL_MS;
    }
    processes.detail(pid).is_none()
}

fn report_killed(processes: &dyn Processes, pid: u32, leaked_before: usize) {
    let leaked = processes.leaked_heap_bytes().saturating_sub(leaked_before);
    if leaked > 0 {
        println!("killed pid {pid} (it left {leaked} heap bytes behind)");
    } else {
        println!("killed pid {pid}");
    }
}
