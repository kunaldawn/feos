//! Multi-core startup.
//!
//! The boot CPU finishes all one-time initialization and then asks PSCI to
//! start each remaining CPU at the architecture crate's secondary entry point.
//! The entry stub gives the core a private stack and exception vectors; the
//! handler below configures that core's GIC interface and joins the scheduler.

use feos_arch_aarch64::{psci, smp};
use feos_board_qemu_virt as board;

/// Scheduler tick rate, shared by every CPU.
pub const TICK_HZ: u64 = 100;

/// Starts the secondary CPUs the machine exposes.
///
/// Returns the number of CPUs firmware accepted and, when a request failed,
/// the raw PSCI status code. QEMU numbers its CPUs contiguously, so the first
/// failure marks the end of the available range (for example `-smp 1` rejects
/// CPU 1 and the loop stops).
pub fn start_secondaries() -> (usize, Option<i64>) {
    smp::set_handler(secondary_main);

    let entry = smp::entry_address() as u64;
    let limit = board::MAX_CPUS.min(smp::MAX_CPUS);
    let mut started = 0;
    let mut error = None;

    for cpu in 1..limit {
        match psci::cpu_on(board::cpu_mpidr(cpu), entry, cpu as u64) {
            Ok(()) => started += 1,
            Err(code) => {
                error = Some(code);
                break;
            }
        }
    }

    (started, error)
}

/// Runs on a secondary CPU after its stack and vector table are ready.
fn secondary_main(cpu: usize) -> ! {
    if cpu >= board::MAX_CPUS || cpu >= smp::MAX_CPUS {
        // The assembly entry already used a stack sized for MAX_CPUS; an
        // out-of-range context id would mean the caller misconfigured PSCI.
        feos_arch_aarch64::cpu::halt()
    }

    // GICC_* registers and the timer PPI enable bit are banked per CPU.
    board::init_cpu_interrupts();
    crate::sched::start_cpu(TICK_HZ)
}
