//! Secondary-CPU startup.
//!
//! `boot.S` provides the PSCI entry point and private stacks for each secondary
//! CPU. This module turns that low-level entry into a callback the
//! kernel can install, mirroring the IRQ and fault handler inversion: the
//! architecture crate owns the entry mechanics, while the kernel owns the
//! scheduler policy.
//!
//! QEMU's `virt` machine starts secondary cores through PSCI `CPU_ON`. The
//! caller passes [`entry_address`] as the entry point and the logical CPU index
//! as the context id; `secondary_entry` writes that index into `TPIDR_EL1`.

use core::mem::transmute;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::cpu;

/// Maximum number of CPUs this architecture port can bring up.
///
/// GICv2 supports up to eight CPUs per machine, and `boot.S` reserves stacks
/// for each of them.
pub const MAX_CPUS: usize = 8;

/// Callback invoked on a secondary CPU once its stack and vectors are ready.
///
/// The handler runs with interrupts masked and must not return; the kernel
/// implementation joins the scheduler and then idles/executes tasks.
pub type SecondaryHandler = fn(usize) -> !;

static HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Starts CPUs `1..limit` with `handler` as their entry point.
///
/// `mpidr` maps a logical CPU index to its firmware affinity value. Returns the
/// number of CPUs firmware accepted and, when a request failed, the raw status
/// code. AArch64 numbers its CPUs contiguously, so the first failure marks the
/// end of the available range (for example `-smp 1` rejects CPU 1 and the loop
/// stops).
pub fn start_secondaries(
    limit: usize,
    mpidr: fn(usize) -> u64,
    handler: SecondaryHandler,
) -> (usize, Option<i64>) {
    HANDLER.store(handler as usize, Ordering::Release);

    let entry = secondary_entry as *const () as u64;
    let mut started = 0;
    for cpu in 1..limit.min(MAX_CPUS) {
        match crate::psci::cpu_on(mpidr(cpu), entry, cpu as u64) {
            Ok(()) => started += 1,
            Err(code) => return (started, Some(code)),
        }
    }
    (started, None)
}

fn handler() -> Option<SecondaryHandler> {
    let address = HANDLER.load(Ordering::Acquire);
    if address == 0 {
        None
    } else {
        // SAFETY: only `set_handler` writes this atomic with a valid function
        // pointer, and a function pointer is `usize`-sized on this target.
        Some(unsafe { transmute::<usize, SecondaryHandler>(address) })
    }
}

extern "C" {
    fn secondary_entry(context: usize) -> !;
}

/// Called from `secondary_entry` in `boot.S`.
#[no_mangle]
extern "C" fn secondary_main(context: usize) -> ! {
    match handler() {
        Some(handler) => handler(context),
        None => cpu::halt(),
    }
}
