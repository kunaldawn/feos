//! Generic timer access and the scheduler's periodic tick.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::smp::MAX_CPUS;
use crate::{cpu, irq};

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
    feos_hal::ticks_to_ms(ticks(), frequency())
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

/// Set before `WFI` so the scheduler can classify the tick as idle.
///
/// This is per-CPU: two cores can be in different states, and the scheduler
/// must only credit the core that actually waited.
static IDLE_HINTS: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Waits for roughly `ms` milliseconds in a low-power state.
///
/// The scheduler's periodic tick wakes the CPU, so this never sleeps longer
/// than one tick. Each wait marks the current tick as idle, which lets the
/// task manager distinguish spinning from genuinely waiting work. The kernel's
/// clock blocks the process instead once the scheduler is running, so this only
/// serves code with interrupts masked or running before it starts.
pub fn idle_ms(ms: u64) {
    let freq = frequency();
    if freq == 0 {
        return;
    }

    let start = ticks();
    let delta = freq.saturating_mul(ms) / 1000;
    let flags = irq::save_and_disable();

    while ticks().wrapping_sub(start) < delta {
        // The mask keeps this task on this CPU from the hint until the wait,
        // so the hint lands on the CPU that actually waits, and no tick can
        // fall between them. `wfi` still wakes on a masked interrupt.
        let cpu = cpu::index().min(MAX_CPUS - 1);
        IDLE_HINTS[cpu].store(true, Ordering::Relaxed);
        // SAFETY: `wfi` only suspends execution; the unmask is what lets the
        // pending tick run, and the handler preserves this thread's state.
        unsafe { asm!("wfi", "msr daifclr, #2", "isb", "msr daifset, #2", options(nostack)) };
    }

    irq::restore(flags);
}

/// Returns and clears the idle hint recorded by [`idle_ms`] on this CPU.
pub fn take_idle_hint() -> bool {
    let cpu = cpu::index().min(MAX_CPUS - 1);
    IDLE_HINTS[cpu].swap(false, Ordering::Relaxed)
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
    let deadline = ticks().wrapping_add(period);
    unsafe {
        asm!("msr cntp_ctl_el0, {}", in(reg) 0u64, options(nomem, nostack));
        asm!("msr cntp_cval_el0, {}", in(reg) deadline, options(nomem, nostack));
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

    // Advance the deadline by exactly one period, so the time spent reaching
    // this handler doesn't accumulate as drift. After a long stall, skip the
    // missed ticks instead of firing once per period until caught up.
    let previous: u64;
    // SAFETY: reading the timer's compare value has no side effects.
    unsafe { asm!("mrs {}, cntp_cval_el0", out(reg) previous, options(nomem, nostack)) };
    let deadline = feos_hal::next_deadline(previous, period, ticks());

    // Writing CVAL clears the condition and schedules the next deadline.
    unsafe {
        asm!("msr cntp_cval_el0, {}", in(reg) deadline, options(nomem, nostack));
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
