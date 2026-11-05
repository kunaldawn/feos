//! Hardware abstraction traits shared by feos drivers, architectures and
//! boards.
//!
//! Drivers implement device traits such as [`Serial`]. Architecture crates
//! implement the system-service traits ([`Clock`], [`Power`], [`Cpu`]), and the
//! kernel passes those implementations to apps through `feos-app`. This keeps
//! apps independent of a concrete architecture or board.
//!
//! This crate is intentionally dependency-free.

#![cfg_attr(not(test), no_std)]

pub mod arch;
pub mod display;
pub mod dma;
pub mod net;
pub mod time;

pub use arch::{Arch, FaultDetails, FirmwareConduit, PageAccess, PageBits, SecondaryEntry, TrapContext};

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

/// One key transition reported by a keyboard device.
///
/// `code` uses the Linux evdev key-code numbering (`KEY_A` is 30, `KEY_ENTER`
/// is 28, and so on). Decoding it into characters and modifiers is the job of
/// `feos-keys`, so the HAL stays independent of a keyboard layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyboardEvent {
    /// Linux evdev key code.
    pub code: u16,
    /// True for a press, false for a release.
    pub pressed: bool,
}

/// A polled keyboard device.
pub trait Keyboard: Sync {
    /// Returns the next key transition, or `None` when the queue is empty.
    fn poll(&self) -> Option<KeyboardEvent>;
}

/// A monotonic hardware counter.
pub trait Clock: Sync {
    /// Counter frequency in Hz.
    fn frequency(&self) -> u64;

    /// Current counter value.
    fn ticks(&self) -> u64;

    /// Milliseconds elapsed since the counter started.
    fn uptime_ms(&self) -> u64 {
        ticks_to_ms(self.ticks(), self.frequency())
    }

    /// Human-readable name of the counter, shown by diagnostics.
    fn name(&self) -> &'static str {
        "timer"
    }

    /// Busy-waits for roughly `ms` milliseconds.
    fn spin_ms(&self, ms: u64);

    /// Waits for at least `ms` milliseconds without burning CPU time.
    ///
    /// The kernel's clock blocks the calling process, so other processes run
    /// and the time is accounted as idle. The wake-up happens on a scheduler
    /// tick. The default falls back to the busy-wait so a port still works
    /// without idle support.
    fn idle_ms(&self, ms: u64) {
        self.spin_ms(ms);
    }

    /// Wall-clock seconds since the Unix epoch, when the machine has a clock
    /// that knows the date. The default is a port without one.
    fn unix_time(&self) -> Option<u64> {
        None
    }
}

/// Why a block device refused or failed a transfer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockIoError {
    /// The device reported a failure, or stopped answering.
    Io,
    /// The range falls outside the device, or isn't whole blocks.
    OutOfRange,
    /// The device is write-protected.
    ReadOnly,
}

/// A device that transfers whole blocks: a disk, seen from the driver's side.
///
/// Calls don't overlap, because the caller owns the device. `feos-fs` builds
/// partitions, a buffer cache and filesystems on top.
pub trait BlockIo: Send {
    /// Logical block size in bytes, a power of two.
    fn block_size(&self) -> usize;

    /// Number of blocks.
    fn block_count(&self) -> u64;

    /// True when writes will be refused.
    fn is_read_only(&self) -> bool {
        false
    }

    /// Reads whole blocks starting at `start`.
    fn read(&mut self, start: u64, out: &mut [u8]) -> Result<(), BlockIoError>;

    /// Writes whole blocks starting at `start`.
    fn write(&mut self, start: u64, data: &[u8]) -> Result<(), BlockIoError>;

    /// Returns once everything written so far is on the medium.
    fn flush(&mut self) -> Result<(), BlockIoError>;
}

/// Counters from the USB host controller, for proving things work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbStats {
    /// Devices configured.
    pub devices: u32,
    /// Keyboard reports taken.
    pub keyboard_reports: u32,
    /// Pointer reports taken.
    pub pointer_reports: u32,
    /// SCSI commands sent to disks.
    pub storage_commands: u32,
}

/// A battery-backed clock that knows the date.
pub trait RealTimeClock: Sync {
    /// Seconds since 1970-01-01 00:00:00 UTC, or `None` if the clock can't say.
    fn unix_time(&self) -> Option<u64>;
}

/// Converts a counter value to milliseconds.
///
/// The product needs 128 bits: at 3 GHz, `ticks * 1000` overflows `u64` after
/// about 71 days, and saturating there would freeze the clock. Returns 0 for a
/// zero frequency.
pub const fn ticks_to_ms(ticks: u64, frequency: u64) -> u64 {
    if frequency == 0 {
        return 0;
    }
    (ticks as u128 * 1000 / frequency as u128) as u64
}

/// The compare value after `previous` for a timer ticking every `period`.
///
/// Advancing by exactly one period keeps time spent reaching the handler from
/// accumulating as drift. After a stall longer than a period, this skips the
/// missed ticks rather than firing once per period until it has caught up.
pub const fn next_deadline(previous: u64, period: u64, now: u64) -> u64 {
    let next = previous.wrapping_add(period);
    if next.wrapping_sub(now) as i64 > 0 {
        next
    } else {
        now.wrapping_add(period)
    }
}

/// A snapshot of the kernel's memory accounting.
///
/// `total_bytes`, `used_bytes` and `free_bytes` describe the machine's RAM: the
/// kernel image, `.bss`, stacks, the heap and what's free. The heap-specific
/// figures let the task manager show commit charge separately from physical
/// memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MemoryInfo {
    /// RAM the machine has, as far as the kernel can use it.
    pub total_bytes: usize,
    /// Memory in use by the image, stacks and live heap allocations.
    pub used_bytes: usize,
    /// Memory currently available for new work.
    pub free_bytes: usize,
    /// Size of the kernel heap arena.
    pub heap_total_bytes: usize,
    /// Heap bytes handed out to live allocations.
    pub heap_used_bytes: usize,
    /// Heap bytes still on the free list.
    pub heap_free_bytes: usize,
}

/// Kernel memory accounting exposed to apps.
pub trait Memory: Sync {
    /// Returns a consistent snapshot of the system's memory usage.
    fn info(&self) -> MemoryInfo;
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
/// Drivers own the DMA buffers. Samples are interleaved signed 16-bit PCM at
/// [`sample_rate`] and [`channels`]. There are two ways to play them:
///
/// - Streaming: call [`write`] whenever [`available`] has room. It never
///   blocks, so a frame loop can keep running. Stop writing and the stream
///   plays out what is queued, then goes quiet by itself.
/// - Blocking: [`play`] does the same on behalf of the caller and returns once
///   the block has been consumed.
///
/// Writes from several processes interleave, so callers serialize them.
///
/// [`sample_rate`]: Audio::sample_rate
/// [`channels`]: Audio::channels
/// [`write`]: Audio::write
/// [`available`]: Audio::available
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

    /// Queues samples for playback without blocking.
    ///
    /// Returns how many interleaved samples were accepted, which may be fewer
    /// than offered (or zero when the queue is full). Only whole frames are
    /// accepted. Drivers without streaming support accept nothing.
    fn write(&self, _samples: &[i16]) -> usize {
        0
    }

    /// Number of interleaved samples [`write`](Audio::write) can accept now.
    fn available(&self) -> usize {
        0
    }

    /// Number of interleaved samples queued and not yet consumed.
    fn queued(&self) -> usize {
        0
    }

    /// Does periodic housekeeping for a stream nobody is writing to.
    ///
    /// The kernel calls this from its timer interrupt, so it must not block.
    fn service(&self) {}
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

    /// Acknowledges the highest-priority pending interrupt.
    ///
    /// Prefer this to [`acknowledge`](Self::acknowledge): the claim carries
    /// whatever the controller needs back at end-of-interrupt, such as the
    /// source CPU of a GIC SGI.
    fn claim(&self) -> IrqClaim {
        let id = self.acknowledge();
        IrqClaim { id, raw: id }
    }

    /// Signals end-of-interrupt for a claim from [`claim`](Self::claim).
    fn complete(&self, claim: IrqClaim) {
        self.end(claim.id);
    }

    /// Returns `true` when `id` means no interrupt was actually pending.
    ///
    /// Such an ID must not be completed.
    fn is_spurious(&self, id: u32) -> bool {
        id == 1023
    }

    /// Says how a device line behaves and which CPU takes it. Call before
    /// [`enable`](Self::enable). The default suits a controller with one CPU.
    fn configure(&self, _irq: u32, _trigger: IrqTrigger, _cpu: usize) {}

    /// Sends an inter-processor interrupt.
    ///
    /// `id` is a controller-specific number, published by the board. The
    /// default does nothing, which is correct for a single-CPU controller.
    fn send_ipi(&self, _target: IpiTarget, _id: u32) {}

    /// Interrupts every other CPU in a way masking can't hold off, for stopping
    /// the machine. Returns `false` when the controller can't, as a GICv2 can't.
    fn send_stop_nmi(&self) -> bool {
        false
    }
}

/// How a device signals an interrupt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IrqTrigger {
    /// The line stays asserted until the device is told to stop.
    Level,
    /// A pulse, which is what message-signalled interrupts become.
    Edge,
}

/// An acknowledged interrupt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IrqClaim {
    /// Interrupt ID.
    pub id: u32,
    /// Controller-specific acknowledge value, handed back at end-of-interrupt.
    pub raw: u32,
}

/// The CPUs an inter-processor interrupt goes to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IpiTarget {
    /// One logical CPU.
    Cpu(usize),
    /// Every CPU except the sender.
    AllOthers,
}

/// CPU identification and idle behaviour.
pub trait Cpu: Sync {
    /// Current exception level (0..=3).
    fn current_el(&self) -> u64;

    /// Main ID register, describing the CPU implementation.
    fn midr(&self) -> u64;

    /// Stops the CPU until an event occurs, forever.
    fn halt(&self) -> !;

    /// Current privilege level, formatted as the architecture names it.
    ///
    /// `current_el` and `midr` keep their numeric values for diagnostics; these
    /// labels stop AArch64 names from appearing on other ports.
    fn privilege_prefix(&self) -> &'static str {
        "PL"
    }

    /// Name of the identification value returned by [`midr`](Self::midr).
    fn id_name(&self) -> &'static str {
        "CPU ID"
    }
}

/// The facts and devices one machine provides to the kernel.
///
/// Each board crate implements this once. The kernel only calls boards through
/// this trait, so the compiler holds both ports to the same surface. Types the
/// HAL can't name, such as the display canvas, are associated types.
pub trait Board {
    /// Human-readable board name, shown by the `info` app.
    const NAME: &'static str;
    /// Architecture reported by `uname -m`.
    const ARCH: &'static str;
    /// Name of the serial device, used by the boot log.
    const CONSOLE_NAME: &'static str;
    /// Address of the console device.
    const CONSOLE_BASE: usize;
    /// Name of the pointer device family, used by the boot log.
    const POINTER_NAME: &'static str;
    /// Hint shown when no pointer was found.
    const POINTER_HINT: &'static str;
    /// Name of the keyboard device family, used by the boot log.
    const KEYBOARD_NAME: &'static str;
    /// Hint shown when no keyboard was found.
    const KEYBOARD_HINT: &'static str;
    /// Interrupt ID of the scheduler tick.
    const TIMER_IRQ: u32;
    /// Interrupt ID that stops a CPU for good.
    const IPI_STOP: u32;
    /// Interrupt ID that asks a CPU to reschedule.
    const IPI_RESCHEDULE: u32;
    /// CPUs the board can bring up.
    const MAX_CPUS: usize;
    /// RAM at or above this address isn't mapped cached, so it isn't used.
    const RAM_LIMIT: u64;

    /// The framebuffer canvas.
    type Display;
    /// A discovered audio device and its details.
    type AudioDevice;
    /// What kind of pointer was found.
    type PointerKind;
    /// The filesystem apps browse.
    type FileSystem: ?Sized + 'static;

    /// Initializes the UART and registers it as the kernel console.
    fn init_console();
    /// Writes terminal text directly to the UART, for the fatal path.
    ///
    /// This must not depend on any lock or on mutable state, which a failure may
    /// have corrupted.
    fn write_emergency(bytes: &[u8]);
    /// Reads what firmware says about the machine from the pointer it passed to
    /// the kernel. Runs once, first thing, with only the console up.
    fn init_boot_info(boot_info: usize);
    /// RAM ranges firmware reported, as `(base, size)`. Returns how many it wrote.
    /// None at all means the machine didn't say.
    fn usable_ram(out: &mut [(u64, u64)]) -> usize;
    /// How many CPUs firmware says this machine has, at most [`MAX_CPUS`](Self::MAX_CPUS).
    fn cpu_count() -> usize;
    /// Initializes the interrupt controller and the timer on the boot CPU.
    fn init_interrupts();
    /// Initializes the calling secondary CPU's interrupt state.
    fn init_cpu_interrupts();
    /// Returns the interrupt controller.
    fn interrupts() -> &'static dyn InterruptController;
    /// Name of the interrupt controller, used by the boot log.
    fn interrupt_controller_name() -> &'static str;
    /// Firmware affinity value for a logical CPU index.
    fn cpu_mpidr(index: usize) -> u64;

    /// Brings up the display, when the machine has one.
    fn init_display() -> Option<Self::Display>;
    /// Returns a canvas over the off-screen composition buffer.
    fn back_buffer() -> Option<Self::Display>;
    /// Physical address of the scanout buffer.
    fn framebuffer_addr() -> usize;

    /// Finds and initialises the audio device.
    fn init_audio(clock: &'static dyn Clock) -> Option<Self::AudioDevice>;
    /// Finds and initialises the pointer.
    fn init_pointer() -> Option<Self::PointerKind>;
    /// Returns the pointer shared with apps, when one was initialised.
    fn pointer() -> Option<&'static dyn Pointer>;
    /// Finds and initialises the keyboard.
    fn init_keyboard() -> bool;
    /// Returns the keyboard shared with apps, when one was initialised.
    fn keyboard() -> Option<&'static dyn Keyboard>;
    /// Everything below this, or below the kernel's image if that's lower, is off
    /// limits to the frame allocator.
    const LOW_RESERVED: u64 = u64::MAX;

    /// How firmware says it's reached.
    fn firmware_conduit() -> FirmwareConduit {
        FirmwareConduit::Unknown
    }
    /// Brings up the first network card, if the machine has one.
    fn init_network() -> Option<net::NetBinding> {
        None
    }
    /// Brings up the USB host controller, if the machine has one. Runs after the
    /// display, whose size a USB pointer maps to, and before anything asks for input
    /// devices or disks.
    fn init_usb(_clock: &'static dyn Clock) {}
    /// Counters from the USB host controller, when there is one.
    fn usb_stats() -> Option<UsbStats> {
        None
    }
    /// Returns the filesystem apps browse.
    fn filesystem() -> &'static Self::FileSystem;

    /// The driver behind the display, for the boot log.
    fn display_name() -> &'static str {
        "ramfb"
    }

    /// Wall-clock seconds since the Unix epoch, when the board has a clock.
    fn unix_time() -> Option<u64> {
        None
    }

    /// Address range `[start, end)` of the loaded kernel image.
    fn kernel_range() -> (usize, usize);
    /// Address range `[start, end)` of `.bss`.
    fn bss_range() -> (usize, usize);
    /// Address range `[start, end)` of the boot stack.
    fn stack_range() -> (usize, usize);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_is_exact_for_ordinary_values() {
        assert_eq!(ticks_to_ms(0, 1_000), 0);
        assert_eq!(ticks_to_ms(1_500, 1_000), 1_500);
        assert_eq!(ticks_to_ms(62_500_000, 62_500_000), 1_000);
        assert_eq!(ticks_to_ms(5, 0), 0);
    }

    #[test]
    fn uptime_does_not_saturate_after_71_days() {
        // KRN-19: `ticks * 1000` overflows u64 here, which froze the clock.
        const FREQUENCY: u64 = 3_000_000_000;
        let day_ms: u64 = 24 * 3600 * 1000;
        for days in [71, 72, 100, 10_000] {
            let ticks = days * day_ms * (FREQUENCY / 1000);
            assert_eq!(ticks_to_ms(ticks, FREQUENCY), days * day_ms);
        }
    }

    #[test]
    fn deadlines_advance_by_exactly_one_period() {
        // KRN-21: re-arming relative to "now" drifted by the handler latency.
        assert_eq!(next_deadline(1_000, 100, 1_030), 1_100);
    }

    #[test]
    fn deadlines_skip_missed_ticks_after_a_stall() {
        assert_eq!(next_deadline(1_000, 100, 1_100), 1_200);
        assert_eq!(next_deadline(1_000, 100, 5_000), 5_100);
    }

    #[test]
    fn deadlines_survive_counter_wrap() {
        assert_eq!(next_deadline(u64::MAX - 10, 100, u64::MAX - 5), 89);
    }

    struct Fake;

    impl InterruptController for Fake {
        fn enable(&self, _irq: u32) {}
        fn disable(&self, _irq: u32) {}
        fn acknowledge(&self) -> u32 {
            30
        }
        fn end(&self, _irq: u32) {}
    }

    #[test]
    fn controller_defaults_build_on_the_required_methods() {
        let claim = Fake.claim();
        assert_eq!(claim.id, 30);
        assert_eq!(claim.raw, 30);
        Fake.complete(claim);
        assert!(Fake.is_spurious(1023));
        assert!(!Fake.is_spurious(30));
        Fake.send_ipi(IpiTarget::AllOthers, 1);
    }
}
