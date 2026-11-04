//! What happens when the kernel can't continue.
//!
//! The policy is the same for every panic and CPU fault, on every port and on
//! any CPU: **the whole machine stops**. Processes share one address space and
//! can hold each other's locks, so nothing would be safe to keep running after
//! one of them has failed, and a half-alive system hides the failure.
//!
//! The sequence is: mask this CPU's interrupts, stop every other CPU with an
//! IPI, print one report, and halt. Only the first failure gets to report. A
//! second one, on any CPU, including one raised while printing the report, halts
//! quietly.

use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use feos_hal::{Board, IpiTarget};

use crate::arch::{cpu, exceptions, irq, timer};
use crate::{sched, Machine};

/// How long to wait for other CPUs to stop. One may have interrupts masked, so
/// the stop has to be allowed to time out, but the report shouldn't interleave
/// with output from a CPU that's about to take the IPI.
const STOP_WAIT_MS: u64 = 50;

static CLAIMED: AtomicBool = AtomicBool::new(false);
static STOPPED_CPUS: AtomicUsize = AtomicUsize::new(0);
/// The CPU writing the report, plus one.
static REPORTER: AtomicUsize = AtomicUsize::new(0);

/// Stops the rest of the machine, or this CPU if another failure got here first.
fn begin() {
    irq::disable();
    if CLAIMED.swap(true, Ordering::AcqRel) {
        if REPORTER.load(Ordering::Acquire) == cpu::index() + 1 {
            let _ = writeln!(Report, "\n[fatal] failed while reporting; system halted");
        }
        cpu::halt();
    }
    REPORTER.store(cpu::index() + 1, Ordering::Release);

    let others = sched::online_cpus().saturating_sub(1);
    if others == 0 {
        return;
    }
    Machine::interrupts().send_ipi(IpiTarget::AllOthers, Machine::IPI_STOP);

    let start = timer::ticks();
    let wait = timer::frequency() / 1000 * STOP_WAIT_MS;
    while STOPPED_CPUS.load(Ordering::Acquire) < others
        && timer::ticks().wrapping_sub(start) < wait
    {
        core::hint::spin_loop();
    }
}

/// Handles the stop IPI on a CPU that isn't reporting.
pub fn stop_this_cpu() -> ! {
    STOPPED_CPUS.fetch_add(1, Ordering::AcqRel);
    cpu::halt()
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
        cpu::index(),
    );
}

/// Reports a CPU exception and stops the machine.
pub fn on_exception(info: &exceptions::FaultInfo) {
    begin();
    let _ = write!(
        Report,
        "\n[fatal] {} / {} (vector {})\n\
         [fatal] error={:#018x} class={:#04x} ({})\n\
         [fatal] ip={:#018x} fault address={:#018x}\n\
         [fatal] flags={:#018x} sp={:#018x}\n",
        info.vector_group(),
        info.vector_type(),
        info.index,
        info.esr,
        info.exception_class(),
        info.syndrome_name(),
        info.elr,
        info.far,
        info.spsr,
        info.sp,
    );
    let _ = info.write_registers(&mut Report);
    report_location();
    cpu::halt()
}

/// Reports a panic and stops the machine.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    begin();
    let _ = writeln!(Report, "\n[fatal] panic: {info}");
    report_location();
    cpu::halt()
}
