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

/// Whether this machine has PAN on, once the boot CPU has looked.
static PAN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// `SCTLR_EL1.SPAN`: clear, an exception to EL1 sets PAN by itself.
const SCTLR_SPAN: u64 = 1 << 23;

/// Shuts what EL0 can reach by default on this CPU. Call once on each, at EL1.
///
/// `SCTLR_EL1` is already right for this: `el_setup` left WFI, WFE, `dc zva`, `msr
/// daif`, CTR_EL0 and cache maintenance all trapping from EL0. The counters and
/// performance monitors have their own switches, and aren't promised to be off. PAN
/// keeps the kernel off user pages, which it never needs: see `feos-vm`. A CPU before
/// ARMv8.1, such as the Cortex-A53, doesn't have it.
pub fn protect() {
    // SAFETY: at EL1, with nothing mapped for EL0 yet, and all of this only narrows
    // what EL0 can do. The encoding of `msr pan, #1` is spelt out because the assembler
    // wants a newer architecture named otherwise.
    unsafe {
        asm!("msr cntkctl_el1, xzr", "msr pmuserenr_el0, xzr", options(nomem, nostack));
        let features: u64;
        asm!("mrs {}, id_aa64mmfr1_el1", out(reg) features, options(nomem, nostack));
        if (features >> 20) & 0xf != 0 {
            let mut control: u64;
            asm!("mrs {}, sctlr_el1", out(reg) control, options(nomem, nostack));
            control &= !SCTLR_SPAN;
            asm!("msr sctlr_el1, {}", "isb", ".inst 0xd500419f", "isb", in(reg) control, options(nostack));
            PAN.store(true, core::sync::atomic::Ordering::Relaxed);
            crate::irq::set_thread_pan();
        }
    }
}

/// Whether PAN is on.
pub fn has_pan() -> bool {
    PAN.load(core::sync::atomic::Ordering::Relaxed)
}
