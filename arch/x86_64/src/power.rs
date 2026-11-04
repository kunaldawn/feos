//! Machine power control.
//!
//! QEMU's q35 chipset exposes the ACPI PM1a control register at port `0x604`.
//! Its DSDT's `_S5` package gives `SLP_TYP=0`, and writing that with `SLP_EN` set
//! powers the machine off. The value is hard-coded because the kernel doesn't
//! parse ACPI tables yet. The reset control register at `0xCF9` is the standard
//! PCI reset mechanism.

use feos_port_io::{outb, outw};

use crate::cpu;

/// ACPI PM1a control register on QEMU's q35 machine.
const PM1A_CNT: u16 = 0x604;
const SLP_TYP_S5: u16 = 0;
const SLP_EN: u16 = 1 << 13;

/// PCI reset control register.
const RESET_CNTRL: u16 = 0xcf9;

/// Powers the machine off (QEMU exits).
pub fn system_off() -> ! {
    // SAFETY: PM1A_CNT is the ACPI power-management control register.
    unsafe {
        outw(PM1A_CNT, (SLP_TYP_S5 << 10) | SLP_EN);
    }
    cpu::halt()
}

/// Resets the machine.
pub fn system_reset() -> ! {
    // SAFETY: RESET_CNTRL is the standard PCI reset register.
    unsafe {
        outb(RESET_CNTRL, 0x02);
        outb(RESET_CNTRL, 0x06);
    }
    cpu::halt()
}
