//! The toolkit's icons.
//!
//! Where `assets/icons` has a picture of the same thing, an [`Icon`] draws that
//! picture (see [`Icon::picture`]), in its own colours. The rest are small marks
//! (chevrons, arrows, check marks, plus and minus, close) and symbols the
//! iconset has nothing for; they are glyphs defined on an 8x8 grid, scaled to
//! the target rectangle and drawn in the colour asked for, so they follow the
//! same colour, clipping and focus rules as text.

use feos_gfx::{Color, Rect};

use feos_iconset::Stock;

use crate::geometry::RectExt;
use crate::math::{cos, sin};
use crate::painter::Painter;
use crate::response::{Response, Sense};
use crate::ui::Ui;

/// A built-in icon.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    ChevronUp,
    ChevronDown,
    ChevronLeft,
    ChevronRight,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Check,
    Close,
    Plus,
    Minus,
    Search,
    Menu,
    Settings,
    Info,
    Warning,
    Error,
    Success,
    Help,
    Home,
    User,
    Bell,
    Calendar,
    Clock,
    File,
    Folder,
    Trash,
    Edit,
    Refresh,
    Download,
    Upload,
    Play,
    Pause,
    Star,
    Heart,
    MoreHorizontal,
    MoreVertical,
    Lock,
    Unlock,
    Eye,
    EyeOff,
    Filter,
    Sort,
    Grid,
    List,
    Wifi,
    Sun,
    Moon,
    Copy,
    ExternalLink,
    Mail,
    MapPin,
    Globe,
    Code,
    Database,
    Cpu,
    Cloud,
    Bookmark,
    Tag,
    Flag,
    LogIn,
    LogOut,
    Undo,
    Redo,
    Shield,
    Sliders,
    Cart,
    CreditCard,
    Package,
    Terminal,
    Link,
}

impl Icon {
    /// Every icon, useful for galleries and icon pickers.
    pub const ALL: &'static [Icon] = &[
        Self::ChevronUp,
        Self::ChevronDown,
        Self::ChevronLeft,
        Self::ChevronRight,
        Self::ArrowUp,
        Self::ArrowDown,
        Self::ArrowLeft,
        Self::ArrowRight,
        Self::Check,
        Self::Close,
        Self::Plus,
        Self::Minus,
        Self::Search,
        Self::Menu,
        Self::Settings,
        Self::Info,
        Self::Warning,
        Self::Error,
        Self::Success,
        Self::Help,
        Self::Home,
        Self::User,
        Self::Bell,
        Self::Calendar,
        Self::Clock,
        Self::File,
        Self::Folder,
        Self::Trash,
        Self::Edit,
        Self::Refresh,
        Self::Download,
        Self::Upload,
        Self::Play,
        Self::Pause,
        Self::Star,
        Self::Heart,
        Self::MoreHorizontal,
        Self::MoreVertical,
        Self::Lock,
        Self::Unlock,
        Self::Eye,
        Self::EyeOff,
        Self::Filter,
        Self::Sort,
        Self::Grid,
        Self::List,
        Self::Wifi,
        Self::Sun,
        Self::Moon,
        Self::Copy,
        Self::ExternalLink,
        Self::Mail,
        Self::MapPin,
        Self::Globe,
        Self::Code,
        Self::Database,
        Self::Cpu,
        Self::Cloud,
        Self::Bookmark,
        Self::Tag,
        Self::Flag,
        Self::LogIn,
        Self::LogOut,
        Self::Undo,
        Self::Redo,
        Self::Shield,
        Self::Sliders,
        Self::Cart,
        Self::CreditCard,
        Self::Package,
        Self::Terminal,
        Self::Link,
    ];
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// Draws an icon in a square cell and returns its rectangle.
    pub fn icon(&mut self, icon: Icon, size: i32, color: Color) -> Rect {
        let size = size.max(8);
        let rect = self.allocate_sized(size, size);
        paint_icon(&mut self.painter, rect, icon, color);
        rect
    }

    /// A square icon button.
    pub fn icon_button(&mut self, icon: Icon, primary: bool) -> Response {
        let size = self.theme.metrics.control_height;
        self.icon_button_sized(size, icon, primary)
    }

    /// An icon button with an explicit square size.
    pub fn icon_button_sized(&mut self, size: i32, icon: Icon, primary: bool) -> Response {
        let rect = self.allocate_sized(size.max(8), size.max(8));
        self.icon_button_at(rect, icon, primary)
    }

    /// An icon button in a caller-provided rectangle.
    pub fn icon_button_at(&mut self, rect: Rect, icon: Icon, primary: bool) -> Response {
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::click());
        let palette = self.theme.palette;
        let metrics = self.theme.metrics;
        let (mut fill, color) = if primary {
            (palette.accent, palette.accent_text)
        } else {
            (palette.surface_alt, palette.text)
        };
        if response.pressed {
            fill = if primary {
                palette.accent.lerp(Color::BLACK, 50)
            } else {
                palette.surface_active
            };
        } else if response.hovered {
            fill = if primary {
                palette.accent_hover
            } else {
                palette.surface_hover
            };
        }
        self.painter.fill_rounded_rect(rect, metrics.radius, fill);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            if primary {
                fill.lerp(Color::WHITE, 24)
            } else {
                palette.border
            },
        );
        if response.focused {
            self.painter
                .stroke_rounded_rect(rect.inset(1), metrics.radius, 2, palette.focus);
        }
        let glyph = rect.inset((rect.width as i32 / 4).clamp(4, 12));
        paint_icon(&mut self.painter, glyph, icon, color);
        response.changed = false;
        response
    }
}

/// Draws `icon` into `rect` using `painter`.
///
/// This is public so custom widgets and app-specific components can reuse the
/// built-in icon set without going through [`Ui`].
pub fn paint_icon(painter: &mut Painter<'_, '_>, rect: Rect, icon: Icon, color: Color) {
    if rect.is_empty() {
        return;
    }
    let (x, y, u) = origin(rect);
    if let Some(stock) = icon.picture() {
        paint_stock(painter, stock, Rect::new(x, y, u as u32, u as u32));
        return;
    }
    let cx = x + u / 2;
    let cy = y + u / 2;

    match icon {
        Icon::ChevronUp => seg(painter, rect, 2, 5, 4, 3, color),
        Icon::ChevronDown => seg(painter, rect, 2, 3, 4, 5, color),
        Icon::ChevronLeft => seg(painter, rect, 5, 2, 3, 4, color),
        Icon::ChevronRight => seg(painter, rect, 3, 2, 5, 4, color),
        Icon::ArrowUp => {
            seg(painter, rect, 4, 7, 4, 1, color);
            seg(painter, rect, 2, 3, 4, 1, color);
            seg(painter, rect, 6, 3, 4, 1, color);
        }
        Icon::ArrowDown => {
            seg(painter, rect, 4, 1, 4, 7, color);
            seg(painter, rect, 2, 5, 4, 7, color);
            seg(painter, rect, 6, 5, 4, 7, color);
        }
        Icon::ArrowLeft => {
            seg(painter, rect, 7, 4, 1, 4, color);
            seg(painter, rect, 3, 2, 1, 4, color);
            seg(painter, rect, 3, 6, 1, 4, color);
        }
        Icon::ArrowRight => {
            seg(painter, rect, 1, 4, 7, 4, color);
            seg(painter, rect, 5, 2, 7, 4, color);
            seg(painter, rect, 5, 6, 7, 4, color);
        }
        Icon::Check => {
            seg(painter, rect, 1, 4, 3, 6, color);
            seg(painter, rect, 3, 6, 7, 2, color);
        }
        Icon::Close => {
            seg(painter, rect, 2, 2, 6, 6, color);
            seg(painter, rect, 6, 2, 2, 6, color);
        }
        Icon::Plus => {
            seg(painter, rect, 4, 1, 4, 7, color);
            seg(painter, rect, 1, 4, 7, 4, color);
        }
        Icon::Minus => seg(painter, rect, 1, 4, 7, 4, color),
        Icon::Search => {
            ring(painter, rect, 3, 3, 2, color);
            seg(painter, rect, 5, 5, 7, 7, color);
        }
        Icon::Menu => {
            seg(painter, rect, 1, 2, 7, 2, color);
            seg(painter, rect, 1, 4, 7, 4, color);
            seg(painter, rect, 1, 6, 7, 6, color);
        }
        Icon::Settings => {
            ring(painter, rect, 4, 4, 2, color);
            for (dx, dy) in [(0, -3), (0, 3), (-3, 0), (3, 0)] {
                seg(painter, rect, 4 + dx / 2, 4 + dy / 2, 4 + dx, 4 + dy, color);
            }
        }
        Icon::Info => {
            ring(painter, rect, 4, 4, 3, color);
            dot(painter, rect, 4, 2, color);
            seg(painter, rect, 4, 4, 4, 6, color);
        }
        Icon::Warning => {
            seg(painter, rect, 4, 1, 1, 7, color);
            seg(painter, rect, 1, 7, 7, 7, color);
            seg(painter, rect, 7, 7, 4, 1, color);
            seg(painter, rect, 4, 3, 4, 5, color);
            dot(painter, rect, 4, 6, color);
        }
        Icon::Error => {
            ring(painter, rect, 4, 4, 3, color);
            seg(painter, rect, 3, 3, 5, 5, color);
            seg(painter, rect, 5, 3, 3, 5, color);
        }
        Icon::Success => {
            ring(painter, rect, 4, 4, 3, color);
            seg(painter, rect, 2, 4, 3, 5, color);
            seg(painter, rect, 3, 5, 6, 2, color);
        }
        Icon::Help => {
            ring(painter, rect, 4, 4, 3, color);
            seg(painter, rect, 3, 3, 4, 2, color);
            seg(painter, rect, 4, 2, 5, 3, color);
            seg(painter, rect, 5, 3, 4, 4, color);
            seg(painter, rect, 4, 4, 4, 5, color);
            dot(painter, rect, 4, 6, color);
        }
        Icon::Home => {
            seg(painter, rect, 1, 4, 4, 1, color);
            seg(painter, rect, 4, 1, 7, 4, color);
            stroke(painter, rect, 2, 4, 6, 7, color);
            seg(painter, rect, 4, 5, 4, 7, color);
        }
        Icon::User => {
            ring(painter, rect, 4, 3, 1, color);
            seg(painter, rect, 2, 7, 2, 6, color);
            seg(painter, rect, 2, 6, 3, 5, color);
            seg(painter, rect, 3, 5, 5, 5, color);
            seg(painter, rect, 5, 5, 6, 6, color);
            seg(painter, rect, 6, 6, 6, 7, color);
        }
        Icon::Bell => {
            seg(painter, rect, 2, 5, 2, 3, color);
            seg(painter, rect, 2, 3, 3, 1, color);
            seg(painter, rect, 3, 1, 5, 1, color);
            seg(painter, rect, 5, 1, 6, 3, color);
            seg(painter, rect, 6, 3, 6, 5, color);
            seg(painter, rect, 1, 5, 7, 5, color);
            dot(painter, rect, 4, 6, color);
        }
        Icon::Calendar => {
            stroke(painter, rect, 1, 2, 7, 7, color);
            seg(painter, rect, 1, 4, 7, 4, color);
            seg(painter, rect, 2, 1, 2, 3, color);
            seg(painter, rect, 6, 1, 6, 3, color);
        }
        Icon::Clock => {
            ring(painter, rect, 4, 4, 3, color);
            seg(painter, rect, 4, 4, 4, 2, color);
            seg(painter, rect, 4, 4, 6, 4, color);
        }
        Icon::File => {
            stroke(painter, rect, 2, 1, 6, 7, color);
            seg(painter, rect, 4, 1, 6, 3, color);
            seg(painter, rect, 4, 1, 4, 3, color);
            seg(painter, rect, 3, 5, 5, 5, color);
        }
        Icon::Folder => {
            seg(painter, rect, 1, 2, 3, 2, color);
            seg(painter, rect, 3, 2, 4, 3, color);
            seg(painter, rect, 4, 3, 7, 3, color);
            stroke(painter, rect, 1, 2, 7, 7, color);
        }
        Icon::Trash => {
            seg(painter, rect, 2, 2, 6, 2, color);
            seg(painter, rect, 3, 1, 5, 1, color);
            stroke(painter, rect, 3, 3, 6, 7, color);
            seg(painter, rect, 4, 3, 4, 7, color);
        }
        Icon::Edit => {
            seg(painter, rect, 2, 6, 5, 3, color);
            seg(painter, rect, 5, 3, 6, 2, color);
            seg(painter, rect, 2, 6, 2, 7, color);
            seg(painter, rect, 2, 7, 3, 6, color);
        }
        Icon::Refresh => {
            seg(painter, rect, 3, 1, 5, 1, color);
            seg(painter, rect, 5, 1, 6, 2, color);
            seg(painter, rect, 6, 2, 7, 4, color);
            seg(painter, rect, 7, 4, 6, 6, color);
            seg(painter, rect, 6, 6, 5, 7, color);
            seg(painter, rect, 5, 7, 3, 7, color);
            seg(painter, rect, 3, 7, 2, 6, color);
            seg(painter, rect, 2, 6, 1, 4, color);
            seg(painter, rect, 2, 2, 2, 4, color);
            seg(painter, rect, 2, 2, 1, 1, color);
            seg(painter, rect, 2, 2, 3, 2, color);
        }
        Icon::Download => {
            seg(painter, rect, 4, 1, 4, 6, color);
            seg(painter, rect, 2, 4, 4, 6, color);
            seg(painter, rect, 6, 4, 4, 6, color);
            seg(painter, rect, 2, 7, 6, 7, color);
        }
        Icon::Upload => {
            seg(painter, rect, 4, 6, 4, 1, color);
            seg(painter, rect, 2, 3, 4, 1, color);
            seg(painter, rect, 6, 3, 4, 1, color);
            seg(painter, rect, 2, 7, 6, 7, color);
        }
        Icon::Play => {
            let (x0, y0, u) = origin(rect);
            let left = x0 + 2 * u / 8;
            let right = x0 + 6 * u / 8;
            let top = y0 + u / 8;
            let bottom = y0 + 7 * u / 8;
            let mid = (top + bottom) / 2;
            for py in top..bottom {
                let distance = (py - mid).abs();
                let half = (bottom - top) / 2;
                let end = right - (right - left) * distance / half.max(1);
                painter.fill_rect(Rect::new(left, py, (end - left).max(1) as u32, 1), color);
            }
        }
        Icon::Pause => {
            band(painter, rect, 2, 2, 3, 6, color);
            band(painter, rect, 5, 2, 6, 6, color);
        }
        Icon::Star => {
            let (x0, y0, u) = origin(rect);
            let c = (x0 + u / 2, y0 + u / 2);
            let mut points = [(0, 0); 10];
            for (i, point) in points.iter_mut().enumerate() {
                let angle = -core::f32::consts::FRAC_PI_2 + i as f32 * core::f32::consts::PI / 5.0;
                let radius = if i % 2 == 0 { 3.4 } else { 1.5 } * u as f32 / 8.0;
                *point = (
                    c.0 + (cos(angle) * radius) as i32,
                    c.1 + (sin(angle) * radius) as i32,
                );
            }
            for i in 0..10 {
                let a = points[i];
                let b = points[(i + 1) % 10];
                painter.line(a.0, a.1, b.0, b.1, color);
            }
        }
        Icon::Heart => {
            let r = (u / 4).max(2);
            painter.fill_circle(cx - r, cy - r / 2, r as u32, color);
            painter.fill_circle(cx + r, cy - r / 2, r as u32, color);
            for i in 0..=r * 2 {
                let width = (r * 2 - i).max(1);
                painter.fill_rect(
                    Rect::new(cx - width, cy + r / 2 + i, (width * 2) as u32, 1),
                    color,
                );
            }
        }
        Icon::MoreHorizontal => {
            for i in -1..=1 {
                dot(painter, rect, 4 + i * 2, 4, color);
            }
        }
        Icon::MoreVertical => {
            for i in -1..=1 {
                dot(painter, rect, 4, 4 + i * 2, color);
            }
        }
        Icon::Lock => {
            stroke(painter, rect, 2, 4, 6, 7, color);
            seg(painter, rect, 3, 4, 3, 2, color);
            seg(painter, rect, 3, 2, 5, 2, color);
            seg(painter, rect, 5, 2, 5, 4, color);
            seg(painter, rect, 4, 5, 4, 6, color);
        }
        Icon::Unlock => {
            stroke(painter, rect, 2, 4, 6, 7, color);
            seg(painter, rect, 3, 4, 3, 2, color);
            seg(painter, rect, 3, 2, 5, 2, color);
            seg(painter, rect, 4, 5, 4, 6, color);
        }
        Icon::Eye => {
            seg(painter, rect, 1, 4, 2, 3, color);
            seg(painter, rect, 2, 3, 4, 2, color);
            seg(painter, rect, 4, 2, 6, 3, color);
            seg(painter, rect, 6, 3, 7, 4, color);
            seg(painter, rect, 7, 4, 6, 5, color);
            seg(painter, rect, 6, 5, 4, 6, color);
            seg(painter, rect, 4, 6, 2, 5, color);
            seg(painter, rect, 2, 5, 1, 4, color);
            dot(painter, rect, 4, 4, color);
        }
        Icon::EyeOff => {
            seg(painter, rect, 1, 4, 2, 3, color);
            seg(painter, rect, 2, 3, 4, 2, color);
            seg(painter, rect, 4, 2, 6, 3, color);
            seg(painter, rect, 6, 3, 7, 4, color);
            seg(painter, rect, 2, 6, 6, 2, color);
        }
        Icon::Filter => {
            seg(painter, rect, 1, 2, 7, 2, color);
            seg(painter, rect, 2, 2, 4, 5, color);
            seg(painter, rect, 6, 2, 4, 5, color);
            seg(painter, rect, 4, 5, 4, 7, color);
        }
        Icon::Sort => {
            seg(painter, rect, 3, 7, 3, 1, color);
            seg(painter, rect, 1, 3, 3, 1, color);
            seg(painter, rect, 5, 1, 3, 1, color);
            seg(painter, rect, 5, 1, 5, 7, color);
            seg(painter, rect, 7, 5, 5, 7, color);
        }
        Icon::Grid => {
            for (gx, gy) in [(1, 1), (4, 1), (1, 4), (4, 4)] {
                stroke(painter, rect, gx, gy, gx + 2, gy + 2, color);
            }
        }
        Icon::List => {
            for gy in [2, 4, 6] {
                dot(painter, rect, 2, gy, color);
                seg(painter, rect, 4, gy, 7, gy, color);
            }
        }
        Icon::Wifi => {
            seg(painter, rect, 1, 4, 2, 2, color);
            seg(painter, rect, 2, 2, 6, 2, color);
            seg(painter, rect, 6, 2, 7, 4, color);
            seg(painter, rect, 2, 5, 3, 4, color);
            seg(painter, rect, 3, 4, 5, 4, color);
            seg(painter, rect, 5, 4, 6, 5, color);
            dot(painter, rect, 4, 7, color);
        }
        Icon::Sun => {
            ring(painter, rect, 4, 4, 2, color);
            for (dx, dy) in [
                (0, -3),
                (0, 3),
                (-3, 0),
                (3, 0),
                (-2, -2),
                (2, -2),
                (-2, 2),
                (2, 2),
            ] {
                seg(painter, rect, 4 + dx, 4 + dy, 4 + dx, 4 + dy, color);
            }
        }
        Icon::Moon => {
            seg(painter, rect, 5, 1, 3, 2, color);
            seg(painter, rect, 3, 2, 2, 4, color);
            seg(painter, rect, 2, 4, 3, 6, color);
            seg(painter, rect, 3, 6, 5, 7, color);
            seg(painter, rect, 5, 7, 6, 6, color);
            seg(painter, rect, 6, 2, 6, 6, color);
        }
        Icon::Copy => {
            stroke(painter, rect, 1, 1, 5, 5, color);
            stroke(painter, rect, 3, 3, 7, 7, color);
        }
        Icon::ExternalLink => {
            seg(painter, rect, 4, 1, 7, 1, color);
            seg(painter, rect, 7, 1, 7, 4, color);
            seg(painter, rect, 7, 1, 3, 5, color);
            seg(painter, rect, 1, 3, 1, 7, color);
            seg(painter, rect, 1, 7, 5, 7, color);
            seg(painter, rect, 5, 7, 5, 5, color);
        }
        Icon::Mail => {
            stroke(painter, rect, 1, 2, 7, 6, color);
            seg(painter, rect, 1, 2, 4, 4, color);
            seg(painter, rect, 4, 4, 7, 2, color);
        }
        Icon::MapPin => {
            ring(painter, rect, 4, 3, 2, color);
            seg(painter, rect, 2, 4, 4, 7, color);
            seg(painter, rect, 6, 4, 4, 7, color);
            dot(painter, rect, 4, 3, color);
        }
        Icon::Globe => {
            ring(painter, rect, 4, 4, 3, color);
            seg(painter, rect, 1, 4, 7, 4, color);
            seg(painter, rect, 4, 1, 4, 7, color);
            seg(painter, rect, 2, 2, 6, 6, color);
        }
        Icon::Code => {
            seg(painter, rect, 3, 2, 1, 4, color);
            seg(painter, rect, 1, 4, 3, 6, color);
            seg(painter, rect, 5, 2, 7, 4, color);
            seg(painter, rect, 7, 4, 5, 6, color);
        }
        Icon::Database => {
            ring(painter, rect, 4, 2, 3, color);
            seg(painter, rect, 1, 2, 1, 6, color);
            seg(painter, rect, 7, 2, 7, 6, color);
            seg(painter, rect, 1, 6, 4, 7, color);
            seg(painter, rect, 4, 7, 7, 6, color);
            seg(painter, rect, 1, 4, 4, 5, color);
            seg(painter, rect, 4, 5, 7, 4, color);
        }
        Icon::Cpu => {
            stroke(painter, rect, 2, 2, 6, 6, color);
            stroke(painter, rect, 3, 3, 5, 5, color);
            for i in [2, 4, 6] {
                seg(painter, rect, i, 1, i, 2, color);
                seg(painter, rect, i, 6, i, 7, color);
                seg(painter, rect, 1, i, 2, i, color);
                seg(painter, rect, 6, i, 7, i, color);
            }
        }
        Icon::Cloud => {
            ring(painter, rect, 3, 5, 1, color);
            ring(painter, rect, 5, 5, 1, color);
            seg(painter, rect, 2, 5, 2, 4, color);
            seg(painter, rect, 2, 4, 3, 3, color);
            seg(painter, rect, 3, 3, 5, 3, color);
            seg(painter, rect, 5, 3, 6, 4, color);
            seg(painter, rect, 6, 4, 7, 5, color);
            seg(painter, rect, 2, 6, 6, 6, color);
        }
        Icon::Bookmark => {
            seg(painter, rect, 2, 1, 6, 1, color);
            seg(painter, rect, 2, 1, 2, 7, color);
            seg(painter, rect, 6, 1, 6, 7, color);
            seg(painter, rect, 2, 7, 4, 5, color);
            seg(painter, rect, 4, 5, 6, 7, color);
        }
        Icon::Tag => {
            seg(painter, rect, 1, 4, 4, 1, color);
            seg(painter, rect, 4, 1, 7, 4, color);
            seg(painter, rect, 7, 4, 4, 7, color);
            seg(painter, rect, 4, 7, 1, 4, color);
            dot(painter, rect, 4, 4, color);
        }
        Icon::Flag => {
            seg(painter, rect, 2, 1, 2, 7, color);
            seg(painter, rect, 2, 1, 7, 2, color);
            seg(painter, rect, 7, 2, 2, 4, color);
        }
        Icon::LogIn => {
            stroke(painter, rect, 1, 1, 4, 7, color);
            seg(painter, rect, 5, 4, 7, 4, color);
            seg(painter, rect, 6, 3, 7, 4, color);
            seg(painter, rect, 6, 5, 7, 4, color);
            seg(painter, rect, 7, 3, 7, 5, color);
        }
        Icon::LogOut => {
            stroke(painter, rect, 1, 1, 4, 7, color);
            seg(painter, rect, 4, 4, 7, 4, color);
            seg(painter, rect, 6, 3, 7, 4, color);
            seg(painter, rect, 6, 5, 7, 4, color);
            seg(painter, rect, 4, 1, 4, 2, color);
        }
        Icon::Undo => {
            seg(painter, rect, 3, 2, 1, 4, color);
            seg(painter, rect, 1, 4, 3, 6, color);
            seg(painter, rect, 1, 4, 5, 4, color);
            seg(painter, rect, 5, 4, 7, 6, color);
        }
        Icon::Redo => {
            seg(painter, rect, 5, 2, 7, 4, color);
            seg(painter, rect, 7, 4, 5, 6, color);
            seg(painter, rect, 7, 4, 3, 4, color);
            seg(painter, rect, 3, 4, 1, 6, color);
        }
        Icon::Shield => {
            seg(painter, rect, 4, 1, 1, 2, color);
            seg(painter, rect, 1, 2, 1, 5, color);
            seg(painter, rect, 1, 5, 4, 7, color);
            seg(painter, rect, 4, 7, 7, 5, color);
            seg(painter, rect, 7, 5, 7, 2, color);
            seg(painter, rect, 7, 2, 4, 1, color);
            seg(painter, rect, 3, 4, 4, 5, color);
            seg(painter, rect, 4, 5, 6, 3, color);
        }
        Icon::Sliders => {
            for (gy, kx) in [(2, 3), (4, 6), (6, 2)] {
                seg(painter, rect, 1, gy, 7, gy, color);
                band(painter, rect, kx, gy - 1, kx + 1, gy + 1, color);
            }
        }
        Icon::Cart => {
            seg(painter, rect, 1, 1, 2, 1, color);
            seg(painter, rect, 2, 1, 2, 4, color);
            seg(painter, rect, 2, 4, 6, 4, color);
            seg(painter, rect, 6, 4, 7, 2, color);
            seg(painter, rect, 2, 2, 7, 2, color);
            dot(painter, rect, 3, 6, color);
            dot(painter, rect, 6, 6, color);
        }
        Icon::CreditCard => {
            stroke(painter, rect, 1, 2, 7, 6, color);
            seg(painter, rect, 1, 4, 7, 4, color);
            seg(painter, rect, 2, 5, 4, 5, color);
        }
        Icon::Package => {
            seg(painter, rect, 1, 3, 4, 1, color);
            seg(painter, rect, 4, 1, 7, 3, color);
            seg(painter, rect, 7, 3, 7, 6, color);
            seg(painter, rect, 7, 6, 4, 7, color);
            seg(painter, rect, 4, 7, 1, 6, color);
            seg(painter, rect, 1, 6, 1, 3, color);
            seg(painter, rect, 4, 1, 4, 4, color);
            seg(painter, rect, 1, 3, 4, 4, color);
            seg(painter, rect, 7, 3, 4, 4, color);
        }
        Icon::Terminal => {
            stroke(painter, rect, 1, 1, 7, 7, color);
            seg(painter, rect, 2, 3, 3, 4, color);
            seg(painter, rect, 3, 4, 2, 5, color);
            seg(painter, rect, 4, 5, 6, 5, color);
        }
        Icon::Link => {
            seg(painter, rect, 2, 5, 1, 6, color);
            seg(painter, rect, 1, 6, 2, 7, color);
            seg(painter, rect, 2, 7, 3, 6, color);
            seg(painter, rect, 3, 6, 5, 4, color);
            seg(painter, rect, 5, 4, 6, 3, color);
            seg(painter, rect, 6, 3, 7, 4, color);
            seg(painter, rect, 7, 4, 6, 5, color);
            seg(painter, rect, 6, 5, 5, 5, color);
        }
    }
}

/// Draws a picture from the iconset to fill `rect`.
pub fn paint_stock(painter: &mut Painter<'_, '_>, stock: Stock, rect: Rect) {
    feos_iconset::draw(painter, stock, rect);
}

impl feos_iconset::Target for Painter<'_, '_> {
    fn visible(&self, rect: Rect) -> bool {
        Painter::visible(self, rect)
    }

    fn blend_pixel(&mut self, x: i32, y: i32, color: Color, alpha: u8) {
        Painter::blend_pixel(self, x, y, color, alpha);
    }
}

impl Icon {
    /// The iconset picture this icon is drawn as, if it has one.
    ///
    /// Icons whose colour carries meaning stay glyphs even where a picture
    /// exists: the rating's stars are filled by colour, and play and lock sit
    /// beside pause and unlock, which have no picture.
    pub const fn picture(self) -> Option<Stock> {
        Some(match self {
            Icon::Info => Stock::Info,
            Icon::Error => Stock::Error,
            Icon::Help => Stock::Help,
            Icon::Search => Stock::Search,
            Icon::Settings => Stock::Gear,
            Icon::Home => Stock::Home,
            Icon::User => Stock::Users,
            Icon::Calendar => Stock::Calendar,
            Icon::File => Stock::GenericFile,
            Icon::Folder => Stock::Folder,
            Icon::Trash => Stock::RecycleBin,
            Icon::Edit => Stock::Edit,
            Icon::Copy => Stock::Copy,
            Icon::Mail => Stock::Mail,
            Icon::Globe => Stock::Globe,
            Icon::Database => Stock::Drive,
            Icon::Cpu => Stock::Chip,
            Icon::Package => Stock::Software,
            Icon::LogOut => Stock::LogOff,
            Icon::Terminal => Stock::Terminal,
            _ => return None,
        })
    }
}

fn origin(rect: Rect) -> (i32, i32, i32) {
    let u = rect.width.min(rect.height) as i32;
    (
        rect.x + (rect.width as i32 - u) / 2,
        rect.y + (rect.height as i32 - u) / 2,
        u,
    )
}

fn point(rect: Rect, gx: i32, gy: i32) -> (i32, i32) {
    let (x, y, u) = origin(rect);
    (x + gx * u / 8, y + gy * u / 8)
}

fn seg(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: Color,
) {
    let a = point(rect, x0, y0);
    let b = point(rect, x1, y1);
    painter.line(a.0, a.1, b.0, b.1, color);
}

fn band(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: Color,
) {
    let a = point(rect, x0, y0);
    let b = point(rect, x1, y1);
    painter.fill_rect(
        Rect::new(
            a.0,
            a.1,
            (b.0 - a.0).max(1) as u32,
            (b.1 - a.1).max(1) as u32,
        ),
        color,
    );
}

fn ring(painter: &mut Painter<'_, '_>, rect: Rect, cx: i32, cy: i32, radius: i32, color: Color) {
    let (x, y, u) = origin(rect);
    painter.circle(
        x + cx * u / 8,
        y + cy * u / 8,
        (radius * u / 8).max(1) as u32,
        color,
    );
}

fn dot(painter: &mut Painter<'_, '_>, rect: Rect, cx: i32, cy: i32, color: Color) {
    let (x, y, u) = origin(rect);
    painter.fill_circle(
        x + cx * u / 8,
        y + cy * u / 8,
        (u / 16).max(1) as u32,
        color,
    );
}

fn stroke(
    painter: &mut Painter<'_, '_>,
    rect: Rect,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: Color,
) {
    seg(painter, rect, x0, y0, x1, y0, color);
    seg(painter, rect, x1, y0, x1, y1, color);
    seg(painter, rect, x1, y1, x0, y1, color);
    seg(painter, rect, x0, y1, x0, y0, color);
}
