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

/// An audio output device.
///
/// Drivers own the DMA buffers and present a simple "play this block" API.
/// Samples are interleaved signed 16-bit PCM at [`sample_rate`] and
/// [`channels`]; [`play`] blocks until the hardware has consumed the block.
///
/// [`sample_rate`]: Audio::sample_rate
/// [`channels`]: Audio::channels
/// [`play`]: Audio::play
pub trait Audio: Sync {
    /// Playback sample rate in Hz.
    fn sample_rate(&self) -> u32;

    /// Number of interleaved channels (usually 2).
    fn channels(&self) -> u16;

    /// Returns `true` when a device is present and configured.
    fn is_ready(&self) -> bool;

    /// Plays one interleaved PCM block, blocking until it has been consumed.
    ///
    /// Blocks longer than [`max_samples`](Audio::max_samples) are rejected
    /// with [`AudioError::TooLong`]; callers should size their buffers to the
    /// device limit.
    fn play(&self, samples: &[i16]) -> Result<(), AudioError>;

    /// Stops playback immediately and discards queued samples.
    fn stop(&self);

    /// Maximum number of interleaved samples accepted by one [`play`] call.
    ///
    /// [`play`]: Audio::play
    fn max_samples(&self) -> usize;
}

/// Errors reported by an [`Audio`] device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioError {
    /// No audio device was found or it failed to initialise.
    Unavailable,
    /// The block was empty.
    Empty,
    /// The block is longer than [`Audio::max_samples`].
    TooLong,
    /// The device did not finish the transfer in time.
    Timeout,
}

/// One pointer report produced between two input frames.
///
/// A report carries the absolute cursor position plus the button transitions
/// that happened since the previous report. `Some(true)` means the button was
/// pressed, `Some(false)` means it was released; `None` leaves it unchanged.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PointerEvent {
    /// Cursor x coordinate in framebuffer pixels.
    pub x: u16,
    /// Cursor y coordinate in framebuffer pixels.
    pub y: u16,
    /// Left-button transition.
    pub left: Option<bool>,
    /// Right-button transition.
    pub right: Option<bool>,
    /// Middle-button transition.
    pub middle: Option<bool>,
    /// Wheel movement in notches (positive is away from the user).
    pub wheel: i32,
}

impl PointerEvent {
    /// Returns `true` when the report contains no button or wheel change.
    pub const fn is_position_only(&self) -> bool {
        self.left.is_none() && self.right.is_none() && self.middle.is_none() && self.wheel == 0
    }
}

/// A relative or absolute pointing device.
///
/// The device is polled: [`poll`](Pointer::poll) returns one complete report
/// at a time, or `None` when no new input has arrived. Implementations keep
/// the cursor position between calls.
pub trait Pointer: Sync {
    /// Returns the next report, if any.
    fn poll(&self) -> Option<PointerEvent>;

    /// Current cursor position in framebuffer pixels.
    fn position(&self) -> (u16, u16);
}

/// A programmable interrupt controller.
///
/// The kernel installs one implementation at boot and enables the interrupts
/// it needs (today: the generic timer tick used by the scheduler). Handlers
/// acknowledge the interrupt, service it, and then end it so the controller
/// can deliver the next one.
pub trait InterruptController: Sync {
    /// Enables delivery of one interrupt ID.
    fn enable(&self, irq: u32);

    /// Disables delivery of one interrupt ID.
    fn disable(&self, irq: u32);

    /// Returns the ID of the highest-priority pending interrupt.
    ///
    /// A spurious interrupt is reported as `1023`.
    fn acknowledge(&self) -> u32;

    /// Signals end-of-interrupt for a previously acknowledged ID.
    fn end(&self, irq: u32);
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
