//! ARM Generic Interrupt Controller v2 (GICv2) driver.
//!
//! QEMU's `virt` machine exposes a GICv2 distributor at `0x0800_0000` and a
//! CPU interface at `0x0801_0000`. The board owns those addresses and calls
//! [`GicV2::init`]; the kernel then enables the timer interrupt and uses the
//! HAL [`InterruptController`] trait to acknowledge and end interrupts.
//!
//! Only the CPU interface for the boot CPU is configured. The kernel is
//! single-core, so private peripheral interrupts (PPIs) are banked by hardware
//! and no target list has to be programmed.

#![no_std]

use core::ptr::{read_volatile, write_volatile};

use feos_hal::InterruptController;

/// Distributor register block.
const GICD_CTLR: usize = 0x000;
const GICD_IGROUPR: usize = 0x080;
const GICD_ISENABLER: usize = 0x100;
const GICD_ICENABLER: usize = 0x180;
const GICD_IPRIORITYR: usize = 0x400;

/// CPU interface register block.
const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004;
const GICC_BPR: usize = 0x008;
const GICC_IAR: usize = 0x00c;
const GICC_EOIR: usize = 0x010;

/// Priority assigned to every interrupt the kernel enables.
const PRIORITY: u32 = 0x80;

/// Interrupt ID reported by [`InterruptController::acknowledge`] when no real
/// interrupt is pending.
pub const SPURIOUS: u32 = 1023;

/// A GICv2 distributor + CPU interface pair.
#[derive(Clone, Copy, Debug)]
pub struct GicV2 {
    distributor: usize,
    cpu_interface: usize,
}

impl GicV2 {
    /// Creates a driver for the given distributor and CPU interface addresses.
    pub const fn new(distributor: usize, cpu_interface: usize) -> Self {
        Self {
            distributor,
            cpu_interface,
        }
    }

    /// Configures the controller and enables Group 0.
    ///
    /// QEMU's `virt` machine builds a GICv2 without the security extensions
    /// (the default `secure=off` configuration), so Group 0 is the only group
    /// and is acknowledged through `GICC_IAR`. If the machine is started with
    /// `secure=on`, the kernel would need the aliased Group 1 registers
    /// (`GICC_AIAR`/`GICC_AEOIR`) instead.
    pub fn init(&self) {
        write32(self.distributor + GICD_CTLR, 0);
        barrier();

        // Priority mask: allow every priority. Binary point: group split at 0.
        write32(self.cpu_interface + GICC_PMR, 0xff);
        write32(self.cpu_interface + GICC_BPR, 0);

        barrier();
        write32(self.distributor + GICD_CTLR, 0b01);
        write32(self.cpu_interface + GICC_CTLR, 0b01);
        barrier();
    }

    fn set_group(&self, irq: u32) {
        let register = self.distributor + GICD_IGROUPR + (irq as usize / 32) * 4;
        let bit = 1u32 << (irq % 32);
        write32(register, read32(register) & !bit);
    }

    fn set_priority(&self, irq: u32, priority: u32) {
        let register = self.distributor + GICD_IPRIORITYR + (irq as usize & !3);
        let shift = (irq % 4) * 8;
        let mask = 0xffu32 << shift;
        write32(register, (read32(register) & !mask) | (priority << shift));
    }
}

impl InterruptController for GicV2 {
    fn enable(&self, irq: u32) {
        if irq >= SPURIOUS {
            return;
        }

        // Group 0, medium priority, then enable the line.
        self.set_group(irq);
        self.set_priority(irq, PRIORITY);
        let register = self.distributor + GICD_ISENABLER + (irq as usize / 32) * 4;
        write32(register, 1u32 << (irq % 32));
        barrier();
    }

    fn disable(&self, irq: u32) {
        if irq >= SPURIOUS {
            return;
        }

        let register = self.distributor + GICD_ICENABLER + (irq as usize / 32) * 4;
        write32(register, 1u32 << (irq % 32));
        barrier();
    }

    fn acknowledge(&self) -> u32 {
        read32(self.cpu_interface + GICC_IAR) & 0x3ff
    }

    fn end(&self, irq: u32) {
        if irq >= SPURIOUS {
            return;
        }

        write32(self.cpu_interface + GICC_EOIR, irq);
        barrier();
    }
}

#[inline]
fn read32(address: usize) -> u32 {
    // SAFETY: callers pass addresses inside the board's GIC MMIO window.
    unsafe { read_volatile(address as *const u32) }
}

#[inline]
fn write32(address: usize, value: u32) {
    // SAFETY: callers pass addresses inside the board's GIC MMIO window.
    unsafe { write_volatile(address as *mut u32, value) }
}

#[inline]
fn barrier() {
    // Order device accesses before the CPU continues.
    unsafe { core::arch::asm!("dsb sy", "isb", options(nostack)) }
}
