//! Secondary-CPU startup.
//!
//! The current x86_64 port runs the scheduler on the boot processor only.
//! The interface mirrors the AArch64 crate so the kernel's scheduler and
//! process model are architecture-independent; a future SMP port can fill in
//! an ACPI MADT + INIT/SIPI implementation without changing the kernel.

use core::mem::transmute;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Maximum number of CPUs this architecture port can bring up.
pub const MAX_CPUS: usize = 1;

/// Callback invoked on a secondary CPU once its stack and vectors are ready.
pub type SecondaryHandler = fn(usize) -> !;

static HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Installs the function called on every secondary CPU.
pub fn set_handler(handler: SecondaryHandler) {
    HANDLER.store(handler as usize, Ordering::Release);
}

#[allow(dead_code)]
fn handler() -> Option<SecondaryHandler> {
    let address = HANDLER.load(Ordering::Acquire);
    if address == 0 {
        None
    } else {
        // SAFETY: only `set_handler` writes this atomic with a valid function
        // pointer.
        Some(unsafe { transmute::<usize, SecondaryHandler>(address) })
    }
}

/// Address a firmware CPU start would jump to. Unused on this port.
pub fn entry_address() -> usize {
    0
}
