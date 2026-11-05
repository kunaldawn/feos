//! `gui`: the feos-gui dashboard and component gallery.
//!
//! On the desktop this is a windowed app driven by real input, which makes it
//! a live test of every widget. From the shell it draws one frame from the same
//! state on the framebuffer.

use feos_app::{App, Context, WindowContext, WindowFrame};
use feos_console::println;
use feos_gfx::{Canvas, Rect};
use feos_gui::prelude::*;
use feos_gui::{InteractionState, ScrollState, TextBuffer, WindowState};

/// Demo state that must survive between invocations of the app.
struct DemoState {
    /// This window's events, in content coordinates.
    input: Input,
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
    select: SelectState,
    select_value: usize,
    menu: MenuState,
    accordion: AccordionState<6>,
    split: SplitState,
    tree: TreeState<12>,
    rating: u8,
    page: usize,
    color: usize,
    stepper: f32,
    crumb: usize,
    step: usize,
    nav: NavigationViewState,
    fluent_tabs: TabViewState<3>,
    menu_bar: MenuBarState,
    command_bar: CommandBarState,
    list: ListState<8>,
    info_open: bool,
    grid_selected: usize,
    suggest: AutoSuggestState,
    calendar: CalendarState,
    tags: TagSet<6, 16>,
    tag_draft: TextBuffer<48>,
    password: TextBuffer<32>,
    notes: TextBuffer<128>,
    range: RangeValue,
    range_state: RangeSliderState,
    knob: f32,
    zoom: ZoomState,
    log_scroll: ScrollState,
    swipe: SwipeState,
    pull: PullState,
    sheet_open: bool,
    confirm_open: bool,
    command_palette: CommandPaletteState,
    flyout_menu: MenuState,
    hotkey: Hotkey,
    pattern_tab: usize,
    pivot_tab: usize,
}

impl DemoState {
    const fn new() -> Self {
        Self {
            input: Input::new(),
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
            select: SelectState::new(),
            select_value: 0,
            menu: MenuState::new(),
            accordion: AccordionState::new(),
            split: SplitState::new(),
            tree: TreeState::new(),
            rating: 3,
            page: 0,
            color: 0,
            stepper: 40.0,
            crumb: 1,
            step: 1,
            nav: NavigationViewState::new(),
            fluent_tabs: TabViewState::from_titles(["Overview", "Details", "Activity"]),
            menu_bar: MenuBarState::new(),
            command_bar: CommandBarState::new(),
            list: ListState::new(),
            info_open: true,
            grid_selected: 1,
            suggest: AutoSuggestState::new(),
            calendar: CalendarState::new(2026, 10, 30),
            tags: TagSet::new(),
            tag_draft: TextBuffer::new(),
            password: TextBuffer::new(),
            notes: TextBuffer::new(),
            range: RangeValue::new(24.0, 76.0),
            range_state: RangeSliderState::new(),
            knob: 42.0,
            zoom: ZoomState::new(1.0),
            log_scroll: ScrollState::new(),
            swipe: SwipeState::new(),
            pull: PullState::new(),
            sheet_open: false,
            confirm_open: false,
            command_palette: CommandPaletteState::new(),
            flyout_menu: MenuState::new(),
            hotkey: Hotkey::new(),
            pattern_tab: 0,
            pivot_tab: 0,
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
        &["dashboard"]
    }

    fn usage(&self) -> &'static str {
        "gui"
    }

    fn description(&self) -> &'static str {
        "draw the feos-gui dashboard and component gallery"
    }

    fn windowed(&self) -> bool {
        true
    }

    fn open_window(&self, _context: &WindowContext<'_, '_>) -> bool {
        // SAFETY: the desktop runs hosted apps on its one thread, and a bundle
        // has one window, so no other reference to the demo state exists.
        let state = unsafe { &mut *core::ptr::addr_of_mut!(DEMO) };
        *state = DemoState::new();
        true
    }

    fn frame_window(&self, context: &mut WindowContext<'_, '_>) -> WindowFrame {
        // SAFETY: as in `open_window`.
        let state = unsafe { &mut *core::ptr::addr_of_mut!(DEMO) };
        localise_input(context, &mut state.input);
        render(context.canvas, state, context.platform.board.arch);
        WindowFrame::Continue
    }

    fn needs_display(&self) -> bool {
        true
    }

    fn run(&self, context: &mut Context<'_>) -> i32 {
        match context.display.as_mut() {
            Some(canvas) => {
                // SAFETY: foreground graphics apps run one at a time in the
                // shell; no other process touches the demo state.
                let state = unsafe { &mut *core::ptr::addr_of_mut!(DEMO) };
                render(canvas, state, context.platform.board.arch);
                println!(
                    "drew the feos-gui dashboard on a {}x{} ramfb framebuffer",
                    canvas.width(),
                    canvas.height()
                );
            }
            None => {
                println!("no framebuffer; restart QEMU with -device ramfb");
                return 1;
            }
        }
        0
    }
}

/// Rebuilds this frame's events in content coordinates. Only events the desktop
/// routed to this window are kept, and the button state persists between frames.
fn localise_input(context: &WindowContext<'_, '_>, input: &mut Input) {
    input.clear_events();
    let origin = context.origin;
    let local = |pos: Point| Point::new(pos.x - origin.x, pos.y - origin.y);
    for event in context.input.events() {
        let translated = match event {
            Event::PointerMoved(pos) if context.pointer_active => {
                Some(Event::PointerMoved(local(pos)))
            }
            Event::PointerDown { pos, button } if context.pointer_active => {
                Some(Event::PointerDown {
                    pos: local(pos),
                    button,
                })
            }
            Event::PointerUp { pos, button } if context.pointer_active => Some(Event::PointerUp {
                pos: local(pos),
                button,
            }),
            Event::PointerWheel(delta) if context.pointer_active => {
                Some(Event::PointerWheel(delta))
            }
            Event::KeyDown(..) | Event::KeyUp(..) | Event::Text(_) | Event::ModifiersChanged(_)
                if context.keyboard_active =>
            {
                Some(event)
            }
            _ => None,
        };
        if let Some(event) = translated {
            input.push(event);
        }
    }
    if context.pointer_active {
        input.set_pointer(context.pointer());
    }
}

fn render(canvas: &mut Canvas, state: &mut DemoState, arch: &str) {
    let theme = Theme::dark();
    let DemoState {
        input,
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
        select,
        select_value,
        menu,
        accordion,
        split,
        tree,
        rating,
        page,
        color,
        stepper,
        crumb,
        step,
        nav,
        fluent_tabs,
        menu_bar,
        command_bar,
        list,
        info_open,
        grid_selected,
        suggest,
        calendar,
        tags,
        tag_draft,
        password,
        notes,
        range,
        range_state,
        knob,
        zoom,
        log_scroll,
        swipe,
        pull,
        sheet_open,
        confirm_open,
        command_palette,
        flyout_menu,
        hotkey,
        pattern_tab,
        pivot_tab,
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
        ui.label_muted(&alloc::format!("QEMU {arch} / ramfb / XRGB8888"));
        ui.avatar("KD", palette.accent, 28);
    });

    // Sidebar -------------------------------------------------------------
    let sidebar = Rect::new(20, 74, 210, (height - 100).max(0) as u32);
    ui.panel_with_padding(sidebar, Padding::all(16), |ui| {
        ui.label_muted("WORKSPACE");
        for (index, label) in [
            "Overview",
            "Charts",
            "Components",
            "Fluent",
            "Patterns",
            "Inspector",
        ]
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
                if index == 5 {
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
        let title = match *tab {
            0 => "Overview",
            1 => "Charts",
            2 => "Components",
            3 => "Fluent",
            4 => "Patterns",
            _ => "Inspector",
        };
        let header = ui.allocate(0, 36);
        ui.row(header, 12, |ui| {
            ui.heading(title);
            let (tag, subtitle) = match *tab {
                2 => ("KIT", "reusable pieces"),
                3 => ("DESKTOP", "Window-style controls"),
                4 => ("PATTERNS", "advanced composition"),
                _ => ("LIVE", "all systems nominal"),
            };
            ui.badge(tag, palette.success);
            ui.label_muted(subtitle);
            if ui.button_sized(130, "New window").clicked {
                window.open = true;
            }
        });

        if *tab == 3 {
            // Fluent gallery ----------------------------------------------
            let file_menu = [
                MenuEntry::shortcut("New", "Ctrl+N"),
                MenuEntry::shortcut("Open", "Ctrl+O"),
                MenuEntry::Separator,
                MenuEntry::shortcut("Save", "Ctrl+S"),
            ];
            let edit_menu = [
                MenuEntry::shortcut("Undo", "Ctrl+Z"),
                MenuEntry::shortcut("Redo", "Ctrl+Y"),
                MenuEntry::Separator,
                MenuEntry::disabled("Cut"),
                MenuEntry::shortcut("Copy", "Ctrl+C"),
            ];
            let view_menu = [MenuEntry::item("Compact"), MenuEntry::item("Full screen")];
            ui.menu_bar(
                menu_bar,
                &[
                    MenuBarMenu::new("File", &file_menu),
                    MenuBarMenu::new("Edit", &edit_menu),
                    MenuBarMenu::new("View", &view_menu),
                ],
            );

            ui.command_bar(
                command_bar,
                &[
                    CommandItem::new(Icon::Plus, "New"),
                    CommandItem::new(Icon::Edit, "Edit"),
                    CommandItem::new(Icon::Download, "Export"),
                    CommandItem::new(Icon::Trash, "Delete"),
                    CommandItem::new(Icon::Refresh, "Refresh"),
                    CommandItem::new(Icon::Settings, "Settings"),
                ],
            );

            let _ = ui.info_bar(
                Tone::Info,
                "Fluent components",
                "Menu bars, command bars, list and tab views, navigation panes, \
                 calendars, info badges and personas all follow the same \
                 caller-owned state rules.",
                Some("Learn more"),
                info_open,
            );

            let body = ui.allocate(0, 380);
            let left_width = ((body.width as i32) * 11 / 20).max(0);
            let (left, rest) = body.split_left(left_width);
            let right = Rect::new(
                rest.x + 14,
                rest.y,
                rest.width.saturating_sub(14),
                rest.height,
            );

            let items = [
                ListItem::new(Icon::Home, "Home").with_detail("dashboard and overview"),
                ListItem::new(Icon::Folder, "Files").with_trailing("12"),
                ListItem::new(Icon::Database, "Storage").with_detail("318 MB used"),
                ListItem::new(Icon::Settings, "Settings"),
                ListItem::disabled(ListItem::new(Icon::Lock, "Vault")),
            ];
            let list_rect = Rect::new(left.x, left.y, left.width, 176);
            ui.list_view(list_rect, &items, list);

            let cards = Rect::new(
                left.x,
                list_rect.bottom_i32() + 12,
                left.width,
                (left.bottom_i32() - list_rect.bottom_i32() - 12).max(0) as u32,
            );
            ui.column(cards, 10, |ui| {
                let card = ui.allocate(0, 64);
                ui.settings_card(card, Icon::Bell, "Notifications", "Desktop alerts", |ui| {
                    ui.toggle(notifications, "On");
                });
                let card = ui.allocate(0, 64);
                ui.settings_card(
                    card,
                    Icon::Sun,
                    "Appearance",
                    "Use the dark palette",
                    |ui| {
                        ui.toggle_button(compact, "Dark");
                    },
                );
                let line = ui.allocate(0, 40);
                ui.row(line, 10, |ui| {
                    ui.persona("Ada Lovelace", "Engineering", palette.accent, 30);
                    ui.info_badge(12, Tone::Danger);
                    ui.badge("NEW", palette.success);
                });
            });

            let active = fluent_tabs.selected;
            ui.tab_view(fluent_tabs, right, |ui, content| match active {
                0 => {
                    ui.heading("Overview");
                    ui.paragraph(
                        "A fixed-capacity tab strip with close buttons and a \
                             trailing add button. Each tab runs the same layout \
                             stack inside its content area.",
                    );
                    let grid = ui.allocate(0, 150);
                    let grid_items = [
                        GridItem::new(Icon::Cpu, "CPU").with_detail("42%"),
                        GridItem::new(Icon::Database, "Memory").with_detail("318 MB"),
                        GridItem::new(Icon::Wifi, "Network").with_detail("2.4 Mb/s"),
                    ];
                    ui.grid_view(grid, &grid_items, grid_selected);
                }
                1 => {
                    ui.heading("Details");
                    ui.search_box(name, "Filter details");
                    ui.date_picker(calendar);
                    ui.label_muted(
                        "Calendar popup, date fields and auto-suggest share one overlay rule.",
                    );
                    let _ = ui.auto_suggest(
                        suggest,
                        name,
                        "Type to search",
                        &["Alpha", "Beta", "Gamma", "Delta"],
                    );
                }
                _ => {
                    ui.heading("Activity");
                    ui.label("Navigation panes and overlays are components too.");
                    let _ = ui.navigation_view(
                        nav,
                        Rect::new(content.x, content.y + 60, content.width, 200),
                        "Sections",
                        &[
                            NavItem::new(Icon::Home, "Home"),
                            NavItem::new(Icon::Folder, "Files").with_badge(2),
                            NavItem::new(Icon::Settings, "Settings"),
                        ],
                        |ui, content| {
                            ui.label("Content");
                            ui.label_muted("pane collapses to a rail");
                            content
                        },
                    );
                }
            });
        } else if *tab == 2 {
            // Component gallery -------------------------------------------
            let toolbar = ui.allocate(0, metrics.control_height + 6);
            ui.toolbar(toolbar, |ui| {
                ui.icon_button(Icon::Home, false);
                ui.icon_button(Icon::Search, false);
                ui.icon_button(Icon::Refresh, false);
                ui.icon_button(Icon::Settings, false);
                ui.primary_button("New");
                let _ = ui.menu_button(
                    menu,
                    "Actions",
                    &[
                        MenuEntry::item("Duplicate"),
                        MenuEntry::shortcut("Export", "Ctrl+E"),
                        MenuEntry::Separator,
                        MenuEntry::disabled("Delete"),
                    ],
                );
            });

            ui.breadcrumbs(&["Workspace", "Components", "Gallery"], crumb);
            ui.steps(step, &["Design", "Build", "Ship"]);

            let split_rect = ui.allocate(0, 230);
            let columns = [
                Column::fixed("Component", 132),
                Column::fixed("Kind", 78),
                Column::flex("State", 1),
            ];
            let rows: [&[&str]; 6] = [
                &["Select", "input", "closed"],
                &["Table", "data", "6 rows"],
                &["Tree", "data", "expanded"],
                &["Accordion", "layout", "open"],
                &["Rating", "input", "3 / 5"],
                &["Chips", "display", "2 tags"],
            ];
            let tree_rows = [
                TreeRow::branch("components", 0, true),
                TreeRow::new("inputs", 1),
                TreeRow::new("data", 1),
                TreeRow::new("layout", 1),
            ];
            ui.split_pane(
                split_rect,
                Axis::Horizontal,
                split,
                |ui| {
                    let table_rect = ui.allocate(0, 180);
                    ui.table(table_rect, &columns, &rows, selected);
                    ui.pagination(page, 8);
                },
                |ui| {
                    let tree_rect = ui.allocate(0, 92);
                    ui.tree_state(tree_rect, &tree_rows, tree);
                    ui.accordion(accordion, 0, "Accordion", 58, |ui| {
                        ui.label("Body content shares the same layout engine.");
                        ui.label_muted("No measuring pass, no allocator.");
                    });
                },
            );

            let controls = ui.allocate(0, 116);
            ui.panel(controls, |ui| {
                let row = ui.allocate(0, metrics.control_height);
                ui.row(row, 10, |ui| {
                    ui.search_box(name, "Search components");
                    ui.select(
                        select,
                        select_value,
                        &["Dark", "Light", "High contrast"],
                        "Theme",
                    );
                    ui.number_stepper(stepper, 0.0, 100.0, 5.0, 0);
                });
                let row = ui.allocate(0, metrics.control_height);
                ui.row(row, 10, |ui| {
                    ui.rating(rating, 5);
                    ui.color_picker(
                        color,
                        &[
                            palette.accent,
                            palette.success,
                            palette.warning,
                            palette.danger,
                        ],
                    );
                    ui.chip("reusable", palette.accent, false);
                    ui.chip("no_std", palette.success, false);
                    ui.spinner(20);
                });
            });

            ui.alert(
                Tone::Info,
                "Component library",
                "Tables, trees, accordions, selects, menus, chips, ratings, \
                 drawers, popovers and feedback share the same painter and \
                 caller-owned state rules.",
            );
        } else if *tab == 4 {
            // Patterns gallery --------------------------------------------
            if tags.is_empty() {
                tags.push("no_std");
                tags.push("components");
            }
            let title_rect = ui.allocate(0, 34);
            ui.title_bar(title_rect, "Patterns", "Desktop composition");
            let selector = ui.allocate(0, metrics.control_height + 4);
            ui.row(selector, 12, |ui| {
                ui.selector_bar(pattern_tab, &["Library", "Activity", "Settings"]);
                ui.pivot(pivot_tab, &["Design", "Build"]);
            });

            let columns = ui.allocate(0, 220);
            let left_width = ((columns.width as i32) * 11 / 20).max(0);
            let (left, rest) = columns.split_left(left_width);
            let right = Rect::new(
                rest.x + 14,
                rest.y,
                rest.width.saturating_sub(14),
                rest.height,
            );
            ui.column(left, 8, |ui| {
                let properties = ui.allocate(0, 84);
                ui.property_grid(
                    properties,
                    &[
                        ("Build", "release"),
                        ("Target", arch),
                        ("Allocator", "none"),
                    ],
                );
                let log = ui.allocate(0, 118);
                ui.log_view(
                    log,
                    &[
                        "INFO  gui started",
                        "WARN  low memory",
                        "ERROR retry failed",
                        "INFO  ready",
                    ],
                    log_scroll,
                );
            });
            ui.column(right, 8, |ui| {
                let people = ui.allocate(0, 28);
                ui.row(people, 10, |ui| {
                    ui.avatar_group(&["AL", "BK", "CJ", "DK"], &[], 26);
                    ui.presence_badge("KD", palette.accent, Presence::Online, 26);
                    ui.label_muted("4 collaborators online");
                });
                ui.comment_thread(
                    &[ChatMessage::new("Ada", "The new painter handles polygons.")
                        .with_time("09:12")
                        .with_initials("AL")],
                );
                ui.chat_thread(&[ChatMessage::new("Kunal", "Shipping the component pass.")
                    .with_time("09:14")
                    .with_initials("KD")
                    .outgoing()]);
            });

            let controls = ui.allocate(0, 156);
            ui.panel(controls, |ui| {
                let row = ui.allocate(0, metrics.control_height);
                ui.row(row, 12, |ui| {
                    ui.range_slider(range_state, range, 0.0, 100.0);
                    ui.zoom_control(zoom, 0.5, 2.0);
                    ui.knob(knob, 0.0, 100.0, "Gain");
                });
                let row = ui.allocate(0, metrics.control_height);
                ui.row(row, 12, |ui| {
                    ui.password_box(password, "Password");
                    ui.hotkey_input(hotkey);
                    ui.button("Command palette")
                        .clicked
                        .then(|| command_palette.show());
                });
                let row = ui.allocate(0, metrics.control_height);
                ui.row(row, 12, |ui| {
                    ui.tag_input(tags, tag_draft, "Add tag");
                    let drop = ui.allocate_sized(150, metrics.control_height);
                    ui.button_at(drop, "Confirm delete", false)
                        .clicked
                        .then(|| {
                            *confirm_open = true;
                        });
                });
            });

            let lower = ui.allocate(0, 178);
            let (code_side, action_side) = lower.split_left(left_width);
            let action_side = Rect::new(
                action_side.x + 14,
                action_side.y,
                action_side.width.saturating_sub(14),
                action_side.height,
            );
            ui.column(code_side, 8, |ui| {
                let code = ui.allocate(0, 72);
                ui.code_block(
                    code,
                    "fn main() {\n    let mut ui = Ui::new(..);\n    ui.button(\"Run\");\n}",
                );
                let diff = ui.allocate(0, 72);
                ui.diff_view(
                    diff,
                    &[
                        DiffLine::context("painter.fill_rect(..)"),
                        DiffLine::added("painter.fill_polygon(..)"),
                        DiffLine::removed("painter.fill_triangle(..)"),
                    ],
                );
            });
            ui.column(action_side, 8, |ui| {
                let result_rect = ui.allocate(0, 120);
                ui.result_page(
                    result_rect,
                    Icon::Success,
                    "All checks passed",
                    "The advanced gallery renders without allocation.",
                    "Open details",
                );
                let actions = ui.allocate(0, metrics.control_height + 6);
                ui.row(actions, 10, |ui| {
                    ui.float_button(Icon::Plus);
                    ui.back_to_top(true);
                    if ui.button("Sheet").clicked {
                        *sheet_open = true;
                    }
                    let _ = ui.command_bar_flyout(
                        flyout_menu,
                        "More",
                        &[MenuEntry::item("Duplicate"), MenuEntry::item("Archive")],
                    );
                });
                let swipe_row = ui.allocate(0, 36);
                let swipe_result = ui.swipe_actions(swipe_row, swipe, &["Delete"], |ui| {
                    ui.label("Swipe this row");
                });
                if swipe_result.action.is_some() {
                    *clicks = clicks.saturating_add(1);
                }
                let pull_rect = ui.allocate(0, 44);
                ui.pull_to_refresh(pull_rect, pull);
            });
        } else {
            // Stat cards.
            let cards = [
                ("CPU", "42%", "4 cores at 1.2 GHz", Tone::Info),
                ("MEMORY", "318 MB", "of 1.0 GB mapped", Tone::Warning),
                ("NETWORK", "2.4 Mb/s", "loopback + virtio", Tone::Success),
            ];
            let grid_rect = ui.allocate(0, 104);
            ui.grid(grid_rect, 3, 104, 12, |ui| {
                for (title, value, detail, tone) in cards {
                    let cell = ui.allocate(0, 0);
                    ui.stat_card(cell, title, value, detail, tone);
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
        }
    });

    // Status line.
    let status = Rect::new(20, (height - 24).max(0), (width - 40).max(0) as u32, 18);
    ui.status_bar(
        status,
        "immediate-mode · clipped painter · reusable components · charts",
        "1024x768 · XRGB8888 · no_std",
    );

    if *tab == 4 {
        let sheet_area = Rect::new(
            250,
            74,
            (width - 270).max(0) as u32,
            (height - 100).max(0) as u32,
        );
        let mut cancel_sheet = false;
        let _ = ui.bottom_sheet(sheet_area, sheet_open, "Quick actions", |ui| {
            ui.label("Bottom sheets float above the page.");
            let row = ui.allocate(0, metrics.control_height);
            ui.row(row, 10, |ui| {
                ui.primary_button("Apply");
                if ui.button("Cancel").clicked {
                    cancel_sheet = true;
                }
            });
        });
        if cancel_sheet {
            *sheet_open = false;
        }
        let _ = ui.popconfirm(
            Rect::new(650, 540, 90, 32),
            confirm_open,
            "Delete the selected item?",
        );
        let _ = ui.command_palette(
            command_palette,
            &[
                "Open file",
                "Toggle theme",
                "New window",
                "Rebuild",
                "Reset layout",
            ],
        );
    }

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

    let _ = (compact, notes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use feos_gfx::{Canvas, Point};
    use feos_gui::{Event, Key, Modifiers, MouseButton};

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
        let pos = Point::new(
            random.below(width as u64 + 8) as i32 - 4,
            random.below(height as u64 + 8) as i32 - 4,
        );
        match random.below(14) {
            0..=4 => Event::PointerMoved(pos),
            5 | 6 => Event::PointerDown {
                pos,
                button: MouseButton::Left,
            },
            7 | 8 => Event::PointerUp {
                pos,
                button: MouseButton::Left,
            },
            9 => Event::PointerWheel(random.below(9) as i32 - 4),
            10 => Event::Text((b'a' + random.below(26) as u8) as char),
            11 => Event::Text('\u{e9}'),
            _ => {
                const KEYS: [Key; 14] = [
                    Key::Tab,
                    Key::Enter,
                    Key::Escape,
                    Key::Space,
                    Key::Backspace,
                    Key::Delete,
                    Key::Left,
                    Key::Right,
                    Key::Up,
                    Key::Down,
                    Key::Home,
                    Key::End,
                    Key::PageUp,
                    Key::PageDown,
                ];
                const MODIFIERS: [Modifiers; 3] = [Modifiers::NONE, Modifiers::SHIFT, Modifiers::CTRL];
                Event::KeyDown(
                    KEYS[random.below(14) as usize],
                    MODIFIERS[random.below(3) as usize],
                )
            }
        }
    }

    /// Every widget in the toolkit, under random input, on canvases from one
    /// pixel up to a normal window. The demo state is large and not shared, so
    /// each run gets its own on a thread with a big enough stack.
    #[test]
    fn the_gallery_survives_random_input_on_any_canvas() {
        const SIZES: [(usize, usize); 10] = [
            (1, 1),
            (2, 3),
            (17, 9),
            (40, 500),
            (500, 30),
            (120, 90),
            (200, 150),
            (320, 240),
            (640, 480),
            (900, 640),
        ];
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                for (seed, (width, height)) in SIZES.iter().cycle().take(20).enumerate() {
                    let mut pixels = std::vec![0u32; width * height];
                    let mut state = std::boxed::Box::new(DemoState::new());
                    let mut random =
                        Random((seed as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
                    for _ in 0..300 {
                        state.input.clear_events();
                        for _ in 0..random.below(5) {
                            let event =
                                random_event(&mut random, *width as i32, *height as i32);
                            state.input.push(event);
                        }
                        let mut canvas =
                            Canvas::new(&mut pixels, *width, *height, *width).unwrap();
                        render(&mut canvas, &mut state, "test");
                    }
                }
            })
            .unwrap()
            .join()
            .expect("the gallery panicked");
    }
}
