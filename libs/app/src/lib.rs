//! The application interface for feos.
//!
//! Everything the shell can run is an [`App`]. Apps live in `apps/`, are
//! stateless, and receive a [`Context`] with their argument string, the current
//! display, and a [`Platform`] describing the running system.
//!
//! Apps deliberately do not depend on a board or architecture crate. Timers,
//! power control, CPU information and board facts arrive through the HAL
//! traits in [`Platform`], so the same app set works on every port.
//!
//! Adding an app:
//!
//! 1. Add a module (or a new crate) that exposes a unit struct implementing
//!    [`App`].
//! 2. Register it in the `REGISTRY` of `feos-apps` (or your own app crate).
//! 3. Add the crate to the kernel's dependencies if it is a separate crate.

#![no_std]

use feos_gfx::Canvas;
use feos_hal::{Audio, Clock, Cpu, Power};

/// The version shared by every crate in the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Static facts about the board the kernel is running on.
pub struct BoardInfo {
    /// Human-readable board name.
    pub name: &'static str,
    /// Base address of the console UART.
    pub console_base: usize,
    /// Physical address of the scanout buffer (0 when there is no display).
    pub framebuffer_addr: usize,
    /// Address range `[start, end)` of the loaded kernel image.
    pub kernel_range: (usize, usize),
    /// Address range `[start, end)` of `.bss`.
    pub bss_range: (usize, usize),
}

/// Services the kernel makes available to every app.
pub struct Platform {
    /// Static board facts.
    pub board: BoardInfo,
    /// Monotonic timer implementation.
    pub clock: &'static dyn Clock,
    /// Power/reset implementation.
    pub power: &'static dyn Power,
    /// CPU identification.
    pub cpu: &'static dyn Cpu,
    /// Audio output device, when the board found one.
    pub audio: Option<&'static dyn Audio>,
}

/// Everything an app is allowed to see when it runs.
pub struct Context<'a> {
    /// Arguments following the command name, trimmed but otherwise untouched.
    pub args: &'a str,
    /// The boot framebuffer, if the board provided one.
    ///
    /// The canvas is stable for the lifetime of the kernel; apps draw
    /// directly into it.
    pub display: &'a mut Option<Canvas<'static>>,
    /// Board facts and kernel services.
    pub platform: &'a Platform,
}

/// A shell application.
///
/// Implementations are zero-sized unit structs; the registry stores trait
/// objects, so `App` must be [`Sync`].
pub trait App: Sync {
    /// Primary command name.
    fn name(&self) -> &'static str;

    /// Alternative command names (the empty slice when there are none).
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    /// Short usage string shown in `help`, for example `sleep <ms>`.
    fn usage(&self) -> &'static str;

    /// One-line description of what the app does.
    fn description(&self) -> &'static str;

    /// Executes the app.
    fn run(&self, context: &mut Context<'_>);

    /// Returns `true` if `name` is the primary name or an alias.
    fn matches(&self, name: &str) -> bool {
        self.name() == name || self.aliases().contains(&name)
    }
}
