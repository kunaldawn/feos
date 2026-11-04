//! Secondary-CPU startup.
//!
//! The current x86_64 port runs the scheduler on the boot processor only.
//! The interface mirrors the AArch64 crate so the kernel's scheduler and
//! process model are architecture-independent; a future SMP port can fill in
//! an ACPI MADT + INIT/SIPI implementation without changing the kernel.

/// Maximum number of CPUs this architecture port can bring up.
pub const MAX_CPUS: usize = 1;

/// Callback invoked on a secondary CPU once its stack and vectors are ready.
pub type SecondaryHandler = fn(usize) -> !;

/// Starts the secondary CPUs.
///
/// The current port has no AP trampoline yet, so the boot processor is the only
/// scheduler CPU. Returns the number of CPUs started and any firmware status.
pub fn start_secondaries(
    _limit: usize,
    _mpidr: fn(usize) -> u64,
    _handler: SecondaryHandler,
) -> (usize, Option<i64>) {
    (0, None)
}
