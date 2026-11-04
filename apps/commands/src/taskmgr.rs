//! `taskmgr`: a Windows XP-style Task Manager.
//!
//! The task manager is a foreground application. It reads live data from the
//! kernel [`Processes`](feos_app::Processes) service and the HAL memory
//! service, keeps its own sampling history, and paints an XP Luna interface:
//!
//! * **Applications** - the running tasks, with End Task / Switch To.
//! * **Processes** - sortable PID/CPU/CPU-time/memory/threads/priority columns,
//!   End Process, Properties and a context menu.
//! * **Performance** - combined or per-core CPU history graphs, a memory
//!   graph, totals, commit charge, physical memory and kernel memory.
//! * **Networking** - the adapter view, with an honest "no adapter" state on
//!   boards that have no network driver.
//! * **Users** - the console user session.
//!
//! Menus (File, Options, View, Help), dialogs (Run, Select Columns, Process
//! Properties, About, Help, End Process, Log Off), update-speed control,
//! always-on-top / minimize options, status bar, sortable headers, scrolling,
//! mouse and serial-keyboard support are all implemented here. Like the rest
//! of the GUI, it is allocation-free apart from the off-screen frame buffer.

use core::cmp::Ordering;
use core::fmt::Write;

use alloc::vec;

use feos_app::{
    App, Context, Platform, ProcessInfo, ProcessState, SystemInfo, WindowContext, WindowFrame,
    MAX_CPUS, MAX_PROCESSES,
};
use feos_console::println;
use feos_gfx::{Canvas, Color, Point, Rect};
use feos_gui::{
    paint_icon, Align, Event, Icon, Input, Key, Modifiers, Painter, RectExt, TextBuffer,
    VerticalAlign,
};
use feos_hal::{MemoryInfo, PointerEvent};
use feos_keys::SerialKeys;
use feos_sync::SpinLock;
use feos_text::Text;

/// Number of history samples kept for the performance graphs.
const HISTORY: usize = 120;
/// Height of one process list row.
const ROW_HEIGHT: i32 = 20;
/// Height of a list column header.
const HEADER_HEIGHT: i32 = 22;
/// Width of a scrollbar.
const SCROLL_WIDTH: i32 = 16;
/// Fixed row height for menus.
const MENU_ROW: i32 = 22;
/// Fixed row height for dialogs.
const DIALOG_ROW: i32 = 22;

const TITLE_HEIGHT: i32 = 28;
const MENU_HEIGHT: i32 = 22;
const TABS_HEIGHT: i32 = 27;
const STATUS_HEIGHT: i32 = 22;

const FACE: Color = Color::rgb(236, 233, 216);
const FACE_LIGHT: Color = Color::rgb(244, 241, 232);
const FACE_DARK: Color = Color::rgb(214, 208, 190);
const BORDER: Color = Color::rgb(172, 168, 153);
const BORDER_DARK: Color = Color::rgb(128, 124, 111);
const TITLE_TOP: Color = Color::rgb(0, 88, 238);
const TITLE_BOTTOM: Color = Color::rgb(58, 145, 255);
const SELECT: Color = Color::rgb(49, 106, 197);
const SELECT_SOFT: Color = Color::rgb(214, 228, 250);
const FIELD: Color = Color::WHITE;
const FIELD_BORDER: Color = Color::rgb(127, 157, 185);
const HEADER_TOP: Color = Color::rgb(123, 162, 231);
const HEADER_BOTTOM: Color = Color::rgb(99, 117, 214);
const TEXT: Color = Color::rgb(24, 24, 24);
const MUTED: Color = Color::rgb(92, 92, 92);
const DISABLED: Color = Color::rgb(160, 160, 160);
const GRAPH_BG: Color = Color::rgb(0, 0, 0);
const GRAPH_GRID: Color = Color::rgb(0, 110, 0);
const GRAPH_CPU: Color = Color::rgb(0, 255, 0);
const GRAPH_MEM: Color = Color::rgb(255, 224, 0);
const GRAPH_LABEL: Color = Color::rgb(0, 230, 0);
const GRAPH_MEM_LABEL: Color = Color::rgb(230, 210, 0);
const HOVER_LIGHT: Color = Color::rgb(232, 240, 254);
const NOTICE_BG: Color = Color::rgb(255, 255, 225);
const NOTICE_BORDER: Color = Color::rgb(160, 150, 100);

/// The task manager application.
pub struct TaskManager;

impl App for TaskManager {
    fn name(&self) -> &'static str {
        "taskmgr"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["task-manager", "taskmanager", "tm", "taskmgr.exe"]
    }

    fn usage(&self) -> &'static str {
        "taskmgr"
    }

    fn description(&self) -> &'static str {
        "inspect applications, processes, CPU and memory with an XP-style task manager"
    }

    fn foreground(&self) -> bool {
        true
    }

    fn windowed(&self) -> bool {
        true
    }

    fn animates(&self) -> bool {
        true
    }

    fn animation_period_ms(&self) -> u32 {
        500
    }

    fn open_window(&self, context: &WindowContext<'_, '_>) -> bool {
        open_task_manager(context)
    }

    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        frame_task_manager(context)
    }

    fn close_window(&self) {
        close_task_manager();
    }

    fn run(&self, context: &mut Context<'_>) {
        run(context);
    }
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Applications,
    Processes,
    Performance,
    Networking,
    Users,
}

impl Tab {
    const ALL: [Self; 5] = [
        Self::Applications,
        Self::Processes,
        Self::Performance,
        Self::Networking,
        Self::Users,
    ];

    const fn title(self) -> &'static str {
        match self {
            Self::Applications => "Applications",
            Self::Processes => "Processes",
            Self::Performance => "Performance",
            Self::Networking => "Networking",
            Self::Users => "Users",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Applications => 0,
            Self::Processes => 1,
            Self::Performance => 2,
            Self::Networking => 3,
            Self::Users => 4,
        }
    }

    fn from_index(index: usize) -> Self {
        Self::ALL[index.min(Self::ALL.len() - 1)]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UpdateSpeed {
    High,
    Normal,
    Low,
    Paused,
}

impl UpdateSpeed {
    const fn interval_ms(self) -> u64 {
        match self {
            Self::High => 500,
            Self::Normal => 1_000,
            Self::Low => 2_000,
            Self::Paused => u64::MAX,
        }
    }
}

/// How the Performance tab presents CPU usage.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CpuView {
    /// One aggregate graph for the whole machine.
    Combined,
    /// One graph per online CPU.
    PerCore,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    File,
    Options,
    View,
    Help,
}

impl Menu {
    const ALL: [Self; 4] = [Self::File, Self::Options, Self::View, Self::Help];

    const fn title(self) -> &'static str {
        match self {
            Self::File => "File",
            Self::Options => "Options",
            Self::View => "View",
            Self::Help => "Help",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Pid,
    Cpu,
    CpuTime,
    Memory,
    Threads,
    Started,
    Status,
    Priority,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Column {
    ImageName,
    Pid,
    Cpu,
    CpuTime,
    MemUsage,
    Threads,
    StartTime,
    Status,
    Priority,
    UserName,
    SessionId,
}

impl Column {
    const ALL: [Self; 11] = [
        Self::ImageName,
        Self::Pid,
        Self::Cpu,
        Self::CpuTime,
        Self::MemUsage,
        Self::Threads,
        Self::StartTime,
        Self::Status,
        Self::Priority,
        Self::UserName,
        Self::SessionId,
    ];

    const fn title(self) -> &'static str {
        match self {
            Self::ImageName => "Image Name",
            Self::Pid => "PID",
            Self::Cpu => "CPU",
            Self::CpuTime => "CPU Time",
            Self::MemUsage => "Mem Usage",
            Self::Threads => "Threads",
            Self::StartTime => "Start Time",
            Self::Status => "Status",
            Self::Priority => "Priority",
            Self::UserName => "User Name",
            Self::SessionId => "Session ID",
        }
    }

    const fn width(self) -> i32 {
        match self {
            Self::ImageName => 142,
            Self::Pid => 44,
            Self::Cpu => 40,
            Self::CpuTime => 78,
            Self::MemUsage => 82,
            Self::Threads => 64,
            Self::StartTime => 90,
            Self::Status => 68,
            Self::Priority => 72,
            Self::UserName => 78,
            Self::SessionId => 58,
        }
    }

    const fn alignment(self) -> Align {
        match self {
            Self::ImageName | Self::Status | Self::Priority | Self::UserName => Align::Start,
            _ => Align::End,
        }
    }

    const fn sort_key(self) -> Option<SortKey> {
        match self {
            Self::ImageName => Some(SortKey::Name),
            Self::Pid => Some(SortKey::Pid),
            Self::Cpu => Some(SortKey::Cpu),
            Self::CpuTime => Some(SortKey::CpuTime),
            Self::MemUsage => Some(SortKey::Memory),
            Self::Threads => Some(SortKey::Threads),
            Self::StartTime => Some(SortKey::Started),
            Self::Status => Some(SortKey::Status),
            Self::Priority => Some(SortKey::Priority),
            Self::UserName | Self::SessionId => None,
        }
    }
}

const COLUMN_COUNT: usize = Column::ALL.len();

#[derive(Clone, Copy)]
struct Row {
    pid: u32,
    name: Text<24>,
    state: ProcessState,
    cpu_percent: u8,
    cpu_ticks: u64,
    cpu_time_ms: u64,
    memory_bytes: usize,
    threads: u16,
    priority: u8,
    started_ms: u64,
}

impl Row {
    const EMPTY: Self = Self {
        pid: 0,
        name: Text::EMPTY,
        state: ProcessState::Exited,
        cpu_percent: 0,
        cpu_ticks: 0,
        cpu_time_ms: 0,
        memory_bytes: 0,
        threads: 0,
        priority: 0,
        started_ms: 0,
    };

    fn status_name(&self) -> &'static str {
        match self.state {
            ProcessState::Running => "Running",
            ProcessState::Ready => "Ready",
            ProcessState::Exited => "Exited",
        }
    }

    fn priority_name(&self) -> &'static str {
        match self.priority {
            0..=5 => "High",
            6..=10 => "Normal",
            11..=15 => "Low",
            _ => "Normal",
        }
    }
}

#[derive(Clone, Copy)]
struct Previous {
    pid: u32,
    cpu_ticks: u64,
}

impl Previous {
    const EMPTY: Self = Self {
        pid: 0,
        cpu_ticks: 0,
    };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialog {
    None,
    Run,
    About,
    Columns,
    Help,
    Properties(u32),
    EndProcess { pid: u32, tab: Tab },
    LogOff,
}

/// The edge or corner used to resize the task manager window.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ResizeEdge {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl ResizeEdge {
    const fn moves_left(self) -> bool {
        matches!(self, Self::Left | Self::TopLeft | Self::BottomLeft)
    }

    const fn moves_right(self) -> bool {
        matches!(self, Self::Right | Self::TopRight | Self::BottomRight)
    }

    const fn moves_top(self) -> bool {
        matches!(self, Self::Top | Self::TopLeft | Self::TopRight)
    }

    const fn moves_bottom(self) -> bool {
        matches!(self, Self::Bottom | Self::BottomLeft | Self::BottomRight)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DragMode {
    None,
    Move {
        dx: i32,
        dy: i32,
    },
    Resize {
        edge: ResizeEdge,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        pointer_x: i32,
        pointer_y: i32,
    },
}

#[derive(Clone, Copy)]
struct ContextMenu {
    tab: Tab,
    x: i32,
    y: i32,
    hover: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    None,
    NewTask,
    EndTask,
    EndProcess,
    Close,
    Refresh,
    AlwaysOnTop,
    MinimizeOnUse,
    HideWhenMinimized,
    SpeedHigh,
    SpeedNormal,
    SpeedLow,
    SpeedPaused,
    SelectColumns,
    ToggleStatusBar,
    ToggleCpuGraph,
    ToggleMemGraph,
    CpuGraphCombined,
    CpuGraphPerCore,
    HelpTopics,
    About,
    SwitchTo,
    Properties,
}

#[derive(Clone, Copy)]
struct MenuItem {
    label: &'static str,
    action: Action,
    enabled: bool,
    checked: bool,
    separator: bool,
}

impl MenuItem {
    const fn new(label: &'static str, action: Action, enabled: bool) -> Self {
        Self {
            label,
            action,
            enabled,
            checked: false,
            separator: false,
        }
    }

    const fn checked(label: &'static str, action: Action, checked: bool) -> Self {
        Self {
            label,
            action,
            enabled: true,
            checked,
            separator: false,
        }
    }

    const fn separator() -> Self {
        Self {
            label: "",
            action: Action::None,
            enabled: false,
            checked: false,
            separator: true,
        }
    }
}

struct State {
    screen: Rect,
    maximized: bool,
    window_x: i32,
    window_y: i32,
    window_w: i32,
    window_h: i32,
    /// True when the task manager is hosted by the desktop window manager.
    embedded: bool,

    tab: Tab,
    pointer: Point,
    has_pointer: bool,
    mouse_down: bool,
    drag: DragMode,
    last_click_ms: u64,
    last_click_pid: Option<u32>,
    title_click_ms: u64,

    menu: Option<Menu>,
    menu_hover: usize,
    context: Option<ContextMenu>,
    dialog: Dialog,
    dialog_scroll: usize,

    rows: [Row; MAX_PROCESSES],
    row_count: usize,
    sort: SortKey,
    sort_desc: bool,
    selected_pid: Option<u32>,
    cursor: usize,
    process_scroll: usize,
    app_scroll: usize,
    user_scroll: usize,

    previous: [Previous; MAX_PROCESSES],
    previous_count: usize,
    last_total_ticks: u64,
    last_idle_ticks: u64,
    last_sample_ms: u64,
    system: SystemInfo,
    memory: MemoryInfo,
    cpu_percent: u8,
    memory_percent: u8,
    commit_total: usize,
    commit_limit: usize,
    commit_peak: usize,

    cpu_history: [u8; HISTORY],
    cpu_len: usize,
    cpu_head: usize,
    cpu_core_percent: [u8; MAX_CPUS],
    cpu_core_history: [[u8; HISTORY]; MAX_CPUS],
    cpu_core_len: [usize; MAX_CPUS],
    cpu_core_head: [usize; MAX_CPUS],
    last_core_total_ticks: [u64; MAX_CPUS],
    last_core_idle_ticks: [u64; MAX_CPUS],
    mem_history: [u8; HISTORY],
    mem_len: usize,
    mem_head: usize,

    update: UpdateSpeed,
    always_on_top: bool,
    minimize_on_use: bool,
    hide_when_minimized: bool,
    show_status: bool,
    show_cpu_graph: bool,
    show_mem_graph: bool,
    cpu_view: CpuView,

    column_visible: [bool; COLUMN_COUNT],
    column_backup: [bool; COLUMN_COUNT],

    run_text: TextBuffer<96>,

    notice: Text<160>,
    notice_until: u64,

    exit: bool,
    exit_message: &'static str,
}

impl State {
    fn new(screen: Rect) -> Self {
        let width = (screen.width as i32).min(980).max(720);
        let height = (screen.height as i32).min(660).max(520);
        let mut state = Self {
            screen,
            maximized: false,
            window_x: (screen.width as i32 - width).max(0) / 2,
            window_y: (screen.height as i32 - height).max(0) / 2,
            window_w: width,
            window_h: height,
            embedded: false,
            tab: Tab::Applications,
            pointer: Point::new(-100, -100),
            has_pointer: false,
            mouse_down: false,
            drag: DragMode::None,
            last_click_ms: 0,
            last_click_pid: None,
            title_click_ms: 0,
            menu: None,
            menu_hover: 0,
            context: None,
            dialog: Dialog::None,
            dialog_scroll: 0,
            rows: [Row::EMPTY; MAX_PROCESSES],
            row_count: 0,
            sort: SortKey::Name,
            sort_desc: false,
            selected_pid: None,
            cursor: 0,
            process_scroll: 0,
            app_scroll: 0,
            user_scroll: 0,
            previous: [Previous::EMPTY; MAX_PROCESSES],
            previous_count: 0,
            last_total_ticks: 0,
            last_idle_ticks: 0,
            last_sample_ms: 0,
            system: SystemInfo::EMPTY,
            memory: MemoryInfo::default(),
            cpu_percent: 0,
            memory_percent: 0,
            commit_total: 0,
            commit_limit: 0,
            commit_peak: 0,
            cpu_history: [0; HISTORY],
            cpu_len: 0,
            cpu_head: 0,
            cpu_core_percent: [0; MAX_CPUS],
            cpu_core_history: [[0; HISTORY]; MAX_CPUS],
            cpu_core_len: [0; MAX_CPUS],
            cpu_core_head: [0; MAX_CPUS],
            last_core_total_ticks: [0; MAX_CPUS],
            last_core_idle_ticks: [0; MAX_CPUS],
            mem_history: [0; HISTORY],
            mem_len: 0,
            mem_head: 0,
            update: UpdateSpeed::Normal,
            always_on_top: true,
            minimize_on_use: false,
            hide_when_minimized: false,
            show_status: true,
            show_cpu_graph: true,
            show_mem_graph: true,
            cpu_view: CpuView::Combined,
            column_visible: [true; COLUMN_COUNT],
            column_backup: [true; COLUMN_COUNT],
            run_text: TextBuffer::new(),
            notice: Text::EMPTY,
            notice_until: 0,
            exit: false,
            exit_message: "",
        };
        // The last two columns are optional; enable them from View > Select
        // Columns on a wide display.
        state.column_visible[Column::UserName as usize] = false;
        state.column_visible[Column::SessionId as usize] = false;
        state.column_backup = state.column_visible;
        state
    }

    fn set_screen(&mut self, screen: Rect) {
        self.screen = screen;
        if !self.maximized {
            self.window_w = self.window_w.min(screen.width as i32 - 40).max(720);
            self.window_h = self.window_h.min(screen.height as i32 - 40).max(480);
            self.window_x = self
                .window_x
                .clamp(0, (screen.width as i32 - self.window_w).max(0));
            self.window_y = self
                .window_y
                .clamp(0, (screen.height as i32 - self.window_h).max(0));
        }
    }

    /// Returns a small value that changes only when the pointer enters a
    /// different hoverable element.
    ///
    /// The run loop uses this to decide whether a mouse move needs a full
    /// repaint (hover changed) or only a cursor update. Repainting a
    /// 1024x768 frame for every pointer report is what made the desktop feel
    /// sluggish on the emulated CPU.
    fn hover_signature(&self) -> u64 {
        let layout = Layout::compute(self.screen, self);

        if self.dialog != Dialog::None {
            let panel = dialog_panel(layout, self.dialog);
            let buttons = dialog_buttons(self.dialog);
            for index in 0..buttons.len() {
                if dialog_button_rect(panel, index, buttons.len()).contains(self.pointer) {
                    return 100 + index as u64;
                }
            }
            let title = Rect::new(panel.x + 1, panel.y + 1, panel.width.saturating_sub(2), 25);
            if Rect::new(title.right_i32() - 22, title.y + 4, 16, 16).contains(self.pointer) {
                return 199;
            }
            if self.dialog == Dialog::Columns {
                let body = Rect::new(
                    panel.x + 14,
                    panel.y + 34,
                    panel.width.saturating_sub(28),
                    panel.height.saturating_sub(76),
                );
                let first = body.y + 22;
                let column = ((self.pointer.y - first) / DIALOG_ROW) as usize;
                if self.pointer.y >= first && column < COLUMN_COUNT {
                    return 200 + column as u64;
                }
            }
            return 0;
        }

        if let Some(context) = self.context {
            let panel = context_panel_rect(layout, context);
            if panel.contains(self.pointer) {
                let items = context_items(context.tab);
                let index = ((self.pointer.y - panel.y - 4) / MENU_ROW)
                    .clamp(0, items.len().saturating_sub(1) as i32);
                return 300 + index as u64;
            }
            return 0;
        }

        if let Some(menu) = self.menu {
            let panel = menu_panel_rect(self, layout, menu);
            if panel.contains(self.pointer) {
                let mut items = [MenuItem::new("", Action::None, false); 12];
                let count = self.menu_items(menu, &mut items);
                let mut y = panel.y + 4;
                for (index, item) in items.iter().take(count).enumerate() {
                    let height = if item.separator { 8 } else { MENU_ROW };
                    let rect = Rect::new(panel.x, y, panel.width, height as u32);
                    y += height;
                    if rect.contains(self.pointer) {
                        return 400 + index as u64;
                    }
                }
            }
            return 0;
        }

        for index in 0..3 {
            if title_button_rect(layout, index).contains(self.pointer) {
                return 10 + index as u64;
            }
        }
        for index in 0..Menu::ALL.len() {
            if menu_title_rect(layout, index).contains(self.pointer) {
                return 20 + index as u64;
            }
        }
        for (index, rect) in tab_rects(layout).iter().enumerate() {
            if rect.contains(self.pointer) {
                return 30 + index as u64;
            }
        }

        if !layout.content.contains(self.pointer) {
            return 0;
        }

        let list = list_layout(layout);
        match self.tab {
            Tab::Applications => {
                if let Some(index) = row_at(list.rows, self.pointer, self.app_scroll) {
                    if index < self.row_count {
                        return 100 + index as u64;
                    }
                }
                for index in 0..3 {
                    if button_rect(list.buttons, index, 3).contains(self.pointer) {
                        return 500 + index as u64;
                    }
                }
            }
            Tab::Processes => {
                if let Some(index) = column_at(list.header, self.pointer, self) {
                    return 200 + index as u64;
                }
                if let Some(index) = row_at(list.rows, self.pointer, self.process_scroll) {
                    if index < self.row_count {
                        return 100 + index as u64;
                    }
                }
                for index in 0..2 {
                    if button_rect(list.buttons, index, 2).contains(self.pointer) {
                        return 500 + index as u64;
                    }
                }
            }
            Tab::Users => {
                for index in 0..2 {
                    if button_rect(list.buttons, index, 2).contains(self.pointer) {
                        return 500 + index as u64;
                    }
                }
            }
            _ => {}
        }
        0
    }

    fn selected_row(&self) -> Option<&Row> {
        let pid = self.selected_pid?;
        self.rows[..self.row_count]
            .iter()
            .find(|row| row.pid == pid)
    }

    fn selected_index(&self) -> Option<usize> {
        let pid = self.selected_pid?;
        self.rows[..self.row_count]
            .iter()
            .position(|row| row.pid == pid)
    }

    fn find_previous(&self, pid: u32) -> u64 {
        self.previous[..self.previous_count]
            .iter()
            .find(|entry| entry.pid == pid)
            .map(|entry| entry.cpu_ticks)
            .unwrap_or(0)
    }

    fn set_notice(&mut self, now: u64, message: &str) {
        self.notice.set(message);
        self.notice_until = now.saturating_add(4_000);
    }

    fn notice_active(&self, now: u64) -> bool {
        self.notice_until > now && !self.notice.is_empty()
    }

    /// Samples the kernel and recomputes the percentages.
    fn sample(&mut self, platform: &Platform, now: u64) {
        let mut infos = [ProcessInfo::EMPTY; MAX_PROCESSES];
        let count = platform.processes.snapshot(&mut infos);
        let system = platform.processes.system();

        let total_delta = system.total_ticks.saturating_sub(self.last_total_ticks);
        let idle_delta = system.idle_ticks.saturating_sub(self.last_idle_ticks);
        let busy_delta = total_delta.saturating_sub(idle_delta);
        self.cpu_percent = percentage(busy_delta, total_delta);

        let hz = system.ticks_per_second.max(1);
        let mut rows = [Row::EMPTY; MAX_PROCESSES];
        let mut previous = [Previous::EMPTY; MAX_PROCESSES];
        let mut row_count = 0;
        let mut previous_count = 0;

        for info in infos.iter().take(count.min(MAX_PROCESSES)) {
            previous[previous_count] = Previous {
                pid: info.pid,
                cpu_ticks: info.cpu_ticks,
            };
            previous_count += 1;

            if info.state == ProcessState::Exited {
                continue;
            }

            let before = self.find_previous(info.pid);
            let delta = info.cpu_ticks.saturating_sub(before);
            rows[row_count] = Row {
                pid: info.pid,
                name: Text::from_str(info.name()),
                state: info.state,
                cpu_percent: percentage(delta, total_delta),
                cpu_ticks: info.cpu_ticks,
                cpu_time_ms: info.cpu_ticks.saturating_mul(1_000) / hz,
                memory_bytes: info.memory_bytes,
                threads: info.threads,
                priority: info.priority,
                started_ms: info.started_ms,
            };
            row_count += 1;
        }

        self.rows = rows;
        self.row_count = row_count;
        self.previous = previous;
        self.previous_count = previous_count;
        self.last_total_ticks = system.total_ticks;
        self.last_idle_ticks = system.idle_ticks;
        self.system = system;

        let memory = platform
            .memory
            .map(|memory| memory.info())
            .unwrap_or_default();
        self.memory = memory;
        let process_memory: usize = self.rows[..self.row_count]
            .iter()
            .map(|row| row.memory_bytes)
            .sum();
        self.commit_total = memory.heap_used_bytes.saturating_add(process_memory);
        self.commit_limit = memory.heap_total_bytes.saturating_add(process_memory);
        // "Memory Usage" is physical memory in use, not just the kernel heap:
        // the heap alone is usually a few hundred KiB against a 64 MiB arena,
        // which rounded to 0% and left the graph flat.
        self.memory_percent = percentage(memory.used_bytes as u64, memory.total_bytes as u64);
        if self.commit_total > self.commit_peak {
            self.commit_peak = self.commit_total;
        }

        self.sort_rows();
        self.clamp_selection();
        push_history(
            &mut self.cpu_history,
            &mut self.cpu_len,
            &mut self.cpu_head,
            self.cpu_percent,
        );
        push_history(
            &mut self.mem_history,
            &mut self.mem_len,
            &mut self.mem_head,
            self.memory_percent,
        );

        let online = system.cpus.clamp(1, MAX_CPUS);
        for cpu in 0..MAX_CPUS {
            let total = system.cpu_ticks[cpu];
            let idle = system.cpu_idle_ticks[cpu];
            let total_delta = total.saturating_sub(self.last_core_total_ticks[cpu]);
            let idle_delta = idle.saturating_sub(self.last_core_idle_ticks[cpu]);
            let percent = if cpu < online {
                percentage(total_delta.saturating_sub(idle_delta), total_delta)
            } else {
                0
            };

            self.last_core_total_ticks[cpu] = total;
            self.last_core_idle_ticks[cpu] = idle;
            self.cpu_core_percent[cpu] = percent;
            push_history(
                &mut self.cpu_core_history[cpu],
                &mut self.cpu_core_len[cpu],
                &mut self.cpu_core_head[cpu],
                percent,
            );
        }

        self.last_sample_ms = now;
    }

    fn clamp_selection(&mut self) {
        if self.row_count == 0 {
            self.selected_pid = None;
            self.cursor = 0;
            self.process_scroll = 0;
            self.app_scroll = 0;
            return;
        }
        if let Some(index) = self.selected_index() {
            self.cursor = index;
        } else {
            self.cursor = self.cursor.min(self.row_count - 1);
            self.selected_pid = Some(self.rows[self.cursor].pid);
        }
        if let Some(index) = self.selected_index() {
            self.cursor = index;
        }
    }

    fn sort_rows(&mut self) {
        let count = self.row_count;
        let mut data = self.rows;
        let mut index = 1;
        while index < count {
            let value = data[index];
            let mut position = index;
            while position > 0 && self.before(value, data[position - 1]) {
                data[position] = data[position - 1];
                position -= 1;
            }
            data[position] = value;
            index += 1;
        }
        self.rows = data;
    }

    fn before(&self, left: Row, right: Row) -> bool {
        let ordering = match self.sort {
            SortKey::Name => left.name.as_str().cmp(right.name.as_str()),
            SortKey::Pid => left.pid.cmp(&right.pid),
            SortKey::Cpu => left.cpu_percent.cmp(&right.cpu_percent),
            SortKey::CpuTime => left.cpu_ticks.cmp(&right.cpu_ticks),
            SortKey::Memory => left.memory_bytes.cmp(&right.memory_bytes),
            SortKey::Threads => left.threads.cmp(&right.threads),
            SortKey::Started => left.started_ms.cmp(&right.started_ms),
            SortKey::Status => left.status_name().cmp(right.status_name()),
            SortKey::Priority => left.priority.cmp(&right.priority),
        };
        let ordering = match ordering {
            Ordering::Equal => left.pid.cmp(&right.pid),
            value => value,
        };
        if self.sort_desc {
            ordering == Ordering::Greater
        } else {
            ordering == Ordering::Less
        }
    }

    fn set_sort(&mut self, key: SortKey) {
        if self.sort == key {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort = key;
            self.sort_desc = false;
        }
        self.sort_rows();
        self.clamp_selection();
    }

    fn move_cursor(&mut self, delta: i32) {
        if self.row_count == 0 {
            return;
        }
        let max = self.row_count.saturating_sub(1) as i32;
        let next = (self.cursor as i32 + delta).clamp(0, max) as usize;
        if next != self.cursor {
            self.cursor = next;
            self.selected_pid = Some(self.rows[next].pid);
        }
    }

    fn scroll_for(tab: Tab) -> bool {
        matches!(tab, Tab::Applications | Tab::Processes)
    }

    fn scroll_mut(&mut self, tab: Tab) -> &mut usize {
        match tab {
            Tab::Applications => &mut self.app_scroll,
            Tab::Processes => &mut self.process_scroll,
            Tab::Users => &mut self.user_scroll,
            _ => &mut self.user_scroll,
        }
    }

    fn scroll_by(&mut self, tab: Tab, delta: i32, visible_rows: usize) {
        let count = self.row_count;
        let max = count.saturating_sub(visible_rows);
        if max == 0 {
            *self.scroll_mut(tab) = 0;
            return;
        }
        let scroll = self.scroll_mut(tab);
        let next = (*scroll as i32 + delta).clamp(0, max as i32);
        *scroll = next as usize;
    }

    fn ensure_visible(&mut self, tab: Tab, visible_rows: usize) {
        if !Self::scroll_for(tab) || visible_rows == 0 {
            return;
        }
        let max = self.row_count.saturating_sub(visible_rows);
        let cursor = self.cursor;
        let scroll = self.scroll_mut(tab);
        if cursor < *scroll {
            *scroll = cursor;
        } else if cursor >= *scroll + visible_rows {
            *scroll = (cursor + 1).saturating_sub(visible_rows).min(max);
        }
        if *scroll > max {
            *scroll = max;
        }
    }

    fn end_selected(&mut self) {
        let Some(row) = self.selected_row().copied() else {
            return;
        };
        self.dialog = Dialog::EndProcess {
            pid: row.pid,
            tab: self.tab,
        };
    }

    fn confirm_end(&mut self, platform: &Platform, pid: u32, now: u64) {
        if pid == platform.processes.current() {
            self.set_notice(now, "Access is denied: Task Manager cannot end itself.");
            return;
        }
        let name = self
            .rows
            .iter()
            .find(|row| row.pid == pid)
            .map(|row| row.name)
            .unwrap_or_else(|| Text::from_str("process"));
        if platform.processes.kill(pid) {
            let mut message = Text::<160>::new();
            let _ = write!(message, "Ended '{}' (PID {pid}).", name.as_str());
            self.set_notice(now, message.as_str());
            self.sample(platform, now);
        } else {
            self.set_notice(
                now,
                "Unable to end the process. It may already have exited.",
            );
        }
    }

    fn launch(&mut self, platform: &Platform, now: u64) {
        let mut command = Text::<PROCESS_COMMAND_MAX>::new();
        command.set(self.run_text.as_str().trim());
        if command.is_empty() {
            self.set_notice(now, "Type the name of a program to run.");
            return;
        }

        let mut split = command.as_str().splitn(2, char::is_whitespace);
        let name = split.next().unwrap_or("");
        let args = split.next().unwrap_or("").trim();

        let Some(app) = platform.apps.find(name) else {
            let mut message = Text::<160>::new();
            let _ = write!(message, "Windows cannot find '{}'.", name);
            self.set_notice(now, message.as_str());
            return;
        };

        if app.foreground() {
            let mut message = Text::<160>::new();
            let _ = write!(
                message,
                "'{}' needs the desktop; close Task Manager and use the Start menu.",
                app.name()
            );
            self.set_notice(now, message.as_str());
            return;
        }

        match platform.processes.spawn(app, args) {
            Ok(pid) => {
                let mut message = Text::<160>::new();
                let _ = write!(message, "Started '{}' as PID {pid}.", app.name());
                self.set_notice(now, message.as_str());
                self.sample(platform, now);
            }
            Err(feos_app::SpawnError::NoFreeSlot) => {
                self.set_notice(now, "No free process slot is available.");
            }
            Err(feos_app::SpawnError::ArgsTooLong) => {
                self.set_notice(now, "The command line is too long.");
            }
        }
    }
}

/// Longest command accepted by the Run dialog.
const PROCESS_COMMAND_MAX: usize = 96;

fn percentage(part: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    ((part as u128 * 100) / total as u128).min(100) as u8
}

fn push_history(history: &mut [u8; HISTORY], len: &mut usize, head: &mut usize, value: u8) {
    history[*head] = value;
    *head = (*head + 1) % HISTORY;
    if *len < HISTORY {
        *len += 1;
    }
}

fn history_at(history: &[u8; HISTORY], len: usize, head: usize, index: usize) -> u8 {
    let start = if len == HISTORY { head } else { 0 };
    history[(start + index) % HISTORY]
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn format_duration(ms: u64) -> Text<16> {
    let total = ms / 1_000;
    let hours = total / 3_600;
    let minutes = (total / 60) % 60;
    let seconds = total % 60;
    let mut text = Text::<16>::new();
    if hours > 0 {
        let _ = write!(text, "{hours}:{minutes:02}:{seconds:02}");
    } else {
        let _ = write!(text, "{minutes}:{seconds:02}");
    }
    text
}

fn format_memory(bytes: usize) -> Text<20> {
    let mut text = Text::<20>::new();
    if bytes < 1024 {
        let _ = write!(text, "{bytes} B");
    } else if bytes < 1024 * 1024 {
        let _ = write!(text, "{} K", (bytes + 512) / 1024);
    } else {
        let tenths = (bytes as u64 * 10) / (1024 * 1024);
        let _ = write!(text, "{}.{} MB", tenths / 10, tenths % 10);
    }
    text
}

fn format_count(value: usize) -> Text<20> {
    let mut text = Text::<20>::new();
    let _ = write!(text, "{value}");
    text
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Layout {
    window: Rect,
    inner: Rect,
    title: Rect,
    menu_bar: Rect,
    tabs: Rect,
    content: Rect,
    status: Rect,
}

impl Layout {
    fn compute(screen: Rect, state: &State) -> Self {
        let window = if state.maximized {
            screen
        } else {
            Rect::new(
                state.window_x,
                state.window_y,
                state.window_w.max(720) as u32,
                state.window_h.max(480) as u32,
            )
        };
        let inner = if state.maximized {
            window
        } else {
            window.inset(3)
        };
        let title_height = if state.embedded { 0 } else { TITLE_HEIGHT };
        let (title, rest) = inner.split_top(title_height);
        let (menu_bar, rest) = rest.split_top(MENU_HEIGHT);
        let (tabs, rest) = rest.split_top(TABS_HEIGHT);
        let (status, content) = if state.show_status {
            rest.split_bottom(STATUS_HEIGHT)
        } else {
            (Rect::new(rest.x, rest.bottom_i32(), rest.width, 0), rest)
        };
        Self {
            window,
            inner,
            title,
            menu_bar,
            tabs,
            content,
            status,
        }
    }
}

#[derive(Clone, Copy)]
struct ListLayout {
    header: Rect,
    list: Rect,
    scroll: Rect,
    rows: Rect,
    buttons: Rect,
}

fn content_area(layout: Layout) -> Rect {
    layout.content.inset_each(8, 7, 8, 6)
}

fn list_layout(layout: Layout) -> ListLayout {
    let area = content_area(layout);
    let (buttons, body) = area.split_bottom(30);
    let (header, list_area) = body.split_top(HEADER_HEIGHT);
    let (scroll, rows) = list_area.split_right(SCROLL_WIDTH);
    ListLayout {
        header,
        list: list_area,
        scroll,
        rows,
        buttons,
    }
}

fn button_rect(area: Rect, index: usize, total: usize) -> Rect {
    const WIDTH: i32 = 88;
    const HEIGHT: i32 = 24;
    const GAP: i32 = 8;
    let right = area.right_i32() - 8;
    let x = right - (total as i32) * (WIDTH + GAP) + GAP + (index as i32) * (WIDTH + GAP);
    Rect::new(
        x,
        area.bottom_i32() - HEIGHT - 3,
        WIDTH as u32,
        HEIGHT as u32,
    )
}

fn menu_title_width(menu: Menu) -> i32 {
    menu.title().len() as i32 * 8 + 18
}

fn menu_title_rect(layout: Layout, index: usize) -> Rect {
    let mut x = layout.menu_bar.x + 4;
    for (position, menu) in Menu::ALL.iter().enumerate() {
        let width = menu_title_width(*menu);
        if position == index {
            return Rect::new(x, layout.menu_bar.y, width as u32, layout.menu_bar.height);
        }
        x += width;
    }
    Rect::new(x, layout.menu_bar.y, 0, layout.menu_bar.height)
}

fn tab_rects(layout: Layout) -> [Rect; 5] {
    let mut rects = [Rect::new(0, 0, 0, 0); 5];
    let mut x = layout.tabs.x + 8;
    for (index, tab) in Tab::ALL.iter().enumerate() {
        let width = tab.title().len() as i32 * 8 + 22;
        rects[index] = Rect::new(x, layout.tabs.y, width as u32, layout.tabs.height);
        x += width + 2;
    }
    rects
}

fn dialog_panel(layout: Layout, dialog: Dialog) -> Rect {
    let (width, height) = match dialog {
        Dialog::Run => (470, 220),
        Dialog::About => (540, 360),
        Dialog::Columns => (330, 470),
        Dialog::Help => (560, 440),
        Dialog::Properties(_) => (480, 400),
        Dialog::EndProcess { .. } => (440, 200),
        Dialog::LogOff => (440, 200),
        Dialog::None => (1, 1),
    };
    Rect::new(0, 0, width, height).center_in(layout.window.inset(12))
}

fn dialog_buttons(dialog: Dialog) -> &'static [&'static str] {
    match dialog {
        Dialog::Run => &["OK", "Cancel", "Browse..."],
        Dialog::Columns => &["OK", "Cancel"],
        Dialog::EndProcess { .. } => &["Yes", "No"],
        Dialog::LogOff => &["Log Off", "Cancel"],
        Dialog::About | Dialog::Help | Dialog::Properties(_) => &["OK"],
        Dialog::None => &[],
    }
}

fn dialog_button_rect(panel: Rect, index: usize, total: usize) -> Rect {
    const WIDTH: i32 = 76;
    const HEIGHT: i32 = 24;
    const GAP: i32 = 8;
    let total_width = total as i32 * WIDTH + (total as i32 - 1) * GAP;
    let x = panel.right_i32() - 12 - total_width + index as i32 * (WIDTH + GAP);
    Rect::new(x, panel.bottom_i32() - 36, WIDTH as u32, HEIGHT as u32)
}

// ---------------------------------------------------------------------------
// Run loop
// ---------------------------------------------------------------------------

/// Pixels saved around the pointer so the cursor can be restored. The box is
/// large enough for both the arrow and the wider resize cursors.
const CURSOR_BOX: i32 = 21;
const CURSOR_PIXELS: usize = 24 * 24;

/// A hardware cursor composited directly onto the front buffer.
///
/// Moving the mouse normally only needs this small restore/save/draw cycle
/// instead of repainting the whole window. A full frame invalidates the saved
/// background because every pixel changed underneath it.
struct Cursor {
    saved: bool,
    rect: Rect,
    pixels: [u32; CURSOR_PIXELS],
}

impl Cursor {
    const fn new() -> Self {
        Self {
            saved: false,
            rect: Rect::new(0, 0, 0, 0),
            pixels: [0; CURSOR_PIXELS],
        }
    }

    fn invalidate(&mut self) {
        self.saved = false;
    }

    fn restore(&mut self, canvas: &mut Canvas<'_>) {
        if !self.saved {
            return;
        }
        let width = self.rect.width as i32;
        for row in 0..self.rect.height as i32 {
            for column in 0..width {
                let index = (row * width + column) as usize;
                canvas.put_pixel(
                    self.rect.x + column,
                    self.rect.y + row,
                    Color::from_u32(self.pixels[index]),
                );
            }
        }
        self.saved = false;
    }

    fn move_to(&mut self, canvas: &mut Canvas<'_>, pointer: Point, shape: feos_gui::CursorShape) {
        self.restore(canvas);
        let bounds = Rect::new(0, 0, canvas.width() as u32, canvas.height() as u32);
        // The arrow grows right/down from its tip; resize cursors are centred.
        let origin = match shape {
            feos_gui::CursorShape::Arrow => pointer,
            _ => Point::new(pointer.x - 10, pointer.y - 10),
        };
        let rect =
            Rect::new(origin.x, origin.y, CURSOR_BOX as u32, CURSOR_BOX as u32).clipped_to(bounds);
        if rect.is_empty() {
            return;
        }

        let width = rect.width as i32;
        for row in 0..rect.height as i32 {
            for column in 0..width {
                let color = canvas
                    .pixel(rect.x + column, rect.y + row)
                    .unwrap_or(Color::BLACK);
                self.pixels[(row * width + column) as usize] = color.raw();
            }
        }
        self.rect = rect;
        self.saved = true;

        let mut painter = Painter::new(canvas);
        feos_gui::draw_cursor_shape(&mut painter, pointer.x, pointer.y, shape);
    }
}

fn state_cursor_shape(state: &State) -> feos_gui::CursorShape {
    if state.menu.is_some()
        || state.context.is_some()
        || state.dialog != Dialog::None
        || state.maximized
    {
        return feos_gui::CursorShape::Arrow;
    }
    let window = Rect::new(
        state.window_x,
        state.window_y,
        state.window_w.max(1) as u32,
        state.window_h.max(1) as u32,
    );
    resize_edge_at(window, state.pointer)
        .map(resize_cursor)
        .unwrap_or(feos_gui::CursorShape::Arrow)
}

/// State of a task manager hosted inside a desktop window.
static WINDOW: SpinLock<Option<State>> = SpinLock::new(None);

pub(super) fn open_task_manager(context: &WindowContext<'_, '_>) -> bool {
    let screen = Rect::new(
        0,
        0,
        context.canvas.width() as u32,
        context.canvas.height() as u32,
    );
    let mut state = State::new(screen);
    state.has_pointer = true;
    state.pointer = context.pointer();
    state.embedded = true;
    state.maximized = true;
    state.sample(context.platform, context.now_ms);
    *WINDOW.lock() = Some(state);
    true
}

pub(super) fn frame_task_manager(context: &mut WindowContext<'_, '_>) -> WindowFrame {
    let mut guard = WINDOW.lock();
    let Some(state) = guard.as_mut() else {
        return WindowFrame::Close;
    };
    let platform = context.platform;
    let now = context.now_ms;

    context.dispatch_pointer(|report| state.handle_pointer(platform, report, now));
    context.dispatch_keys(|event| match event {
        Event::KeyDown(key, modifiers) => state.handle_key(platform, key, modifiers, now),
        Event::Text(ch) => state.handle_text(platform, ch, now),
        _ => {}
    });

    if state.exit {
        return WindowFrame::Close;
    }

    let due = state.update != UpdateSpeed::Paused
        && now.wrapping_sub(state.last_sample_ms) >= state.update.interval_ms();
    if due {
        state.sample(platform, now);
    }
    if state.notice_until != 0 && now >= state.notice_until {
        state.notice.clear();
        state.notice_until = 0;
    }

    render(context.canvas, platform, state, now);
    WindowFrame::Continue
}

pub(super) fn close_task_manager() {
    *WINDOW.lock() = None;
}

fn run(context: &mut Context<'_>) {
    let platform = context.platform;
    let suspend = context.suspend;
    let suspend_area = context.suspend_area;
    let Some(display) = context.display.as_mut() else {
        println!("taskmgr: no framebuffer; run 'ps' from the serial shell instead");
        return;
    };

    let full = Rect::new(0, 0, display.width() as u32, display.height() as u32);
    let view = context.viewport.unwrap_or(full).clipped_to(full);
    if view.is_empty() {
        return;
    }
    let screen = Rect::new(0, 0, view.width, view.height);
    let mut state = State::new(screen);
    state.has_pointer = platform.pointer.is_some();
    if let Some(pointer) = platform.pointer {
        let (x, y) = pointer.position();
        state.pointer = Point::new(x as i32 - view.x, y as i32 - view.y);
    }

    let width = view.width as usize;
    let height = view.height as usize;
    let mut back_pixels = vec![0u32; width * height];
    let mut back = Canvas::new(&mut back_pixels, width, height, width);

    let mut input = Input::new();
    let mut keys = SerialKeys::new();
    let mut cursor = Cursor::new();
    let mut dirty = true;

    state.sample(platform, platform.clock.uptime_ms());
    draw_frame(
        display,
        back.as_mut(),
        platform,
        &mut state,
        platform.clock.uptime_ms(),
        &mut cursor,
        view,
    );

    loop {
        let now = platform.clock.uptime_ms();
        if keys.poll(now, &mut input) {
            break;
        }

        let mut pointer_full = false;
        let mut cursor_only = false;
        let mut suspend_requested = false;
        if let Some(pointer) = platform.pointer {
            let mut budget = 32;
            while budget > 0 {
                let Some(mut report) = pointer.poll() else {
                    break;
                };
                if let (Some(cell), Some(area), Some(true)) = (suspend, suspend_area, report.left) {
                    if area.contains(Point::new(report.x as i32, report.y as i32)) {
                        cell.set(true);
                        suspend_requested = true;
                        break;
                    }
                }
                report.x = report.x.saturating_sub(view.x as u16);
                report.y = report.y.saturating_sub(view.y as u16);
                let interactive = report.left.is_some()
                    || report.right.is_some()
                    || report.middle.is_some()
                    || report.wheel != 0;
                let before = state.hover_signature();
                state.handle_pointer(platform, report, now);
                let after = state.hover_signature();
                if interactive
                    || state.mouse_down
                    || state.drag != DragMode::None
                    || before != after
                {
                    pointer_full = true;
                } else {
                    cursor_only = true;
                }
                budget -= 1;
            }
        }
        if suspend_requested {
            break;
        }

        let input_changed = !input.is_empty();
        if input_changed {
            for event in input.events() {
                match event {
                    Event::KeyDown(key, modifiers) => {
                        state.handle_key(platform, key, modifiers, now);
                    }
                    Event::Text(ch) => state.handle_text(platform, ch, now),
                    Event::Resize(size) => {
                        state.set_screen(Rect::new(0, 0, size.width, size.height));
                    }
                    _ => {}
                }
            }
        }

        if state.exit {
            break;
        }

        let due = state.update != UpdateSpeed::Paused
            && now.wrapping_sub(state.last_sample_ms) >= state.update.interval_ms();
        if due {
            state.sample(platform, now);
            dirty = true;
        }

        if state.notice_until != 0 && now >= state.notice_until {
            state.notice.clear();
            state.notice_until = 0;
            dirty = true;
        }

        if dirty || input_changed || pointer_full {
            draw_frame(
                display,
                back.as_mut(),
                platform,
                &mut state,
                now,
                &mut cursor,
                view,
            );
            dirty = false;
        } else if cursor_only && state.has_pointer {
            if Rect::new(0, 0, view.width, view.height).contains(state.pointer) {
                cursor.move_to(
                    display,
                    Point::new(state.pointer.x + view.x, state.pointer.y + view.y),
                    state_cursor_shape(&state),
                );
            } else {
                // The pointer is over the taskbar. Restore the last cursor
                // pixels and leave the taskbar untouched.
                cursor.restore(display);
            }
        }

        input.clear_events();
        platform.clock.idle_ms(10);
    }

    if !state.exit_message.is_empty() {
        println!("[taskmgr] {}", state.exit_message);
    }
    println!("[taskmgr] closed");
}

fn draw_frame(
    front: &mut Canvas<'static>,
    back: Option<&mut Canvas<'_>>,
    platform: &Platform,
    state: &mut State,
    now: u64,
    cursor: &mut Cursor,
    view: Rect,
) {
    cursor.invalidate();
    if let Some(back) = back {
        render(back, platform, state, now);
        front.blit(
            view.x,
            view.y,
            back.width() as u32,
            back.height() as u32,
            back.pixels(),
        );
    } else {
        render(front, platform, state, now);
    }
    if Rect::new(0, 0, view.width, view.height).contains(state.pointer) {
        cursor.move_to(
            front,
            Point::new(state.pointer.x + view.x, state.pointer.y + view.y),
            state_cursor_shape(state),
        );
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

impl State {
    fn handle_pointer(&mut self, platform: &Platform, report: PointerEvent, now: u64) {
        self.pointer = Point::new(report.x as i32, report.y as i32);
        self.has_pointer = true;

        if self.mouse_down {
            match self.drag {
                DragMode::Move { dx, dy } => {
                    if !self.maximized {
                        self.window_x = self.pointer.x - dx;
                        self.window_y = self.pointer.y - dy;
                        self.clamp_window();
                    }
                }
                DragMode::Resize {
                    edge,
                    x,
                    y,
                    width,
                    height,
                    pointer_x,
                    pointer_y,
                } => {
                    if !self.maximized {
                        const MIN_WIDTH: i32 = 720;
                        const MIN_HEIGHT: i32 = 480;
                        let dx = self.pointer.x - pointer_x;
                        let dy = self.pointer.y - pointer_y;
                        let max_right = self.screen.width as i32;
                        let max_bottom = self.screen.height as i32;
                        let mut left = x;
                        let mut top = y;
                        let mut right = (x + width).min(max_right);
                        let mut bottom = (y + height).min(max_bottom);

                        if edge.moves_left() {
                            left = (x + dx).clamp(0, right - MIN_WIDTH);
                        } else if edge.moves_right() {
                            right = (x + width + dx).clamp(left + MIN_WIDTH, max_right);
                        }
                        if edge.moves_top() {
                            top = (y + dy).clamp(0, bottom - MIN_HEIGHT);
                        } else if edge.moves_bottom() {
                            bottom = (y + height + dy).clamp(top + MIN_HEIGHT, max_bottom);
                        }

                        self.window_x = left;
                        self.window_y = top;
                        self.window_w = (right - left).max(1);
                        self.window_h = (bottom - top).max(1);
                        self.clamp_window();
                    }
                }
                DragMode::None => {}
            }
        }

        if let Some(down) = report.left {
            if down {
                self.on_left_down(platform, now);
            } else {
                self.mouse_down = false;
                self.drag = DragMode::None;
            }
        }
        if let Some(down) = report.right {
            if down {
                self.on_right_down(now);
            }
        }
        if report.wheel != 0 {
            self.on_wheel(report.wheel);
        }
    }

    fn clamp_window(&mut self) {
        self.window_w = self
            .window_w
            .clamp(720, self.screen.width as i32)
            .min(self.screen.width as i32);
        self.window_h = self
            .window_h
            .clamp(480, self.screen.height as i32)
            .min(self.screen.height as i32);
        self.window_x = self
            .window_x
            .clamp(0, (self.screen.width as i32 - self.window_w).max(0));
        self.window_y = self
            .window_y
            .clamp(0, (self.screen.height as i32 - self.window_h).max(0));
    }

    fn on_left_down(&mut self, platform: &Platform, now: u64) {
        self.mouse_down = true;
        let layout = Layout::compute(self.screen, self);

        if self.dialog != Dialog::None {
            self.handle_dialog_click(platform, layout, now);
            return;
        }

        if self.context.is_some() {
            if self.handle_context_click(platform, layout, now) {
                return;
            }
            self.context = None;
            return;
        }

        if self.menu.is_some() {
            for index in 0..Menu::ALL.len() {
                if menu_title_rect(layout, index).contains(self.pointer) {
                    let target = Menu::ALL[index];
                    self.menu = if self.menu == Some(target) {
                        None
                    } else {
                        Some(target)
                    };
                    self.menu_hover = 0;
                    return;
                }
            }
            let Some(menu) = self.menu else {
                return;
            };
            if self.handle_menu_click(platform, layout, menu, now) {
                return;
            }
            self.menu = None;
            return;
        }

        // Title bar buttons.
        if !self.embedded {
            for index in 0..3 {
                if title_button_rect(layout, index).contains(self.pointer) {
                    match index {
                        0 => {
                            self.exit = true;
                            self.exit_message = "minimized";
                        }
                        1 => self.maximized = !self.maximized,
                        _ => {
                            self.exit = true;
                            self.exit_message = "";
                        }
                    }
                    return;
                }
            }
        }

        // Resize edges and corners.
        if !self.maximized {
            if let Some(edge) = resize_edge_at(layout.window, self.pointer) {
                self.drag = DragMode::Resize {
                    edge,
                    x: self.window_x,
                    y: self.window_y,
                    width: self.window_w,
                    height: self.window_h,
                    pointer_x: self.pointer.x,
                    pointer_y: self.pointer.y,
                };
                return;
            }
        }

        // Title bar drag.
        if layout.title.contains(self.pointer) {
            let double = self.title_click_ms != 0 && now.wrapping_sub(self.title_click_ms) <= 500;
            self.title_click_ms = now;
            if double {
                self.maximized = !self.maximized;
                self.drag = DragMode::None;
                return;
            }
            if !self.maximized {
                self.drag = DragMode::Move {
                    dx: self.pointer.x - self.window_x,
                    dy: self.pointer.y - self.window_y,
                };
            }
            return;
        }

        // Menu bar.
        for index in 0..Menu::ALL.len() {
            if menu_title_rect(layout, index).contains(self.pointer) {
                let menu = Menu::ALL[index];
                if self.menu == Some(menu) {
                    self.menu = None;
                } else {
                    self.menu = Some(menu);
                    self.menu_hover = 0;
                }
                return;
            }
        }

        // Tab row.
        for (index, rect) in tab_rects(layout).iter().enumerate() {
            if rect.contains(self.pointer) {
                self.tab = Tab::from_index(index);
                self.context = None;
                return;
            }
        }

        if !layout.content.contains(self.pointer) {
            return;
        }

        match self.tab {
            Tab::Applications => self.handle_applications_click(layout, now),
            Tab::Processes => self.handle_processes_click(layout, now),
            Tab::Users => self.handle_users_click(platform, layout, now),
            _ => {}
        }
    }

    fn on_right_down(&mut self, now: u64) {
        let layout = Layout::compute(self.screen, self);
        if self.dialog != Dialog::None {
            return;
        }
        if !matches!(self.tab, Tab::Applications | Tab::Processes) {
            self.context = None;
            return;
        }
        let list = list_layout(layout);
        let scroll = state_scroll(self, self.tab);
        if let Some(index) = row_at(list.rows, self.pointer, scroll) {
            if index < self.row_count {
                let pid = self.rows[index].pid;
                self.selected_pid = Some(pid);
                self.cursor = index;
                self.context = Some(ContextMenu {
                    tab: self.tab,
                    x: self.pointer.x,
                    y: self.pointer.y,
                    hover: 0,
                });
                self.menu = None;
                return;
            }
        }
        self.context = None;
        self.menu = None;
        let _ = now;
    }

    fn on_wheel(&mut self, delta: i32) {
        let layout = Layout::compute(self.screen, self);
        if self.dialog == Dialog::Help {
            self.dialog_scroll = (self.dialog_scroll as i32 - delta * 2).max(0) as usize;
            return;
        }
        if self.dialog != Dialog::None {
            return;
        }
        let list = list_layout(layout);
        let visible = visible_rows(list.rows);
        match self.tab {
            Tab::Applications => {
                self.scroll_by(Tab::Applications, -delta, visible);
            }
            Tab::Processes => {
                self.scroll_by(Tab::Processes, -delta, visible);
            }
            Tab::Users => {}
            _ => {}
        }
    }

    fn handle_applications_click(&mut self, layout: Layout, now: u64) {
        let list = list_layout(layout);
        if let Some(index) = row_at(list.rows, self.pointer, self.app_scroll) {
            if index < self.row_count {
                let pid = self.rows[index].pid;
                let double =
                    self.last_click_pid == Some(pid) && now.wrapping_sub(self.last_click_ms) <= 500;
                self.selected_pid = Some(pid);
                self.cursor = index;
                self.last_click_pid = Some(pid);
                self.last_click_ms = now;
                if double {
                    self.switch_to(now);
                }
            }
            return;
        }
        if button_rect(list.buttons, 0, 3).contains(self.pointer) {
            self.end_selected();
        } else if button_rect(list.buttons, 1, 3).contains(self.pointer) {
            self.switch_to(now);
        } else if button_rect(list.buttons, 2, 3).contains(self.pointer) {
            self.run_text.clear();
            self.dialog = Dialog::Run;
        }
    }

    fn handle_processes_click(&mut self, layout: Layout, now: u64) {
        let list = list_layout(layout);
        if let Some(index) = column_at(list.header, self.pointer, self) {
            if let Some(key) = Column::ALL[index].sort_key() {
                self.set_sort(key);
            }
            return;
        }
        if let Some(index) = row_at(list.rows, self.pointer, self.process_scroll) {
            if index < self.row_count {
                let pid = self.rows[index].pid;
                let double =
                    self.last_click_pid == Some(pid) && now.wrapping_sub(self.last_click_ms) <= 500;
                self.selected_pid = Some(pid);
                self.cursor = index;
                self.last_click_pid = Some(pid);
                self.last_click_ms = now;
                if double {
                    self.dialog = Dialog::Properties(pid);
                }
            }
            return;
        }
        if button_rect(list.buttons, 0, 2).contains(self.pointer) {
            self.end_selected();
        } else if button_rect(list.buttons, 1, 2).contains(self.pointer) {
            if let Some(row) = self.selected_row() {
                self.dialog = Dialog::Properties(row.pid);
            }
        }
    }

    fn handle_users_click(&mut self, platform: &Platform, layout: Layout, now: u64) {
        let list = list_layout(layout);
        if button_rect(list.buttons, 0, 2).contains(self.pointer) {
            self.set_notice(now, "The console session cannot be disconnected.");
        } else if button_rect(list.buttons, 1, 2).contains(self.pointer) {
            let _ = platform;
            self.dialog = Dialog::LogOff;
        }
    }

    fn switch_to(&mut self, now: u64) {
        if let Some(row) = self.selected_row() {
            if row.pid == 1 {
                self.set_notice(now, "Task Manager is already the active task.");
                return;
            }
            self.exit = true;
            self.exit_message = "switching to the desktop";
        } else {
            self.set_notice(now, "Select a task to switch to.");
        }
    }

    fn handle_menu_click(
        &mut self,
        platform: &Platform,
        layout: Layout,
        menu: Menu,
        now: u64,
    ) -> bool {
        let panel = menu_panel_rect(self, layout, menu);
        if !panel.contains(self.pointer) {
            return false;
        }
        let mut items = [MenuItem::new("", Action::None, false); 12];
        let count = self.menu_items(menu, &mut items);
        let mut y = panel.y + 4;
        for item in items.iter().take(count) {
            let height = if item.separator { 8 } else { MENU_ROW };
            let rect = Rect::new(panel.x, y, panel.width, height as u32);
            y += height;
            if item.separator || !rect.contains(self.pointer) {
                continue;
            }
            if item.enabled {
                self.context = None;
                self.menu = None;
                self.execute_action(platform, item.action, now);
            }
            return true;
        }
        true
    }

    fn handle_context_click(&mut self, platform: &Platform, layout: Layout, now: u64) -> bool {
        let Some(context) = self.context else {
            return false;
        };
        let panel = context_panel_rect(layout, context);
        if !panel.contains(self.pointer) {
            return false;
        }
        let items = context_items(context.tab);
        let index = ((self.pointer.y - panel.y - 4) / MENU_ROW) as usize;
        if index < items.len() {
            let action = items[index].1;
            self.context = None;
            self.execute_action(platform, action, now);
        }
        true
    }

    fn handle_dialog_click(&mut self, platform: &Platform, layout: Layout, now: u64) {
        let dialog = self.dialog;
        let panel = dialog_panel(layout, dialog);
        if !panel.contains(self.pointer) {
            return;
        }
        let buttons = dialog_buttons(dialog);
        for index in 0..buttons.len() {
            if dialog_button_rect(panel, index, buttons.len()).contains(self.pointer) {
                self.activate_dialog_button(platform, index, now);
                return;
            }
        }
        let title = Rect::new(panel.x + 1, panel.y + 1, panel.width.saturating_sub(2), 25);
        if Rect::new(title.right_i32() - 22, title.y + 4, 16, 16).contains(self.pointer) {
            if dialog == Dialog::Columns {
                self.column_visible = self.column_backup;
            }
            self.dialog = Dialog::None;
            return;
        }
        if dialog == Dialog::Columns {
            let body = Rect::new(
                panel.x + 14,
                panel.y + 34,
                panel.width.saturating_sub(28),
                panel.height.saturating_sub(76),
            );
            let first = body.y + 22;
            let column = ((self.pointer.y - first) / DIALOG_ROW) as usize;
            if self.pointer.y >= first && column < COLUMN_COUNT {
                let visible = self.column_visible.iter().filter(|value| **value).count();
                if self.column_visible[column] && visible <= 1 {
                    self.set_notice(now, "At least one column must remain visible.");
                } else {
                    self.column_visible[column] = !self.column_visible[column];
                }
            }
        }
    }

    fn activate_dialog_button(&mut self, platform: &Platform, index: usize, now: u64) {
        match self.dialog {
            Dialog::Run => {
                if index == 0 {
                    self.dialog = Dialog::None;
                    self.launch(platform, now);
                } else if index == 1 {
                    self.dialog = Dialog::None;
                } else {
                    self.set_notice(now, "Browse is unavailable in this build.");
                }
            }
            Dialog::Columns => {
                if index == 0 {
                    self.dialog = Dialog::None;
                } else {
                    self.column_visible = self.column_backup;
                    self.dialog = Dialog::None;
                }
            }
            Dialog::EndProcess { pid, .. } => {
                self.dialog = Dialog::None;
                if index == 0 {
                    self.confirm_end(platform, pid, now);
                }
            }
            Dialog::LogOff => {
                if index == 0 {
                    self.dialog = Dialog::None;
                    self.exit = true;
                    self.exit_message = "logging off to the desktop";
                } else {
                    self.dialog = Dialog::None;
                }
            }
            Dialog::About | Dialog::Help | Dialog::Properties(_) | Dialog::None => {
                self.dialog = Dialog::None;
            }
        }
    }

    fn handle_key(&mut self, platform: &Platform, key: Key, modifiers: Modifiers, now: u64) {
        if self.dialog != Dialog::None {
            self.handle_dialog_key(platform, key, modifiers, now);
            return;
        }

        if self.menu.is_some() {
            match key {
                Key::Escape => self.menu = None,
                Key::Up => {
                    let count = self.menu_item_count(self.menu.unwrap_or(Menu::File));
                    self.menu_hover = (self.menu_hover + count - 1) % count.max(1);
                }
                Key::Down => {
                    let count = self.menu_item_count(self.menu.unwrap_or(Menu::File));
                    self.menu_hover = (self.menu_hover + 1) % count.max(1);
                }
                Key::Enter => {
                    let menu = self.menu.unwrap_or(Menu::File);
                    let mut items = [MenuItem::new("", Action::None, false); 12];
                    let count = self.menu_items(menu, &mut items);
                    let index = self.menu_hover.min(count.saturating_sub(1));
                    if let Some(item) = items.get(index) {
                        if item.enabled && !item.separator {
                            let action = item.action;
                            self.menu = None;
                            self.execute_action(platform, action, now);
                        }
                    }
                }
                Key::Left => {
                    let current = self.menu.unwrap_or(Menu::File);
                    let index = Menu::ALL
                        .iter()
                        .position(|menu| *menu == current)
                        .unwrap_or(0);
                    self.menu = Some(Menu::ALL[(index + Menu::ALL.len() - 1) % Menu::ALL.len()]);
                    self.menu_hover = 0;
                }
                Key::Right => {
                    let current = self.menu.unwrap_or(Menu::File);
                    let index = Menu::ALL
                        .iter()
                        .position(|menu| *menu == current)
                        .unwrap_or(0);
                    self.menu = Some(Menu::ALL[(index + 1) % Menu::ALL.len()]);
                    self.menu_hover = 0;
                }
                _ => {}
            }
            return;
        }

        if self.context.is_some() {
            match key {
                Key::Escape => self.context = None,
                Key::Up => {
                    if let Some(context) = self.context.as_mut() {
                        context.hover = context.hover.saturating_sub(1);
                    }
                }
                Key::Down => {
                    if let Some(context) = self.context.as_mut() {
                        context.hover =
                            (context.hover + 1).min(context_items(context.tab).len() - 1);
                    }
                }
                Key::Enter => {
                    if let Some(context) = self.context {
                        let items = context_items(context.tab);
                        let index = context.hover.min(items.len() - 1);
                        let action = items[index].1;
                        self.context = None;
                        self.execute_action(platform, action, now);
                    }
                }
                _ => {}
            }
            return;
        }

        match key {
            Key::Escape => {
                self.exit = true;
                self.exit_message = "";
            }
            Key::Tab => {
                let next = if modifiers.contains(Modifiers::SHIFT) {
                    (self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()
                } else {
                    (self.tab.index() + 1) % Tab::ALL.len()
                };
                self.tab = Tab::from_index(next);
            }
            Key::Left if modifiers.contains(Modifiers::CTRL) => {
                self.tab =
                    Tab::from_index((self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len());
            }
            Key::Right if modifiers.contains(Modifiers::CTRL) => {
                self.tab = Tab::from_index((self.tab.index() + 1) % Tab::ALL.len());
            }
            Key::F1 => {
                self.dialog = Dialog::Help;
                self.dialog_scroll = 0;
            }
            Key::F5 => self.sample(platform, now),
            Key::F10 => {
                self.menu = Some(Menu::File);
                self.menu_hover = 0;
            }
            Key::Up => {
                self.move_cursor(-1);
                let visible = visible_rows(list_layout(Layout::compute(self.screen, self)).rows);
                self.ensure_visible(self.tab, visible);
            }
            Key::Down => {
                self.move_cursor(1);
                let visible = visible_rows(list_layout(Layout::compute(self.screen, self)).rows);
                self.ensure_visible(self.tab, visible);
            }
            Key::Home => {
                self.move_cursor(-(self.row_count as i32));
                let visible = visible_rows(list_layout(Layout::compute(self.screen, self)).rows);
                self.ensure_visible(self.tab, visible);
            }
            Key::End => {
                self.move_cursor(self.row_count as i32);
                let visible = visible_rows(list_layout(Layout::compute(self.screen, self)).rows);
                self.ensure_visible(self.tab, visible);
            }
            Key::PageUp => {
                let visible = visible_rows(list_layout(Layout::compute(self.screen, self)).rows);
                self.move_cursor(-(visible as i32));
                self.ensure_visible(self.tab, visible);
            }
            Key::PageDown => {
                let visible = visible_rows(list_layout(Layout::compute(self.screen, self)).rows);
                self.move_cursor(visible as i32);
                self.ensure_visible(self.tab, visible);
            }
            Key::Delete => {
                if matches!(self.tab, Tab::Applications | Tab::Processes) {
                    self.end_selected();
                }
            }
            Key::Enter => {
                if self.tab == Tab::Applications {
                    self.switch_to(now);
                } else if self.tab == Tab::Processes {
                    if let Some(row) = self.selected_row() {
                        self.dialog = Dialog::Properties(row.pid);
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_dialog_key(
        &mut self,
        platform: &Platform,
        key: Key,
        _modifiers: Modifiers,
        now: u64,
    ) {
        match self.dialog {
            Dialog::Run => match key {
                Key::Escape => self.dialog = Dialog::None,
                Key::Enter => {
                    self.dialog = Dialog::None;
                    self.launch(platform, now);
                }
                Key::Backspace => {
                    self.run_text.backspace();
                }
                Key::Delete => {
                    self.run_text.delete();
                }
                Key::Left => self.run_text.move_left(false),
                Key::Right => self.run_text.move_right(false),
                Key::Home => self.run_text.set_cursor(0),
                Key::End => self.run_text.set_cursor(self.run_text.len()),
                _ => {}
            },
            Dialog::Help => match key {
                Key::Escape => self.dialog = Dialog::None,
                Key::Up => self.dialog_scroll = self.dialog_scroll.saturating_sub(1),
                Key::Down => self.dialog_scroll = self.dialog_scroll.saturating_add(1),
                Key::PageUp => self.dialog_scroll = self.dialog_scroll.saturating_sub(10),
                Key::PageDown => self.dialog_scroll = self.dialog_scroll.saturating_add(10),
                Key::Enter => self.dialog = Dialog::None,
                _ => {}
            },
            Dialog::Columns => match key {
                Key::Escape => {
                    self.column_visible = self.column_backup;
                    self.dialog = Dialog::None;
                }
                Key::Enter => self.dialog = Dialog::None,
                _ => {}
            },
            _ => {
                if key == Key::Escape {
                    self.dialog = Dialog::None;
                } else if key == Key::Enter {
                    self.activate_dialog_button(platform, 0, now);
                }
            }
        }
    }

    fn handle_text(&mut self, _platform: &Platform, ch: char, _now: u64) {
        if self.dialog == Dialog::Run {
            self.run_text.insert(ch);
        }
    }

    fn menu_item_count(&self, menu: Menu) -> usize {
        let mut items = [MenuItem::new("", Action::None, false); 12];
        self.menu_items(menu, &mut items)
    }

    fn menu_items(&self, menu: Menu, out: &mut [MenuItem; 12]) -> usize {
        let selected = self.selected_pid.is_some();
        let on_processes = self.tab == Tab::Processes;
        let on_applications = self.tab == Tab::Applications;
        let mut count = 0;
        match menu {
            Menu::File => {
                out[count] = MenuItem::new("New Task (Run...)", Action::NewTask, true);
                count += 1;
                out[count] = MenuItem::separator();
                count += 1;
                out[count] =
                    MenuItem::new("End Task", Action::EndTask, on_applications && selected);
                count += 1;
                out[count] =
                    MenuItem::new("End Process", Action::EndProcess, on_processes && selected);
                count += 1;
                out[count] = MenuItem::separator();
                count += 1;
                out[count] = MenuItem::new("Close", Action::Close, true);
                count += 1;
            }
            Menu::Options => {
                out[count] =
                    MenuItem::checked("Always On Top", Action::AlwaysOnTop, self.always_on_top);
                count += 1;
                out[count] = MenuItem::checked(
                    "Minimize on Use",
                    Action::MinimizeOnUse,
                    self.minimize_on_use,
                );
                count += 1;
                out[count] = MenuItem::checked(
                    "Hide When Minimized",
                    Action::HideWhenMinimized,
                    self.hide_when_minimized,
                );
                count += 1;
                out[count] = MenuItem::separator();
                count += 1;
                out[count] = MenuItem::checked(
                    "Update Speed: High",
                    Action::SpeedHigh,
                    self.update == UpdateSpeed::High,
                );
                count += 1;
                out[count] = MenuItem::checked(
                    "Update Speed: Normal",
                    Action::SpeedNormal,
                    self.update == UpdateSpeed::Normal,
                );
                count += 1;
                out[count] = MenuItem::checked(
                    "Update Speed: Low",
                    Action::SpeedLow,
                    self.update == UpdateSpeed::Low,
                );
                count += 1;
                out[count] = MenuItem::checked(
                    "Update Speed: Paused",
                    Action::SpeedPaused,
                    self.update == UpdateSpeed::Paused,
                );
                count += 1;
            }
            Menu::View => {
                out[count] = MenuItem::new("Refresh Now", Action::Refresh, true);
                count += 1;
                out[count] = MenuItem::new("Select Columns...", Action::SelectColumns, true);
                count += 1;
                out[count] = MenuItem::separator();
                count += 1;
                out[count] =
                    MenuItem::checked("Show Status Bar", Action::ToggleStatusBar, self.show_status);
                count += 1;
                out[count] = MenuItem::separator();
                count += 1;
                out[count] = MenuItem::checked(
                    "CPU History Graph",
                    Action::ToggleCpuGraph,
                    self.show_cpu_graph,
                );
                count += 1;
                out[count] = MenuItem::checked(
                    "Memory History Graph",
                    Action::ToggleMemGraph,
                    self.show_mem_graph,
                );
                count += 1;
                out[count] = MenuItem::separator();
                count += 1;
                out[count] = MenuItem::checked(
                    "CPU Graph: Combined",
                    Action::CpuGraphCombined,
                    self.cpu_view == CpuView::Combined,
                );
                count += 1;
                out[count] = MenuItem::checked(
                    "CPU Graph: Per Core",
                    Action::CpuGraphPerCore,
                    self.cpu_view == CpuView::PerCore,
                );
                count += 1;
            }
            Menu::Help => {
                out[count] = MenuItem::new("Task Manager Help Topics", Action::HelpTopics, true);
                count += 1;
                out[count] = MenuItem::separator();
                count += 1;
                out[count] = MenuItem::new("About Windows Task Manager", Action::About, true);
                count += 1;
            }
        }
        count
    }

    fn execute_action(&mut self, platform: &Platform, action: Action, now: u64) {
        match action {
            Action::None => {}
            Action::NewTask => {
                self.run_text.clear();
                self.dialog = Dialog::Run;
            }
            Action::EndTask | Action::EndProcess => self.end_selected(),
            Action::Close => {
                self.exit = true;
                self.exit_message = "";
            }
            Action::Refresh => self.sample(platform, now),
            Action::AlwaysOnTop => self.always_on_top = !self.always_on_top,
            Action::MinimizeOnUse => self.minimize_on_use = !self.minimize_on_use,
            Action::HideWhenMinimized => self.hide_when_minimized = !self.hide_when_minimized,
            Action::SpeedHigh => self.update = UpdateSpeed::High,
            Action::SpeedNormal => self.update = UpdateSpeed::Normal,
            Action::SpeedLow => self.update = UpdateSpeed::Low,
            Action::SpeedPaused => self.update = UpdateSpeed::Paused,
            Action::SelectColumns => {
                self.column_backup = self.column_visible;
                self.dialog = Dialog::Columns;
            }
            Action::ToggleStatusBar => self.show_status = !self.show_status,
            Action::ToggleCpuGraph => self.show_cpu_graph = !self.show_cpu_graph,
            Action::ToggleMemGraph => self.show_mem_graph = !self.show_mem_graph,
            Action::CpuGraphCombined => self.cpu_view = CpuView::Combined,
            Action::CpuGraphPerCore => self.cpu_view = CpuView::PerCore,
            Action::HelpTopics => {
                self.dialog = Dialog::Help;
                self.dialog_scroll = 0;
            }
            Action::About => self.dialog = Dialog::About,
            Action::SwitchTo => self.switch_to(now),
            Action::Properties => {
                if let Some(row) = self.selected_row() {
                    self.dialog = Dialog::Properties(row.pid);
                }
            }
        }
    }
}

fn state_scroll(state: &State, tab: Tab) -> usize {
    match tab {
        Tab::Applications => state.app_scroll,
        Tab::Processes => state.process_scroll,
        Tab::Users => state.user_scroll,
        _ => 0,
    }
}

fn visible_rows(rows: Rect) -> usize {
    (rows.height as i32 / ROW_HEIGHT).max(1) as usize
}

fn row_at(rows: Rect, pointer: Point, scroll: usize) -> Option<usize> {
    if !rows.contains(pointer) {
        return None;
    }
    Some(scroll + ((pointer.y - rows.y) / ROW_HEIGHT) as usize)
}

fn column_at(header: Rect, pointer: Point, state: &State) -> Option<usize> {
    if !header.contains(pointer) {
        return None;
    }
    let mut x = header.x;
    for (index, column) in Column::ALL.iter().enumerate() {
        if !state.column_visible[index] {
            continue;
        }
        let width = column.width();
        if pointer.x >= x && pointer.x < x + width {
            return Some(index);
        }
        x += width;
    }
    None
}

fn menu_panel_rect(state: &State, layout: Layout, menu: Menu) -> Rect {
    let index = Menu::ALL
        .iter()
        .position(|value| *value == menu)
        .unwrap_or(0);
    let title = menu_title_rect(layout, index);
    let mut items = [MenuItem::new("", Action::None, false); 12];
    let count = state.menu_items(menu, &mut items);
    let height: i32 = items
        .iter()
        .take(count)
        .map(|item| if item.separator { 8 } else { MENU_ROW })
        .sum();
    Rect::new(
        title.x,
        layout.menu_bar.bottom_i32(),
        238,
        (4 + height) as u32,
    )
}

fn context_items(tab: Tab) -> &'static [(&'static str, Action)] {
    if tab == Tab::Applications {
        &[
            ("Switch To", Action::SwitchTo),
            ("End Task", Action::EndTask),
            ("Bring to Front", Action::SwitchTo),
            ("Properties", Action::Properties),
        ]
    } else {
        &[
            ("End Process", Action::EndProcess),
            ("End Process Tree", Action::EndProcess),
            ("Set Priority", Action::None),
            ("Debug", Action::None),
            ("Properties", Action::Properties),
        ]
    }
}

fn context_panel_rect(layout: Layout, context: ContextMenu) -> Rect {
    let items = context_items(context.tab);
    let height = 8 + items.len() as i32 * MENU_ROW;
    let x = context
        .x
        .min(layout.window.right_i32() - 196)
        .max(layout.window.x);
    let y = context
        .y
        .min(layout.window.bottom_i32() - height)
        .max(layout.window.y);
    Rect::new(x, y, 196, height as u32)
}

fn title_button_rect(layout: Layout, index: usize) -> Rect {
    const WIDTH: i32 = 21;
    const HEIGHT: i32 = 20;
    const GAP: i32 = 3;
    let right = layout.inner.right_i32() - 6;
    let x = right - (3 - index) as i32 * (WIDTH + GAP) + GAP;
    Rect::new(x, layout.title.y + 4, WIDTH as u32, HEIGHT as u32)
}

/// Thickness of the invisible resize border around the task manager window.
const RESIZE_BORDER: i32 = 5;
/// Size of the invisible resize corners around the task manager window.
const RESIZE_CORNER: i32 = 14;

fn resize_edge_at(window: Rect, point: Point) -> Option<ResizeEdge> {
    if !window.contains(point) {
        return None;
    }
    if window.width <= (RESIZE_CORNER * 2) as u32 || window.height <= (RESIZE_CORNER * 2) as u32 {
        return None;
    }

    let left = point.x < window.x + RESIZE_CORNER;
    let right = point.x >= window.right_i32() - RESIZE_CORNER;
    let top = point.y < window.y + RESIZE_CORNER;
    let bottom = point.y >= window.bottom_i32() - RESIZE_CORNER;

    if left && top {
        Some(ResizeEdge::TopLeft)
    } else if right && top {
        Some(ResizeEdge::TopRight)
    } else if left && bottom {
        Some(ResizeEdge::BottomLeft)
    } else if right && bottom {
        Some(ResizeEdge::BottomRight)
    } else if point.x < window.x + RESIZE_BORDER {
        Some(ResizeEdge::Left)
    } else if point.x >= window.right_i32() - RESIZE_BORDER {
        Some(ResizeEdge::Right)
    } else if point.y < window.y + RESIZE_BORDER {
        Some(ResizeEdge::Top)
    } else if point.y >= window.bottom_i32() - RESIZE_BORDER {
        Some(ResizeEdge::Bottom)
    } else {
        None
    }
}

fn resize_cursor(edge: ResizeEdge) -> feos_gui::CursorShape {
    match edge {
        ResizeEdge::Left | ResizeEdge::Right => feos_gui::CursorShape::SizeHorizontal,
        ResizeEdge::Top | ResizeEdge::Bottom => feos_gui::CursorShape::SizeVertical,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => feos_gui::CursorShape::SizeNwse,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => feos_gui::CursorShape::SizeNesw,
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(canvas: &mut Canvas<'_>, platform: &Platform, state: &mut State, now: u64) {
    state.set_screen(Rect::new(
        0,
        0,
        canvas.width() as u32,
        canvas.height() as u32,
    ));
    let mut painter = Painter::new(canvas);
    let layout = Layout::compute(state.screen, state);

    draw_backdrop(&mut painter, state);
    draw_window(&mut painter, state, layout, now);
    draw_tabs(&mut painter, state, layout);
    draw_content(&mut painter, platform, state, layout, now);
    if state.show_status {
        draw_status(&mut painter, state, layout);
    }
    if !state.maximized {
        draw_resize_grip(&mut painter, layout.window);
    }
    if let Some(menu) = state.menu {
        draw_menu(&mut painter, state, layout, menu);
    }
    if let Some(context) = state.context {
        draw_context(&mut painter, state, layout, context);
    }
    if state.dialog != Dialog::None {
        draw_dialog(&mut painter, platform, state, layout, now);
    }
    if state.notice_active(now) {
        draw_notice(&mut painter, state, layout, now);
    }
}

fn draw_backdrop(painter: &mut Painter<'_, '_>, state: &State) {
    let full = painter.canvas_rect();
    if state.maximized {
        painter.fill_rect(full, FACE);
        return;
    }
    painter.canvas_mut().fill_vertical_gradient(
        full,
        Color::rgb(63, 140, 243),
        Color::rgb(36, 94, 219),
    );
}

fn draw_window_shadow(painter: &mut Painter<'_, '_>, window: Rect) {
    // Two opaque bands are far cheaper than an alpha-blended full-window
    // shadow, and the window covers all but the outer few pixels anyway.
    let outer = Color::rgb(38, 82, 158);
    let inner = Color::rgb(52, 104, 184);
    painter.fill_rect(
        Rect::new(window.x + 8, window.bottom_i32(), window.width, 3),
        inner,
    );
    painter.fill_rect(
        Rect::new(window.right_i32(), window.y + 8, 3, window.height),
        inner,
    );
    painter.fill_rect(
        Rect::new(window.x + 11, window.bottom_i32() + 3, window.width, 5),
        outer,
    );
    painter.fill_rect(
        Rect::new(window.right_i32() + 3, window.y + 11, 5, window.height),
        outer,
    );
}

fn draw_window(painter: &mut Painter<'_, '_>, state: &State, layout: Layout, now: u64) {
    if !state.maximized {
        draw_window_shadow(painter, layout.window);
    }
    if state.maximized {
        painter.fill_rect(layout.inner, FACE);
    } else {
        // Paint only the 3 px Luna border, then the client area. Filling the
        // whole window first and covering it again doubles the pixel traffic.
        let border = Color::rgb(58, 145, 255);
        painter.fill_rect(
            Rect::new(layout.window.x, layout.window.y, layout.window.width, 3),
            border,
        );
        painter.fill_rect(
            Rect::new(
                layout.window.x,
                layout.window.bottom_i32() - 3,
                layout.window.width,
                3,
            ),
            border,
        );
        painter.fill_rect(
            Rect::new(layout.window.x, layout.window.y, 3, layout.window.height),
            border,
        );
        painter.fill_rect(
            Rect::new(
                layout.window.right_i32() - 3,
                layout.window.y,
                3,
                layout.window.height,
            ),
            border,
        );
        painter.fill_rect(layout.inner, FACE);
    }
    if !state.embedded {
        painter
            .canvas_mut()
            .fill_vertical_gradient(layout.title, TITLE_TOP, TITLE_BOTTOM);

        draw_taskmgr_icon(
            painter,
            Rect::new(layout.title.x + 7, layout.title.y + 5, 18, 18),
        );

        let buttons_left = title_button_rect(layout, 0).x - 6;
        painter.draw_text_ellipsis(
            Rect::new(
                layout.title.x + 31,
                layout.title.y,
                (buttons_left - layout.title.x - 35).max(10) as u32,
                layout.title.height,
            ),
            "Windows Task Manager",
            Color::WHITE,
            2,
            Align::Start,
            VerticalAlign::Middle,
        );

        for index in 0..3 {
            draw_title_button(
                painter,
                title_button_rect(layout, index),
                index,
                state.pointer,
                state.maximized,
            );
        }
    }

    draw_menu_bar(painter, state, layout);
    let _ = now;
}

fn draw_taskmgr_icon(painter: &mut Painter<'_, '_>, rect: Rect) {
    let w = rect.width as i32;
    let h = rect.height as i32;
    painter.fill_rounded_rect(rect, 2, Color::rgb(238, 238, 238));
    painter.stroke_rounded_rect(rect, 2, 1, Color::rgb(70, 70, 70));
    let graph = Rect::new(
        rect.x + 3,
        rect.y + 4,
        (w - 6).max(4) as u32,
        (h - 7).max(4) as u32,
    );
    painter.fill_rect(graph, GRAPH_BG);
    painter.fill_rect(
        Rect::new(graph.x + 1, graph.y + 1, graph.width.saturating_sub(2), 1),
        GRAPH_GRID,
    );
    let mid = graph.bottom_i32() - graph.height as i32 / 2;
    painter.fill_rect(
        Rect::new(graph.x + 1, mid, graph.width.saturating_sub(2), 1),
        GRAPH_GRID,
    );
    let points = [
        Point::new(graph.x + 1, graph.bottom_i32() - 2),
        Point::new(graph.x + 4, graph.bottom_i32() - 5),
        Point::new(graph.x + 7, graph.bottom_i32() - 3),
        Point::new(graph.x + 10, graph.bottom_i32() - 8),
        Point::new(graph.x + 13, graph.bottom_i32() - 6),
        Point::new(graph.x + 15, graph.bottom_i32() - 9),
    ];
    for pair in points.windows(2) {
        painter.line(pair[0].x, pair[0].y, pair[1].x, pair[1].y, GRAPH_CPU);
    }
}

fn draw_title_button(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    index: usize,
    pointer: Point,
    maximized: bool,
) {
    let hovered = rect.contains(pointer);
    let color = if index == 2 {
        if hovered {
            Color::rgb(232, 78, 58)
        } else {
            Color::rgb(214, 58, 40)
        }
    } else if hovered {
        Color::rgb(96, 170, 255)
    } else {
        Color::rgb(62, 122, 220)
    };
    painter.fill_rounded_rect(rect, 3, color);
    painter.stroke_rounded_rect(rect, 3, 1, Color::WHITE.lerp(color, 90));
    let center_y = rect.y + rect.height as i32 / 2;
    match index {
        0 => {
            painter.fill_rect(Rect::new(rect.x + 5, center_y + 2, 8, 2), Color::WHITE);
        }
        1 => {
            if maximized {
                painter.stroke_rect(Rect::new(rect.x + 6, center_y - 5, 7, 6), 1, Color::WHITE);
                painter.stroke_rect(Rect::new(rect.x + 4, center_y - 3, 7, 6), 1, Color::WHITE);
            } else {
                painter.stroke_rect(Rect::new(rect.x + 5, center_y - 4, 8, 6), 1, Color::WHITE);
                painter.fill_rect(Rect::new(rect.x + 5, center_y - 4, 8, 2), Color::WHITE);
            }
        }
        _ => {
            painter.line(
                rect.x + 5,
                center_y - 4,
                rect.x + 12,
                center_y + 3,
                Color::WHITE,
            );
            painter.line(
                rect.x + 12,
                center_y - 4,
                rect.x + 5,
                center_y + 3,
                Color::WHITE,
            );
        }
    }
}

fn draw_menu_bar(painter: &mut Painter<'_, '_>, state: &State, layout: Layout) {
    painter.fill_rect(layout.menu_bar, FACE);
    painter.fill_rect(
        Rect::new(
            layout.menu_bar.x,
            layout.menu_bar.bottom_i32() - 1,
            layout.menu_bar.width,
            1,
        ),
        BORDER,
    );
    for (index, menu) in Menu::ALL.iter().enumerate() {
        let rect = menu_title_rect(layout, index);
        let open = state.menu == Some(*menu);
        if open || rect.contains(state.pointer) {
            painter.fill_rect(rect, if open { SELECT } else { SELECT_SOFT });
        }
        let color = if open { Color::WHITE } else { TEXT };
        painter.draw_text_aligned(
            rect,
            menu.title(),
            color,
            1,
            Align::Center,
            VerticalAlign::Middle,
        );
    }
}

fn draw_tabs(painter: &mut Painter<'_, '_>, state: &State, layout: Layout) {
    painter.fill_rect(
        Rect::new(
            layout.tabs.x,
            layout.tabs.bottom_i32() - 1,
            layout.tabs.width,
            1,
        ),
        BORDER,
    );
    for (index, tab) in Tab::ALL.iter().enumerate() {
        let rect = tab_rects(layout)[index];
        let active = state.tab == *tab;
        let hovered = rect.contains(state.pointer);
        if active {
            painter.fill_rect(rect, FACE_LIGHT);
            painter.fill_rect(Rect::new(rect.x, rect.y, rect.width, 1), Color::WHITE);
            painter.fill_rect(Rect::new(rect.x, rect.y, 1, rect.height), BORDER);
            painter.fill_rect(
                Rect::new(rect.right_i32() - 1, rect.y, 1, rect.height),
                BORDER,
            );
        } else {
            let top = if hovered {
                Color::rgb(228, 236, 250)
            } else {
                Color::rgb(198, 205, 220)
            };
            let bottom = if hovered {
                Color::rgb(208, 220, 242)
            } else {
                Color::rgb(176, 186, 208)
            };
            painter
                .canvas_mut()
                .fill_vertical_gradient(rect.inset_each(0, 3, 0, 1), top, bottom);
            painter.stroke_rect(rect.inset_each(0, 3, 0, 1), 1, BORDER);
        }
        painter.draw_text_aligned(
            rect,
            tab.title(),
            TEXT,
            1,
            Align::Center,
            VerticalAlign::Middle,
        );
    }
}

fn draw_content(
    painter: &mut Painter<'_, '_>,
    platform: &Platform,
    state: &mut State,
    layout: Layout,
    now: u64,
) {
    painter.fill_rect(layout.content, FACE);
    match state.tab {
        Tab::Applications => draw_applications(painter, state, layout),
        Tab::Processes => draw_processes(painter, state, layout),
        Tab::Performance => draw_performance(painter, state, layout),
        Tab::Networking => draw_networking(painter, state, layout),
        Tab::Users => draw_users(painter, state, layout),
    }
    let _ = (platform, now);
}

fn application_name(row: &Row) -> &str {
    if row.pid == 1 {
        "Windows Task Manager"
    } else {
        row.name.as_str()
    }
}

fn draw_applications(painter: &mut Painter<'_, '_>, state: &State, layout: Layout) {
    let list = list_layout(layout);
    let visible = visible_rows(list.rows);
    let scroll = state
        .app_scroll
        .min(state.row_count.saturating_sub(visible));

    draw_applications_header(painter, list.header);

    draw_sunken(painter, list.list);
    if state.row_count == 0 {
        painter.draw_text_centered(list.rows, "No tasks are running.", MUTED, 1);
    }
    for row_index in 0..visible {
        let index = scroll + row_index;
        if index >= state.row_count {
            break;
        }
        let row = state.rows[index];
        let rect = Rect::new(
            list.rows.x,
            list.rows.y + row_index as i32 * ROW_HEIGHT,
            list.rows.width,
            ROW_HEIGHT as u32,
        );
        let selected = state.selected_pid == Some(row.pid);
        draw_row_background(painter, rect, selected, rect.contains(state.pointer));
        let name = application_name(&row);
        if selected {
            let icon_rect = Rect::new(rect.x + 4, rect.y + 3, 14, 14);
            draw_app_icon(painter, icon_rect, name);
            painter.draw_text_ellipsis(
                Rect::new(
                    rect.x + 24,
                    rect.y,
                    (rect.width as i32 - 150).max(20) as u32,
                    rect.height,
                ),
                name,
                Color::WHITE,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
            painter.draw_text_aligned(
                Rect::new(rect.right_i32() - 110, rect.y, 100, rect.height),
                row.status_name(),
                Color::WHITE,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
        } else {
            let icon_rect = Rect::new(rect.x + 4, rect.y + 3, 14, 14);
            draw_app_icon(painter, icon_rect, name);
            painter.draw_text_ellipsis(
                Rect::new(
                    rect.x + 24,
                    rect.y,
                    (rect.width as i32 - 150).max(20) as u32,
                    rect.height,
                ),
                name,
                TEXT,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
            painter.draw_text_aligned(
                Rect::new(rect.right_i32() - 110, rect.y, 100, rect.height),
                row.status_name(),
                MUTED,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
        }
    }

    draw_scrollbar(painter, list.scroll, scroll, visible, state.row_count);

    draw_button(
        painter,
        button_rect(list.buttons, 0, 3),
        "End Task",
        state.selected_pid.is_some(),
        false,
        state.pointer,
    );
    draw_button(
        painter,
        button_rect(list.buttons, 1, 3),
        "Switch To",
        state.selected_pid.is_some(),
        false,
        state.pointer,
    );
    draw_button(
        painter,
        button_rect(list.buttons, 2, 3),
        "New Task...",
        true,
        false,
        state.pointer,
    );
}

fn draw_processes(painter: &mut Painter<'_, '_>, state: &State, layout: Layout) {
    let list = list_layout(layout);
    let visible = visible_rows(list.rows);
    let scroll = state
        .process_scroll
        .min(state.row_count.saturating_sub(visible));

    draw_column_header(painter, list.header, state);
    draw_sunken(painter, list.list);
    if state.row_count == 0 {
        painter.draw_text_centered(list.rows, "No processes are running.", MUTED, 1);
    }

    for row_index in 0..visible {
        let index = scroll + row_index;
        if index >= state.row_count {
            break;
        }
        let row = state.rows[index];
        let rect = Rect::new(
            list.rows.x,
            list.rows.y + row_index as i32 * ROW_HEIGHT,
            list.rows.width,
            ROW_HEIGHT as u32,
        );
        let selected = state.selected_pid == Some(row.pid);
        draw_row_background(painter, rect, selected, rect.contains(state.pointer));
        let color = if selected { Color::WHITE } else { TEXT };
        let muted = if selected { Color::WHITE } else { MUTED };
        let mut x = rect.x;
        for (column_index, column) in Column::ALL.iter().enumerate() {
            if !state.column_visible[column_index] {
                continue;
            }
            let width = column.width();
            let cell = Rect::new(x + 4, rect.y, width.saturating_sub(8) as u32, rect.height);
            let align = column.alignment();
            match column {
                Column::ImageName => {
                    let icon_rect = Rect::new(x + 2, rect.y + 3, 14, 14);
                    draw_app_icon(painter, icon_rect, row.name.as_str());
                    painter.draw_text_ellipsis(
                        Rect::new(x + 20, rect.y, (width - 24).max(8) as u32, rect.height),
                        row.name.as_str(),
                        color,
                        1,
                        Align::Start,
                        VerticalAlign::Middle,
                    );
                }
                Column::Pid => {
                    let mut value = Text::<12>::new();
                    let _ = write!(value, "{}", row.pid);
                    painter.draw_text_aligned(
                        cell,
                        value.as_str(),
                        color,
                        1,
                        align,
                        VerticalAlign::Middle,
                    );
                }
                Column::Cpu => {
                    let mut value = Text::<12>::new();
                    let _ = write!(value, "{}%", row.cpu_percent);
                    painter.draw_text_aligned(
                        cell,
                        value.as_str(),
                        color,
                        1,
                        align,
                        VerticalAlign::Middle,
                    );
                }
                Column::CpuTime => {
                    let value = format_duration(row.cpu_time_ms);
                    painter.draw_text_aligned(
                        cell,
                        value.as_str(),
                        color,
                        1,
                        align,
                        VerticalAlign::Middle,
                    );
                }
                Column::MemUsage => {
                    let value = format_memory(row.memory_bytes);
                    painter.draw_text_aligned(
                        cell,
                        value.as_str(),
                        color,
                        1,
                        align,
                        VerticalAlign::Middle,
                    );
                }
                Column::Threads => {
                    let value = format_count(row.threads as usize);
                    painter.draw_text_aligned(
                        cell,
                        value.as_str(),
                        color,
                        1,
                        align,
                        VerticalAlign::Middle,
                    );
                }
                Column::StartTime => {
                    let value = format_duration(row.started_ms);
                    painter.draw_text_aligned(
                        cell,
                        value.as_str(),
                        color,
                        1,
                        align,
                        VerticalAlign::Middle,
                    );
                }
                Column::Status => {
                    painter.draw_text_aligned(
                        cell,
                        row.status_name(),
                        muted,
                        1,
                        align,
                        VerticalAlign::Middle,
                    );
                }
                Column::Priority => {
                    painter.draw_text_aligned(
                        cell,
                        row.priority_name(),
                        muted,
                        1,
                        align,
                        VerticalAlign::Middle,
                    );
                }
                Column::UserName => {
                    painter.draw_text_aligned(cell, "feos", muted, 1, align, VerticalAlign::Middle);
                }
                Column::SessionId => {
                    painter.draw_text_aligned(cell, "0", muted, 1, align, VerticalAlign::Middle);
                }
            }
            x += width;
        }
    }

    draw_scrollbar(painter, list.scroll, scroll, visible, state.row_count);

    draw_button(
        painter,
        button_rect(list.buttons, 0, 2),
        "End Process",
        state.selected_pid.is_some(),
        false,
        state.pointer,
    );
    draw_button(
        painter,
        button_rect(list.buttons, 1, 2),
        "Properties",
        state.selected_pid.is_some(),
        false,
        state.pointer,
    );
}

fn draw_applications_header(painter: &mut Painter<'_, '_>, rect: Rect) {
    painter
        .canvas_mut()
        .fill_vertical_gradient(rect, HEADER_TOP, HEADER_BOTTOM);
    painter.fill_rect(
        Rect::new(rect.x, rect.bottom_i32() - 1, rect.width, 1),
        BORDER_DARK,
    );
    painter.draw_text_aligned(
        Rect::new(
            rect.x + 4,
            rect.y,
            (rect.width as i32 - 120).max(20) as u32,
            rect.height,
        ),
        "Task",
        Color::WHITE,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    painter.draw_text_aligned(
        Rect::new(rect.right_i32() - 110, rect.y, 100, rect.height),
        "Status",
        Color::WHITE,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
}

fn draw_column_header(painter: &mut Painter<'_, '_>, rect: Rect, state: &State) {
    painter
        .canvas_mut()
        .fill_vertical_gradient(rect, HEADER_TOP, HEADER_BOTTOM);
    painter.fill_rect(
        Rect::new(rect.x, rect.bottom_i32() - 1, rect.width, 1),
        BORDER_DARK,
    );
    let mut x = rect.x;
    for (index, column) in Column::ALL.iter().enumerate() {
        if !state.column_visible[index] {
            continue;
        }
        let width = column.width();
        let active = column
            .sort_key()
            .map(|key| state.sort == key)
            .unwrap_or(false);
        if active {
            let arrow = if state.sort_desc {
                Icon::ArrowDown
            } else {
                Icon::ArrowUp
            };
            paint_icon(
                painter,
                Rect::new(x + width - 12, rect.y + 7, 8, 8),
                arrow,
                Color::WHITE,
            );
        }
        let text_width = if active { width - 20 } else { width - 8 };
        painter.draw_text_ellipsis(
            Rect::new(x + 4, rect.y, text_width.max(8) as u32, rect.height),
            column.title(),
            Color::WHITE,
            1,
            column.alignment(),
            VerticalAlign::Middle,
        );
        x += width;
    }
}

fn draw_row_background(painter: &mut Painter<'_, '_>, rect: Rect, selected: bool, hovered: bool) {
    if selected {
        painter.fill_rect(rect, SELECT);
    } else if hovered {
        painter.fill_rect(rect, SELECT_SOFT);
    }
    painter.fill_rect(
        Rect::new(rect.x, rect.bottom_i32() - 1, rect.width, 1),
        Color::rgb(238, 238, 232),
    );
}

fn draw_sunken(painter: &mut Painter<'_, '_>, rect: Rect) {
    painter.fill_rect(rect, FIELD);
    painter.stroke_rect(rect, 1, BORDER_DARK);
    painter.fill_rect(
        Rect::new(rect.x + 1, rect.y + 1, rect.width.saturating_sub(2), 1),
        Color::rgb(128, 128, 128),
    );
    painter.fill_rect(
        Rect::new(rect.x + 1, rect.y + 1, 1, rect.height.saturating_sub(2)),
        Color::rgb(128, 128, 128),
    );
}

fn draw_scrollbar(
    painter: &mut Painter<'_, '_>,
    track: Rect,
    scroll: usize,
    visible: usize,
    total: usize,
) {
    painter.fill_rect(track, FACE_LIGHT);
    painter.stroke_rect(track, 1, BORDER);
    let inner = track.inset(2);
    painter.fill_rect(inner, Color::rgb(238, 238, 232));
    if total <= visible || total == 0 {
        return;
    }
    let thumb_height = ((inner.height as usize * visible) / total).max(18) as u32;
    let span = inner.height.saturating_sub(thumb_height) as usize;
    let max_scroll = total.saturating_sub(visible).max(1);
    let offset = (span * scroll.min(max_scroll)) / max_scroll;
    painter.fill_rounded_rect(
        Rect::new(inner.x, inner.y + offset as i32, inner.width, thumb_height),
        2,
        Color::rgb(190, 190, 178),
    );
}

fn draw_button(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    label: &str,
    enabled: bool,
    default: bool,
    pointer: Point,
) {
    let hovered = enabled && rect.contains(pointer);
    painter.fill_rounded_rect(
        rect,
        3,
        if !enabled {
            FACE_DARK
        } else if hovered {
            HOVER_LIGHT
        } else {
            FACE_LIGHT
        },
    );
    painter.stroke_rounded_rect(
        rect,
        3,
        1,
        if default && enabled && !hovered {
            Color::rgb(70, 110, 180)
        } else {
            BORDER_DARK
        },
    );
    painter.draw_text_centered(rect, label, if enabled { TEXT } else { DISABLED }, 1);
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return true;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn draw_app_icon(painter: &mut Painter<'_, '_>, rect: Rect, name: &str) {
    let accent = if contains_ignore_ascii_case(name, "media")
        || contains_ignore_ascii_case(name, "player")
    {
        Color::rgb(230, 120, 30)
    } else if contains_ignore_ascii_case(name, "task") {
        Color::rgb(0, 140, 60)
    } else if contains_ignore_ascii_case(name, "file")
        || contains_ignore_ascii_case(name, "explorer")
    {
        Color::rgb(220, 170, 40)
    } else if contains_ignore_ascii_case(name, "terminal")
        || contains_ignore_ascii_case(name, "shell")
    {
        Color::rgb(30, 30, 30)
    } else if contains_ignore_ascii_case(name, "tick") {
        Color::rgb(40, 90, 210)
    } else {
        Color::rgb(70, 110, 180)
    };
    if rect.width < 4 || rect.height < 4 {
        return;
    }
    let w = rect.width as i32;
    let h = rect.height as i32;
    let page = Rect::new(
        rect.x + w / 6,
        rect.y + 1,
        (w * 2 / 3).max(5) as u32,
        (h - 2).max(5) as u32,
    );
    painter.fill_rect(page, Color::WHITE);
    painter.stroke_rect(page, 1, Color::rgb(110, 110, 110));
    painter.fill_rect(
        Rect::new(page.x + 2, page.y + 2, page.width.saturating_sub(4), 3),
        accent,
    );
    painter.fill_rect(
        Rect::new(
            page.x + 2,
            page.bottom_i32() - 4,
            page.width.saturating_sub(5),
            1,
        ),
        Color::rgb(170, 170, 170),
    );
}

fn draw_resize_grip(painter: &mut Painter<'_, '_>, window: Rect) {
    let x = window.right_i32() - 13;
    let y = window.bottom_i32() - 13;
    for index in 0..3 {
        let offset = index * 4;
        painter.line(x + offset, y + 9, x + 9, y + offset, BORDER_DARK);
    }
}

// ---------------------------------------------------------------------------
// Performance tab
// ---------------------------------------------------------------------------

fn draw_performance(painter: &mut Painter<'_, '_>, state: &State, layout: Layout) {
    let area = content_area(layout);
    let graphs = state.show_cpu_graph || state.show_mem_graph;
    let per_core = state.show_cpu_graph && state.cpu_view == CpuView::PerCore;
    let (graph_area, stats_area) = if graphs {
        let height = if per_core && state.show_mem_graph {
            // The per-core strip needs a little more room than one combined
            // line, but memory still keeps its own panel underneath.
            ((area.height as i32 * 70) / 100).clamp(220, 360)
        } else {
            ((area.height as i32 * 52) / 100).clamp(160, 320)
        };
        area.split_top(height)
    } else {
        (Rect::new(area.x, area.y, area.width, 0), area)
    };

    if graphs {
        let inner = graph_area.inset_each(2, 2, 2, 4);
        if state.show_cpu_graph && state.show_mem_graph {
            if per_core {
                // Give every CPU a full-width slice; memory moves below.
                let core_height = (inner.height as i32 * 72 / 100).max(120);
                let (cpu_rect, mem_rect) = inner.split_top(core_height);
                draw_cpu_graphs(painter, cpu_rect.inset_each(0, 0, 0, 4), state);
                draw_graph_panel(
                    painter,
                    mem_rect.inset_each(0, 4, 0, 0),
                    "Memory Usage History",
                    state.memory_percent,
                    &state.mem_history,
                    state.mem_len,
                    state.mem_head,
                    GRAPH_MEM,
                    GRAPH_MEM_LABEL,
                );
            } else {
                // Side by side when there is room; stack them in a narrow
                // window so the memory graph is never squeezed out.
                let (cpu_rect, mem_rect) = if inner.width >= 520 {
                    let (left, right) = inner.split_left(inner.width as i32 / 2);
                    (left.inset_each(0, 0, 4, 0), right.inset_each(4, 0, 0, 0))
                } else {
                    let (top, bottom) = inner.split_top(inner.height as i32 / 2);
                    (top.inset_each(0, 0, 0, 4), bottom.inset_each(0, 4, 0, 0))
                };
                draw_cpu_graphs(painter, cpu_rect, state);
                draw_graph_panel(
                    painter,
                    mem_rect,
                    "Memory Usage History",
                    state.memory_percent,
                    &state.mem_history,
                    state.mem_len,
                    state.mem_head,
                    GRAPH_MEM,
                    GRAPH_MEM_LABEL,
                );
            }
        } else if state.show_cpu_graph {
            draw_cpu_graphs(painter, inner, state);
        } else {
            draw_graph_panel(
                painter,
                inner,
                "Memory Usage History",
                state.memory_percent,
                &state.mem_history,
                state.mem_len,
                state.mem_head,
                GRAPH_MEM,
                GRAPH_MEM_LABEL,
            );
        }
    }

    draw_performance_stats(painter, stats_area.inset_each(2, 6, 2, 2), state);
}

/// Draws either the aggregate CPU line or one mini-graph per online core.
fn draw_cpu_graphs(painter: &mut Painter<'_, '_>, rect: Rect, state: &State) {
    if state.cpu_view == CpuView::Combined {
        draw_graph_panel(
            painter,
            rect,
            "CPU Usage History",
            state.cpu_percent,
            &state.cpu_history,
            state.cpu_len,
            state.cpu_head,
            GRAPH_CPU,
            GRAPH_LABEL,
        );
        return;
    }

    let online = state.system.cpus.clamp(1, MAX_CPUS);
    let gap = 3;
    let available = (rect.width as i32 - gap * (online as i32 - 1)).max(online as i32);
    let cell_width = available / online as i32;
    for cpu in 0..online {
        let x = rect.x + cpu as i32 * (cell_width + gap);
        let right = if cpu + 1 == online {
            rect.right_i32()
        } else {
            x + cell_width
        };
        let cell = Rect::new(x, rect.y, (right - x).max(1) as u32, rect.height);
        let mut title = Text::<16>::new();
        let _ = write!(title, "CPU {cpu}");
        draw_graph_panel(
            painter,
            cell,
            title.as_str(),
            state.cpu_core_percent[cpu],
            &state.cpu_core_history[cpu],
            state.cpu_core_len[cpu],
            state.cpu_core_head[cpu],
            GRAPH_CPU,
            GRAPH_LABEL,
        );
    }
}

fn draw_graph_panel(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    title: &str,
    current: u8,
    history: &[u8; HISTORY],
    len: usize,
    head: usize,
    color: Color,
    label_color: Color,
) {
    if rect.is_empty() {
        return;
    }
    painter.fill_rect(rect, FACE);
    painter.stroke_rect(rect, 1, BORDER);
    painter.draw_text(rect.x + 8, rect.y + 5, title, TEXT, 1);
    let mut value = Text::<12>::new();
    let _ = write!(value, "{current} %");
    painter.draw_text_aligned(
        Rect::new(rect.right_i32() - 60, rect.y + 4, 52, 12),
        value.as_str(),
        TEXT,
        1,
        Align::End,
        VerticalAlign::Top,
    );
    let graph = Rect::new(
        rect.x + 8,
        rect.y + 20,
        rect.width.saturating_sub(16),
        rect.height.saturating_sub(28),
    );
    draw_graph(painter, graph, history, len, head, color, label_color);
}

fn draw_graph(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    history: &[u8; HISTORY],
    len: usize,
    head: usize,
    color: Color,
    label_color: Color,
) {
    if rect.is_empty() {
        return;
    }
    painter.fill_rect(rect, GRAPH_BG);
    painter.stroke_rect(rect, 1, Color::rgb(90, 90, 90));
    let plot = rect.inset_each(30, 6, 6, 8);
    if plot.is_empty() {
        return;
    }

    let old_clip = painter.push_clip(plot);
    for index in 0..=4 {
        let y = plot.y + plot.height as i32 * index / 4;
        painter.fill_rect(Rect::new(plot.x, y, plot.width, 1), GRAPH_GRID);
    }
    for index in 0..=6 {
        let x = plot.x + plot.width as i32 * index / 6;
        painter.fill_rect(Rect::new(x, plot.y, 1, plot.height), GRAPH_GRID);
    }

    if len >= 2 {
        let mut previous: Option<(i32, i32)> = None;
        for index in 0..len {
            let value = history_at(history, len, head, index);
            let x = plot.x + plot.width as i32 * index as i32 / (HISTORY as i32 - 1);
            let y = plot.bottom_i32() - (value as i32 * (plot.height as i32 - 1) / 100);
            if let Some((px, py)) = previous {
                if x > px {
                    painter.line(px, py, x, y, color);
                }
            }
            previous = Some((x, y));
        }
    } else if len == 1 {
        let value = history_at(history, len, head, 0);
        let y = plot.bottom_i32() - (value as i32 * (plot.height as i32 - 1) / 100);
        painter.fill_rect(Rect::new(plot.x, y, 3, 1), color);
    }
    painter.restore_clip(old_clip);

    for (index, label) in [("100", 0), ("50", 2), ("0", 4)] {
        let y = plot.y + plot.height as i32 * label / 4 - 4;
        painter.draw_text_aligned(
            Rect::new(rect.x + 3, y, 24, 10),
            index,
            label_color,
            1,
            Align::End,
            VerticalAlign::Top,
        );
    }

    painter.draw_text(
        rect.right_i32() - 84,
        rect.bottom_i32() - 10,
        "60 seconds",
        Color::rgb(120, 120, 120),
        1,
    );
}

fn draw_performance_stats(painter: &mut Painter<'_, '_>, area: Rect, state: &State) {
    if area.is_empty() {
        return;
    }
    let (left, right) = area.split_left(area.width as i32 / 2);
    let (totals, commit) = left.split_top(left.height as i32 / 2);
    let (physical, kernel) = right.split_top(right.height as i32 / 2);

    let cores = format_count(state.system.cpus.max(1));
    let threads = format_count(state.system.threads);
    let processes = format_count(state.system.processes);
    draw_stat_panel(
        painter,
        totals.inset_each(0, 0, 4, 3),
        "Totals",
        &[
            ("CPU cores", cores),
            ("Threads", threads),
            ("Processes", processes),
        ],
    );

    let commit_total = format_memory(state.commit_total);
    let commit_limit = format_memory(state.commit_limit);
    let commit_peak = format_memory(state.commit_peak);
    draw_stat_panel(
        painter,
        commit.inset_each(0, 3, 4, 0),
        "Commit Charge",
        &[
            ("Total", commit_total),
            ("Limit", commit_limit),
            ("Peak", commit_peak),
        ],
    );

    let physical_total = format_memory(state.memory.total_bytes);
    let available = format_memory(state.memory.free_bytes);
    let cache = format_memory(state.memory.heap_free_bytes);
    draw_stat_panel(
        painter,
        physical.inset_each(4, 0, 0, 3),
        "Physical Memory",
        &[
            ("Total", physical_total),
            ("Available", available),
            ("System Cache", cache),
        ],
    );

    let kernel_total = format_memory(state.memory.used_bytes);
    let paged = format_memory(0);
    let nonpaged = format_memory(state.memory.heap_used_bytes);
    draw_stat_panel(
        painter,
        kernel.inset_each(4, 3, 0, 0),
        "Kernel Memory",
        &[
            ("Total", kernel_total),
            ("Paged", paged),
            ("Nonpaged", nonpaged),
        ],
    );
}

fn draw_stat_panel(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    title: &str,
    rows: &[(&str, Text<20>)],
) {
    if rect.is_empty() {
        return;
    }
    painter.fill_rect(rect, FACE);
    painter.stroke_rect(rect, 1, BORDER);
    painter.draw_text(rect.x + 8, rect.y + 4, title, TEXT, 1);
    let mut y = rect.y + 20;
    for (label, value) in rows {
        if y + 12 > rect.bottom_i32() {
            break;
        }
        painter.draw_text(rect.x + 10, y, label, MUTED, 1);
        painter.draw_text_aligned(
            Rect::new(rect.x + 90, y, (rect.width as i32 - 100).max(10) as u32, 12),
            value.as_str(),
            TEXT,
            1,
            Align::End,
            VerticalAlign::Top,
        );
        y += 17;
    }
}

// ---------------------------------------------------------------------------
// Networking and Users tabs
// ---------------------------------------------------------------------------

fn draw_networking(painter: &mut Painter<'_, '_>, state: &State, layout: Layout) {
    let area = content_area(layout);
    let (adapter_area, right) = area.split_left(238);
    let adapter_area = adapter_area.inset_each(0, 0, 8, 0);

    draw_sunken(painter, adapter_area);
    painter.draw_text(
        adapter_area.x + 6,
        adapter_area.y + 5,
        "Adapters and Bindings",
        TEXT,
        1,
    );
    let row = Rect::new(
        adapter_area.x + 4,
        adapter_area.y + 24,
        adapter_area.width.saturating_sub(8),
        22,
    );
    painter.fill_rect(row, SELECT);
    painter.draw_text(
        row.x + 6,
        row.y + 7,
        "Local Area Connection",
        Color::WHITE,
        1,
    );
    painter.draw_text(
        adapter_area.x + 8,
        row.bottom_i32() + 6,
        "No network adapter",
        MUTED,
        1,
    );
    painter.draw_text(
        adapter_area.x + 8,
        row.bottom_i32() + 22,
        "is present on this board.",
        MUTED,
        1,
    );

    let (graph_area, stats_area) = right.split_top((right.height as i32 * 60 / 100).max(150));
    let zeros = [0u8; HISTORY];
    draw_graph_panel(
        painter,
        graph_area.inset_each(2, 2, 2, 4),
        "Network Utilization",
        0,
        &zeros,
        HISTORY,
        0,
        Color::rgb(0, 220, 220),
        Color::rgb(0, 210, 210),
    );

    let rows = [
        ("Adapter", Text::<20>::from_str("none")),
        ("Link Speed", Text::<20>::from_str("n/a")),
        ("State", Text::<20>::from_str("Disconnected")),
        ("Bytes", Text::<20>::from_str("0")),
    ];
    draw_stat_panel(
        painter,
        stats_area.inset_each(2, 6, 2, 2),
        "Local Area Connection",
        &rows,
    );

    let _ = state;
}

fn draw_users(painter: &mut Painter<'_, '_>, state: &State, layout: Layout) {
    let list = list_layout(layout);
    painter
        .canvas_mut()
        .fill_vertical_gradient(list.header, HEADER_TOP, HEADER_BOTTOM);
    painter.fill_rect(
        Rect::new(
            list.header.x,
            list.header.bottom_i32() - 1,
            list.header.width,
            1,
        ),
        BORDER_DARK,
    );
    let widths = [150, 100, 100, 140, 80];
    let labels = ["User", "ID", "Status", "Client Name", "Session"];
    let mut x = list.header.x;
    for (index, label) in labels.iter().enumerate() {
        painter.draw_text_aligned(
            Rect::new(
                x + 4,
                list.header.y,
                widths[index] as u32,
                list.header.height,
            ),
            label,
            Color::WHITE,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        x += widths[index];
    }

    draw_sunken(painter, list.list);
    let row = Rect::new(list.rows.x, list.rows.y, list.rows.width, ROW_HEIGHT as u32);
    painter.fill_rect(row, SELECT);
    let values = ["feos", "1", "Active", "Console", "0"];
    let mut x = row.x;
    for (index, value) in values.iter().enumerate() {
        painter.draw_text_aligned(
            Rect::new(x + 4, row.y, widths[index] as u32, row.height),
            value,
            Color::WHITE,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        x += widths[index];
    }
    painter.fill_rect(
        Rect::new(list.rows.x, list.rows.y + ROW_HEIGHT, list.rows.width, 1),
        Color::rgb(238, 238, 232),
    );

    draw_button(
        painter,
        button_rect(list.buttons, 0, 2),
        "Disconnect",
        false,
        false,
        state.pointer,
    );
    draw_button(
        painter,
        button_rect(list.buttons, 1, 2),
        "Log Off",
        true,
        false,
        state.pointer,
    );
}

// ---------------------------------------------------------------------------
// Status bar, menus, context menu and notices
// ---------------------------------------------------------------------------

fn draw_status(painter: &mut Painter<'_, '_>, state: &State, layout: Layout) {
    if layout.status.is_empty() {
        return;
    }
    painter.fill_rect(layout.status, FACE);
    painter.fill_rect(
        Rect::new(layout.status.x, layout.status.y, layout.status.width, 1),
        BORDER,
    );

    let mut first = Text::<96>::new();
    let mut second = Text::<96>::new();
    let mut third = Text::<96>::new();
    match state.tab {
        Tab::Applications => {
            let _ = write!(first, "Tasks: {}", state.row_count);
            let _ = write!(second, "CPU Usage: {}%", state.cpu_percent);
            let _ = write!(
                third,
                "Commit Charge: {} / {}",
                format_memory(state.commit_total).as_str(),
                format_memory(state.commit_limit).as_str()
            );
        }
        Tab::Processes => {
            let _ = write!(first, "Processes: {}", state.row_count);
            let _ = write!(second, "CPU Usage: {}%", state.cpu_percent);
            let _ = write!(
                third,
                "Commit Charge: {} / {}",
                format_memory(state.commit_total).as_str(),
                format_memory(state.commit_limit).as_str()
            );
        }
        Tab::Performance => {
            let _ = write!(first, "CPU Usage: {}%", state.cpu_percent);
            let _ = write!(second, "Memory Usage: {}%", state.memory_percent);
            let _ = write!(third, "Processes: {}", state.system.processes);
        }
        Tab::Networking => {
            first.set("Network: No adapter");
            let _ = write!(second, "CPU Usage: {}%", state.cpu_percent);
            let _ = write!(
                third,
                "Uptime: {}",
                format_duration(state.system.uptime_ms).as_str()
            );
        }
        Tab::Users => {
            first.set("Users: 1");
            second.set("Session: Console");
            let _ = write!(
                third,
                "Uptime: {}",
                format_duration(state.system.uptime_ms).as_str()
            );
        }
    }

    let area = layout.status.inset_each(2, 2, 2, 2);
    let widths = [
        (area.width as i32 * 38) / 100,
        (area.width as i32 * 32) / 100,
    ];
    let segments = [
        Rect::new(area.x, area.y, widths[0] as u32, area.height),
        Rect::new(area.x + widths[0], area.y, widths[1] as u32, area.height),
        Rect::new(
            area.x + widths[0] + widths[1],
            area.y,
            area.width.saturating_sub((widths[0] + widths[1]) as u32),
            area.height,
        ),
    ];
    let texts = [first.as_str(), second.as_str(), third.as_str()];
    for (index, rect) in segments.iter().enumerate() {
        painter.draw_text_ellipsis(
            Rect::new(
                rect.x + 6,
                rect.y,
                rect.width.saturating_sub(10),
                rect.height,
            ),
            texts[index],
            TEXT,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        if index > 0 {
            painter.fill_rect(
                Rect::new(rect.x, rect.y + 2, 1, rect.height.saturating_sub(4)),
                BORDER,
            );
        }
    }
}

fn draw_menu(painter: &mut Painter<'_, '_>, state: &State, layout: Layout, menu: Menu) {
    let panel = menu_panel_rect(state, layout, menu);
    painter.shadow(panel, 0, 4, 80);
    painter.fill_rect(panel, FACE_LIGHT);
    painter.stroke_rect(panel, 1, BORDER_DARK);

    let mut items = [MenuItem::new("", Action::None, false); 12];
    let count = state.menu_items(menu, &mut items);
    let mut y = panel.y + 4;
    for (index, item) in items.iter().take(count).enumerate() {
        let height = if item.separator { 8 } else { MENU_ROW };
        let rect = Rect::new(panel.x, y, panel.width, height as u32);
        y += height;
        if item.separator {
            painter.fill_rect(
                Rect::new(panel.x + 8, rect.y + 4, panel.width.saturating_sub(16), 1),
                BORDER,
            );
            continue;
        }
        let hovered = rect.contains(state.pointer) || index == state.menu_hover;
        if hovered && item.enabled {
            painter.fill_rect(rect, SELECT);
        }
        let color = if !item.enabled {
            DISABLED
        } else if hovered {
            Color::WHITE
        } else {
            TEXT
        };
        if item.checked {
            paint_icon(
                painter,
                Rect::new(panel.x + 6, rect.y + 7, 8, 8),
                Icon::Check,
                color,
            );
        }
        painter.draw_text_ellipsis(
            Rect::new(
                panel.x + 22,
                rect.y,
                panel.width.saturating_sub(28),
                rect.height,
            ),
            item.label,
            color,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }
}

fn draw_context(
    painter: &mut Painter<'_, '_>,
    state: &State,
    layout: Layout,
    context: ContextMenu,
) {
    let panel = context_panel_rect(layout, context);
    painter.shadow(panel, 0, 4, 80);
    painter.fill_rect(panel, FACE_LIGHT);
    painter.stroke_rect(panel, 1, BORDER_DARK);
    let items = context_items(context.tab);
    let mut y = panel.y + 4;
    for (index, (label, action)) in items.iter().enumerate() {
        let rect = Rect::new(panel.x, y, panel.width, MENU_ROW as u32);
        y += MENU_ROW;
        let enabled = *action != Action::None;
        let hovered = rect.contains(state.pointer) || index == context.hover;
        if hovered && enabled {
            painter.fill_rect(rect, SELECT);
        }
        let color = if !enabled {
            DISABLED
        } else if hovered {
            Color::WHITE
        } else {
            TEXT
        };
        painter.draw_text_ellipsis(
            Rect::new(
                panel.x + 10,
                rect.y,
                panel.width.saturating_sub(16),
                rect.height,
            ),
            label,
            color,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }
}

fn draw_notice(painter: &mut Painter<'_, '_>, state: &State, layout: Layout, _now: u64) {
    let width = 330.min(layout.window.width.saturating_sub(20));
    let panel = Rect::new(
        layout.window.right_i32() - width as i32 - 10,
        layout.window.bottom_i32() - 96,
        width,
        54,
    );
    painter.shadow(panel, 3, 5, 70);
    painter.fill_rect(panel, NOTICE_BG);
    painter.stroke_rect(panel, 1, NOTICE_BORDER);
    paint_icon(
        painter,
        Rect::new(panel.x + 8, panel.y + 9, 20, 20),
        Icon::Info,
        Color::rgb(0, 90, 180),
    );
    painter.draw_text_wrapped(
        Rect::new(
            panel.x + 34,
            panel.y + 7,
            panel.width.saturating_sub(42),
            panel.height.saturating_sub(12),
        ),
        state.notice.as_str(),
        TEXT,
        1,
    );
}

// ---------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------

fn dialog_title(dialog: Dialog) -> &'static str {
    match dialog {
        Dialog::Run => "Run",
        Dialog::About => "About Windows Task Manager",
        Dialog::Columns => "Select Columns",
        Dialog::Help => "Task Manager Help",
        Dialog::Properties(_) => "Process Properties",
        Dialog::EndProcess { .. } => "Task Manager Warning",
        Dialog::LogOff => "Log Off Windows",
        Dialog::None => "",
    }
}

fn draw_dialog(
    painter: &mut Painter<'_, '_>,
    platform: &Platform,
    state: &State,
    layout: Layout,
    now: u64,
) {
    let dialog = state.dialog;
    let panel = dialog_panel(layout, dialog);
    painter.shadow(panel, 5, 10, 90);
    painter.fill_rect(panel, FACE);
    painter.stroke_rect(panel, 1, BORDER_DARK);

    let title = Rect::new(panel.x + 1, panel.y + 1, panel.width.saturating_sub(2), 25);
    painter
        .canvas_mut()
        .fill_vertical_gradient(title, TITLE_TOP, TITLE_BOTTOM);
    painter.draw_text_ellipsis(
        Rect::new(
            title.x + 9,
            title.y,
            title.width.saturating_sub(40),
            title.height,
        ),
        dialog_title(dialog),
        Color::WHITE,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    draw_close_x(
        painter,
        Rect::new(title.right_i32() - 22, title.y + 4, 16, 16),
        false,
    );

    let body = Rect::new(
        panel.x + 14,
        panel.y + 34,
        panel.width.saturating_sub(28),
        panel.height.saturating_sub(76),
    );
    match dialog {
        Dialog::Run => draw_run_dialog(painter, state, body),
        Dialog::About => draw_about_dialog(painter, platform, state, body),
        Dialog::Columns => draw_columns_dialog(painter, state, body),
        Dialog::Help => draw_help_dialog(painter, state, body),
        Dialog::Properties(pid) => draw_properties_dialog(painter, state, pid, body),
        Dialog::EndProcess { pid, .. } => draw_end_dialog(painter, state, pid, body),
        Dialog::LogOff => draw_logoff_dialog(painter, body),
        Dialog::None => {}
    }

    let buttons = dialog_buttons(dialog);
    for index in 0..buttons.len() {
        let rect = dialog_button_rect(panel, index, buttons.len());
        draw_button(
            painter,
            rect,
            buttons[index],
            index != 2 || dialog == Dialog::Run,
            index == 0,
            state.pointer,
        );
    }
    let _ = now;
}

fn draw_run_dialog(painter: &mut Painter<'_, '_>, state: &State, body: Rect) {
    paint_icon(
        painter,
        Rect::new(body.x, body.y, 24, 24),
        Icon::Terminal,
        Color::rgb(30, 30, 30),
    );
    painter.draw_text(
        body.x + 34,
        body.y,
        "Type the name of a program, folder or command,",
        TEXT,
        1,
    );
    painter.draw_text(
        body.x + 34,
        body.y + 14,
        "and Task Manager will run it for you.",
        TEXT,
        1,
    );
    painter.draw_text(body.x, body.y + 46, "Open:", TEXT, 1);
    let field = Rect::new(body.x + 42, body.y + 42, body.width.saturating_sub(42), 22);
    draw_text_field(painter, field, state.run_text.as_str(), true);
    painter.draw_text(
        body.x,
        body.y + 76,
        "Tip: use 'tick 10 200' or another background command.",
        MUTED,
        1,
    );
}

fn draw_about_dialog(
    painter: &mut Painter<'_, '_>,
    platform: &Platform,
    state: &State,
    body: Rect,
) {
    draw_taskmgr_icon(painter, Rect::new(body.x, body.y, 40, 40));
    painter.draw_text(body.x + 52, body.y + 2, "Windows Task Manager", TEXT, 2);
    painter.draw_text(body.x + 52, body.y + 24, "feos task manager", MUTED, 1);
    painter.draw_text(body.x + 52, body.y + 38, "Version 1.0 (feos)", MUTED, 1);

    let mut line = Text::<96>::new();
    let mut y = body.y + 62;
    let _ = write!(line, "Board: {}", platform.board.name);
    painter.draw_text(body.x, y, line.as_str(), TEXT, 1);
    y += 16;
    let _ = write!(
        line,
        "CPU: EL{}  MIDR {:#018x}",
        platform.cpu.current_el(),
        platform.cpu.midr()
    );
    painter.draw_text(body.x, y, line.as_str(), TEXT, 1);
    y += 16;
    let _ = write!(
        line,
        "Uptime: {}",
        format_duration(platform.clock.uptime_ms()).as_str()
    );
    painter.draw_text(body.x, y, line.as_str(), TEXT, 1);
    y += 16;
    let _ = write!(
        line,
        "Processes: {}   Threads: {}",
        state.system.processes, state.system.threads
    );
    painter.draw_text(body.x, y, line.as_str(), TEXT, 1);
    y += 16;
    let _ = write!(
        line,
        "Memory: {} total, {} free",
        format_memory(state.memory.total_bytes).as_str(),
        format_memory(state.memory.free_bytes).as_str()
    );
    painter.draw_text(body.x, y, line.as_str(), TEXT, 1);
    y += 24;
    painter.draw_text(body.x, y, "This product is licensed to:", MUTED, 1);
    painter.draw_text(body.x, y + 14, "feos user", TEXT, 1);
}

fn draw_columns_dialog(painter: &mut Painter<'_, '_>, state: &State, body: Rect) {
    painter.draw_text(body.x, body.y, "Select the columns to display:", TEXT, 1);
    let mut y = body.y + 22;
    for (index, column) in Column::ALL.iter().enumerate() {
        let row = Rect::new(body.x, y, body.width, DIALOG_ROW as u32);
        if row.contains(state.pointer) {
            painter.fill_rect(row, SELECT_SOFT);
        }
        draw_checkbox(
            painter,
            Rect::new(body.x + 2, y + 4, 14, 14),
            state.column_visible[index],
        );
        painter.draw_text(body.x + 24, y + 7, column.title(), TEXT, 1);
        y += DIALOG_ROW;
    }
}

const HELP_LINES: &[&str] = &[
    "Task Manager",
    "",
    "Applications",
    "  Lists the tasks running on this system. Select a task and choose",
    "  End Task to stop it, or Switch To to return to the desktop.",
    "",
    "Processes",
    "  Shows every kernel process with its PID, CPU usage, CPU time,",
    "  memory usage, thread count, start time, status and priority.",
    "  Click a column heading to sort; click it again to reverse.",
    "  Double-click a process to open its properties.",
    "",
    "Performance",
    "  The CPU graph shows busy scheduler ticks as a percentage. Time",
    "  spent in an idle wait is excluded, so an idle desktop is quiet.",
    "  View > CPU Graph switches between one combined line and a graph",
    "  for each online core. Each core needs its own runnable thread; one",
    "  desktop task can only occupy one core. Spawn background tasks to",
    "  exercise the other cores.",
    "  The memory graph shows commit charge against the heap limit.",
    "  Totals, commit charge, physical memory and kernel memory are",
    "  sampled from the live kernel and the static heap.",
    "",
    "Networking",
    "  Lists the network adapters. This board has no network driver,",
    "  so the adapter is reported as disconnected and the graph is flat.",
    "",
    "Users",
    "  Shows the console session and the current user.",
    "",
    "Keyboard",
    "  F1 Help    F5 Refresh    F10 Menu    Delete End process",
    "  Tab Next tab    Ctrl+Left/Right Previous/next tab",
    "  Up/Down Select    Page Up/Down Scroll    Enter Properties",
    "  Esc Close a dialog/menu, or leave Task Manager.",
    "",
    "Options",
    "  Update Speed controls sampling (High, Normal, Low, Paused).",
    "  Select Columns chooses the process columns to display.",
];

fn draw_help_dialog(painter: &mut Painter<'_, '_>, state: &State, body: Rect) {
    let line_height = 14;
    let visible = (body.height as i32 / line_height).max(1) as usize;
    let scroll = state
        .dialog_scroll
        .min(HELP_LINES.len().saturating_sub(visible));
    let mut y = body.y;
    for line in HELP_LINES.iter().skip(scroll).take(visible) {
        painter.draw_text_ellipsis(
            Rect::new(body.x, y, body.width, line_height as u32),
            line,
            if line.is_empty() { MUTED } else { TEXT },
            1,
            Align::Start,
            VerticalAlign::Top,
        );
        y += line_height;
    }
    if HELP_LINES.len() > visible {
        draw_scrollbar(
            painter,
            Rect::new(body.right_i32() - 10, body.y, 10, body.height),
            scroll,
            visible,
            HELP_LINES.len(),
        );
    }
}

fn draw_properties_dialog(painter: &mut Painter<'_, '_>, state: &State, pid: u32, body: Rect) {
    let Some(row) = state.rows[..state.row_count]
        .iter()
        .find(|row| row.pid == pid)
    else {
        painter.draw_text(body.x, body.y, "The process has exited.", TEXT, 1);
        return;
    };

    draw_app_icon(
        painter,
        Rect::new(body.x, body.y, 32, 32),
        row.name.as_str(),
    );
    painter.draw_text(body.x + 44, body.y + 2, row.name.as_str(), TEXT, 2);
    painter.draw_text(body.x + 44, body.y + 24, row.status_name(), MUTED, 1);

    let mut y = body.y + 48;
    let fields: [(&str, Text<32>); 9] = [
        ("PID", text_value(row.pid as u64)),
        ("CPU Usage", text_percent(row.cpu_percent)),
        (
            "CPU Time",
            Text::<32>::from_str(format_duration(row.cpu_time_ms).as_str()),
        ),
        (
            "Memory Usage",
            Text::<32>::from_str(format_memory(row.memory_bytes).as_str()),
        ),
        (
            "Threads",
            Text::<32>::from_str(format_count(row.threads as usize).as_str()),
        ),
        ("Handles", Text::<32>::from_str(format_count(0).as_str())),
        ("Priority", Text::from_str(row.priority_name())),
        (
            "Started",
            Text::<32>::from_str(format_duration(row.started_ms).as_str()),
        ),
        ("Session ID", Text::from_str("0")),
    ];
    for (label, value) in fields {
        painter.draw_text(body.x, y, label, MUTED, 1);
        painter.draw_text_aligned(
            Rect::new(body.x + 110, y, body.width.saturating_sub(110), 12),
            value.as_str(),
            TEXT,
            1,
            Align::Start,
            VerticalAlign::Top,
        );
        y += 17;
    }

    painter.draw_text(body.x, y + 4, "Command line:", MUTED, 1);
    painter.fill_rect(Rect::new(body.x, y + 18, body.width, 20), FIELD);
    painter.stroke_rect(Rect::new(body.x, y + 18, body.width, 20), 1, FIELD_BORDER);
    painter.draw_text(body.x + 4, y + 24, row.name.as_str(), TEXT, 1);
}

fn text_value(value: u64) -> Text<32> {
    let mut text = Text::<32>::new();
    let _ = write!(text, "{value}");
    text
}

fn text_percent(value: u8) -> Text<32> {
    let mut text = Text::<32>::new();
    let _ = write!(text, "{value}%");
    text
}

fn draw_end_dialog(painter: &mut Painter<'_, '_>, state: &State, pid: u32, body: Rect) {
    let name = state
        .rows
        .iter()
        .find(|row| row.pid == pid)
        .map(|row| row.name)
        .unwrap_or_else(|| Text::from_str("process"));
    paint_icon(
        painter,
        Rect::new(body.x, body.y + 4, 28, 28),
        Icon::Warning,
        Color::rgb(210, 140, 0),
    );
    let mut line = Text::<180>::new();
    let _ = write!(
        line,
        "Are you sure you want to end '{}' (PID {pid})?",
        name.as_str()
    );
    painter.draw_text_wrapped(
        Rect::new(body.x + 40, body.y, body.width.saturating_sub(40), 40),
        line.as_str(),
        TEXT,
        1,
    );
    painter.draw_text_wrapped(
        Rect::new(body.x + 40, body.y + 46, body.width.saturating_sub(40), 50),
        "If an open program is associated with this process, it will close and you could lose unsaved data.",
        MUTED,
        1,
    );
}

fn draw_logoff_dialog(painter: &mut Painter<'_, '_>, body: Rect) {
    paint_icon(
        painter,
        Rect::new(body.x, body.y + 4, 28, 28),
        Icon::LogOut,
        Color::rgb(0, 100, 190),
    );
    painter.draw_text_wrapped(
        Rect::new(body.x + 40, body.y, body.width.saturating_sub(40), 40),
        "Logging off closes Task Manager and returns to the desktop session.",
        TEXT,
        1,
    );
    painter.draw_text_wrapped(
        Rect::new(body.x + 40, body.y + 44, body.width.saturating_sub(40), 40),
        "No other session will be affected.",
        MUTED,
        1,
    );
}

fn draw_text_field(painter: &mut Painter<'_, '_>, rect: Rect, text: &str, focused: bool) {
    painter.fill_rect(rect, FIELD);
    painter.stroke_rect(
        rect,
        1,
        if focused {
            Color::rgb(40, 100, 190)
        } else {
            FIELD_BORDER
        },
    );
    painter.draw_text(rect.x + 4, rect.y + 7, text, TEXT, 1);
    if focused {
        let width = painter.text_width(text, 1);
        let x = (rect.x + 4 + width).min(rect.right_i32() - 3);
        painter.fill_rect(
            Rect::new(x + 1, rect.y + 5, 1, rect.height.saturating_sub(8)),
            TEXT,
        );
    }
}

fn draw_checkbox(painter: &mut Painter<'_, '_>, rect: Rect, checked: bool) {
    painter.fill_rect(rect, FIELD);
    painter.stroke_rect(rect, 1, FIELD_BORDER);
    if checked {
        paint_icon(
            painter,
            Rect::new(rect.x + 2, rect.y + 2, 10, 10),
            Icon::Check,
            Color::rgb(20, 90, 200),
        );
    }
}

fn draw_close_x(painter: &mut Painter<'_, '_>, rect: Rect, hovered: bool) {
    painter.fill_rounded_rect(
        rect,
        3,
        if hovered {
            Color::rgb(232, 78, 58)
        } else {
            Color::rgb(214, 58, 40)
        },
    );
    let cx = rect.x + rect.width as i32 / 2;
    let cy = rect.y + rect.height as i32 / 2;
    painter.line(cx - 3, cy - 3, cx + 4, cy + 4, Color::WHITE);
    painter.line(cx + 4, cy - 3, cx - 3, cy + 4, Color::WHITE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages_are_bounded() {
        assert_eq!(percentage(0, 0), 0);
        assert_eq!(percentage(0, 100), 0);
        assert_eq!(percentage(50, 100), 50);
        assert_eq!(percentage(100, 100), 100);
        assert_eq!(percentage(500, 100), 100);
    }

    #[test]
    fn durations_are_formatted_like_a_task_manager() {
        assert_eq!(format_duration(0).as_str(), "0:00");
        assert_eq!(format_duration(65_000).as_str(), "1:05");
        assert_eq!(format_duration(3_725_000).as_str(), "1:02:05");
    }

    #[test]
    fn memory_is_scaled() {
        assert_eq!(format_memory(512).as_str(), "512 B");
        assert_eq!(format_memory(1536).as_str(), "2 K");
        assert_eq!(format_memory(2 * 1024 * 1024).as_str(), "2.0 MB");
    }

    #[test]
    fn history_ring_wraps() {
        let mut history = [0u8; HISTORY];
        let mut len = 0;
        let mut head = 0;
        for index in 0..(HISTORY + 3) {
            push_history(&mut history, &mut len, &mut head, (index % 100) as u8);
        }
        assert_eq!(len, HISTORY);
        // The oldest retained sample is index 3.
        assert_eq!(history_at(&history, len, head, 0), 3);
        assert_eq!(
            history_at(&history, len, head, HISTORY - 1),
            (HISTORY + 2) as u8 % 100
        );
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use feos_app::{AppRegistry, BoardInfo, Processes, SpawnError};
    use feos_hal::{Clock, Cpu, Memory, Power};

    struct TestClock;
    impl Clock for TestClock {
        fn frequency(&self) -> u64 {
            62_500_000
        }
        fn ticks(&self) -> u64 {
            62_500_000
        }
        fn spin_ms(&self, _ms: u64) {}
    }

    struct TestPower;
    impl Power for TestPower {
        fn system_off(&self) -> ! {
            loop {}
        }
        fn system_reset(&self) -> ! {
            loop {}
        }
    }

    struct TestCpu;
    impl Cpu for TestCpu {
        fn current_el(&self) -> u64 {
            1
        }
        fn midr(&self) -> u64 {
            0x410f_d083
        }
        fn halt(&self) -> ! {
            loop {}
        }
    }

    struct TestMemory;
    impl feos_hal::Memory for TestMemory {
        fn info(&self) -> MemoryInfo {
            MemoryInfo {
                total_bytes: 32 * 1024 * 1024,
                used_bytes: 24 * 1024 * 1024,
                free_bytes: 8 * 1024 * 1024,
                heap_total_bytes: 16 * 1024 * 1024,
                heap_used_bytes: 6 * 1024 * 1024,
                heap_free_bytes: 10 * 1024 * 1024,
            }
        }
    }

    struct TestApps;
    impl AppRegistry for TestApps {
        fn find(&self, _name: &str) -> Option<&'static dyn App> {
            None
        }
    }

    fn name_bytes(name: &str) -> [u8; feos_app::PROCESS_NAME_MAX] {
        let mut out = [0u8; feos_app::PROCESS_NAME_MAX];
        let len = name.len().min(out.len());
        out[..len].copy_from_slice(&name.as_bytes()[..len]);
        out
    }

    struct TestProcesses;
    impl Processes for TestProcesses {
        fn spawn(&self, _app: &'static dyn App, _args: &str) -> Result<u32, SpawnError> {
            Err(SpawnError::NoFreeSlot)
        }
        fn kill(&self, _pid: u32) -> bool {
            false
        }
        fn current(&self) -> u32 {
            1
        }
        fn snapshot(&self, out: &mut [ProcessInfo]) -> usize {
            if out.is_empty() {
                return 0;
            }
            out[0] = ProcessInfo {
                pid: 1,
                state: ProcessState::Running,
                name: name_bytes("shell"),
                name_len: 5,
                ..ProcessInfo::EMPTY
            };
            if out.len() > 1 {
                out[1] = ProcessInfo {
                    pid: 2,
                    state: ProcessState::Ready,
                    name: name_bytes("tick"),
                    name_len: 4,
                    ..ProcessInfo::EMPTY
                };
                2
            } else {
                1
            }
        }
        fn system(&self) -> SystemInfo {
            SystemInfo {
                uptime_ms: 10_000,
                total_ticks: 1_000,
                idle_ticks: 250,
                ticks_per_second: 100,
                processes: 2,
                threads: 2,
                cpus: 2,
                cpu_ticks: [600, 400, 0, 0, 0, 0, 0, 0],
                cpu_idle_ticks: [150, 100, 0, 0, 0, 0, 0, 0],
            }
        }
    }

    static CLOCK: TestClock = TestClock;
    static POWER: TestPower = TestPower;
    static CPU: TestCpu = TestCpu;
    static MEMORY: TestMemory = TestMemory;
    static APPS: TestApps = TestApps;
    static PROCESSES: TestProcesses = TestProcesses;

    pub(super) fn platform() -> Platform {
        Platform {
            board: BoardInfo {
                name: "qemu-virt",
                console_base: 0x0900_0000,
                framebuffer_addr: 0x4000_0000,
                kernel_range: (0x4008_0000, 0x4020_0000),
                bss_range: (0x4010_0000, 0x4020_0000),
            },
            clock: &CLOCK,
            power: &POWER,
            cpu: &CPU,
            audio: None,
            pointer: None,
            keyboard: None,
            processes: &PROCESSES,
            memory: Some(&MEMORY),
            apps: &APPS,
            filesystem: None,
        }
    }

    pub(super) fn test_state(screen: Rect) -> State {
        let mut state = State::new(screen);
        state.row_count = 2;
        state.rows[0] = Row {
            pid: 1,
            name: Text::from_str("shell"),
            state: ProcessState::Running,
            cpu_percent: 42,
            cpu_ticks: 4_200,
            cpu_time_ms: 42_000,
            memory_bytes: 16 * 1024,
            threads: 1,
            priority: 8,
            started_ms: 0,
        };
        state.rows[1] = Row {
            pid: 2,
            name: Text::from_str("tick"),
            state: ProcessState::Ready,
            cpu_percent: 18,
            cpu_ticks: 1_800,
            cpu_time_ms: 18_000,
            memory_bytes: 16 * 1024,
            threads: 1,
            priority: 8,
            started_ms: 1_000,
        };
        state.selected_pid = Some(2);
        state.cursor = 1;
        state.system = SystemInfo {
            uptime_ms: 123_000,
            total_ticks: 12_300,
            idle_ticks: 4_000,
            ticks_per_second: 100,
            processes: 2,
            threads: 2,
            cpus: 2,
            cpu_ticks: [600, 400, 0, 0, 0, 0, 0, 0],
            cpu_idle_ticks: [150, 100, 0, 0, 0, 0, 0, 0],
        };
        state.memory = MEMORY.info();
        state.commit_total = 3 * 1024 * 1024;
        state.commit_limit = 16 * 1024 * 1024;
        state.commit_peak = 4 * 1024 * 1024;
        state.cpu_percent = 67;
        state.memory_percent = 19;
        state.cpu_len = 20;
        state.mem_len = 20;
        state.cpu_history = [55; HISTORY];
        state.mem_history = [19; HISTORY];
        state
    }

    #[test]
    fn memory_percent_tracks_physical_memory() {
        let screen = Rect::new(0, 0, 1024, 768);
        let mut state = test_state(screen);
        state.sample(&platform(), 1_000);
        // TestMemory reports 24 MiB used of 32 MiB total.
        assert_eq!(state.memory_percent, 75);
        assert_eq!(
            state.mem_history[state.mem_head.saturating_sub(1) % HISTORY],
            75
        );
    }

    #[test]
    fn sampling_tracks_usage_for_each_core() {
        let screen = Rect::new(0, 0, 1024, 768);
        let mut state = test_state(screen);
        state.sample(&platform(), 1_000);
        // TestProcesses reports 600/150 ticks for CPU 0 and 400/100 for CPU 1.
        assert_eq!(state.cpu_core_percent[0], 75);
        assert_eq!(state.cpu_core_percent[1], 75);
        assert_eq!(state.cpu_core_percent[2], 0);
        assert_eq!(state.cpu_core_len[0], 1);
        assert_eq!(state.cpu_core_len[1], 1);
        assert_eq!(
            state.cpu_core_history[0][state.cpu_core_head[0].saturating_sub(1) % HISTORY],
            75
        );
    }

    #[test]
    fn performance_shows_both_graphs() {
        let screen = Rect::new(0, 0, 1024, 768);
        let mut state = test_state(screen);
        state.tab = Tab::Performance;
        let platform = platform();
        let mut pixels = vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();
        let mut painter = Painter::new(&mut canvas);
        let layout = Layout::compute(screen, &state);
        draw_backdrop(&mut painter, &state);
        draw_window(&mut painter, &state, layout, 1_000);
        draw_tabs(&mut painter, &state, layout);
        draw_content(&mut painter, &platform, &mut state, layout, 1_000);

        let mut cpu_pixels = 0;
        let mut memory_pixels = 0;
        for pixel in &pixels {
            if *pixel == GRAPH_CPU.raw() {
                cpu_pixels += 1;
            }
            if *pixel == GRAPH_MEM.raw() {
                memory_pixels += 1;
            }
        }
        assert!(cpu_pixels > 0, "CPU graph was not drawn");
        assert!(memory_pixels > 0, "memory graph was not drawn");
    }

    #[test]
    fn performance_can_show_per_core_graphs() {
        let screen = Rect::new(0, 0, 1024, 768);
        let mut state = test_state(screen);
        state.tab = Tab::Performance;
        state.cpu_view = CpuView::PerCore;
        state.cpu_percent = 0;
        state.cpu_len = 0;
        state.system.cpus = 2;
        for cpu in 0..2 {
            state.cpu_core_percent[cpu] = 40 + cpu as u8 * 20;
            state.cpu_core_len[cpu] = HISTORY;
            for index in 0..HISTORY {
                state.cpu_core_history[cpu][index] = ((index * 5 + cpu * 17) % 80) as u8;
            }
        }

        let platform = platform();
        let mut pixels = vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();
        let mut painter = Painter::new(&mut canvas);
        let layout = Layout::compute(screen, &state);
        draw_backdrop(&mut painter, &state);
        draw_window(&mut painter, &state, layout, 1_000);
        draw_tabs(&mut painter, &state, layout);
        draw_content(&mut painter, &platform, &mut state, layout, 1_000);

        assert!(
            pixels.iter().any(|pixel| *pixel == GRAPH_CPU.raw()),
            "per-core CPU graphs were not drawn"
        );
    }

    #[test]
    fn performance_keeps_memory_graph_in_a_narrow_window() {
        let screen = Rect::new(0, 0, 420, 420);
        let mut state = test_state(screen);
        state.tab = Tab::Performance;
        let platform = platform();
        let mut pixels = vec![0u32; 420 * 420];
        let mut canvas = Canvas::new(&mut pixels, 420, 420, 420).unwrap();
        let mut painter = Painter::new(&mut canvas);
        let layout = Layout::compute(screen, &state);
        draw_backdrop(&mut painter, &state);
        draw_window(&mut painter, &state, layout, 1_000);
        draw_tabs(&mut painter, &state, layout);
        draw_content(&mut painter, &platform, &mut state, layout, 1_000);
        assert!(
            pixels.iter().any(|pixel| *pixel == GRAPH_MEM.raw()),
            "memory graph was squeezed out of a narrow window"
        );
    }

    #[test]
    fn every_tab_and_dialog_renders_without_panicking() {
        let screen = Rect::new(0, 0, 1024, 768);
        let mut state = test_state(screen);
        let platform = platform();
        let mut pixels = vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();
        let mut painter = Painter::new(&mut canvas);
        let layout = Layout::compute(screen, &state);

        draw_backdrop(&mut painter, &state);
        draw_window(&mut painter, &state, layout, 1_000);
        draw_tabs(&mut painter, &state, layout);
        for tab in Tab::ALL {
            state.tab = tab;
            draw_content(&mut painter, &platform, &mut state, layout, 1_000);
        }
        draw_status(&mut painter, &state, layout);

        state.menu = Some(Menu::Options);
        draw_menu(&mut painter, &state, layout, Menu::Options);
        state.menu = None;
        state.context = Some(ContextMenu {
            tab: Tab::Processes,
            x: 300,
            y: 300,
            hover: 0,
        });
        draw_context(&mut painter, &state, layout, state.context.unwrap());
        state.context = None;

        for dialog in [
            Dialog::Run,
            Dialog::About,
            Dialog::Columns,
            Dialog::Help,
            Dialog::Properties(2),
            Dialog::EndProcess {
                pid: 2,
                tab: Tab::Processes,
            },
            Dialog::LogOff,
        ] {
            state.dialog = dialog;
            draw_dialog(&mut painter, &platform, &state, layout, 1_000);
        }
    }

    #[test]
    fn minimum_window_layout_has_room_for_default_columns() {
        let screen = Rect::new(0, 0, 720, 520);
        let state = test_state(screen);
        let layout = Layout::compute(screen, &state);
        let list = list_layout(layout);
        let width: i32 = Column::ALL
            .iter()
            .enumerate()
            .filter(|(index, _)| state.column_visible[*index])
            .map(|(_, column)| column.width())
            .sum();
        assert!(
            width <= list.rows.width as i32,
            "{width} > {}",
            list.rows.width
        );
    }
}

#[cfg(test)]
mod preview_tests {
    use super::render_tests::*;
    use super::*;

    fn write_ppm(path: &str, pixels: &[u32], width: usize, height: usize) {
        let mut out = std::vec::Vec::with_capacity(width * height * 3 + 32);
        out.extend_from_slice(b"P6\n");
        out.extend_from_slice(std::format!("{width} {height}\n255\n").as_bytes());
        for pixel in pixels.iter().take(width * height) {
            let value = *pixel;
            out.push(((value >> 16) & 0xff) as u8);
            out.push(((value >> 8) & 0xff) as u8);
            out.push((value & 0xff) as u8);
        }
        std::fs::write(path, out).expect("write preview");
    }

    fn preview_state(screen: Rect) -> State {
        let mut state = test_state(screen);
        let names = ["shell", "tick", "player", "files", "tone", "desktop"];
        let percents = [34u8, 18, 12, 7, 4, 2];
        state.row_count = names.len();
        for index in 0..names.len() {
            let pid = index as u32 + 1;
            state.rows[index] = Row {
                pid,
                name: Text::from_str(names[index]),
                state: if index == 0 {
                    ProcessState::Running
                } else {
                    ProcessState::Ready
                },
                cpu_percent: percents[index],
                cpu_ticks: percents[index] as u64 * 120,
                cpu_time_ms: percents[index] as u64 * 1_200,
                memory_bytes: 16 * 1024,
                threads: 1,
                priority: 8,
                started_ms: index as u64 * 10_000,
            };
        }
        state.selected_pid = Some(3);
        state.cursor = 2;
        state.cpu_percent = 77;
        state.memory_percent = 23;
        state.system.processes = names.len();
        state.system.threads = names.len();
        for index in 0..HISTORY {
            let wave = ((index * 7) % 43) as u8 + 25;
            state.cpu_history[index] = wave.min(96);
            state.mem_history[index] = (18 + (index * 3) % 21) as u8;
        }
        state.cpu_len = HISTORY;
        state.mem_len = HISTORY;
        state
    }

    #[test]
    fn render_preview_frames_when_requested() {
        if std::env::var_os("FEOS_TASKMGR_PREVIEW").is_none() {
            return;
        }
        let screen = Rect::new(0, 0, 1024, 768);
        let platform = platform();
        let mut pixels = vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();
        let _ = std::fs::create_dir_all("/tmp/opencode");

        for (name, tab) in [
            ("applications", Tab::Applications),
            ("processes", Tab::Processes),
            ("performance", Tab::Performance),
            ("networking", Tab::Networking),
            ("users", Tab::Users),
        ] {
            let mut state = preview_state(screen);
            state.tab = tab;
            render(&mut canvas, &platform, &mut state, 12_345);
            write_ppm(
                &std::format!("/tmp/opencode/feos-taskmgr-{name}.ppm"),
                canvas.pixels(),
                1024,
                768,
            );
        }

        let mut state = preview_state(screen);
        state.tab = Tab::Performance;
        state.cpu_view = CpuView::PerCore;
        state.system.cpus = 4;
        for cpu in 0..4 {
            state.cpu_core_percent[cpu] = 34 + cpu as u8 * 9;
            state.cpu_core_len[cpu] = HISTORY;
            for index in 0..HISTORY {
                state.cpu_core_history[cpu][index] = ((index * (cpu + 3) + cpu * 17) % 82) as u8;
            }
        }
        render(&mut canvas, &platform, &mut state, 12_345);
        write_ppm(
            "/tmp/opencode/feos-taskmgr-performance-per-core.ppm",
            canvas.pixels(),
            1024,
            768,
        );

        let mut state = preview_state(screen);
        state.tab = Tab::Processes;
        state.dialog = Dialog::Properties(3);
        render(&mut canvas, &platform, &mut state, 12_345);
        write_ppm(
            "/tmp/opencode/feos-taskmgr-properties.ppm",
            canvas.pixels(),
            1024,
            768,
        );

        state.dialog = Dialog::None;
        state.menu = Some(Menu::Options);
        render(&mut canvas, &platform, &mut state, 12_345);
        write_ppm(
            "/tmp/opencode/feos-taskmgr-menu.ppm",
            canvas.pixels(),
            1024,
            768,
        );
    }
}

#[cfg(test)]
mod model_tests {
    use super::render_tests::{platform, test_state};
    use super::*;

    #[test]
    fn sampling_and_sorting_keep_the_table_consistent() {
        let screen = Rect::new(0, 0, 1024, 768);
        let platform = platform();
        let mut state = State::new(screen);
        state.sample(&platform, 1_000);
        assert_eq!(state.row_count, 2);
        assert_eq!(state.system.processes, 2);
        assert!(state.selected_pid.is_some());
        assert_eq!(state.cpu_len, 1);
        assert_eq!(state.mem_len, 1);

        state.sort = SortKey::Name;
        state.sort_desc = false;
        state.sort_rows();
        assert_eq!(state.rows[0].name.as_str(), "shell");
        state.sort_desc = true;
        state.sort_rows();
        assert_eq!(state.rows[0].name.as_str(), "tick");

        state.set_sort(SortKey::Pid);
        assert_eq!(state.rows[0].pid, 1);
        state.set_sort(SortKey::Pid);
        assert_eq!(state.rows[0].pid, 2);
    }

    #[test]
    fn cursor_movement_clamps_to_the_rows() {
        let mut state = test_state(Rect::new(0, 0, 1024, 768));
        state.move_cursor(-10);
        assert_eq!(state.cursor, 0);
        assert_eq!(state.selected_pid, Some(1));
        state.move_cursor(10);
        assert_eq!(state.cursor, 1);
        assert_eq!(state.selected_pid, Some(2));
    }
}

#[cfg(test)]
mod bench_tests {
    use super::render_tests::{platform, test_state};
    use super::*;

    #[test]
    fn benchmark_full_frame_when_requested() {
        if std::env::var_os("FEOS_TASKMGR_BENCH").is_none() {
            return;
        }
        let screen = Rect::new(0, 0, 1024, 768);
        let platform = platform();
        let mut state = test_state(screen);
        state.tab = Tab::Processes;
        let mut pixels = vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();
        // Warm up.
        for _ in 0..3 {
            render(&mut canvas, &platform, &mut state, 12_345);
        }
        let start = std::time::Instant::now();
        let frames = 100;
        for _ in 0..frames {
            render(&mut canvas, &platform, &mut state, 12_345);
        }
        let elapsed = start.elapsed();
        std::println!(
            "render: {:.3} ms/frame ({} frames in {:?})",
            elapsed.as_secs_f64() * 1000.0 / frames as f64,
            frames,
            elapsed
        );
    }
}

#[cfg(test)]
mod stage_bench_tests {
    use super::render_tests::{platform, test_state};
    use super::*;

    #[test]
    fn benchmark_stages_when_requested() {
        if std::env::var_os("FEOS_TASKMGR_BENCH").is_none() {
            return;
        }
        let screen = Rect::new(0, 0, 1024, 768);
        let platform = platform();
        let mut state = test_state(screen);
        state.tab = Tab::Processes;
        let mut pixels = vec![0u32; 1024 * 768];
        let mut front_pixels = vec![0u32; 1024 * 768];
        let mut canvas = Canvas::new(&mut pixels, 1024, 768, 1024).unwrap();
        let mut front = Canvas::new(&mut front_pixels, 1024, 768, 1024).unwrap();
        let layout = Layout::compute(screen, &state);
        let rounds = 50;
        let mut backdrop = std::time::Duration::ZERO;
        let mut window = std::time::Duration::ZERO;
        let mut tabs = std::time::Duration::ZERO;
        let mut content = std::time::Duration::ZERO;
        let mut status = std::time::Duration::ZERO;
        let mut copy = std::time::Duration::ZERO;
        for _ in 0..rounds {
            let start = std::time::Instant::now();
            {
                let mut painter = Painter::new(&mut canvas);
                draw_backdrop(&mut painter, &state);
            }
            backdrop += start.elapsed();
            let start = std::time::Instant::now();
            {
                let mut painter = Painter::new(&mut canvas);
                draw_window(&mut painter, &state, layout, 1_000);
            }
            window += start.elapsed();
            let start = std::time::Instant::now();
            {
                let mut painter = Painter::new(&mut canvas);
                draw_tabs(&mut painter, &state, layout);
            }
            tabs += start.elapsed();
            let start = std::time::Instant::now();
            {
                let mut painter = Painter::new(&mut canvas);
                draw_content(&mut painter, &platform, &mut state, layout, 1_000);
            }
            content += start.elapsed();
            let start = std::time::Instant::now();
            {
                let mut painter = Painter::new(&mut canvas);
                draw_status(&mut painter, &state, layout);
            }
            status += start.elapsed();
            let start = std::time::Instant::now();
            front.copy_from(&canvas);
            copy += start.elapsed();
        }
        std::println!(
            "backdrop {:.3} ms",
            backdrop.as_secs_f64() * 1000.0 / rounds as f64
        );
        std::println!(
            "window   {:.3} ms",
            window.as_secs_f64() * 1000.0 / rounds as f64
        );
        std::println!(
            "tabs     {:.3} ms",
            tabs.as_secs_f64() * 1000.0 / rounds as f64
        );
        std::println!(
            "content  {:.3} ms",
            content.as_secs_f64() * 1000.0 / rounds as f64
        );
        std::println!(
            "status   {:.3} ms",
            status.as_secs_f64() * 1000.0 / rounds as f64
        );
        std::println!(
            "copy     {:.3} ms",
            copy.as_secs_f64() * 1000.0 / rounds as f64
        );
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    #[test]
    fn cursor_restore_leaves_the_buffer_untouched() {
        let background = Color::rgb(10, 40, 80);
        let mut pixels = vec![background.raw(); 64 * 64];
        let before = pixels.clone();
        let mut canvas = Canvas::new(&mut pixels, 64, 64, 64).unwrap();
        let mut cursor = Cursor::new();

        cursor.move_to(
            &mut canvas,
            Point::new(10, 10),
            feos_gui::CursorShape::Arrow,
        );
        assert_ne!(canvas.pixel(10, 10), Some(background));

        cursor.move_to(
            &mut canvas,
            Point::new(32, 24),
            feos_gui::CursorShape::SizeHorizontal,
        );
        cursor.restore(&mut canvas);
        assert_eq!(canvas.pixels(), before.as_slice());
    }

    #[test]
    fn hover_signature_tracks_rows_and_buttons() {
        let mut state = super::render_tests::test_state(Rect::new(0, 0, 1024, 768));
        state.tab = Tab::Processes;
        let layout = Layout::compute(state.screen, &state);
        let list = list_layout(layout);

        state.pointer = Point::new(list.rows.x + 5, list.rows.y + 3);
        let first = state.hover_signature();
        state.pointer = Point::new(list.rows.x + 5, list.rows.y + ROW_HEIGHT + 3);
        let second = state.hover_signature();
        assert_ne!(first, second);

        state.pointer = Point::new(list.header.x + 5, list.header.y + 5);
        assert!(state.hover_signature() >= 200);

        let button = button_rect(list.buttons, 0, 2);
        state.pointer = Point::new(button.x + 4, button.y + 4);
        assert!(state.hover_signature() >= 500);
    }
}
