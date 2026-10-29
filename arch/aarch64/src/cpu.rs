//! CPU-wide operations available from Rust.

use core::arch::asm;

/// Stops the CPU until an event occurs, forever.
///
/// This is the kernel's idle loop and the final state of every fatal path.
pub fn halt() -> ! {
    loop {
        // SAFETY: `wfe` is available at EL1 and has no memory side effects.
        unsafe { asm!("wfe", options(nomem, nostack)) }
    }
}

/// Current exception level (0..=3), read from `CurrentEL`.
pub fn current_el() -> u64 {
    let value: u64;
    // SAFETY: reading a system register has no side effects.
    unsafe { asm!("mrs {}, CurrentEL", out(reg) value, options(nomem, nostack)) };
    (value >> 2) & 0x3
}

/// Main ID register (`MIDR_EL1`), describing the CPU implementation.
pub fn midr_el1() -> u64 {
    let value: u64;
    // SAFETY: reading a system register has no side effects.
    unsafe { asm!("mrs {}, MIDR_EL1", out(reg) value, options(nomem, nostack)) };
    value
}
