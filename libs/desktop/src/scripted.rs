//! Scripted-input tests that drive whole frames through
//! [`Desktop::render_with_platform`].

use std::boxed::Box;
use std::cell::Cell;
use std::format;
use std::vec;
use std::vec::Vec;

use feos_app::{BoardInfo, Context, ProcessInfo, ProcessState};
use feos_gui::{Event, Key, Modifiers, MouseButton};
use feos_hal::{Clock, Cpu, Power};
use feos_sync::SpinLock;

use super::*;

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

struct FakePower;
impl Power for FakePower {
    fn system_off(&self) -> ! {
        panic!("system_off")
    }
    fn system_reset(&self) -> ! {
        panic!("system_reset")
    }
}

struct FakeCpu;
impl Cpu for FakeCpu {
    fn current_el(&self) -> u64 {
        1
    }
    fn midr(&self) -> u64 {
        0
    }
    fn halt(&self) -> ! {
        panic!("halt")
    }
}

std::thread_local! {
    /// Key-down events a [`Probe`] app has seen on this thread.
    static KEYS_SEEN: Cell<u32> = const { Cell::new(0) };
    /// Frames a [`Probe`] app has been asked to draw on this thread.
    static FRAMES: Cell<u32> = const { Cell::new(0) };
    /// Arguments the probe last opened with, and how many times it opened.
    static OPENED: Cell<(u32, usize)> = const { Cell::new((0, 0)) };
    /// Canvas size of the probe's last frame.
    static CANVAS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    /// Whether the last frame had keyboard or pointer events routed to it.
    static ACTIVE: Cell<(bool, bool)> = const { Cell::new((false, false)) };
    /// Paths offered to the probe by a drop, as a length.
    static DROPPED: Cell<usize> = const { Cell::new(0) };
    /// Background ticks seen.
    static TICKS: Cell<u32> = const { Cell::new(0) };
}

/// A windowed app whose pixels are a pure function of what it has been told.
struct Probe;

impl App for Probe {
    fn name(&self) -> &'static str {
        "probe"
    }
    fn usage(&self) -> &'static str {
        "probe"
    }
    fn description(&self) -> &'static str {
        "records what the desktop routes to it"
    }
    fn windowed(&self) -> bool {
        true
    }
    fn open_window(&self, context: &WindowContext<'_, '_>) -> bool {
        OPENED.with(|cell| cell.set((cell.get().0 + 1, context.args.len())));
        true
    }
    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        FRAMES.with(|cell| cell.set(cell.get() + 1));
        context.dispatch_keys(|event| {
            if matches!(event, Event::KeyDown(..)) {
                KEYS_SEEN.with(|cell| cell.set(cell.get() + 1));
            }
        });
        CANVAS.with(|cell| cell.set((context.canvas.width(), context.canvas.height())));
        ACTIVE.with(|cell| cell.set((context.keyboard_active, context.pointer_active)));

        let keys = KEYS_SEEN.with(Cell::get);
        let shade = (keys % 200) as u8;
        context
            .canvas
            .clear(Color::rgb(shade, 40 + context.keyboard_active as u8 * 80, 90));
        if context.pointer_active {
            let pointer = context.pointer();
            context
                .canvas
                .fill_rect(Rect::new(pointer.x, pointer.y, 6, 6), Color::WHITE);
        }
        WindowFrame::Continue
    }
    fn background_tick(&self, _platform: &Platform, _now_ms: u64) {
        TICKS.with(|cell| cell.set(cell.get() + 1));
    }
    fn accept_drop(&self, payload: &str) -> bool {
        DROPPED.with(|cell| cell.set(payload.len()));
        true
    }
    fn run(&self, _context: &mut Context<'_>) -> i32 {
        0
    }
}

struct Animated;

impl App for Animated {
    fn name(&self) -> &'static str {
        "animated"
    }
    fn usage(&self) -> &'static str {
        "animated"
    }
    fn description(&self) -> &'static str {
        "asks for frames"
    }
    fn windowed(&self) -> bool {
        true
    }
    fn animates(&self) -> bool {
        true
    }
    fn animation_period_ms(&self) -> u32 {
        100
    }
    fn open_window(&self, _context: &WindowContext<'_, '_>) -> bool {
        true
    }
    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        context.canvas.clear(Color::rgb(10, 200, 30));
        WindowFrame::Continue
    }
    fn run(&self, _context: &mut Context<'_>) -> i32 {
        0
    }
}

struct Background;

impl App for Background {
    fn name(&self) -> &'static str {
        "background"
    }
    fn usage(&self) -> &'static str {
        "background"
    }
    fn description(&self) -> &'static str {
        "runs as a process"
    }
    fn run(&self, _context: &mut Context<'_>) -> i32 {
        0
    }
}

struct Blocking;

impl App for Blocking {
    fn name(&self) -> &'static str {
        "blocking"
    }
    fn usage(&self) -> &'static str {
        "blocking"
    }
    fn description(&self) -> &'static str {
        "the foreground path"
    }
    fn foreground(&self) -> bool {
        true
    }
    fn run(&self, _context: &mut Context<'_>) -> i32 {
        0
    }
}

static PROBE: Probe = Probe;
static ANIMATED: Animated = Animated;
static BACKGROUND: Background = Background;
static BLOCKING: Blocking = Blocking;

struct Registry;

impl AppRegistry for Registry {
    fn find(&self, name: &str) -> Option<&'static dyn App> {
        match name {
            "probe" => Some(&PROBE),
            "animated" => Some(&ANIMATED),
            "background" => Some(&BACKGROUND),
            "blocking" => Some(&BLOCKING),
            _ => None,
        }
    }
}

static REGISTRY: Registry = Registry;

#[derive(Default)]
struct ProcessLog {
    next_pid: u32,
    live: Vec<u32>,
    kill_attempts: u32,
    /// Refuse this many kills first, as a target running on another CPU does.
    refuse_kills: u32,
}

struct FakeProcesses(SpinLock<Option<ProcessLog>>);

impl FakeProcesses {
    fn leaked(refuse_kills: u32) -> &'static Self {
        Box::leak(Box::new(Self(SpinLock::new(Some(ProcessLog {
            next_pid: 2,
            refuse_kills,
            ..ProcessLog::default()
        })))))
    }

    fn with<R>(&self, f: impl FnOnce(&mut ProcessLog) -> R) -> R {
        f(self.0.lock().as_mut().unwrap())
    }
}

impl Processes for FakeProcesses {
    fn spawn(&self, _app: &'static dyn App, _args: &str) -> Result<u32, SpawnError> {
        Ok(self.with(|log| {
            let pid = log.next_pid;
            log.next_pid += 1;
            log.live.push(pid);
            pid
        }))
    }
    fn kill(&self, pid: u32) -> bool {
        self.with(|log| {
            log.kill_attempts += 1;
            if log.refuse_kills > 0 {
                log.refuse_kills -= 1;
                return false;
            }
            log.live.retain(|live| *live != pid);
            true
        })
    }
    fn current(&self) -> u32 {
        1
    }
    fn snapshot(&self, out: &mut [ProcessInfo]) -> usize {
        self.with(|log| {
            let mut count = 0;
            for pid in &log.live {
                if count < out.len() {
                    out[count] = ProcessInfo {
                        pid: *pid,
                        state: ProcessState::Running,
                        ..ProcessInfo::EMPTY
                    };
                    count += 1;
                }
            }
            count
        })
    }
}

fn platform(processes: &'static FakeProcesses) -> Platform {
    static CLOCK: FakeClock = FakeClock;
    static POWER: FakePower = FakePower;
    static CPU: FakeCpu = FakeCpu;
    Platform {
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
        processes,
        memory: None,
        apps: &REGISTRY,
        filesystem: None,
        net: None,
    }
}

fn bundle(id: &str, entry: &str) -> Vec<u8> {
    use feos_archive::ZipWriter;
    let manifest = format!(
        "id: {id}\nname: {id}\nentry: {entry}\ndescription: test app\nwindow:\n  width: 300\n  height: 240\n"
    );
    let mut output = vec![0u8; 8192];
    let mut writer = ZipWriter::new(&mut output);
    writer
        .write_stored("manifest.yaml", manifest.as_bytes())
        .unwrap();
    let len = writer.finish().unwrap();
    output.truncate(len);
    output
}

type TestDesktop = Desktop<4, 8, 2>;

/// One desktop, canvas and platform, with helpers for scripting frames.
struct Rig {
    desktop: Box<TestDesktop>,
    pixels: Vec<u32>,
    width: usize,
    height: usize,
    now: u64,
    input: Input,
    platform: Platform,
    processes: &'static FakeProcesses,
}

impl Rig {
    fn new(width: usize, height: usize, apps: &[(&str, &str)]) -> Self {
        Self::with_processes(width, height, apps, FakeProcesses::leaked(0))
    }

    fn with_processes(
        width: usize,
        height: usize,
        apps: &[(&str, &str)],
        processes: &'static FakeProcesses,
    ) -> Self {
        KEYS_SEEN.with(|cell| cell.set(0));
        FRAMES.with(|cell| cell.set(0));
        OPENED.with(|cell| cell.set((0, 0)));
        let bundles: Vec<(std::string::String, Vec<u8>)> = apps
            .iter()
            .map(|(id, entry)| (format!("{id}.fapp"), bundle(id, entry)))
            .collect();
        let entries: Vec<DirEntry<'_>> = bundles
            .iter()
            .map(|(name, bytes)| DirEntry::new(name, bytes))
            .collect();
        let mut desktop = Box::new(TestDesktop::default());
        desktop.scan(&entries);
        let mut rig = Self {
            desktop,
            pixels: vec![0; width * height],
            width,
            height,
            now: 1_000,
            input: Input::new(),
            platform: platform(processes),
            processes,
        };
        rig.frame();
        rig
    }

    /// Renders one frame from whatever was queued, and clears the queue.
    fn frame(&mut self) -> DesktopResponse {
        self.now += 16;
        let mut canvas = Canvas::new(&mut self.pixels, self.width, self.height, self.width).unwrap();
        let response = self.desktop.render_with_platform(
            &mut canvas,
            &self.input,
            self.now,
            Some(&self.platform),
        );
        self.input.clear_events();
        if let Some(request) = response.launch {
            self.desktop
                .launch(request.app, &REGISTRY, self.processes);
        }
        response
    }

    fn launch(&mut self, index: usize) -> usize {
        let outcome = self.desktop.launch(index, &REGISTRY, self.processes);
        let window = match outcome {
            LaunchOutcome::Windowed { window }
            | LaunchOutcome::Spawned { window, .. }
            | LaunchOutcome::Foreground { window } => window,
            other => panic!("unexpected {other:?}"),
        };
        self.frame();
        self.frame();
        window
    }

    fn key(&mut self, key: Key, modifiers: Modifiers) -> DesktopResponse {
        self.input.push(Event::KeyDown(key, modifiers));
        let response = self.frame();
        self.input.push(Event::ModifiersChanged(Modifiers::NONE));
        response
    }

    fn move_to(&mut self, x: i32, y: i32) -> DesktopResponse {
        self.input.push(Event::PointerMoved(Point::new(x, y)));
        self.frame()
    }

    fn press(&mut self, x: i32, y: i32) -> DesktopResponse {
        self.input.push(Event::PointerDown {
            pos: Point::new(x, y),
            button: MouseButton::Left,
        });
        self.frame()
    }

    fn release(&mut self, x: i32, y: i32) -> DesktopResponse {
        self.input.push(Event::PointerUp {
            pos: Point::new(x, y),
            button: MouseButton::Left,
        });
        self.frame()
    }

    fn click(&mut self, x: i32, y: i32) -> DesktopResponse {
        self.move_to(x, y);
        self.press(x, y);
        let response = self.release(x, y);
        // Let the second frame after a state change go by.
        self.frame();
        response
    }

    fn right_click(&mut self, x: i32, y: i32) -> DesktopResponse {
        self.input.push(Event::PointerMoved(Point::new(x, y)));
        self.input.push(Event::PointerDown {
            pos: Point::new(x, y),
            button: MouseButton::Right,
        });
        self.frame();
        self.input.push(Event::PointerUp {
            pos: Point::new(x, y),
            button: MouseButton::Right,
        });
        let response = self.frame();
        self.frame();
        response
    }

    fn place(&mut self, window: usize, rect: Rect) {
        let slot = self.desktop.windows_mut().get_mut(window).unwrap();
        slot.rect = rect;
        slot.restore = rect;
        self.frame();
    }

    fn window(&self, index: usize) -> DesktopWindow {
        *self.desktop.windows().get(index).unwrap()
    }

    fn panel(&self) -> Rect {
        let canvas = Rect::new(0, 0, self.width as u32, self.height as u32);
        let config = self.desktop.config();
        let layout = taskbar::layout(canvas, &config.taskbar);
        start_menu::panel_rect(canvas, &config.start_menu, &layout, config.taskbar.position)
    }

    fn bar(&self) -> Rect {
        let canvas = Rect::new(0, 0, self.width as u32, self.height as u32);
        taskbar::layout(canvas, &self.desktop.config().taskbar).bar
    }
}

const APP: [(&str, &str); 1] = [("probe", "probe")];
const TWO: [(&str, &str); 2] = [("animated", "animated"), ("probe", "probe")];

// Keyboard routing ---------------------------------------------------------

#[test]
fn enter_in_a_focused_app_does_not_launch_the_wallpaper_selection() {
    let mut rig = Rig::new(1024, 768, &APP);
    rig.launch(0);
    let response = rig.key(Key::Enter, Modifiers::NONE);
    assert!(
        response.launch.is_none(),
        "Enter typed into a focused app launched the wallpaper selection"
    );
    assert_eq!(KEYS_SEEN.with(Cell::get), 1, "the app should get the key");
}

#[test]
fn enter_on_the_empty_desktop_still_launches_the_selection() {
    let mut rig = Rig::new(1024, 768, &APP);
    assert!(rig.key(Key::Enter, Modifiers::NONE).launch.is_some());
}

#[test]
fn ctrl_esc_opens_the_start_menu_and_it_stays_open() {
    let mut rig = Rig::new(1024, 768, &APP);
    rig.launch(0);
    let response = rig.key(Key::Escape, Modifiers::CTRL);
    assert!(response.start_open, "opened and closed in one frame");
    assert!(rig.frame().start_open);
    assert_eq!(KEYS_SEEN.with(Cell::get), 0, "shortcuts are not typed");
    let response = rig.key(Key::Escape, Modifiers::NONE);
    assert!(!response.start_open);
}

#[test]
fn alt_tab_cycles_every_window_and_the_app_never_sees_it() {
    let mut rig = Rig::new(1024, 768, &TWO);
    let first = rig.launch(0);
    let second = rig.launch(1);
    assert_eq!(rig.desktop.windows().focused(), Some(second));
    rig.desktop.windows_mut().minimize(first);
    rig.frame();
    rig.key(Key::Tab, Modifiers::ALT);
    assert_eq!(rig.desktop.windows().focused(), Some(first));
    assert_eq!(rig.window(first).state, WindowState::Normal);
    rig.key(Key::Tab, Modifiers::ALT);
    assert_eq!(rig.desktop.windows().focused(), Some(second));
    assert_eq!(KEYS_SEEN.with(Cell::get), 0);
    rig.key(Key::Tab, Modifiers::ALT.with(Modifiers::SHIFT));
    assert_eq!(rig.desktop.windows().focused(), Some(first));
}

#[test]
fn alt_f4_closes_the_focused_window_then_asks_to_turn_off() {
    let mut rig = Rig::new(1024, 768, &APP);
    let window = rig.launch(0);
    rig.key(Key::F4, Modifiers::ALT);
    assert!(!rig.window(window).open);
    rig.key(Key::F4, Modifiers::ALT);
    assert!(rig.desktop.shutdown.open);
    rig.key(Key::F4, Modifiers::ALT);
    assert!(!rig.desktop.shutdown.open);
}

#[test]
fn the_window_menu_works_from_the_keyboard() {
    let mut rig = Rig::new(1024, 768, &APP);
    let window = rig.launch(0);
    rig.key(Key::Space, Modifiers::ALT);
    assert!(rig.desktop.window_menu.open);
    rig.key(Key::Down, Modifiers::NONE);
    rig.key(Key::Enter, Modifiers::NONE);
    assert!(!rig.desktop.window_menu.open);
    assert_eq!(rig.window(window).state, WindowState::Minimized);
    assert_eq!(KEYS_SEEN.with(Cell::get), 0, "the menu owned the keys");
}

#[test]
fn right_clicking_a_title_bar_opens_the_window_menu() {
    let mut rig = Rig::new(1024, 768, &APP);
    let window = rig.launch(0);
    let rect = Rect::new(200, 100, 400, 300);
    rig.place(window, rect);
    rig.right_click(rect.x + 120, rect.y + 12);
    assert!(rig.desktop.window_menu.open);
    assert!(!rig.desktop.context.open);
}

#[test]
fn tab_cannot_focus_a_minimized_window() {
    let mut rig = Rig::new(1024, 768, &TWO);
    let first = rig.launch(0);
    let second = rig.launch(1);
    rig.desktop.windows_mut().minimize(first);
    rig.desktop.windows_mut().minimize(second);
    rig.frame();
    rig.key(Key::Tab, Modifiers::NONE);
    assert_eq!(rig.desktop.windows().focused(), None);
    assert!(rig.key(Key::Space, Modifiers::NONE).start_open, "desktop keys still work");
}

// Pointer routing ----------------------------------------------------------

#[test]
fn clicking_blank_start_menu_space_cannot_reach_the_window_beneath() {
    let mut rig = Rig::new(1024, 768, &APP);
    let window = rig.launch(0);
    // Put the close button under the Start menu's header.
    let panel = rig.panel();
    let rect = Rect::new(panel.right_i32() - 300, panel.y, 300, 240);
    rig.place(window, rect);
    rig.key(Key::Escape, Modifiers::CTRL);
    assert!(rig.frame().start_open);
    let close_x = rect.right_i32() - 15;
    let close_y = rect.y + 15;
    assert!(panel.contains(Point::new(close_x, close_y)));
    rig.click(close_x, close_y);
    assert!(rig.window(window).open, "the click went through the menu");
}

#[test]
fn double_clicking_blank_start_menu_space_cannot_launch_the_icon_beneath() {
    // A canvas short enough that the Start menu covers the first shortcut.
    let mut rig = Rig::new(1024, 400, &APP);
    let panel = rig.panel();
    let cell = shortcut_cell(rig.desktop.config(), rig.desktop.work_area(), 0).unwrap();
    // The panel's header row holds no controls.
    let inside = Point::new(cell.x + 20, panel.y + 10);
    assert!(panel.contains(inside) && cell.contains(inside), "{panel:?} {cell:?}");
    rig.key(Key::Space, Modifiers::NONE);
    rig.frame();
    for _ in 0..2 {
        let response = rig.click(inside.x, inside.y);
        assert!(response.launch.is_none());
    }
    assert_eq!(rig.desktop.windows().count(), 0);
}

#[test]
fn clicking_the_taskbar_cannot_reach_a_window_hanging_under_it() {
    let mut rig = Rig::new(1024, 768, &APP);
    let window = rig.launch(0);
    let bar = rig.bar();
    // The window's lower part hangs below the work area, under the taskbar.
    let rect = Rect::new(300, bar.y - 100, 400, 240);
    rig.place(window, rect);
    rig.desktop.windows_mut().clear_focus();
    rig.frame();
    rig.click(500, bar.y + 10);
    assert_eq!(rig.desktop.windows().focused(), None);
}

#[test]
fn pressing_a_top_window_does_not_raise_the_one_beneath_it() {
    let mut rig = Rig::new(1024, 768, &TWO);
    let lower = rig.launch(0);
    let upper = rig.launch(1);
    rig.place(lower, Rect::new(100, 100, 400, 300));
    rig.place(upper, Rect::new(120, 110, 400, 300));
    let z = rig.window(lower).z;
    rig.click(300, 118);
    assert_eq!(rig.window(lower).z, z, "the lower window was raised");
    assert_eq!(rig.desktop.windows().focused(), Some(upper));
}

#[test]
fn the_corner_of_the_close_button_closes_instead_of_resizing() {
    let mut rig = Rig::new(1024, 768, &APP);
    let window = rig.launch(0);
    let rect = Rect::new(200, 100, 760, 560);
    rig.place(window, rect);
    // Inside the close button, and inside the 14x14 corner that used to win.
    let (x, y) = (rect.right_i32() - 10, rect.y + 8);
    assert_eq!(resize_edge_at(rect, Point::new(x, y)), None);
    rig.click(x, y);
    assert!(!rig.window(window).open);
}

#[test]
fn the_top_right_corner_still_resizes_from_its_outer_edges() {
    let rect = Rect::new(200, 100, 400, 300);
    for point in [
        Point::new(rect.right_i32() - 1, rect.y + 10),
        Point::new(rect.right_i32() - 12, rect.y + 1),
    ] {
        assert_eq!(resize_edge_at(rect, point), Some(ResizeEdge::TopRight));
    }
}

#[test]
fn show_desktop_leaves_windows_the_user_minimized_alone() {
    let mut rig = Rig::new(1024, 768, &TWO);
    let mine = rig.launch(0);
    let other = rig.launch(1);
    rig.desktop.windows_mut().minimize(mine);
    rig.desktop.toggle_show_desktop();
    assert_eq!(rig.window(other).state, WindowState::Minimized);
    rig.desktop.toggle_show_desktop();
    assert_eq!(rig.window(other).state, WindowState::Normal);
    assert_eq!(rig.window(mine).state, WindowState::Minimized);
}

#[test]
fn a_drag_and_a_quick_press_are_not_a_double_click() {
    let mut rig = Rig::new(1024, 768, &APP);
    let window = rig.launch(0);
    let rect = Rect::new(200, 100, 400, 300);
    rig.place(window, rect);
    let (x, y) = (rect.x + 120, rect.y + 12);
    rig.move_to(x, y);
    rig.press(x, y);
    rig.move_to(x + 2, y);
    rig.release(x + 2, y);
    let moved = rig.window(window).rect;
    assert_eq!(moved.x, rect.x + 2);
    // A second click right away, in the same place.
    rig.press(x + 2, y);
    rig.release(x + 2, y);
    rig.frame();
    assert_eq!(rig.window(window).state, WindowState::Normal);
    // A real double click still works.
    rig.press(x + 2, y);
    rig.release(x + 2, y);
    rig.frame();
    assert_eq!(rig.window(window).state, WindowState::Maximized);
}

#[test]
fn right_clicking_during_a_modal_dialog_opens_no_menu() {
    let mut rig = Rig::new(1024, 768, &APP);
    rig.desktop.shutdown.open = true;
    rig.frame();
    rig.right_click(900, 300);
    assert!(!rig.desktop.context.open);
    assert!(rig.desktop.shutdown.open);
}

// Launching ----------------------------------------------------------------

#[test]
fn windowed_autostart_entries_open_windows_and_are_not_supervised() {
    let mut rig = Rig::new(1024, 768, &[("probe", "probe"), ("background", "background")]);
    let config = DesktopConfig::parse("init:\n  autostart:\n    - probe\n    - background\n").unwrap();
    rig.desktop.set_config(config);
    let started = rig.desktop.start_session(&REGISTRY, rig.processes);
    assert_eq!(started, 2);
    assert_eq!(rig.desktop.windows().count(), 1);
    assert!(rig.window(0).runtime.is_some());
    assert_eq!(rig.desktop.init().len(), 1, "only the process is a service");
}

#[test]
fn run_prefers_a_bundle_over_the_foreground_path() {
    let rig = Rig::new(
        1024,
        768,
        &[("probe", "probe"), ("blocker", "blocking")],
    );
    let catalog = rig.desktop.catalog();
    let probe = catalog.find_id("probe").unwrap();
    let blocker = catalog.find_id("blocker").unwrap();
    let resolve = |command| rig.desktop.resolve_command(command, &REGISTRY);
    assert!(matches!(
        resolve("probe"),
        CommandTarget::Bundle { index, args: None } if index == probe
    ));
    // The command name of a bundle's entry point also goes through the bundle,
    // and keeps its arguments.
    assert!(matches!(
        resolve("blocking now"),
        CommandTarget::Bundle { index, args: Some("now") } if index == blocker
    ));
    assert!(matches!(resolve("background x"), CommandTarget::Spawn { args: "x", .. }));
    assert!(matches!(resolve("nonsense"), CommandTarget::NotFound));
    assert!(matches!(resolve("   "), CommandTarget::NotFound));
}

#[test]
fn run_uses_the_foreground_path_only_without_a_bundle() {
    let rig = Rig::new(1024, 768, &APP);
    assert!(matches!(
        rig.desktop.resolve_command("blocking", &REGISTRY),
        CommandTarget::Foreground { .. }
    ));
}

#[test]
fn launching_with_arguments_reaches_the_app_and_a_new_path_restarts_it() {
    let mut rig = Rig::new(1024, 768, &APP);
    rig.desktop
        .launch_with(0, Some("/media/a.mp4"), &REGISTRY, rig.processes);
    rig.frame();
    assert_eq!(OPENED.with(Cell::get), (1, "/media/a.mp4".len()));
    assert_eq!(rig.window(0).args.as_str(), "/media/a.mp4");

    // The same arguments just focus the window.
    rig.desktop
        .launch_with(0, Some("/media/a.mp4"), &REGISTRY, rig.processes);
    rig.frame();
    assert_eq!(OPENED.with(Cell::get).0, 1);

    rig.desktop
        .launch_with(0, Some("/x"), &REGISTRY, rig.processes);
    rig.frame();
    assert_eq!(OPENED.with(Cell::get), (2, 2));
}

#[test]
fn a_multi_instance_app_opens_a_window_per_launch_without_a_blank_one() {
    struct Multi;
    impl App for Multi {
        fn name(&self) -> &'static str {
            "probe"
        }
        fn usage(&self) -> &'static str {
            "probe"
        }
        fn description(&self) -> &'static str {
            "several windows"
        }
        fn windowed(&self) -> bool {
            true
        }
        fn multi_instance(&self) -> bool {
            true
        }
        fn open_window(&self, _context: &WindowContext<'_, '_>) -> bool {
            true
        }
        fn frame_window(&self, _context: &mut WindowContext<'_, '_>) -> WindowFrame {
            WindowFrame::Continue
        }
        fn run(&self, _context: &mut Context<'_>) -> i32 {
            0
        }
    }
    static MULTI: Multi = Multi;
    struct Only;
    impl AppRegistry for Only {
        fn find(&self, _name: &str) -> Option<&'static dyn App> {
            Some(&MULTI)
        }
    }
    let mut rig = Rig::new(1024, 768, &APP);
    // A click opens a window before the launch request arrives.
    let opened = rig.desktop.open_app_with(0, None).unwrap();
    let first = rig.desktop.launch(0, &Only, rig.processes);
    assert_eq!(first, LaunchOutcome::Windowed { window: opened });
    let second = rig.desktop.launch(0, &Only, rig.processes);
    assert!(matches!(second, LaunchOutcome::Windowed { window } if window != opened));
    assert_eq!(rig.desktop.windows().count(), 2);
}

#[test]
fn a_window_follows_its_bundle_across_a_rescan() {
    let mut rig = Rig::new(1024, 768, &TWO);
    let window = rig.launch(1);
    assert_eq!(rig.window(window).bundle.as_str(), "probe");

    // A new bundle that sorts first pushes the old indices down.
    let bundles = [
        ("aaa.fapp", bundle("aaa", "animated")),
        ("animated.fapp", bundle("animated", "animated")),
        ("probe.fapp", bundle("probe", "probe")),
    ];
    let entries: Vec<_> = bundles
        .iter()
        .map(|(name, bytes)| DirEntry::new(name, bytes))
        .collect();
    rig.desktop.scan(&entries);
    let index = rig.desktop.catalog().find_id("probe").unwrap();
    assert_eq!(rig.window(window).app, index);
    assert_eq!(rig.desktop.windows().get_open_for(index), Some(window));

    // And a bundle that vanished does not take over another app's slot.
    let entries = [DirEntry::new("aaa.fapp", &bundles[0].1)];
    rig.desktop.scan(&entries);
    assert_eq!(rig.window(window).app, window::NO_APP);
    rig.frame();
}

#[test]
fn closing_a_window_ends_its_process_even_if_the_first_kills_fail() {
    let processes = FakeProcesses::leaked(2);
    let mut rig = Rig::with_processes(1024, 768, &[("background", "background")], processes);
    let window = rig.launch(0);
    assert_eq!(processes.with(|log| log.live.len()), 1);
    assert!(rig.desktop.close_window(window));
    assert_eq!(processes.with(|log| log.live.len()), 1);
    rig.desktop.tick_session(processes);
    rig.desktop.tick_session(processes);
    assert_eq!(processes.with(|log| log.live.len()), 1);
    rig.desktop.tick_session(processes);
    assert_eq!(processes.with(|log| log.live.len()), 0);
    rig.desktop.tick_session(processes);
    assert_eq!(processes.with(|log| log.kill_attempts), 3, "no more kills once gone");
}

#[test]
fn log_off_closes_every_window_and_ends_their_processes() {
    let processes = FakeProcesses::leaked(2);
    let mut rig = Rig::with_processes(
        1024,
        768,
        &[("background", "background"), ("probe", "probe")],
        processes,
    );
    let first = rig.launch(0);
    let second = rig.launch(1);
    rig.frame();
    assert!(rig.window(first).open && rig.window(second).open);
    assert_eq!(processes.with(|log| log.live.len()), 1);

    // The first two kills are refused; log off keeps asking.
    assert_eq!(rig.desktop.close_all_windows(processes), 2);
    assert!(!rig.window(first).open && !rig.window(second).open);
    assert_eq!(processes.with(|log| log.live.len()), 0);
    assert_eq!(rig.desktop.close_all_windows(processes), 0);
}

#[test]
fn minimized_windows_get_background_ticks_and_visible_ones_do_not() {
    let mut rig = Rig::new(1024, 768, &APP);
    let window = rig.launch(0);
    TICKS.with(|cell| cell.set(0));
    let platform_ref = platform(rig.processes);
    rig.desktop.tick_background(&platform_ref, 5);
    assert_eq!(TICKS.with(Cell::get), 0);
    rig.desktop.windows_mut().minimize(window);
    rig.desktop.tick_background(&platform_ref, 5);
    assert_eq!(TICKS.with(Cell::get), 1);
}

#[test]
fn unknown_entry_explains_itself_in_the_window() {
    let mut rig = Rig::new(1024, 768, &[("ghost", "not-registered")]);
    let outcome = rig.desktop.launch(0, &REGISTRY, rig.processes);
    assert_eq!(outcome, LaunchOutcome::UnknownEntry);
    assert_eq!(rig.window(0).status, WindowStatus::UnknownEntry);
    rig.frame();
}

// Dragging -----------------------------------------------------------------

#[test]
fn dropping_a_path_on_a_shortcut_opens_it_with_that_path() {
    let mut rig = Rig::new(1024, 768, &APP);
    let cell = shortcut_cell(rig.desktop.config(), rig.desktop.work_area(), 0).unwrap();
    let (x, y) = (cell.x + 10, cell.y + 10);
    rig.press(x, y);
    rig.desktop.begin_drag("/media/clip.mp4");
    rig.move_to(x + 1, y);
    let response = rig.release(x + 1, y);
    assert_eq!(response.launch.unwrap().args.as_str(), "/media/clip.mp4");
    assert!(rig.desktop.drag.is_none());
}

#[test]
fn dropping_a_path_on_a_window_offers_it_to_the_app() {
    let mut rig = Rig::new(1024, 768, &APP);
    let window = rig.launch(0);
    let rect = Rect::new(300, 100, 400, 300);
    rig.place(window, rect);
    DROPPED.with(|cell| cell.set(0));
    rig.press(800, 600);
    rig.desktop.begin_drag("/x/y");
    rig.move_to(400, 200);
    rig.release(400, 200);
    assert_eq!(DROPPED.with(Cell::get), 4);
}

#[test]
fn escape_cancels_a_drag() {
    let mut rig = Rig::new(1024, 768, &APP);
    rig.press(500, 500);
    rig.desktop.begin_drag("/x");
    rig.frame();
    rig.key(Key::Escape, Modifiers::NONE);
    assert!(rig.desktop.drag.is_none());
    rig.release(500, 500);
}

// Off-screen windows -------------------------------------------------------

#[test]
fn a_window_dragged_off_screen_keeps_its_layout_size() {
    let mut rig = Rig::new(800, 600, &APP);
    let scratch: &'static mut [u32] = Box::leak(vec![0u32; 800 * 600].into_boxed_slice());
    rig.desktop.set_buffers(None, Some(scratch));
    let window = rig.launch(0);
    let content = |rig: &Rig| window::content_rect(rig.window(window).rect);
    rig.place(window, Rect::new(-200, 300, 320, 280));
    let expected = content(&rig);
    rig.frame();
    assert_eq!(
        CANVAS.with(Cell::get),
        (expected.width as usize, expected.height as usize),
        "the app was reflowed instead of clipped"
    );
    // The part on screen is the app's, the part off screen went nowhere.
    let inside = |x: usize, y: usize| rig.pixels[y * 800 + x];
    assert_eq!(inside(50, 400), Color::rgb(0, 120, 90).raw());
}

#[test]
fn off_screen_right_and_below_also_keep_the_size() {
    let mut rig = Rig::new(800, 600, &APP);
    let scratch: &'static mut [u32] = Box::leak(vec![0u32; 800 * 600].into_boxed_slice());
    rig.desktop.set_buffers(None, Some(scratch));
    let window = rig.launch(0);
    rig.place(window, Rect::new(700, 500, 320, 280));
    rig.frame();
    let expected = window::content_rect(rig.window(window).rect);
    assert_eq!(
        CANVAS.with(Cell::get),
        (expected.width as usize, expected.height as usize),
    );
}

// Scrolling ----------------------------------------------------------------

#[test]
fn all_programs_scrolls_past_what_fits() {
    let apps: Vec<(std::string::String, Vec<u8>)> = (0..8)
        .map(|index| (format!("app{index}.fapp"), bundle(&format!("app{index}"), "probe")))
        .collect();
    let entries: Vec<_> = apps
        .iter()
        .map(|(name, bytes)| DirEntry::new(name, bytes))
        .collect();
    let mut rig = Rig::new(1024, 400, &APP);
    rig.desktop.scan(&entries);
    rig.key(Key::Space, Modifiers::NONE);
    rig.key(Key::Tab, Modifiers::NONE);
    assert!(rig.desktop.start_menu().all_programs);
    for _ in 0..7 {
        rig.key(Key::Down, Modifiers::NONE);
    }
    assert_eq!(rig.desktop.start_menu().highlighted, 7);
    assert!(rig.desktop.start_menu().scroll > 0, "the highlight scrolled off");
    let response = rig.key(Key::Enter, Modifiers::NONE);
    assert_eq!(response.launch.unwrap().app, 7);
}

// Notifications ------------------------------------------------------------

#[test]
fn a_toast_appears_blocks_clicks_and_dismisses() {
    let mut rig = Rig::new(1024, 768, &APP);
    let before = rig.pixels.clone();
    rig.desktop
        .notify_text("Hello", "World", Level::Info, rig.now);
    rig.frame();
    assert_ne!(before, rig.pixels);
    assert_eq!(rig.desktop.notifications().visible_count(), 1);
    let canvas = Rect::new(0, 0, 1024, 768);
    let layout = taskbar::layout(canvas, &rig.desktop.config().taskbar);
    let area = rig
        .desktop
        .notifications()
        .area(&layout, rig.desktop.work_area());
    rig.click(area.x + 20, area.y + 20);
    assert_eq!(rig.desktop.notifications().visible_count(), 0);
    assert!(rig.desktop.windows().focused().is_none());
}

#[test]
fn the_bell_opens_the_history_and_marks_it_read() {
    let mut rig = Rig::new(1024, 768, &APP);
    rig.desktop.notify_text("Hi", "x", Level::Error, rig.now);
    rig.frame();
    assert_eq!(rig.desktop.notifications().unread(), 1);
    let canvas = Rect::new(0, 0, 1024, 768);
    let layout = taskbar::layout(canvas, &rig.desktop.config().taskbar);
    let bell = taskbar::bell_rect(&layout);
    rig.click(bell.x + 4, bell.y + 4);
    assert!(rig.desktop.notifications_open);
    assert!(!rig.desktop.clock_open, "the bell is not the clock");
    assert_eq!(rig.desktop.notifications().unread(), 0);
    rig.key(Key::Escape, Modifiers::NONE);
    assert!(!rig.desktop.notifications_open);
}

#[test]
fn settings_change_the_theme_and_invalidate_the_cache() {
    let mut rig = Rig::new(1024, 768, &APP);
    rig.desktop.settings_open = true;
    let before = *rig.desktop.config();
    rig.desktop.set_theme(ThemeName::Dark);
    assert!(rig.desktop.repaint_all);
    assert_ne!(before.theme, rig.desktop.config().theme);
    rig.frame();
    rig.key(Key::Escape, Modifiers::NONE);
    assert!(!rig.desktop.settings_open);
}

// Retained rendering -------------------------------------------------------

#[test]
fn an_idle_retained_frame_paints_nothing() {
    let mut rig = Rig::new(640, 480, &APP);
    rig.desktop.set_retained(true);
    let window = rig.launch(0);
    assert!(rig.window(window).open);
    rig.frame();
    let before = FRAMES.with(Cell::get);
    let response = rig.frame();
    assert_eq!(response.damage, None);
    assert!(!response.state_changed);
    assert_eq!(FRAMES.with(Cell::get), before, "the app was asked to redraw");
}

#[test]
fn moving_over_empty_wallpaper_costs_no_paint() {
    let mut rig = Rig::new(800, 600, &APP);
    rig.desktop.set_retained(true);
    rig.frame();
    rig.frame();
    rig.move_to(700, 300);
    rig.frame();
    let response = rig.move_to(701, 300);
    assert_eq!(response.damage, None);
}

#[test]
fn typing_repaints_only_the_focused_window() {
    let mut rig = Rig::new(800, 600, &APP);
    rig.desktop.set_retained(true);
    let window = rig.launch(0);
    rig.place(window, Rect::new(300, 100, 300, 240));
    rig.frame();
    let response = rig.key(Key::Character('a'), Modifiers::NONE);
    assert_eq!(response.damage, Some(Rect::new(300, 100, 300, 240)));
    assert_eq!(KEYS_SEEN.with(Cell::get), 1);
}

#[test]
fn an_animated_window_repaints_on_its_own_period_only() {
    let mut rig = Rig::new(800, 600, &TWO);
    rig.desktop.set_retained(true);
    let window = rig.launch(0);
    let rect = Rect::new(300, 100, 300, 240);
    rig.place(window, rect);
    rig.frame();
    rig.now += 200;
    let response = rig.frame();
    assert_eq!(response.damage, Some(rect));
    assert!(response.redraw);
    assert_eq!(response.animation_period_ms, 100);
    // Well inside its period nothing is due.
    assert_eq!(rig.frame().damage, None);
}

#[test]
fn only_a_real_state_change_asks_for_a_second_frame() {
    let mut rig = Rig::new(800, 600, &APP);
    rig.desktop.set_retained(true);
    let window = rig.launch(0);
    let rect = Rect::new(300, 100, 300, 240);
    rig.place(window, rect);
    rig.frame();
    assert!(!rig.move_to(310, 200).state_changed);
    assert!(!rig.key(Key::Character('a'), Modifiers::NONE).state_changed);
    // Minimising changes state after that frame was painted.
    let x = rect.right_i32() - 6 - 20 - 2 - 20 - 2 - 10;
    rig.move_to(x, rect.y + 15);
    rig.press(x, rect.y + 15);
    let response = rig.release(x, rect.y + 15);
    assert!(response.state_changed);
    assert_eq!(rig.window(window).state, WindowState::Minimized);
    let response = rig.frame();
    assert!(!response.state_changed);
    assert!(response.damage.is_some());
}

/// Random input, as a script both rendering modes can replay identically.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }
}

fn random_event(random: &mut Random, width: i32, height: i32) -> Event {
    let point = Point::new(
        random.below(width as u64 + 40) as i32 - 20,
        random.below(height as u64 + 40) as i32 - 20,
    );
    match random.below(16) {
        0..=4 => Event::PointerMoved(point),
        5 | 6 => Event::PointerDown {
            pos: point,
            button: MouseButton::Left,
        },
        7 | 8 => Event::PointerUp {
            pos: point,
            button: MouseButton::Left,
        },
        9 => Event::PointerDown {
            pos: point,
            button: MouseButton::Right,
        },
        10 => Event::PointerUp {
            pos: point,
            button: MouseButton::Right,
        },
        11 => Event::PointerWheel(random.below(7) as i32 - 3),
        12 => Event::Text((b'a' + random.below(26) as u8) as char),
        _ => {
            const KEYS: [Key; 12] = [
                Key::Tab,
                Key::Enter,
                Key::Escape,
                Key::Space,
                Key::Backspace,
                Key::Up,
                Key::Down,
                Key::Left,
                Key::Right,
                Key::F4,
                Key::Character('v'),
                Key::Delete,
            ];
            const MODIFIERS: [Modifiers; 4] = [
                Modifiers::NONE,
                Modifiers::NONE,
                Modifiers::CTRL,
                Modifiers::ALT,
            ];
            Event::KeyDown(
                KEYS[random.below(KEYS.len() as u64) as usize],
                MODIFIERS[random.below(4) as usize],
            )
        }
    }
}

fn scripted_session(rig: &mut Rig, seed: u64, frames: usize) {
    let mut random = Random(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let width = rig.width as i32;
    let height = rig.height as i32;
    for _ in 0..frames {
        for _ in 0..random.below(4) {
            rig.input.push(random_event(&mut random, width, height));
        }
        rig.frame();
        rig.now += random.below(60);
    }
}

#[test]
fn random_input_on_small_canvases_never_panics() {
    let sizes = [
        (1, 1),
        (2, 3),
        (16, 16),
        (40, 500),
        (500, 30),
        (120, 90),
        (200, 150),
        (320, 240),
        (640, 480),
    ];
    for (seed, (width, height)) in sizes.iter().cycle().take(36).enumerate() {
        let mut rig = Rig::new(*width, *height, &TWO);
        if seed % 2 == 1 {
            rig.desktop.set_retained(true);
        }
        if seed % 3 == 0 {
            rig.desktop
                .launch(0, &REGISTRY, rig.processes);
            rig.desktop
                .launch(1, &REGISTRY, rig.processes);
        }
        scripted_session(&mut rig, seed as u64 + 1, 200);
    }
}

#[test]
fn random_input_keeps_window_state_consistent() {
    let mut rig = Rig::new(640, 480, &TWO);
    rig.desktop.launch(0, &REGISTRY, rig.processes);
    rig.desktop.launch(1, &REGISTRY, rig.processes);
    let mut random = Random(0xfeed_f00d);
    for _ in 0..3000 {
        for _ in 0..random.below(4) {
            rig.input.push(random_event(&mut random, 640, 480));
        }
        rig.frame();
        if let Some(index) = rig.desktop.windows().focused() {
            let window = rig.window(index);
            assert!(window.open, "focus on a closed window");
            assert_ne!(window.state, WindowState::Minimized, "focus on a minimized window");
        }
        if rig.desktop.windows().count() == 0 {
            rig.desktop.launch(0, &REGISTRY, rig.processes);
        }
    }
}

/// The damage-tracked frame must equal the full repaint it stands in for,
/// including after thousands of mixed events.
#[test]
fn retained_frames_match_full_repaints_under_random_input() {
    for seed in 1..=3u64 {
        let handle = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || compare_modes(seed))
            .unwrap();
        handle.join().unwrap();
    }
}

fn compare_modes(seed: u64) {
    const WIDTH: usize = 640;
    const HEIGHT: usize = 480;
    let mut retained = Rig::new(WIDTH, HEIGHT, &TWO);
    let mut full = Rig::new(WIDTH, HEIGHT, &TWO);
    retained.desktop.set_retained(true);
    let background: &'static mut [u32] = Box::leak(vec![0u32; WIDTH * HEIGHT].into_boxed_slice());
    let scratch: &'static mut [u32] = Box::leak(vec![0u32; WIDTH * HEIGHT].into_boxed_slice());
    retained.desktop.set_buffers(Some(background), Some(scratch));
    let scratch: &'static mut [u32] = Box::leak(vec![0u32; WIDTH * HEIGHT].into_boxed_slice());
    full.desktop.set_buffers(None, Some(scratch));

    let mut random = Random(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    for step in 0..800 {
        // Keep the two probe apps' key counts in step: they share a thread.
        if retained.desktop.windows().count() == 0 {
            retained.desktop.launch(1, &REGISTRY, retained.processes);
            full.desktop.launch(1, &REGISTRY, full.processes);
        }
        let events: Vec<Event> = (0..random.below(3))
            .map(|_| random_event(&mut random, WIDTH as i32, HEIGHT as i32))
            // A probe counts keys globally, so typing would differ between the
            // rigs; what it does to the frame is covered by its own test.
            .filter(|event| !matches!(event, Event::KeyDown(Key::F4, _)))
            .collect();
        let advance = random.below(40);
        for rig in [&mut retained, &mut full] {
            for event in &events {
                rig.input.push(*event);
            }
            rig.now += advance;
        }

        // State that changes after a frame is painted shows on the next one,
        // and the session asks for that frame. Compare once things settle.
        let mut retained_response;
        let mut full_response;
        let mut settle = 0;
        loop {
            let saved = KEYS_SEEN.with(Cell::get);
            retained_response = retained.frame();
            KEYS_SEEN.with(|cell| cell.set(saved));
            full_response = full.frame();
            assert_eq!(
                retained_response.launch.is_some(),
                full_response.launch.is_some(),
                "seed {seed} step {step}"
            );
            settle += 1;
            assert!(settle < 8, "seed {seed} step {step}: state never settled");
            if !retained_response.state_changed && retained_response.launch.is_none() {
                break;
            }
            retained.now -= 16;
            full.now -= 16;
        }

        // The full rig painted its cursor into the frame, so ignore the pixels
        // it can touch. Everything else must match exactly.
        let pointer = full.input.pointer();
        let cursor = paint::cursor_bounds(pointer.x, pointer.y, full_response.cursor);
        for y in 0..HEIGHT {
            let row = y * WIDTH..(y + 1) * WIDTH;
            if retained.pixels[row.clone()] == full.pixels[row] {
                continue;
            }
            for x in 0..WIDTH {
                if cursor.contains(Point::new(x as i32, y as i32)) {
                    continue;
                }
                let got = retained.pixels[y * WIDTH + x];
                let want = full.pixels[y * WIDTH + x];
                assert_eq!(
                    got, want,
                    "seed {seed} step {step} pixel {x},{y}: damage was {:?}, events {events:?}",
                    retained_response.damage
                );
            }
        }
    }
}

// Golden images ------------------------------------------------------------

fn golden_hash(pixels: &[u32]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for pixel in pixels {
        for byte in pixel.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// Records one frame's hash for the comparison with the checked-in file. Run
/// with `FEOS_UPDATE_GOLDEN=1` to refresh the file after an intended visual
/// change. A frame that differs is written out as a PPM to look at.
fn check_golden(
    expected: &str,
    results: &mut Vec<(std::string::String, u64)>,
    name: &str,
    rig: &Rig,
) {
    let hash = golden_hash(&rig.pixels);
    results.push((name.into(), hash));
    if expected.contains(&format!("{name} {hash:016x}")) {
        return;
    }
    let mut bytes = format!("P6\n{} {}\n255\n", rig.width, rig.height).into_bytes();
    for pixel in &rig.pixels {
        bytes.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8]);
    }
    let directory = std::env::temp_dir().join("feos-golden");
    let _ = std::fs::create_dir_all(&directory);
    let _ = std::fs::write(directory.join(format!("{name}.ppm")), bytes);
}

#[test]
fn golden_desktop_frames() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/golden.txt");
    let expected = std::fs::read_to_string(path).unwrap_or_default();
    let mut results = Vec::new();

    let mut rig = Rig::new(1024, 768, &APP);
    rig.frame();
    check_golden(&expected, &mut results, "desktop_empty", &rig);

    rig.key(Key::Space, Modifiers::NONE);
    rig.frame();
    check_golden(&expected, &mut results, "desktop_start_menu", &rig);
    rig.key(Key::Escape, Modifiers::NONE);

    let window = rig.launch(0);
    rig.place(window, Rect::new(200, 100, 400, 300));
    rig.frame();
    check_golden(&expected, &mut results, "desktop_window", &rig);

    rig.desktop.settings_open = true;
    rig.frame();
    check_golden(&expected, &mut results, "desktop_settings", &rig);
    rig.desktop.settings_open = false;

    let mut tiny = Rig::new(320, 240, &APP);
    tiny.frame();
    check_golden(&expected, &mut results, "desktop_tiny", &tiny);

    let mut text = std::string::String::new();
    for (name, hash) in &results {
        text.push_str(&format!("{name} {hash:016x}\n"));
    }
    if std::env::var_os("FEOS_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
        std::fs::write(path, &text).unwrap();
        return;
    }
    assert_eq!(
        expected,
        text,
        "frames changed; PPMs are in {:?}. If this is intended, rerun with FEOS_UPDATE_GOLDEN=1.",
        std::env::temp_dir().join("feos-golden")
    );
}
