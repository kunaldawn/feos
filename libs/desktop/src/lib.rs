//! A configurable desktop environment for feos.
//!
//! The desktop is the session layer above the GUI toolkit. It owns the
//! wallpaper, application shortcuts, the window manager, the XP-style taskbar
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
pub mod paint;
pub mod start_menu;
pub mod taskbar;
pub mod theme;
pub mod window;

use core::fmt::Write as _;

use feos_app::{
    App, AppRegistry, Platform, ProcessInfo, ProcessState, Processes, SpawnError, WindowContext,
    WindowFrame, MAX_PROCESSES,
};
use feos_bundle::{AppCatalog, InstalledApp, ScanReport};
use feos_gfx::{Canvas, Color, Point, Rect};
use feos_gui::{
    Align, Id, Input, InteractionState, Key, Modifiers, RectExt, Sense, Ui, VerticalAlign,
};
use feos_init::{Init, MAX_SERVICES};
use feos_text::Text;

pub use config::{
    ConfigError, DesktopConfig, ShortcutConfig, StartMenuConfig, TaskbarConfig, TaskbarPosition,
    WallpaperConfig, WallpaperStyle, WindowConfig,
};
pub use feos_bundle::{BundleFormat, DirEntry};
pub use feos_init::{InitError, RestartPolicy, ServiceState, ServiceStatus};
pub use start_menu::{StartMenuCommand, StartMenuState};
pub use theme::{DesktopTheme, ThemeName};
pub use window::{
    DesktopWindow, ResizeEdge, WindowManager, WindowState, MAX_WINDOWS, MIN_WINDOW_HEIGHT,
    MIN_WINDOW_WIDTH,
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
    /// Arguments from the manifest.
    pub args: Text<64>,
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

/// Position of the desktop context menu.
#[derive(Clone, Copy, Debug, Default)]
struct ContextMenuState {
    open: bool,
    x: i32,
    y: i32,
}

/// The XP-style "Run..." dialog.
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

/// What the user chose in the "Log Off Windows" dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum LogoffAction {
    #[default]
    None,
    LogOff,
    Cancel,
}

/// Remembers the previous click so icons and title bars can distinguish a
/// single click from a double click, as Windows XP does.
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
    /// Whether the "Log Off Windows" confirmation dialog is open.
    logoff_open: bool,
    /// Whether "Show Desktop" is active.
    show_desktop: bool,
    /// Window that captured the current pointer drag, if any.
    pointer_window: Option<usize>,
}

impl<const N: usize, const A: usize, const S: usize> Desktop<N, A, S> {
    /// Creates an empty desktop from `config`.
    pub const fn new(config: DesktopConfig) -> Self {
        let theme = DesktopTheme::from_config(&config);
        let start_menu = StartMenuState {
            open: config.start_menu.initially_open,
            highlighted: 0,
            all_programs: false,
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
            context: ContextMenuState {
                open: false,
                x: 0,
                y: 0,
            },
            run_dialog: RunDialogState::new(),
            shutdown: ShutdownDialogState { open: false },
            clock_open: false,
            logoff_open: false,
            show_desktop: false,
            pointer_window: None,
        }
    }

    /// The active configuration.
    pub const fn config(&self) -> &DesktopConfig {
        &self.config
    }

    /// Replaces the configuration and rebuilds the theme.
    ///
    /// Open windows are kept; the init restart policy is updated in place.
    pub fn set_config(&mut self, config: DesktopConfig) {
        self.config = config;
        self.theme = DesktopTheme::from_config(&config);
        self.init.set_policy(config.init.policy);
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

    /// Minimises every window, or restores them when "Show Desktop" is
    /// clicked a second time.
    fn toggle_show_desktop(&mut self) {
        if self.show_desktop {
            for index in 0..N {
                if self
                    .windows
                    .get(index)
                    .is_some_and(|window| window.open && window.state == WindowState::Minimized)
                {
                    self.windows.restore(index);
                }
            }
            self.show_desktop = false;
        } else {
            for index in 0..N {
                self.windows.minimize(index);
            }
            self.show_desktop = true;
        }
        self.start_menu.close();
    }

    /// Feeds typed text and editing keys into the Run dialog.
    fn handle_run_keys(&mut self, input: &Input, submit: &mut bool) {
        for ch in input.text() {
            if !ch.is_control() {
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
        // XP's Ctrl+Esc opens the Start menu even when an application has
        // keyboard focus. Space only opens it from the desktop.
        if input
            .keys()
            .any(|(key, modifiers)| key == Key::Escape && modifiers.contains(Modifiers::CTRL))
        {
            self.start_menu.show();
            return;
        }

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
        self.windows.open(
            index,
            app.name(),
            work_area,
            width,
            height,
            self.config.windows.cascade,
        )
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
        let Some(app) = self.catalog.get(index).copied() else {
            return LaunchOutcome::UnknownApp;
        };
        let Some(window_index) = self.open_app(index) else {
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
            return LaunchOutcome::UnknownEntry;
        };
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
        match processes.spawn(runtime, app.manifest.args.as_str()) {
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
            Err(error) => LaunchOutcome::SpawnFailed(error),
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

        for id in config.autostart() {
            self.register_service(id.as_str(), registry);
        }

        self.session_started = true;
        self.init.start_all(processes)
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
        let mut changed = self.init.tick(processes);

        let mut live = [ProcessInfo::EMPTY; MAX_PROCESSES];
        let count = processes.snapshot(&mut live);
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
        let clock = paint::format_clock(uptime_ms, &config.taskbar);
        let mut response = DesktopResponse::default();

        let canvas_rect = Rect::new(0, 0, canvas.width() as u32, canvas.height() as u32);
        let layout = taskbar::layout(canvas_rect, &config.taskbar);
        self.work_area = layout.work_area(canvas_rect, config.taskbar.position);
        let run_dialog_open = self.run_dialog.open;
        let shutdown_open = self.shutdown.open;
        let clock_open = self.clock_open;
        let logoff_open = self.logoff_open;
        let mut run_submit = false;
        if run_dialog_open {
            self.handle_run_keys(input, &mut run_submit);
        } else if shutdown_open {
            if input.keys().any(|(key, _)| key == Key::Escape) {
                self.shutdown.open = false;
            }
        } else if clock_open {
            if input.keys().any(|(key, _)| key == Key::Escape) {
                self.clock_open = false;
            }
        } else if logoff_open {
            if input.keys().any(|(key, _)| key == Key::Escape) {
                self.logoff_open = false;
            }
        } else {
            // With a hosted app focused, Tab/Space/arrow keys belong to the
            // app. Ctrl+Esc is the XP shortcut that always opens Start, and an
            // already open menu always owns the keyboard.
            let ctrl_escape = input
                .keys()
                .any(|(key, modifiers)| key == Key::Escape && modifiers.contains(Modifiers::CTRL));
            let window_owns_keyboard = self
                .windows
                .focused_window()
                .is_some_and(|window| window.runtime.is_some());
            if self.start_menu.open || !window_owns_keyboard || ctrl_escape {
                self.handle_keyboard(input, &config);
            }
        }

        // The desktop consumes Tab/Space/Enter itself; keep the toolkit from
        // activating a focused widget for the same key press.
        let pointer_input = input.without_keyboard();
        let mut ui = Ui::new(canvas, &theme.ui, &pointer_input, &mut self.interaction);

        // Follow the pointer into the window that owns the current drag. This
        // keeps a resize or text selection alive even when the pointer crosses
        // the window border.
        let desktop_overlay = self.start_menu.open
            || self.context.open
            || run_dialog_open
            || shutdown_open
            || clock_open
            || logoff_open;
        let pointer_in_work_area = self.work_area.contains(ui.pointer());
        if (ui.pointer_pressed() || ui.secondary_pressed())
            && !desktop_overlay
            && pointer_in_work_area
        {
            self.pointer_window = self.windows.topmost_content_at(ui.pointer());
        }
        let pointer_window = if desktop_overlay {
            None
        } else if self.pointer_window.is_some() {
            // Keep delivering pointer events to the window that owns the drag,
            // even when the pointer crosses the taskbar or another window.
            self.pointer_window
        } else if pointer_in_work_area {
            self.windows.topmost_content_at(ui.pointer())
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

        let over_window = pointer_in_work_area && self.windows.topmost_at(ui.pointer()).is_some();
        paint::draw_wallpaper(&mut ui, &theme, &config.wallpaper);

        let mut launch_index = None;
        let mut shortcut_clicked = None;
        if config.icons.show_icons {
            shortcut_clicked = render_shortcuts(
                &mut ui,
                &theme,
                &config.icons,
                &self.catalog,
                self.work_area,
                self.selection,
            );
        }

        if let Some(index) = shortcut_clicked {
            // A single click selects the icon, exactly as XP does; the second
            // click within the double-click interval opens it.
            self.selection = index;
            if self
                .clicks
                .hit(0x1000_0000 | index as u32, ui.pointer(), uptime_ms)
            {
                launch_index = Some(index);
            }
        }

        let mut order = [0usize; N];
        let count = self.windows.z_order(&mut order);
        let work_area = self.work_area;
        let mut close_after = [false; N];
        for &index in &order[..count] {
            close_after[index] = render_window(
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
            );
        }
        for index in 0..N {
            if !close_after[index] {
                continue;
            }
            if let Some(window) = self.windows.get(index) {
                if let Some(runtime) = window.runtime {
                    runtime.close_window();
                }
            }
            self.windows.close(index);
        }

        let taskbar_response = taskbar::render(
            &mut ui,
            &theme,
            &config.taskbar,
            &layout,
            &self.windows,
            &self.catalog,
            clock.as_str(),
        );

        let menu_response = start_menu::render(
            &mut ui,
            &theme,
            &config.start_menu,
            &layout,
            config.taskbar.position,
            &self.catalog,
            &mut self.start_menu,
        );

        let pointer = ui.pointer();
        let pointer_pressed = ui.pointer_pressed();
        let pointer_released = ui.pointer_released();
        let secondary_released = ui.secondary_released();

        // A right click on the wallpaper opens the XP desktop menu. Right
        // clicks on a window, the Start menu or the taskbar belong to those
        // surfaces instead.
        if secondary_released
            && !over_window
            && !layout.bar.contains(pointer)
            && !menu_response.panel.contains(pointer)
        {
            self.start_menu.close();
            self.context.open = true;
            self.context.x = pointer.x;
            self.context.y = pointer.y;
        }

        let context_action = render_context_menu(
            &mut ui,
            &theme,
            &mut self.context,
            self.work_area,
            pointer_pressed,
        );

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

        let logoff_action = if self.logoff_open {
            render_logoff_dialog(&mut ui, &theme, self.work_area)
        } else {
            LogoffAction::None
        };

        // The pointer is painted last so it stays visible over windows and the
        // Start menu, exactly like the XP cursor. Its shape follows the window
        // edge underneath it.
        let cursor = ui.pointer();
        let over_desktop_menu = self.start_menu.open
            || self.context.open
            || self.run_dialog.open
            || self.shutdown.open
            || self.clock_open
            || self.logoff_open
            || layout.bar.contains(cursor);
        let cursor_shape = if over_desktop_menu {
            paint::CursorShape::Arrow
        } else {
            window_cursor_shape(&self.windows, cursor)
        };
        paint::draw_cursor_shape(&mut ui.painter, cursor.x, cursor.y, cursor_shape);

        let pointer = ui.pointer();
        drop(ui);

        if pointer_released || secondary_released {
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

        let escape = input.keys().any(|(key, _)| key == Key::Escape);
        let enter = !run_dialog_open
            && !shutdown_open
            && !clock_open
            && !logoff_open
            && input.keys().any(|(key, _)| key == Key::Enter);

        if taskbar_response.start_clicked {
            self.start_menu.toggle();
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
        match context_action {
            ContextAction::None | ContextAction::Refresh => {}
            ContextAction::ShowDesktop => {
                self.toggle_show_desktop();
            }
            ContextAction::TaskManager => {
                if let Some(index) = self.catalog.find_id("process-monitor") {
                    launch_index = Some(index);
                }
            }
            ContextAction::Properties => command = StartMenuCommand::Settings,
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
        } else if enter && self.selection < self.catalog.len() {
            // Enter launches the wallpaper shortcut selected with the arrows.
            launch_index = Some(self.selection).or(launch_index);
        }

        if let Some(index) = launch_index {
            self.show_desktop = false;
            if let Some(window) = self.open_app(index) {
                if let Some(app) = self.catalog.get(index) {
                    response.launch = Some(LaunchRequest {
                        app: index,
                        entry: app.manifest.entry,
                        args: app.manifest.args,
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
            && !layout.bar.contains(pointer)
        {
            self.start_menu.close();
        }

        // Clicking the wallpaper moves keyboard focus off the active window,
        // exactly like XP.
        if pointer_pressed
            && !over_window
            && !self.start_menu.open
            && !layout.bar.contains(pointer)
            && !menu_response.panel.contains(pointer)
        {
            self.windows.clear_focus();
        }

        if escape {
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
        response
    }
}

impl<const N: usize, const A: usize, const S: usize> Default for Desktop<N, A, S> {
    fn default() -> Self {
        Self::new(DesktopConfig::default())
    }
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
    ui.painter.canvas_mut().fill_vertical_gradient(
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
        "Log Off Windows",
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
    ui.painter.canvas_mut().fill_vertical_gradient(
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
    ui.painter.canvas_mut().fill_vertical_gradient(
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
    ui.painter.canvas_mut().fill_vertical_gradient(
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
    ui.painter.fill_rounded_rect(icon, 4, theme.caption_button);
    ui.painter
        .draw_text_centered(icon, ">", theme.caption_glyph, 2);

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

fn render_context_menu<const N: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    state: &mut ContextMenuState,
    work_area: Rect,
    pointer_pressed: bool,
) -> ContextAction {
    if !state.open {
        return ContextAction::None;
    }

    const WIDTH: u32 = 196;
    const ROW: u32 = 24;
    const PAD: u32 = 4;
    const SEP: u32 = 6;
    const ITEMS: [(&str, ContextAction); 4] = [
        ("Refresh", ContextAction::Refresh),
        ("Show Desktop", ContextAction::ShowDesktop),
        ("Task Manager", ContextAction::TaskManager),
        ("Properties", ContextAction::Properties),
    ];

    let height = PAD * 2 + ROW * ITEMS.len() as u32 + SEP * 2;
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
    let _blocker = ui.interact(
        Id::from_name("desktop.context.modal"),
        ui.canvas_rect(),
        Sense::click(),
    );

    if pointer_pressed && !panel.contains(ui.pointer()) {
        state.open = false;
        return ContextAction::None;
    }

    ui.painter.shadow(panel, 6, 8, 120);
    ui.painter.fill_rounded_rect(panel, 3, theme.menu_bg);
    ui.painter
        .stroke_rounded_rect(panel, 3, 1, theme.menu_border);

    let mut action = ContextAction::None;
    let mut row_y = panel.y + PAD as i32;
    for (index, (label, item_action)) in ITEMS.iter().enumerate() {
        if index == 2 || index == 3 {
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
        let id = Id::from_name("desktop.context.item").child(index as u64 + 1);
        let hit = ui.interact(id, row, Sense::click());
        if hit.hovered {
            ui.painter.fill_rounded_rect(row, 2, theme.menu_hover);
        }
        ui.painter.draw_text_ellipsis(
            Rect::new(row.x + 26, row.y, row.width.saturating_sub(30), row.height),
            label,
            if hit.hovered {
                theme.menu_hover_text
            } else {
                theme.menu_text
            },
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        if hit.clicked {
            action = *item_action;
            state.open = false;
        }
        row_y += ROW as i32;
    }

    action
}

fn render_shortcuts<const N: usize, const A: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    config: &ShortcutConfig,
    catalog: &AppCatalog<A>,
    work_area: Rect,
    selection: usize,
) -> Option<usize> {
    if catalog.is_empty() || work_area.is_empty() {
        return None;
    }

    let icon_size = config.icon_size.clamp(24, 96) as i32;
    let label_height = if config.show_labels { 34 } else { 8 };
    let cell_width = (icon_size + 60).max(150);
    let cell_height = icon_size + label_height;
    let rows = (work_area.height as i32 / cell_height).max(1);
    let mut launched = None;

    for index in 0..catalog.len().min(MAX_DESKTOP_ICONS) {
        let row = index as i32 % rows;
        let column = index as i32 / rows;
        let cell = Rect::new(
            work_area.x + 10 + column * (cell_width + 6),
            work_area.y + 10 + row * cell_height,
            cell_width as u32,
            cell_height as u32,
        );
        if cell.right_i32() > work_area.right_i32() || cell.bottom_i32() > work_area.bottom_i32() {
            break;
        }

        let id = Id::from_name("desktop.shortcut").child(index as u64 + 1);
        let hit = ui.interact(id, cell, Sense::click());
        let selected = index == selection;
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
        if let Some(app) = catalog.get(index) {
            let icon_rect = Rect::new(
                cell.x + (cell_width - icon_size) / 2,
                cell.y + 2,
                icon_size as u32,
                icon_size as u32,
            );
            paint::draw_icon(&mut ui.painter, &app.icon, icon_rect, theme.caption_button);
            if config.show_labels {
                draw_shortcut_label(
                    &mut ui.painter,
                    Rect::new(cell.x + 2, cell.y + icon_size + 2, cell.width - 4, 30),
                    app.name(),
                    if hit.hovered {
                        theme.desktop_text_hover
                    } else {
                        theme.desktop_text
                    },
                );
            }
        }
        if hit.clicked {
            launched = Some(index);
        }
    }

    launched
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
) -> bool {
    let Some(window) = windows.get(index).copied() else {
        return false;
    };
    if !window.open || window.state == WindowState::Minimized {
        return false;
    }

    let focused = windows.is_focused(index);
    let border = if focused {
        theme.window_border_active
    } else {
        theme.window_border_inactive
    };
    let rect = window.rect;
    if rect.width < 80 || rect.height < 40 {
        return false;
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
        .canvas_mut()
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
    if title_drag.pressed {
        windows.focus(index);
    }
    if title_drag.dragged {
        // Dragging a maximised caption restores the window first, exactly as
        // XP does, then continues moving the restored rectangle.
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
    }
    // Double-clicking a caption maximises or restores the window, matching the
    // XP window manager.
    if title_drag.clicked
        && !title_drag.dragged
        && clicks.hit(0x2000_0000 | index as u32, ui.pointer(), now_ms)
    {
        windows.toggle_maximize(index, work_area);
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
        if let Some(runtime) = window.runtime {
            runtime.close_window();
        }
        windows.close(index);
        return false;
    }
    if minimize_hit.clicked {
        windows.minimize(index);
        return false;
    }
    if maximize_hit.clicked {
        windows.toggle_maximize(index, work_area);
    }

    // Content -------------------------------------------------------------
    let content = window::content_rect(rect);
    let clip = ui.painter.push_clip(content);
    ui.painter.fill_rect(content, theme.window_bg);

    let body_id = Id::from_name("desktop.window.body").child(index as u64 + 1);
    let body = ui.interact(body_id, content, Sense::click());
    if body.clicked || body.pressed {
        windows.focus(index);
    }

    let mut close_requested = false;
    let windowed_runtime = window.runtime.filter(|runtime| runtime.windowed());
    if let (Some(runtime), Some(platform)) = (windowed_runtime, platform) {
        let args = catalog
            .get(window.app)
            .map(|app| app.manifest.args.as_str())
            .unwrap_or("");
        let mut started = window.app_started;
        // A window dragged past the framebuffer edge still gets the visible
        // part of its content. Passing the unclipped rectangle would make
        // `sub_canvas` fail and leave the frame with only its background.
        let app_area = content.clipped_to(ui.painter.canvas_rect());
        if let Some(mut app_canvas) = ui.painter.canvas_mut().sub_canvas(app_area) {
            let mut context = WindowContext {
                args,
                canvas: &mut app_canvas,
                input,
                platform,
                now_ms,
                origin: Point::new(app_area.x, app_area.y),
                pointer_active,
                keyboard_active,
            };
            if !started {
                started = runtime.open_window(&context);
                if started {
                    if let Some(window) = windows.get_mut(index) {
                        window.app_started = true;
                    }
                }
            }
            if started && runtime.frame_window(&mut context) == WindowFrame::Close {
                close_requested = true;
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

            let status = if window.launched {
                "Running in a background process"
            } else {
                "Ready - choose the app again to run it"
            };
            let status_color = if window.launched {
                theme.task_button
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

    close_requested
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
        (
            ResizeEdge::TopRight,
            Rect::new(right - RESIZE_CORNER, rect.y, corner, corner),
        ),
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
        if response.pressed {
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
        Some(ResizeEdge::TopRight)
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
    if !window.open || window.state != WindowState::Normal {
        return paint::CursorShape::Arrow;
    }

    match resize_edge_at(window.rect, pointer) {
        Some(ResizeEdge::Left | ResizeEdge::Right) => paint::CursorShape::SizeHorizontal,
        Some(ResizeEdge::Top | ResizeEdge::Bottom) => paint::CursorShape::SizeVertical,
        Some(ResizeEdge::TopLeft | ResizeEdge::BottomRight) => paint::CursorShape::SizeNwse,
        Some(ResizeEdge::TopRight | ResizeEdge::BottomLeft) => paint::CursorShape::SizeNesw,
        None => paint::CursorShape::Arrow,
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

        fn run(&self, _context: &mut Context<'_>) {}
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

        fn run(&self, _context: &mut Context<'_>) {}
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

        fn run(&self, _context: &mut Context<'_>) {}
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
