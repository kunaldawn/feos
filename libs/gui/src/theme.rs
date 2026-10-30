//! Colours, spacing and typography for the toolkit.

use feos_gfx::Color;

/// Named colours used by every widget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// Desktop / page background.
    pub background: Color,
    /// Panels, cards and window bodies.
    pub surface: Color,
    /// A slightly recessed surface (buttons, input fields).
    pub surface_alt: Color,
    /// Hovered controls.
    pub surface_hover: Color,
    /// Pressed / selected controls.
    pub surface_active: Color,
    /// Default border.
    pub border: Color,
    /// Emphasised border.
    pub border_strong: Color,
    /// Keyboard focus ring.
    pub focus: Color,
    /// Primary text.
    pub text: Color,
    /// Secondary text and labels.
    pub text_muted: Color,
    /// Disabled text.
    pub text_disabled: Color,
    /// Accent fill (primary buttons, selection).
    pub accent: Color,
    /// Accent under the pointer.
    pub accent_hover: Color,
    /// Text drawn on top of the accent.
    pub accent_text: Color,
    /// Success / positive values.
    pub success: Color,
    /// Warning / attention values.
    pub warning: Color,
    /// Errors and destructive actions.
    pub danger: Color,
    /// Translucent-looking overlay colour for dialogs.
    pub overlay: Color,
    /// Shadow colour.
    pub shadow: Color,
    /// Chart grid lines.
    pub chart_grid: Color,
    /// Chart axis lines and labels.
    pub chart_axis: Color,
    /// Text selection highlight.
    pub selection: Color,
    /// Scrollbar track.
    pub scrollbar_track: Color,
    /// Scrollbar thumb.
    pub scrollbar_thumb: Color,
    /// Scrollbar thumb while hovered/dragged.
    pub scrollbar_hover: Color,
}

impl Palette {
    /// The default dark palette used by feos.
    pub const fn dark() -> Self {
        Self {
            background: Color::rgb(9, 12, 22),
            surface: Color::rgb(19, 24, 39),
            surface_alt: Color::rgb(28, 35, 55),
            surface_hover: Color::rgb(40, 50, 78),
            surface_active: Color::rgb(53, 66, 100),
            border: Color::rgb(45, 56, 84),
            border_strong: Color::rgb(76, 92, 132),
            focus: Color::rgb(92, 200, 255),
            text: Color::rgb(232, 238, 252),
            text_muted: Color::rgb(148, 162, 194),
            text_disabled: Color::rgb(92, 104, 132),
            accent: Color::rgb(64, 156, 255),
            accent_hover: Color::rgb(96, 178, 255),
            accent_text: Color::rgb(4, 12, 26),
            success: Color::rgb(76, 210, 142),
            warning: Color::rgb(255, 190, 80),
            danger: Color::rgb(255, 96, 112),
            overlay: Color::rgb(2, 4, 10),
            shadow: Color::rgb(0, 0, 0),
            chart_grid: Color::rgb(35, 44, 68),
            chart_axis: Color::rgb(110, 126, 162),
            selection: Color::rgb(38, 92, 168),
            scrollbar_track: Color::rgb(24, 30, 47),
            scrollbar_thumb: Color::rgb(75, 90, 124),
            scrollbar_hover: Color::rgb(108, 128, 174),
        }
    }

    /// A light palette for daylight use.
    pub const fn light() -> Self {
        Self {
            background: Color::rgb(238, 242, 249),
            surface: Color::rgb(255, 255, 255),
            surface_alt: Color::rgb(244, 247, 252),
            surface_hover: Color::rgb(230, 237, 248),
            surface_active: Color::rgb(214, 226, 244),
            border: Color::rgb(207, 216, 232),
            border_strong: Color::rgb(160, 175, 200),
            focus: Color::rgb(24, 112, 214),
            text: Color::rgb(22, 30, 46),
            text_muted: Color::rgb(92, 106, 132),
            text_disabled: Color::rgb(150, 160, 178),
            accent: Color::rgb(24, 112, 214),
            accent_hover: Color::rgb(16, 92, 184),
            accent_text: Color::rgb(255, 255, 255),
            success: Color::rgb(20, 138, 82),
            warning: Color::rgb(184, 112, 8),
            danger: Color::rgb(198, 48, 64),
            overlay: Color::rgb(20, 26, 38),
            shadow: Color::rgb(30, 40, 60),
            chart_grid: Color::rgb(225, 232, 244),
            chart_axis: Color::rgb(110, 126, 152),
            selection: Color::rgb(188, 214, 248),
            scrollbar_track: Color::rgb(236, 241, 249),
            scrollbar_thumb: Color::rgb(176, 190, 212),
            scrollbar_hover: Color::rgb(134, 154, 186),
        }
    }

    /// A high-contrast palette for accessibility and small displays.
    pub const fn high_contrast() -> Self {
        Self {
            background: Color::rgb(0, 0, 0),
            surface: Color::rgb(8, 8, 8),
            surface_alt: Color::rgb(20, 20, 20),
            surface_hover: Color::rgb(40, 40, 40),
            surface_active: Color::rgb(64, 64, 64),
            border: Color::rgb(180, 180, 180),
            border_strong: Color::rgb(255, 255, 255),
            focus: Color::rgb(255, 255, 0),
            text: Color::rgb(255, 255, 255),
            text_muted: Color::rgb(210, 210, 210),
            text_disabled: Color::rgb(140, 140, 140),
            accent: Color::rgb(255, 220, 0),
            accent_hover: Color::rgb(255, 240, 120),
            accent_text: Color::rgb(0, 0, 0),
            success: Color::rgb(0, 255, 120),
            warning: Color::rgb(255, 200, 0),
            danger: Color::rgb(255, 64, 64),
            overlay: Color::rgb(0, 0, 0),
            shadow: Color::rgb(0, 0, 0),
            chart_grid: Color::rgb(70, 70, 70),
            chart_axis: Color::rgb(200, 200, 200),
            selection: Color::rgb(80, 80, 0),
            scrollbar_track: Color::rgb(20, 20, 20),
            scrollbar_thumb: Color::rgb(190, 190, 190),
            scrollbar_hover: Color::rgb(255, 255, 255),
        }
    }
}

/// Sizes, spacing and font scales shared by widgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metrics {
    /// Body text scale (the bitmap font is 8x8 pixels).
    pub font_scale: u32,
    /// Small text scale, used for chart labels.
    pub small_scale: u32,
    /// Section heading scale.
    pub heading_scale: u32,
    /// Window title scale.
    pub title_scale: u32,
    /// Hero/display text scale.
    pub display_scale: u32,
    /// Height of buttons, inputs, tabs and sliders.
    pub control_height: i32,
    /// Height of a list/table row.
    pub row_height: i32,
    /// Horizontal padding inside controls.
    pub padding_x: i32,
    /// Vertical padding inside controls.
    pub padding_y: i32,
    /// Gap between widgets in a layout.
    pub gap: i32,
    /// Gap between sections.
    pub section_gap: i32,
    /// Corner radius for panels and controls.
    pub radius: u32,
    /// Border thickness.
    pub border_width: i32,
    /// Width of the scrollbar.
    pub scrollbar_width: i32,
    /// Height of a window title bar.
    pub title_bar_height: i32,
    /// Minimum sensible control size.
    pub min_control_size: i32,
    /// Extra spacing around tooltips.
    pub tooltip_padding: i32,
}

impl Metrics {
    /// The default desktop metrics.
    pub const fn desktop() -> Self {
        Self {
            font_scale: 2,
            small_scale: 1,
            heading_scale: 3,
            title_scale: 3,
            display_scale: 6,
            control_height: 34,
            row_height: 26,
            padding_x: 14,
            padding_y: 8,
            gap: 10,
            section_gap: 20,
            radius: 7,
            border_width: 1,
            scrollbar_width: 10,
            title_bar_height: 36,
            min_control_size: 24,
            tooltip_padding: 6,
        }
    }

    /// Compact metrics for dense screens and small widgets.
    pub const fn compact() -> Self {
        Self {
            font_scale: 1,
            small_scale: 1,
            heading_scale: 2,
            title_scale: 2,
            display_scale: 4,
            control_height: 24,
            row_height: 20,
            padding_x: 8,
            padding_y: 4,
            gap: 6,
            section_gap: 12,
            radius: 4,
            border_width: 1,
            scrollbar_width: 8,
            title_bar_height: 26,
            min_control_size: 18,
            tooltip_padding: 4,
        }
    }
}

/// A complete visual style: a palette plus metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub palette: Palette,
    pub metrics: Metrics,
}

impl Theme {
    /// The default dark theme.
    pub const fn dark() -> Self {
        Self {
            palette: Palette::dark(),
            metrics: Metrics::desktop(),
        }
    }

    /// The light theme.
    pub const fn light() -> Self {
        Self {
            palette: Palette::light(),
            metrics: Metrics::desktop(),
        }
    }

    /// A high-contrast dark theme.
    pub const fn high_contrast() -> Self {
        Self {
            palette: Palette::high_contrast(),
            metrics: Metrics::desktop(),
        }
    }

    /// The dark theme with compact metrics.
    pub const fn compact() -> Self {
        Self {
            palette: Palette::dark(),
            metrics: Metrics::compact(),
        }
    }

    /// Replaces the metrics, keeping the palette.
    pub const fn with_metrics(self, metrics: Metrics) -> Self {
        Self { metrics, ..self }
    }

    /// Replaces the palette, keeping the metrics.
    pub const fn with_palette(self, palette: Palette) -> Self {
        Self { palette, ..self }
    }

    /// Body text line height in pixels.
    pub const fn line_height(&self) -> i32 {
        (feos_gfx::font::GLYPH_HEIGHT as i32) * self.metrics.font_scale as i32
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn themes_keep_their_identity() {
        assert_ne!(
            Theme::dark().palette.background,
            Theme::light().palette.background
        );
        assert_eq!(Theme::dark().metrics, Theme::light().metrics);
        assert_eq!(Theme::default(), Theme::dark());
    }
}
