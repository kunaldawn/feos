//! Timekeeping and the scheduler's periodic tick.
//!
//! The monotonic clock is the TSC, calibrated once against PIT channel 0.
//! The scheduler tick is a separate 100 Hz PIT interrupt routed through the
//! boards' IOAPIC to vector 34.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use feos_port_io::{inl, outb};

use crate::smp::MAX_CPUS;

/// PIT input clock.
const PIT_HZ: u64 = 1_193_182;

/// ACPI PM timer I/O port (PM1a_EVT + 8 on QEMU's default PM base `0x600`).
const ACPI_PM_TIMER: u16 = 0x608;
/// ACPI PM timer frequency: 3.579545 MHz.
const ACPI_PM_HZ: u64 = 3_579_545;
/// Calibration interval in PM timer ticks (about 28 ms).
const CALIBRATION_TICKS: u32 = 100_000;

static FREQUENCY: AtomicU64 = AtomicU64::new(0);
static PERIOD: AtomicU64 = AtomicU64::new(0);
static IDLE_HINTS: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Reads the timestamp counter.
pub fn ticks() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: RDTSC is available on every x86_64 CPU. LFENCE serialises it so
    // successive reads are monotonically ordered.
    unsafe {
        asm!("lfence", "rdtsc", out("eax") low, out("edx") high, options(nomem, nostack));
    }
    ((high as u64) << 32) | low as u64
}

/// Calibrates the TSC against the ACPI power-management timer.
pub fn init() {
    if FREQUENCY.load(Ordering::Relaxed) != 0 {
        return;
    }

    let start_pm = unsafe { inl(ACPI_PM_TIMER) } & 0x00ff_ffff;
    let start_tsc = ticks();

    let mut guard = 0u32;
    loop {
        let elapsed = (unsafe { inl(ACPI_PM_TIMER) } & 0x00ff_ffff)
            .wrapping_sub(start_pm)
            & 0x00ff_ffff;
        if elapsed >= CALIBRATION_TICKS || guard >= 10_000_000 {
            break;
        }
        guard += 1;
        core::hint::spin_loop();
    }

    let end_tsc = ticks();
    let end_pm = unsafe { inl(ACPI_PM_TIMER) } & 0x00ff_ffff;
    let pm_ticks = end_pm.wrapping_sub(start_pm) & 0x00ff_ffff;

    let frequency = if pm_ticks == 0 {
        // The PM timer did not advance; keep a conservative fallback rather
        // than dividing by zero. Tick-based delays still work at this rate.
        1_000_000_000
    } else {
        ((end_tsc.wrapping_sub(start_tsc)) as u128 * ACPI_PM_HZ as u128 / pm_ticks as u128)
            as u64
    };

    FREQUENCY.store(frequency.max(1), Ordering::Relaxed);
}

/// Frequency of the timestamp counter in Hz.
pub fn frequency() -> u64 {
    let frequency = FREQUENCY.load(Ordering::Relaxed);
    if frequency == 0 {
        init();
        return FREQUENCY.load(Ordering::Relaxed).max(1);
    }
    frequency
}

/// Milliseconds elapsed since the counter started.
pub fn uptime_ms() -> u64 {
    feos_hal::ticks_to_ms(ticks(), frequency())
}

/// Busy-waits for roughly `ms` milliseconds.
pub fn spin_ms(ms: u64) {
    let frequency = frequency();
    let delta = frequency.saturating_mul(ms) / 1000;
    let start = ticks();
    while ticks().wrapping_sub(start) < delta {
        core::hint::spin_loop();
    }
}

/// Waits for roughly `ms` milliseconds in a low-power state.
///
/// The kernel's clock blocks the process instead once the scheduler is running,
/// so this only serves code running before it starts.
pub fn idle_ms(ms: u64) {
    let frequency = frequency();
    let delta = frequency.saturating_mul(ms) / 1000;
    let start = ticks();
    let cpu = crate::cpu::index().min(MAX_CPUS - 1);
    let flags = crate::irq::save_and_disable();

    while ticks().wrapping_sub(start) < delta {
        IDLE_HINTS[cpu].store(true, Ordering::Relaxed);
        // SAFETY: unmask, halt until the next interrupt, then mask again so
        // the loop can decide whether it is done. The handler preserves this
        // thread's state.
        unsafe { asm!("sti", "hlt", "cli", options(nostack)) };
    }

    crate::irq::restore(flags);
}

/// Returns and clears the idle hint recorded by [`idle_ms`] on this CPU.
pub fn take_idle_hint() -> bool {
    let cpu = crate::cpu::index().min(MAX_CPUS - 1);
    IDLE_HINTS[cpu].swap(false, Ordering::Relaxed)
}

/// Starts the PIT as a periodic scheduler tick at `hz` hertz.
pub fn start_periodic(hz: u64) {
    if hz == 0 {
        stop();
        return;
    }

    let divisor = (PIT_HZ / hz).clamp(1, 0xffff);
    PERIOD.store(divisor, Ordering::Relaxed);

    // SAFETY: channel 0, mode 3 (square wave), lobyte/hibyte.
    unsafe {
        outb(0x43, 0x36);
        outb(0x40, divisor as u8);
        outb(0x40, (divisor >> 8) as u8);
    }
}

/// Reprograms the timer after a tick and returns `true` when it was running.
///
/// PIT mode 3 reloads automatically, so this only reports whether the tick is
/// active. The interrupt controller has already been acknowledged by the
/// kernel before calling this.
pub fn tick() -> bool {
    PERIOD.load(Ordering::Relaxed) != 0
}

/// Stops the periodic tick.
pub fn stop() {
    PERIOD.store(0, Ordering::Relaxed);

    // SAFETY: channel 0, mode 0, count 0 (stops counting).
    unsafe {
        outb(0x43, 0x30);
        outb(0x40, 0);
        outb(0x40, 0);
    }
}
