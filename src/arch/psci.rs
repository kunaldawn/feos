//! PSCI power management calls.
//!
//! On the QEMU `virt` machine firmware exposes PSCI over the HVC conduit when
//! the kernel runs at EL1, which lets us power the machine off cleanly.

use core::arch::asm;

const SYSTEM_OFF: u64 = 0x8400_0008;
const SYSTEM_RESET: u64 = 0x8400_0009;

/// Powers the machine off (QEMU exits).
pub fn system_off() -> ! {
    unsafe { asm!("hvc #0", in("x0") SYSTEM_OFF, options(noreturn)) }
}

/// Resets the machine.
pub fn system_reset() -> ! {
    unsafe { asm!("hvc #0", in("x0") SYSTEM_RESET, options(noreturn)) }
}
