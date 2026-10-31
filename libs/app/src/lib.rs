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

/// Maximum number of processes the kernel can track at once.
pub const MAX_PROCESSES: usize = 8;

/// Maximum length of a process name, in bytes.
pub const PROCESS_NAME_MAX: usize = 24;

/// Maximum argument string length copied into a new process, in bytes.
pub const PROCESS_ARGS_MAX: usize = 96;

/// Lifecycle state of a process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProcessState {
    /// Runnable, waiting for the scheduler.
    Ready,
    /// Currently executing.
    Running,
    /// Finished; its slot can be reused.
    Exited,
}

/// Snapshot of one process for `ps`-style inspection.
#[derive(Clone, Copy)]
pub struct ProcessInfo {
    /// Process identifier (the main shell is PID 1).
    pub pid: u32,
    /// Current lifecycle state.
    pub state: ProcessState,
    /// UTF-8 process name, `name_len` bytes long.
    pub name: [u8; PROCESS_NAME_MAX],
    /// Number of valid bytes in `name`.
    pub name_len: u8,
}

impl ProcessInfo {
    /// An empty entry, useful for initializing a snapshot array.
    pub const EMPTY: Self = Self {
        pid: 0,
        state: ProcessState::Exited,
        name: [0; PROCESS_NAME_MAX],
        name_len: 0,
    };

    /// Returns the process name as a string.
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("?")
    }
}

/// Reasons a process could not be started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpawnError {
    /// Every process slot is in use.
    NoFreeSlot,
    /// The argument string did not fit in the process table.
    ArgsTooLong,
}

/// Kernel process-management service.
///
/// Each process has its own stack and saved register context. The kernel
/// preempts processes on a timer tick and round-robins between them, so an app
/// launched with [`spawn`](Processes::spawn) runs concurrently with the shell
/// and with other background apps.
///
/// Background processes receive their own argument string and the [`Platform`]
/// services, but no framebuffer: graphics apps should run in the foreground.
pub trait Processes: Sync {
    /// Starts `app` in a new process with `args`.
    fn spawn(&self, app: &'static dyn App, args: &str) -> Result<u32, SpawnError>;

    /// Stops the process with `pid`. Returns `false` when it does not exist or
    /// is the main shell process.
    fn kill(&self, pid: u32) -> bool;

    /// PID of the process calling this method.
    fn current(&self) -> u32;

    /// Copies a snapshot of live processes into `out` and returns its length.
    fn snapshot(&self, out: &mut [ProcessInfo]) -> usize;
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
    /// Process creation and inspection.
    pub processes: &'static dyn Processes,
    /// Lookup table the desktop and init manager resolve entries through.
    pub apps: &'static dyn AppRegistry,
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

/// A lookup table for applications by command name.
///
/// The shell's registry and the desktop's launcher both need to turn a name
/// from a manifest or configuration into an [`App`]. Keeping that operation
/// behind a trait lets the desktop and init manager live in libraries that do
/// not depend on the app registry crate.
pub trait AppRegistry: Sync {
    /// Finds an app by primary name or alias.
    fn find(&self, name: &str) -> Option<&'static dyn App>;
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
