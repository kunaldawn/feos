//! `kill`: send a process a signal, `SIGKILL` unless another is named.
//!
//! A user program takes the signal's default action, as `KILL` from a program would
//! give it: `-TERM` ends it with `SIGTERM` (status 143), `-STOP` and `-CONT` stop and
//! continue it, which its parent sees. A kernel-linked app catches what its shell has a
//! trap for, and is otherwise killed by anything that would end it.

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
        "kill [-SIGNAL|-NUMBER] <pid>...   (SIGNAL: KILL TERM HUP INT QUIT USR1 USR2 ALRM PIPE STOP TSTP CONT ...)"
    }

    fn description(&self) -> &'static str {
        "send a process a signal"
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
            let Some(number) = signal_number(signal.trim_start_matches('-')) else {
                eprintln!("kill: {signal}: unknown signal");
                return 2;
            };
            let done = match number {
                // A shell with a trap for one of these sees it; anything else takes the
                // signal's default action.
                HUP | INT | QUIT | TERM => catchable(processes, pid, number),
                STOP | TSTP | TTIN | TTOU => change(processes, pid, number),
                CONT => change(processes, pid, number),
                CHLD | URG | WINCH => change(processes, pid, number),
                _ => kill(processes, pid, number),
            };
            if !done {
                status = 1;
            }
        }
        status
    }
}

const HUP: u32 = 1;
const INT: u32 = 2;
const QUIT: u32 = 3;
const TERM: u32 = 15;
const CHLD: u32 = 17;
const CONT: u32 = 18;
const STOP: u32 = 19;
const TSTP: u32 = 20;
const TTIN: u32 = 21;
const TTOU: u32 = 22;
const URG: u32 = 23;
const WINCH: u32 = 28;

/// Signals by name, with or without `SIG`: Linux's numbers.
const NAMES: [(&str, u32); 31] = [
    ("HUP", 1), ("INT", 2), ("QUIT", 3), ("ILL", 4), ("TRAP", 5), ("ABRT", 6), ("BUS", 7), ("FPE", 8),
    ("KILL", 9), ("USR1", 10), ("SEGV", 11), ("USR2", 12), ("PIPE", 13), ("ALRM", 14), ("TERM", 15),
    ("STKFLT", 16), ("CHLD", 17), ("CONT", 18), ("STOP", 19), ("TSTP", 20), ("TTIN", 21), ("TTOU", 22),
    ("URG", 23), ("XCPU", 24), ("XFSZ", 25), ("VTALRM", 26), ("PROF", 27), ("WINCH", 28), ("IO", 29),
    ("PWR", 30), ("SYS", 31),
];

/// The signal `name` names: a number from 1 to 64, or a name from [`NAMES`].
fn signal_number(name: &str) -> Option<u32> {
    if let Ok(number) = name.parse::<u32>() {
        return (1..=64).contains(&number).then_some(number);
    }
    let name = name.strip_prefix("SIG").unwrap_or(name);
    NAMES.iter().find(|(known, _)| known.eq_ignore_ascii_case(name)).map(|&(_, number)| number)
}

fn catchable(processes: &dyn Processes, pid: u32, number: u32) -> bool {
    processes.signal(pid, number) || kill(processes, pid, number)
}

/// A signal that doesn't end what it's sent to: a stop, a continue, or one that's
/// ignored.
fn change(processes: &dyn Processes, pid: u32, number: u32) -> bool {
    let done = matches!(processes.send_signal(pid, number), KillResult::Killed | KillResult::Pending | KillResult::AlreadyPending);
    if !done {
        eprintln!("cannot signal pid {pid}: no such process, or it is the shell");
    }
    done
}

/// A signal that ends what it's sent to, `SIGKILL` or another: waits a moment for it to.
fn kill(processes: &dyn Processes, pid: u32, number: u32) -> bool {
    let leaked_before = processes.leaked_heap_bytes();
    match processes.send_signal(pid, number) {
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
