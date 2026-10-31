//! Visual themes for the desktop shell.
//!
//! A [`DesktopTheme`] is the desktop's own palette: XP-style gradients for the
//! taskbar, title bars and Start menu, plus a [`feos_gui::Theme`] used by the
//! widgets inside application windows.

use feos_gfx::Color;
use feos_gui::{Metrics, Palette, Theme};

use crate::config::DesktopConfig;

/// A named desktop theme.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ThemeName {
    /// Windows XP Luna blue.
    #[default]
    XpBlue,
    /// Windows XP Luna olive green.
    XpOlive,
    /// Windows XP Luna silver.
    XpSilver,
    /// A modern dark theme.
    Dark,
    /// A modern light theme.
    Light,
    /// A high-contrast theme for accessibility.
    HighContrast,
}

impl ThemeName {
    /// Parses a theme name from configuration.
    pub fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("xp-blue")
            || value.eq_ignore_ascii_case("xp")
            || value.eq_ignore_ascii_case("luna")
        {
            Some(Self::XpBlue)
        } else if value.eq_ignore_ascii_case("xp-olive") || value.eq_ignore_ascii_case("olive") {
            Some(Self::XpOlive)
        } else if value.eq_ignore_ascii_case("xp-silver") || value.eq_ignore_ascii_case("silver") {
            Some(Self::XpSilver)
        } else if value.eq_ignore_ascii_case("dark") {
            Some(Self::Dark)
        } else if value.eq_ignore_ascii_case("light") {
            Some(Self::Light)
        } else if value.eq_ignore_ascii_case("high-contrast")
            || value.eq_ignore_ascii_case("high_contrast")
            || value.eq_ignore_ascii_case("contrast")
        {
            Some(Self::HighContrast)
        } else {
            None
        }
    }

    /// The canonical configuration name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::XpBlue => "xp-blue",
            Self::XpOlive => "xp-olive",
            Self::XpSilver => "xp-silver",
            Self::Dark => "dark",
            Self::Light => "light",
            Self::HighContrast => "high-contrast",
        }
    }

    /// Returns `true` for themes that want a dark wallpaper by default.
    pub const fn is_dark(self) -> bool {
        matches!(self, Self::Dark | Self::HighContrast)
    }
}

/// Desktop-specific colours and the widget theme used inside windows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DesktopTheme {
    /// Theme name.
    pub name: ThemeName,
    /// Theme used by `feos-gui` widgets inside windows.
    pub ui: Theme,

    /// Wallpaper gradient top.
    pub wallpaper_top: Color,
    /// Wallpaper gradient bottom.
    pub wallpaper_bottom: Color,
    /// Wallpaper grid lines.
    pub wallpaper_grid: Color,

    /// Taskbar gradient top.
    pub taskbar_top: Color,
    /// Taskbar gradient bottom.
    pub taskbar_bottom: Color,
    /// Taskbar top highlight.
    pub taskbar_highlight: Color,
    /// Taskbar text.
    pub taskbar_text: Color,

    /// Start button gradient top.
    pub start_top: Color,
    /// Start button gradient bottom.
    pub start_bottom: Color,
    /// Start button border.
    pub start_border: Color,
    /// Start button text.
    pub start_text: Color,

    /// Quick-launch strip background.
    pub quick_bg: Color,
    /// Notification-area background.
    pub tray_bg: Color,
    /// Notification-area text.
    pub tray_text: Color,

    /// Task button fill.
    pub task_button: Color,
    /// Task button hover fill.
    pub task_button_hover: Color,
    /// Task button pressed/active fill.
    pub task_button_active: Color,
    /// Task button border.
    pub task_border: Color,
    /// Task button text.
    pub task_text: Color,

    /// Active window border.
    pub window_border_active: Color,
    /// Inactive window border.
    pub window_border_inactive: Color,
    /// Window body.
    pub window_bg: Color,
    /// Window content panel.
    pub window_content: Color,

    /// Active title-bar gradient top.
    pub title_active_top: Color,
    /// Active title-bar gradient bottom.
    pub title_active_bottom: Color,
    /// Inactive title-bar gradient top.
    pub title_inactive_top: Color,
    /// Inactive title-bar gradient bottom.
    pub title_inactive_bottom: Color,
    /// Active title text.
    pub title_text: Color,
    /// Inactive title text.
    pub title_text_inactive: Color,

    /// Start-menu background.
    pub menu_bg: Color,
    /// Start-menu border.
    pub menu_border: Color,
    /// Start-menu header gradient top.
    pub menu_header_top: Color,
    /// Start-menu header gradient bottom.
    pub menu_header_bottom: Color,
    /// Start-menu header text.
    pub menu_header_text: Color,
    /// Start-menu body text.
    pub menu_text: Color,
    /// Start-menu muted text.
    pub menu_muted: Color,
    /// Start-menu hover/selection fill.
    pub menu_hover: Color,
    /// Start-menu hover/selection text.
    pub menu_hover_text: Color,
    /// Start-menu separators.
    pub menu_separator: Color,

    /// Desktop shortcut label.
    pub desktop_text: Color,
    /// Desktop shortcut label with the pointer over it.
    pub desktop_text_hover: Color,

    /// Close-button fill.
    pub close_button: Color,
    /// Minimise/maximise button fill.
    pub caption_button: Color,
    /// Caption button glyph.
    pub caption_glyph: Color,
}

impl DesktopTheme {
    /// Builds the desktop theme selected by `config`.
    pub const fn from_config(config: &DesktopConfig) -> Self {
        let mut theme = match config.theme {
            ThemeName::XpBlue => Self::xp_blue(),
            ThemeName::XpOlive => Self::xp_olive(),
            ThemeName::XpSilver => Self::xp_silver(),
            ThemeName::Dark => Self::dark(),
            ThemeName::Light => Self::light(),
            ThemeName::HighContrast => Self::high_contrast(),
        };
        theme.wallpaper_top = config.wallpaper.top;
        theme.wallpaper_bottom = config.wallpaper.bottom;
        theme.wallpaper_grid = config.wallpaper.grid;
        theme
    }

    /// Windows XP Luna blue.
    pub const fn xp_blue() -> Self {
        let mut palette = Palette::light();
        palette.surface = Color::rgb(244, 241, 232);
        palette.surface_alt = Color::rgb(236, 233, 216);
        palette.surface_hover = Color::rgb(222, 232, 250);
        palette.surface_active = Color::rgb(198, 214, 246);
        palette.border = Color::rgb(172, 168, 153);
        palette.border_strong = Color::rgb(0, 84, 227);
        palette.accent = Color::rgb(0, 84, 227);
        palette.accent_hover = Color::rgb(48, 118, 240);
        palette.accent_text = Color::WHITE;
        palette.text = Color::rgb(24, 24, 24);
        palette.text_muted = Color::rgb(92, 92, 92);
        palette.focus = Color::rgb(0, 84, 227);

        Self {
            name: ThemeName::XpBlue,
            ui: Theme {
                palette,
                metrics: Metrics::compact(),
            },
            wallpaper_top: Color::rgb(63, 140, 243),
            wallpaper_bottom: Color::rgb(36, 94, 219),
            wallpaper_grid: Color::rgb(84, 156, 246),
            taskbar_top: Color::rgb(63, 140, 243),
            taskbar_bottom: Color::rgb(25, 65, 165),
            taskbar_highlight: Color::rgb(98, 168, 255),
            taskbar_text: Color::WHITE,
            start_top: Color::rgb(76, 175, 80),
            start_bottom: Color::rgb(39, 118, 43),
            start_border: Color::rgb(24, 82, 28),
            start_text: Color::WHITE,
            quick_bg: Color::rgb(36, 94, 219),
            tray_bg: Color::rgb(15, 61, 158),
            tray_text: Color::WHITE,
            task_button: Color::rgb(62, 121, 219),
            task_button_hover: Color::rgb(92, 152, 240),
            task_button_active: Color::rgb(25, 65, 165),
            task_border: Color::rgb(20, 54, 140),
            task_text: Color::WHITE,
            window_border_active: Color::rgb(0, 84, 227),
            window_border_inactive: Color::rgb(122, 150, 223),
            window_bg: Color::rgb(244, 241, 232),
            window_content: Color::rgb(255, 255, 255),
            title_active_top: Color::rgb(0, 88, 238),
            title_active_bottom: Color::rgb(63, 140, 243),
            title_inactive_top: Color::rgb(122, 150, 223),
            title_inactive_bottom: Color::rgb(180, 200, 240),
            title_text: Color::WHITE,
            title_text_inactive: Color::rgb(230, 236, 250),
            menu_bg: Color::WHITE,
            menu_border: Color::rgb(0, 84, 227),
            menu_header_top: Color::rgb(0, 88, 238),
            menu_header_bottom: Color::rgb(63, 140, 243),
            menu_header_text: Color::WHITE,
            menu_text: Color::rgb(24, 24, 24),
            menu_muted: Color::rgb(96, 96, 96),
            menu_hover: Color::rgb(49, 106, 197),
            menu_hover_text: Color::WHITE,
            menu_separator: Color::rgb(200, 208, 224),
            desktop_text: Color::WHITE,
            desktop_text_hover: Color::rgb(255, 240, 160),
            close_button: Color::rgb(216, 62, 48),
            caption_button: Color::rgb(0, 84, 227),
            caption_glyph: Color::WHITE,
        }
    }

    /// Windows XP Luna olive green.
    pub const fn xp_olive() -> Self {
        let mut theme = Self::xp_blue();
        theme.name = ThemeName::XpOlive;
        theme.wallpaper_top = Color::rgb(142, 160, 92);
        theme.wallpaper_bottom = Color::rgb(86, 105, 47);
        theme.wallpaper_grid = Color::rgb(160, 176, 112);
        theme.taskbar_top = Color::rgb(142, 160, 92);
        theme.taskbar_bottom = Color::rgb(74, 92, 40);
        theme.taskbar_highlight = Color::rgb(180, 194, 126);
        theme.start_top = Color::rgb(122, 158, 62);
        theme.start_bottom = Color::rgb(67, 102, 30);
        theme.start_border = Color::rgb(45, 72, 20);
        theme.quick_bg = Color::rgb(86, 105, 47);
        theme.tray_bg = Color::rgb(64, 82, 34);
        theme.task_button = Color::rgb(122, 142, 74);
        theme.task_button_hover = Color::rgb(150, 170, 96);
        theme.task_button_active = Color::rgb(64, 82, 34);
        theme.task_border = Color::rgb(52, 68, 26);
        theme.window_border_active = Color::rgb(96, 120, 48);
        theme.window_border_inactive = Color::rgb(156, 168, 122);
        theme.title_active_top = Color::rgb(110, 136, 56);
        theme.title_active_bottom = Color::rgb(160, 180, 104);
        theme.title_inactive_top = Color::rgb(166, 176, 138);
        theme.title_inactive_bottom = Color::rgb(202, 210, 178);
        theme.menu_border = Color::rgb(96, 120, 48);
        theme.menu_header_top = Color::rgb(110, 136, 56);
        theme.menu_header_bottom = Color::rgb(160, 180, 104);
        theme.menu_hover = Color::rgb(106, 132, 54);
        theme.close_button = Color::rgb(192, 64, 48);
        theme.caption_button = Color::rgb(96, 120, 48);
        theme.ui.palette.accent = Color::rgb(96, 120, 48);
        theme.ui.palette.accent_hover = Color::rgb(122, 148, 70);
        theme.ui.palette.border_strong = Color::rgb(96, 120, 48);
        theme.ui.palette.focus = Color::rgb(96, 120, 48);
        theme
    }

    /// Windows XP Luna silver.
    pub const fn xp_silver() -> Self {
        let mut theme = Self::xp_blue();
        theme.name = ThemeName::XpSilver;
        theme.wallpaper_top = Color::rgb(190, 196, 208);
        theme.wallpaper_bottom = Color::rgb(120, 130, 148);
        theme.wallpaper_grid = Color::rgb(206, 212, 224);
        theme.taskbar_top = Color::rgb(190, 196, 208);
        theme.taskbar_bottom = Color::rgb(112, 122, 140);
        theme.taskbar_highlight = Color::rgb(226, 230, 238);
        theme.taskbar_text = Color::rgb(20, 24, 32);
        theme.start_top = Color::rgb(150, 158, 172);
        theme.start_bottom = Color::rgb(92, 102, 120);
        theme.start_border = Color::rgb(64, 72, 88);
        theme.quick_bg = Color::rgb(150, 158, 172);
        theme.tray_bg = Color::rgb(112, 122, 140);
        theme.tray_text = Color::rgb(20, 24, 32);
        theme.task_button = Color::rgb(164, 172, 186);
        theme.task_button_hover = Color::rgb(196, 202, 214);
        theme.task_button_active = Color::rgb(112, 122, 140);
        theme.task_border = Color::rgb(92, 102, 120);
        theme.task_text = Color::rgb(20, 24, 32);
        theme.window_border_active = Color::rgb(96, 106, 124);
        theme.window_border_inactive = Color::rgb(166, 174, 188);
        theme.title_active_top = Color::rgb(110, 120, 140);
        theme.title_active_bottom = Color::rgb(178, 186, 200);
        theme.title_inactive_top = Color::rgb(190, 196, 208);
        theme.title_inactive_bottom = Color::rgb(214, 218, 226);
        theme.title_text = Color::WHITE;
        theme.title_text_inactive = Color::rgb(50, 56, 68);
        theme.menu_border = Color::rgb(96, 106, 124);
        theme.menu_header_top = Color::rgb(110, 120, 140);
        theme.menu_header_bottom = Color::rgb(178, 186, 200);
        theme.menu_hover = Color::rgb(96, 106, 124);
        theme.close_button = Color::rgb(192, 64, 48);
        theme.caption_button = Color::rgb(96, 106, 124);
        theme.ui.palette.accent = Color::rgb(96, 106, 124);
        theme.ui.palette.accent_hover = Color::rgb(126, 136, 154);
        theme.ui.palette.border_strong = Color::rgb(96, 106, 124);
        theme.ui.palette.focus = Color::rgb(96, 106, 124);
        theme
    }

    /// A modern dark desktop.
    pub const fn dark() -> Self {
        let mut palette = Palette::dark();
        palette.surface = Color::rgb(19, 24, 39);
        palette.surface_alt = Color::rgb(28, 35, 55);
        palette.accent = Color::rgb(64, 156, 255);
        let ui = Theme {
            palette,
            metrics: Metrics::compact(),
        };

        Self {
            name: ThemeName::Dark,
            ui,
            wallpaper_top: Color::rgb(12, 18, 34),
            wallpaper_bottom: Color::rgb(4, 7, 16),
            wallpaper_grid: Color::rgb(24, 34, 56),
            taskbar_top: Color::rgb(28, 36, 58),
            taskbar_bottom: Color::rgb(10, 14, 26),
            taskbar_highlight: Color::rgb(58, 74, 112),
            taskbar_text: Color::rgb(226, 234, 250),
            start_top: Color::rgb(44, 132, 88),
            start_bottom: Color::rgb(22, 84, 54),
            start_border: Color::rgb(16, 58, 38),
            start_text: Color::WHITE,
            quick_bg: Color::rgb(18, 24, 40),
            tray_bg: Color::rgb(12, 17, 30),
            tray_text: Color::rgb(200, 214, 238),
            task_button: Color::rgb(34, 44, 70),
            task_button_hover: Color::rgb(48, 62, 96),
            task_button_active: Color::rgb(26, 34, 54),
            task_border: Color::rgb(52, 66, 98),
            task_text: Color::rgb(226, 234, 250),
            window_border_active: Color::rgb(64, 156, 255),
            window_border_inactive: Color::rgb(52, 64, 92),
            window_bg: Color::rgb(19, 24, 39),
            window_content: Color::rgb(24, 30, 48),
            title_active_top: Color::rgb(38, 76, 138),
            title_active_bottom: Color::rgb(24, 48, 92),
            title_inactive_top: Color::rgb(38, 46, 66),
            title_inactive_bottom: Color::rgb(28, 34, 50),
            title_text: Color::rgb(240, 246, 255),
            title_text_inactive: Color::rgb(150, 164, 194),
            menu_bg: Color::rgb(22, 28, 45),
            menu_border: Color::rgb(64, 84, 124),
            menu_header_top: Color::rgb(38, 76, 138),
            menu_header_bottom: Color::rgb(24, 48, 92),
            menu_header_text: Color::rgb(240, 246, 255),
            menu_text: Color::rgb(226, 234, 250),
            menu_muted: Color::rgb(148, 162, 194),
            menu_hover: Color::rgb(52, 92, 160),
            menu_hover_text: Color::WHITE,
            menu_separator: Color::rgb(44, 56, 84),
            desktop_text: Color::rgb(226, 234, 250),
            desktop_text_hover: Color::rgb(120, 190, 255),
            close_button: Color::rgb(190, 58, 70),
            caption_button: Color::rgb(38, 76, 138),
            caption_glyph: Color::WHITE,
        }
    }

    /// A modern light desktop.
    pub const fn light() -> Self {
        let mut theme = Self::dark();
        theme.name = ThemeName::Light;
        theme.ui = Theme::light().with_metrics(Metrics::compact());
        theme.wallpaper_top = Color::rgb(214, 226, 246);
        theme.wallpaper_bottom = Color::rgb(150, 178, 220);
        theme.wallpaper_grid = Color::rgb(190, 208, 238);
        theme.taskbar_top = Color::rgb(244, 247, 252);
        theme.taskbar_bottom = Color::rgb(206, 218, 236);
        theme.taskbar_highlight = Color::WHITE;
        theme.taskbar_text = Color::rgb(24, 32, 48);
        theme.start_top = Color::rgb(76, 175, 80);
        theme.start_bottom = Color::rgb(39, 118, 43);
        theme.quick_bg = Color::rgb(222, 232, 246);
        theme.tray_bg = Color::rgb(206, 218, 236);
        theme.tray_text = Color::rgb(24, 32, 48);
        theme.task_button = Color::rgb(232, 239, 250);
        theme.task_button_hover = Color::rgb(214, 228, 248);
        theme.task_button_active = Color::rgb(190, 210, 240);
        theme.task_border = Color::rgb(160, 178, 206);
        theme.task_text = Color::rgb(24, 32, 48);
        theme.window_border_active = Color::rgb(24, 112, 214);
        theme.window_border_inactive = Color::rgb(176, 190, 212);
        theme.window_bg = Color::rgb(248, 250, 254);
        theme.window_content = Color::WHITE;
        theme.title_active_top = Color::rgb(24, 112, 214);
        theme.title_active_bottom = Color::rgb(72, 152, 232);
        theme.title_inactive_top = Color::rgb(196, 206, 222);
        theme.title_inactive_bottom = Color::rgb(222, 228, 238);
        theme.title_text = Color::WHITE;
        theme.title_text_inactive = Color::rgb(48, 58, 78);
        theme.menu_bg = Color::WHITE;
        theme.menu_border = Color::rgb(160, 178, 206);
        theme.menu_header_top = Color::rgb(24, 112, 214);
        theme.menu_header_bottom = Color::rgb(72, 152, 232);
        theme.menu_text = Color::rgb(24, 32, 48);
        theme.menu_muted = Color::rgb(92, 106, 132);
        theme.menu_hover = Color::rgb(24, 112, 214);
        theme.menu_hover_text = Color::WHITE;
        theme.menu_separator = Color::rgb(214, 222, 236);
        theme.desktop_text = Color::rgb(24, 32, 48);
        theme.desktop_text_hover = Color::rgb(0, 84, 227);
        theme.caption_button = Color::rgb(24, 112, 214);
        theme
    }

    /// A high-contrast desktop.
    pub const fn high_contrast() -> Self {
        let mut theme = Self::dark();
        theme.name = ThemeName::HighContrast;
        theme.ui = Theme::high_contrast().with_metrics(Metrics::compact());
        theme.wallpaper_top = Color::BLACK;
        theme.wallpaper_bottom = Color::BLACK;
        theme.wallpaper_grid = Color::rgb(40, 40, 40);
        theme.taskbar_top = Color::BLACK;
        theme.taskbar_bottom = Color::BLACK;
        theme.taskbar_highlight = Color::rgb(80, 80, 80);
        theme.taskbar_text = Color::WHITE;
        theme.start_top = Color::rgb(255, 220, 0);
        theme.start_bottom = Color::rgb(200, 170, 0);
        theme.start_border = Color::WHITE;
        theme.start_text = Color::BLACK;
        theme.quick_bg = Color::BLACK;
        theme.tray_bg = Color::BLACK;
        theme.tray_text = Color::WHITE;
        theme.task_button = Color::rgb(20, 20, 20);
        theme.task_button_hover = Color::rgb(60, 60, 60);
        theme.task_button_active = Color::rgb(90, 90, 90);
        theme.task_border = Color::WHITE;
        theme.task_text = Color::WHITE;
        theme.window_border_active = Color::YELLOW;
        theme.window_border_inactive = Color::WHITE;
        theme.window_bg = Color::BLACK;
        theme.window_content = Color::BLACK;
        theme.title_active_top = Color::BLACK;
        theme.title_active_bottom = Color::BLACK;
        theme.title_inactive_top = Color::rgb(30, 30, 30);
        theme.title_inactive_bottom = Color::rgb(30, 30, 30);
        theme.title_text = Color::YELLOW;
        theme.title_text_inactive = Color::WHITE;
        theme.menu_bg = Color::BLACK;
        theme.menu_border = Color::WHITE;
        theme.menu_header_top = Color::BLACK;
        theme.menu_header_bottom = Color::BLACK;
        theme.menu_header_text = Color::YELLOW;
        theme.menu_text = Color::WHITE;
        theme.menu_muted = Color::LIGHT_GRAY;
        theme.menu_hover = Color::YELLOW;
        theme.menu_hover_text = Color::BLACK;
        theme.menu_separator = Color::WHITE;
        theme.desktop_text = Color::WHITE;
        theme.desktop_text_hover = Color::YELLOW;
        theme.close_button = Color::RED;
        theme.caption_button = Color::BLACK;
        theme.caption_glyph = Color::WHITE;
        theme
    }
}

impl Default for DesktopTheme {
    fn default() -> Self {
        Self::xp_blue()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_names_parse_and_round_trip() {
        for name in [
            ThemeName::XpBlue,
            ThemeName::XpOlive,
            ThemeName::XpSilver,
            ThemeName::Dark,
            ThemeName::Light,
            ThemeName::HighContrast,
        ] {
            assert_eq!(ThemeName::parse(name.name()), Some(name));
        }
        assert_eq!(ThemeName::parse("nope"), None);
    }

    #[test]
    fn themes_are_distinct() {
        assert_ne!(
            DesktopTheme::xp_blue().taskbar_top,
            DesktopTheme::xp_olive().taskbar_top
        );
        assert_ne!(
            DesktopTheme::dark().window_bg,
            DesktopTheme::light().window_bg
        );
    }

    #[test]
    fn config_overrides_wallpaper() {
        let config = DesktopConfig::parse(concat!(
            "theme: xp-blue\n",
            "wallpaper:\n",
            "  top: \"#010203\"\n",
            "  bottom: \"#040506\"\n",
        ))
        .unwrap();
        let theme = DesktopTheme::from_config(&config);
        assert_eq!(theme.wallpaper_top, Color::rgb(1, 2, 3));
        assert_eq!(theme.wallpaper_bottom, Color::rgb(4, 5, 6));
    }
}
