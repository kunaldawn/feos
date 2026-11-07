//! `kill`: stop a background process.

use feos_app::{App, Context, KillResult, Processes};
use feos_console::{eprintln, println};

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
        "kill [-KILL|-TERM|-HUP|-QUIT|-INT|-STOP|-CONT] <pid>..."
    }

    fn description(&self) -> &'static str {
        "stop a background process"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let processes = context.platform.processes;
        // `-STOP` and `-CONT` hold a job off the CPU and let it run again.
        let (signal, targets) = match context.argv.split_first() {
            Some((first, rest)) if first.starts_with('-') => (*first, rest),
            _ => ("-KILL", context.argv),
        };
        if targets.is_empty() {
            eprintln!("usage: {}", self.usage());
            return 2;
        }
        let mut status = 0;
        for target in targets {
            let Ok(pid) = target.parse::<u32>() else {
                eprintln!("kill: {target}: not a process id");
                status = 2;
                continue;
            };
            let done = match signal.trim_start_matches('-') {
                "9" | "KILL" | "SIGKILL" => kill(processes, pid),
                // A shell with a trap for one of these sees it; for anything else, they
                // end the process, and SIGKILL's the only way the kernel has of doing that.
                "1" | "HUP" | "SIGHUP" => catchable(processes, pid, feos_app::signal::HUP),
                "2" | "INT" | "SIGINT" => catchable(processes, pid, feos_app::signal::INT),
                "3" | "QUIT" | "SIGQUIT" => catchable(processes, pid, feos_app::signal::QUIT),
                "15" | "TERM" | "SIGTERM" => catchable(processes, pid, feos_app::signal::TERM),
                "19" | "STOP" | "SIGSTOP" => change(processes, pid, true),
                "18" | "CONT" | "SIGCONT" => change(processes, pid, false),
                _ => {
                    eprintln!("kill: {signal}: unknown signal");
                    return 2;
                }
            };
            if !done {
                status = 1;
            }
        }
        status
    }
}

fn catchable(processes: &dyn Processes, pid: u32, number: u32) -> bool {
    processes.signal(pid, number) || kill(processes, pid)
}

fn change(processes: &dyn Processes, pid: u32, stopped: bool) -> bool {
    let done = processes.set_stopped(pid, stopped);
    if !done {
        eprintln!("cannot signal pid {pid}: no such process, or it is the shell");
    }
    done
}

fn kill(processes: &dyn Processes, pid: u32) -> bool {
    let leaked_before = processes.leaked_heap_bytes();
    match processes.try_kill(pid) {
        KillResult::Killed => {
            report_killed(processes, pid, leaked_before);
            true
        }
        KillResult::Pending | KillResult::AlreadyPending => {
            if wait_for_exit(processes, pid) {
                report_killed(processes, pid, leaked_before);
            } else {
                println!("pid {pid} will exit when it releases its locks");
            }
            true
        }
        KillResult::NotFound => {
            eprintln!("cannot kill pid {pid}: no such process");
            false
        }
        KillResult::Protected => {
            eprintln!("cannot kill pid {pid}: it is the shell");
            false
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
