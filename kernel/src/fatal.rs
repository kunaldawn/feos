//! What happens when something fails.
//!
//! The policy is the same for every panic and CPU fault, on every port and on
//! any CPU. A failure in an ordinary task, which holds no lock and wasn't masking
//! interrupts, **ends that task**: see [`sched::contain_failure`]. A failure
//! anywhere else, in the boot task, an interrupt handler or with a lock held,
//! **stops the whole machine**: it could have left anything inconsistent, and a
//! half-alive system hides the failure.
//!
//! For a kernel thread, ending a task isn't full isolation. They share one address
//! space, so one that scribbled on someone else's memory before it faulted has done
//! it, and nothing can tell. What it held under a lock is safe; what it allocated stays
//! allocated, and is reported as a leak. A user program's fault isn't decided here at
//! all: the trap hands it to a kernel thread on the task's own stack, which gives the
//! program its page or ends it (see [`sched::on_user_fault`]). Only a fault from user
//! mode with the task not there, which can't happen, stops the machine.
//!
//! Stopping the machine is:
//! mask this CPU's interrupts, stop every other CPU with an
//! IPI, then, for a CPU with interrupts masked, with an NMI where the machine has
//! them, print one report, and halt. Only the first failure gets to report. A
//! second one, on any CPU, including one raised while printing the report, halts
//! quietly.

use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use feos_hal::{Arch, Board, Clock, FaultDetails, IpiTarget};

use crate::{sched, Fault, Frame, Machine, Port};

/// How long to wait for other CPUs to stop. One may have interrupts masked, so
/// the stop has to be allowed to time out, but the report shouldn't interleave
/// with output from a CPU that's about to take the IPI.
const STOP_WAIT_MS: u64 = 50;

static CLAIMED: AtomicBool = AtomicBool::new(false);
/// CPUs that have stopped, as a bit each: an NMI can reach one that an IPI did too.
static STOPPED_CPUS: AtomicU64 = AtomicU64::new(0);
/// The CPU writing the report, plus one.
static REPORTER: AtomicUsize = AtomicUsize::new(0);

/// Stops the rest of the machine, or this CPU if another failure got here first.
fn begin() {
    Port::irq_disable();
    if CLAIMED.swap(true, Ordering::AcqRel) {
        if REPORTER.load(Ordering::Acquire) == Port::cpu_index() + 1 {
            let _ = writeln!(Report, "\n[fatal] failed while reporting; system halted");
        }
        // An NMI that lands here is how a CPU with interrupts masked stops.
        stop_this_cpu();
    }
    REPORTER.store(Port::cpu_index() + 1, Ordering::Release);

    let others = sched::online_cpus().saturating_sub(1);
    if others == 0 {
        return;
    }
    Machine::interrupts().send_ipi(IpiTarget::AllOthers, Machine::IPI_STOP);
    let mut stopped = wait_for_stop(others);
    if !stopped && Machine::interrupts().send_stop_nmi() {
        stopped = wait_for_stop(others);
    }
    let count = stopped_cpus();
    let verdict = if stopped { "" } else { " (the rest have interrupts masked)" };
    let _ = writeln!(Report, "\n[fatal] stopped {count} of {others} other CPU(s){verdict}");
}

/// Waits a moment for `others` CPUs to report they've stopped.
fn wait_for_stop(others: usize) -> bool {
    let start = Port::services().ticks();
    let wait = Port::services().frequency() / 1000 * STOP_WAIT_MS;
    while stopped_cpus() < others {
        if Port::services().ticks().wrapping_sub(start) >= wait {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

fn stopped_cpus() -> usize {
    STOPPED_CPUS.load(Ordering::Acquire).count_ones() as usize
}

/// Handles the stop IPI on a CPU that isn't reporting.
pub fn stop_this_cpu() -> ! {
    STOPPED_CPUS.fetch_or(1 << Port::cpu_index(), Ordering::AcqRel);
    Port::halt()
}

/// Writes straight to the board's UART.
///
/// This avoids the console's lock, which the failed code may hold, and its
/// function pointers, which a stack overflow may have overwritten: they're in
/// `.bss`, which is where an overflowing boot stack goes.
struct Report;

impl Write for Report {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        Machine::write_emergency(text.as_bytes());
        Ok(())
    }
}

fn report_location() {
    let mut name = [0u8; feos_app::PROCESS_NAME_MAX];
    let (pid, len) = sched::current_identity(&mut name);
    // The name is unavailable if this failure happened under the scheduler lock.
    let name = core::str::from_utf8(&name[..len]).unwrap_or("?");
    let separator = if name.is_empty() { "" } else { " " };
    let _ = writeln!(
        Report,
        "[fatal] cpu {}, pid {pid}{separator}{name}; system halted",
        Port::cpu_index(),
    );
}

/// Whether the machine has started to stop.
pub fn is_stopping() -> bool {
    CLAIMED.load(Ordering::Acquire)
}

/// Reports a task ended because its stack is gone wrong. Called under the
/// scheduler's lock, and writes around every other lock.
pub fn report_stack_overflow(pid: u32, name: &str, sp: usize, bottom: usize, top: usize) {
    let _ = writeln!(
        Report,
        "\n[fault] pid {pid} ({name}): stack overflow, sp={sp:#x}, stack {bottom:#x}..{top:#x}; ended"
    );
}

/// Writes which task failed, for a report of a failure it alone pays for.
fn report_task() {
    let mut name = [0u8; feos_app::PROCESS_NAME_MAX];
    let (pid, len) = sched::current_identity(&mut name);
    let name = core::str::from_utf8(&name[..len]).unwrap_or("?");
    let _ = write!(Report, "\n[fault] pid {pid} ({name}) on cpu {}: ", Port::cpu_index());
}

/// Takes a CPU exception: ends the task if that's safe, and otherwise reports it
/// and stops the machine. Returns `true` having replaced the frame.
pub fn on_exception(info: &Fault, frame: &mut Frame) -> bool {
    // A program's own doing is the program's business, decided with interrupts on: a
    // page it was promised, or the end of it.
    if info.from_user() && info.is_task_fault() {
        let fault = sched::UserFault {
            address: info.fault_address(),
            name: info.syndrome_name(),
            instruction: info.instruction(),
        };
        if sched::on_user_fault(fault, frame) {
            return true;
        }
        begin();
        let _ = writeln!(Report, "\n[fatal] a fault from user mode, from a task that wasn't there");
        let _ = info.write_report(&mut Report);
        report_location();
        Port::halt()
    }
    let overflow = info.fault_address().and_then(sched::stack_overflow_at);
    if info.is_task_fault() && frame.interrupts_were_enabled() && sched::contain_failure() {
        // The task holds no lock, so this one can be waited for, and the machine carries
        // on: what others are printing shouldn't end up in the middle of this, or this in it.
        let console = feos_console::hold();
        report_task();
        match info.fault_address() {
            Some(address) if overflow.is_some() => {
                let _ = writeln!(Report, "stack overflow into the guard page at {address:#x}; ended");
            }
            Some(address) => {
                let _ = writeln!(
                    Report,
                    "{} at {address:#x}, ip={:#x}; ended",
                    info.syndrome_name(),
                    info.instruction()
                );
            }
            None => {
                let _ = writeln!(Report, "{}, ip={:#x}; ended", info.syndrome_name(), info.instruction());
            }
        }
        drop(console);
        sched::end_task_from_trap(frame);
        return true;
    }

    begin();
    if overflow.is_some() {
        let _ = writeln!(Report, "\n[fatal] stack overflow into a guard page");
    }
    let _ = writeln!(Report);
    let _ = info.write_report(&mut Report);
    report_location();
    Port::halt()
}

/// Reports a user program's fault that ends it. Called in its kernel thread, with
/// interrupts on, so the console can be waited for.
pub fn report_user_fault(fault: &sched::UserFault) {
    let console = feos_console::hold();
    report_task();
    match fault.address {
        Some(address) => {
            let _ = writeln!(Report, "{} at {address:#x}, ip={:#x}; ended", fault.name, fault.instruction);
        }
        None => {
            let _ = writeln!(Report, "{}, ip={:#x}; ended", fault.name, fault.instruction);
        }
    }
    drop(console);
}

/// Ends the task if that's safe, and otherwise reports the panic and stops the
/// machine.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    if Port::irqs_enabled() && sched::contain_failure() {
        let console = feos_console::hold();
        report_task();
        let _ = writeln!(Report, "{info}; ended");
        drop(console);
        sched::end_task();
    }
    begin();
    let _ = writeln!(Report, "\n[fatal] panic: {info}");
    report_location();
    Port::halt()
}
