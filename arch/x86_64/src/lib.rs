//! x86_64 architecture support.
//!
//! The crate owns the 32-bit multiboot entry stub, the transition into long
//! mode, the interrupt descriptor table, the APIC timer and the HAL system
//! services. Boards choose the machine-specific addresses; the kernel only
//! sees the same `Clock`, `Power`, `Cpu`, `irq` and `smp` surface as it does on
//! AArch64.

#![no_std]

use feos_hal::{Clock, Cpu, Power};

pub mod boot;
pub mod cpu;
pub mod exceptions;
pub mod irq;
pub mod power;
pub mod smp;
pub mod timer;

/// The x86_64 implementation of the kernel's system services.
#[derive(Clone, Copy, Debug, Default)]
pub struct X86_64;

/// Shared instance of the x86_64 system services.
pub static ARCH: X86_64 = X86_64;

/// Initializes the architecture's one-time boot state.
///
/// Called by the kernel after the console is up. It installs the IDT and
/// calibrates the TSC against the PIT, both of which need to happen before
/// interrupts are unmasked.
pub fn init() {
    exceptions::init();
    timer::init();
}

impl Clock for X86_64 {
    fn frequency(&self) -> u64 {
        timer::frequency()
    }

    fn ticks(&self) -> u64 {
        timer::ticks()
    }

    fn spin_ms(&self, ms: u64) {
        timer::spin_ms(ms);
    }

    fn idle_ms(&self, ms: u64) {
        timer::idle_ms(ms);
    }
}

impl Power for X86_64 {
    fn system_off(&self) -> ! {
        power::system_off()
    }

    fn system_reset(&self) -> ! {
        power::system_reset()
    }
}

impl Cpu for X86_64 {
    fn current_el(&self) -> u64 {
        cpu::current_el()
    }

    fn midr(&self) -> u64 {
        cpu::midr_el1()
    }

    fn halt(&self) -> ! {
        cpu::halt()
    }
}
