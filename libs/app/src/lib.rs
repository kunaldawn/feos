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

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod args;
pub mod isolate;
pub mod pipe;
pub mod shell;

use alloc::boxed::Box;
use core::cell::Cell;

use feos_fs::FileSystem;
use feos_gfx::{Canvas, Point, Rect};
use feos_gui::{Event, Input};
use feos_hal::net::Network;
use feos_hal::{Audio, Clock, Cpu, Keyboard, Memory, Pointer, PointerEvent, Power};

pub use args::{ArgBlock, MAX_ARGC};

/// The version shared by every crate in the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Static facts about the board the kernel is running on.
pub struct BoardInfo {
    /// Human-readable board name.
    pub name: &'static str,
    /// Architecture name (`uname -m`), for example `aarch64` or `x86_64`.
    pub arch: &'static str,
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
pub const MAX_PROCESSES: usize = 32;

/// Maximum number of CPUs the scheduler can report in [`SystemInfo`].
///
/// The public figure matches the AArch64 port and QEMU's GICv2 limit of eight
/// cores. Ports with more CPUs can still report aggregate figures.
pub const MAX_CPUS: usize = 8;

/// Maximum length of a process name, in bytes.
pub const PROCESS_NAME_MAX: usize = 24;

/// Maximum argument string length copied into a new process, in bytes.
pub const PROCESS_ARGS_MAX: usize = 1024;

/// Lifecycle state of a process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProcessState {
    /// Runnable, waiting for the scheduler. Also reported for a blocked process;
    /// [`Processes::detail`] distinguishes the two.
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
    /// Scheduler priority, from 0 to [`PRIORITY_MAX`]. Larger is more urgent, and
    /// [`PRIORITY_NORMAL`] is what a process gets unless someone changes it.
    pub priority: u8,
    /// PID of the process that started this one, or zero for the first.
    pub parent: u32,
    /// Started as a background job, so an interrupt to its parent spares it.
    pub background: bool,
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
        parent: 0,
        background: false,
    };

    /// Returns the process name as a string.
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("?")
    }
}

/// Collects `pid` and its descendants from a snapshot, parents first, and
/// returns how many there are.
pub fn process_tree(infos: &[ProcessInfo], pid: u32, out: &mut [u32; MAX_PROCESSES]) -> usize {
    out[0] = pid;
    let mut count = 1;
    let mut next = 0;
    while next < count {
        let parent = out[next];
        next += 1;
        for info in infos {
            if info.parent == parent && count < MAX_PROCESSES && !out[..count].contains(&info.pid) {
                out[count] = info.pid;
                count += 1;
            }
        }
    }
    count
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
    /// Number of CPUs that have joined the scheduler.
    pub cpus: usize,
    /// Timer ticks accounted on each CPU, indexed by logical CPU.
    ///
    /// Only the first [`cpus`](Self::cpus) entries are meaningful. Summing
    /// these arrays on successive samples gives per-core CPU percentages
    /// without relying on the aggregate totals.
    pub cpu_ticks: [u64; MAX_CPUS],
    /// Idle ticks accounted on each CPU, indexed by logical CPU.
    pub cpu_idle_ticks: [u64; MAX_CPUS],
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
        cpus: 0,
        cpu_ticks: [0; MAX_CPUS],
        cpu_idle_ticks: [0; MAX_CPUS],
    };

    /// Returns the busy (non-idle) tick count.
    pub const fn busy_ticks(&self) -> u64 {
        self.total_ticks.saturating_sub(self.idle_ticks)
    }
}

/// The outcome of asking for a process to stop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KillResult {
    /// The process is gone.
    Killed,
    /// The process is flagged and goes at its next safe point: its owner's next
    /// tick once it holds no locks. See [`Processes::kill_requested`].
    Pending,
    /// A kill was already pending for this process.
    AlreadyPending,
    /// No process has this PID.
    NotFound,
    /// This process may not be killed: it is the main shell.
    Protected,
}

impl KillResult {
    /// Returns `true` when the process has stopped or is going to.
    pub const fn accepted(self) -> bool {
        matches!(self, Self::Killed | Self::Pending | Self::AlreadyPending)
    }
}

/// More about one process than [`ProcessInfo`] carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ProcessDetail {
    /// The process is blocked, sleeping or waiting for a lock.
    pub blocked: bool,
    /// A kill is waiting for the process's next safe point.
    pub kill_pending: bool,
    /// Size of the process's stack, in bytes.
    pub stack_size: usize,
    /// The most stack the process has used so far, in bytes.
    pub stack_used: usize,
    /// Heap bytes the process currently owns.
    pub heap_bytes: usize,
    /// Held off the CPU by job control.
    pub stopped: bool,
    /// The CPU whose queue the process is on.
    pub cpu: u8,
    /// The CPUs the process may run on, as a bit each. Zero means any.
    pub affinity: u32,
}

/// The priority of a process nobody has said anything about.
pub const PRIORITY_NORMAL: u8 = 8;
/// The highest priority.
pub const PRIORITY_MAX: u8 = 15;

/// What a process can look forward to, or what became of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaitStatus {
    /// It is still alive.
    Running,
    /// It finished with this status. A killed process reports [`EXIT_KILLED`].
    Exited(i32),
    /// No such process, or it finished so long ago that its status is gone.
    Unknown,
}

/// Status of a process that was killed: 128 plus SIGKILL, as a shell shows it.
pub const EXIT_KILLED: i32 = 137;

/// The console a new process gets. A field left `None` inherits the parent's.
#[derive(Clone, Copy, Default)]
pub struct SpawnIo<'a> {
    /// Where a user program's standard input comes from. Kernel-linked apps are given
    /// their streams some other way, and don't look at this.
    pub stdin: Option<&'a pipe::PipeReader>,
    /// Where the process's standard output goes.
    pub stdout: Option<&'a pipe::PipeWriter>,
    /// Where the process's standard error goes.
    pub stderr: Option<&'a pipe::PipeWriter>,
    /// The terminal the process belongs to, for its background children.
    pub console: Option<&'a pipe::PipeWriter>,
    /// Spared by an interrupt to the parent: see [`ProcessInfo::background`].
    pub background: bool,
}

/// Reasons a process could not be started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpawnError {
    /// Every process slot is in use.
    NoFreeSlot,
    /// The argument string did not fit in the process table.
    ArgsTooLong,
    /// This kernel, or test double, can't do what was asked.
    Unsupported,
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

    /// Starts `app` with exactly these arguments, each a word of its own, and
    /// the given console.
    ///
    /// The default joins the words, for simple test doubles.
    fn spawn_argv(
        &self,
        app: &'static dyn App,
        argv: &[&str],
        _io: SpawnIo<'_>,
    ) -> Result<u32, SpawnError> {
        let mut block = ArgBlock::new();
        if !block.push_all(argv) {
            return Err(SpawnError::ArgsTooLong);
        }
        let mut joined = [0u8; PROCESS_ARGS_MAX];
        self.spawn(app, block.joined(&mut joined))
    }

    /// Runs `task` as a new process. Its value is the exit status.
    ///
    /// A kill may stop the task without dropping what it owns, so anything that
    /// has to outlive it belongs to the kernel, as a pipe end does.
    fn spawn_task(
        &self,
        _name: &str,
        _io: SpawnIo<'_>,
        _task: Box<dyn FnOnce() -> i32 + Send>,
    ) -> Result<u32, SpawnError> {
        Err(SpawnError::Unsupported)
    }

    /// Starts the user program at `path` in a new process, with its own address space.
    /// `argv` doesn't include the program's name, and `cwd` is the directory relative
    /// paths in it start from. A program that can't be loaded isn't an error here: the
    /// process ends straight away with a status of 126 or 127.
    fn spawn_user(
        &self,
        _path: &str,
        _argv: &[&str],
        _cwd: &str,
        _io: SpawnIo<'_>,
    ) -> Result<u32, SpawnError> {
        Err(SpawnError::Unsupported)
    }

    /// Blocks until the process has finished and returns its status. `None`
    /// when there's no such process, or its status is gone.
    fn wait(&self, _pid: u32) -> Option<i32> {
        None
    }

    /// Looks at a process without blocking.
    fn try_wait(&self, _pid: u32) -> WaitStatus {
        WaitStatus::Unknown
    }

    /// Holds a process off the CPU (`stopped`) or lets it run again. Returns
    /// `false` when there's no such process, or it may not be stopped.
    fn set_stopped(&self, _pid: u32, _stopped: bool) -> bool {
        false
    }

    /// Stops the process with `pid`. Returns `false` when it does not exist or
    /// is the main shell process.
    ///
    /// `true` doesn't mean it has stopped yet; [`try_kill`](Self::try_kill)
    /// tells the two cases apart.
    fn kill(&self, pid: u32) -> bool;

    /// Stops the process with `pid` and reports exactly what happened.
    ///
    /// The default keeps simple test doubles working.
    fn try_kill(&self, pid: u32) -> KillResult {
        if self.kill(pid) {
            KillResult::Killed
        } else {
            KillResult::NotFound
        }
    }

    /// Returns `true` when someone has asked the calling process to stop.
    ///
    /// A process holding heap memory gets a short grace period before it is
    /// reaped. A long-running app that polls this and returns frees its memory
    /// normally; one that doesn't has its allocations written off as leaked.
    fn kill_requested(&self) -> bool {
        false
    }

    /// Offers the CPU to another runnable process.
    fn yield_now(&self) {}

    /// Blocks the calling process for at least `ms` milliseconds.
    ///
    /// The default returns at once, for test doubles.
    fn sleep_ms(&self, _ms: u64) {}

    /// Details of the process with `pid`, when it exists.
    fn detail(&self, _pid: u32) -> Option<ProcessDetail> {
        None
    }

    /// Sets how urgent a process is, from 0 to [`PRIORITY_MAX`]. Returns `false`
    /// when it can't be: no such process, or a priority out of range.
    fn set_priority(&self, _pid: u32, _priority: u8) -> bool {
        false
    }

    /// Sets the CPUs a process may run on, as a bit each. Returns `false` when it
    /// can't be: no such process, or a mask with no online CPU.
    fn set_affinity(&self, _pid: u32, _mask: u32) -> bool {
        false
    }

    /// Heap bytes lost so far to processes killed while they owned them.
    fn leaked_heap_bytes(&self) -> usize {
        0
    }

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
    /// The TCP/IP stack, on a machine with a network card.
    pub net: Option<&'static dyn Network>,
}

static GLOBAL_PLATFORM: core::sync::atomic::AtomicPtr<Platform> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Publishes the kernel's [`Platform`], once, for code that outlives its caller.
pub fn publish_platform(platform: &'static Platform) {
    let pointer = platform as *const Platform as *mut Platform;
    GLOBAL_PLATFORM.store(pointer, core::sync::atomic::Ordering::Release);
}

/// The kernel's [`Platform`]. A task of an app's own needs this, because a
/// [`Context`] lasts only as long as the call it came with. `None` on a host.
pub fn platform() -> Option<&'static Platform> {
    let pointer = GLOBAL_PLATFORM.load(core::sync::atomic::Ordering::Acquire);
    // SAFETY: only `publish_platform` writes this, from a `'static` reference.
    unsafe { pointer.cast_const().as_ref() }
}

/// Everything an app is allowed to see when it runs.
pub struct Context<'a> {
    /// Arguments following the command name, joined by single spaces.
    ///
    /// Good for a single free-form argument such as a path. Anything that takes
    /// several should read [`argv`](Self::argv), which keeps them apart.
    pub args: &'a str,
    /// Each argument following the command name, as the shell expanded it.
    pub argv: &'a [&'a str],
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
    /// Which window this is, for an app with [`App::multi_instance`]: stable
    /// while the window is open and less than the desktop's window count.
    pub instance: usize,
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
    /// The session asks on every pass, so an app can change its answer.
    fn animates(&self) -> bool {
        false
    }

    /// Minimum interval between automatic frames for an animating window, in
    /// milliseconds. The media player wants ~30 fps; a task manager's graphs
    /// are fine at 2 fps and should not force a full desktop repaint at 30 fps.
    fn animation_period_ms(&self) -> u32 {
        33
    }

    /// Opens a windowed session. Returns `false` when the app cannot start.
    fn open_window(&self, _context: &WindowContext<'_, '_>) -> bool {
        false
    }

    /// Draws and advances one windowed frame.
    fn frame_window(&self, _context: &mut WindowContext<'_, '_>) -> WindowFrame {
        WindowFrame::Close
    }

    /// Whether each launch opens another window, instead of raising the first.
    fn multi_instance(&self) -> bool {
        false
    }

    /// Releases any state owned by a windowed session.
    fn close_window(&self) {}

    /// Releases the state of one window of an app with several.
    fn close_instance(&self, _instance: usize) {
        self.close_window();
    }

    /// Advances a minimized window, which receives no frames.
    ///
    /// The desktop calls this about ten times a second while the window is
    /// minimized, with no canvas and no input. Use it to keep sampling or
    /// playback state moving.
    fn background_tick(&self, _platform: &Platform, _now_ms: u64) {}

    /// Offers a dragged path to an open window. Returns `true` when the app
    /// took it.
    fn accept_drop(&self, _payload: &str) -> bool {
        false
    }

    /// Whether the app must run where the framebuffer is, instead of in a
    /// process of its own. Everything else run by a shell gets its own process
    /// and console, so its output can be captured and it can be interrupted.
    fn needs_display(&self) -> bool {
        self.foreground()
    }

    /// Executes the app and returns its exit status: zero means success.
    fn run(&self, context: &mut Context<'_>) -> i32;

    /// Returns `true` if `name` is the primary name or an alias.
    fn matches(&self, name: &str) -> bool {
        self.name() == name || self.aliases().contains(&name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Double;

    impl Processes for Double {
        fn spawn(&self, _app: &'static dyn App, _args: &str) -> Result<u32, SpawnError> {
            Err(SpawnError::NoFreeSlot)
        }

        fn kill(&self, pid: u32) -> bool {
            pid == 2
        }

        fn current(&self) -> u32 {
            1
        }

        fn snapshot(&self, _out: &mut [ProcessInfo]) -> usize {
            0
        }
    }

    #[test]
    fn spawn_task_defaults_to_unsupported() {
        let result = Double.spawn_task("x", SpawnIo::default(), Box::new(|| 0));
        assert_eq!(result, Err(SpawnError::Unsupported));
        assert_eq!(Double.try_wait(2), WaitStatus::Unknown);
    }

    #[test]
    fn try_kill_defaults_to_the_bool_answer() {
        assert_eq!(Double.try_kill(2), KillResult::Killed);
        assert_eq!(Double.try_kill(3), KillResult::NotFound);
        assert!(!Double.kill_requested());
        assert_eq!(Double.detail(2), None);
    }

    #[test]
    fn only_some_kill_results_count_as_accepted() {
        assert!(KillResult::Killed.accepted());
        assert!(KillResult::Pending.accepted());
        assert!(KillResult::AlreadyPending.accepted());
        assert!(!KillResult::NotFound.accepted());
        assert!(!KillResult::Protected.accepted());
    }
}
