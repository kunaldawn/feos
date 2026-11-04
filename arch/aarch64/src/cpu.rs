//! CPU-wide operations available from Rust.

use core::arch::asm;

/// Masks every interrupt and stops this CPU for good.
///
/// This is the final state of every fatal path. With the mask set, the tick can
/// wake `wfe` but can't run a handler, so the CPU stays here.
pub fn halt() -> ! {
    loop {
        // SAFETY: `daifset` and `wfe` are available at EL1.
        unsafe { asm!("msr daifset, #0xf", "wfe", options(nomem, nostack)) }
    }
}

/// Current exception level (0..=3), read from `CurrentEL`.
pub fn current_el() -> u64 {
    let value: u64;
    // SAFETY: reading a system register has no side effects.
    unsafe { asm!("mrs {}, CurrentEL", out(reg) value, options(nomem, nostack)) };
    (value >> 2) & 0x3
}

/// Logical index of the CPU running this code.
///
/// The boot CPU sets `TPIDR_EL1` to zero and `secondary_entry` writes the PSCI
/// context id (the logical index) into it, so this is a cheap per-CPU register
/// read rather than a shared table lookup.
pub fn index() -> usize {
    let value: u64;
    // SAFETY: `TPIDR_EL1` is EL1-readable and has no memory side effects.
    unsafe { asm!("mrs {}, tpidr_el1", out(reg) value, options(nomem, nostack)) };
    value as usize
}

/// Assigns the logical CPU index stored in `TPIDR_EL1`.
pub fn set_index(index: usize) {
    // SAFETY: writing the thread pointer has no memory side effects.
    unsafe { asm!("msr tpidr_el1, {}", in(reg) index as u64, options(nomem, nostack)) };
}

/// Main ID register (`MIDR_EL1`), describing the CPU implementation.
pub fn midr_el1() -> u64 {
    let value: u64;
    // SAFETY: reading a system register has no side effects.
    unsafe { asm!("mrs {}, MIDR_EL1", out(reg) value, options(nomem, nostack)) };
    value
}
