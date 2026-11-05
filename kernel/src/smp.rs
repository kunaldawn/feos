//! Multi-core startup.
//!
//! The boot CPU finishes all one-time initialization and then asks the
//! architecture to start each remaining CPU. On AArch64 that is a PSCI
//! `CPU_ON`; on x86_64, INIT and startup IPIs through the local APIC.

use feos_hal::{Arch, Board};

use crate::{Machine, Port};

/// Scheduler tick rate, shared by every CPU.
pub const TICK_HZ: u64 = 100;

/// Starts the secondary CPUs the machine exposes.
///
/// Returns the number of CPUs firmware accepted and, when a request failed,
/// the raw status code.
pub fn start_secondaries() -> (usize, Option<i64>) {
    Port::start_secondaries(Machine::cpu_count(), Machine::cpu_mpidr, secondary_main)
}

/// Runs on a secondary CPU after its stack and vectors are ready.
fn secondary_main(cpu: usize) -> ! {
    if cpu >= Machine::MAX_CPUS || cpu >= Port::MAX_CPUS {
        // The assembly entry already used a stack sized for MAX_CPUS; an
        // out-of-range context id would mean the caller misconfigured the
        // firmware start.
        Port::halt()
    }

    // Per-CPU interrupt-controller state, such as GICC_* and the timer PPI.
    Machine::init_cpu_interrupts();
    crate::sched::start_cpu(TICK_HZ)
}
