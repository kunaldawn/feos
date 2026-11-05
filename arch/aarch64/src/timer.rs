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
/// task manager distinguish spinning from genuinely waiting work. Interrupts are
/// unmasked during the wait, so this must not be called from a critical section.
/// The kernel's clock blocks the process instead of using this.
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

/// Counter units per scheduler tick, once started.
static PERIOD: AtomicU64 = AtomicU64::new(0);

/// Starts this CPU's timer, first firing a tick from now at `hz` hertz.
///
/// The interrupt is delivered on the board's non-secure EL1 timer PPI and must
/// be enabled in the interrupt controller separately.
pub fn start(hz: u64) {
    let period = (frequency() / hz.max(1)).max(1);
    PERIOD.store(period, Ordering::Relaxed);
    arm(ticks().wrapping_add(period));
    // SAFETY: enabling this CPU's timer has no memory effect.
    unsafe {
        asm!("msr cntp_ctl_el0, {}", in(reg) 1u64, options(nomem, nostack));
        asm!("isb", options(nomem, nostack));
    }
}

/// Counter units per scheduler tick.
pub fn period() -> u64 {
    PERIOD.load(Ordering::Relaxed)
}

/// Has this CPU's timer interrupt fire at counter value `deadline`, replacing what
/// was set before. One that's already passed fires at once.
pub fn arm(deadline: u64) {
    // SAFETY: writing the compare value clears the condition and sets the next one.
    unsafe {
        asm!("msr cntp_cval_el0, {}", in(reg) deadline, options(nomem, nostack));
        asm!("isb", options(nomem, nostack));
    }
}
