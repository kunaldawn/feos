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

/// `CNTKCTL_EL1.EL0VCTEN`: EL0 may read the virtual counter and its frequency.
const CNTKCTL_EL0VCTEN: u64 = 1 << 1;

/// Sets what EL0 can reach on this CPU. Call once on each, at EL1.
///
/// `SCTLR_EL1` is already right for this: `el_setup` left WFI, WFE, `msr daif` and the
/// cache maintenance by address (`dc cvau`, `dc civac`, `ic ivau`: `UCI` clear, as no
/// program generates code) trapping from EL0, and let it read `CTR_EL0` (`UCT`) and zero
/// with `dc zva` (`DZE`), which compiler-rt's and libc's assembly use. Of the counters,
/// EL0 gets the virtual count and its frequency, as `docs/feos-abi-v2.md` section 1 says,
/// and nothing else: the physical counter, every timer and the performance monitors
/// trap. PAN keeps the kernel off user pages, which it never needs: see `feos-vm`. A CPU
/// before ARMv8.1, such as the Cortex-A53, doesn't have it.
pub fn protect() {
    // SAFETY: at EL1, with nothing mapped for EL0 yet. The encoding of `msr pan, #1` is
    // spelt out because the assembler wants a newer architecture named otherwise.
    unsafe {
        asm!(
            "msr cntkctl_el1, {}",
            "msr pmuserenr_el0, xzr",
            "msr tpidrro_el0, xzr",
            "msr tpidr_el0, xzr",
            in(reg) CNTKCTL_EL0VCTEN,
            options(nomem, nostack)
        );
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

/// Linux's `HWCAP_*` and `HWCAP2_*` bits for what the ID registers say this CPU has, for
/// the aux vector. Never `HWCAP_CPUID`, since EL0 can't read the ID registers, and never
/// `HWCAP_SVE`, which stays off.
pub fn user_hwcaps() -> (u64, u64) {
    let (isar0, pfr0): (u64, u64);
    // SAFETY: reading ID registers has no effect.
    unsafe {
        asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0, options(nomem, nostack, preserves_flags));
        asm!("mrs {}, id_aa64pfr0_el1", out(reg) pfr0, options(nomem, nostack, preserves_flags));
    }
    let field = |value: u64, shift: u32| (value >> shift) & 0xf;
    let mut caps = 0u64;
    // FP and AdvSIMD: 0 is there, 1 with half precision too, 0xf is absent.
    match field(pfr0, 16) {
        0 => caps |= 1 << 0,
        1 => caps |= 1 << 0 | 1 << 9,
        _ => {}
    }
    match field(pfr0, 20) {
        0 => caps |= 1 << 1,
        1 => caps |= 1 << 1 | 1 << 10,
        _ => {}
    }
    match field(isar0, 4) {
        1 => caps |= 1 << 3,
        2 => caps |= 1 << 3 | 1 << 4,
        _ => {}
    }
    if field(isar0, 8) >= 1 {
        caps |= 1 << 5;
    }
    if field(isar0, 12) >= 1 {
        caps |= 1 << 6;
    }
    if field(isar0, 16) >= 1 {
        caps |= 1 << 7;
    }
    if field(isar0, 20) >= 2 {
        caps |= 1 << 8;
    }
    // `HWCAP2_RNG` where there's `RNDR`.
    let caps2 = if field(isar0, 60) >= 1 { 1 << 16 } else { 0 };
    (caps, caps2)
}

/// Sets the user thread pointer, `TPIDR_EL0`.
pub fn set_user_tls(value: usize) {
    // SAFETY: an EL0 register the kernel never uses itself.
    unsafe { asm!("msr tpidr_el0, {}", in(reg) value as u64, options(nomem, nostack, preserves_flags)) };
}

/// The user thread pointer, `TPIDR_EL0`, as a program may have left it.
pub fn user_tls() -> usize {
    let value: u64;
    // SAFETY: reading a system register has no side effects.
    unsafe { asm!("mrs {}, tpidr_el0", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value as usize
}
