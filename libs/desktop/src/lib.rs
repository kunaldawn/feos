//! A configurable desktop environment for feos.
//!
//! The desktop is the session layer above the GUI toolkit. It owns the
//! wallpaper, application shortcuts, the window manager, the classic-style taskbar
//! and Start menu, and an init/service supervisor. Everything is driven by a
//! YAML [`DesktopConfig`], so a board can change the look and the startup
//! behaviour without touching code.
//!
//! Applications are discovered from an *application folder*: a list of files
//! handed to [`Desktop::scan`]. Each file is a single `.fapp` bundle — one ZIP
//! or TAR archive containing `manifest.yaml`, an icon and any resources. The
//! scanner detects bundles from their magic bytes, so renaming a bundle does
//! not hide it. The QEMU port embeds its folder in the kernel image; a board
//! with a filesystem passes the same [`DirEntry`] list at run time.
//!
//! # Drawing and input
//!
//! [`Desktop::render`] is immediate mode: the caller supplies a canvas, the
//! input for this frame and the clock, and receives a [`DesktopResponse`] with
//! any launch request or session command. Rendering never spawns a process by
//! itself; the caller decides whether a request becomes a kernel process,
//! which keeps the frame logic testable on the host. [`Desktop::launch`] is
//! the separate entry point that performs the lookup and spawn.
//!
//! A session that keeps its last frame calls [`Desktop::set_retained`]. Every
//! frame still runs all of its hit testing, but paints only the part of the
//! screen that changed and reports it in [`DesktopResponse::damage`].
//!
//! ```no_run
//! use feos_desktop::{Desktop, DesktopConfig};
//! use feos_gui::Input;
//!
//! let config = DesktopConfig::parse("theme: xp-blue\n").unwrap();
//! let mut desktop = Desktop::<8, 12, 12>::new(config);
//! let input = Input::new();
//! // let response = desktop.render(&mut canvas, &input, uptime_ms);
//! ```

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod config;
pub mod keyboard;
pub mod notify;
pub mod volume;
pub mod paint;
pub mod start_menu;
pub mod taskbar;
pub mod theme;
pub mod window;

#[cfg(test)]
mod scripted;

use core::fmt::Write as _;

use feos_app::isolate;
use feos_app::shell::{Level, Notification};
use feos_app::{
    App, AppRegistry, Platform, ProcessInfo, ProcessState, Processes, SpawnError, WindowContext,
    WindowFrame, MAX_PROCESSES,
};
use feos_bundle::{AppCatalog, InstalledApp, ScanReport};
use feos_gfx::{Canvas, Color, Point, Rect};
use feos_gui::{
    Align, Event, Id, Input, InteractionState, Key, Modifiers, RectExt, Sense, Ui, VerticalAlign,
};
use feos_init::{Init, MAX_SERVICES};
use feos_text::Text;

use notify::Notifications;

pub use config::{
    ConfigError, DesktopConfig, ShortcutConfig, StartMenuConfig, TaskbarConfig, TaskbarPosition,
    WallpaperConfig, WallpaperPreset, WallpaperStyle, WindowConfig,
};
pub use feos_bundle::{BundleFormat, DirEntry};
pub use feos_init::{InitError, RestartPolicy, ServiceState, ServiceStatus};
pub use keyboard::KeyboardLayout;
pub use paint::CursorShape;
pub use start_menu::{StartMenuCommand, StartMenuState};
pub use theme::{DesktopTheme, ThemeName};
pub use window::{
    DesktopWindow, ResizeEdge, WindowManager, WindowState, WindowStatus, MAX_WINDOWS,
    MIN_WINDOW_HEIGHT, MIN_WINDOW_WIDTH,
};

/// Default maximum number of applications in the catalog.
pub const MAX_APPS: usize = 12;

/// Maximum number of shortcut cells drawn on the wallpaper.
pub const MAX_DESKTOP_ICONS: usize = 24;

/// Thickness of the resize border along each window edge.
const RESIZE_EDGE: i32 = 5;
/// Size of a resize corner.
const RESIZE_CORNER: i32 = 14;

/// A request to launch a catalog application.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LaunchRequest {
    /// Index into the application catalog.
    pub app: usize,
    /// Shell registry entry point from the manifest.
    pub entry: Text<32>,
    /// Arguments for this launch: the window's own, or the manifest's.
    pub args: Text<96>,
    /// Window opened for the application.
    pub window: usize,
}

/// What happened when a launch request was handed to the platform.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LaunchOutcome {
    /// A new process was created.
    Spawned {
        /// Window that displays the application.
        window: usize,
        /// Kernel process id.
        pid: u32,
    },
    /// The app runs in the foreground and needs the full framebuffer.
    Foreground {
        /// Window opened for the application.
        window: usize,
    },
    /// The app runs inside its desktop window and is driven by the desktop.
    Windowed {
        /// Window hosting the application.
        window: usize,
    },
    /// The window was already running this application.
    AlreadyRunning {
        /// Window that displays the application.
        window: usize,
        /// Existing process id, when still known.
        pid: Option<u32>,
    },
    /// The catalog index does not exist.
    UnknownApp,
    /// The window table is full.
    NoWindowSlot,
    /// The manifest entry point is not in the shell registry.
    UnknownEntry,
    /// The kernel refused to create a process.
    SpawnFailed(SpawnError),
}

/// Result of one rendered frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DesktopResponse {
    /// The user asked to launch an application.
    pub launch: Option<LaunchRequest>,
    /// A Start-menu command, when one was chosen.
    pub command: StartMenuCommand,
    /// A command typed into the Run dialog.
    pub run: Option<Text<96>>,
    /// The notification area was clicked.
    pub tray_clicked: bool,
    /// Start menu visibility after the frame.
    pub start_open: bool,
    /// Number of open windows after the frame.
    pub window_count: usize,
    /// A windowed app is animating and the caller should render another frame.
    pub redraw: bool,
    /// Minimum interval between animated frames, in milliseconds (0 when no
    /// window is animating).
    pub animation_period_ms: u32,
    /// The part of the canvas this frame changed. `None` when it changed
    /// nothing, which only a retained session sees; a caller presenting from
    /// an off-screen buffer copies just this rectangle.
    pub damage: Option<Rect>,
    /// Desktop state changed after the frame was painted (a window closed, say),
    /// so one more frame is needed to show the result.
    pub state_changed: bool,
    /// The pointer shape for this position, for a caller drawing its own.
    pub cursor: CursorShape,
}

impl DesktopResponse {
    /// Returns `true` when the result is empty.
    pub fn is_empty(&self) -> bool {
        self.launch.is_none()
            && self.command == StartMenuCommand::None
            && self.run.is_none()
            && !self.tray_clicked
    }
}

/// A command chosen from the desktop's right-click menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum ContextAction {
    #[default]
    None,
    Refresh,
    ShowDesktop,
    TaskManager,
    Properties,
}

const CONTEXT_ITEMS: [MenuItem<ContextAction>; 4] = [
    MenuItem::new("Refresh", ContextAction::Refresh, false),
    MenuItem::new("Show Desktop", ContextAction::ShowDesktop, false),
    MenuItem::new("Task Manager", ContextAction::TaskManager, true),
    MenuItem::new("Properties", ContextAction::Properties, true),
];

/// A command chosen from a window's system menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum WindowAction {
    #[default]
    None,
    Restore,
    Minimize,
    Maximize,
    Close,
}

const WINDOW_ITEMS: [MenuItem<WindowAction>; 4] = [
    MenuItem::new("Restore", WindowAction::Restore, false),
    MenuItem::new("Minimize", WindowAction::Minimize, false),
    MenuItem::new("Maximize", WindowAction::Maximize, false),
    MenuItem::new("Close  Alt+F4", WindowAction::Close, true),
];

/// One row of a pop-up menu.
#[derive(Clone, Copy)]
struct MenuItem<T: Copy> {
    label: &'static str,
    action: T,
    separator_before: bool,
}

impl<T: Copy> MenuItem<T> {
    const fn new(label: &'static str, action: T, separator_before: bool) -> Self {
        Self {
            label,
            action,
            separator_before,
        }
    }
}

/// Position and highlight of a pop-up menu.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ContextMenuState {
    open: bool,
    x: i32,
    y: i32,
    highlighted: usize,
}

impl ContextMenuState {
    const CLOSED: Self = Self {
        open: false,
        x: 0,
        y: 0,
        highlighted: 0,
    };

    fn show(&mut self, point: Point) {
        *self = Self {
            open: true,
            x: point.x,
            y: point.y,
            highlighted: NOTHING_HIGHLIGHTED,
        };
    }
}

/// Where a typed command should go.
#[derive(Clone, Copy)]
pub enum CommandTarget<'a> {
    /// Open a catalog bundle, which is how a windowed app gets its window.
    Bundle {
        /// Catalog index.
        index: usize,
        /// Arguments replacing the manifest's, if any were typed.
        args: Option<&'a str>,
    },
    /// Run the app through the synchronous foreground path.
    Foreground {
        /// The resolved app.
        app: &'static dyn App,
        /// Arguments.
        args: &'a str,
    },
    /// Start the app as a background process.
    Spawn {
        /// The resolved app.
        app: &'static dyn App,
        /// Arguments.
        args: &'a str,
    },
    /// Nothing has that name.
    NotFound,
}

/// The "Run..." dialog.
#[derive(Clone, Copy, Debug)]
struct RunDialogState {
    open: bool,
    text: Text<96>,
}

impl RunDialogState {
    const fn new() -> Self {
        Self {
            open: false,
            text: Text::EMPTY,
        }
    }

    fn show(&mut self) {
        self.open = true;
        self.text.clear();
    }

    fn close(&mut self) {
        self.open = false;
        self.text.clear();
    }
}

/// What the user chose in the "Turn off computer" dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum ShutdownAction {
    #[default]
    None,
    TurnOff,
    Restart,
    Cancel,
}

/// The XP "Turn off computer" dialog.
#[derive(Clone, Copy, Debug, Default)]
struct ShutdownDialogState {
    open: bool,
}

/// What the user chose in the "Log Off" dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum LogoffAction {
    #[default]
    None,
    LogOff,
    Cancel,
}

/// Remembers the previous click so icons and title bars can distinguish a
/// single click from a double click.
///
/// A double click is a second click on the same target within half a second
/// and within a few pixels of the first. After a double click the tracker is
/// cleared, so a third click starts a new pair instead of opening two windows.
#[derive(Clone, Copy, Debug, Default)]
struct ClickMemory {
    target: u32,
    at_ms: u64,
    x: i32,
    y: i32,
    valid: bool,
}

impl ClickMemory {
    const INTERVAL_MS: u64 = 500;
    const SLOP: i32 = 4;

    /// Forgets the last click, so the next one cannot complete a pair.
    fn forget(&mut self) {
        self.valid = false;
    }

    fn hit(&mut self, target: u32, position: Point, now_ms: u64) -> bool {
        let double = self.valid
            && self.target == target
            && now_ms.saturating_sub(self.at_ms) <= Self::INTERVAL_MS
            && (position.x - self.x).abs() <= Self::SLOP
            && (position.y - self.y).abs() <= Self::SLOP;

        if double {
            self.valid = false;
        } else {
            self.target = target;
            self.at_ms = now_ms;
            self.x = position.x;
            self.y = position.y;
            self.valid = true;
        }
        double
    }
}

/// What decided how one window was last painted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PaintedWindow {
    open: bool,
    state: WindowState,
    rect: Rect,
    z: u32,
    focused: bool,
    hosted: bool,
    started: bool,
    launched: bool,
    status: WindowStatus,
}

impl PaintedWindow {
    const CLOSED: Self = Self {
        open: false,
        state: WindowState::Normal,
        rect: Rect::new(0, 0, 0, 0),
        z: 0,
        focused: false,
        hosted: false,
        started: false,
        launched: false,
        status: WindowStatus::Ready,
    };

    fn visible_rect(&self) -> Option<Rect> {
        (self.open && self.state != WindowState::Minimized).then_some(self.rect)
    }
}

/// Everything outside the windows that decided the last painted frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PaintedDesktop {
    /// A menu, dialog or drag covers the screen.
    overlay: bool,
    start_open: bool,
    start_all_programs: bool,
    start_highlighted: usize,
    start_scroll: usize,
    selection: usize,
    context: ContextMenuState,
    window_menu: ContextMenuState,
    toasts: usize,
    unread: usize,
    sound: volume::Sound,
    show_desktop: bool,
}

/// A complete desktop session.
///
/// `N` is the number of window slots, `A` the application catalog capacity and
/// `S` the number of supervised services.
pub struct Desktop<
    const N: usize = MAX_WINDOWS,
    const A: usize = MAX_APPS,
    const S: usize = MAX_SERVICES,
> {
    config: DesktopConfig,
    theme: DesktopTheme,
    windows: WindowManager<N>,
    start_menu: StartMenuState,
    catalog: AppCatalog<A>,
    interaction: InteractionState<48>,
    init: Init<S>,
    work_area: Rect,
    session_started: bool,
    /// Keyboard focus on the wallpaper shortcuts (catalog index).
    selection: usize,
    /// Double-click state for icons, title bars and desktop menu items.
    clicks: ClickMemory,
    /// The wallpaper's right-click menu.
    context: ContextMenuState,
    /// The Start menu's Run dialog.
    run_dialog: RunDialogState,
    /// The Start menu's "Turn off computer" dialog.
    shutdown: ShutdownDialogState,
    /// Whether the tray clock's Date and Time dialog is open.
    clock_open: bool,
    /// Whether the "Log Off" confirmation dialog is open.
    logoff_open: bool,
    /// Whether "Show Desktop" is active.
    show_desktop: bool,
    /// Window that captured the current pointer drag, if any.
    pointer_window: Option<usize>,
    /// A window's system menu.
    window_menu: ContextMenuState,
    /// The window that menu belongs to.
    window_menu_target: usize,
    /// Seconds until a new screen size goes back by itself, while it's being asked about.
    resolution_prompt: Option<u32>,
    resolution_answer: Option<bool>,
    /// Whether the Display Properties dialog is open.
    settings_open: bool,
    /// The user changed something that should outlive the session.
    settings_changed: bool,
    /// Whether the notification history is open.
    notifications_open: bool,
    /// The volume panel above the tray is showing.
    volume_open: bool,
    /// The volume and mute state shown in the tray.
    sound: volume::Sound,
    /// Toasts and their history.
    notifications: Notifications,
    /// A path being dragged between apps.
    drag: Option<Text<96>>,
    /// Processes whose windows closed, until they are gone.
    pending_kill: [Option<u32>; N],

    // Retained rendering ---------------------------------------------------
    /// The canvas keeps the last frame, and the caller draws the cursor.
    retained: bool,
    /// The next frame must repaint everything.
    repaint_all: bool,
    /// Canvas-sized cache of the wallpaper and shortcut icons.
    background: Option<&'static mut [u32]>,
    background_valid: bool,
    /// Canvas-sized surface for hosting a window partly off-screen.
    scratch: Option<&'static mut [u32]>,
    painted_windows: [PaintedWindow; N],
    painted_desktop: Option<PaintedDesktop>,
    painted_size: (u32, u32),
    painted_pointer: Point,
    painted_clock: Text<16>,
    /// The windows that last had the pointer and the keyboard.
    painted_routes: (Option<usize>, Option<usize>),
}

impl<const N: usize, const A: usize, const S: usize> Desktop<N, A, S> {
    /// Creates an empty desktop from `config`.
    pub const fn new(config: DesktopConfig) -> Self {
        let theme = DesktopTheme::from_config(&config);
        let start_menu = StartMenuState {
            open: config.start_menu.initially_open,
            highlighted: 0,
            all_programs: false,
            scroll: 0,
        };
        Self {
            config,
            theme,
            windows: WindowManager::new(),
            start_menu,
            catalog: AppCatalog::new(),
            interaction: InteractionState::new(),
            init: Init::new(config.init.policy, config.init.max_restarts),
            work_area: Rect::new(0, 0, 0, 0),
            session_started: false,
            selection: 0,
            clicks: ClickMemory {
                target: 0,
                at_ms: 0,
                x: 0,
                y: 0,
                valid: false,
            },
            context: ContextMenuState::CLOSED,
            run_dialog: RunDialogState::new(),
            shutdown: ShutdownDialogState { open: false },
            clock_open: false,
            logoff_open: false,
            show_desktop: false,
            pointer_window: None,
            window_menu: ContextMenuState::CLOSED,
            window_menu_target: 0,
            resolution_prompt: None,
            resolution_answer: None,
            settings_open: false,
            settings_changed: false,
            notifications_open: false,
            volume_open: false,
            sound: volume::Sound::FULL,
            notifications: Notifications::new(),
            drag: None,
            pending_kill: [None; N],
            retained: false,
            repaint_all: true,
            background: None,
            background_valid: false,
            scratch: None,
            painted_windows: [PaintedWindow::CLOSED; N],
            painted_desktop: None,
            painted_size: (0, 0),
            painted_pointer: Point::new(0, 0),
            painted_clock: Text::EMPTY,
            painted_routes: (None, None),
        }
    }

    /// Switches retained rendering on or off.
    ///
    /// In retained mode the caller promises that the canvas still holds the
    /// previous frame, and draws the pointer itself from
    /// [`DesktopResponse::cursor`]. Each frame then repaints only what changed
    /// and reports it in [`DesktopResponse::damage`]. Hosted apps paint only
    /// when they have input, are due an animation frame, or are uncovered.
    pub fn set_retained(&mut self, retained: bool) {
        self.retained = retained;
        self.repaint_all = true;
    }

    /// Gives the desktop canvas-sized buffers for its optional caches.
    ///
    /// `background` caches the wallpaper and shortcut icons. `scratch` hosts a
    /// window that is partly off-screen, so the app keeps its full size and is
    /// clipped when copied out. Either may be `None`: without `scratch`, such
    /// an app sees only its visible part.
    pub fn set_buffers(
        &mut self,
        background: Option<&'static mut [u32]>,
        scratch: Option<&'static mut [u32]>,
    ) {
        self.background = background;
        self.scratch = scratch;
        self.background_valid = false;
        self.repaint_all = true;
    }

    /// Takes the caches back, for whoever provided them to free.
    pub fn take_buffers(&mut self) -> (Option<&'static mut [u32]>, Option<&'static mut [u32]>) {
        self.background_valid = false;
        self.repaint_all = true;
        (self.background.take(), self.scratch.take())
    }

    /// Fits the session to a screen of a new size: its windows, and everything painted,
    /// all of which is redone on the next frame. The caches are the caller's to replace.
    pub fn screen_changed(&mut self, width: usize, height: usize) {
        let screen = Rect::new(0, 0, width as u32, height as u32);
        let layout = taskbar::layout(screen, &self.config.taskbar);
        self.work_area = layout.work_area(screen, self.config.taskbar.position);
        self.windows.fit_to(self.work_area);
        self.background_valid = false;
        self.repaint_all = true;
    }

    /// Shows the prompt asking whether to keep a new screen size, or takes it down.
    pub fn set_resolution_prompt(&mut self, seconds_left: Option<u32>) {
        if self.resolution_prompt != seconds_left {
            self.repaint_all = true;
        }
        self.resolution_prompt = seconds_left;
    }

    /// What the prompt was answered with: `true` to keep the size, `false` to go back.
    pub fn take_resolution_answer(&mut self) -> Option<bool> {
        self.resolution_answer.take()
    }

    /// Returns `true` once [`set_buffers`](Self::set_buffers) has provided the
    /// caches.
    pub const fn has_buffers(&self) -> bool {
        self.background.is_some() || self.scratch.is_some()
    }

    /// Makes the next frame repaint the whole canvas, for when something else
    /// drew on it.
    pub fn invalidate(&mut self) {
        self.repaint_all = true;
    }

    /// Shows a notification.
    pub fn notify(&mut self, notification: Notification, now_ms: u64) {
        self.notifications.push(notification, now_ms);
        self.repaint_all = true;
    }

    /// Shows a notification from a title and a line of detail.
    pub fn notify_text(&mut self, title: &str, body: &str, level: Level, now_ms: u64) {
        self.notify(
            Notification {
                title: Text::from_str(title),
                body: Text::from_str(body),
                level,
            },
            now_ms,
        );
    }

    /// The notification state.
    pub const fn notifications(&self) -> &Notifications {
        &self.notifications
    }

    /// Hides toasts whose time is up. Returns `true` when a frame is needed.
    pub fn expire_toasts(&mut self, now_ms: u64) -> bool {
        let changed = self.notifications.expire(now_ms);
        if changed {
            self.repaint_all = true;
        }
        changed
    }

    /// Starts dragging `payload`, normally a file path. The drag ends on the
    /// next left-button release, or on Escape.
    pub fn begin_drag(&mut self, payload: &str) {
        self.drag = Some(Text::from_str(payload));
    }

    /// The active configuration.
    pub const fn config(&self) -> &DesktopConfig {
        &self.config
    }

    /// Replaces the configuration and rebuilds the theme.
    ///
    /// Open windows are kept; the init restart policy is updated in place.
    /// True once per change to something that should outlive the session, for
    /// the caller to save. See [`DesktopConfig::settings_text`].
    pub fn take_settings_changed(&mut self) -> bool {
        core::mem::take(&mut self.settings_changed)
    }

    pub fn set_config(&mut self, config: DesktopConfig) {
        if self.config != config {
            self.background_valid = false;
            self.repaint_all = true;
        }
        self.config = config;
        self.theme = DesktopTheme::from_config(&config);
        self.init.set_policy(config.init.policy);
    }

    /// Applies one setting in the form they're saved in, as the Settings app asks.
    /// One that makes no sense changes nothing.
    pub fn apply_setting(&mut self, key: &str, value: &str) {
        let mut line = Text::<64>::new();
        let _ = writeln!(line, "{key}: {value}");
        let mut next = self.config;
        next.apply_settings(line.as_str());
        if next != self.config {
            self.set_config(next);
            self.settings_changed = true;
        }
    }

    /// Switches to a colour scheme, including its wallpaper.
    pub fn set_theme(&mut self, name: ThemeName) {
        let mut config = self.config;
        config.theme = name;
        config.wallpaper = config.wallpaper_preset.wallpaper(name);
        self.set_config(config);
    }

    /// The resolved visual theme.
    pub const fn theme(&self) -> &DesktopTheme {
        &self.theme
    }

    /// The application catalog.
    pub const fn catalog(&self) -> &AppCatalog<A> {
        &self.catalog
    }

    /// The window manager.
    pub const fn windows(&self) -> &WindowManager<N> {
        &self.windows
    }

    /// Mutable access to the window manager.
    pub fn windows_mut(&mut self) -> &mut WindowManager<N> {
        &mut self.windows
    }

    /// Persistent Start-menu state.
    pub const fn start_menu(&self) -> &StartMenuState {
        &self.start_menu
    }

    /// Mutable Start-menu state, for keyboard shortcuts.
    pub fn start_menu_mut(&mut self) -> &mut StartMenuState {
        &mut self.start_menu
    }

    /// Catalog index currently selected on the wallpaper.
    ///
    /// Keyboard navigation moves this selection; `Enter` launches it when the
    /// Start menu is closed.
    pub const fn selection(&self) -> usize {
        self.selection
    }

    /// Selects a wallpaper shortcut by catalog index.
    ///
    /// The index is clamped to the installed applications.
    pub fn select(&mut self, index: usize) {
        self.selection = index.min(self.catalog.len().saturating_sub(1));
    }

    /// Number of shortcut rows the current work area can hold.
    fn shortcut_rows(&self, config: &DesktopConfig) -> usize {
        if self.work_area.is_empty() {
            return 1;
        }
        let icon_size = config.icons.icon_size.clamp(24, 96) as i32;
        let label_height = if config.icons.show_labels { 34 } else { 8 };
        let cell_height = (icon_size + label_height).max(1);
        (self.work_area.height as i32 / cell_height).max(1) as usize
    }

    /// Moves the wallpaper selection through the column-major shortcut grid.
    fn move_selection(&mut self, columns: i32, rows_delta: i32, rows: usize) {
        let count = self.catalog.len();
        if count == 0 {
            self.selection = 0;
            return;
        }
        let rows = rows.max(1) as i32;
        let current = self.selection.min(count - 1) as i32;
        let next = current + rows_delta + columns * rows;
        self.selection = next.clamp(0, count as i32 - 1) as usize;
    }

    /// Minimises every window, or restores the ones this hid when "Show
    /// Desktop" is clicked a second time. Windows the user minimised beforehand
    /// stay minimised.
    fn toggle_show_desktop(&mut self) {
        if self.show_desktop {
            let mut order: [usize; N] = core::array::from_fn(|index| index);
            order.sort_unstable_by_key(|index| {
                self.windows.get(*index).map_or(0, |window| window.z)
            });
            for index in order {
                let hidden = self.windows.get_mut(index).is_some_and(|window| {
                    let hidden = window.open
                        && window.hidden_by_show_desktop
                        && window.state == WindowState::Minimized;
                    window.hidden_by_show_desktop = false;
                    hidden
                });
                if hidden {
                    self.windows.restore(index);
                }
            }
            self.show_desktop = false;
        } else {
            for index in 0..N {
                if self.windows.minimize(index) {
                    if let Some(window) = self.windows.get_mut(index) {
                        window.hidden_by_show_desktop = true;
                    }
                }
            }
            self.show_desktop = true;
        }
        self.start_menu.close();
    }

    /// Whether a dialog or pop-up menu has the keyboard and pointer to itself.
    fn modal_open(&self) -> bool {
        self.run_dialog.open
            || self.resolution_prompt.is_some()
            || self.shutdown.open
            || self.clock_open
            || self.logoff_open
            || self.context.open
            || self.window_menu.open
            || self.settings_open
            || self.notifications_open
            || self.volume_open
    }

    /// Closes a window, its app and any process started for it.
    ///
    /// Killing can need more than one attempt, so [`tick_session`] keeps trying
    /// until the process has gone.
    ///
    /// [`tick_session`]: Self::tick_session
    fn close_failed_window(&mut self, index: usize, runtime: &'static dyn App, status: i32, now_ms: u64) -> bool {
        self.notify_failed(runtime.name(), status, now_ms);
        self.close_window(index)
    }

    fn notify_failed(&mut self, name: &str, status: i32, now_ms: u64) {
        let mut body = Text::<96>::new();
        let _ = write!(body, "{name} failed (status {status}) and its window was closed.");
        self.notify_text("Application failed", body.as_str(), Level::Error, now_ms);
    }

    pub fn close_window(&mut self, index: usize) -> bool {
        let Some(window) = self.windows.get(index).copied() else {
            return false;
        };
        if !window.open {
            return false;
        }
        if let Some(runtime) = window.runtime {
            if window.app_started {
                close_hosted(runtime, index);
            } else {
                isolate::finish(index);
            }
        }
        if let Some(pid) = window.pid {
            if let Some(slot) = self.pending_kill.iter_mut().find(|slot| slot.is_none()) {
                *slot = Some(pid);
            }
        }
        if self.pointer_window == Some(index) {
            self.pointer_window = None;
        }
        self.windows.close(index)
    }

    /// Closes every open window, for log off, and stops the processes behind
    /// them. Returns how many windows were open.
    ///
    /// A kill can fail while its target is running on another CPU, so each
    /// one is retried for a short while; whatever is still left is picked up
    /// by the next [`tick_session`](Self::tick_session).
    pub fn close_all_windows(&mut self, processes: &dyn Processes) -> usize {
        let mut closed = 0;
        for index in 0..N {
            if self.close_window(index) {
                closed += 1;
            }
        }
        for _ in 0..LOG_OFF_KILL_TRIES {
            let mut live = [ProcessInfo::EMPTY; MAX_PROCESSES];
            let count = processes.snapshot(&mut live);
            let mut waiting = false;
            for slot in &mut self.pending_kill {
                let Some(pid) = *slot else {
                    continue;
                };
                let alive = live[..count]
                    .iter()
                    .any(|process| process.pid == pid && process.state != ProcessState::Exited);
                if !alive || processes.kill(pid) {
                    *slot = None;
                } else {
                    waiting = true;
                }
            }
            if !waiting {
                break;
            }
            processes.sleep_ms(10);
        }
        self.publish_windows();
        closed
    }

    /// Handles the shortcuts that work no matter what has the keyboard.
    fn handle_shortcuts(&mut self, input: &Input) {
        for (key, modifiers) in input.keys() {
            if !is_shortcut(key, modifiers) {
                continue;
            }
            match key {
                Key::Escape => {
                    self.close_overlays();
                    self.start_menu.show();
                }
                Key::Tab => {
                    self.close_overlays();
                    self.start_menu.close();
                    self.show_desktop = false;
                    if modifiers.contains(Modifiers::SHIFT) {
                        self.windows.cycle_backward();
                    } else {
                        self.windows.cycle_forward();
                    }
                }
                Key::F4 => {
                    if self.modal_open() {
                        self.close_overlays();
                    } else if let Some(index) = self.windows.focused() {
                        self.close_window(index);
                    } else {
                        self.start_menu.close();
                        self.shutdown.open = true;
                    }
                }
                Key::Space => {
                    self.close_overlays();
                    if let Some(index) = self.windows.focused() {
                        if let Some(window) = self.windows.get(index) {
                            let point = Point::new(window.rect.x + 4, window.rect.y + 30);
                            self.open_window_menu(index, point);
                            self.window_menu.highlighted = 0;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn open_window_menu(&mut self, index: usize, point: Point) {
        self.window_menu_target = index;
        self.window_menu.show(point);
        self.start_menu.close();
    }

    /// Closes every dialog and pop-up menu, as a shortcut that changes context
    /// does. The Run dialog keeps its text cleared.
    fn close_overlays(&mut self) {
        self.run_dialog.close();
        self.shutdown.open = false;
        self.clock_open = false;
        self.logoff_open = false;
        self.context.open = false;
        self.window_menu.open = false;
        self.settings_open = false;
        self.notifications_open = false;
        self.volume_open = false;
    }

    fn apply_window_action(&mut self, index: usize, action: WindowAction) {
        match action {
            WindowAction::None => {}
            WindowAction::Restore => {
                let state = self.windows.get(index).map(|window| window.state);
                match state {
                    Some(WindowState::Minimized) => {
                        self.windows.restore(index);
                    }
                    Some(WindowState::Maximized) => {
                        self.windows.toggle_maximize(index, self.work_area);
                    }
                    _ => {}
                }
            }
            WindowAction::Minimize => {
                self.windows.minimize(index);
            }
            WindowAction::Maximize => {
                let normal = self
                    .windows
                    .get(index)
                    .is_some_and(|window| window.state != WindowState::Maximized);
                if normal {
                    self.windows.toggle_maximize(index, self.work_area);
                }
            }
            WindowAction::Close => {
                self.close_window(index);
            }
        }
    }

    /// Feeds typed text and editing keys into the Run dialog.
    fn handle_run_keys(&mut self, input: &Input, submit: &mut bool) {
        for ch in input.text() {
            if !ch.is_control() {
                self.run_dialog.text.push(ch);
            }
        }
        if input.keys().any(|(key, modifiers)| {
            modifiers.contains(Modifiers::CTRL) && matches!(key, Key::Character('v' | 'V'))
        }) {
            let mut buffer = [0u8; 96];
            let pasted = feos_app::shell::clipboard_get(&mut buffer);
            for ch in pasted.chars().filter(|ch| !ch.is_control()) {
                self.run_dialog.text.push(ch);
            }
        }
        if input.keys().any(|(key, _)| key == Key::Backspace) {
            self.run_dialog.text.pop();
        }
        if input.keys().any(|(key, _)| key == Key::Escape) {
            self.run_dialog.close();
        }
        if input.keys().any(|(key, _)| key == Key::Enter) {
            *submit = true;
        }
    }

    /// Applies keyboard navigation before drawing a frame.
    ///
    /// With the Start menu open the arrow keys move its highlight and `Tab`
    /// toggles the "All Programs" grid. With it closed the arrows move the
    /// wallpaper selection and `Space` opens the menu. `Enter` activation is
    /// handled by [`render`](Self::render) once the frame is laid out.
    fn handle_keyboard(&mut self, input: &Input, config: &DesktopConfig) {
        let up = input.keys().any(|(key, _)| key == Key::Up);
        let down = input.keys().any(|(key, _)| key == Key::Down);
        let left = input.keys().any(|(key, _)| key == Key::Left);
        let right = input.keys().any(|(key, _)| key == Key::Right);
        let tab = input.keys().any(|(key, _)| key == Key::Tab);
        let space = input.keys().any(|(key, _)| key == Key::Space);

        if self.start_menu.open {
            let count = self.catalog.len();
            if self.start_menu.all_programs {
                if left {
                    self.start_menu
                        .move_grid(-1, 0, count, start_menu::GRID_COLUMNS);
                }
                if right {
                    self.start_menu
                        .move_grid(1, 0, count, start_menu::GRID_COLUMNS);
                }
                if up {
                    self.start_menu
                        .move_grid(0, -1, count, start_menu::GRID_COLUMNS);
                }
                if down {
                    self.start_menu
                        .move_grid(0, 1, count, start_menu::GRID_COLUMNS);
                }
            } else {
                if up {
                    self.start_menu.move_highlight(-1, count, false);
                }
                if down {
                    self.start_menu.move_highlight(1, count, false);
                }
            }
            if tab || space {
                self.start_menu.all_programs = !self.start_menu.all_programs;
                self.start_menu.highlighted = 0;
            }
            return;
        }

        if tab {
            self.windows.focus_next();
            return;
        }
        if space {
            self.start_menu.show();
            return;
        }

        let rows = self.shortcut_rows(config);
        if up {
            self.move_selection(0, -1, rows);
        }
        if down {
            self.move_selection(0, 1, rows);
        }
        if left {
            self.move_selection(-1, 0, rows);
        }
        if right {
            self.move_selection(1, 0, rows);
        }
    }

    /// The service supervisor.
    pub const fn init(&self) -> &Init<S> {
        &self.init
    }

    /// Mutable access to the service supervisor.
    pub fn init_mut(&mut self) -> &mut Init<S> {
        &mut self.init
    }

    /// The last work area computed from the taskbar.
    pub const fn work_area(&self) -> Rect {
        self.work_area
    }

    /// Returns `true` once autostart services have been registered.
    pub const fn session_started(&self) -> bool {
        self.session_started
    }

    /// Replaces the catalog contents by scanning an application folder.
    ///
    /// Call this at session start, and again whenever the backing storage is
    /// known to have changed. Files are detected by content, not by suffix.
    pub fn scan(&mut self, entries: &[DirEntry<'_>]) -> ScanReport {
        let report = self.catalog.scan(entries);
        self.selection = self.selection.min(self.catalog.len().saturating_sub(1));
        let catalog = &self.catalog;
        self.windows.remap_apps(|id| catalog.find_id(id));
        self.background_valid = false;
        self.repaint_all = true;
        report
    }

    /// Looks up an installed application by manifest id.
    pub fn app(&self, id: &str) -> Option<&InstalledApp> {
        self.catalog
            .find_id(id)
            .and_then(|index| self.catalog.get(index))
    }

    /// Opens (or focuses) the window for catalog entry `index`.
    ///
    /// The window size comes from the manifest, falling back to the global
    /// defaults, and is clamped to the current work area.
    pub fn open_app(&mut self, index: usize) -> Option<usize> {
        self.open_app_with(index, None)
    }

    /// Opens (or focuses) the window for catalog entry `index`, optionally with
    /// arguments replacing the manifest's.
    ///
    /// Windowed apps keep one session per window, so asking an open window for
    /// different arguments restarts its app with them.
    pub fn open_app_with(&mut self, index: usize, args: Option<&str>) -> Option<usize> {
        let app = *self.catalog.get(index)?;
        self.show_desktop = false;
        let work_area = if self.work_area.is_empty() {
            Rect::new(0, 0, 1024, 768)
        } else {
            self.work_area
        };
        let width = if app.manifest.window_width == 0 {
            self.config.windows.default_width
        } else {
            app.manifest.window_width
        };
        let height = if app.manifest.window_height == 0 {
            self.config.windows.default_height
        } else {
            app.manifest.window_height
        };
        let was_open = self
            .windows
            .get_open_for(index)
            .is_some();
        let slot = self.windows.open(
            index,
            app.name(),
            work_area,
            width,
            height,
            self.config.windows.cascade,
        )?;
        if let Some(window) = self.windows.get_mut(slot) {
            if !was_open {
                window.bundle.set(app.id());
                window.args.set(args.unwrap_or(app.manifest.args.as_str()));
            } else if let Some(args) = args {
                if !window.args.eq_str(args) {
                    window.args.set(args);
                    if window.app_started {
                        if let Some(runtime) = window.runtime {
                            close_hosted(runtime, slot);
                        }
                        window.app_started = false;
                    }
                }
            }
        }
        Some(slot)
    }

    /// Tells apps which windows are open, for a task manager.
    pub fn publish_windows(&self) {
        let mut list = [feos_app::shell::WindowInfo::empty(); feos_app::shell::MAX_WINDOWS];
        let mut count = 0;
        for index in 0..N.min(list.len()) {
            let Some(window) = self.windows.get(index).filter(|window| window.open) else {
                continue;
            };
            let mut title = Text::new();
            title.set(window.title.as_str());
            list[count] = feos_app::shell::WindowInfo {
                window: index as u16,
                pid: window.pid,
                hosted: window.runtime.is_some(),
                title,
                cpu_us: window.cpu_us,
                minimized: window.state == WindowState::Minimized,
                focused: self.windows.is_focused(index),
            };
            count += 1;
        }
        feos_app::shell::publish_windows(&list[..count]);
    }

    /// Brings a window forward, restoring it if it is minimized.
    pub fn activate_window(&mut self, index: usize) -> bool {
        let Some(window) = self.windows.get_mut(index).filter(|window| window.open) else {
            return false;
        };
        if window.state == WindowState::Minimized {
            window.state = window.restore_state;
        }
        window.suspended = false;
        self.show_desktop = false;
        let focused = self.windows.focus(index);
        self.repaint_all = true;
        focused
    }

    /// The shortest frame period any visible window currently asks for, or zero.
    pub fn animation_period(&self) -> u32 {
        let mut shortest = 0;
        for index in 0..N {
            let Some(window) = self.windows.get(index) else {
                continue;
            };
            if !window.open || window.state == WindowState::Minimized {
                continue;
            }
            if let Some(app) = window.runtime.filter(|app| app.animates()) {
                let period = app.animation_period_ms().max(1);
                if shortest == 0 || period < shortest {
                    shortest = period;
                }
            }
        }
        shortest
    }

    /// Opens another window for catalog entry `index`, even if it has one.
    fn open_app_instance(&mut self, index: usize, args: Option<&str>) -> Option<usize> {
        let app = *self.catalog.get(index)?;
        self.show_desktop = false;
        // Clicking an icon opens a window before the launch reaches here. That
        // one is this launch's, not a reason for another.
        let fresh = (0..N).find(|&slot| {
            self.windows
                .get(slot)
                .is_some_and(|window| window.open && window.app == index && window.runtime.is_none())
        });
        if let Some(slot) = fresh {
            if let (Some(args), Some(window)) = (args, self.windows.get_mut(slot)) {
                window.args.set(args);
            }
            return Some(slot);
        }
        let work_area = if self.work_area.is_empty() {
            Rect::new(0, 0, 1024, 768)
        } else {
            self.work_area
        };
        let width = if app.manifest.window_width == 0 {
            self.config.windows.default_width
        } else {
            app.manifest.window_width
        };
        let height = if app.manifest.window_height == 0 {
            self.config.windows.default_height
        } else {
            app.manifest.window_height
        };
        let slot = self.windows.open_new(
            index,
            app.name(),
            work_area,
            width,
            height,
            self.config.windows.cascade,
        )?;
        if let Some(window) = self.windows.get_mut(slot) {
            window.bundle.set(app.id());
            window.args.set(args.unwrap_or(app.manifest.args.as_str()));
        }
        Some(slot)
    }

    /// Launches catalog entry `index` through the shell registry.
    ///
    /// The window is opened first, so even an entry that is missing from the
    /// registry remains visible and reports the problem.
    pub fn launch(
        &mut self,
        index: usize,
        registry: &dyn AppRegistry,
        processes: &dyn Processes,
    ) -> LaunchOutcome {
        self.launch_with(index, None, registry, processes)
    }

    /// [`launch`](Self::launch), with arguments replacing the manifest's.
    pub fn launch_with(
        &mut self,
        index: usize,
        args: Option<&str>,
        registry: &dyn AppRegistry,
        processes: &dyn Processes,
    ) -> LaunchOutcome {
        let Some(app) = self.catalog.get(index).copied() else {
            return LaunchOutcome::UnknownApp;
        };
        let multi = registry
            .find(app.manifest.entry.as_str())
            .is_some_and(|runtime| runtime.multi_instance());
        let opened = if multi {
            self.open_app_instance(index, args)
        } else {
            self.open_app_with(index, args)
        };
        let Some(window_index) = opened else {
            return LaunchOutcome::NoWindowSlot;
        };

        if let Some(window) = self.windows.get(window_index) {
            if window.launched {
                return LaunchOutcome::AlreadyRunning {
                    window: window_index,
                    pid: window.pid,
                };
            }
        }

        let Some(runtime) = registry.find(app.manifest.entry.as_str()) else {
            if let Some(window) = self.windows.get_mut(window_index) {
                window.status = WindowStatus::UnknownEntry;
            }
            return LaunchOutcome::UnknownEntry;
        };
        if let Some(window) = self.windows.get_mut(window_index) {
            window.status = WindowStatus::Ready;
        }
        if runtime.windowed() {
            if let Some(window) = self.windows.get_mut(window_index) {
                if window.runtime.is_none() {
                    window.runtime = Some(runtime);
                    window.app_started = false;
                }
            }
            return LaunchOutcome::Windowed {
                window: window_index,
            };
        }
        if runtime.foreground() {
            return LaunchOutcome::Foreground {
                window: window_index,
            };
        }
        let spawn_args = self
            .windows
            .get(window_index)
            .map(|window| window.args)
            .unwrap_or(Text::EMPTY);
        match processes.spawn(runtime, spawn_args.as_str()) {
            Ok(pid) => {
                if let Some(window) = self.windows.get_mut(window_index) {
                    window.pid = Some(pid);
                    window.launched = true;
                }
                LaunchOutcome::Spawned {
                    window: window_index,
                    pid,
                }
            }
            Err(error) => {
                if let Some(window) = self.windows.get_mut(window_index) {
                    window.status = WindowStatus::SpawnFailed;
                }
                LaunchOutcome::SpawnFailed(error)
            }
        }
    }

    /// Decides what a command typed into the Run dialog should do.
    ///
    /// A bundle id wins, then a file path with an associated app, then a shell
    /// command. A command with a bundle is opened through the bundle, so it
    /// gets a window: `taskmgr` and `process-monitor` do the same thing. Only a
    /// command no bundle offers can reach the foreground or background paths.
    pub fn resolve_command<'c>(
        &self,
        command: &'c str,
        registry: &dyn AppRegistry,
    ) -> CommandTarget<'c> {
        let command = command.trim();
        if command.is_empty() {
            return CommandTarget::NotFound;
        }
        if let Some(index) = self.catalog.find_id(command) {
            return CommandTarget::Bundle { index, args: None };
        }

        let (name, args) = match command.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (command, ""),
        };
        let typed_args = (!args.is_empty()).then_some(args);
        if let Some(index) = self.catalog.find_id(name) {
            return CommandTarget::Bundle {
                index,
                args: typed_args,
            };
        }

        if command.starts_with('/') {
            if let Some(index) =
                feos_app::shell::association_for(command).and_then(|id| self.catalog.find_id(id))
            {
                return CommandTarget::Bundle {
                    index,
                    args: Some(command),
                };
            }
        }

        let Some(runtime) = registry.find(name) else {
            return CommandTarget::NotFound;
        };
        if let Some(index) = self
            .catalog
            .iter()
            .position(|app| runtime.matches(app.manifest.entry.as_str()))
        {
            return CommandTarget::Bundle {
                index,
                args: typed_args,
            };
        }
        if runtime.foreground() {
            CommandTarget::Foreground { app: runtime, args }
        } else {
            CommandTarget::Spawn { app: runtime, args }
        }
    }

    /// Registers the configured autostart services and starts them.
    ///
    /// Ids are resolved through the catalog first (so `init.autostart` can name
    /// a bundle id) and then through the shell registry. Unknown ids appear in
    /// the status table as `missing` instead of being silently dropped.
    ///
    /// Returns the number of services that started.
    pub fn start_session(
        &mut self,
        registry: &dyn AppRegistry,
        processes: &dyn Processes,
    ) -> usize {
        let config = self.config.init;
        self.init = Init::new(config.policy, config.max_restarts);

        // A windowed app has no process to supervise: it lives in a window the
        // desktop drives. Open those, and give the rest to the supervisor.
        let mut opened = 0;
        for id in config.autostart() {
            let id = id.as_str();
            if let Some(index) = self.windowed_bundle(id, registry) {
                if matches!(
                    self.launch(index, registry, processes),
                    LaunchOutcome::Windowed { .. }
                ) {
                    opened += 1;
                }
                continue;
            }
            self.register_service(id, registry);
        }

        self.session_started = true;
        self.init.start_all(processes) + opened
    }

    /// The catalog index of `id`, when it names a bundle whose app is windowed.
    pub fn windowed_bundle(&self, id: &str, registry: &dyn AppRegistry) -> Option<usize> {
        let index = self.catalog.find_id(id)?;
        let manifest = &self.catalog.get(index)?.manifest;
        registry
            .find(manifest.entry.as_str())
            .filter(|app| app.windowed())
            .map(|_| index)
    }

    /// Registers one service without starting it.
    ///
    /// The id is resolved through the catalog first, then through the shell
    /// registry. Unknown ids are still registered so they appear as `missing`
    /// in the status table. Returns `false` only when the service table is
    /// full.
    pub fn register_service(&mut self, id: &str, registry: &dyn AppRegistry) -> bool {
        if self.init.find(id).is_some() {
            return true;
        }
        match self.resolve_service(id, registry) {
            Some((app, args)) => self.init.register(id, app, args.as_str()),
            None => self.init.register_missing(id),
        }
    }

    fn resolve_service(
        &self,
        id: &str,
        registry: &dyn AppRegistry,
    ) -> Option<(&'static dyn App, Text<64>)> {
        if let Some(index) = self.catalog.find_id(id) {
            let manifest = &self.catalog.get(index)?.manifest;
            return registry
                .find(manifest.entry.as_str())
                .map(|app| (app, manifest.args));
        }
        registry.find(id).map(|app| (app, Text::EMPTY))
    }

    /// Applies the restart policy to supervised services.
    ///
    /// The same pass notices when a background process launched from a window
    /// has exited, so the window can report `Ready` and be launched again.
    pub fn tick_session(&mut self, processes: &dyn Processes) -> usize {
        let changed = self.init.tick(processes);
        self.finish_session_tick(processes, changed)
    }

    /// [`Self::tick_session`] with the real clock, so restart backoff follows
    /// wall time however often the session ticks.
    pub fn tick_session_at(&mut self, processes: &dyn Processes, now_ms: u64) -> usize {
        let changed = self.init.tick_at(processes, now_ms);
        self.finish_session_tick(processes, changed)
    }

    fn finish_session_tick(&mut self, processes: &dyn Processes, mut changed: usize) -> usize {

        let mut live = [ProcessInfo::EMPTY; MAX_PROCESSES];
        let count = processes.snapshot(&mut live);

        // A closed window's process goes away with it. Keep asking until it
        // has: a kill can fail while the target is running on another CPU.
        for slot in &mut self.pending_kill {
            let Some(pid) = *slot else {
                continue;
            };
            let alive = live[..count]
                .iter()
                .any(|process| process.pid == pid && process.state != ProcessState::Exited);
            if !alive || processes.kill(pid) {
                *slot = None;
            }
        }

        for index in 0..N {
            let Some(window) = self.windows.get_mut(index) else {
                continue;
            };
            let Some(pid) = window.pid else {
                continue;
            };
            let running = live[..count]
                .iter()
                .any(|process| process.pid == pid && process.state != ProcessState::Exited);
            if !running {
                window.pid = None;
                window.launched = false;
                changed += 1;
            }
        }

        changed
    }

    /// Gives minimized hosted apps their background tick.
    pub fn tick_background(&mut self, platform: &Platform, now_ms: u64) {
        for index in 0..N {
            let Some(window) = self.windows.get(index) else {
                continue;
            };
            if window.open && window.app_started && window.state == WindowState::Minimized {
                if let Some(runtime) = window.runtime {
                    let ran = isolate::call(platform, index, runtime.name(), || {
                        runtime.background_tick(platform, now_ms);
                    });
                    if let Err(died) = ran {
                        // Nothing's on screen, but the same goes for it.
                        let _ = self.close_failed_window(index, runtime, died.status, now_ms);
                    }
                }
            }
        }
    }

    /// Copies the service status table into `out`.
    pub fn services(&self, out: &mut [ServiceStatus]) -> usize {
        self.init.snapshot(out)
    }

    /// Starts one supervised service.
    pub fn start_service(&mut self, id: &str, processes: &dyn Processes) -> Result<u32, InitError> {
        self.init.start(id, processes)
    }

    /// Stops one supervised service.
    pub fn stop_service(&mut self, id: &str, processes: &dyn Processes) -> bool {
        self.init.stop(id, processes)
    }

    /// Restarts one supervised service.
    pub fn restart_service(
        &mut self,
        id: &str,
        processes: &dyn Processes,
    ) -> Result<u32, InitError> {
        self.init.restart(id, processes)
    }

    /// Renders one complete frame and returns the requested actions.
    ///
    /// The caller owns input dispatch and process execution; this method only
    /// draws, updates window placement and reports what the user chose.
    pub fn render(
        &mut self,
        canvas: &mut Canvas<'_>,
        input: &Input,
        uptime_ms: u64,
    ) -> DesktopResponse {
        self.render_with_platform(canvas, input, uptime_ms, None)
    }

    /// Renders one frame with platform services available to windowed apps.
    pub fn render_with_platform(
        &mut self,
        canvas: &mut Canvas<'_>,
        input: &Input,
        uptime_ms: u64,
        platform: Option<&Platform>,
    ) -> DesktopResponse {
        let theme = self.theme;
        let config = self.config;
        // The real time, when the machine has a clock that knows it. A board
        // without one keeps showing time since boot.
        let wall_ms = platform
            .and_then(|platform| platform.clock.unix_time())
            .map_or(uptime_ms, |seconds| seconds.saturating_mul(1000));
        let clock = paint::format_clock(wall_ms, &config.taskbar);
        let mut response = DesktopResponse::default();

        let canvas_rect = Rect::new(0, 0, canvas.width() as u32, canvas.height() as u32);
        let layout = taskbar::layout(canvas_rect, &config.taskbar);
        self.work_area = layout.work_area(canvas_rect, config.taskbar.position);

        // Shortcuts act first and are then hidden from everything below, so
        // Alt+Tab does not also reach the app or count as a plain Tab.
        self.handle_shortcuts(input);
        let filtered = input.filtered(|event| match event {
            Event::KeyDown(key, modifiers) => !is_shortcut(*key, *modifiers),
            _ => true,
        });
        let input = &filtered;

        let run_dialog_open = self.run_dialog.open;
        self.sound.sync();
        let modal_at_start = self.modal_open();
        let window_owns_keyboard = self
            .windows
            .focused_window()
            .is_some_and(|window| window.runtime.is_some() && window.state != WindowState::Minimized);
        let enter_pressed = input.keys().any(|(key, _)| key == Key::Enter);
        let escape_pressed = input.keys().any(|(key, _)| key == Key::Escape);

        let mut run_submit = false;
        let mut context_choice = None;
        let mut window_choice = None;
        if run_dialog_open {
            self.handle_run_keys(input, &mut run_submit);
        } else if self.context.open {
            context_choice = handle_menu_keys(&mut self.context, &CONTEXT_ITEMS, input);
        } else if self.window_menu.open {
            window_choice = handle_menu_keys(&mut self.window_menu, &WINDOW_ITEMS, input);
        } else if self.modal_open() {
            if escape_pressed {
                self.shutdown.open = false;
                self.clock_open = false;
                self.logoff_open = false;
                self.settings_open = false;
                self.notifications_open = false;
                self.volume_open = false;
            }
        } else if self.drag.is_some() {
            if escape_pressed {
                self.drag = None;
            }
        } else if self.start_menu.open || !window_owns_keyboard {
            // With a hosted app focused, Tab, Space and the arrow keys belong
            // to the app. An open menu always owns the keyboard.
            self.handle_keyboard(input, &config);
        }

        // Pointer routing -----------------------------------------------------
        let pointer = input.pointer();
        let pressed_now = input.events().any(|event| {
            matches!(event, Event::PointerDown { .. })
        });
        let released_now = !input.pointer_down() && !input.secondary_down();
        let modal = self.modal_open();
        let start_open = self.start_menu.open;
        let drag_active = self.drag.is_some();
        let dropped = drag_active && !input.pointer_down();
        let desktop_overlay = start_open || modal || modal_at_start || (drag_active && !dropped);

        let toast_area = self.notifications.area(&layout, self.work_area);
        let in_toast = toast_area.contains(pointer);
        let in_bar = layout.bar.contains(pointer);
        let pointer_in_work_area = self.work_area.contains(pointer) && !in_toast;
        if pressed_now && !desktop_overlay && pointer_in_work_area {
            self.pointer_window = self.windows.topmost_content_at(pointer);
        }
        let pointer_window = if desktop_overlay {
            None
        } else if self.pointer_window.is_some() {
            // Keep delivering pointer events to the window that owns the drag,
            // even when the pointer crosses the taskbar or another window.
            self.pointer_window
        } else if pointer_in_work_area {
            self.windows.topmost_content_at(pointer)
        } else {
            None
        };
        let keyboard_window = if desktop_overlay {
            None
        } else {
            self.windows.focused().filter(|index| {
                self.windows
                    .get(*index)
                    .is_some_and(|window| window.runtime.is_some())
            })
        };

        // What this frame must repaint ----------------------------------------
        let painted_windows = self.window_snapshot();
        let painted_desktop = self.desktop_snapshot();
        let damage = self.compute_damage(
            input,
            uptime_ms,
            canvas_rect,
            &layout,
            &clock,
            pointer_window,
            keyboard_window,
            &painted_windows,
            &painted_desktop,
        );
        let size = (canvas_rect.width, canvas_rect.height);
        if self.painted_size != size {
            self.background_valid = false;
        }
        self.painted_windows = painted_windows;
        self.painted_desktop = Some(painted_desktop);
        self.painted_size = size;
        self.painted_pointer = pointer;
        self.painted_clock = clock;
        self.painted_routes = (pointer_window, keyboard_window);
        self.repaint_all = false;

        // The desktop consumes Tab/Space/Enter itself; keep the toolkit from
        // activating a focused widget for the same key press.
        let pointer_input = input.without_keyboard();
        let mut ui = Ui::new(canvas, &theme.ui, &pointer_input, &mut self.interaction);
        ui.painter.set_paint_mask(damage);

        // Which surface the pointer really touches. Each surface below is told
        // whether something above it covers the pointer, so hit testing follows
        // the stacking order instead of the order things are drawn in.
        let panel = if start_open {
            start_menu::panel_rect(
                canvas_rect,
                &config.start_menu,
                &layout,
                config.taskbar.position,
            )
        } else {
            Rect::new(0, 0, 0, 0)
        };
        let in_panel = panel.contains(pointer);
        let above_bar = modal || drag_active || in_panel || in_toast;
        let above_windows = above_bar || in_bar || start_open;
        let over_window = pointer_in_work_area && self.windows.topmost_at(pointer).is_some();

        // Background ----------------------------------------------------------
        let cached = paint_background(
            &mut ui,
            &theme,
            &config,
            &self.catalog,
            self.work_area,
            self.background.as_deref_mut(),
            &mut self.background_valid,
        );

        let mut launch_index = None;
        let mut launch_args: Option<Text<96>> = None;
        let mut shortcut_clicked = None;
        if config.icons.show_icons {
            ui.set_pointer_occluded(above_windows || over_window);
            shortcut_clicked = render_shortcuts(
                &mut ui,
                &theme,
                &config,
                &self.catalog,
                self.work_area,
                self.selection,
                cached,
            );
        }

        if let Some(index) = shortcut_clicked {
            // A single click selects the icon; the second click within the
            // double-click interval opens it.
            self.selection = index;
            if self
                .clicks
                .hit(0x1000_0000 | index as u32, ui.pointer(), uptime_ms)
            {
                launch_index = Some(index);
            }
        }

        // Windows, bottom to top ----------------------------------------------
        let mut order = [0usize; N];
        let count = self.windows.z_order(&mut order);
        let work_area = self.work_area;
        let mut close_after = [false; N];
        let mut menu_request = None;
        for position in 0..count {
            let index = order[position];
            let covered = order[position + 1..count].iter().any(|above| {
                self.windows
                    .get(*above)
                    .is_some_and(|window| window.rect.contains(pointer))
            });
            ui.set_pointer_occluded(above_windows || covered);
            let paint_app = self
                .windows
                .get(index)
                .is_some_and(|window| !window.rect.clipped_to(damage).is_empty());
            let outcome = render_window(
                &mut ui,
                &theme,
                &mut self.windows,
                &self.catalog,
                index,
                work_area,
                &mut self.clicks,
                uptime_ms,
                input,
                platform,
                pointer_window == Some(index),
                keyboard_window == Some(index),
                paint_app,
                self.scratch.as_deref_mut(),
            );
            close_after[index] = outcome.close;
            if let Some(point) = outcome.menu {
                menu_request = Some((index, point));
            }
        }

        ui.set_pointer_occluded(above_bar);
        let taskbar_response = taskbar::render(
            &mut ui,
            &theme,
            &config.taskbar,
            &layout,
            &self.windows,
            &self.catalog,
            clock.as_str(),
            self.notifications.unread(),
            self.sound,
        );

        ui.set_pointer_occluded(modal || drag_active || in_toast);
        let menu_response = start_menu::render(
            &mut ui,
            &theme,
            &config.start_menu,
            &layout,
            config.taskbar.position,
            &self.catalog,
            &mut self.start_menu,
        );

        ui.set_pointer_occluded(modal || drag_active);
        self.notifications
            .render_toasts(&mut ui, &theme, &layout, self.work_area);
        ui.set_pointer_occluded(false);

        let pointer_pressed = ui.pointer_pressed();
        let pointer_released = ui.pointer_released();
        let secondary_released = ui.secondary_released();

        // A right click on the wallpaper opens the desktop menu. Right clicks
        // on a window, the Start menu or the taskbar belong to those surfaces,
        // and none of them react while a dialog is up.
        if secondary_released
            && !modal_at_start
            && !drag_active
            && !over_window
            && !in_bar
            && !in_toast
            && !menu_response.panel.contains(pointer)
        {
            self.start_menu.close();
            self.context.show(pointer);
        }

        let context_action = context_choice.or(render_menu(
            &mut ui,
            &theme,
            &mut self.context,
            self.work_area,
            pointer_pressed,
            &CONTEXT_ITEMS,
            "desktop.context",
        ));
        let window_action = window_choice.or(render_menu(
            &mut ui,
            &theme,
            &mut self.window_menu,
            self.work_area,
            pointer_pressed,
            &WINDOW_ITEMS,
            "desktop.window_menu",
        ));

        let run_clicked = if self.run_dialog.open {
            render_run_dialog(&mut ui, &theme, self.work_area, &mut self.run_dialog)
        } else {
            false
        };

        let shutdown_action = if self.shutdown.open {
            render_shutdown_dialog(&mut ui, &theme, self.work_area)
        } else {
            ShutdownAction::None
        };

        let clock_closed = if self.clock_open {
            render_clock_dialog(&mut ui, &theme, self.work_area, uptime_ms, clock.as_str())
        } else {
            false
        };

        if let Some(seconds) = self.resolution_prompt {
            let answer = render_resolution_prompt(&mut ui, &theme, self.work_area, seconds);
            let answer = answer.or({
                if enter_pressed {
                    Some(true)
                } else if escape_pressed {
                    Some(false)
                } else {
                    None
                }
            });
            if answer.is_some() {
                self.resolution_answer = answer;
            }
        }
        let logoff_action = if self.logoff_open {
            render_logoff_dialog(&mut ui, &theme, self.work_area)
        } else {
            LogoffAction::None
        };

        let settings_action = if self.settings_open {
            render_settings_dialog(&mut ui, &theme, self.work_area, &config)
        } else {
            SettingsAction::None
        };

        if self.notifications_open {
            let _blocker = ui.interact(
                Id::from_name("desktop.notifications.modal"),
                ui.canvas_rect(),
                Sense::click(),
            );
            let history = self
                .notifications
                .render_history(&mut ui, &theme, &layout, self.work_area);
            if pointer_pressed && !history.contains(pointer) {
                self.notifications_open = false;
            }
        }

        if self.volume_open {
            let _blocker = ui.interact(
                Id::from_name("desktop.volume.modal"),
                ui.canvas_rect(),
                Sense::click(),
            );
            let panel = volume::render_panel(&mut ui, &theme, &layout, self.work_area, &mut self.sound);
            if pointer_pressed && !panel.contains(pointer) {
                self.volume_open = false;
            }
        }

        if let Some(payload) = self.drag {
            paint::draw_drag_label(&mut ui.painter, &theme, pointer, payload.as_str());
        }

        // The pointer is painted last so it stays visible over windows and the
        // Start menu. Its shape follows the window edge underneath it.
        let over_desktop_menu = self.start_menu.open
            || modal
            || self.drag.is_some()
            || in_bar
            || in_toast;
        // A text field the desktop drew itself, such as the Run dialog's,
        // asks for the text beam.
        let cursor_shape = if ui.cursor() != CursorShape::Arrow {
            ui.cursor()
        } else if over_desktop_menu {
            CursorShape::Arrow
        } else {
            window_cursor_shape(&self.windows, pointer)
        };
        response.cursor = cursor_shape;
        if !self.retained {
            paint::draw_cursor_shape(&mut ui.painter, pointer.x, pointer.y, cursor_shape);
        }

        drop(ui);

        // State changes -------------------------------------------------------
        for (index, close) in close_after.iter().enumerate() {
            if *close {
                let failed = self.windows.get_mut(index).and_then(|window| {
                    let status = window.failed.take()?;
                    Some((window.runtime?, status))
                });
                if let Some((runtime, status)) = failed {
                    self.notify_failed(runtime.name(), status, uptime_ms);
                }
            }
            if *close {
                self.close_window(index);
            }
        }

        if pointer_released || released_now {
            self.pointer_window = None;
        }

        if run_dialog_open && (run_submit || run_clicked) {
            let command = self.run_dialog.text;
            if !command.trim().is_empty() {
                response.run = Some(command);
            }
            self.run_dialog.close();
        }

        if clock_closed {
            self.clock_open = false;
        }

        let enter =
            enter_pressed && !modal_at_start && !(self.drag.is_some() || drag_active);

        if taskbar_response.start_clicked {
            self.start_menu.toggle();
        }

        if let Some((index, point)) = menu_request {
            self.windows.focus(index);
            self.open_window_menu(index, point);
        }
        if let Some(index) = taskbar_response.task_menu {
            self.open_window_menu(index, pointer);
        }

        if let Some(index) = taskbar_response.task_clicked {
            self.start_menu.close();
            self.show_desktop = false;
            // XP uses a double-click on a taskbar button to toggle between
            // normal and maximized. The click memory already used by title
            // bars and desktop icons gives us the same interval here.
            let double = self
                .clicks
                .hit(0x5000_0000 | index as u32, pointer, uptime_ms);
            if double {
                self.windows.toggle_maximize(index, self.work_area);
            } else {
                let minimized = self
                    .windows
                    .get(index)
                    .is_some_and(|window| window.state == WindowState::Minimized);
                let suspended = self
                    .windows
                    .get(index)
                    .is_some_and(|window| window.suspended);
                if minimized {
                    // A taskbar button for a minimised window brings it back,
                    // including returning it maximised when that is how it was
                    // before it was minimised.
                    self.windows.restore(index);
                    if suspended {
                        // A foreground app owned the screen before it was
                        // suspended; run it again now that its window is shown.
                        if let Some(window) = self.windows.get_mut(index) {
                            window.suspended = false;
                            launch_index = Some(window.app);
                        }
                    }
                } else if self.windows.is_focused(index) {
                    // Clicking the focused window's button minimises it,
                    // whether the window is normal or maximised.
                    self.windows.minimize(index);
                } else {
                    self.windows.focus(index);
                }
            }
        }

        if taskbar_response.desktop_clicked {
            self.toggle_show_desktop();
        }

        if taskbar_response.tray_clicked {
            self.start_menu.close();
            self.clock_open = true;
            response.tray_clicked = true;
        }

        if taskbar_response.speaker_wheel != 0 {
            let change = if taskbar_response.speaker_wheel > 0 { 5 } else { -5 };
            self.sound.set_level(self.sound.level as i32 + change);
        }

        if taskbar_response.speaker_clicked {
            self.start_menu.close();
            self.volume_open = !self.volume_open;
        }

        if taskbar_response.bell_clicked {
            self.start_menu.close();
            self.notifications.mark_read();
            self.notifications.dismiss_all();
            self.notifications_open = true;
        }

        let mut launch_index = menu_response.launched.or(launch_index);
        if let Some(index) = taskbar_response.quick_clicked {
            self.start_menu.close();
            launch_index = Some(index);
        }

        let mut command = menu_response.command;
        if command == StartMenuCommand::Run {
            self.run_dialog.show();
            self.context.open = false;
            command = StartMenuCommand::None;
        }
        if command == StartMenuCommand::ShutDown {
            self.shutdown.open = true;
            self.context.open = false;
            command = StartMenuCommand::None;
        }
        if command == StartMenuCommand::LogOff {
            self.logoff_open = true;
            self.context.open = false;
            command = StartMenuCommand::None;
        }
        if command == StartMenuCommand::Settings {
            self.settings_open = true;
            self.context.open = false;
            command = StartMenuCommand::None;
        }
        match context_action.unwrap_or_default() {
            ContextAction::None => {}
            ContextAction::Refresh => {
                self.repaint_all = true;
                command = StartMenuCommand::Refresh;
            }
            ContextAction::ShowDesktop => {
                self.toggle_show_desktop();
            }
            ContextAction::TaskManager => {
                if let Some(index) = self.catalog.find_id("process-monitor") {
                    launch_index = Some(index);
                }
            }
            ContextAction::Properties => self.settings_open = true,
        }
        if let Some(action) = window_action {
            self.apply_window_action(self.window_menu_target, action);
        }

        match settings_action {
            SettingsAction::None => {}
            SettingsAction::Theme(name) => {
                self.set_theme(name);
                self.settings_changed = true;
            }
            SettingsAction::Layout(layout) => {
                let mut next = self.config;
                next.keyboard_layout = layout;
                self.set_config(next);
                self.settings_changed = true;
            }
            SettingsAction::Close => self.settings_open = false,
        }

        match shutdown_action {
            ShutdownAction::None => {}
            ShutdownAction::Cancel => self.shutdown.open = false,
            ShutdownAction::TurnOff => {
                self.shutdown.open = false;
                command = StartMenuCommand::ShutDown;
            }
            ShutdownAction::Restart => {
                self.shutdown.open = false;
                command = StartMenuCommand::Restart;
            }
        }

        match logoff_action {
            LogoffAction::None => {}
            LogoffAction::Cancel => self.logoff_open = false,
            LogoffAction::LogOff => {
                self.logoff_open = false;
                command = StartMenuCommand::LogOff;
            }
        }

        if self.start_menu.open && enter {
            launch_index = self
                .start_menu
                .selected(self.catalog.len())
                .or(launch_index);
        } else if enter && !window_owns_keyboard && self.selection < self.catalog.len() {
            // Enter launches the wallpaper shortcut selected with the arrows,
            // but a focused app's Enter is that app's.
            launch_index = Some(self.selection).or(launch_index);
        }

        // A dragged path ends where the button came up: a shortcut opens it, and
        // an open window is offered it.
        if dropped {
            if let Some(payload) = self.drag.take() {
                if let Some(index) =
                    shortcut_at(&config, &self.catalog, self.work_area, pointer).filter(|_| !over_window)
                {
                    launch_index = Some(index);
                    launch_args = Some(payload);
                } else if let Some(index) = self.windows.topmost_at(pointer).filter(|_| !in_bar) {
                    let window = self.windows.get(index).copied();
                    let taken = window
                        .and_then(|window| window.runtime)
                        .is_some_and(|runtime| runtime.accept_drop(payload.as_str()));
                    if taken {
                        self.windows.focus(index);
                    }
                }
            }
        }

        if let Some(index) = launch_index {
            self.show_desktop = false;
            let args = launch_args.as_ref().map(|args| args.as_str());
            if let Some(window) = self.open_app_with(index, args) {
                if let Some(app) = self.catalog.get(index) {
                    let args = self
                        .windows
                        .get(window)
                        .map(|window| window.args)
                        .unwrap_or(Text::EMPTY);
                    response.launch = Some(LaunchRequest {
                        app: index,
                        entry: app.manifest.entry,
                        args,
                        window,
                    });
                }
            }
            self.start_menu.close();
        }

        response.command = command;

        if pointer_pressed
            && !taskbar_response.start_clicked
            && self.start_menu.open
            && !menu_response.panel.contains(pointer)
            && !in_bar
        {
            self.start_menu.close();
        }

        // Clicking the wallpaper moves keyboard focus off the active window.
        if pointer_pressed
            && !over_window
            && !start_open
            && !modal_at_start
            && !in_bar
            && !in_toast
            && !menu_response.panel.contains(pointer)
        {
            self.windows.clear_focus();
        }

        if input
            .keys()
            .any(|(key, modifiers)| key == Key::Escape && !modifiers.contains(Modifiers::CTRL))
        {
            self.start_menu.close();
        }

        response.start_open = self.start_menu.open;
        response.window_count = self.windows.count();
        for index in 0..N {
            if let Some(window) = self.windows.get(index) {
                if window.open
                    && window.state != WindowState::Minimized
                    && window.runtime.is_some_and(|app| app.animates())
                {
                    response.redraw = true;
                    if let Some(app) = window.runtime {
                        let period = app.animation_period_ms().max(1);
                        if response.animation_period_ms == 0
                            || period < response.animation_period_ms
                        {
                            response.animation_period_ms = period;
                        }
                    }
                }
            }
        }

        response.damage = (!damage.is_empty()).then_some(damage);
        // Closing a dialog gives the app the keyboard back from the next frame.
        let overlay_after = self.start_menu.open || self.modal_open() || self.drag.is_some();
        response.state_changed = self.repaint_all
            || desktop_overlay != overlay_after
            || self.window_snapshot() != self.painted_windows
            || Some(self.desktop_snapshot()) != self.painted_desktop;
        response
    }

    fn window_snapshot(&self) -> [PaintedWindow; N] {
        core::array::from_fn(|index| {
            let Some(window) = self.windows.get(index) else {
                return PaintedWindow::CLOSED;
            };
            if !window.open {
                return PaintedWindow::CLOSED;
            }
            PaintedWindow {
                open: true,
                state: window.state,
                rect: window.rect,
                z: window.z,
                focused: self.windows.is_focused(index),
                hosted: window.runtime.is_some(),
                started: window.app_started,
                launched: window.launched,
                status: window.status,
            }
        })
    }

    fn desktop_snapshot(&self) -> PaintedDesktop {
        PaintedDesktop {
            overlay: self.start_menu.open || self.modal_open() || self.drag.is_some(),
            start_open: self.start_menu.open,
            start_all_programs: self.start_menu.all_programs,
            start_highlighted: self.start_menu.highlighted,
            start_scroll: self.start_menu.scroll,
            selection: self.selection,
            context: self.context,
            window_menu: self.window_menu,
            toasts: self.notifications.visible_count(),
            unread: self.notifications.unread(),
            sound: self.sound,
            show_desktop: self.show_desktop,
        }
    }

    /// The part of the canvas this frame has to repaint.
    ///
    /// Hosted apps paint a whole window or nothing, straight into the canvas.
    /// So a window that touches the damage joins it, which makes the windows
    /// above and below it repaint as well, until the set stops growing.
    #[allow(clippy::too_many_arguments)]
    fn compute_damage(
        &self,
        input: &Input,
        now_ms: u64,
        canvas: Rect,
        layout: &taskbar::TaskbarLayout,
        clock: &Text<16>,
        pointer_window: Option<usize>,
        keyboard_window: Option<usize>,
        windows: &[PaintedWindow; N],
        desktop: &PaintedDesktop,
    ) -> Rect {
        let Some(previous) = self.painted_desktop.as_ref() else {
            return canvas;
        };
        let size = (canvas.width, canvas.height);
        if !self.retained
            || self.repaint_all
            || self.painted_size != size
            || desktop.overlay
            || previous.overlay
            || previous != desktop
        {
            return canvas;
        }

        let mut damage = Rect::new(0, 0, 0, 0);
        for (now, then) in windows.iter().zip(self.painted_windows.iter()) {
            if now != then {
                if let Some(rect) = now.visible_rect() {
                    damage = union(damage, rect);
                }
                if let Some(rect) = then.visible_rect() {
                    damage = union(damage, rect);
                }
                // Task buttons show every window's state.
                damage = union(damage, layout.bar);
            }
        }
        if clock != &self.painted_clock {
            damage = union(damage, layout.bar);
        }

        // An app draws differently once it has or loses the pointer or keyboard.
        let (old_pointer, old_keyboard) = self.painted_routes;
        for (old, new) in [(old_pointer, pointer_window), (old_keyboard, keyboard_window)] {
            if old != new {
                for index in [old, new].into_iter().flatten() {
                    if let Some(rect) = windows.get(index).and_then(PaintedWindow::visible_rect) {
                        damage = union(damage, rect);
                    }
                }
            }
        }

        let pointer = input.pointer();
        let pointer_events = input.events().any(|event| {
            matches!(
                event,
                Event::PointerMoved(_)
                    | Event::PointerDown { .. }
                    | Event::PointerUp { .. }
                    | Event::PointerWheel(_)
            )
        });
        if pointer_events || pointer != self.painted_pointer {
            damage = union(damage, self.pointer_damage(pointer, layout));
            damage = union(damage, self.pointer_damage(self.painted_pointer, layout));
            if let Some(window) = pointer_window.and_then(|index| self.windows.get(index)) {
                damage = union(damage, window.rect);
            }
        }

        let keyboard_events = input
            .events()
            .any(|event| matches!(event, Event::KeyDown(..) | Event::KeyUp(..) | Event::Text(_)));
        if keyboard_events {
            if let Some(window) = keyboard_window.and_then(|index| self.windows.get(index)) {
                damage = union(damage, window.rect);
            }
        }

        for index in 0..N {
            let Some(window) = self.windows.get(index) else {
                continue;
            };
            if !window.open || window.state == WindowState::Minimized {
                continue;
            }
            if let Some(runtime) = window.runtime.filter(|runtime| runtime.animates()) {
                let period = runtime.animation_period_ms().max(1) as u64;
                if now_ms.saturating_sub(window.last_frame_ms) >= period {
                    damage = union(damage, window.rect);
                }
            }
        }

        // Hosted apps paint all of a window or none of it.
        if !damage.is_empty() {
            loop {
                let mut grew = false;
                for window in windows.iter() {
                    let Some(rect) = window.visible_rect() else {
                        continue;
                    };
                    if !rect.clipped_to(damage).is_empty() {
                        let next = union(damage, rect);
                        if next != damage {
                            damage = next;
                            grew = true;
                        }
                    }
                }
                if !grew {
                    break;
                }
            }
        }
        damage.clipped_to(canvas)
    }

    /// What can look different when the pointer enters or leaves `point`.
    fn pointer_damage(&self, point: Point, layout: &taskbar::TaskbarLayout) -> Rect {
        if layout.bar.contains(point) {
            return layout.bar;
        }
        let toasts = self.notifications.area(layout, self.work_area);
        if toasts.contains(point) {
            return toasts;
        }
        if let Some(window) = self
            .windows
            .topmost_at(point)
            .and_then(|index| self.windows.get(index))
        {
            let title = Rect::new(
                window.rect.x,
                window.rect.y,
                window.rect.width,
                window::TITLE_HEIGHT + 2,
            );
            return if title.contains(point) {
                title
            } else {
                window.rect
            };
        }
        shortcut_at(&self.config, &self.catalog, self.work_area, point)
            .and_then(|index| shortcut_cell(&self.config, self.work_area, index))
            .unwrap_or(Rect::new(0, 0, 0, 0))
    }
}

impl<const N: usize, const A: usize, const S: usize> Default for Desktop<N, A, S> {
    fn default() -> Self {
        Self::new(DesktopConfig::default())
    }
}

/// The smallest rectangle containing both, treating an empty one as nothing.
fn union(a: Rect, b: Rect) -> Rect {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = a.right_i32().max(b.right_i32());
    let y1 = a.bottom_i32().max(b.bottom_i32());
    Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32)
}

/// A desktop-wide shortcut, which no app ever sees.
fn is_shortcut(key: Key, modifiers: Modifiers) -> bool {
    let alt = modifiers.contains(Modifiers::ALT);
    let ctrl = modifiers.contains(Modifiers::CTRL);
    match key {
        Key::Escape => ctrl,
        Key::Tab | Key::F4 | Key::Space => alt,
        _ => false,
    }
}

/// Moves a pop-up menu's highlight, and returns the action Enter chose.
fn handle_menu_keys<T: Copy>(
    state: &mut ContextMenuState,
    items: &[MenuItem<T>],
    input: &Input,
) -> Option<T> {
    let count = items.len();
    let mut chosen = None;
    for (key, _) in input.keys() {
        match key {
            Key::Up => {
                state.highlighted = match state.highlighted {
                    0 => count - 1,
                    NOTHING_HIGHLIGHTED => count - 1,
                    other => other - 1,
                };
            }
            Key::Down => {
                state.highlighted = match state.highlighted {
                    NOTHING_HIGHLIGHTED => 0,
                    other => (other + 1) % count,
                };
            }
            Key::Enter => {
                if let Some(item) = items.get(state.highlighted) {
                    chosen = Some(item.action);
                }
                state.open = false;
            }
            Key::Escape => state.open = false,
            _ => {}
        }
    }
    chosen
}

/// [`ContextMenuState::highlighted`] when no row is highlighted.
const NOTHING_HIGHLIGHTED: usize = usize::MAX;

/// Draws the wallpaper and, when a cache is available, the shortcut icons from
/// it. Returns `true` when the icons were drawn from the cache.
fn paint_background<const UI: usize, const A: usize>(
    ui: &mut Ui<'_, '_, '_, UI>,
    theme: &DesktopTheme,
    config: &DesktopConfig,
    catalog: &AppCatalog<A>,
    work_area: Rect,
    cache: Option<&mut [u32]>,
    valid: &mut bool,
) -> bool {
    let full = ui.canvas_rect();
    let width = full.width as usize;
    let height = full.height as usize;
    let Some(cache) = cache.filter(|cache| cache.len() >= width * height) else {
        paint::draw_wallpaper_on(&mut ui.painter, theme, &config.wallpaper);
        return false;
    };

    if !*valid {
        if let Some(mut layer) = Canvas::new(cache, width, height, width) {
            let mut painter = feos_gui::Painter::new(&mut layer);
            paint::draw_wallpaper_on(&mut painter, theme, &config.wallpaper);
            if config.icons.show_icons {
                for index in 0..catalog.len().min(MAX_DESKTOP_ICONS) {
                    let Some(cell) = shortcut_cell(config, work_area, index) else {
                        break;
                    };
                    paint_shortcut(&mut painter, theme, config, catalog, index, cell, false);
                }
            }
        }
        *valid = true;
    }
    ui.painter.restore_layer(full, cache, width, height);
    true
}

/// The rectangle of shortcut `index`, or `None` past the last one that fits.
fn shortcut_cell(config: &DesktopConfig, work_area: Rect, index: usize) -> Option<Rect> {
    if work_area.is_empty() {
        return None;
    }
    let icons = &config.icons;
    let icon_size = icons.icon_size.clamp(24, 96) as i32;
    let label_height = if icons.show_labels { 34 } else { 8 };
    let cell_width = (icon_size + 60).max(150);
    let cell_height = icon_size + label_height;
    let rows = (work_area.height as i32 / cell_height).max(1);
    let row = index as i32 % rows;
    let column = index as i32 / rows;
    let cell = Rect::new(
        work_area.x + 10 + column * (cell_width + 6),
        work_area.y + 10 + row * cell_height,
        cell_width as u32,
        cell_height as u32,
    );
    (cell.right_i32() <= work_area.right_i32() && cell.bottom_i32() <= work_area.bottom_i32())
        .then_some(cell)
}

/// The shortcut under `point`, if any.
fn shortcut_at<const A: usize>(
    config: &DesktopConfig,
    catalog: &AppCatalog<A>,
    work_area: Rect,
    point: Point,
) -> Option<usize> {
    if !config.icons.show_icons {
        return None;
    }
    (0..catalog.len().min(MAX_DESKTOP_ICONS)).find(|index| {
        shortcut_cell(config, work_area, *index).is_some_and(|cell| cell.contains(point))
    })
}

fn paint_shortcut<const A: usize>(
    painter: &mut feos_gui::Painter<'_, '_>,
    theme: &DesktopTheme,
    config: &DesktopConfig,
    catalog: &AppCatalog<A>,
    index: usize,
    cell: Rect,
    hovered: bool,
) {
    let icons = &config.icons;
    let icon_size = icons.icon_size.clamp(24, 96) as i32;
    if let Some(app) = catalog.get(index) {
        let icon_rect = Rect::new(
            cell.x + (cell.width as i32 - icon_size) / 2,
            cell.y + 2,
            icon_size as u32,
            icon_size as u32,
        );
        paint::draw_icon(painter, &app.icon, icon_rect, theme.caption_button);
        if icons.show_labels {
            draw_shortcut_label(
                painter,
                Rect::new(cell.x + 2, cell.y + icon_size + 2, cell.width - 4, 30),
                app.name(),
                if hovered {
                    theme.desktop_text_hover
                } else {
                    theme.desktop_text
                },
            );
        }
    }
}

/// A choice in the Display Properties dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum SettingsAction {
    #[default]
    None,
    Theme(ThemeName),
    Layout(KeyboardLayout),
    Close,
}

fn render_settings_dialog<const N: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    work_area: Rect,
    config: &DesktopConfig,
) -> SettingsAction {
    const WIDTH: u32 = 420;
    const HEIGHT: u32 = 256;
    const TITLE: u32 = 28;
    const ROW: u32 = 24;

    let x = work_area.x + (work_area.width as i32 - WIDTH as i32) / 2;
    let y = work_area.y + (work_area.height as i32 - HEIGHT as i32) / 2;
    let panel = Rect::new(x.max(work_area.x), y.max(work_area.y), WIDTH, HEIGHT);

    let _blocker = ui.interact(
        Id::from_name("desktop.settings.modal"),
        ui.canvas_rect(),
        Sense::click(),
    );

    ui.painter.shadow(panel, 8, 10, 120);
    ui.painter.fill_rounded_rect(panel, 4, theme.menu_bg);
    ui.painter
        .stroke_rounded_rect(panel, 4, 1, theme.menu_border);
    let title = Rect::new(panel.x + 1, panel.y + 1, panel.width - 2, TITLE);
    ui.painter
        .fill_vertical_gradient(title, theme.title_active_top, theme.title_active_bottom);
    ui.painter.draw_text_ellipsis(
        Rect::new(title.x + 10, title.y, title.width.saturating_sub(20), TITLE),
        "Display Properties",
        theme.title_text,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );

    let mut action = SettingsAction::None;
    let top = panel.y + TITLE as i32 + 8;
    let column = |index: i32| panel.x + 12 + index * 200;
    for (index, heading) in ["Colour scheme", "Keyboard layout"].iter().enumerate() {
        ui.painter.draw_text_ellipsis(
            Rect::new(column(index as i32), top, 190, 18),
            heading,
            theme.menu_muted,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }

    let mut row_y = top + 20;
    for (index, name) in ThemeName::ALL.iter().enumerate() {
        let row = Rect::new(column(0), row_y, 190, ROW);
        let id = Id::from_name("desktop.settings.theme").child(index as u64 + 1);
        let hit = ui.interact(id, row, Sense::click());
        if hit.clicked {
            action = SettingsAction::Theme(*name);
        }
        draw_setting_row(ui, theme, row, name.name(), hit.hovered, config.theme == *name);
        row_y += ROW as i32;
    }

    let mut row_y = top + 20;
    for (index, layout) in KeyboardLayout::ALL.iter().enumerate() {
        let row = Rect::new(column(1), row_y, 190, ROW);
        let id = Id::from_name("desktop.settings.layout").child(index as u64 + 1);
        let hit = ui.interact(id, row, Sense::click());
        if hit.clicked {
            action = SettingsAction::Layout(*layout);
        }
        draw_setting_row(
            ui,
            theme,
            row,
            layout.name(),
            hit.hovered,
            config.keyboard_layout == *layout,
        );
        row_y += ROW as i32;
    }

    let close = Rect::new(panel.right_i32() - 88, panel.bottom_i32() - 34, 72, 24);
    let close_hit = ui.interact(Id::from_name("desktop.settings.close"), close, Sense::click());
    ui.painter.fill_rounded_rect(
        close,
        3,
        if close_hit.hovered {
            theme.caption_button.lerp(Color::WHITE, 40)
        } else {
            theme.caption_button
        },
    );
    ui.painter
        .stroke_rounded_rect(close, 3, 1, theme.menu_border);
    ui.painter
        .draw_text_centered(close, "Close", Color::WHITE, 1);
    if close_hit.clicked {
        action = SettingsAction::Close;
    }
    action
}

fn draw_setting_row<const N: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    row: Rect,
    label: &str,
    hovered: bool,
    selected: bool,
) {
    if hovered {
        ui.painter.fill_rounded_rect(row, 2, theme.menu_hover);
    }
    let mark = Rect::new(row.x + 6, row.y + 7, 10, 10);
    ui.painter.stroke_rect(mark, 1, theme.menu_text);
    if selected {
        ui.painter.fill_rect(
            Rect::new(mark.x + 2, mark.y + 2, 6, 6),
            theme.menu_text,
        );
    }
    ui.painter.draw_text_ellipsis(
        Rect::new(row.x + 24, row.y, row.width.saturating_sub(28), row.height),
        label,
        if hovered {
            theme.menu_hover_text
        } else {
            theme.menu_text
        },
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
}

fn render_logoff_dialog<const N: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    work_area: Rect,
) -> LogoffAction {
    const WIDTH: u32 = 360;
    const HEIGHT: u32 = 158;
    const TITLE: u32 = 28;

    let full = ui.canvas_rect();
    ui.painter.fill_rect_alpha(full, Color::BLACK, 90);
    let _blocker = ui.interact(Id::from_name("desktop.logoff.modal"), full, Sense::click());

    let x = work_area.x + (work_area.width as i32 - WIDTH as i32) / 2;
    let y = work_area.y + (work_area.height as i32 - HEIGHT as i32) / 2;
    let panel = Rect::new(x.max(work_area.x), y.max(work_area.y), WIDTH, HEIGHT);

    ui.painter.shadow(panel, 8, 10, 120);
    ui.painter.fill_rounded_rect(panel, 4, theme.menu_bg);
    ui.painter
        .stroke_rounded_rect(panel, 4, 1, theme.menu_border);

    let title = Rect::new(panel.x + 1, panel.y + 1, panel.width - 2, TITLE);
    ui.painter.fill_vertical_gradient(
        title,
        theme.title_active_top,
        theme.title_active_bottom,
    );
    ui.painter.draw_text_ellipsis(
        Rect::new(
            title.x + 10,
            title.y,
            title.width.saturating_sub(20),
            title.height,
        ),
        "Log Off",
        theme.title_text,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );

    ui.painter.draw_text_ellipsis(
        Rect::new(
            panel.x + 16,
            panel.y + 48,
            panel.width.saturating_sub(32),
            24,
        ),
        "Are you sure you want to log off?",
        theme.menu_text,
        1,
        Align::Center,
        VerticalAlign::Middle,
    );

    let logoff = Rect::new(panel.right_i32() - 166, panel.bottom_i32() - 38, 70, 24);
    let cancel = Rect::new(panel.right_i32() - 88, panel.bottom_i32() - 38, 72, 24);
    let logoff_hit = ui.interact(
        Id::from_name("desktop.logoff.confirm"),
        logoff,
        Sense::click(),
    );
    let cancel_hit = ui.interact(
        Id::from_name("desktop.logoff.cancel"),
        cancel,
        Sense::click(),
    );

    for (rect, hit, label, color) in [
        (logoff, logoff_hit, "Log Off", theme.caption_button),
        (cancel, cancel_hit, "Cancel", theme.task_button),
    ] {
        ui.painter.fill_rounded_rect(
            rect,
            3,
            if hit.hovered {
                color.lerp(Color::WHITE, 40)
            } else {
                color
            },
        );
        ui.painter
            .stroke_rounded_rect(rect, 3, 1, theme.menu_border);
        ui.painter.draw_text_centered(rect, label, Color::WHITE, 1);
    }

    if logoff_hit.clicked {
        LogoffAction::LogOff
    } else if cancel_hit.clicked {
        LogoffAction::Cancel
    } else {
        LogoffAction::None
    }
}

/// Asks whether a new screen size can be seen, and so should stay.
fn render_resolution_prompt<const N: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    work_area: Rect,
    seconds: u32,
) -> Option<bool> {
    const WIDTH: u32 = 380;
    const HEIGHT: u32 = 140;
    const TITLE: u32 = 28;

    let full = ui.canvas_rect();
    let _blocker = ui.interact(Id::from_name("desktop.resolution.modal"), full, Sense::click());
    let x = work_area.x + (work_area.width as i32 - WIDTH as i32) / 2;
    let y = work_area.y + (work_area.height as i32 - HEIGHT as i32) / 2;
    let panel = Rect::new(x.max(work_area.x), y.max(work_area.y), WIDTH, HEIGHT);
    ui.painter.shadow(panel, 8, 10, 120);
    ui.painter.fill_rounded_rect(panel, 4, theme.menu_bg);
    ui.painter.stroke_rounded_rect(panel, 4, 1, theme.menu_border);
    let title = Rect::new(panel.x + 1, panel.y + 1, panel.width - 2, TITLE);
    ui.painter
        .fill_vertical_gradient(title, theme.title_active_top, theme.title_active_bottom);
    ui.painter.draw_text_ellipsis(
        Rect::new(title.x + 10, title.y, title.width.saturating_sub(20), TITLE),
        "Screen Resolution",
        theme.title_text,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let mut line = Text::<64>::new();
    let _ = write!(line, "Keep this size? Going back in {seconds} s.");
    ui.painter.draw_text_ellipsis(
        Rect::new(panel.x + 16, panel.y + 48, panel.width.saturating_sub(32), 24),
        line.as_str(),
        theme.menu_text,
        1,
        Align::Center,
        VerticalAlign::Middle,
    );
    let keep = Rect::new(panel.right_i32() - 166, panel.bottom_i32() - 38, 70, 24);
    let revert = Rect::new(panel.right_i32() - 88, panel.bottom_i32() - 38, 72, 24);
    let keep_hit = ui.interact(Id::from_name("desktop.resolution.keep"), keep, Sense::click());
    let revert_hit = ui.interact(Id::from_name("desktop.resolution.revert"), revert, Sense::click());
    for (rect, hit, label, color) in [
        (keep, keep_hit, "Keep", theme.caption_button),
        (revert, revert_hit, "Revert", theme.task_button),
    ] {
        ui.painter.fill_rounded_rect(
            rect,
            3,
            if hit.hovered { color.lerp(Color::WHITE, 40) } else { color },
        );
        ui.painter.stroke_rounded_rect(rect, 3, 1, theme.menu_border);
        ui.painter.draw_text_centered(rect, label, Color::WHITE, 1);
    }
    if keep_hit.clicked {
        Some(true)
    } else if revert_hit.clicked {
        Some(false)
    } else {
        None
    }
}

fn render_clock_dialog<const N: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    work_area: Rect,
    uptime_ms: u64,
    clock: &str,
) -> bool {
    const WIDTH: u32 = 410;
    const HEIGHT: u32 = 236;
    const TITLE: u32 = 28;
    const HOUR_END: [(i32, i32); 12] = [
        (0, -30),
        (15, -26),
        (26, -15),
        (30, 0),
        (26, 15),
        (15, 26),
        (0, 30),
        (-15, 26),
        (-26, 15),
        (-30, 0),
        (-26, -15),
        (-15, -26),
    ];
    const MINUTE_END: [(i32, i32); 12] = [
        (0, -44),
        (22, -38),
        (38, -22),
        (44, 0),
        (38, 22),
        (22, 38),
        (0, 44),
        (-22, 38),
        (-38, 22),
        (-44, 0),
        (-38, -22),
        (-22, -38),
    ];

    let x = work_area.x + (work_area.width as i32 - WIDTH as i32) / 2;
    let y = work_area.y + (work_area.height as i32 - HEIGHT as i32) / 2;
    let panel = Rect::new(x.max(work_area.x), y.max(work_area.y), WIDTH, HEIGHT);

    let _blocker = ui.interact(
        Id::from_name("desktop.clock.modal"),
        ui.canvas_rect(),
        Sense::click(),
    );

    ui.painter.shadow(panel, 8, 10, 120);
    ui.painter.fill_rounded_rect(panel, 4, theme.menu_bg);
    ui.painter
        .stroke_rounded_rect(panel, 4, 1, theme.menu_border);

    let title = Rect::new(panel.x + 1, panel.y + 1, panel.width - 2, TITLE);
    ui.painter.fill_vertical_gradient(
        title,
        theme.title_active_top,
        theme.title_active_bottom,
    );
    ui.painter.draw_text_ellipsis(
        Rect::new(
            title.x + 10,
            title.y,
            title.width.saturating_sub(20),
            title.height,
        ),
        "Date and Time Properties",
        theme.title_text,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );

    // Analog clock face --------------------------------------------------
    let cx = panel.x + 92;
    let cy = panel.y + 128;
    ui.painter.circle(cx, cy, 58, theme.menu_border);
    ui.painter.circle(cx, cy, 54, Color::WHITE);
    for step in 0..12 {
        let (dx, dy) = MINUTE_END[step];
        let tick_x = cx + dx * 55 / 44;
        let tick_y = cy + dy * 55 / 44;
        ui.painter
            .fill_rect(Rect::new(tick_x - 1, tick_y - 1, 2, 2), theme.menu_muted);
    }

    let total_seconds = uptime_ms / 1000;
    let hours = (total_seconds / 3600) % 24;
    let minutes = (total_seconds / 60) % 60;
    let (hx, hy) = HOUR_END[(hours % 12) as usize];
    let (mx, my) = MINUTE_END[((minutes / 5) % 12) as usize];
    ui.painter.line(cx, cy, cx + hx, cy + hy, theme.menu_text);
    ui.painter.line(cx, cy, cx + mx, cy + my, theme.menu_text);
    ui.painter.circle(cx, cy, 3, theme.menu_border);

    // Current time and uptime -------------------------------------------
    let text_x = panel.x + 170;
    let text_width = panel.width.saturating_sub(190);
    ui.painter.draw_text_ellipsis(
        Rect::new(text_x, panel.y + 52, text_width, 18),
        "Current time",
        theme.menu_muted,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    ui.painter.draw_text_ellipsis(
        Rect::new(text_x, panel.y + 72, text_width, 28),
        clock,
        theme.menu_text,
        2,
        Align::Start,
        VerticalAlign::Middle,
    );

    let days = total_seconds / 86_400;
    let mut uptime = Text::<48>::new();
    let _ = write!(
        uptime,
        "{days} day(s), {:02}:{:02}:{:02}",
        (total_seconds / 3600) % 24,
        minutes,
        total_seconds % 60
    );
    ui.painter.draw_text_ellipsis(
        Rect::new(text_x, panel.y + 116, text_width, 18),
        "Uptime",
        theme.menu_muted,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    ui.painter.draw_text_ellipsis(
        Rect::new(text_x, panel.y + 136, text_width, 18),
        uptime.as_str(),
        theme.menu_text,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );

    let ok = Rect::new(panel.right_i32() - 166, panel.bottom_i32() - 38, 70, 24);
    let cancel = Rect::new(panel.right_i32() - 88, panel.bottom_i32() - 38, 72, 24);
    let ok_hit = ui.interact(Id::from_name("desktop.clock.ok"), ok, Sense::click());
    let cancel_hit = ui.interact(
        Id::from_name("desktop.clock.cancel"),
        cancel,
        Sense::click(),
    );
    for (rect, hit, label) in [(ok, ok_hit, "OK"), (cancel, cancel_hit, "Cancel")] {
        ui.painter.fill_rounded_rect(
            rect,
            3,
            if hit.hovered {
                theme.caption_button.lerp(Color::WHITE, 40)
            } else {
                theme.caption_button
            },
        );
        ui.painter
            .stroke_rounded_rect(rect, 3, 1, theme.menu_border);
        ui.painter.draw_text_centered(rect, label, Color::WHITE, 1);
    }

    ok_hit.clicked || cancel_hit.clicked
}

fn render_shutdown_dialog<const N: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    work_area: Rect,
) -> ShutdownAction {
    const WIDTH: u32 = 470;
    const HEIGHT: u32 = 232;
    const TITLE: u32 = 30;

    let full = ui.canvas_rect();
    ui.painter.fill_rect_alpha(full, Color::BLACK, 110);
    let _blocker = ui.interact(
        Id::from_name("desktop.shutdown.modal"),
        full,
        Sense::click(),
    );

    let x = work_area.x + (work_area.width as i32 - WIDTH as i32) / 2;
    let y = work_area.y + (work_area.height as i32 - HEIGHT as i32) / 2;
    let panel = Rect::new(x.max(work_area.x), y.max(work_area.y), WIDTH, HEIGHT);

    ui.painter.shadow(panel, 8, 10, 130);
    ui.painter.fill_rounded_rect(panel, 4, theme.menu_bg);
    ui.painter
        .stroke_rounded_rect(panel, 4, 1, theme.menu_border);

    let title = Rect::new(panel.x + 1, panel.y + 1, panel.width - 2, TITLE);
    ui.painter.fill_vertical_gradient(
        title,
        theme.title_active_top,
        theme.title_active_bottom,
    );
    ui.painter.draw_text_ellipsis(
        Rect::new(
            title.x + 10,
            title.y,
            title.width.saturating_sub(20),
            title.height,
        ),
        "Turn off computer",
        theme.title_text,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );

    ui.painter.draw_text_ellipsis(
        Rect::new(
            panel.x + 16,
            panel.y + 42,
            panel.width.saturating_sub(32),
            24,
        ),
        "What do you want the computer to do?",
        theme.menu_text,
        1,
        Align::Center,
        VerticalAlign::Middle,
    );

    let button_width = 120u32;
    let button_height = 78u32;
    let gap = 16u32;
    let total = button_width * 3 + gap * 2;
    let base_x = panel.x + ((panel.width - total) / 2) as i32;
    let base_y = panel.bottom_i32() - button_height as i32 - 18;
    let buttons = [
        (ShutdownAction::TurnOff, "Turn Off", theme.close_button),
        (ShutdownAction::Restart, "Restart", theme.caption_button),
        (ShutdownAction::Cancel, "Cancel", theme.task_button),
    ];

    let mut action = ShutdownAction::None;
    for (index, (item_action, label, color)) in buttons.iter().enumerate() {
        let rect = Rect::new(
            base_x + index as i32 * (button_width + gap) as i32,
            base_y,
            button_width,
            button_height,
        );
        let id = Id::from_name("desktop.shutdown.button").child(index as u64 + 1);
        let hit = ui.interact(id, rect, Sense::click());
        let fill = if hit.hovered {
            color.lerp(Color::WHITE, 35)
        } else {
            *color
        };
        ui.painter.fill_rounded_rect(rect, 4, fill);
        ui.painter
            .stroke_rounded_rect(rect, 4, 1, theme.menu_border);

        let cx = rect.x + rect.width as i32 / 2;
        let cy = rect.y + 22;
        ui.painter.circle(cx, cy, 13, Color::WHITE);
        match item_action {
            ShutdownAction::TurnOff => {
                ui.painter
                    .fill_rect(Rect::new(cx - 1, cy - 10, 2, 9), *color);
                ui.painter.circle(cx, cy + 2, 5, *color);
            }
            ShutdownAction::Restart => {
                ui.painter.circle(cx, cy, 6, *color);
                let points = [
                    feos_gfx::Point::new(cx + 7, cy - 10),
                    feos_gfx::Point::new(cx + 12, cy - 6),
                    feos_gfx::Point::new(cx + 6, cy - 4),
                ];
                ui.painter.fill_polygon(&points, *color);
            }
            ShutdownAction::Cancel | ShutdownAction::None => {
                for offset in -5..=5 {
                    ui.painter.put_pixel(cx + offset, cy + offset, *color);
                    ui.painter.put_pixel(cx + offset, cy - offset, *color);
                }
            }
        }

        ui.painter.draw_text_ellipsis(
            Rect::new(rect.x + 2, rect.y + 42, rect.width - 4, 24),
            label,
            Color::WHITE,
            1,
            Align::Center,
            VerticalAlign::Middle,
        );

        if hit.clicked {
            action = *item_action;
        }
    }

    action
}

fn render_run_dialog<const N: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    work_area: Rect,
    state: &mut RunDialogState,
) -> bool {
    const WIDTH: u32 = 400;
    const HEIGHT: u32 = 168;
    const TITLE: u32 = 26;

    let x = work_area.x + (work_area.width as i32 - WIDTH as i32) / 2;
    let y = work_area.y + (work_area.height as i32 - HEIGHT as i32) / 2;
    let panel = Rect::new(x.max(work_area.x), y.max(work_area.y), WIDTH, HEIGHT);

    // Modal blocker: it is created after every other surface, so a press
    // outside the dialog cannot reach a window or the taskbar behind it.
    let _blocker = ui.interact(
        Id::from_name("desktop.run.modal"),
        ui.canvas_rect(),
        Sense::click(),
    );

    ui.painter.shadow(panel, 8, 10, 120);
    ui.painter.fill_rounded_rect(panel, 4, theme.menu_bg);
    ui.painter
        .stroke_rounded_rect(panel, 4, 1, theme.menu_border);

    let title = Rect::new(panel.x + 1, panel.y + 1, panel.width - 2, TITLE);
    ui.painter.fill_vertical_gradient(
        title,
        theme.title_active_top,
        theme.title_active_bottom,
    );
    ui.painter.draw_text_ellipsis(
        Rect::new(
            title.x + 8,
            title.y,
            title.width.saturating_sub(16),
            title.height,
        ),
        "Run",
        theme.title_text,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );

    let icon = Rect::new(panel.x + 16, panel.y + 38, 32, 32);
    feos_iconset::draw(&mut ui.painter, feos_iconset::Stock::Run, icon);

    ui.painter.draw_text_ellipsis(
        Rect::new(
            panel.x + 58,
            panel.y + 34,
            panel.width.saturating_sub(74),
            17,
        ),
        "Type the name of a program, folder, document or command,",
        theme.menu_text,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    ui.painter.draw_text_ellipsis(
        Rect::new(
            panel.x + 58,
            panel.y + 51,
            panel.width.saturating_sub(74),
            17,
        ),
        "and feos will open it for you.",
        theme.menu_text,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );

    ui.painter.draw_text_ellipsis(
        Rect::new(panel.x + 16, panel.y + 78, 42, 24),
        "Open:",
        theme.menu_text,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );

    let field = Rect::new(
        panel.x + 56,
        panel.y + 78,
        panel.width.saturating_sub(72),
        24,
    );
    ui.painter.fill_rect(field, theme.window_content);
    ui.painter.stroke_rect(field, 1, theme.menu_separator);
    if field.contains(ui.pointer()) {
        ui.set_cursor(CursorShape::Text);
    }
    let text_rect = Rect::new(
        field.x + 4,
        field.y + 1,
        field.width.saturating_sub(8),
        field.height.saturating_sub(2),
    );
    ui.painter.draw_text_ellipsis(
        text_rect,
        state.text.as_str(),
        theme.menu_text,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    let caret_x =
        (field.x + 5 + ui.painter.text_width(state.text.as_str(), 1)).min(field.right_i32() - 3);
    ui.painter
        .fill_rect(Rect::new(caret_x, field.y + 5, 1, 14), theme.menu_text);

    let ok = Rect::new(panel.right_i32() - 166, panel.bottom_i32() - 38, 70, 24);
    let cancel = Rect::new(panel.right_i32() - 88, panel.bottom_i32() - 38, 72, 24);
    let ok_hit = ui.interact(Id::from_name("desktop.run.ok"), ok, Sense::click());
    let cancel_hit = ui.interact(Id::from_name("desktop.run.cancel"), cancel, Sense::click());

    for (rect, hit, label, fill) in [
        (ok, ok_hit, "OK", theme.caption_button),
        (cancel, cancel_hit, "Cancel", theme.task_button),
    ] {
        ui.painter.fill_rounded_rect(
            rect,
            3,
            if hit.hovered {
                fill.lerp(Color::WHITE, 40)
            } else {
                fill
            },
        );
        ui.painter
            .stroke_rounded_rect(rect, 3, 1, theme.menu_border);
        ui.painter.draw_text_centered(rect, label, Color::WHITE, 1);
    }

    if cancel_hit.clicked {
        state.close();
    }
    ok_hit.clicked
}

fn render_menu<const N: usize, T: Copy>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    state: &mut ContextMenuState,
    work_area: Rect,
    pointer_pressed: bool,
    items: &[MenuItem<T>],
    name: &str,
) -> Option<T> {
    if !state.open {
        return None;
    }

    const WIDTH: u32 = 196;
    const ROW: u32 = 24;
    const PAD: u32 = 4;
    const SEP: u32 = 6;

    let separators = items.iter().filter(|item| item.separator_before).count() as u32;
    let height = PAD * 2 + ROW * items.len() as u32 + SEP * separators;
    let x = state
        .x
        .min(work_area.right_i32() - WIDTH as i32)
        .max(work_area.x);
    let y = state
        .y
        .min(work_area.bottom_i32() - height as i32)
        .max(work_area.y);
    let panel = Rect::new(x, y, WIDTH, height);

    // Consume a press outside the menu so it only dismisses the menu instead
    // of also activating the window or icon beneath it.
    let base = Id::from_name(name);
    let _blocker = ui.interact(base.child(0xffff), ui.canvas_rect(), Sense::click());

    if pointer_pressed && !panel.contains(ui.pointer()) {
        state.open = false;
        return None;
    }

    ui.painter.shadow(panel, 6, 8, 120);
    ui.painter.fill_rounded_rect(panel, 3, theme.menu_bg);
    ui.painter
        .stroke_rounded_rect(panel, 3, 1, theme.menu_border);

    let mut action = None;
    let mut row_y = panel.y + PAD as i32;
    for (index, item) in items.iter().enumerate() {
        if item.separator_before {
            ui.painter.fill_rect(
                Rect::new(
                    panel.x + 6,
                    row_y + (SEP / 2) as i32,
                    panel.width.saturating_sub(12),
                    1,
                ),
                theme.menu_separator,
            );
            row_y += SEP as i32;
        }

        let row = Rect::new(panel.x + 2, row_y, panel.width.saturating_sub(4), ROW);
        let id = base.child(index as u64 + 1);
        let hit = ui.interact(id, row, Sense::click());
        if hit.hovered {
            state.highlighted = index;
        }
        let lit = hit.hovered || state.highlighted == index;
        if lit {
            ui.painter.fill_rounded_rect(row, 2, theme.menu_hover);
        }
        ui.painter.draw_text_ellipsis(
            Rect::new(row.x + 26, row.y, row.width.saturating_sub(30), row.height),
            item.label,
            if lit {
                theme.menu_hover_text
            } else {
                theme.menu_text
            },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        if hit.clicked {
            action = Some(item.action);
            state.open = false;
        }
        row_y += ROW as i32;
    }

    action
}

fn render_shortcuts<const N: usize, const A: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    config: &DesktopConfig,
    catalog: &AppCatalog<A>,
    work_area: Rect,
    selection: usize,
    cached: bool,
) -> Option<usize> {
    if catalog.is_empty() || work_area.is_empty() {
        return None;
    }

    let mut launched = None;
    for index in 0..catalog.len().min(MAX_DESKTOP_ICONS) {
        let Some(cell) = shortcut_cell(config, work_area, index) else {
            break;
        };

        let id = Id::from_name("desktop.shortcut").child(index as u64 + 1);
        let hit = ui.interact(id, cell, Sense::click());
        let selected = index == selection;
        if hit.clicked {
            launched = Some(index);
        }
        // The cache already holds every icon in its plain state. Only a
        // highlighted one has to be drawn again, over clean wallpaper.
        if cached {
            if !selected && !hit.hovered {
                continue;
            }
            let clip = ui.painter.push_clip(cell);
            paint::draw_wallpaper_on(&mut ui.painter, theme, &config.wallpaper);
            ui.painter.restore_clip(clip);
        }
        if selected {
            ui.painter
                .fill_rounded_rect_alpha(cell, 4, theme.menu_hover_text, 55);
            ui.painter
                .stroke_rounded_rect(cell, 4, 1, theme.menu_hover_text);
        }
        if hit.hovered {
            ui.painter
                .fill_rounded_rect_alpha(cell, 4, theme.menu_hover, 70);
        }
        paint_shortcut(
            &mut ui.painter,
            theme,
            config,
            catalog,
            index,
            cell,
            hit.hovered,
        );
    }

    launched
}

/// What a window asked for while it was drawn.
#[derive(Clone, Copy, Default)]
struct WindowOutcome {
    close: bool,
    menu: Option<Point>,
}

#[allow(clippy::too_many_arguments)]
fn render_window<const UI: usize, const N: usize, const A: usize>(
    ui: &mut Ui<'_, '_, '_, UI>,
    theme: &DesktopTheme,
    windows: &mut WindowManager<N>,
    catalog: &AppCatalog<A>,
    index: usize,
    work_area: Rect,
    clicks: &mut ClickMemory,
    now_ms: u64,
    input: &Input,
    platform: Option<&Platform>,
    pointer_active: bool,
    keyboard_active: bool,
    paint_app: bool,
    scratch: Option<&mut [u32]>,
) -> WindowOutcome {
    let mut outcome = WindowOutcome::default();
    let Some(window) = windows.get(index).copied() else {
        return outcome;
    };
    if !window.open || window.state == WindowState::Minimized {
        return outcome;
    }

    let focused = windows.is_focused(index);
    let border = if focused {
        theme.window_border_active
    } else {
        theme.window_border_inactive
    };
    let rect = window.rect;
    if rect.width < 80 || rect.height < 40 {
        return outcome;
    }

    // Frame ---------------------------------------------------------------
    ui.painter.fill_rect(rect, border);
    ui.painter.fill_rect(
        Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2),
        theme.window_bg,
    );

    // Title bar -----------------------------------------------------------
    let title_height = window::TITLE_HEIGHT;
    let title = Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, title_height);
    let (title_top, title_bottom) = if focused {
        (theme.title_active_top, theme.title_active_bottom)
    } else {
        (theme.title_inactive_top, theme.title_inactive_bottom)
    };
    ui.painter
        .fill_vertical_gradient(title, title_top, title_bottom);
    ui.painter.fill_rect(
        Rect::new(title.x, title.bottom_i32() - 1, title.width, 1),
        border,
    );

    let title_drag = ui.interact(
        Id::from_name("desktop.window.title").child(index as u64 + 1),
        title,
        Sense::drag().with(Sense::CLICK),
    );
    // A held button keeps reporting `pressed`; only the first frame raises, or
    // holding the button down would count as a state change on every frame.
    if title_drag.pressed && !windows.is_focused(index) {
        windows.focus(index);
    }
    if ui.pointer_pressed() && title_drag.active {
        if let Some(window) = windows.get_mut(index) {
            window.title_moved = false;
        }
    }
    if title_drag.dragged {
        // Dragging a maximised caption restores the window first, then
        // continues moving the restored rectangle.
        if windows
            .get(index)
            .is_some_and(|window| window.state == WindowState::Maximized)
        {
            windows.toggle_maximize(index, work_area);
        }
        windows.move_by(
            index,
            title_drag.drag_delta.x,
            title_drag.drag_delta.y,
            work_area,
        );
        if let Some(window) = windows.get_mut(index) {
            window.title_moved = true;
        }
    }
    // Double-clicking a caption maximises or restores the window. The release
    // that ends a drag is not a click, so it cannot start or finish a pair.
    if title_drag.clicked {
        if windows.get(index).is_some_and(|window| window.title_moved) {
            clicks.forget();
        } else if clicks.hit(0x2000_0000 | index as u32, ui.pointer(), now_ms) {
            windows.toggle_maximize(index, work_area);
        }
    }
    if title_drag.secondary_clicked {
        outcome.menu = Some(ui.pointer());
    }
    // The icon opens the system menu.
    let icon_slot = Rect::new(title.x + 3, title.y + 3, 22, title_height.saturating_sub(6));
    let icon_hit = ui.interact(
        Id::from_name("desktop.window.icon").child(index as u64 + 1),
        icon_slot,
        Sense::click(),
    );
    if icon_hit.clicked {
        outcome.menu = Some(Point::new(rect.x + 4, rect.y + title_height as i32 + 2));
    }

    if let Some(app) = catalog.get(window.app) {
        let icon_size = (title_height - 8).min(18);
        paint::draw_icon(
            &mut ui.painter,
            &app.icon,
            Rect::new(title.x + 5, title.y + 5, icon_size, icon_size),
            theme.caption_button,
        );
    }
    ui.painter.draw_text_ellipsis(
        Rect::new(
            title.x + 28,
            title.y,
            title.width.saturating_sub(112),
            title_height,
        ),
        window.title.as_str(),
        if focused {
            theme.title_text
        } else {
            theme.title_text_inactive
        },
        1,
        Align::Start,
        VerticalAlign::Middle,
    );

    // Caption buttons -----------------------------------------------------
    let button_size = (title_height.saturating_sub(8)).min(20);
    let mut button_x = title.right_i32() - button_size as i32 - 5;
    let button_y = title.y + (title_height as i32 - button_size as i32) / 2;
    let close = Rect::new(button_x, button_y, button_size, button_size);
    button_x -= button_size as i32 + 2;
    let maximize = Rect::new(button_x, button_y, button_size, button_size);
    button_x -= button_size as i32 + 2;
    let minimize = Rect::new(button_x, button_y, button_size, button_size);

    let close_hit = ui.interact(
        Id::from_name("desktop.window.close").child(index as u64 + 1),
        close,
        Sense::click(),
    );
    let maximize_hit = ui.interact(
        Id::from_name("desktop.window.maximize").child(index as u64 + 1),
        maximize,
        Sense::click(),
    );
    let minimize_hit = ui.interact(
        Id::from_name("desktop.window.minimize").child(index as u64 + 1),
        minimize,
        Sense::click(),
    );

    draw_caption_button(
        ui,
        close,
        close_hit.hovered,
        theme.close_button,
        theme.caption_glyph,
        CaptionGlyph::Close,
    );
    draw_caption_button(
        ui,
        maximize,
        maximize_hit.hovered,
        theme.caption_button,
        theme.caption_glyph,
        if window.state == WindowState::Maximized {
            CaptionGlyph::Restore
        } else {
            CaptionGlyph::Maximize
        },
    );
    draw_caption_button(
        ui,
        minimize,
        minimize_hit.hovered,
        theme.caption_button,
        theme.caption_glyph,
        CaptionGlyph::Minimize,
    );

    if close_hit.clicked {
        outcome.close = true;
        return outcome;
    }
    if minimize_hit.clicked {
        windows.minimize(index);
        return outcome;
    }
    if maximize_hit.clicked {
        windows.toggle_maximize(index, work_area);
    }

    // Content -------------------------------------------------------------
    let content = window::content_rect(rect);
    let clip = ui.painter.push_clip(content);
    if paint_app {
        ui.painter.fill_rect(content, theme.window_bg);
    }

    let body_id = Id::from_name("desktop.window.body").child(index as u64 + 1);
    let body = ui.interact(body_id, content, Sense::click());
    if (body.clicked || body.pressed) && !windows.is_focused(index) {
        windows.focus(index);
    }

    let windowed_runtime = window.runtime.filter(|runtime| runtime.windowed());
    if let (Some(runtime), Some(platform)) = (windowed_runtime, platform) {
        if paint_app {
            let visible = content.clipped_to(ui.painter.canvas_rect());
            let width = content.width as usize;
            let height = content.height as usize;
            // The app lays itself out for its window, not for however much of
            // the window is on screen. When part of it is off-screen, give the
            // app a full-size surface and copy out what shows.
            match scratch.filter(|buffer| {
                visible != content && !content.is_empty() && buffer.len() >= width * height
            }) {
                Some(buffer) => {
                    if let Some(mut surface) = Canvas::new(buffer, width, height, width) {
                        surface.clear(theme.window_bg);
                        outcome.close = run_hosted_app(
                            runtime,
                            windows,
                            index,
                            &window,
                            &mut surface,
                            input,
                            platform,
                            now_ms,
                            Point::new(content.x, content.y),
                            &theme.ui,
                            pointer_active,
                            keyboard_active,
                        );
                        ui.painter.blit(
                            content.x,
                            content.y,
                            content.width,
                            content.height,
                            surface.pixels(),
                        );
                    }
                }
                None => {
                    if let Some(mut surface) = ui.painter.canvas_mut().sub_canvas(visible) {
                        outcome.close = run_hosted_app(
                            runtime,
                            windows,
                            index,
                            &window,
                            &mut surface,
                            input,
                            platform,
                            now_ms,
                            Point::new(visible.x, visible.y),
                            &theme.ui,
                            pointer_active,
                            keyboard_active,
                        );
                    }
                }
            }
        }
    } else if let Some(app) = catalog.get(window.app) {
        if content.width > 40 && content.height > 40 {
            let panel = Rect::new(
                content.x + 12,
                content.y + 12,
                content.width.saturating_sub(24),
                content.height.saturating_sub(24),
            );
            ui.painter.fill_rect(panel, theme.window_content);
            ui.painter.stroke_rect(panel, 1, theme.menu_separator);

            let icon_size = 56u32;
            let icon_rect = Rect::new(panel.x + 14, panel.y + 14, icon_size, icon_size);
            paint::draw_icon(&mut ui.painter, &app.icon, icon_rect, theme.caption_button);

            let text_x = panel.x + icon_size as i32 + 24;
            let text_width = (panel.right_i32() - text_x - 14).max(20) as u32;
            ui.painter.draw_text_ellipsis(
                Rect::new(text_x, panel.y + 12, text_width, 24),
                app.name(),
                theme.menu_text,
                2,
                Align::Start,
                VerticalAlign::Middle,
            );
            ui.painter.draw_text_ellipsis(
                Rect::new(text_x, panel.y + 38, text_width, 18),
                app.manifest.description.as_str(),
                theme.menu_muted,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
            ui.painter.draw_text_ellipsis(
                Rect::new(text_x, panel.y + 58, text_width, 18),
                app.manifest.category.as_str(),
                theme.menu_muted,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );

            let details_y = panel.y + icon_size as i32 + 28;
            ui.painter.fill_rect(
                Rect::new(panel.x + 14, details_y, panel.width.saturating_sub(28), 1),
                theme.menu_separator,
            );
            ui.painter.draw_text_ellipsis(
                Rect::new(panel.x + 14, details_y + 8, 96, 18),
                "Entry point",
                theme.menu_muted,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
            ui.painter.draw_text_ellipsis(
                Rect::new(
                    panel.x + 116,
                    details_y + 8,
                    panel.width.saturating_sub(130),
                    18,
                ),
                app.manifest.entry.as_str(),
                theme.menu_text,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
            ui.painter.draw_text_ellipsis(
                Rect::new(panel.x + 14, details_y + 28, 96, 18),
                "Version",
                theme.menu_muted,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
            ui.painter.draw_text_ellipsis(
                Rect::new(
                    panel.x + 116,
                    details_y + 28,
                    panel.width.saturating_sub(130),
                    18,
                ),
                app.manifest.version.as_str(),
                theme.menu_text,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );

            let status = match window.status {
                WindowStatus::UnknownEntry => "This entry point is not in the shell registry",
                WindowStatus::SpawnFailed => "The kernel could not start a process for this app",
                WindowStatus::OpenFailed => "The app could not open its window",
                WindowStatus::Ready if window.launched => {
                    "Running in the background; its output goes to the serial console"
                }
                WindowStatus::Ready => "Not running - choose the app again to start it",
            };
            let status_color = if window.launched {
                theme.task_button
            } else if window.status != WindowStatus::Ready {
                theme.close_button
            } else {
                theme.menu_muted
            };
            ui.painter.draw_text_ellipsis(
                Rect::new(
                    panel.x + 14,
                    panel.bottom_i32() - 26,
                    panel.width.saturating_sub(28),
                    18,
                ),
                status,
                status_color,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
        }
    } else {
        ui.painter.draw_text_ellipsis(
            Rect::new(
                content.x + 12,
                content.y + 12,
                content.width.saturating_sub(24),
                20,
            ),
            "Application not found in the catalog",
            theme.menu_muted,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }

    ui.painter.restore_clip(clip);

    // Resize handles ------------------------------------------------------
    // These are registered last so they take priority over the title bar and
    // content interactions underneath them.
    resize_edges(ui, windows, index, rect, work_area);

    // Resize grip ---------------------------------------------------------
    if window.state == WindowState::Normal && rect.width > 140 && rect.height > 100 {
        let grip = Rect::new(rect.right_i32() - 14, rect.bottom_i32() - 14, 14, 14);
        let grip_color = if ui.pointer_over(grip) {
            theme.menu_text
        } else {
            theme.menu_muted
        };
        for step in 0..3 {
            let offset = step * 4;
            ui.painter.fill_rect(
                Rect::new(grip.right_i32() - 4 - offset, grip.bottom_i32() - 3, 3, 1),
                grip_color,
            );
            ui.painter.fill_rect(
                Rect::new(grip.right_i32() - 3, grip.bottom_i32() - 4 - offset, 1, 3),
                grip_color,
            );
        }
    }

    outcome
}

/// Lets a hosted app clean up. Its window's task does that too, a new one if the old one
/// died, so that an app whose state is ruined can't take the desktop with it.
fn close_hosted(runtime: &'static dyn App, index: usize) {
    match feos_app::platform() {
        Some(platform) => {
            let closed = isolate::call(platform, index, runtime.name(), || runtime.close_instance(index));
            if let Err(died) = closed {
                feos_console::println!(
                    "[desktop] {} failed while closing, with status {}",
                    runtime.name(),
                    died.status
                );
            }
            isolate::finish(index);
        }
        None => runtime.close_instance(index),
    }
}

/// A hosted app's task died in a call: its window closes, and that's all that happens.
fn hosted_failed<const N: usize>(
    runtime: &'static dyn App,
    windows: &mut WindowManager<N>,
    index: usize,
    died: isolate::Died,
) -> bool {
    feos_console::println!(
        "[desktop] {} failed with status {}; closing its window",
        runtime.name(),
        died.status
    );
    if let Some(slot) = windows.get_mut(index) {
        slot.failed = Some(died.status);
    }
    true
}

/// How many times log off asks for a closed window's process to stop.
const LOG_OFF_KILL_TRIES: usize = 50;

/// Starts a hosted app if needed and runs one frame of it. Returns `true` when
/// the app asked to close.
#[allow(clippy::too_many_arguments)]
fn run_hosted_app<const N: usize>(
    runtime: &'static dyn App,
    windows: &mut WindowManager<N>,
    index: usize,
    window: &DesktopWindow,
    canvas: &mut Canvas<'_>,
    input: &Input,
    platform: &Platform,
    now_ms: u64,
    origin: Point,
    theme: &feos_gui::Theme,
    pointer_active: bool,
    keyboard_active: bool,
) -> bool {
    let mut context = WindowContext {
        args: window.args.as_str(),
        instance: index,
        canvas,
        input,
        platform,
        now_ms,
        origin,
        theme,
        pointer_active,
        keyboard_active,
        cursor: feos_gui::CursorShape::Arrow,
    };
    let mut started = window.app_started;
    if !started {
        started = match isolate::call(platform, index, runtime.name(), || runtime.open_window(&context)) {
            Ok(started) => started,
            Err(died) => return hosted_failed(runtime, windows, index, died),
        };
        if let Some(slot) = windows.get_mut(index) {
            slot.app_started = started;
            slot.status = if started {
                WindowStatus::Ready
            } else {
                WindowStatus::OpenFailed
            };
        }
    }
    if !started {
        return false;
    }
    if let Some(slot) = windows.get_mut(index) {
        slot.last_frame_ms = now_ms;
    }
    let clock = platform.clock;
    let started_at = clock.ticks();
    let closed = match isolate::call(platform, index, runtime.name(), || runtime.frame_window(&mut context)) {
        Ok(frame) => frame == WindowFrame::Close,
        Err(died) => return hosted_failed(runtime, windows, index, died),
    };
    if let Some(slot) = windows.get_mut(index) {
        slot.cursor = if pointer_active {
            context.cursor
        } else {
            feos_gui::CursorShape::Arrow
        };
        let elapsed = clock.ticks().wrapping_sub(started_at) as u128;
        let micros = elapsed * 1_000_000 / clock.frequency().max(1) as u128;
        slot.cpu_us = slot.cpu_us.saturating_add(micros as u64);
    }
    closed
}

/// Registers the eight invisible resize grips around a normal window.
///
/// XP lets a window be resized from any side or corner, not just the lower
/// right grip. The grips are thin so they do not steal normal title-bar or
/// content clicks; the corners are larger, which makes diagonal resizing easy.
fn resize_edges<const UI: usize, const N: usize>(
    ui: &mut Ui<'_, '_, '_, UI>,
    windows: &mut WindowManager<N>,
    index: usize,
    rect: Rect,
    work_area: Rect,
) {
    let resizable = windows
        .get(index)
        .is_some_and(|window| window.open && window.state == WindowState::Normal);
    if !resizable {
        return;
    }

    let corner = RESIZE_CORNER as u32;
    let edge = RESIZE_EDGE as u32;
    if rect.width <= corner * 2 || rect.height <= corner * 2 {
        return;
    }

    let right = rect.right_i32();
    let bottom = rect.bottom_i32();
    let handles = [
        (
            ResizeEdge::TopLeft,
            Rect::new(rect.x, rect.y, corner, corner),
        ),
        (ResizeEdge::TopRight, top_right_grip(rect, ui.pointer())),
        (
            ResizeEdge::BottomLeft,
            Rect::new(rect.x, bottom - RESIZE_CORNER, corner, corner),
        ),
        (
            ResizeEdge::BottomRight,
            Rect::new(
                right - RESIZE_CORNER,
                bottom - RESIZE_CORNER,
                corner,
                corner,
            ),
        ),
        (
            ResizeEdge::Left,
            Rect::new(
                rect.x,
                rect.y + RESIZE_CORNER,
                edge,
                rect.height - corner * 2,
            ),
        ),
        (
            ResizeEdge::Right,
            Rect::new(
                right - RESIZE_EDGE,
                rect.y + RESIZE_CORNER,
                edge,
                rect.height - corner * 2,
            ),
        ),
        (
            ResizeEdge::Top,
            Rect::new(
                rect.x + RESIZE_CORNER,
                rect.y,
                rect.width - corner * 2,
                edge,
            ),
        ),
        (
            ResizeEdge::Bottom,
            Rect::new(
                rect.x + RESIZE_CORNER,
                bottom - RESIZE_EDGE,
                rect.width - corner * 2,
                edge,
            ),
        ),
    ];

    for (edge, area) in handles {
        let id = Id::from_name("desktop.window.resize")
            .child(index as u64 + 1)
            .child(edge as u64 + 1);
        let response = ui.interact(id, area, Sense::drag());
        if response.pressed && !windows.is_focused(index) {
            windows.focus(index);
        }
        if response.dragged {
            windows.resize_from(
                index,
                edge,
                response.drag_delta.x,
                response.drag_delta.y,
                work_area,
            );
        }
    }
}

/// The top-right resize grip for a pointer at `point`.
///
/// The close button sits in this corner, so unlike the others the grip is only
/// the thin L along the outside edges. It takes whichever arm is nearer, since
/// one id can only be registered once per frame.
fn top_right_grip(rect: Rect, point: Point) -> Rect {
    let right = rect.right_i32();
    if point.x >= right - RESIZE_EDGE {
        Rect::new(
            right - RESIZE_EDGE,
            rect.y,
            RESIZE_EDGE as u32,
            RESIZE_CORNER as u32,
        )
    } else {
        Rect::new(
            right - RESIZE_CORNER,
            rect.y,
            RESIZE_CORNER as u32,
            RESIZE_EDGE as u32,
        )
    }
}

/// The resize handle under `point`, matching the regions registered by
/// [`resize_edges`].
fn resize_edge_at(rect: Rect, point: Point) -> Option<ResizeEdge> {
    if !rect.contains(point) {
        return None;
    }
    let corner = RESIZE_CORNER;
    if rect.width <= (corner * 2) as u32 || rect.height <= (corner * 2) as u32 {
        return None;
    }

    let left = point.x < rect.x + corner;
    let right = point.x >= rect.right_i32() - corner;
    let top = point.y < rect.y + corner;
    let bottom = point.y >= rect.bottom_i32() - corner;

    if left && top {
        Some(ResizeEdge::TopLeft)
    } else if right && top {
        top_right_grip(rect, point)
            .contains(point)
            .then_some(ResizeEdge::TopRight)
    } else if left && bottom {
        Some(ResizeEdge::BottomLeft)
    } else if right && bottom {
        Some(ResizeEdge::BottomRight)
    } else if point.x < rect.x + RESIZE_EDGE {
        Some(ResizeEdge::Left)
    } else if point.x >= rect.right_i32() - RESIZE_EDGE {
        Some(ResizeEdge::Right)
    } else if point.y < rect.y + RESIZE_EDGE {
        Some(ResizeEdge::Top)
    } else if point.y >= rect.bottom_i32() - RESIZE_EDGE {
        Some(ResizeEdge::Bottom)
    } else {
        None
    }
}

/// Picks the cursor for the window edge under `pointer`.
fn window_cursor_shape<const N: usize>(
    windows: &WindowManager<N>,
    pointer: Point,
) -> paint::CursorShape {
    let Some(index) = windows.topmost_at(pointer) else {
        return paint::CursorShape::Arrow;
    };
    let Some(window) = windows.get(index) else {
        return paint::CursorShape::Arrow;
    };
    if !window.open || window.state == WindowState::Minimized {
        return paint::CursorShape::Arrow;
    }
    // Over its content, a hosted app picks the pointer (a text beam over a
    // text field, say).
    let content_cursor = if window::content_rect(window.rect).contains(pointer) {
        window.cursor
    } else {
        paint::CursorShape::Arrow
    };
    if window.state != WindowState::Normal {
        return content_cursor;
    }

    match resize_edge_at(window.rect, pointer) {
        Some(ResizeEdge::Left | ResizeEdge::Right) => paint::CursorShape::SizeHorizontal,
        Some(ResizeEdge::Top | ResizeEdge::Bottom) => paint::CursorShape::SizeVertical,
        Some(ResizeEdge::TopLeft | ResizeEdge::BottomRight) => paint::CursorShape::SizeNwse,
        Some(ResizeEdge::TopRight | ResizeEdge::BottomLeft) => paint::CursorShape::SizeNesw,
        None => content_cursor,
    }
}

fn draw_shortcut_label(
    painter: &mut feos_gui::Painter<'_, '_>,
    rect: Rect,
    text: &str,
    color: Color,
) {
    let scale = 1;
    let mut lines = [Text::<64>::EMPTY; 2];
    let mut count = 0usize;
    if rect.width == 0 || rect.height == 0 {
        return;
    }

    for word in text.split(' ') {
        if count == 2 {
            break;
        }
        let mut candidate = lines[count];
        if !candidate.is_empty() {
            candidate.push(' ');
        }
        candidate.push_str(word);
        if painter.text_width(candidate.as_str(), scale) <= rect.width as i32
            || lines[count].is_empty()
        {
            lines[count] = candidate;
        } else {
            count += 1;
            if count < 2 {
                lines[count] = Text::from_str(word);
            }
        }
    }
    if !lines[1].is_empty() {
        count = 2;
    } else if !lines[0].is_empty() {
        count = 1;
    }

    let line_height = painter.line_height(scale);
    let top = rect.y + (rect.height as i32 - line_height * count as i32).max(0) / 2;
    for (index, line) in lines[..count].iter().enumerate() {
        painter.draw_text_ellipsis(
            Rect::new(
                rect.x,
                top + line_height * index as i32,
                rect.width,
                line_height as u32,
            ),
            line.as_str(),
            color,
            scale,
            Align::Center,
            VerticalAlign::Middle,
        );
    }
}

#[derive(Clone, Copy)]
enum CaptionGlyph {
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn draw_caption_button<const N: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    rect: Rect,
    hovered: bool,
    fill: Color,
    glyph: Color,
    kind: CaptionGlyph,
) {
    let background = if hovered {
        fill.lerp(Color::WHITE, 55)
    } else {
        fill
    };
    ui.painter.fill_rounded_rect(rect, 3, background);
    ui.painter
        .stroke_rounded_rect(rect, 3, 1, background.lerp(Color::BLACK, 40));
    let cx = rect.x + rect.width as i32 / 2;
    let cy = rect.y + rect.height as i32 / 2;
    match kind {
        CaptionGlyph::Minimize => {
            ui.painter.fill_rect(Rect::new(cx - 4, cy + 3, 8, 2), glyph);
        }
        CaptionGlyph::Maximize => {
            ui.painter
                .stroke_rect(Rect::new(cx - 4, cy - 4, 9, 8), 1, glyph);
        }
        CaptionGlyph::Restore => {
            // Two overlapping frames, like the XP "restore down" button.
            ui.painter
                .stroke_rect(Rect::new(cx - 4, cy - 2, 8, 7), 1, glyph);
            ui.painter
                .stroke_rect(Rect::new(cx - 1, cy - 5, 6, 6), 1, glyph);
        }
        CaptionGlyph::Close => {
            for offset in -4..=4 {
                ui.painter.put_pixel(cx + offset, cy + offset, glyph);
                ui.painter.put_pixel(cx + offset, cy - offset, glyph);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feos_app::{BoardInfo, Context, ProcessInfo, ProcessState};
    use feos_gui::{Event, Key, Modifiers, MouseButton};
    use feos_hal::{Clock, Cpu, Power};
    use feos_sync::SpinLock;

    struct FakeClock;

    impl Clock for FakeClock {
        fn frequency(&self) -> u64 {
            1_000_000
        }

        fn ticks(&self) -> u64 {
            0
        }

        fn spin_ms(&self, _ms: u64) {}
    }

    static CLOCK: FakeClock = FakeClock;

    struct FakePower;

    impl Power for FakePower {
        fn system_off(&self) -> ! {
            loop {}
        }

        fn system_reset(&self) -> ! {
            loop {}
        }
    }

    static POWER: FakePower = FakePower;

    struct FakeCpu;

    impl Cpu for FakeCpu {
        fn current_el(&self) -> u64 {
            1
        }

        fn midr(&self) -> u64 {
            0
        }

        fn halt(&self) -> ! {
            loop {}
        }
    }

    static CPU: FakeCpu = FakeCpu;

    struct Dummy;

    impl App for Dummy {
        fn name(&self) -> &'static str {
            "dummy"
        }

        fn usage(&self) -> &'static str {
            "dummy"
        }

        fn description(&self) -> &'static str {
            "test app"
        }

        fn run(&self, _context: &mut Context<'_>) -> i32 {
        0
    }
    }

    static DUMMY: Dummy = Dummy;

    struct WindowedDummy;

    impl App for WindowedDummy {
        fn name(&self) -> &'static str {
            "windowed"
        }

        fn usage(&self) -> &'static str {
            "windowed"
        }

        fn description(&self) -> &'static str {
            "windowed test app"
        }

        fn windowed(&self) -> bool {
            true
        }

        fn animates(&self) -> bool {
            true
        }

        fn run(&self, _context: &mut Context<'_>) -> i32 {
        0
    }
    }

    static WINDOWED: WindowedDummy = WindowedDummy;

    struct Recorder;

    static RECORDED: SpinLock<(bool, usize)> = SpinLock::new((false, 0));

    impl App for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }

        fn usage(&self) -> &'static str {
            "recorder"
        }

        fn description(&self) -> &'static str {
            "records keyboard input"
        }

        fn windowed(&self) -> bool {
            true
        }

        fn open_window(&self, _context: &WindowContext<'_, '_>) -> bool {
            true
        }

        fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
            *RECORDED.lock() = (context.keyboard_active, context.input.text().count());
            WindowFrame::Continue
        }

        fn run(&self, _context: &mut Context<'_>) -> i32 {
        0
    }
    }

    static RECORDER: Recorder = Recorder;

    struct Registry;

    impl AppRegistry for Registry {
        fn find(&self, name: &str) -> Option<&'static dyn App> {
            match name {
                "dummy" => Some(&DUMMY),
                "windowed" => Some(&WINDOWED),
                "recorder" => Some(&RECORDER),
                _ => None,
            }
        }
    }

    #[derive(Default)]
    struct FakeState {
        next_pid: u32,
        spawned: usize,
    }

    struct FakeProcesses {
        state: SpinLock<FakeState>,
    }

    impl FakeProcesses {
        fn new() -> Self {
            Self {
                state: SpinLock::new(FakeState {
                    next_pid: 2,
                    spawned: 0,
                }),
            }
        }
    }

    static TEST_PROCESSES: FakeProcesses = FakeProcesses {
        state: SpinLock::new(FakeState {
            next_pid: 2,
            spawned: 0,
        }),
    };

    impl Processes for FakeProcesses {
        fn spawn(&self, _app: &'static dyn App, _args: &str) -> Result<u32, SpawnError> {
            let mut state = self.state.lock();
            let pid = state.next_pid;
            state.next_pid += 1;
            state.spawned += 1;
            Ok(pid)
        }

        fn kill(&self, _pid: u32) -> bool {
            false
        }

        fn current(&self) -> u32 {
            1
        }

        fn snapshot(&self, out: &mut [ProcessInfo]) -> usize {
            if out.is_empty() {
                0
            } else {
                out[0] = ProcessInfo {
                    pid: 1,
                    state: ProcessState::Running,
                    ..ProcessInfo::EMPTY
                };
                1
            }
        }
    }

    fn catalog_bundle(id: &str, entry: &str) -> std::vec::Vec<u8> {
        use feos_archive::ZipWriter;
        let manifest =
            std::format!("id: {id}\nname: {id}\nentry: {entry}\ndescription: test app\n");
        let mut out = [0u8; 8192];
        let mut writer = ZipWriter::new(&mut out);
        writer
            .write_stored("manifest.yaml", manifest.as_bytes())
            .unwrap();
        let len = writer.finish().unwrap();
        out[..len].to_vec()
    }

    #[test]
    fn scans_an_application_folder() {
        let bytes = catalog_bundle("terminal", "dummy");
        let entries = [DirEntry::new("terminal.fapp", &bytes)];
        let mut desktop = Desktop::<4, 4, 2>::default();
        let report = desktop.scan(&entries);
        assert_eq!(report.installed, 1);
        assert_eq!(desktop.catalog().len(), 1);
        assert!(desktop.app("terminal").is_some());
    }

    #[test]
    fn launches_catalog_entries_into_windows() {
        let bytes = catalog_bundle("terminal", "dummy");
        let entries = [DirEntry::new("terminal.fapp", &bytes)];
        let mut desktop = Desktop::<4, 4, 2>::default();
        desktop.scan(&entries);
        desktop.work_area = Rect::new(0, 0, 800, 600);

        let processes = FakeProcesses::new();
        assert_eq!(
            desktop.launch(0, &Registry, &processes),
            LaunchOutcome::Spawned { window: 0, pid: 2 }
        );
        assert_eq!(desktop.windows().count(), 1);
        assert!(desktop.windows().get(0).unwrap().launched);

        assert_eq!(
            desktop.launch(0, &Registry, &processes),
            LaunchOutcome::AlreadyRunning {
                window: 0,
                pid: Some(2)
            }
        );
        assert_eq!(
            desktop.launch(99, &Registry, &processes),
            LaunchOutcome::UnknownApp
        );

        // The fake snapshot never reports pid 2, so the supervisor pass must
        // notice the exit and clear the window's running state.
        assert_eq!(desktop.tick_session(&processes), 1);
        let window = desktop.windows().get(0).unwrap();
        assert!(!window.launched);
        assert_eq!(window.pid, None);
    }

    #[test]
    fn windowed_apps_open_in_managed_windows() {
        let bytes = catalog_bundle("win", "windowed");
        let entries = [DirEntry::new("win.fapp", &bytes)];
        let mut desktop = Desktop::<4, 4, 2>::default();
        desktop.scan(&entries);
        desktop.work_area = Rect::new(0, 0, 800, 560);
        let processes = FakeProcesses::new();

        let outcome = desktop.launch(0, &Registry, &processes);
        assert!(matches!(outcome, LaunchOutcome::Windowed { window: 0 }));
        assert!(desktop.windows().get(0).unwrap().runtime.is_some());
        assert_eq!(processes.state.lock().spawned, 0);

        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();
        let response = desktop.render(&mut canvas, &Input::new(), 100);
        assert!(response.redraw);
        assert_eq!(response.animation_period_ms, 33);
        assert_eq!(response.window_count, 1);

        desktop.windows_mut().minimize(0);
        let response = desktop.render(&mut canvas, &Input::new(), 200);
        assert!(!response.redraw);
    }

    #[test]
    fn windowed_apps_receive_keyboard_events() {
        let bytes = catalog_bundle("rec", "recorder");
        let entries = [DirEntry::new("rec.fapp", &bytes)];
        let mut desktop = Desktop::<4, 4, 2>::default();
        desktop.scan(&entries);
        desktop.work_area = Rect::new(0, 0, 800, 560);

        let outcome = desktop.launch(0, &Registry, &TEST_PROCESSES);
        assert!(matches!(outcome, LaunchOutcome::Windowed { window: 0 }));

        let platform = Platform {
            board: BoardInfo {
                name: "test",
                arch: "test",
                console_base: 0,
                framebuffer_addr: 0,
                kernel_range: (0, 0),
                bss_range: (0, 0),
            },
            clock: &CLOCK,
            power: &POWER,
            cpu: &CPU,
            audio: None,
            pointer: None,
            keyboard: None,
            processes: &TEST_PROCESSES,
            memory: None,
            apps: &Registry,
            filesystem: None,
            net: None,
        };

        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();
        desktop.render_with_platform(&mut canvas, &Input::new(), 100, Some(&platform));

        let mut input = Input::new();
        input.push(Event::Text('a'));
        input.push(Event::Text('b'));
        desktop.render_with_platform(&mut canvas, &input, 200, Some(&platform));

        let (keyboard_active, text_count) = *RECORDED.lock();
        assert!(
            keyboard_active,
            "focused windowed app should own the keyboard"
        );
        assert_eq!(text_count, 2);
    }

    #[test]
    fn unknown_entry_keeps_the_window_open() {
        let bytes = catalog_bundle("broken", "nope");
        let entries = [DirEntry::new("broken.fapp", &bytes)];
        let mut desktop = Desktop::<4, 4, 2>::default();
        desktop.scan(&entries);
        desktop.work_area = Rect::new(0, 0, 800, 600);
        let processes = FakeProcesses::new();
        assert_eq!(
            desktop.launch(0, &Registry, &processes),
            LaunchOutcome::UnknownEntry
        );
        assert_eq!(desktop.windows().count(), 1);
    }

    #[test]
    fn starts_autostart_services_from_the_init_section() {
        let config = DesktopConfig::parse(concat!(
            "init:\n",
            "  restart: always\n",
            "  autostart: [terminal, ghost]\n",
        ))
        .unwrap();
        let bytes = catalog_bundle("terminal", "dummy");
        let entries = [DirEntry::new("terminal.fapp", &bytes)];
        let mut desktop = Desktop::<4, 4, 4>::new(config);
        desktop.scan(&entries);
        let processes = FakeProcesses::new();
        assert_eq!(desktop.start_session(&Registry, &processes), 1);
        assert!(desktop.session_started());

        let mut statuses = [feos_init::ServiceStatus::EMPTY; 4];
        let count = desktop.services(&mut statuses);
        assert_eq!(count, 2);
        assert_eq!(statuses[0].id.as_str(), "terminal");
        assert_eq!(statuses[0].state, feos_init::ServiceState::Running);
        assert_eq!(statuses[1].state, feos_init::ServiceState::Missing);
    }

    #[test]
    fn registers_and_starts_services_on_demand() {
        let bytes = catalog_bundle("terminal", "dummy");
        let entries = [DirEntry::new("terminal.fapp", &bytes)];
        let mut desktop = Desktop::<4, 4, 4>::default();
        desktop.scan(&entries);
        let processes = FakeProcesses::new();

        assert!(desktop.register_service("terminal", &Registry));
        assert_eq!(desktop.start_service("terminal", &processes), Ok(2));
        assert_eq!(
            desktop.init().status_of("terminal").unwrap().state,
            feos_init::ServiceState::Running
        );
        desktop.stop_service("terminal", &processes);
        assert_eq!(
            desktop.init().status_of("terminal").unwrap().state,
            feos_init::ServiceState::Stopped
        );
    }

    #[test]
    fn renders_without_a_theme_or_panic() {
        let config = DesktopConfig::parse(concat!(
            "theme: xp-blue\n",
            "start_menu:\n",
            "  initially_open: true\n",
        ))
        .unwrap();
        let bytes = catalog_bundle("terminal", "dummy");
        let entries = [DirEntry::new("terminal.fapp", &bytes)];
        let mut desktop = Desktop::<4, 4, 2>::new(config);
        desktop.scan(&entries);
        desktop.work_area = Rect::new(0, 0, 800, 560);

        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();
        let input = Input::new();
        let response = desktop.render(&mut canvas, &input, 3_723_000);
        assert!(response.start_open);
        assert!(response.is_empty());
        assert!(pixels.iter().any(|&pixel| pixel != 0));
    }

    #[test]
    fn keyboard_moves_start_menu_and_enter_launches() {
        let config = DesktopConfig::parse("start_menu:\n  initially_open: true\n").unwrap();
        let names = ["a.fapp", "b.fapp", "c.fapp"];
        let bundles: std::vec::Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|id| catalog_bundle(id, "dummy"))
            .collect();
        let entries: std::vec::Vec<_> = names
            .iter()
            .zip(bundles.iter())
            .map(|(name, data)| DirEntry::new(*name, data))
            .collect();
        let mut desktop = Desktop::<4, 4, 2>::new(config);
        desktop.scan(&entries);
        desktop.work_area = Rect::new(0, 0, 800, 560);

        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();

        let mut input = Input::new();
        input.push(Event::KeyDown(Key::Down, Modifiers::NONE));
        let response = desktop.render(&mut canvas, &input, 0);
        assert!(response.start_open);
        assert_eq!(desktop.start_menu().highlighted, 1);

        let mut input = Input::new();
        input.push(Event::KeyDown(Key::Enter, Modifiers::NONE));
        let response = desktop.render(&mut canvas, &input, 16);
        assert_eq!(response.launch.map(|request| request.app), Some(1));
        assert!(!response.start_open);
    }

    #[test]
    fn space_opens_the_menu_and_enter_launches_the_wallpaper_selection() {
        let names = ["a.fapp", "b.fapp", "c.fapp"];
        let bundles: std::vec::Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|id| catalog_bundle(id, "dummy"))
            .collect();
        let entries: std::vec::Vec<_> = names
            .iter()
            .zip(bundles.iter())
            .map(|(name, data)| DirEntry::new(*name, data))
            .collect();
        let mut desktop = Desktop::<4, 4, 2>::default();
        desktop.scan(&entries);
        desktop.work_area = Rect::new(0, 0, 800, 560);

        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();

        // With the menu closed, Down moves the wallpaper selection.
        let mut input = Input::new();
        input.push(Event::KeyDown(Key::Down, Modifiers::NONE));
        let _ = desktop.render(&mut canvas, &input, 0);
        assert_eq!(desktop.selection(), 1);

        let mut input = Input::new();
        input.push(Event::KeyDown(Key::Enter, Modifiers::NONE));
        let response = desktop.render(&mut canvas, &input, 16);
        assert_eq!(response.launch.map(|request| request.app), Some(1));

        // Space opens the Start menu.
        let mut input = Input::new();
        input.push(Event::KeyDown(Key::Space, Modifiers::NONE));
        let response = desktop.render(&mut canvas, &input, 32);
        assert!(response.start_open);
    }

    #[test]
    fn configuration_changes_theme_in_place() {
        let mut desktop = Desktop::<4, 4, 2>::default();
        assert_eq!(desktop.config().theme, ThemeName::XpBlue);
        let config = DesktopConfig::parse("theme: dark\n").unwrap();
        desktop.set_config(config);
        assert_eq!(desktop.theme().name, ThemeName::Dark);
    }

    fn click_at(x: i32, y: i32) -> Input {
        let mut input = Input::new();
        input.push(Event::PointerDown {
            pos: feos_gfx::Point::new(x, y),
            button: MouseButton::Left,
        });
        input.push(Event::PointerUp {
            pos: feos_gfx::Point::new(x, y),
            button: MouseButton::Left,
        });
        input
    }

    #[test]
    fn clicking_start_opens_and_closes_the_menu() {
        let config = DesktopConfig::parse("start_menu:\n  initially_open: false\n").unwrap();
        let mut desktop = Desktop::<4, 4, 2>::new(config);
        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();

        let response = desktop.render(&mut canvas, &click_at(30, 745), 1000);
        assert!(response.start_open);
        assert!(desktop.start_menu().open);

        let response = desktop.render(&mut canvas, &click_at(30, 745), 2000);
        assert!(!response.start_open);
    }

    #[test]
    fn clicking_the_desktop_closes_the_start_menu() {
        let config = DesktopConfig::parse("start_menu:\n  initially_open: true\n").unwrap();
        let mut desktop = Desktop::<4, 4, 2>::new(config);
        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();

        let response = desktop.render(&mut canvas, &click_at(700, 400), 1000);
        assert!(!response.start_open);
        assert!(!desktop.start_menu().open);
    }

    #[test]
    fn clicking_all_programs_opens_the_list() {
        let config = DesktopConfig::parse("start_menu:\n  initially_open: true\n").unwrap();
        let mut desktop = Desktop::<4, 4, 2>::new(config);
        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();

        desktop.render(&mut canvas, &click_at(120, 656), 1000);
        assert!(desktop.start_menu().all_programs);
    }

    #[test]
    fn turn_off_asks_for_confirmation() {
        let config = DesktopConfig::parse("start_menu:\n  initially_open: true\n").unwrap();
        let mut desktop = Desktop::<4, 4, 2>::new(config);
        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();

        let response = desktop.render(&mut canvas, &click_at(389, 705), 1000);
        assert!(desktop.shutdown.open);
        assert_eq!(response.command, StartMenuCommand::None);

        let response = desktop.render(&mut canvas, &click_at(376, 423), 1400);
        assert!(!desktop.shutdown.open);
        assert_eq!(response.command, StartMenuCommand::ShutDown);
    }

    #[test]
    fn log_off_asks_for_confirmation() {
        let config = DesktopConfig::parse("start_menu:\n  initially_open: true\n").unwrap();
        let mut desktop = Desktop::<4, 4, 2>::new(config);
        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();

        let response = desktop.render(&mut canvas, &click_at(283, 705), 1000);
        assert!(desktop.logoff_open);
        assert_eq!(response.command, StartMenuCommand::None);

        let response = desktop.render(&mut canvas, &click_at(560, 417), 1400);
        assert!(!desktop.logoff_open);
        assert_eq!(response.command, StartMenuCommand::LogOff);
    }

    #[test]
    fn clicking_the_clock_opens_date_and_time() {
        let mut desktop =
            Desktop::<4, 4, 2>::new(DesktopConfig::parse("theme: xp-blue\n").unwrap());
        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();

        desktop.render(&mut canvas, &click_at(980, 746), 1000);
        assert!(desktop.clock_open);

        let mut input = Input::new();
        input.push(Event::KeyDown(Key::Escape, Modifiers::NONE));
        desktop.render(&mut canvas, &input, 1200);
        assert!(!desktop.clock_open);
    }

    #[test]
    fn show_desktop_toggles_windows() {
        let bytes = catalog_bundle("terminal", "dummy");
        let entries = [DirEntry::new("terminal.fapp", &bytes)];
        let mut desktop = Desktop::<4, 4, 2>::new(
            DesktopConfig::parse("theme: xp-blue\ntaskbar:\n  start_label: start\n").unwrap(),
        );
        desktop.scan(&entries);
        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();
        desktop.render(&mut canvas, &Input::new(), 1000);
        desktop.open_app(0).expect("window");

        desktop.render(&mut canvas, &click_at(177, 748), 1100);
        assert_eq!(
            desktop.windows().get(0).unwrap().state,
            WindowState::Minimized
        );

        desktop.render(&mut canvas, &click_at(177, 748), 1400);
        assert_eq!(desktop.windows().get(0).unwrap().state, WindowState::Normal);
    }

    #[test]
    fn window_edges_choose_the_xp_resize_cursor() {
        let mut windows = WindowManager::<4>::new();
        let index = windows
            .open(0, "One", Rect::new(0, 0, 800, 600), 400, 300, 0)
            .unwrap();
        let rect = Rect::new(100, 100, 400, 300);
        windows.get_mut(index).unwrap().rect = rect;

        let middle_y = rect.y + rect.height as i32 / 2;
        let middle_x = rect.x + rect.width as i32 / 2;
        assert_eq!(
            window_cursor_shape(&windows, Point::new(rect.x + 1, middle_y)),
            paint::CursorShape::SizeHorizontal
        );
        assert_eq!(
            window_cursor_shape(&windows, Point::new(rect.right_i32() - 1, middle_y)),
            paint::CursorShape::SizeHorizontal
        );
        assert_eq!(
            window_cursor_shape(&windows, Point::new(middle_x, rect.y + 1)),
            paint::CursorShape::SizeVertical
        );
        assert_eq!(
            window_cursor_shape(&windows, Point::new(rect.x + 1, rect.y + 1)),
            paint::CursorShape::SizeNwse
        );
        assert_eq!(
            window_cursor_shape(&windows, Point::new(rect.right_i32() - 1, rect.y + 1)),
            paint::CursorShape::SizeNesw
        );
        assert_eq!(
            window_cursor_shape(&windows, rect.center()),
            paint::CursorShape::Arrow
        );
    }

    #[test]
    fn dragging_a_window_edge_resizes_it() {
        let bytes = catalog_bundle("terminal", "dummy");
        let entries = [DirEntry::new("terminal.fapp", &bytes)];
        let mut desktop =
            Desktop::<4, 4, 2>::new(DesktopConfig::parse("theme: xp-blue\n").unwrap());
        desktop.scan(&entries);
        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();
        desktop.render(&mut canvas, &Input::new(), 1000);

        let index = desktop.open_app(0).expect("window");
        desktop.render(&mut canvas, &Input::new(), 1100);
        let before = desktop.windows().get(index).unwrap().rect;
        let x = before.right_i32() - 2;
        let y = before.y + before.height as i32 / 2;

        // Press on the right border, then drag it 40 pixels to the right.
        let mut input = Input::new();
        input.push(Event::PointerMoved(feos_gfx::Point::new(x, y)));
        input.push(Event::PointerDown {
            pos: feos_gfx::Point::new(x, y),
            button: MouseButton::Left,
        });
        desktop.render(&mut canvas, &input, 1200);
        input.clear_events();

        input.push(Event::PointerMoved(feos_gfx::Point::new(x + 40, y)));
        desktop.render(&mut canvas, &input, 1300);

        let after = desktop.windows().get(index).unwrap().rect;
        assert_eq!(after.width, before.width + 40);
        assert_eq!(after.x, before.x);
    }

    #[test]
    fn a_suspended_foreground_window_relaunches_from_the_taskbar() {
        let bytes = catalog_bundle("terminal", "dummy");
        let entries = [DirEntry::new("terminal.fapp", &bytes)];
        let mut desktop =
            Desktop::<4, 4, 2>::new(DesktopConfig::parse("theme: xp-blue\n").unwrap());
        desktop.scan(&entries);
        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();
        desktop.render(&mut canvas, &Input::new(), 1000);

        let index = desktop.open_app(0).expect("window");
        let layout = taskbar::layout(Rect::new(0, 0, 1024, 768), &desktop.config().taskbar);
        let button = taskbar::task_button_rect(&layout, desktop.windows(), index).expect("button");

        // Simulate the foreground app returning because its taskbar button
        // was clicked.
        desktop.windows_mut().minimize(index);
        desktop.windows_mut().get_mut(index).unwrap().suspended = true;

        let response = desktop.render(
            &mut canvas,
            &click_at(
                button.x + button.width as i32 / 2,
                button.y + button.height as i32 / 2,
            ),
            1400,
        );
        assert_eq!(response.launch.map(|request| request.app), Some(0));
        assert_eq!(
            desktop.windows().get(index).unwrap().state,
            WindowState::Normal
        );
        assert!(!desktop.windows().get(index).unwrap().suspended);
    }

    fn right_click_at(x: i32, y: i32) -> Input {
        let mut input = Input::new();
        input.push(Event::PointerDown {
            pos: feos_gfx::Point::new(x, y),
            button: MouseButton::Right,
        });
        input.push(Event::PointerUp {
            pos: feos_gfx::Point::new(x, y),
            button: MouseButton::Right,
        });
        input
    }

    #[test]
    fn right_click_opens_and_uses_the_desktop_menu() {
        let bytes = catalog_bundle("terminal", "dummy");
        let entries = [DirEntry::new("terminal.fapp", &bytes)];
        let mut desktop =
            Desktop::<4, 4, 2>::new(DesktopConfig::parse("theme: xp-blue\n").unwrap());
        desktop.scan(&entries);
        desktop.work_area = Rect::new(0, 0, 800, 560);
        desktop.open_app(0).expect("window");

        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();
        desktop.render(&mut canvas, &right_click_at(700, 500), 1000);
        assert!(desktop.context.open);

        // The menu opens at the pointer, clamped to the work area; "Show
        // Desktop" is the second row.
        desktop.render(&mut canvas, &click_at(750, 540), 1400);
        assert!(!desktop.context.open);
        assert_eq!(
            desktop.windows().get(0).unwrap().state,
            WindowState::Minimized
        );
    }

    #[test]
    fn double_clicking_a_desktop_icon_launches_it() {
        let bytes = catalog_bundle("terminal", "dummy");
        let entries = [DirEntry::new("terminal.fapp", &bytes)];
        let mut desktop =
            Desktop::<4, 4, 2>::new(DesktopConfig::parse("theme: xp-blue\n").unwrap());
        desktop.scan(&entries);
        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();

        // The first click only selects the icon.
        let response = desktop.render(&mut canvas, &click_at(30, 30), 1000);
        assert_eq!(desktop.selection(), 0);
        assert!(response.launch.is_none());

        let response = desktop.render(&mut canvas, &click_at(30, 30), 1200);
        assert_eq!(response.launch.map(|request| request.app), Some(0));
    }

    #[test]
    fn hovering_a_start_menu_item_moves_the_highlight() {
        let config = DesktopConfig::parse("start_menu:\n  initially_open: true\n").unwrap();
        let names = ["a.fapp", "b.fapp", "c.fapp"];
        let bundles: std::vec::Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|id| catalog_bundle(id, "dummy"))
            .collect();
        let entries: std::vec::Vec<_> = names
            .iter()
            .zip(bundles.iter())
            .map(|(name, data)| DirEntry::new(*name, data))
            .collect();
        let mut desktop = Desktop::<4, 4, 2>::new(config);
        desktop.scan(&entries);
        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();

        let mut input = Input::new();
        input.push(Event::PointerMoved(feos_gfx::Point::new(100, 380)));
        desktop.render(&mut canvas, &input, 1000);
        assert_eq!(desktop.start_menu().highlighted, 1);
    }

    #[test]
    fn double_click_interval_expires() {
        let mut clicks = ClickMemory::default();
        let position = feos_gfx::Point::new(10, 12);
        assert!(!clicks.hit(7, position, 1000));
        assert!(clicks.hit(7, position, 1300));
        assert!(!clicks.hit(7, position, 1900));
        assert!(!clicks.hit(8, position, 2000));
    }

    #[test]
    fn run_dialog_collects_text_and_submits_on_enter() {
        let mut desktop =
            Desktop::<4, 4, 2>::new(DesktopConfig::parse("theme: xp-blue\n").unwrap());
        desktop.run_dialog.show();
        let mut pixels = std::vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();

        let mut input = Input::new();
        input.push(Event::Text('p'));
        input.push(Event::Text('s'));
        let response = desktop.render(&mut canvas, &input, 1000);
        assert!(response.run.is_none());
        assert!(desktop.run_dialog.open);
        assert_eq!(desktop.run_dialog.text.as_str(), "ps");

        let mut input = Input::new();
        input.push(Event::KeyDown(Key::Enter, Modifiers::NONE));
        let response = desktop.render(&mut canvas, &input, 1100);
        let command = response.run.expect("submitted command");
        assert_eq!(command.as_str(), "ps");
        assert!(!desktop.run_dialog.open);
    }
}
