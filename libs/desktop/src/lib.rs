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

use feos_app::{App, AppRegistry, ProcessInfo, ProcessState, Processes, SpawnError, MAX_PROCESSES};
use feos_bundle::{AppCatalog, InstalledApp, ScanReport};
use feos_gfx::{Canvas, Color, Rect};
use feos_gui::{Align, Id, Input, InteractionState, Key, RectExt, Sense, Ui, VerticalAlign};
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
pub use window::{DesktopWindow, WindowManager, WindowState, MAX_WINDOWS};

/// Default maximum number of applications in the catalog.
pub const MAX_APPS: usize = 12;

/// Maximum number of shortcut cells drawn on the wallpaper.
pub const MAX_DESKTOP_ICONS: usize = 24;

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
    /// The notification area was clicked.
    pub tray_clicked: bool,
    /// Start menu visibility after the frame.
    pub start_open: bool,
    /// Number of open windows after the frame.
    pub window_count: usize,
}

impl DesktopResponse {
    /// Returns `true` when the result is empty.
    pub fn is_empty(&self) -> bool {
        self.launch.is_none() && self.command == StartMenuCommand::None && !self.tray_clicked
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
        let theme = self.theme;
        let config = self.config;
        let clock = paint::format_clock(uptime_ms, &config.taskbar);
        let mut response = DesktopResponse::default();

        let canvas_rect = Rect::new(0, 0, canvas.width() as u32, canvas.height() as u32);
        let layout = taskbar::layout(canvas_rect, &config.taskbar);
        self.work_area = layout.work_area(canvas_rect, config.taskbar.position);
        self.handle_keyboard(input, &config);

        // The desktop consumes Tab/Space/Enter itself; keep the toolkit from
        // activating a focused widget for the same key press.
        let pointer_input = input.without_keyboard();
        let mut ui = Ui::new(canvas, &theme.ui, &pointer_input, &mut self.interaction);
        paint::draw_wallpaper(&mut ui, &theme, &config.wallpaper);

        let mut shortcut_launch = None;
        if config.icons.show_icons {
            shortcut_launch = render_shortcuts(
                &mut ui,
                &theme,
                &config.icons,
                &self.catalog,
                self.work_area,
                self.selection,
            );
        }

        let mut order = [0usize; N];
        let count = self.windows.z_order(&mut order);
        let work_area = self.work_area;
        for &index in &order[..count] {
            render_window(
                &mut ui,
                &theme,
                &mut self.windows,
                &self.catalog,
                index,
                work_area,
            );
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
        drop(ui);

        let escape = input.keys().any(|(key, _)| key == Key::Escape);
        let enter = input.keys().any(|(key, _)| key == Key::Enter);

        if taskbar_response.start_clicked {
            self.start_menu.toggle();
        }

        if let Some(index) = taskbar_response.task_clicked {
            if self.windows.is_focused(index)
                && self
                    .windows
                    .get(index)
                    .is_some_and(|window| window.state == WindowState::Normal)
            {
                self.windows.minimize(index);
            } else if self
                .windows
                .get(index)
                .is_some_and(|window| window.state == WindowState::Minimized)
            {
                self.windows.restore(index);
            } else {
                self.windows.focus(index);
            }
        }

        if taskbar_response.desktop_clicked {
            for index in 0..N {
                self.windows.minimize(index);
            }
            self.start_menu.close();
        }

        if taskbar_response.tray_clicked {
            response.tray_clicked = true;
        }

        let mut launch_index = menu_response.launched.or(shortcut_launch);
        if let Some(index) = taskbar_response.quick_clicked {
            launch_index = Some(index);
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

        response.command = menu_response.command;

        if pointer_pressed
            && !taskbar_response.start_clicked
            && self.start_menu.open
            && !menu_response.panel.contains(pointer)
            && !layout.bar.contains(pointer)
        {
            self.start_menu.close();
        }

        if escape {
            self.start_menu.close();
        }

        response.start_open = self.start_menu.open;
        response.window_count = self.windows.count();
        response
    }
}

impl<const N: usize, const A: usize, const S: usize> Default for Desktop<N, A, S> {
    fn default() -> Self {
        Self::new(DesktopConfig::default())
    }
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

fn render_window<const UI: usize, const N: usize, const A: usize>(
    ui: &mut Ui<'_, '_, '_, UI>,
    theme: &DesktopTheme,
    windows: &mut WindowManager<N>,
    catalog: &AppCatalog<A>,
    index: usize,
    work_area: Rect,
) {
    let Some(window) = windows.get(index).copied() else {
        return;
    };
    if !window.open || window.state == WindowState::Minimized {
        return;
    }

    let focused = windows.is_focused(index);
    let border = if focused {
        theme.window_border_active
    } else {
        theme.window_border_inactive
    };
    let rect = window.rect;
    if rect.width < 80 || rect.height < 40 {
        return;
    }

    // Frame ---------------------------------------------------------------
    ui.painter.fill_rect(rect, border);
    ui.painter.fill_rect(
        Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2),
        theme.window_bg,
    );

    // Title bar -----------------------------------------------------------
    let title_height = 28u32;
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
        Sense::drag(),
    );
    if title_drag.pressed {
        windows.focus(index);
    }
    if title_drag.dragged {
        windows.move_by(
            index,
            title_drag.drag_delta.x,
            title_drag.drag_delta.y,
            work_area,
        );
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
        CaptionGlyph::Maximize,
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
        windows.close(index);
        return;
    }
    if minimize_hit.clicked {
        windows.minimize(index);
        return;
    }
    if maximize_hit.clicked {
        windows.toggle_maximize(index, work_area);
    }

    // Content -------------------------------------------------------------
    let content = Rect::new(
        rect.x + 1,
        rect.y + title_height as i32 + 1,
        rect.width - 2,
        rect.height - title_height - 2,
    );
    let clip = ui.painter.push_clip(content);
    ui.painter.fill_rect(content, theme.window_bg);

    let body_id = Id::from_name("desktop.window.body").child(index as u64 + 1);
    let body = ui.interact(body_id, content, Sense::click());
    if body.clicked || body.pressed {
        windows.focus(index);
    }

    if let Some(app) = catalog.get(window.app) {
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
    use feos_app::{Context, ProcessInfo, ProcessState};
    use feos_gui::{Event, Modifiers};
    use feos_sync::SpinLock;

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

    struct Registry;

    impl AppRegistry for Registry {
        fn find(&self, name: &str) -> Option<&'static dyn App> {
            if name == "dummy" {
                Some(&DUMMY)
            } else {
                None
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
                    name: [0; feos_app::PROCESS_NAME_MAX],
                    name_len: 0,
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
}
