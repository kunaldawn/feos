//! `renice` and `taskset`: change how and where a process is scheduled.

use feos_app::{App, Context, PRIORITY_MAX};
use feos_console::{eprintln, println};

/// Sets a process's priority.
pub struct Renice;

impl App for Renice {
    fn name(&self) -> &'static str {
        "renice"
    }

    fn usage(&self) -> &'static str {
        "renice <priority> <pid>"
    }

    fn description(&self) -> &'static str {
        "set a process's priority, 0 to 15, larger being more urgent"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let &[priority, pid] = context.argv else {
            eprintln!("usage: {}", self.usage());
            return 2;
        };
        let (Ok(priority), Ok(pid)) = (priority.parse::<u8>(), pid.parse::<u32>()) else {
            eprintln!("usage: {}", self.usage());
            return 2;
        };
        if priority > PRIORITY_MAX {
            eprintln!("renice: {priority}: the highest priority is {PRIORITY_MAX}");
            return 2;
        }
        if !context.platform.processes.set_priority(pid, priority) {
            eprintln!("renice: {pid}: no such process");
            return 1;
        }
        println!("pid {pid}: priority {priority}");
        0
    }
}

/// Sets which CPUs a process may run on.
pub struct Taskset;

impl App for Taskset {
    fn name(&self) -> &'static str {
        "taskset"
    }

    fn usage(&self) -> &'static str {
        "taskset <cpu mask> <pid>"
    }

    fn description(&self) -> &'static str {
        "set which CPUs a process may run on, a bit each, in decimal or 0x hex"
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        let &[mask, pid] = context.argv else {
            eprintln!("usage: {}", self.usage());
            return 2;
        };
        let mask = match mask.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16),
            None => mask.parse::<u32>(),
        };
        let (Ok(mask), Ok(pid)) = (mask, pid.parse::<u32>()) else {
            eprintln!("usage: {}", self.usage());
            return 2;
        };
        if !context.platform.processes.set_affinity(pid, mask) {
            eprintln!("taskset: pid {pid}, mask {mask:#x}: no such process, or no such CPU");
            return 1;
        }
        println!("pid {pid}: CPUs {mask:#x}");
        0
    }
}
