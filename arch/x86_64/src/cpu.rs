//! CPU-wide operations available from Rust.

use core::arch::asm;

/// Stops the CPU until an event occurs, forever.
///
/// This is the kernel's idle loop and the final state of every fatal path.
pub fn halt() -> ! {
    loop {
        // SAFETY: `cli` and `hlt` are available at ring 0.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) }
    }
}

/// Current privilege level (always 0 today; the kernel never leaves ring 0).
pub fn current_el() -> u64 {
    let cs: u16;
    // SAFETY: reading a segment register has no side effects.
    unsafe { asm!("mov {:x}, cs", out(reg) cs, options(nomem, nostack)) };
    (cs & 0x3) as u64
}

/// Logical index of the CPU running this code.
///
/// The x86_64 port currently brings up only the boot processor, so every
/// caller is CPU 0.
pub fn index() -> usize {
    0
}

/// Assigns the logical CPU index. Kept for parity with the scheduler's
/// architecture interface; a future SMP port stores this in the GS base.
pub fn set_index(_index: usize) {}

/// CPU identification value exposed through the generic HAL `Cpu` trait.
///
/// On x86 this is the maximum basic CPUID leaf, which tells diagnostics that
/// the CPU answered CPUID even when no model name is decoded.
pub fn midr_el1() -> u64 {
    core::arch::x86_64::__cpuid(0).eax as u64
}

/// EOI-free halt used by the idle loop.
pub fn wait_for_event() {
    // SAFETY: `hlt` is available at ring 0; interrupts may be enabled.
    unsafe { asm!("hlt", options(nomem, nostack)) }
}
