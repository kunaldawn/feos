//! x86 Local APIC and IOAPIC driver.
//!
//! The board supplies the MMIO bases. [`LocalApic`] owns the per-CPU
//! registers used for interrupt acknowledgement; [`IoApic`] routes external
//! lines (today the PIT on GSI 2) to a vector on the boot CPU.
//!
//! Only the boot processor is brought up by the current port. The driver keeps
//! the pieces a future SMP port needs (LAPIC ID, EOI, ISR scan) separate from
//! the routing policy.

#![cfg_attr(not(test), no_std)]

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

use feos_hal::{InterruptController, IpiTarget};
use feos_port_io::outb;

/// `IA32_APIC_BASE`.
const MSR_APIC_BASE: u32 = 0x1b;
const APIC_BASE_ENABLE: u64 = 1 << 11;

/// Local APIC register offsets.
const LAPIC_ID: usize = 0x20;
const LAPIC_TPR: usize = 0x80;
const LAPIC_EOI: usize = 0xb0;
const LAPIC_SVR: usize = 0xf0;
const LAPIC_LVT_TIMER: usize = 0x320;
const LAPIC_LVT_LINT0: usize = 0x350;
const LAPIC_LVT_LINT1: usize = 0x360;
const LAPIC_LVT_ERROR: usize = 0x370;
const LAPIC_ISR: usize = 0x100;
const LAPIC_ICR_LOW: usize = 0x300;
const LAPIC_ICR_HIGH: usize = 0x310;

/// ICR: a previous IPI is still being delivered.
const ICR_PENDING: u32 = 1 << 12;
/// ICR: level assert. Required for anything other than an INIT de-assert.
const ICR_ASSERT: u32 = 1 << 14;
/// ICR destination shorthand: all CPUs excluding this one.
const ICR_ALL_OTHERS: u32 = 0b11 << 18;
/// Polls of the delivery-status bit before sending anyway.
const ICR_POLLS: usize = 100_000;

/// IOAPIC register window.
const IOAPIC_IOREGSEL: usize = 0x00;
const IOAPIC_IOWIN: usize = 0x10;
const IOAPIC_REDIRECT: u32 = 0x10;

/// Vector reported when no in-service interrupt is visible.
pub const SPURIOUS: u32 = 0xff;

/// A local APIC at a fixed MMIO address.
#[derive(Clone, Copy, Debug)]
pub struct LocalApic {
    base: usize,
}

impl LocalApic {
    /// Creates a driver for the local APIC mapped at `base`.
    pub const fn new(base: usize) -> Self {
        Self { base }
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: `base` is the board's local-APIC MMIO window.
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        // SAFETY: `base` is the board's local-APIC MMIO window.
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }

    /// Enables the local APIC and masks unused local vectors.
    pub fn init(&self) {
        // Program the APIC base, keep xAPIC MMIO mode (clear x2APIC enable),
        // and set the global enable bit.
        let base = read_msr(MSR_APIC_BASE) & !0xffff_f000 & !(1 << 10);
        write_msr(
            MSR_APIC_BASE,
            base | (self.base as u64 & 0xffff_f000) | APIC_BASE_ENABLE,
        );

        // Task priority 0: accept everything. Spurious vector 0xff, APIC on.
        self.write(LAPIC_TPR, 0);
        self.write(LAPIC_SVR, 0x100 | SPURIOUS);

        // The PIT drives the scheduler tick, so mask the local timer and
        // local interrupt lines the firmware may have left enabled.
        self.write(LAPIC_LVT_TIMER, 1 << 16);
        self.write(LAPIC_LVT_LINT0, 1 << 16);
        self.write(LAPIC_LVT_LINT1, 1 << 16);
        self.write(LAPIC_LVT_ERROR, 1 << 16);

        self.eoi();
    }

    /// APIC ID of this CPU.
    pub fn id(&self) -> u32 {
        self.read(LAPIC_ID) >> 24
    }

    /// Signals end-of-interrupt.
    pub fn eoi(&self) {
        self.write(LAPIC_EOI, 0);
    }

    /// Sends a fixed-delivery IPI with `vector` to `target`.
    pub fn send_ipi(&self, target: IpiTarget, vector: u8) {
        for _ in 0..ICR_POLLS {
            if self.read(LAPIC_ICR_LOW) & ICR_PENDING == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        let (destination, low) = icr_value(target, vector);
        // The write to the low half is what sends it.
        self.write(LAPIC_ICR_HIGH, destination);
        self.write(LAPIC_ICR_LOW, low);
    }

    /// Returns the highest in-service vector, if any.
    pub fn isr_vector(&self) -> Option<u32> {
        // The ISR is 256 bits wide, one 32-bit register per 32 vectors.
        for register in (0..8).rev() {
            let value = self.read(LAPIC_ISR + register * 0x10);
            if value != 0 {
                let bit = 31 - value.leading_zeros();
                return Some(register as u32 * 32 + bit);
            }
        }
        None
    }
}

/// Encodes an ICR write as its high and low halves.
fn icr_value(target: IpiTarget, vector: u8) -> (u32, u32) {
    let low = vector as u32 | ICR_ASSERT;
    match target {
        IpiTarget::Cpu(cpu) => ((cpu as u32) << 24, low),
        IpiTarget::AllOthers => (0, low | ICR_ALL_OTHERS),
    }
}

/// An IOAPIC at a fixed MMIO address.
#[derive(Clone, Copy, Debug)]
pub struct IoApic {
    base: usize,
}

impl IoApic {
    /// Creates a driver for the IOAPIC mapped at `base`.
    pub const fn new(base: usize) -> Self {
        Self { base }
    }

    fn select(&self, register: u32) {
        // SAFETY: `base` is the board's IOAPIC MMIO window.
        unsafe { write_volatile((self.base + IOAPIC_IOREGSEL) as *mut u32, register) };
    }

    fn read(&self, register: u32) -> u32 {
        self.select(register);
        // SAFETY: `base` is the board's IOAPIC MMIO window.
        unsafe { read_volatile((self.base + IOAPIC_IOWIN) as *const u32) }
    }

    fn write(&self, register: u32, value: u32) {
        self.select(register);
        // SAFETY: `base` is the board's IOAPIC MMIO window.
        unsafe { write_volatile((self.base + IOAPIC_IOWIN) as *mut u32, value) };
    }

    /// Reads the IOAPIC version register.
    pub fn version(&self) -> u32 {
        self.read(0x01)
    }

    /// Returns the highest redirection-table index this IOAPIC supports.
    pub fn max_redirection_entry(&self) -> u32 {
        (self.read(0x01) >> 16) & 0xff
    }

    /// Masks every redirection entry.
    ///
    /// QEMU usually resets them masked, but doing it explicitly keeps a
    /// firmware-left route from delivering an unexpected vector before the
    /// board installs its own.
    pub fn mask_all(&self) {
        for gsi in 0..=self.max_redirection_entry() {
            self.set_masked(gsi, true);
        }
    }

    /// Routes `gsi` to `vector` on the CPU with APIC ID `destination`.
    ///
    /// `masked` keeps the entry disabled until `enable` clears the mask.
    pub fn route(&self, gsi: u32, vector: u8, destination: u32, masked: bool) {
        let register = IOAPIC_REDIRECT + gsi * 2;
        let low = vector as u32 | ((masked as u32) << 16);
        let high = destination << 24;

        // Program the high half first so a partially written entry never
        // carries a stale destination.
        self.write(register + 1, high);
        self.write(register, low);
    }

    /// Masks or unmasks one redirection entry.
    pub fn set_masked(&self, gsi: u32, masked: bool) {
        let register = IOAPIC_REDIRECT + gsi * 2;
        let low = self.read(register);
        let low = if masked {
            low | (1 << 16)
        } else {
            low & !(1 << 16)
        };
        self.write(register, low);
    }
}

/// Local APIC + IOAPIC pair used as the kernel's interrupt controller.
#[derive(Clone, Copy, Debug)]
pub struct ApicController {
    lapic: LocalApic,
    ioapic: IoApic,
}

impl ApicController {
    /// Creates a controller for the given MMIO bases.
    pub const fn new(lapic_base: usize, ioapic_base: usize) -> Self {
        Self {
            lapic: LocalApic::new(lapic_base),
            ioapic: IoApic::new(ioapic_base),
        }
    }

    /// Brings up the local APIC and masks the legacy 8259 PIC.
    pub fn init(&self) {
        // Mask the legacy PIC so its IRQs cannot arrive as vector 0x20..0x27
        // before the IOAPIC is programmed.
        unsafe {
            outb(0x21, 0xff);
            outb(0xa1, 0xff);
        }

        self.lapic.init();
        self.ioapic.mask_all();
    }

    /// Local APIC ID of the boot CPU.
    pub fn cpu_id(&self) -> u32 {
        self.lapic.id()
    }
}

impl InterruptController for ApicController {
    fn enable(&self, irq: u32) {
        if irq < 32 {
            return;
        }
        self.ioapic
            .route(irq - 32, irq as u8, self.lapic.id(), false);
    }

    fn disable(&self, irq: u32) {
        if irq < 32 {
            return;
        }
        self.ioapic.set_masked(irq - 32, true);
    }

    fn acknowledge(&self) -> u32 {
        self.lapic.isr_vector().unwrap_or(SPURIOUS)
    }

    fn end(&self, _irq: u32) {
        self.lapic.eoi();
    }

    fn is_spurious(&self, id: u32) -> bool {
        id == SPURIOUS
    }

    fn send_ipi(&self, target: IpiTarget, id: u32) {
        self.lapic.send_ipi(target, id as u8);
    }
}

fn read_msr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: reading an architectural MSR has no memory side effects.
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack));
    }
    ((high as u64) << 32) | low as u64
}

fn write_msr(msr: u32, value: u64) {
    // SAFETY: writing an architectural MSR has no memory side effects.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack)
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icr_encodes_destination_and_shorthand() {
        assert_eq!(icr_value(IpiTarget::Cpu(2), 50), (2 << 24, 50 | ICR_ASSERT));
        assert_eq!(
            icr_value(IpiTarget::AllOthers, 49),
            (0, 49 | ICR_ASSERT | ICR_ALL_OTHERS)
        );
    }

    #[test]
    fn only_the_spurious_vector_is_spurious() {
        // DRV-11: the HAL's 1023 means nothing on this controller.
        let controller = ApicController::new(0, 0);
        assert!(controller.is_spurious(SPURIOUS));
        assert!(!controller.is_spurious(34));
        assert!(!controller.is_spurious(1023));
    }
}
