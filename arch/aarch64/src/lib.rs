//! AArch64 architecture support.
//!
//! This crate contains only code that is specific to the AArch64 CPU: the
//! early boot entry point, exception vectors, PSCI calls and the generic
//! timer. It knows nothing about boards, drivers, apps or the console.
//!
//! [`ARCH`] implements the system-service traits from `feos-hal`, so the
//! kernel can hand one static implementation to every app without apps
//! depending on this crate.
//!
//! A port to another architecture adds a sibling crate (`arch/riscv64`,
//! `arch/x86_64`, ...) with the same surface and a board crate that selects
//! it.

#![no_std]

use feos_hal::{Clock, Cpu, Power};

pub mod boot;
pub mod cpu;
pub mod exceptions;
pub mod irq;
pub mod psci;
pub mod smp;
pub mod timer;

/// The AArch64 implementation of the kernel's system services.
#[derive(Clone, Copy, Debug, Default)]
pub struct Aarch64;

/// Shared instance of the AArch64 system services.
pub static ARCH: Aarch64 = Aarch64;

/// One-time architecture initialization.
///
/// The AArch64 boot stub already set up the CPU and stack; exception vectors
/// are installed by the kernel after the console is available.
pub fn init() {}

impl Clock for Aarch64 {
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

impl Power for Aarch64 {
    fn system_off(&self) -> ! {
        psci::system_off()
    }

    fn system_reset(&self) -> ! {
        psci::system_reset()
    }
}

impl Cpu for Aarch64 {
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
