//! Hardware abstraction traits shared by feos drivers, architectures and
//! boards.
//!
//! Drivers implement device traits such as [`Serial`]. Architecture crates
//! implement the system-service traits ([`Clock`], [`Power`], [`Cpu`]), and the
//! kernel passes those implementations to apps through `feos-app`. This keeps
//! apps independent of a concrete architecture or board.
//!
//! This crate is intentionally dependency-free.

#![no_std]

/// A byte-oriented character device (UART, virtual console, ...).
///
/// Implementations are polled: [`read_byte`](Serial::read_byte) returns `None`
/// when no byte is available instead of blocking, and
/// [`write_byte`](Serial::write_byte) blocks until the byte can be accepted.
pub trait Serial {
    /// Programs the device for operation.
    fn init(&self);

    /// Writes one byte, waiting for room in the transmit FIFO.
    fn write_byte(&self, byte: u8);

    /// Reads one byte if one is available.
    fn read_byte(&self) -> Option<u8>;
}

/// A monotonic hardware counter.
pub trait Clock: Sync {
    /// Counter frequency in Hz.
    fn frequency(&self) -> u64;

    /// Current counter value.
    fn ticks(&self) -> u64;

    /// Milliseconds elapsed since the counter started.
    fn uptime_ms(&self) -> u64 {
        self.ticks()
            .saturating_mul(1000)
            .checked_div(self.frequency())
            .unwrap_or(0)
    }

    /// Busy-waits for roughly `ms` milliseconds.
    fn spin_ms(&self, ms: u64);
}

/// System power control.
pub trait Power: Sync {
    /// Powers the machine off. Does not return.
    fn system_off(&self) -> !;

    /// Resets the machine. Does not return.
    fn system_reset(&self) -> !;
}

/// CPU identification and idle behaviour.
pub trait Cpu: Sync {
    /// Current exception level (0..=3).
    fn current_el(&self) -> u64;

    /// Main ID register, describing the CPU implementation.
    fn midr(&self) -> u64;

    /// Stops the CPU until an event occurs, forever.
    fn halt(&self) -> !;
}
