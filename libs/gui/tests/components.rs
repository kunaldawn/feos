//! Component-level interaction tests.

use feos_gfx::{Canvas, Color, Point, Rect};
use feos_gui::prelude::*;
use feos_gui::{Column, ScrollState, TimelineItem, ToastQueue};

const WIDTH: usize = 400;
const HEIGHT: usize = 320;

fn canvas(pixels: &mut [u32]) -> Canvas<'_> {
    Canvas::new(pixels, WIDTH, HEIGHT, WIDTH).unwrap()
}

fn click(input: &mut Input, x: i32, y: i32) {
    input.push(Event::PointerDown {
        pos: Point::new(x, y),
        button: MouseButton::Left,
    });
}

fn release(input: &mut Input, x: i32, y: i32) {
    input.push(Event::PointerUp {
        pos: Point::new(x, y),
        button: MouseButton::Left,
    });
}

#[test]
fn select_opens_and_chooses_with_the_keyboard() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut select_state = SelectState::new();
    let mut selected = 0usize;
    let items = ["Alpha", "Beta", "Gamma"];

    click(&mut input, 20, 17);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.select(&mut select_state, &mut selected, &items, "Choose");
    }
    input.clear_events();

    release(&mut input, 20, 17);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.select(&mut select_state, &mut selected, &items, "Choose");
    }
    assert!(select_state.open);
    input.clear_events();

    input.push(Event::KeyDown(Key::Down, Modifiers::NONE));
    input.push(Event::KeyDown(Key::Enter, Modifiers::NONE));
    let changed = {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.select(&mut select_state, &mut selected, &items, "Choose")
            .changed
    };
    assert!(changed);
    assert_eq!(selected, 1);
    assert!(!select_state.open);
}

#[test]
fn menu_skips_separators_and_disabled_entries() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut menu = MenuState::new();
    let entries = [
        MenuEntry::item("Open"),
        MenuEntry::shortcut("Save", "Ctrl+S"),
        MenuEntry::Separator,
        MenuEntry::disabled("Delete"),
    ];

    click(&mut input, 20, 17);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.menu_button(&mut menu, "File", &entries);
    }
    input.clear_events();

    release(&mut input, 20, 17);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.menu_button(&mut menu, "File", &entries);
    }
    assert!(menu.open);
    input.clear_events();

    input.push(Event::KeyDown(Key::Down, Modifiers::NONE));
    input.push(Event::KeyDown(Key::Enter, Modifiers::NONE));
    let result = {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.menu_button(&mut menu, "File", &entries)
    };
    assert_eq!(result.selected, Some(1));
    assert!(!menu.open);
}

#[test]
fn search_box_clears_with_its_button() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut buffer = TextBuffer::<32>::with_text("hello");

    click(&mut input, 380, 17);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.search_box(&mut buffer, "Search");
    }
    input.clear_events();

    release(&mut input, 380, 17);
    let changed = {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.search_box(&mut buffer, "Search").changed
    };
    assert!(changed);
    assert!(buffer.is_empty());
}

#[test]
fn stepper_click_and_keyboard_update_the_value() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut value = 0.0f32;

    click(&mut input, 380, 17);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.number_stepper(&mut value, 0.0, 10.0, 1.0, 0);
    }
    input.clear_events();

    release(&mut input, 380, 17);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.number_stepper(&mut value, 0.0, 10.0, 1.0, 0);
    }
    assert_eq!(value, 1.0);
    input.clear_events();

    input.push(Event::KeyDown(Key::Right, Modifiers::NONE));
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.number_stepper(&mut value, 0.0, 10.0, 1.0, 0);
    }
    assert_eq!(value, 2.0);
}

#[test]
fn rating_sets_the_clicked_star() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut value = 0u8;

    click(&mut input, 65, 10);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.rating(&mut value, 5);
    }
    input.clear_events();

    release(&mut input, 65, 10);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.rating(&mut value, 5);
    }
    assert_eq!(value, 3);
}

#[test]
fn table_selects_a_row() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut selected = 0usize;
    let columns = [Column::fixed("Name", 100), Column::flex("Status", 1)];
    let rows: [&[&str]; 2] = [&["alpha", "ok"], &["beta", "warn"]];

    click(&mut input, 20, 70);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.table(Rect::new(0, 0, 300, 150), &columns, &rows, &mut selected);
    }
    input.clear_events();

    release(&mut input, 20, 70);
    let changed = {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.table(Rect::new(0, 0, 300, 150), &columns, &rows, &mut selected)
            .response
            .changed
    };
    assert!(changed);
    assert_eq!(selected, 1);
}

#[test]
fn tree_toggles_and_selects() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut tree = TreeState::<8>::new();
    let rows = [TreeRow::branch("root", 0, false), TreeRow::new("leaf", 1)];

    click(&mut input, 15, 13);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.tree_state(Rect::new(0, 0, 300, 200), &rows, &mut tree);
    }
    input.clear_events();

    release(&mut input, 15, 13);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.tree_state(Rect::new(0, 0, 300, 200), &rows, &mut tree);
    }
    assert!(tree.is_expanded(0));
    assert_eq!(tree.selected, 0);
}

#[test]
fn accordion_toggles_and_runs_its_body() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut accordion = AccordionState::<4>::new();
    let mut body_ran = false;

    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        let (_, value) = ui.accordion_section(&mut accordion, 0, "Details", 60, |ui| {
            body_ran = true;
            ui.label("hidden");
        });
        assert!(value.is_none());
    }
    assert!(!body_ran);

    click(&mut input, 20, 17);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.accordion_section(&mut accordion, 0, "Details", 60, |_| {});
    }
    input.clear_events();
    release(&mut input, 20, 17);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.accordion_section(&mut accordion, 0, "Details", 60, |_| {});
    }
    assert!(accordion.is_open(0));
    input.clear_events();

    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        let (_, value) = ui.accordion_section(&mut accordion, 0, "Details", 60, |ui| {
            body_ran = true;
            ui.label("visible");
        });
        assert!(value.is_some());
    }
    assert!(body_ran);
}

#[test]
fn split_pane_drag_moves_the_divider() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut split = SplitState::new();
    let rect = Rect::new(0, 0, 300, 100);

    click(&mut input, 150, 50);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.split_pane(
            rect,
            Axis::Horizontal,
            &mut split,
            |ui| ui.label("left"),
            |ui| ui.label("right"),
        );
    }
    input.clear_events();

    input.push(Event::PointerMoved(Point::new(210, 50)));
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.split_pane(
            rect,
            Axis::Horizontal,
            &mut split,
            |ui| ui.label("left"),
            |ui| ui.label("right"),
        );
    }
    assert!((split.fraction - 0.7).abs() < 0.01);
}

#[test]
fn drawers_and_popovers_close_on_escape() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut drawer = DrawerState { open: true };
    let mut popover = PopoverState { open: true };
    let anchor = Rect::new(20, 20, 80, 30);

    input.push(Event::KeyDown(Key::Escape, Modifiers::NONE));
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        let value = ui.drawer(
            Rect::new(0, 0, WIDTH as u32, HEIGHT as u32),
            DrawerSide::Right,
            &mut drawer,
            200,
            "Settings",
            |_| 7,
        );
        assert_eq!(value, Some(7));
        let value = ui.popover(anchor, &mut popover, 120, 80, |_| 9);
        assert_eq!(value, Some(9));
    }
    assert!(!drawer.open);
    assert!(!popover.open);
}

#[test]
fn toast_queue_expires_and_drops_the_oldest() {
    let mut queue = ToastQueue::<2>::new();
    queue.push(Tone::Info, "one", 2, 0);
    queue.push(Tone::Success, "two", 0, 0);
    queue.push(Tone::Warning, "three", 0, 0);
    assert_eq!(queue.len(), 2);
    assert_eq!(queue.get(0).unwrap().message, "two");
    assert_eq!(queue.get(1).unwrap().message, "three");

    queue.tick(10);
    assert_eq!(queue.len(), 2);
    queue.dismiss(0);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.get(0).unwrap().message, "three");

    queue.push(Tone::Info, "four", 1, 10);
    queue.tick(11);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.get(0).unwrap().message, "three");
}

#[test]
fn pagination_clamps_and_advances() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut page = 99usize;

    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.pagination(&mut page, 3);
    }
    assert_eq!(page, 2);
    input.clear_events();

    // The previous arrow sits in the first button-sized cell.
    click(&mut input, 17, 17);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.pagination(&mut page, 3);
    }
    input.clear_events();
    release(&mut input, 17, 17);
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.pagination(&mut page, 3);
    }
    assert_eq!(page, 1);
}

#[test]
fn timeline_and_key_values_do_not_panic_in_small_rects() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let input = Input::new();
    let mut state = InteractionState::<8>::new();
    let items = [
        TimelineItem::new("created", "today").with_tone(Tone::Success),
        TimelineItem::new("updated", "just now"),
    ];

    let mut canvas = canvas(&mut pixels);
    let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
    ui.timeline(Rect::new(0, 0, 120, 40), &items);
    ui.key_value_list(Rect::new(0, 50, 120, 30), &[("a", "1"), ("b", "2")]);
    ui.avatar("AB", Color::rgb(40, 90, 180), 32);
    ui.chip("tag", Color::rgb(40, 90, 180), true);
    ui.skeleton_lines(3);
}

#[test]
fn feedback_and_icon_components_render() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut toasts = ToastQueue::<2>::new();
    toasts.push(Tone::Success, "saved", 0, 0);
    let mut confirm_open = true;
    let full = Rect::new(0, 0, WIDTH as u32, HEIGHT as u32);

    let mut canvas = canvas(&mut pixels);
    let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
    for icon in Icon::ALL {
        ui.icon(*icon, 16, theme.palette.text);
    }
    ui.alert(Tone::Warning, "Warning", "A reusable alert.");
    ui.spinner(18);
    ui.progress_ring(Rect::new(0, 0, 48, 48), 0.6, "60%");
    ui.skeleton(Rect::new(0, 0, 100, 12));
    let empty = ui.allocate(0, 80);
    ui.empty_state(empty, Icon::Folder, "Empty", "Nothing here.");
    ui.loading_overlay(Rect::new(0, 0, 100, 60), "Loading");
    ui.toasts(full, &mut toasts);
    let result = ui.confirm_dialog(
        Rect::new(40, 40, 260, 180),
        "Confirm",
        "Continue?",
        &mut confirm_open,
        "Yes",
        "No",
    );
    assert!(!result.is_finished());
    assert!(confirm_open);
}

fn run_frame<R>(
    pixels: &mut [u32],
    theme: &Theme,
    input: &Input,
    state: &mut InteractionState<8>,
    f: impl FnOnce(&mut Ui<'_, '_, '_, 8>) -> R,
) -> R {
    let mut canvas = canvas(pixels);
    let mut ui = Ui::new(&mut canvas, theme, input, state);
    f(&mut ui)
}

#[test]
fn list_view_selects_single_and_multi_rows() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut list = ListState::<8>::new();
    let items = [
        ListItem::new(Icon::Folder, "Alpha"),
        ListItem::new(Icon::File, "Beta").with_detail("second row"),
        ListItem::new(Icon::Database, "Gamma").with_trailing("12"),
    ];
    let rect = Rect::new(0, 0, 300, 200);

    click(&mut input, 20, 50);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.list_view(rect, &items, &mut list);
    });
    input.clear_events();
    release(&mut input, 20, 50);
    let response = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.list_view(rect, &items, &mut list)
    });
    assert_eq!(response.changed_row, Some(1));
    assert_eq!(list.selected_index(), Some(1));
    input.clear_events();

    let mut multi = ListState::<8>::multi();
    click(&mut input, 20, 15);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.list_view(rect, &items, &mut multi);
    });
    input.clear_events();
    release(&mut input, 20, 15);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.list_view(rect, &items, &mut multi);
    });
    input.clear_events();
    click(&mut input, 20, 90);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.list_view(rect, &items, &mut multi);
    });
    input.clear_events();
    release(&mut input, 20, 90);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.list_view(rect, &items, &mut multi);
    });
    assert_eq!(multi.selected_count(), 2);
    assert!(multi.is_selected(0));
    assert!(multi.is_selected(2));
}

#[test]
fn grid_view_selects_a_card() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut selected = 1usize;
    let items = [
        GridItem::new(Icon::Cpu, "CPU").with_detail("42%"),
        GridItem::new(Icon::Database, "Disk").with_detail("318 MB"),
    ];
    let rect = Rect::new(0, 0, 400, 150);

    click(&mut input, 50, 40);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.grid_view(rect, &items, &mut selected);
    });
    input.clear_events();
    release(&mut input, 50, 40);
    let response = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.grid_view(rect, &items, &mut selected)
    });
    assert!(response.changed);
    assert_eq!(selected, 0);
}

#[test]
fn flip_view_arrows_and_pips_move_the_slide() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut index = 0usize;
    let items = [
        FlipItem::new("One", "First slide"),
        FlipItem::new("Two", "Second slide"),
        FlipItem::new("Three", "Third slide"),
    ];
    let rect = Rect::new(0, 0, 300, 200);

    click(&mut input, 266, 100);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.flip_view(rect, &items, &mut index);
    });
    input.clear_events();
    release(&mut input, 266, 100);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.flip_view(rect, &items, &mut index);
    });
    assert_eq!(index, 1);
    input.clear_events();

    click(&mut input, 172, 183);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.flip_view(rect, &items, &mut index);
    });
    input.clear_events();
    release(&mut input, 172, 183);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.flip_view(rect, &items, &mut index);
    });
    assert_eq!(index, 2);
}

#[test]
fn menu_bar_opens_and_selects_with_keyboard() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut bar = MenuBarState::new();
    let file = [MenuEntry::item("Open"), MenuEntry::item("Save")];
    let edit = [MenuEntry::item("Copy"), MenuEntry::item("Paste")];
    let menus = [
        MenuBarMenu::new("File", &file),
        MenuBarMenu::new("Edit", &edit),
    ];

    click(&mut input, 20, 17);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.menu_bar(&mut bar, &menus);
    });
    input.clear_events();
    release(&mut input, 20, 17);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.menu_bar(&mut bar, &menus);
    });
    assert_eq!(bar.open, Some(0));
    input.clear_events();

    input.push(Event::KeyDown(Key::Down, Modifiers::NONE));
    input.push(Event::KeyDown(Key::Enter, Modifiers::NONE));
    let response = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.menu_bar(&mut bar, &menus)
    });
    assert_eq!(response.selected, Some((0, 1)));
    assert_eq!(bar.open, None);
}

#[test]
fn tab_view_adds_switches_and_closes() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut tabs = TabViewState::<4>::new();
    assert!(tabs.push("One"));
    let rect = Rect::new(0, 0, 400, 200);

    // The + button follows the first tab.
    click(&mut input, 120, 20);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.tab_view(&mut tabs, rect, |_, _| {});
    });
    input.clear_events();
    release(&mut input, 120, 20);
    let response = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.tab_view(&mut tabs, rect, |_, _| {})
    });
    assert!(response.added);
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs.selected, 1);
    input.clear_events();

    click(&mut input, 40, 20);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.tab_view(&mut tabs, rect, |_, _| {});
    });
    input.clear_events();
    release(&mut input, 40, 20);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.tab_view(&mut tabs, rect, |_, _| {});
    });
    assert_eq!(tabs.selected, 0);
    input.clear_events();

    click(&mut input, 90, 20);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.tab_view(&mut tabs, rect, |_, _| {});
    });
    input.clear_events();
    release(&mut input, 90, 20);
    let response = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.tab_view(&mut tabs, rect, |_, _| {})
    });
    assert_eq!(response.closed, Some(0));
    assert_eq!(tabs.len(), 1);
}

#[test]
fn navigation_view_toggles_and_selects() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut nav = NavigationViewState::new();
    let items = [
        NavItem::new(Icon::Home, "Home"),
        NavItem::new(Icon::Folder, "Files").with_badge(3),
        NavItem::new(Icon::Settings, "Settings"),
    ];
    let rect = Rect::new(0, 0, 400, 300);

    let (response, content) = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.navigation_view(&mut nav, rect, "Demo", &items, |ui, content| {
            ui.label("content");
            content
        })
    });
    assert!(content.width > 0);
    assert!(response.selected.is_none());

    click(&mut input, 20, 20);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.navigation_view(&mut nav, rect, "Demo", &items, |_, content| content);
    });
    input.clear_events();
    release(&mut input, 20, 20);
    let (response, _) = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.navigation_view(&mut nav, rect, "Demo", &items, |_, content| content)
    });
    assert!(response.pane_toggled);
    assert!(!nav.pane_open);
    input.clear_events();

    click(&mut input, 20, 110);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.navigation_view(&mut nav, rect, "Demo", &items, |_, content| content);
    });
    input.clear_events();
    release(&mut input, 20, 110);
    let (response, _) = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.navigation_view(&mut nav, rect, "Demo", &items, |_, content| content)
    });
    assert_eq!(response.selected, Some(1));
}

#[test]
fn calendar_and_date_picker_select_a_day() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut calendar = CalendarState::new(2026, 10, 1);
    let rect = Rect::new(0, 0, 300, 220);

    click(&mut input, 145, 60);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.calendar(rect, &mut calendar);
    });
    input.clear_events();
    release(&mut input, 145, 60);
    let response = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.calendar(rect, &mut calendar)
    });
    assert_eq!(response.selected, Some(1));

    let mut picker = CalendarState::new(2026, 10, 1);
    click(&mut input, 20, 17);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.date_picker(&mut picker);
    });
    input.clear_events();
    release(&mut input, 20, 17);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.date_picker(&mut picker);
    });
    assert!(picker.open);
}

#[test]
fn auto_suggest_filters_and_accepts_with_enter() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut suggest = AutoSuggestState::new();
    let mut buffer = TextBuffer::<32>::new();
    let suggestions = ["Car", "Cat", "Cable", "Dog"];

    click(&mut input, 20, 17);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.auto_suggest(&mut suggest, &mut buffer, "Search", &suggestions);
    });
    input.clear_events();
    release(&mut input, 20, 17);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.auto_suggest(&mut suggest, &mut buffer, "Search", &suggestions);
    });
    input.clear_events();

    input.push(Event::Text('c'));
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.auto_suggest(&mut suggest, &mut buffer, "Search", &suggestions);
    });
    assert!(suggest.open);
    input.clear_events();

    input.push(Event::KeyDown(Key::Down, Modifiers::NONE));
    input.push(Event::KeyDown(Key::Enter, Modifiers::NONE));
    let response = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.auto_suggest(&mut suggest, &mut buffer, "Search", &suggestions)
    });
    assert_eq!(response.selected, Some(1));
    assert_eq!(buffer.as_str(), "Cat");
}

#[test]
fn info_bar_runs_its_action_and_closes() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut open = true;

    click(&mut input, 300, 50);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.info_bar(
            Tone::Warning,
            "Heads up",
            "Something needs attention.",
            Some("Retry"),
            &mut open,
        );
    });
    input.clear_events();
    release(&mut input, 300, 50);
    let response = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.info_bar(
            Tone::Warning,
            "Heads up",
            "Something needs attention.",
            Some("Retry"),
            &mut open,
        )
    });
    assert!(response.action_clicked);
    assert!(open);
    input.clear_events();

    click(&mut input, 376, 24);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.info_bar(
            Tone::Warning,
            "Heads up",
            "Something needs attention.",
            Some("Retry"),
            &mut open,
        );
    });
    input.clear_events();
    release(&mut input, 376, 24);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.info_bar(
            Tone::Warning,
            "Heads up",
            "Something needs attention.",
            Some("Retry"),
            &mut open,
        );
    });
    assert!(!open);
}

#[test]
fn context_menu_and_flyout_work() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut menu = MenuState::new();
    menu.open = true;
    let entries = [MenuEntry::item("Cut"), MenuEntry::item("Copy")];

    click(&mut input, 40, 40);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.context_menu(Rect::new(20, 20, 1, 1), &mut menu, &entries);
    });
    input.clear_events();
    release(&mut input, 40, 40);
    let response = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.context_menu(Rect::new(20, 20, 1, 1), &mut menu, &entries)
    });
    assert_eq!(response.selected, Some(0));
    input.clear_events();

    let mut flyout = FlyoutState { open: true };
    let mut ran = false;
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        let value = ui.flyout(
            Rect::new(20, 20, 80, 30),
            &mut flyout,
            FlyoutPlacement::Bottom,
            140,
            90,
            |ui| {
                ran = true;
                ui.label("floating");
            },
        );
        assert!(value.is_some());
    });
    assert!(ran);
    input.push(Event::KeyDown(Key::Escape, Modifiers::NONE));
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.flyout(
            Rect::new(20, 20, 80, 30),
            &mut flyout,
            FlyoutPlacement::Bottom,
            140,
            90,
            |_| {},
        );
    });
    assert!(!flyout.open);
}

#[test]
fn settings_card_and_section_header_render() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut enabled = false;

    click(&mut input, 265, 30);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.column(Rect::new(0, 0, 400, 200), 10, |ui| {
            let card = ui.allocate(0, 60);
            ui.settings_card(card, Icon::Bell, "Notifications", "Desktop alerts", |ui| {
                ui.checkbox(&mut enabled, "On")
            });
            ui.section_header("Recent", Some("See all"));
        });
    });
    input.clear_events();
    release(&mut input, 265, 30);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.column(Rect::new(0, 0, 400, 200), 10, |ui| {
            let card = ui.allocate(0, 60);
            ui.settings_card(card, Icon::Bell, "Notifications", "Desktop alerts", |ui| {
                ui.checkbox(&mut enabled, "On")
            });
            ui.section_header("Recent", Some("See all"));
        });
    });
    assert!(enabled);
}

#[test]
fn command_bar_activates_commands_and_overflows() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut bar = CommandBarState::new();
    let items = [
        CommandItem::new(Icon::Plus, "New"),
        CommandItem::new(Icon::Edit, "Edit"),
        CommandItem::new(Icon::Download, "Export"),
        CommandItem::new(Icon::Trash, "Delete"),
        CommandItem::new(Icon::Settings, "Settings"),
    ];

    click(&mut input, 40, 17);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.command_bar(&mut bar, &items);
    });
    input.clear_events();
    release(&mut input, 40, 17);
    let response = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.command_bar(&mut bar, &items)
    });
    assert_eq!(response.activated, Some(0));

    click(&mut input, 380, 20);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.command_bar(&mut bar, &items);
    });
    input.clear_events();
    release(&mut input, 380, 20);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.command_bar(&mut bar, &items);
    });
    assert!(bar.overflow.open);
}

#[test]
fn info_badges_personas_and_hero_banners_render() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let input = Input::new();
    let mut state = InteractionState::<8>::new();

    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.column(Rect::new(0, 0, 400, 260), 10, |ui| {
            ui.persona("Ada Lovelace", "Engineering", Color::rgb(40, 90, 180), 36);
            ui.info_badge(12, Tone::Danger);
            let banner = ui.allocate(0, 110);
            ui.hero_banner(
                banner,
                "FEOS",
                "Build anything",
                "A small toolkit with every control an app needs.",
                |ui| {
                    ui.primary_button("Start");
                },
            );
        });
    });
}

#[test]
fn extra_form_controls_and_overlays_render_without_panicking() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut bold = false;
    let mut split = MenuState::new();
    let mut dropdown = MenuState::new();
    let mut choice = 1usize;
    let mut hour = 9u32;
    let mut minute = 5u32;
    let mut page = 2usize;
    let mut tip = TeachingTipState::new();
    tip.open = true;
    let mut flyout = FlyoutState::new();
    flyout.open = true;

    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.column(Rect::new(0, 0, 400, 320), 8, |ui| {
            ui.toggle_button(&mut bold, "Bold");
            let _ = ui.split_button(
                &mut split,
                "Run",
                &[MenuEntry::item("Once"), MenuEntry::item("Always")],
            );
            let _ = ui.dropdown_button(
                &mut dropdown,
                "More",
                &[MenuEntry::item("Rename"), MenuEntry::item("Remove")],
            );
            ui.hyperlink_button("Open docs");
            ui.radio_group(&mut choice, &["Fast", "Balanced", "Slow"]);
            ui.time_picker(&mut hour, &mut minute);
            ui.pips_pager(&mut page, 4);
        });
        ui.teaching_tip(
            Rect::new(20, 20, 80, 30),
            &mut tip,
            "Tip",
            "A short piece of guidance.",
        );
        ui.flyout(
            Rect::new(120, 40, 60, 30),
            &mut flyout,
            FlyoutPlacement::Right,
            160,
            80,
            |ui| {
                ui.label("floating");
            },
        );
    });

    assert_eq!(hour, 9);
    assert_eq!(minute, 5);
    assert_eq!(page, 2);
    assert_eq!(choice, 1);
    assert!(!bold);
}

#[test]
fn advanced_and_pattern_components_render_without_panicking() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut tags = TagSet::<4, 16>::new();
    assert!(tags.push("alpha"));
    assert!(tags.push("beta"));
    let mut draft = TextBuffer::<32>::new();
    let mut password = TextBuffer::<32>::with_text("secret");
    let mut knob = 40.0f32;
    let mut range = RangeValue::new(20.0, 80.0);
    let mut range_state = RangeSliderState::new();
    let mut hotkey = Hotkey::new();
    let mut cascade = CascaderState::<3>::new();
    let mut transfer = TransferState::<8>::new();
    let mut scroll = ScrollState::new();
    let mut swipe = SwipeState::new();
    let mut pull = PullState::new();
    let mut offset = 0i32;
    let mut zoom = ZoomState::new(1.0);
    let mut selector = 0usize;
    let mut pivot = 0usize;
    let mut sheet_open = true;
    let mut confirm_open = false;
    let mut palette = CommandPaletteState::new();
    palette.open = true;
    palette.query.set("set");

    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.column(Rect::new(0, 0, 400, 320), 6, |ui| {
            ui.avatar_group(&["AL", "BK", "CJ"], &[], 24);
            ui.presence_badge("AL", Color::rgb(40, 90, 180), Presence::Online, 24);
            let group = ui.allocate(0, 62);
            ui.group_box(group, "Details", |ui| {
                ui.label("Group body");
            });
            let properties = ui.allocate(0, 54);
            ui.property_grid(
                properties,
                &[("Name", "FEOS"), ("Version", "0.1"), ("Mode", "GUI")],
            );
            let log = ui.allocate(0, 70);
            ui.log_view(
                log,
                &["INFO boot", "WARN slow", "ERROR failed", "INFO ready"],
                &mut scroll,
            );
            let code = ui.allocate(0, 62);
            ui.code_block(code, "fn main() {\n    ok();\n}");
            let diff = ui.allocate(0, 62);
            ui.diff_view(
                diff,
                &[
                    DiffLine::context("unchanged"),
                    DiffLine::added("new line"),
                    DiffLine::removed("old line"),
                ],
            );
            ui.comment_thread(&[ChatMessage::new("Ada", "First comment")
                .with_time("12:00")
                .with_initials("AL")]);
            ui.chat_thread(&[ChatMessage::new("Me", "A reply").outgoing()]);
            ui.selector_bar(&mut selector, &["One", "Two", "Three"]);
            ui.pivot(&mut pivot, &["Alpha", "Beta"]);
            let bar = ui.allocate(0, 34);
            ui.title_bar(bar, "FEOS", "Document");
            ui.float_button(Icon::Plus);
            ui.back_to_top(true);
            ui.password_box(&mut password, "Password");
            ui.text_area(&mut draft, 3, "Notes");
            ui.range_slider(&mut range_state, &mut range, 0.0, 100.0);
            ui.knob(&mut knob, 0.0, 100.0, "Gain");
            ui.tag_input(&mut tags, &mut draft, "Add tag");
            let drop = ui.allocate(0, 96);
            ui.file_dropzone(drop, "Drop files", "or browse");
            ui.hotkey_input(&mut hotkey);
            let cascade_rect = ui.allocate(0, 80);
            ui.cascader(cascade_rect, &mut cascade, &[&["A", "B"], &["C", "D"]]);
            let transfer_rect = ui.allocate(0, 70);
            ui.transfer_list(transfer_rect, &["One", "Two"], &mut transfer);
            let row = ui.allocate(0, 36);
            let _ = ui.swipe_actions(row, &mut swipe, &["Delete"], |ui| {
                ui.label("Swipe row");
            });
            let pull_rect = ui.allocate(0, 40);
            ui.pull_to_refresh(pull_rect, &mut pull);
            let scrollbar = ui.allocate_sized(10, 80);
            ui.annotated_scrollbar(scrollbar, 80, 300, &mut offset, &[0.25, 0.75]);
            ui.zoom_control(&mut zoom, 0.5, 2.0);
        });
        ui.bottom_sheet(Rect::new(0, 0, 400, 320), &mut sheet_open, "Sheet", |ui| {
            ui.label("Sheet body");
        });
        let _ = ui.popconfirm(
            Rect::new(100, 20, 80, 30),
            &mut confirm_open,
            "Delete this item?",
        );
        let _ = ui.command_palette(&mut palette, &["Settings", "Open file", "Close window"]);
        ui.result_page(
            Rect::new(0, 0, 400, 180),
            Icon::Success,
            "Done",
            "Everything finished.",
            "Continue",
        );
    });
}

#[test]
fn selector_bar_clicks_change_the_selection() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut selected = 0usize;

    click(&mut input, 160, 17);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.selector_bar(&mut selected, &["Alpha", "Beta", "Gamma"]);
    });
    input.clear_events();
    release(&mut input, 160, 17);
    let response = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.selector_bar(&mut selected, &["Alpha", "Beta", "Gamma"])
    });
    assert!(response.changed);
    assert_eq!(selected, 1);
}

#[test]
fn zoom_control_and_scrollbar_update_their_values() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut zoom = ZoomState::new(1.0);
    let mut offset = 0i32;

    click(&mut input, 200, 17);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.zoom_control(&mut zoom, 0.5, 2.0);
    });
    input.clear_events();
    release(&mut input, 200, 17);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.zoom_control(&mut zoom, 0.5, 2.0);
    });
    assert!(zoom.level > 1.0);

    click(&mut input, 5, 80);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        let rect = ui.allocate_sized(10, 100);
        ui.annotated_scrollbar(rect, 100, 400, &mut offset, &[0.5]);
    });
    input.clear_events();
    release(&mut input, 5, 80);
    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        let rect = ui.allocate_sized(10, 100);
        ui.annotated_scrollbar(rect, 100, 400, &mut offset, &[0.5]);
    });
    assert!(offset > 0);
}

#[test]
fn command_palette_closes_on_escape() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    input.push(Event::KeyDown(Key::Escape, Modifiers::NONE));
    let mut state = InteractionState::<8>::new();
    let mut palette = CommandPaletteState::new();
    palette.open = true;

    let chosen = run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.command_palette(&mut palette, &["Settings", "Open file"])
    });
    assert_eq!(chosen, None);
    assert!(!palette.open);
}

#[test]
fn tag_sets_push_replace_and_remove() {
    let mut tags = TagSet::<3, 12>::new();
    assert!(tags.push("alpha"));
    assert!(tags.push("beta"));
    assert_eq!(tags.get(1), Some("beta"));
    assert!(tags.remove(0));
    assert_eq!(tags.get(0), Some("beta"));
    assert_eq!(tags.len(), 1);

    let mut range = RangeValue::new(120.0, -10.0);
    range.clamp(0.0, 100.0);
    assert_eq!(range, RangeValue::new(0.0, 100.0));

    let mut transfer = TransferState::<4>::new();
    assert!(transfer.toggle(2));
    assert!(transfer.is_included(2));
    assert_eq!(transfer.count(), 1);
    transfer.clear();
    assert_eq!(transfer.count(), 0);
}

#[test]
fn additional_chart_types_render_and_handle_small_rects() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let input = Input::new();
    let mut state = InteractionState::<8>::new();
    let slices = [
        PieSlice::labeled(4.0, Color::rgb(64, 156, 255), "Desktop"),
        PieSlice::labeled(2.0, Color::rgb(76, 210, 142), "Mobile"),
        PieSlice::labeled(1.0, Color::rgb(255, 190, 80), "Other"),
    ];
    let points = [
        ScatterPoint::new(0.1, 0.2),
        ScatterPoint::new(0.6, 0.8),
        ScatterPoint::new(0.9, 0.4),
    ];
    let values = [0.2, 0.6, 0.9, 0.4, 0.7, 0.5];
    let series = [RadarSeries::new(&values, Color::rgb(64, 156, 255))];
    let axes = ["Speed", "Power", "Range", "Cost", "Weight", "Safety"];
    let heat = [0.0, 0.2, 0.4, 0.6, 0.8, 1.0];
    let stages = [
        FunnelStage::new("Visit", 100.0, Color::rgb(64, 156, 255)),
        FunnelStage::new("Signup", 60.0, Color::rgb(76, 210, 142)),
        FunnelStage::new("Buy", 20.0, Color::rgb(255, 190, 80)),
    ];
    let items = [
        TreemapItem::new("Docs", 5.0, Color::rgb(64, 156, 255)),
        TreemapItem::new("Code", 3.0, Color::rgb(76, 210, 142)),
    ];
    let candles = [
        Candle::new(10.0, 14.0, 9.0, 13.0),
        Candle::new(13.0, 15.0, 11.0, 12.0),
    ];

    run_frame(&mut pixels, &theme, &input, &mut state, |ui| {
        ui.column(Rect::new(0, 0, 400, 320), 4, |ui| {
            let rect = ui.allocate(0, 70);
            ui.pie_chart(rect, &slices);
            ui.donut_chart(rect, &slices);
            ui.scatter_chart(rect, &points, (0.0, 1.0), (0.0, 1.0), Color::WHITE);
            ui.radar_chart(rect, &axes, &series);
            ui.heatmap(rect, 3, &heat, 0.0, 1.0, Color::BLACK, Color::WHITE);
            ui.funnel_chart(rect, &stages);
            ui.bullet_chart(rect, 64.0, 80.0, 100.0, "Revenue");
            ui.treemap_chart(rect, &items);
            ui.candlestick_chart(rect, &candles);

            let tiny = Rect::new(2, 2, 1, 1);
            ui.pie_chart(tiny, &slices);
            ui.scatter_chart(tiny, &points, (0.0, 1.0), (0.0, 1.0), Color::WHITE);
            ui.radar_chart(tiny, &axes, &series);
            ui.heatmap(tiny, 0, &[], 0.0, 1.0, Color::BLACK, Color::WHITE);
            ui.funnel_chart(tiny, &stages);
            ui.bullet_chart(tiny, 0.0, 0.0, 0.0, "");
            ui.treemap_chart(tiny, &items);
            ui.candlestick_chart(tiny, &candles);
        });
    });
}
