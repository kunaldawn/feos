//! YAML configuration for the desktop.

use feos_gfx::Color;
use feos_init::Config as InitConfig;
use feos_text::Text;
use feos_yaml::{YamlDocument, YamlNode};

use crate::keyboard::KeyboardLayout;
use crate::theme::ThemeName;

/// Maximum application directories read from the configuration.
pub const MAX_APP_DIRS: usize = 4;

/// Maximum bundle extensions read from the configuration.
pub const MAX_EXTENSIONS: usize = 4;

/// How the wallpaper is painted.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum WallpaperStyle {
    /// Vertical gradient from `top` to `bottom`.
    #[default]
    Gradient,
    /// A single solid colour.
    Solid,
    /// Gradient plus a faint grid.
    Grid,
}

impl WallpaperStyle {
    /// Parses `gradient`, `solid` or `grid`.
    pub fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("gradient") {
            Some(Self::Gradient)
        } else if value.eq_ignore_ascii_case("solid") {
            Some(Self::Solid)
        } else if value.eq_ignore_ascii_case("grid") {
            Some(Self::Grid)
        } else {
            None
        }
    }
}

/// Wallpaper colours and style.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WallpaperConfig {
    /// Painting style.
    pub style: WallpaperStyle,
    /// Top colour (or the only colour for `solid`).
    pub top: Color,
    /// Bottom colour for gradients.
    pub bottom: Color,
    /// Grid line colour for `grid`.
    pub grid: Color,
}

impl WallpaperConfig {
    /// The classic XP blue wallpaper.
    pub const fn xp_blue() -> Self {
        Self {
            style: WallpaperStyle::Gradient,
            top: Color::rgb(63, 140, 243),
            bottom: Color::rgb(36, 94, 219),
            grid: Color::rgb(84, 156, 246),
        }
    }

    /// A dark wallpaper used by the dark themes.
    pub const fn dark() -> Self {
        Self {
            style: WallpaperStyle::Grid,
            top: Color::rgb(12, 18, 34),
            bottom: Color::rgb(4, 7, 16),
            grid: Color::rgb(24, 34, 56),
        }
    }
}

impl Default for WallpaperConfig {
    fn default() -> Self {
        Self::xp_blue()
    }
}

/// Where the taskbar sits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TaskbarPosition {
    /// Along the bottom edge.
    #[default]
    Bottom,
    /// Along the top edge.
    Top,
}

impl TaskbarPosition {
    /// Parses `bottom` or `top`.
    pub fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("bottom") {
            Some(Self::Bottom)
        } else if value.eq_ignore_ascii_case("top") {
            Some(Self::Top)
        } else {
            None
        }
    }
}

/// Taskbar appearance and features.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TaskbarConfig {
    /// Edge the bar is attached to.
    pub position: TaskbarPosition,
    /// Bar height in pixels.
    pub height: u32,
    /// Text on the Start button.
    pub start_label: Text<16>,
    /// Show the clock in the notification area.
    pub show_clock: bool,
    /// Use 24-hour time.
    pub clock_24h: bool,
    /// Show the quick-launch strip.
    pub show_quick_launch: bool,
    /// Number of catalog applications pinned to quick launch.
    pub quick_launch_count: u8,
    /// Show the "show desktop" button.
    pub show_desktop: bool,
}

impl TaskbarConfig {
    /// Default classic-style taskbar settings.
    pub const fn new() -> Self {
        Self {
            position: TaskbarPosition::Bottom,
            height: 40,
            start_label: Text::EMPTY,
            show_clock: true,
            clock_24h: true,
            show_quick_launch: true,
            quick_launch_count: 3,
            show_desktop: true,
        }
    }
}

impl Default for TaskbarConfig {
    fn default() -> Self {
        let mut config = Self::new();
        config.start_label.set("start");
        config
    }
}

/// Start-menu dimensions and content toggles.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StartMenuConfig {
    /// Panel width in pixels.
    pub width: u32,
    /// Panel height in pixels.
    pub height: u32,
    /// Number of application columns (currently one or two).
    pub columns: u8,
    /// Show the one-line description under each app name.
    pub show_descriptions: bool,
    /// Open the menu on the first frame.
    pub initially_open: bool,
}

impl StartMenuConfig {
    /// Default classic-style start menu.
    pub const fn new() -> Self {
        Self {
            width: 448,
            height: 470,
            columns: 2,
            show_descriptions: true,
            initially_open: false,
        }
    }
}

impl Default for StartMenuConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Desktop shortcut appearance.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShortcutConfig {
    /// Show application shortcuts on the wallpaper.
    pub show_icons: bool,
    /// Icon edge length in pixels.
    pub icon_size: u32,
    /// Draw the application name under each icon.
    pub show_labels: bool,
}

impl ShortcutConfig {
    /// Default classic-style desktop icons.
    pub const fn new() -> Self {
        Self {
            show_icons: true,
            icon_size: 48,
            show_labels: true,
        }
    }
}

impl Default for ShortcutConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Default window sizes and cascade spacing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WindowConfig {
    /// Initial width of a new window.
    pub default_width: u32,
    /// Initial height of a new window.
    pub default_height: u32,
    /// Offset between cascaded windows.
    pub cascade: i32,
}

impl WindowConfig {
    /// Default window settings.
    pub const fn new() -> Self {
        Self {
            default_width: 560,
            default_height: 380,
            cascade: 26,
        }
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// The complete desktop configuration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DesktopConfig {
    /// Visual theme.
    pub theme: ThemeName,
    /// Wallpaper.
    pub wallpaper: WallpaperConfig,
    /// Taskbar.
    pub taskbar: TaskbarConfig,
    /// Start menu.
    pub start_menu: StartMenuConfig,
    /// Desktop shortcuts.
    pub icons: ShortcutConfig,
    /// Window defaults.
    pub windows: WindowConfig,
    /// Directories scanned for bundles.
    pub app_dirs: [Text<32>; MAX_APP_DIRS],
    /// Number of valid entries in [`app_dirs`](DesktopConfig::app_dirs).
    pub app_dir_count: usize,
    /// Bundle extensions preferred by the scanner.
    pub extensions: [Text<8>; MAX_EXTENSIONS],
    /// Number of valid entries in [`extensions`](DesktopConfig::extensions).
    pub extension_count: usize,
    /// Init/service manager settings.
    pub init: InitConfig,
    /// Layout applied to the virtio keyboard.
    pub keyboard_layout: KeyboardLayout,
}

impl DesktopConfig {
    /// Default configuration for the bundled desktop session.
    pub const fn new() -> Self {
        Self {
            theme: ThemeName::XpBlue,
            wallpaper: WallpaperConfig::xp_blue(),
            taskbar: TaskbarConfig::new(),
            start_menu: StartMenuConfig::new(),
            icons: ShortcutConfig::new(),
            windows: WindowConfig::new(),
            app_dirs: [Text::EMPTY; MAX_APP_DIRS],
            app_dir_count: 0,
            extensions: [Text::EMPTY; MAX_EXTENSIONS],
            extension_count: 0,
            init: InitConfig::new(),
            keyboard_layout: KeyboardLayout::Us,
        }
    }

    /// Parses a desktop configuration from YAML.
    ///
    /// Unknown keys and invalid values are ignored and keep their defaults;
    /// malformed YAML is reported. This makes the desktop resilient to a
    /// configuration written for a newer build.
    pub fn parse(yaml: &str) -> Result<Self, ConfigError> {
        let mut nodes = [YamlNode::EMPTY; 160];
        let document = YamlDocument::parse(yaml, &mut nodes)?;
        let root = document.root();
        if !document.is_mapping(root) {
            return Err(ConfigError::NotMapping);
        }

        let mut config = Self::new();

        if let Some(keyboard) = document.get(root, "keyboard") {
            if let Some(layout) = read_text_value(&document, keyboard, "layout")
                .and_then(|value| KeyboardLayout::parse(&value))
            {
                config.keyboard_layout = layout;
            }
        }

        if let Some(value) = read_text_value(&document, root, "theme") {
            if let Some(theme) = ThemeName::parse(&value) {
                config.theme = theme;
                if theme.is_dark() {
                    config.wallpaper = WallpaperConfig::dark();
                }
            }
        }

        if let Some(wallpaper) = document.get(root, "wallpaper") {
            if let Some(style) = read_text_value(&document, wallpaper, "style")
                .and_then(|value| WallpaperStyle::parse(&value))
            {
                config.wallpaper.style = style;
            }
            if let Some(color) = read_color(&document, wallpaper, "color") {
                config.wallpaper.top = color;
                config.wallpaper.bottom = color;
            }
            if let Some(color) = read_color(&document, wallpaper, "top") {
                config.wallpaper.top = color;
            }
            if let Some(color) = read_color(&document, wallpaper, "bottom") {
                config.wallpaper.bottom = color;
            }
            if let Some(color) = read_color(&document, wallpaper, "accent") {
                config.wallpaper.grid = color;
            }
            if let Some(color) = read_color(&document, wallpaper, "grid") {
                config.wallpaper.grid = color;
            }
        }

        if let Some(taskbar) = document.get(root, "taskbar") {
            if let Some(position) = read_text_value(&document, taskbar, "position")
                .and_then(|value| TaskbarPosition::parse(&value))
            {
                config.taskbar.position = position;
            }
            if let Some(value) = read_u32(&document, taskbar, "height") {
                config.taskbar.height = value.clamp(28, 96);
            }
            if let Some(value) = read_text_value(&document, taskbar, "start_label") {
                config.taskbar.start_label.set(&value);
            }
            if let Some(value) = read_bool(&document, taskbar, "show_clock") {
                config.taskbar.show_clock = value;
            }
            if let Some(value) = read_bool(&document, taskbar, "clock_24h") {
                config.taskbar.clock_24h = value;
            }
            if let Some(value) = read_bool(&document, taskbar, "show_quick_launch") {
                config.taskbar.show_quick_launch = value;
            }
            if let Some(value) = read_u32(&document, taskbar, "quick_launch_count") {
                config.taskbar.quick_launch_count = value.min(6) as u8;
            }
            if let Some(value) = read_bool(&document, taskbar, "show_desktop") {
                config.taskbar.show_desktop = value;
            }
        }

        if let Some(menu) = document.get(root, "start_menu") {
            if let Some(value) = read_u32(&document, menu, "width") {
                config.start_menu.width = value.clamp(280, 720);
            }
            if let Some(value) = read_u32(&document, menu, "height") {
                config.start_menu.height = value.clamp(280, 700);
            }
            if let Some(value) = read_u32(&document, menu, "columns") {
                config.start_menu.columns = value.clamp(1, 2) as u8;
            }
            if let Some(value) = read_bool(&document, menu, "show_descriptions") {
                config.start_menu.show_descriptions = value;
            }
            if let Some(value) = read_bool(&document, menu, "initially_open") {
                config.start_menu.initially_open = value;
            }
        }

        if let Some(icons) = document.get(root, "desktop") {
            if let Some(value) = read_bool(&document, icons, "show_icons") {
                config.icons.show_icons = value;
            }
            if let Some(value) = read_u32(&document, icons, "icon_size") {
                config.icons.icon_size = value.clamp(24, 96);
            }
            if let Some(value) = read_bool(&document, icons, "show_labels") {
                config.icons.show_labels = value;
            }
        }

        if let Some(windows) = document.get(root, "windows") {
            if let Some(value) = read_u32(&document, windows, "default_width") {
                config.windows.default_width = value.clamp(240, 2048);
            }
            if let Some(value) = read_u32(&document, windows, "default_height") {
                config.windows.default_height = value.clamp(160, 2048);
            }
            if let Some(value) = read_u32(&document, windows, "cascade") {
                config.windows.cascade = value.min(96) as i32;
            }
        }

        if let Some(apps) = document.get(root, "apps") {
            if let Some(directories) = document.get(apps, "directories") {
                for node in document.sequence_iter(directories).into_iter().flatten() {
                    if config.app_dir_count == MAX_APP_DIRS {
                        break;
                    }
                    let mut buffer = [0u8; 64];
                    if let Some(value) = document.as_str(node, &mut buffer) {
                        let value = value.trim();
                        if !value.is_empty() {
                            config.app_dirs[config.app_dir_count].set(value);
                            config.app_dir_count += 1;
                        }
                    }
                }
            }
            if let Some(extensions) = document.get(apps, "extensions") {
                for node in document.sequence_iter(extensions).into_iter().flatten() {
                    if config.extension_count == MAX_EXTENSIONS {
                        break;
                    }
                    let mut buffer = [0u8; 16];
                    if let Some(value) = document.as_str(node, &mut buffer) {
                        let value = value.trim().trim_start_matches('.');
                        if !value.is_empty() {
                            config.extensions[config.extension_count].set(value);
                            config.extension_count += 1;
                        }
                    }
                }
            }
        }

        if config.app_dir_count == 0 {
            config.app_dirs[0].set("apps");
            config.app_dir_count = 1;
        }
        if config.extension_count == 0 {
            config.extensions[0].set("fapp");
            config.extensions[1].set("app");
            config.extension_count = 2;
        }

        config.init = InitConfig::parse(yaml).unwrap_or_default();
        Ok(config)
    }

    /// The configured application directories.
    pub fn app_dirs(&self) -> &[Text<32>] {
        &self.app_dirs[..self.app_dir_count]
    }

    /// The configured bundle extensions.
    pub fn extensions(&self) -> &[Text<8>] {
        &self.extensions[..self.extension_count]
    }
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors returned while parsing a desktop configuration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigError {
    /// The YAML itself is malformed.
    Yaml(feos_yaml::YamlError),
    /// The document root is not a mapping.
    NotMapping,
}

impl From<feos_yaml::YamlError> for ConfigError {
    fn from(error: feos_yaml::YamlError) -> Self {
        Self::Yaml(error)
    }
}

fn read_text_value(
    document: &YamlDocument<'_, '_>,
    node: feos_yaml::NodeId,
    key: &str,
) -> Option<Text<64>> {
    let value = document.get(node, key)?;
    let mut buffer = [0u8; 64];
    let text = document.as_str(value, &mut buffer)?;
    let mut out = Text::new();
    out.set(text);
    Some(out)
}

fn read_u32(document: &YamlDocument<'_, '_>, node: feos_yaml::NodeId, key: &str) -> Option<u32> {
    document
        .get(node, key)
        .and_then(|value| document.as_u64(value))
        .map(|value| value.min(u32::MAX as u64) as u32)
}

fn read_bool(document: &YamlDocument<'_, '_>, node: feos_yaml::NodeId, key: &str) -> Option<bool> {
    document
        .get(node, key)
        .and_then(|value| document.as_bool(value))
}

fn read_color(
    document: &YamlDocument<'_, '_>,
    node: feos_yaml::NodeId,
    key: &str,
) -> Option<Color> {
    let value = document.get(node, key)?;
    let mut buffer = [0u8; 32];
    let text = document.as_str(value, &mut buffer)?;
    parse_color(text)
}

/// Parses `#rrggbb`, `#rgb`, or a small set of colour names.
pub fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        return match hex.len() {
            6 => Some(Color::from_u32(u32::from_str_radix(hex, 16).ok()?)),
            3 => {
                let mut digits = [0u8; 6];
                for (index, byte) in hex.bytes().enumerate() {
                    digits[index * 2] = byte;
                    digits[index * 2 + 1] = byte;
                }
                let text = core::str::from_utf8(&digits).ok()?;
                Some(Color::from_u32(u32::from_str_radix(text, 16).ok()?))
            }
            _ => None,
        };
    }

    let color = if value.eq_ignore_ascii_case("black") {
        Color::BLACK
    } else if value.eq_ignore_ascii_case("white") {
        Color::WHITE
    } else if value.eq_ignore_ascii_case("red") {
        Color::RED
    } else if value.eq_ignore_ascii_case("green") {
        Color::GREEN
    } else if value.eq_ignore_ascii_case("blue") {
        Color::BLUE
    } else if value.eq_ignore_ascii_case("yellow") {
        Color::YELLOW
    } else if value.eq_ignore_ascii_case("orange") {
        Color::ORANGE
    } else if value.eq_ignore_ascii_case("gray") || value.eq_ignore_ascii_case("grey") {
        Color::GRAY
    } else if value.eq_ignore_ascii_case("silver") {
        Color::LIGHT_GRAY
    } else if value.eq_ignore_ascii_case("navy") {
        Color::NAVY
    } else if value.eq_ignore_ascii_case("teal") {
        Color::TEAL
    } else if value.eq_ignore_ascii_case("purple") {
        Color::PURPLE
    } else {
        return None;
    };
    Some(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_desktop_schema() {
        let config = DesktopConfig::parse(concat!(
            "theme: xp-olive\n",
            "wallpaper:\n",
            "  style: grid\n",
            "  top: \"#102030\"\n",
            "  bottom: \"#405060\"\n",
            "  accent: \"#708090\"\n",
            "taskbar:\n",
            "  position: top\n",
            "  height: 48\n",
            "  start_label: menu\n",
            "  show_clock: false\n",
            "  clock_24h: false\n",
            "  show_quick_launch: false\n",
            "  quick_launch_count: 2\n",
            "start_menu:\n",
            "  width: 500\n",
            "  height: 520\n",
            "  columns: 1\n",
            "  show_descriptions: false\n",
            "  initially_open: true\n",
            "desktop:\n",
            "  show_icons: false\n",
            "  icon_size: 64\n",
            "  show_labels: false\n",
            "windows:\n",
            "  default_width: 700\n",
            "  default_height: 500\n",
            "  cascade: 12\n",
            "apps:\n",
            "  directories:\n",
            "    - system/apps\n",
            "    - user/apps\n",
            "  extensions: [.fapp, .app]\n",
            "init:\n",
            "  restart: always\n",
            "  max_restarts: 7\n",
            "  autostart: [dashboard, terminal]\n",
        ))
        .unwrap();

        assert_eq!(config.theme, ThemeName::XpOlive);
        assert_eq!(config.wallpaper.style, WallpaperStyle::Grid);
        assert_eq!(config.wallpaper.top, Color::rgb(0x10, 0x20, 0x30));
        assert_eq!(config.taskbar.position, TaskbarPosition::Top);
        assert_eq!(config.taskbar.height, 48);
        assert_eq!(config.taskbar.start_label.as_str(), "menu");
        assert!(!config.taskbar.show_clock);
        assert!(!config.taskbar.clock_24h);
        assert_eq!(config.start_menu.width, 500);
        assert_eq!(config.start_menu.columns, 1);
        assert!(config.start_menu.initially_open);
        assert!(!config.icons.show_icons);
        assert_eq!(config.icons.icon_size, 64);
        assert!(!config.icons.show_labels);
        assert_eq!(config.taskbar.quick_launch_count, 2);
        assert_eq!(config.windows.default_width, 700);
        assert_eq!(config.app_dirs()[1].as_str(), "user/apps");
        assert_eq!(config.extensions()[0].as_str(), "fapp");
        assert_eq!(config.init.max_restarts, 7);
        assert_eq!(config.init.autostart_len, 2);
    }

    #[test]
    fn defaults_are_sane() {
        let config = DesktopConfig::parse("theme: dark\n").unwrap();
        assert_eq!(config.theme, ThemeName::Dark);
        assert_eq!(config.taskbar.position, TaskbarPosition::Bottom);
        assert_eq!(config.app_dirs()[0].as_str(), "apps");
        assert_eq!(config.extensions()[0].as_str(), "fapp");
        assert_eq!(config.start_menu.width, 448);
    }

    #[test]
    fn colour_parser_handles_hex_and_names() {
        assert_eq!(parse_color("#ff0000"), Some(Color::RED));
        assert_eq!(parse_color("#f00"), Some(Color::RED));
        assert_eq!(parse_color("white"), Some(Color::WHITE));
        assert_eq!(parse_color("not-a-colour"), None);
    }

    #[test]
    fn rejects_non_mapping_roots() {
        assert_eq!(
            DesktopConfig::parse("- one\n- two\n"),
            Err(ConfigError::NotMapping)
        );
    }
}
