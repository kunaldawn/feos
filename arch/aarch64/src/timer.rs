//! Generic timer access and the scheduler's periodic tick.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

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
    ticks().saturating_mul(1000).checked_div(freq).unwrap_or(0)
}

/// Busy-waits for roughly `ms` milliseconds.
///
/// The scheduler can preempt this loop, so other processes still run.
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

/// Period between scheduler ticks, in counter ticks (0 when stopped).
static PERIOD: AtomicU64 = AtomicU64::new(0);

/// Starts the EL1 physical timer as a periodic tick at `hz` hertz.
///
/// The interrupt is delivered on the board's non-secure EL1 timer PPI and must
/// be enabled in the interrupt controller separately.
pub fn start_periodic(hz: u64) {
    let freq = frequency();
    let period = if hz == 0 || freq == 0 {
        0
    } else {
        (freq / hz).max(1)
    };

    PERIOD.store(period, Ordering::Relaxed);
    if period == 0 {
        stop();
        return;
    }

    // Program the first deadline before unmasking the timer.
    unsafe {
        asm!("msr cntp_ctl_el0, {}", in(reg) 0u64, options(nomem, nostack));
        asm!("msr cntp_tval_el0, {}", in(reg) period, options(nomem, nostack));
        asm!("msr cntp_ctl_el0, {}", in(reg) 1u64, options(nomem, nostack));
        asm!("isb", options(nomem, nostack));
    }
}

/// Reprograms the timer after a tick and returns `true` when it was running.
pub fn tick() -> bool {
    let period = PERIOD.load(Ordering::Relaxed);
    if period == 0 {
        return false;
    }

    // Writing TVAL clears the condition and schedules the next deadline.
    unsafe {
        asm!("msr cntp_tval_el0, {}", in(reg) period, options(nomem, nostack));
        asm!("isb", options(nomem, nostack));
    }
    true
}

/// Stops the periodic tick and masks the timer interrupt.
pub fn stop() {
    PERIOD.store(0, Ordering::Relaxed);
    unsafe {
        asm!("msr cntp_ctl_el0, {}", in(reg) 0u64, options(nomem, nostack));
        asm!("isb", options(nomem, nostack));
    }
}
