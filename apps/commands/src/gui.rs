//! `gui`: render the feos-gui dashboard demo.
//!
//! The current QEMU board has no mouse or keyboard device yet, so this app
//! renders a complete frame from static demo state. The same widgets, layout
//! engine, charts and windows are driven by real [`Input`] events as soon as a
//! platform driver feeds them into the toolkit.

use feos_app::{App, Context};
use feos_console::println;
use feos_gfx::{Canvas, Rect};
use feos_gui::prelude::*;
use feos_gui::{InteractionState, TextBuffer, WindowState};

/// Demo state that must survive between invocations of the app.
struct DemoState {
    interaction: InteractionState<24>,
    name: TextBuffer<48>,
    volume: f32,
    notifications: bool,
    compact: bool,
    tab: usize,
    selected: usize,
    clicks: u32,
    window: WindowState,
    dialog_open: bool,
}

impl DemoState {
    const fn new() -> Self {
        Self {
            interaction: InteractionState::new(),
            name: TextBuffer::new(),
            volume: 0.62,
            notifications: true,
            compact: false,
            tab: 0,
            selected: 1,
            clicks: 0,
            window: WindowState::new(Rect::new(560, 150, 420, 330)),
            dialog_open: false,
        }
    }
}

static mut DEMO: DemoState = DemoState::new();

/// Renders the GUI showcase.
pub struct Gui;

impl App for Gui {
    fn name(&self) -> &'static str {
        "gui"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["desktop", "dashboard"]
    }

    fn usage(&self) -> &'static str {
        "gui"
    }

    fn description(&self) -> &'static str {
        "draw the feos-gui dashboard demo"
    }

    fn run(&self, context: &mut Context<'_>) {
        match context.display.as_mut() {
            Some(canvas) => {
                // SAFETY: foreground graphics apps run one at a time in the
                // shell; no other process touches the demo state.
                let state = unsafe { &mut *core::ptr::addr_of_mut!(DEMO) };
                render(canvas, state);
                println!(
                    "drew the feos-gui dashboard on a {}x{} ramfb framebuffer",
                    canvas.width(),
                    canvas.height()
                );
            }
            None => println!("no framebuffer; restart QEMU with -device ramfb"),
        }
    }
}

fn render(canvas: &mut Canvas, state: &mut DemoState) {
    let theme = Theme::dark();
    let input = Input::new();
    let DemoState {
        interaction,
        name,
        volume,
        notifications,
        compact,
        tab,
        selected,
        clicks,
        window,
        dialog_open,
    } = state;

    let mut ui = Ui::new(canvas, &theme, &input, interaction);
    let full = ui.canvas_rect();
    ui.desktop_background(full);

    let palette = theme.palette;
    let metrics = theme.metrics;
    let width = full.width as i32;
    let height = full.height as i32;

    // Top bar -------------------------------------------------------------
    let top = Rect::new(0, 0, full.width, 54);
    ui.painter.fill_rect_alpha(top, palette.surface, 236);
    ui.painter
        .fill_rect(Rect::new(0, 53, full.width, 1), palette.border);
    ui.row(Rect::new(20, 9, (width - 40).max(0) as u32, 36), 12, |ui| {
        ui.badge("FEOS", palette.accent);
        ui.heading("Control Center");
        ui.label_muted("QEMU aarch64 / ramfb / XRGB8888");
    });

    // Sidebar -------------------------------------------------------------
    let sidebar = Rect::new(20, 74, 210, (height - 100).max(0) as u32);
    ui.panel_with_padding(sidebar, Padding::all(16), |ui| {
        ui.label_muted("WORKSPACE");
        for (index, label) in ["Overview", "Charts", "Widgets", "Inspector"]
            .iter()
            .enumerate()
        {
            let response = if *tab == index {
                ui.primary_button(label)
            } else {
                ui.button(label)
            };
            if response.clicked {
                *tab = index;
                if index == 3 {
                    window.open = true;
                }
            }
        }

        ui.separator();
        ui.label_muted("SYSTEM");
        ui.checkbox(notifications, "Notifications");
        ui.checkbox(compact, "Compact density");
        if ui.button("About").clicked {
            *dialog_open = true;
        }
        if ui.button("Reset").clicked {
            *clicks = 0;
        }
        ui.space(metrics.section_gap);
        ui.badge("GUI v0.1", palette.success);
        ui.label_muted("widgets / layout / charts");
    });

    // Main content --------------------------------------------------------
    let main = Rect::new(
        250,
        74,
        (width - 270).max(0) as u32,
        (height - 100).max(0) as u32,
    );
    ui.column(main, 14, |ui| {
        let header = ui.allocate(0, 36);
        ui.row(header, 12, |ui| {
            ui.heading("Overview");
            ui.badge("LIVE", palette.success);
            ui.label_muted("all systems nominal");
            if ui.button_sized(130, "New window").clicked {
                window.open = true;
            }
        });

        // Stat cards.
        let cards = [
            ("CPU", "42%", "4 cores at 1.2 GHz", palette.accent, 0.42f32),
            (
                "MEMORY",
                "318 MB",
                "of 1.0 GB mapped",
                palette.warning,
                0.31,
            ),
            (
                "NETWORK",
                "2.4 Mb/s",
                "loopback + virtio",
                palette.success,
                0.24,
            ),
        ];
        let grid_rect = ui.allocate(0, 104);
        ui.grid(grid_rect, 3, 104, 12, |ui| {
            for (title, value, detail, _color, progress) in cards {
                let cell = ui.allocate(0, 0);
                ui.panel(cell, |ui| {
                    ui.label_muted(title);
                    ui.heading(value);
                    ui.label_muted(detail);
                    ui.progress(progress);
                });
            }
        });

        // Charts.
        let chart_rect = ui.allocate(0, 248);
        let gap = 14;
        let left_width = ((chart_rect.width as i32 - gap) * 3 / 5).max(0);
        let (left, rest) = chart_rect.split_left(left_width);
        let right = Rect::new(
            rest.x + gap,
            rest.y,
            rest.width.saturating_sub(gap as u32),
            rest.height,
        );

        const THROUGHPUT: [ChartPoint; 12] = [
            ChartPoint::new(0.0, 22.0),
            ChartPoint::new(1.0, 38.0),
            ChartPoint::new(2.0, 31.0),
            ChartPoint::new(3.0, 52.0),
            ChartPoint::new(4.0, 44.0),
            ChartPoint::new(5.0, 68.0),
            ChartPoint::new(6.0, 61.0),
            ChartPoint::new(7.0, 78.0),
            ChartPoint::new(8.0, 70.0),
            ChartPoint::new(9.0, 86.0),
            ChartPoint::new(10.0, 74.0),
            ChartPoint::new(11.0, 92.0),
        ];
        const SESSIONS: [ChartPoint; 9] = [
            ChartPoint::new(0.0, 12.0),
            ChartPoint::new(1.0, 26.0),
            ChartPoint::new(2.0, 18.0),
            ChartPoint::new(3.0, 34.0),
            ChartPoint::new(4.0, 30.0),
            ChartPoint::new(5.0, 47.0),
            ChartPoint::new(6.0, 41.0),
            ChartPoint::new(7.0, 56.0),
            ChartPoint::new(8.0, 63.0),
        ];
        let throughput = [Series::line(&THROUGHPUT, palette.accent)];
        let sessions = [Series::area(&SESSIONS, palette.success)];
        let throughput_chart = Chart::new(&throughput)
            .x_range(0.0, 11.0)
            .y_range(0.0, 100.0)
            .grid(4);
        let sessions_chart = Chart::new(&sessions)
            .x_range(0.0, 8.0)
            .y_range(0.0, 70.0)
            .grid(4);

        ui.card(left, "Throughput", |ui| {
            let rect = ui.allocate(0, 170);
            ui.chart(rect, &throughput_chart);
        });
        ui.card(right, "Sessions", |ui| {
            let rect = ui.allocate(0, 170);
            ui.chart(rect, &sessions_chart);
        });

        // Preferences.
        let controls = ui.allocate(0, 150);
        ui.panel(controls, |ui| {
            ui.label_muted("PREFERENCES");
            let row = ui.allocate(0, metrics.control_height);
            ui.row(row, 12, |ui| {
                ui.text_input_with_placeholder(name, "display name");
                ui.slider(volume, 0.0, 1.0);
                ui.checkbox(notifications, "Notifications");
            });

            let row = ui.allocate(0, metrics.control_height);
            ui.row(row, 12, |ui| {
                if ui.button_sized(120, "Apply").clicked {
                    *clicks = clicks.saturating_add(1);
                }
                if ui.button_sized(120, "Open panel").clicked {
                    window.open = true;
                }
                ui.label_muted("fixed-capacity text, no allocator");
            });
        });
    });

    // Status line.
    let status = Rect::new(20, (height - 22).max(0), (width - 40).max(0) as u32, 10);
    ui.painter.draw_text(
        status.x,
        status.y,
        "immediate-mode · clipped painter · themed widgets · drag/resize · charts",
        palette.text_muted,
        metrics.small_scale,
    );

    // Floating inspector window.
    if window.open {
        let response = ui.window(window, "Inspector", |ui| {
            ui.label_muted("LIVE STATE");
            ui.heading("GUI works");
            ui.paragraph(
                "This window is draggable by its title bar and resizable from the lower-right corner.",
            );
            ui.separator();
            ui.checkbox(notifications, "Notifications enabled");
            ui.slider(volume, 0.0, 1.0);
            ui.text_input(name);
            ui.label_muted("Changes are local to this demo frame.");
        });
        let _ = response;
    }

    // About dialog.
    if *dialog_open {
        let mut open = true;
        let mut close = false;
        ui.dialog(
            Rect::new(300, 210, 430, 230),
            "About feos-gui",
            &mut open,
            |ui| {
                ui.heading("feos-gui 0.1");
                ui.paragraph(
                    "A dependency-free immediate-mode GUI toolkit for feos. \
                     Widgets, layouts, charts, windows and input routing share \
                     one clipped painter and a bounded state table.",
                );
                ui.separator();
                let row = ui.allocate(0, metrics.control_height);
                ui.row(row, 12, |ui| {
                    if ui.button_sized(120, "Close").clicked {
                        close = true;
                    }
                    ui.label_muted("no heap, no unsafe");
                });
            },
        );
        if close || !open {
            *dialog_open = false;
        }
    }

    let _ = (selected, compact, clicks);
}
