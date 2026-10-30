//! Reusable higher-level components.
//!
//! The widgets in [`crate::widgets`] are the small primitives: buttons, text
//! fields, panels and charts. This module composes them into the pieces an app
//! actually puts on a screen: search fields, selects, menus, tables, trees,
//! alerts, toasts, pagination, split panes and more. The `advanced` submodule
//! adds masked/multiline inputs, range controls, tag editors and cascaders;
//! `patterns` adds Windows/Fluent composition surfaces such as title bars,
//! property grids, chat threads, sheets, command palettes and result pages.
//!
//! Every component follows the same rules as the primitives:
//!
//! - state is caller-owned and passed as `&mut`;
//! - nothing allocates, and no component owns a widget tree;
//! - components are inherent methods on [`Ui`](crate::Ui), so an app can mix
//!   them with custom widgets without importing a trait.
//!
//! Some components paint a floating surface (selects, menus, popovers,
//! dialogs and toasts). Draw those at the end of the frame so they appear over
//! the content they overlap. This is the usual immediate-mode layering rule;
//! there is no hidden z-order stack.

pub mod advanced;
pub mod charts_extra;
pub mod collections;
pub mod data;
pub mod feedback;
pub mod icons;
pub mod inputs;
pub mod layout;
pub mod navigation;
pub mod overlays;
pub mod patterns;

pub use advanced::*;
pub use charts_extra::*;
pub use collections::*;
pub use data::*;
pub use feedback::*;
pub use icons::*;
pub use inputs::*;
pub use layout::*;
pub use navigation::*;
pub use overlays::*;
pub use patterns::*;

use feos_gfx::Color;

use crate::theme::Palette;

/// A semantic colour used by alerts, toasts, timelines and status chips.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tone {
    /// Neutral / informational grey.
    #[default]
    Neutral,
    /// Blue information.
    Info,
    /// Green success.
    Success,
    /// Amber warning.
    Warning,
    /// Red danger or destructive action.
    Danger,
}

impl Tone {
    /// Resolves the tone to a palette colour.
    pub const fn color(self, palette: Palette) -> Color {
        match self {
            Self::Neutral => palette.text_muted,
            Self::Info => palette.accent,
            Self::Success => palette.success,
            Self::Warning => palette.warning,
            Self::Danger => palette.danger,
        }
    }

    /// The icon that represents this tone.
    pub const fn icon(self) -> Icon {
        match self {
            Self::Neutral => Icon::Info,
            Self::Info => Icon::Info,
            Self::Success => Icon::Success,
            Self::Warning => Icon::Warning,
            Self::Danger => Icon::Error,
        }
    }

    /// A very subtle tint over `surface`, suitable for alert backgrounds.
    pub fn tint(self, palette: Palette) -> Color {
        palette.surface.lerp(self.color(palette), 20)
    }
}
