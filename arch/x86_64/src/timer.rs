//! Timekeeping and the scheduler's periodic tick.
//!
//! The monotonic clock is the TSC, calibrated once against the ACPI PM timer.
//! Each CPU's scheduler timer is its own local APIC timer in one-shot mode,
//! calibrated once against the TSC, on the vector the board publishes as its timer
//! interrupt. The scheduler arms it for the next thing it has to do.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use feos_port_io::inl;

use crate::lapic;
use crate::smp::MAX_CPUS;

/// The tick's interrupt vector. The board's `TIMER_IRQ` names the same number.
pub const TIMER_VECTOR: u32 = 34;
/// LVT: masked.
const LVT_MASKED: u32 = 1 << 16;
/// Divide configuration: by 16.
const DIVIDE_BY_16: u32 = 0b0011;
/// Calibration window for the APIC timer.
const APIC_CALIBRATION_MS: u64 = 10;

/// ACPI PM timer I/O port (PM1a_EVT + 8 on QEMU's default PM base `0x600`).
const ACPI_PM_TIMER: u16 = 0x608;
/// ACPI PM timer frequency: 3.579545 MHz.
const ACPI_PM_HZ: u64 = 3_579_545;
/// Calibration interval in PM timer ticks (about 28 ms).
const CALIBRATION_TICKS: u32 = 100_000;

static FREQUENCY: AtomicU64 = AtomicU64::new(0);
/// APIC timer counts per second, measured once and shared: the timer runs from a
/// clock common to every CPU.
static APIC_HZ: AtomicU64 = AtomicU64::new(0);
/// Counter units per scheduler tick, once started.
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
/// Interrupts are unmasked during the wait, so this must not be called from a
/// critical section. The kernel's clock blocks the process instead of using this.
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

/// Measures the APIC timer against the TSC.
fn apic_timer_hz() -> u64 {
    let known = APIC_HZ.load(Ordering::Relaxed);
    if known != 0 {
        return known;
    }
    lapic::write(lapic::LVT_TIMER, LVT_MASKED);
    lapic::write(lapic::TIMER_DIVIDE, DIVIDE_BY_16);
    lapic::write(lapic::TIMER_INITIAL, u32::MAX);
    spin_ms(APIC_CALIBRATION_MS);
    let counted = u64::from(u32::MAX - lapic::read(lapic::TIMER_CURRENT));
    lapic::write(lapic::TIMER_INITIAL, 0);

    // A timer that doesn't count would never tick, so this is worth stopping for.
    assert!(counted != 0, "the local APIC timer isn't counting");
    let hz = counted * 1000 / APIC_CALIBRATION_MS;
    APIC_HZ.store(hz, Ordering::Relaxed);
    hz
}

/// Starts this CPU's timer, first firing a tick from now at `hz` hertz.
pub fn start(hz: u64) {
    let period = frequency() / hz.max(1);
    PERIOD.store(period, Ordering::Relaxed);
    apic_timer_hz();
    lapic::write(lapic::TIMER_DIVIDE, DIVIDE_BY_16);
    lapic::write(lapic::LVT_TIMER, TIMER_VECTOR);
    arm(ticks().wrapping_add(period));
}

/// Counter units per scheduler tick.
pub fn period() -> u64 {
    PERIOD.load(Ordering::Relaxed)
}

/// Has this CPU's timer interrupt fire at counter value `deadline`, replacing what
/// was set before. One that's already passed fires at once.
pub fn arm(deadline: u64) {
    let wait = deadline.saturating_sub(ticks()) as u128;
    let count = wait * apic_timer_hz() as u128 / frequency() as u128;
    // Zero would stop the timer. A wait too long to count just wakes early.
    lapic::write(lapic::TIMER_INITIAL, count.clamp(1, u128::from(u32::MAX)) as u32);
}
