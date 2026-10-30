# feos-gui component reference

This directory contains the higher-level component layer of `feos-gui`. It sits
on top of the base widgets in [`../widgets`](../widgets/) and the painter in
[`../painter.rs`](../painter.rs). Everything here follows the same rules:

- **Immediate mode** — a component is called again every frame.
- **Caller-owned state** — selection, expansion, open flags, buffers and values
  are passed in as `&mut`; there is no hidden widget tree.
- **No allocation** — lists, tables, menus, threads and charts use fixed-capacity
  state and caller-provided slices.
- **No `unsafe`** — the crate forbids unsafe code.
- **Clipped painting** — every component draws through `Painter`, so it is safe
  to place inside a window, panel or scroll area.
- **Floating surfaces paint inline** — menus, flyouts, popovers, drawers,
  tooltips, toasts, sheets and the command palette must be drawn late in the
  frame. There is no separate z-order stack.

The component directory currently exposes **112 component methods** across
twelve modules, plus the base widgets and charts covered in the appendix.

## Quick start

```rust
use feos_gui::prelude::*;

let mut list = ListState::<8>::new();
let mut select = SelectState::new();
let mut menu = MenuState::new();
let mut choice = 0usize;
let mut page = 0usize;

ui.column(body, 12, |ui| {
    let items = [
        ListItem::new(Icon::Home, "Overview"),
        ListItem::new(Icon::Folder, "Files").with_detail("12 items"),
        ListItem::new(Icon::Settings, "Settings"),
    ];
    let list_rect = ui.allocate(0, 140);
    ui.list_view(list_rect, &items, &mut list);

    let row = ui.allocate(0, ui.theme.metrics.control_height);
    ui.row(row, 10, |ui| {
        ui.select(
            &mut select,
            &mut choice,
            &["Dark", "Light", "High contrast"],
            "Theme",
        );
        ui.menu_button(
            &mut menu,
            "Actions",
            &[MenuEntry::item("Duplicate"), MenuEntry::item("Archive")],
        );
        ui.pagination(&mut page, 8);
    });
});
```

State types with `const fn new()` can live in `static` demo state. Text fields
take a fixed-capacity `TextBuffer<N>`; selection and expansion tables take a
capacity such as `ListState<8>` or `TreeState<32>`.

## Inputs and selection

Source: [`inputs.rs`](inputs.rs)

| Component | Purpose | State / result |
| --- | --- | --- |
| `ui.search_box(buffer, placeholder)` | Search field with magnifier and clear button. | `TextBuffer<M>`, `Response` |
| `ui.select(state, selected, items, placeholder)` | Dropdown select with a popup list. | `SelectState`, `usize`, `Response` |
| `ui.dropdown(...)` | Alias for `select`. | `SelectState`, `usize`, `Response` |
| `ui.menu_button(state, label, entries)` | Button that opens a command menu. | `MenuState`, `MenuResponse` |
| `ui.segmented(selected, labels)` | Segmented control. | `usize`, `Response` |
| `ui.number_stepper(value, min, max, step, decimals)` | Numeric field with minus/plus buttons. | `f32`, `Response` |
| `ui.rating(value, max)` | Star rating. | `u8`, `Response` |
| `ui.color_picker(selected, colors)` | Row of colour swatches. | `usize`, `Response` |
| `ui.form_field(label, hint, closure)` | Label + hint wrapper for any control. | Closure return value |
| `ui.toggle_button(value, label)` | WinUI `ToggleButton`. | `bool`, `Response` |
| `ui.split_button(state, label, entries)` | Primary action plus menu chevron. | `MenuState`, `SplitButtonResponse` |
| `ui.dropdown_button(state, label, entries)` | Full-width dropdown command button. | `MenuState`, `MenuResponse` |
| `ui.hyperlink_button(label)` | Text link styled as a hyperlink. | `Response` |
| `ui.radio_group(selected, labels)` | Vertical radio group. | `usize`, `Response` |
| `ui.auto_suggest(state, buffer, placeholder, suggestions)` | Filtered suggestion list below a text field. | `AutoSuggestState`, `AutoSuggestResponse` |
| `ui.calendar(rect, state)` | Month calendar. | `CalendarState`, `CalendarResponse` |
| `ui.date_picker(state)` | Date field with calendar popup. | `CalendarState`, `Response` |
| `ui.time_picker(hour, minute)` | 24-hour time field. | `u32`, `u32`, `Response` |

Supporting types: `SelectState`, `MenuState`, `MenuEntry`, `MenuResponse`,
`SplitButtonResponse`, `AutoSuggestState`, `AutoSuggestResponse`,
`CalendarState`, `CalendarResponse`.

## Advanced inputs

Source: [`advanced.rs`](advanced.rs)

These components are available both as `ui.method(...)` and as free functions
taking `ui` as their first argument, for use inside generic helpers.

| Component | Purpose | State / result |
| --- | --- | --- |
| `ui.password_box(buffer, placeholder)` | Masked single-line password field. | `TextBuffer<M>`, `Response` |
| `ui.text_area(buffer, rows, placeholder)` | Multiline editor with newline and cursor handling. | `TextBuffer<M>`, `Response` |
| `ui.range_slider(state, value, min, max)` | Two-thumb range slider. | `RangeSliderState`, `RangeValue`, `Response` |
| `ui.knob(value, min, max, label)` | Rotary knob; drag vertically to change. | `f32`, `Response` |
| `ui.tag_input(tags, draft, placeholder)` | Removable tag chips plus a draft field. | `TagSet<T, W>`, `TextBuffer<M>`, `Response` |
| `ui.file_dropzone(rect, title, hint)` | Dashed drop zone with a Browse button. | `Response` |
| `ui.hotkey_input(hotkey)` | Records a keyboard shortcut. | `Hotkey`, `Response` |
| `ui.cascader(rect, state, columns)` | Multi-column cascading list. | `CascaderState<C>`, `Response` |
| `ui.transfer_list(rect, items, state)` | Two-pane transfer list. | `TransferState<M>`, `Response` |
| `ui.mention_box(state, buffer, people)` | Search field with `@mention` suggestions. | `AutoSuggestState`, `TextBuffer<M>`, `AutoSuggestResponse` |
| `ui.dashed_rect(rect, color)` | Dashed outline helper used by drop zones. | — |

Supporting types: `RangeValue`, `RangeSliderState`, `TagSet`, `Hotkey`,
`CascaderState`, `TransferState`.

## Navigation

Source: [`navigation.rs`](navigation.rs)

| Component | Purpose | State / result |
| --- | --- | --- |
| `ui.toolbar(rect, closure)` | Raised horizontal toolbar surface. | Closure return value |
| `ui.status_bar(rect, left, right)` | Status strip with left/right text. | `Rect` |
| `ui.breadcrumbs(items, current)` | Clickable breadcrumb trail. | `usize`, `Response` |
| `ui.pagination(page, pages)` | Previous/next and numbered pages. | `usize`, `Response` |
| `ui.steps(current, labels)` | Horizontal step indicator. | `usize`, `Response` |
| `ui.navigation_view(state, rect, title, items, closure)` | WinUI navigation pane plus content. | `NavigationViewState`, `NavItem`, `(NavigationViewResponse, R)` |
| `ui.tab_view(state, rect, closure)` | Tab strip with close buttons, add button and content. | `TabViewState<M>`, `TabViewResponse` |
| `ui.menu_bar(state, menus)` | Classic desktop menu bar. | `MenuBarState`, `MenuBarMenu`, `MenuBarResponse` |
| `ui.command_bar(state, items)` | Icon command bar with overflow menu. | `CommandBarState`, `CommandItem`, `CommandBarResponse` |

Supporting types: `NavItem`, `NavigationViewState`, `NavigationViewResponse`,
`TabViewState`, `TabViewResponse`, `MenuBarMenu`, `MenuBarState`,
`MenuBarResponse`, `CommandItem`, `CommandBarState`, `CommandBarResponse`.

## Collections

Source: [`collections.rs`](collections.rs)

| Component | Purpose | State / result |
| --- | --- | --- |
| `ui.list_view(rect, items, state)` | Scrollable, selectable list with icons and detail lines. | `ListState<M>`, `ListItem`, `ListResponse` |
| `ui.grid_view(rect, items, selected)` | Wrapping card grid. | `GridItem`, `usize`, `Response` |
| `ui.flip_view(rect, items, index)` | One-slide carousel with arrows and page dots. | `FlipItem`, `usize`, `Response` |
| `ui.pips_pager(current, count)` | Fluent page dots for a carousel. | `usize`, `Response` |

Supporting types: `ListItem`, `ListState`, `ListResponse`, `GridItem`,
`FlipItem`.

## Data display

Source: [`data.rs`](data.rs)

| Component | Purpose | State / result |
| --- | --- | --- |
| `ui.table(rect, columns, rows, selected)` | Table with sticky header, selection and wheel scrolling. | `Column`, `TableResponse` |
| `ui.key_value(label, value)` | One label/value row. | `Rect` |
| `ui.key_value_list(rect, rows)` | Compact list of label/value rows. | `Rect` |
| `ui.timeline(rect, items)` | Vertical timeline with tone markers. | `TimelineItem`, `Rect` |
| `ui.tree(rect, rows, expanded, selected)` | Scrollable tree with expansion flags. | `TreeRow`, `Response` |
| `ui.tree_state(rect, rows, state)` | Tree backed by `TreeState`. | `TreeState<M>`, `Response` |
| `ui.stat_card(rect, label, value, detail, tone)` | KPI/stat card. | `Tone`, `Rect` |
| `ui.avatar(initials, color, size)` | Circular initials avatar. | `Rect` |
| `ui.chip(label, color, removable)` | Tag/chip with optional remove button. | `ChipResponse` |
| `ui.info_badge(count, tone)` | WinUI numeric notification badge. | `Tone`, `Rect` |
| `ui.persona(name, detail, color, size)` | Avatar + name + detail row. | `Rect` |

Supporting types: `Column`, `ColumnWidth`, `TableResponse`, `TimelineItem`,
`TreeRow`, `TreeState`, `ChipResponse`.

## Layout and surfaces

Source: [`layout.rs`](layout.rs)

| Component | Purpose | State / result |
| --- | --- | --- |
| `ui.accordion_section(state, index, title, height, closure)` | One accordion section with a body. | `AccordionState<M>`, `(Response, Option<R>)` |
| `ui.accordion(state, index, title, height, closure)` | Accordion header only; body is drawn separately. | `AccordionState<M>`, `Response` |
| `ui.split_pane(rect, axis, state, first, second)` | Two panes with draggable divider. | `SplitState`, `SplitResponse` |
| `ui.drawer(anchor, side, state, size, title, closure)` | Modal drawer anchored to an edge. | `DrawerState`, `Option<R>` |
| `ui.popover(anchor, state, width, height, closure)` | Floating panel anchored below a control. | `PopoverState`, `Option<R>` |
| `ui.settings_card(rect, icon, title, description, closure)` | Settings row with trailing control. | Closure return value |
| `ui.section_header(title, action)` | Section heading with optional action link. | `Response` |
| `ui.hero_banner(rect, eyebrow, title, message, closure)` | Large accent banner with actions. | Closure return value |

Supporting types: `AccordionState`, `SplitState`, `SplitResponse`, `DrawerSide`,
`DrawerState`, `PopoverState`.

## Feedback

Source: [`feedback.rs`](feedback.rs)

| Component | Purpose | State / result |
| --- | --- | --- |
| `ui.alert(tone, title, message)` | Informational alert. | `Tone`, `Rect` |
| `ui.alert_dismissible(tone, title, message, open)` | Alert with close button. | `bool`, `Response` |
| `ui.toasts(anchor, queue)` | Floating toast stack. | `ToastQueue<M>`, `Rect` |
| `ui.spinner(size)` | Animated loading spinner in the next layout cell. | `Rect` |
| `ui.spinner_at(rect)` | Spinner drawn in a caller-provided rectangle. | — |
| `ui.skeleton(rect)` | Shimmering skeleton block. | — |
| `ui.skeleton_lines(lines)` | Stack of skeleton text lines. | `Rect` |
| `ui.empty_state(rect, icon, title, message)` | Centered empty-state placeholder. | `Icon`, `Rect` |
| `ui.loading_overlay(rect, message)` | Dimmed area with spinner and message. | — |
| `ui.progress_ring(rect, value, label)` | Circular progress gauge. | `Rect` |
| `ui.confirm_dialog(rect, title, message, open, confirm, cancel)` | Modal confirmation dialog. | `ConfirmResult` |
| `ui.info_bar(tone, title, message, action, open)` | WinUI info bar. | `InfoBarResponse` |

Supporting types: `Toast`, `ToastQueue`, `ConfirmResult`, `InfoBarResponse`,
`Tone`.

## Overlays

Source: [`overlays.rs`](overlays.rs)

| Component | Purpose | State / result |
| --- | --- | --- |
| `ui.context_menu(anchor, state, entries)` | Context menu at a pointer anchor. | `MenuState`, `MenuResponse` |
| `ui.flyout(anchor, state, placement, width, height, closure)` | Floating panel on any side of an anchor. | `FlyoutState`, `Option<R>` |
| `ui.teaching_tip(anchor, state, title, message)` | Dismissible WinUI teaching tip. | `TeachingTipState`, `Response` |

Supporting types: `FlyoutPlacement`, `FlyoutState`, `TeachingTipState`.

## Windows/Fluent patterns

Source: [`patterns.rs`](patterns.rs)

| Component | Purpose | State / result |
| --- | --- | --- |
| `ui.avatar_group(initials, colors, size)` | Overlapping avatar row. | `Rect` |
| `ui.presence_badge(initials, color, presence, size)` | Avatar with an online/away/busy dot. | `Presence`, `Rect` |
| `ui.group_box(rect, title, closure)` | Bordered group with a cut-in title. | Closure return value |
| `ui.property_grid(rect, rows)` | Two-column property list. | `Rect` |
| `ui.log_view(rect, lines, state)` | Scrollable log viewer with line numbers and tone prefixes. | `ScrollState`, `Response` |
| `ui.code_block(rect, code)` | Framed code block with line-number gutter. | `Rect` |
| `ui.diff_view(rect, lines)` | Unified diff with added/removed tint. | `DiffLine`, `Rect` |
| `ui.comment_thread(messages)` | Stacked comment discussion. | `ChatMessage`, `Rect` |
| `ui.chat_thread(messages)` | Chat transcript with right-aligned outgoing messages. | `ChatMessage`, `Rect` |
| `ui.result_page(rect, icon, title, message, action)` | Centered result page with one action. | `Icon`, `Response` |
| `ui.selector_bar(selected, labels)` | Selector bar with accent underline. | `usize`, `Response` |
| `ui.pivot(selected, labels)` | Compact Fluent pivot headers. | `usize`, `Response` |
| `ui.command_bar_flyout(state, label, entries)` | Command-bar overflow menu. | `MenuState`, `MenuResponse` |
| `ui.title_bar(rect, title, subtitle)` | Title bar with glyph and window controls. | `Response` |
| `ui.float_button(icon)` | Circular floating action button. | `Icon`, `Response` |
| `ui.back_to_top(visible)` | Back-to-top affordance. | `bool`, `Response` |
| `ui.bottom_sheet(rect, open, title, closure)` | Modal bottom sheet. | `bool`, `Option<R>` |
| `ui.popconfirm(anchor, open, message)` | Compact inline confirmation popover. | `bool`, `ConfirmResult` |
| `ui.command_palette(state, commands)` | Searchable command palette with keyboard navigation. | `CommandPaletteState`, `Option<usize>` |
| `ui.swipe_actions(rect, state, actions, closure)` | Swipe-to-reveal row actions. | `SwipeState`, `SwipeResponse<R>` |
| `ui.pull_to_refresh(rect, state)` | Pull-to-refresh header. | `PullState`, `Response` |
| `ui.annotated_scrollbar(rect, viewport, content, offset, marks)` | Scrollbar with annotated hit marks. | `i32`, `Response` |
| `ui.zoom_control(zoom, min, max)` | Minus/plus zoom control with slider. | `ZoomState`, `Response` |

Supporting types: `Presence`, `DiffKind`, `DiffLine`, `ChatMessage`,
`CommandPaletteState`, `SwipeState`, `SwipeResponse`, `PullState`, `ZoomState`.

## Icons

Source: [`icons.rs`](icons.rs)

| Component | Purpose | Result |
| --- | --- | --- |
| `ui.icon(icon, size, color)` | Draw an icon in a square cell. | `Rect` |
| `ui.icon_button(icon, primary)` | Square icon button in the next layout cell. | `Response` |
| `ui.icon_button_sized(size, icon, primary)` | Square icon button with explicit size. | `Response` |
| `ui.icon_button_at(rect, icon, primary)` | Icon button in a caller-provided rectangle. | `Response` |
| `paint_icon(painter, rect, icon, color)` | Direct painter helper used by custom components. | — |

`Icon::ALL` contains 72 bitmap icons:

`ChevronUp`, `ChevronDown`, `ChevronLeft`, `ChevronRight`, `ArrowUp`,
`ArrowDown`, `ArrowLeft`, `ArrowRight`, `Check`, `Close`, `Plus`, `Minus`,
`Search`, `Menu`, `Settings`, `Info`, `Warning`, `Error`, `Success`, `Help`,
`Home`, `User`, `Bell`, `Calendar`, `Clock`, `File`, `Folder`, `Trash`, `Edit`,
`Refresh`, `Download`, `Upload`, `Play`, `Pause`, `Star`, `Heart`,
`MoreHorizontal`, `MoreVertical`, `Lock`, `Unlock`, `Eye`, `EyeOff`, `Filter`,
`Sort`, `Grid`, `List`, `Wifi`, `Sun`, `Moon`, `Copy`, `ExternalLink`, `Mail`,
`MapPin`, `Globe`, `Code`, `Database`, `Cpu`, `Cloud`, `Bookmark`, `Tag`,
`Flag`, `LogIn`, `LogOut`, `Undo`, `Redo`, `Shield`, `Sliders`, `Cart`,
`CreditCard`, `Package`, `Terminal`, `Link`.

## Charts

Source: [`charts_extra.rs`](charts_extra.rs)

| Component | Purpose | Result |
| --- | --- | --- |
| `ui.pie_chart(rect, slices)` | Pie chart with optional legend. | `PieSlice`, `Response` |
| `ui.donut_chart(rect, slices)` | Donut chart with optional legend. | `PieSlice`, `Response` |
| `ui.scatter_chart(rect, points, x_range, y_range, color)` | Scatter plot with explicit axis ranges. | `ScatterPoint`, `Response` |
| `ui.radar_chart(rect, axes, series)` | Radar/spider chart using `0..=1` values. | `RadarSeries`, `Response` |
| `ui.heatmap(rect, columns, values, min, max, low, high)` | Row-major heatmap. | `Response` |
| `ui.funnel_chart(rect, stages)` | Funnel chart normalised to the largest stage. | `FunnelStage`, `Response` |
| `ui.bullet_chart(rect, value, target, max, label)` | Bullet graph with target marker. | `Response` |
| `ui.treemap_chart(rect, items)` | Proportional treemap strips. | `TreemapItem`, `Response` |
| `ui.candlestick_chart(rect, candles)` | OHLC candlestick chart. | `Candle`, `Response` |

Supporting types: `PieSlice`, `ScatterPoint`, `RadarSeries`, `FunnelStage`,
`TreemapItem`, `Candle`.

## Supporting state and data types

Most types are re-exported from the crate root and the prelude.

| Type | Used by | Notes |
| --- | --- | --- |
| `Tone` | Alerts, info bars, badges, timeline, stat cards, chips | `Neutral`, `Info`, `Success`, `Warning`, `Danger`; resolves icons, colours and tints. |
| `SelectState` | `select`, `dropdown` | Open/highlighted popup state. |
| `MenuState` | Menus, context menus, split/dropdown buttons, command flyout | Open/highlighted entry. |
| `MenuEntry` | All menu-bearing components | Item with optional shortcut, disabled item, or separator. |
| `MenuResponse` | Menu components | `response`, `selected`, `dismissed`. |
| `SplitButtonResponse` | `split_button` | Primary-button response plus menu selection. |
| `AutoSuggestState` / `AutoSuggestResponse` | `auto_suggest`, `mention_box` | Open/highlighted and selected index. |
| `CalendarState` / `CalendarResponse` | `calendar`, `date_picker` | Year, month, day, open flag and selection. |
| `ListState` / `ListResponse` | `list_view` | Fixed-capacity selection flags and activated row. |
| `ListItem`, `GridItem`, `FlipItem` | Collection views | Small `Copy` item descriptors. |
| `Column` / `ColumnWidth` / `TableResponse` | `table` | Fixed or flex columns and header clicks. |
| `TreeRow`, `TreeState` | `tree`, `tree_state` | Fixed-capacity expansion flags and selection. |
| `TimelineItem`, `ChipResponse` | Data display | Timeline entries and chip removal result. |
| `AccordionState` | `accordion` | Fixed-capacity open flags; optional single-open mode. |
| `SplitState` / `SplitResponse` | `split_pane` | Divider fraction and drag result. |
| `DrawerSide`, `DrawerState` | `drawer` | Left/Right side and open flag. |
| `PopoverState` | `popover` | Open flag. |
| `Toast`, `ToastQueue` | `toasts` | Fixed-capacity queue with TTLs. |
| `ConfirmResult` | `confirm_dialog`, `popconfirm` | Confirmed/cancelled/closed flags. |
| `InfoBarResponse` | `info_bar` | Response and action click. |
| `FlyoutPlacement`, `FlyoutState` | `flyout` | Bottom/Top/Left/Right placement and open flag. |
| `TeachingTipState` | `teaching_tip` | Open flag. |
| `RangeValue`, `RangeSliderState` | `range_slider` | Start/end values and active thumb. |
| `TagSet` | `tag_input` | Fixed-capacity UTF-8 tag set. |
| `Hotkey` | `hotkey_input` | Modifiers plus optional key. |
| `CascaderState` | `cascader` | Selected index per column and current depth. |
| `TransferState` | `transfer_list` | Included flags for the right-hand pane. |
| `PieSlice`, `ScatterPoint`, `RadarSeries`, `FunnelStage`, `TreemapItem`, `Candle` | Charts | Caller-owned chart data. |
| `Presence`, `DiffLine`, `DiffKind`, `ChatMessage` | Patterns | Presence, diff lines and thread messages. |
| `CommandPaletteState` | `command_palette` | Open flag, highlight and fixed query buffer. |
| `SwipeState` / `SwipeResponse` | `swipe_actions` | Revealed offset, action index and inner result. |
| `PullState` | `pull_to_refresh` | Pull distance, armed and refreshing flags. |
| `ZoomState` | `zoom_control` | Zoom factor. |
| `ScrollState` | `log_view`, custom scrolling | Offset, viewport height and content height. |

## Base widget appendix

The base widgets live in [`../widgets`](../widgets/). They are the primitives
that components compose.

| Module | Widgets |
| --- | --- |
| [`widgets/basic.rs`](../widgets/basic.rs) | `label`, `label_muted`, `label_colored`, `heading`, `display_text`, `paragraph`, `button`, `button_sized`, `primary_button`, `primary_button_sized`, `button_at`, `checkbox`, `radio`, `toggle`, `slider`, `progress`, `badge`, `separator`, `space` |
| [`widgets/containers.rs`](../widgets/containers.rs) | `panel`, `panel_with_padding`, `card`, `scroll_area`, `scroll_area_with_id`, `tabs`, `window`, `dialog`, `desktop_background`, `tooltip`, `list` |
| [`widgets/text_input.rs`](../widgets/text_input.rs) | `text_input`, `text_input_with_placeholder` |
| [`chart.rs`](../chart.rs) | `chart`, `line_chart`, `bar_chart`, `sparkline` |

`Chart` also exposes line, area and bar series through `Series`, `ChartPoint`
and `SeriesKind`. See [`../chart.rs`](../chart.rs) for the builder API.

## Behaviour notes

- **Draw floating components last.** This includes `select`, `menu_button`,
  `context_menu`, `split_button`, `dropdown_button`, `menu_bar`,
  `command_bar`, `flyout`, `popover`, `drawer`, `teaching_tip`, `toasts`,
  `bottom_sheet`, `popconfirm` and `command_palette`.
- **Menus own the keyboard while open.** Arrow keys, Home/End and Enter operate
  the open menu even when focus still points at the owning button.
- **Use `allocate_sized` for fixed-width controls.** In a column, `allocate`
  returns the full available width; `allocate_sized` preserves an explicit
  width for knobs, buttons, scrollbars and zoom controls.
- **Small rectangles are safe.** Components clip their drawing and clamp sizes;
  passing a tiny rect should render less, not panic.
- **Prefer fixed capacities.** List, tree, accordion, tab and toast state all
  use const-generic capacities and degrade gracefully when full.
- **Text is always `&str` plus fixed buffers.** The component layer never
  allocates strings for labels, suggestions or chart legends.

## Tests

Component behaviour is covered by
[`../../tests/components.rs`](../../tests/components.rs), including selection,
menus, tabs, navigation, calendars, info bars, command bars, charts, advanced
inputs and the newer pattern components. Run the host suite with:

```sh
make test
```
