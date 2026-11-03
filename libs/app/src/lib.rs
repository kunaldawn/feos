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

use core::cell::Cell;

use feos_fs::FileSystem;
use feos_gfx::{Canvas, Point, Rect};
use feos_gui::{Event, Input};
use feos_hal::{Audio, Clock, Cpu, Keyboard, Memory, Pointer, PointerEvent, Power};

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
pub const MAX_PROCESSES: usize = 16;

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
    /// Scheduler ticks consumed by this process since it was created.
    ///
    /// The kernel tick rate is reported by [`SystemInfo::ticks_per_second`],
    /// so a task manager can turn differences between two snapshots into a
    /// CPU percentage without knowing the timer frequency.
    pub cpu_ticks: u64,
    /// Milliseconds between system start and process creation.
    pub started_ms: u64,
    /// Resident stack/process memory charged to this process, in bytes.
    pub memory_bytes: usize,
    /// Number of threads in the process (currently always one).
    pub threads: u16,
    /// Scheduler priority (currently a single normal priority).
    pub priority: u8,
}

impl ProcessInfo {
    /// An empty entry, useful for initializing a snapshot array.
    pub const EMPTY: Self = Self {
        pid: 0,
        state: ProcessState::Exited,
        name: [0; PROCESS_NAME_MAX],
        name_len: 0,
        cpu_ticks: 0,
        started_ms: 0,
        memory_bytes: 0,
        threads: 0,
        priority: 0,
    };

    /// Returns the process name as a string.
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("?")
    }
}

/// System-wide scheduler and process accounting.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SystemInfo {
    /// Milliseconds since boot.
    pub uptime_ms: u64,
    /// Timer ticks elapsed since boot.
    pub total_ticks: u64,
    /// Ticks the CPU spent waiting in an idle instruction.
    pub idle_ticks: u64,
    /// Scheduler tick rate in Hz.
    pub ticks_per_second: u64,
    /// Number of live processes.
    pub processes: usize,
    /// Number of live threads.
    pub threads: usize,
}

impl SystemInfo {
    /// An all-zero snapshot.
    pub const EMPTY: Self = Self {
        uptime_ms: 0,
        total_ticks: 0,
        idle_ticks: 0,
        ticks_per_second: 0,
        processes: 0,
        threads: 0,
    };

    /// Returns the busy (non-idle) tick count.
    pub const fn busy_ticks(&self) -> u64 {
        self.total_ticks.saturating_sub(self.idle_ticks)
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

    /// Returns system-wide scheduler accounting.
    ///
    /// The default keeps simple test doubles working; the kernel implements
    /// real values.
    fn system(&self) -> SystemInfo {
        SystemInfo::EMPTY
    }
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
    /// Pointer device, when the board found one.
    pub pointer: Option<&'static dyn Pointer>,
    /// Keyboard device, when the board found one.
    pub keyboard: Option<&'static dyn Keyboard>,
    /// Process creation and inspection.
    pub processes: &'static dyn Processes,
    /// Kernel memory accounting, when the board provides it.
    pub memory: Option<&'static dyn Memory>,
    /// Lookup table the desktop and init manager resolve entries through.
    pub apps: &'static dyn AppRegistry,
    /// Read-only filesystem exposed by the board, when it has one.
    pub filesystem: Option<&'static dyn FileSystem>,
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
    /// The part of the framebuffer the app may use, when it was launched from
    /// the desktop.
    ///
    /// `None` means the whole framebuffer. The desktop passes its work area
    /// (the screen minus the taskbar) so a foreground app such as the file
    /// explorer can draw full-screen without hiding the taskbar.
    pub viewport: Option<Rect>,
    /// Set to `true` when the user clicks this app's taskbar button.
    ///
    /// An app that owns the framebuffer should watch
    /// [`suspend_area`](Self::suspend_area) and return from `App::run` when it
    /// is clicked; the desktop then minimises the window and can restore and
    /// run the app again from the taskbar.
    pub suspend: Option<&'a Cell<bool>>,
    /// The taskbar button that belongs to this app, in framebuffer
    /// coordinates. `None` when the app was not launched from the desktop.
    pub suspend_area: Option<Rect>,
    /// Board facts and kernel services.
    pub platform: &'a Platform,
}

/// Result of one windowed application frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowFrame {
    /// Keep the window open.
    Continue,
    /// The application asked to close its window.
    Close,
}

/// Everything a windowed application sees for one frame.
pub struct WindowContext<'a, 'b> {
    /// Arguments the application was opened with.
    pub args: &'a str,
    /// Drawing surface for the window content.
    pub canvas: &'a mut Canvas<'b>,
    /// Input events for this frame.
    pub input: &'a Input,
    /// Board facts and kernel services.
    pub platform: &'a Platform,
    /// Milliseconds since boot.
    pub now_ms: u64,
    /// Content origin in framebuffer coordinates.
    pub origin: Point,
    /// Whether pointer events belong to this window this frame.
    pub pointer_active: bool,
    /// Whether keyboard events belong to this window this frame.
    pub keyboard_active: bool,
}

impl WindowContext<'_, '_> {
    /// The pointer position in window-content coordinates.
    pub fn pointer(&self) -> Point {
        let pointer = self.input.pointer();
        Point::new(pointer.x - self.origin.x, pointer.y - self.origin.y)
    }

    /// Dispatches this frame's pointer events in content coordinates.
    pub fn dispatch_pointer(&self, mut handler: impl FnMut(PointerEvent)) {
        if !self.pointer_active {
            return;
        }
        let origin = self.origin;
        let pointer = self.pointer();
        let position = |x: i32, y: i32| {
            (
                (x - origin.x).clamp(0, u16::MAX as i32) as u16,
                (y - origin.y).clamp(0, u16::MAX as i32) as u16,
            )
        };

        for event in self.input.events() {
            match event {
                Event::PointerMoved(pos) => {
                    let (x, y) = position(pos.x, pos.y);
                    handler(PointerEvent {
                        x,
                        y,
                        ..PointerEvent::default()
                    });
                }
                Event::PointerDown { pos, button } => {
                    let (x, y) = position(pos.x, pos.y);
                    let mut report = PointerEvent {
                        x,
                        y,
                        ..PointerEvent::default()
                    };
                    match button {
                        feos_gui::MouseButton::Left => report.left = Some(true),
                        feos_gui::MouseButton::Right => report.right = Some(true),
                        feos_gui::MouseButton::Middle => report.middle = Some(true),
                    }
                    handler(report);
                }
                Event::PointerUp { pos, button } => {
                    let (x, y) = position(pos.x, pos.y);
                    let mut report = PointerEvent {
                        x,
                        y,
                        ..PointerEvent::default()
                    };
                    match button {
                        feos_gui::MouseButton::Left => report.left = Some(false),
                        feos_gui::MouseButton::Right => report.right = Some(false),
                        feos_gui::MouseButton::Middle => report.middle = Some(false),
                    }
                    handler(report);
                }
                Event::PointerWheel(delta) => handler(PointerEvent {
                    x: pointer.x.clamp(0, u16::MAX as i32) as u16,
                    y: pointer.y.clamp(0, u16::MAX as i32) as u16,
                    wheel: delta,
                    ..PointerEvent::default()
                }),
                _ => {}
            }
        }
    }

    /// Dispatches keyboard and text events.
    pub fn dispatch_keys(&self, mut handler: impl FnMut(Event)) {
        if !self.keyboard_active {
            return;
        }
        for event in self.input.events() {
            match event {
                Event::KeyDown(_, _) | Event::Text(_) => handler(event),
                _ => {}
            }
        }
    }
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

    /// Whether the app needs the whole framebuffer and should run in the
    /// foreground instead of as a background process.
    ///
    /// The desktop opens a window for every launch, but an app that returns
    /// `true` here is run synchronously by the desktop session so it can draw
    /// and handle its own input. Pressing the app's exit key returns to the
    /// desktop.
    fn foreground(&self) -> bool {
        false
    }

    /// Whether the desktop should host this app in a managed window.
    ///
    /// Windowed apps are driven one frame at a time by the desktop instead of
    /// taking over the screen from a blocking `run` loop. That keeps desktop
    /// icons, the taskbar and the Start menu responsive while the app runs.
    fn windowed(&self) -> bool {
        false
    }

    /// Whether an open window needs periodic frames even without input.
    ///
    /// The media player and task manager return `true` so playback and live
    /// graphs keep advancing; ordinary document-style apps leave it `false`.
    fn animates(&self) -> bool {
        false
    }

    /// Opens a windowed session. Returns `false` when the app cannot start.
    fn open_window(&self, _context: &WindowContext<'_, '_>) -> bool {
        false
    }

    /// Draws and advances one windowed frame.
    fn frame_window(&self, _context: &mut WindowContext<'_, '_>) -> WindowFrame {
        WindowFrame::Close
    }

    /// Releases any state owned by a windowed session.
    fn close_window(&self) {}

    /// Executes the app.
    fn run(&self, context: &mut Context<'_>);

    /// Returns `true` if `name` is the primary name or an alias.
    fn matches(&self, name: &str) -> bool {
        self.name() == name || self.aliases().contains(&name)
    }
}
