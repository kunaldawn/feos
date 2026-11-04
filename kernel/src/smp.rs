//! Multi-core startup.
//!
//! The boot CPU finishes all one-time initialization and then asks the
//! architecture to start each remaining CPU. On AArch64 that is a PSCI
//! `CPU_ON`; the x86_64 port currently runs on the boot processor only and
//! reports zero secondaries.

use crate::arch;
use crate::board;

/// Scheduler tick rate, shared by every CPU.
pub const TICK_HZ: u64 = 100;

/// Starts the secondary CPUs the machine exposes.
///
/// Returns the number of CPUs firmware accepted and, when a request failed,
/// the raw status code. AArch64 numbers its CPUs contiguously, so the first
/// failure marks the end of the available range (for example `-smp 1` rejects
/// CPU 1 and the loop stops).
#[cfg(target_arch = "aarch64")]
pub fn start_secondaries() -> (usize, Option<i64>) {
    arch::smp::set_handler(secondary_main);

    let entry = arch::smp::entry_address() as u64;
    let limit = board::MAX_CPUS.min(arch::smp::MAX_CPUS);
    let mut started = 0;
    let mut error = None;

    for cpu in 1..limit {
        match arch::psci::cpu_on(board::cpu_mpidr(cpu), entry, cpu as u64) {
            Ok(()) => started += 1,
            Err(code) => {
                error = Some(code);
                break;
            }
        }
    }

    (started, error)
}

/// Starts the secondary CPUs on x86_64.
///
/// The current port has no AP trampoline yet, so the boot processor is the
/// only scheduler CPU.
#[cfg(target_arch = "x86_64")]
pub fn start_secondaries() -> (usize, Option<i64>) {
    let _ = board::MAX_CPUS;
    (0, None)
}

/// Runs on a secondary CPU after its stack and vectors are ready.
#[allow(dead_code)]
fn secondary_main(cpu: usize) -> ! {
    if cpu >= board::MAX_CPUS || cpu >= arch::smp::MAX_CPUS {
        // The assembly entry already used a stack sized for MAX_CPUS; an
        // out-of-range context id would mean the caller misconfigured the
        // firmware start.
        arch::cpu::halt()
    }

    // GICC_* registers and the timer PPI enable bit are banked per CPU.
    board::init_cpu_interrupts();
    crate::sched::start_cpu(TICK_HZ)
}
