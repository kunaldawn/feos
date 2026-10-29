//! Generic timer access (no interrupts, polling only).

use core::arch::asm;

/// Frequency of the system counter in Hz (from CNTFRQ_EL0).
pub fn frequency() -> u64 {
    let value: u64;
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) value, options(nomem, nostack)) };
    value
}

/// Current value of the physical counter (CNTPCT_EL0).
pub fn ticks() -> u64 {
    let value: u64;
    unsafe { asm!("isb; mrs {}, cntpct_el0", out(reg) value, options(nomem, nostack)) };
    value
}

/// Milliseconds elapsed since the counter started.
pub fn uptime_ms() -> u64 {
    let freq = frequency();
    if freq == 0 {
        0
    } else {
        ticks().saturating_mul(1000) / freq
    }
}

/// Busy-waits for roughly `ms` milliseconds.
pub fn spin_ms(ms: u64) {
    let freq = frequency();
    if freq == 0 {
        return;
    }

    let start = ticks();
    let delta = freq.saturating_mul(ms) / 1000;

    while ticks().wrapping_sub(start) < delta {
        core::hint::spin_loop();
    }
}
