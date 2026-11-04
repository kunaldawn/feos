//! Machine power control.
//!
//! QEMU's q35 chipset exposes the ACPI PM1a control register at port `0x604`;
//! writing `SLP_TYP=5` with `SLP_EN` set powers the machine off. The reset
//! control register at `0xCF9` is the standard PCI reset mechanism.

use core::arch::asm;

use crate::cpu;

/// ACPI PM1a control register on QEMU's q35 machine.
const PM1A_CNT: u16 = 0x604;
const SLP_TYP_S5: u16 = 5;
const SLP_EN: u16 = 1 << 13;

/// PCI reset control register.
const RESET_CNTRL: u16 = 0xcf9;

#[inline]
unsafe fn outw(port: u16, value: u16) {
    // SAFETY: callers pass a valid x86 I/O port.
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack)) };
}

#[inline]
unsafe fn outb(port: u16, value: u8) {
    // SAFETY: callers pass a valid x86 I/O port.
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) };
}

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
