//! What the architecture needs from this CPU's local APIC: its timer, and the
//! IPIs that start other CPUs.
//!
//! Interrupt routing and acknowledgement are the interrupt controller driver's,
//! which the board owns. This crate can't reach it, and its timer and startup
//! sequence aren't anybody else's business, so these few registers are here.

use core::ptr::{read_volatile, write_volatile};

/// The architectural default. `feos-driver-apic` keeps every CPU's APIC here.
const BASE: usize = 0xfee0_0000;

pub const LVT_TIMER: usize = 0x320;
pub const TIMER_INITIAL: usize = 0x380;
pub const TIMER_CURRENT: usize = 0x390;
pub const TIMER_DIVIDE: usize = 0x3e0;
const ICR_LOW: usize = 0x300;
const ICR_HIGH: usize = 0x310;

/// ICR: a previous IPI is still being delivered.
const ICR_PENDING: u32 = 1 << 12;
/// ICR delivery modes.
const ICR_INIT: u32 = 0b101 << 8;
const ICR_STARTUP: u32 = 0b110 << 8;
/// ICR: level assert. A startup IPI needs it, and so does INIT.
const ICR_ASSERT: u32 = 1 << 14;
/// ICR: level triggered, as INIT is.
const ICR_LEVEL: u32 = 1 << 15;

pub fn read(register: usize) -> u32 {
    // SAFETY: the boot page tables map this window uncached on every CPU.
    unsafe { read_volatile((BASE + register) as *const u32) }
}

pub fn write(register: usize, value: u32) {
    // SAFETY: as in `read`.
    unsafe { write_volatile((BASE + register) as *mut u32, value) }
}

fn send(apic_id: u32, low: u32) {
    while read(ICR_LOW) & ICR_PENDING != 0 {
        core::hint::spin_loop();
    }
    write(ICR_HIGH, apic_id << 24);
    write(ICR_LOW, low);
}

/// Resets the CPU with this APIC ID into waiting for a startup IPI.
pub fn send_init(apic_id: u32) {
    send(apic_id, ICR_INIT | ICR_ASSERT | ICR_LEVEL);
    send(apic_id, ICR_INIT | ICR_LEVEL);
}

/// Starts a waiting CPU in real mode at `vector * 4096`.
pub fn send_startup(apic_id: u32, vector: u8) {
    send(apic_id, ICR_STARTUP | ICR_ASSERT | u32::from(vector));
}
