//! `feos-gui` is a dependency-free, allocation-free GUI toolkit for feos.
//!
//! It sits directly on top of [`feos_gfx`] and turns a raw XRGB8888
//! framebuffer into a small desktop toolkit: input events, a clipped painter,
//! a constraint-free layout engine, persistent interaction state, themed
//! widgets, a reusable component library, charts and draggable windows.
//!
//! # Design
//!
//! The toolkit is *immediate mode*: an app rebuilds its interface every frame
//! and the library returns a [`Response`] for every interactive widget.
//! Widgets do not allocate and do not own trees. State that must survive a
//! frame (hover, focus, the active/dragged widget, scroll offsets) lives in a
//! caller-owned [`InteractionState`]. Everything else is passed in explicitly:
//! checkboxes take `&mut bool`, text fields take a [`TextBuffer`], charts take
//! slices, and windows take a [`WindowState`].
//!
//! That combination is what makes the library usable on a bare-metal kernel:
//! there is no allocator, no `Rc`, no interior mutability and no `unsafe`.
//!
//! # A minimal frame
//!
//! ```
//! use feos_gfx::{Canvas, Color, Point, Rect};
//! use feos_gui::prelude::*;
//!
//! let mut pixels = [0u32; 320 * 200];
//! let mut canvas = Canvas::new(&mut pixels, 320, 200, 320).unwrap();
//! let theme = Theme::dark();
//! let input = Input::new();
//! let mut state = InteractionState::<8>::new();
//!
//! let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
//! ui.desktop_background(ui.canvas_rect());
//! ui.column(Rect::new(16, 16, 180, 100), 8, |ui| {
//!     ui.heading("hello, feos");
//!     ui.label("a tiny window into the machine");
//!     if ui.button("continue").clicked {
//!         // handle the click
//!     }
//! });
//! ```
//!
//! # Input
//!
//! A platform driver feeds [`Event`]s into [`Input`], calls
//! [`Input::clear_events`] once per frame, and keeps the pointer state that
//! [`Input`] tracks. [`Ui`] derives hover, press, click, drag, wheel and
//! keyboard focus from those events.
//!
//! # State
//!
//! [`InteractionState`] has a bounded number of scroll slots and focus-order
//! entries. It never grows. If an app has more scrollable regions than the
//! selected capacity, extra regions simply render unscrolled instead of
//! failing.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod chart;
pub mod components;
pub mod cursor;
pub mod geometry;
pub mod id;
pub mod input;
pub mod layout;
pub mod painter;
pub mod response;
pub mod state;
pub mod text;
pub mod theme;
pub mod ui;
pub mod widgets;

mod math;

pub use chart::{Chart, ChartPoint, Series, SeriesKind};
pub use components::*;
pub use cursor::{cursor_bounds, draw_cursor, draw_cursor_shape, CursorShape};
pub use geometry::{Align, Padding, RectExt, VerticalAlign};
pub use id::Id;
pub use input::{Event, Input, Key, Modifiers, MouseButton};
pub use layout::{Axis, LayoutStack};
pub use painter::Painter;
pub use response::{Response, Sense};
pub use state::{InteractionState, ScrollState};
pub use text::{TextBuffer, TextEditState};
pub use theme::{Metrics, Palette, Theme};
pub use ui::{Ui, WindowResponse, WindowState};
pub use widgets::*;

/// The widgets, types and traits most apps need in one import.
pub mod prelude {
    pub use crate::{
        AccordionState, Align, AutoSuggestResponse, AutoSuggestState, Axis, CalendarResponse,
        CalendarState, Candle, CascaderState, Chart, ChartPoint, ChatMessage, ChipResponse, Column,
        ColumnWidth, CommandBarResponse, CommandBarState, CommandItem, CommandPaletteState,
        ConfirmResult, DiffKind, DiffLine, DrawerSide, DrawerState, Event, FlipItem,
        FlyoutPlacement, FlyoutState, FunnelStage, GridItem, Hotkey, Icon, Id, InfoBarResponse,
        Input, InteractionState, Key, ListItem, ListResponse, ListState, MenuBarMenu,
        MenuBarResponse, MenuBarState, MenuEntry, MenuResponse, MenuState, Metrics, Modifiers,
        MouseButton, NavItem, NavigationViewResponse, NavigationViewState, Padding, Palette,
        PieSlice, PopoverState, Presence, PullState, RadarSeries, RangeSliderState, RangeValue,
        RectExt, Response, ScatterPoint, SelectState, Sense, Series, SeriesKind,
        SplitButtonResponse, SplitResponse, SplitState, SwipeResponse, SwipeState, TabViewResponse,
        TabViewState, TableResponse, TagSet, TeachingTipState, TextBuffer, TextEditState, Theme,
        TimelineItem, Toast, ToastQueue, Tone, TransferState, TreeRow, TreeState, TreemapItem, Ui,
        VerticalAlign, WindowState, ZoomState,
    };
    pub use feos_gfx::{Canvas, Color, Point, Rect, Size};
}
