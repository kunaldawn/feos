//! Shared painting helpers for the desktop shell.

use core::fmt::Write;

use feos_bundle::Icon;
use feos_gfx::{Color, Rect};
use feos_gui::{Painter, RectExt, Ui};
use feos_text::Text;

use crate::config::{TaskbarConfig, WallpaperConfig, WallpaperStyle};
use crate::theme::DesktopTheme;

/// Draws a decoded bundle icon, scaling it to `rect` with nearest-neighbour
/// sampling. An empty icon becomes a small coloured placeholder.
pub fn draw_icon(painter: &mut Painter<'_, '_>, icon: &Icon, rect: Rect, fallback: Color) {
    if rect.is_empty() {
        return;
    }
    if icon.is_empty() {
        painter.fill_rounded_rect(rect, 4, fallback);
        let inset = (rect.width.min(rect.height) / 4) as i32;
        painter.fill_rounded_rect(
            Rect::new(
                rect.x + inset,
                rect.y + inset,
                rect.width.saturating_sub((inset * 2) as u32),
                rect.height.saturating_sub((inset * 2) as u32),
            ),
            3,
            Color::WHITE,
        );
        return;
    }

    let width = rect.width.max(1) as i32;
    let height = rect.height.max(1) as i32;
    let icon_width = icon.width().max(1) as i32;
    let icon_height = icon.height().max(1) as i32;
    for y in 0..height {
        let source_y = (y * icon_height / height) as u32;
        for x in 0..width {
            let source_x = (x * icon_width / width) as u32;
            if let Some([r, g, b, a]) = icon.pixel(source_x, source_y) {
                painter.blend_pixel(rect.x + x, rect.y + y, Color::rgb(r, g, b), a);
            }
        }
    }
}

/// Paints the configured wallpaper.
pub fn draw_wallpaper<const N: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    config: &WallpaperConfig,
) {
    let full = ui.canvas_rect();
    match config.style {
        WallpaperStyle::Solid => {
            ui.painter.canvas_mut().fill_rect(full, config.top);
        }
        WallpaperStyle::Gradient | WallpaperStyle::Grid => {
            ui.painter
                .canvas_mut()
                .fill_vertical_gradient(full, config.top, config.bottom);
        }
    }

    if config.style == WallpaperStyle::Grid {
        let step = 48;
        let mut x = full.x;
        while x < full.right_i32() {
            ui.painter
                .fill_rect(Rect::new(x, full.y, 1, full.height), config.grid);
            x += step;
        }
        let mut y = full.y;
        while y < full.bottom_i32() {
            ui.painter
                .fill_rect(Rect::new(full.x, y, full.width, 1), config.grid);
            y += step;
        }
    }

    // A soft horizon at the bottom gives the gradient some depth without
    // needing an image decoder in the desktop library.
    if config.style != WallpaperStyle::Solid {
        let horizon = full.y + (full.height as i32 * 2 / 3);
        let hill = theme.wallpaper_bottom.lerp(Color::BLACK, 40);
        let points = [
            feos_gfx::Point::new(full.x, horizon),
            feos_gfx::Point::new(full.x + (full.width as i32 / 3), horizon - 60),
            feos_gfx::Point::new(full.x + (full.width as i32 * 2 / 3), horizon - 20),
            feos_gfx::Point::new(full.right_i32(), horizon - 90),
            feos_gfx::Point::new(full.right_i32(), full.bottom_i32()),
            feos_gfx::Point::new(full.x, full.bottom_i32()),
        ];
        ui.painter.fill_polygon(&points, hill);
    }
}

/// Formats the taskbar clock from uptime.
pub fn format_clock(uptime_ms: u64, config: &TaskbarConfig) -> Text<16> {
    let total_seconds = uptime_ms / 1000;
    let hours = (total_seconds / 3600) % 24;
    let minutes = (total_seconds / 60) % 60;
    let mut out = Text::new();
    if config.clock_24h {
        let _ = write!(out, "{hours:02}:{minutes:02}");
    } else {
        let mut hour = hours % 12;
        if hour == 0 {
            hour = 12;
        }
        let suffix = if hours < 12 { "AM" } else { "PM" };
        let _ = write!(out, "{hour}:{minutes:02} {suffix}");
    }
    out
}

/// Draws the four-pane "flag" used by the XP Start button.
pub fn draw_start_flag(painter: &mut Painter<'_, '_>, rect: Rect) {
    let size = (rect.width.min(rect.height) / 2).max(4) as i32;
    let gap = 2;
    let x = rect.x;
    let y = rect.y;
    let colors = [
        Color::rgb(255, 92, 64),
        Color::rgb(92, 196, 80),
        Color::rgb(64, 132, 255),
        Color::rgb(255, 208, 64),
    ];
    for (index, color) in colors.iter().enumerate() {
        let cx = x + (index % 2) as i32 * (size + gap);
        let cy = y + (index / 2) as i32 * (size + gap);
        painter.fill_rect(Rect::new(cx, cy, size as u32, size as u32), *color);
    }
}

/// Draws the classic XP arrow pointer at `x`/`y`.
///
/// The bitmap is small enough that the desktop can paint it after every other
/// layer, which keeps the cursor above windows and menus without a compositor.
pub fn draw_cursor(painter: &mut Painter<'_, '_>, x: i32, y: i32) {
    const CURSOR: [&str; 19] = [
        "X           ",
        "XX          ",
        "X.X         ",
        "X..X        ",
        "X...X       ",
        "X....X      ",
        "X.....X     ",
        "X......X    ",
        "X.......X   ",
        "X........X  ",
        "X.....XXXXX ",
        "X..X..X     ",
        "X.X X..X    ",
        "XX  X..X    ",
        "X    X..X   ",
        "     X..X   ",
        "      X..X  ",
        "      X..X  ",
        "       XX   ",
    ];

    let outline = Color::BLACK;
    let fill = Color::WHITE;
    for (row, line) in CURSOR.iter().enumerate() {
        for (column, pixel) in line.bytes().enumerate() {
            let color = match pixel {
                b'X' => outline,
                b'.' => fill,
                _ => continue,
            };
            painter.put_pixel(x + column as i32, y + row as i32, color);
        }
    }

    // A one-pixel soft shadow along the lower-right edge gives the pointer the
    // same slightly raised look as the Luna cursor.
    let shadow = Color::rgb(96, 96, 96);
    for offset in 1..=2 {
        painter.blend_pixel(x + 10 + offset, y + 17 + offset, shadow, 90);
        painter.blend_pixel(x + 9 + offset, y + 18, shadow, 70);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feos_gfx::Canvas;

    #[test]
    fn clock_formats_24_and_12_hour() {
        let mut config = TaskbarConfig::default();
        assert_eq!(format_clock(0, &config).as_str(), "00:00");
        assert_eq!(format_clock(3_723_000, &config).as_str(), "01:02");
        config.clock_24h = false;
        assert_eq!(format_clock(3_723_000, &config).as_str(), "1:02 AM");
        assert_eq!(format_clock(45_723_000, &config).as_str(), "12:42 PM");
    }

    #[test]
    fn cursor_does_not_paint_a_solid_background() {
        let mut pixels = [0u32; 64 * 64];
        let background = Color::rgb(10, 20, 30);
        let mut canvas = Canvas::new(&mut pixels, 64, 64, 64).unwrap();
        canvas.fill_rect(Rect::new(0, 0, 64, 64), background);
        let mut painter = Painter::new(&mut canvas);
        draw_cursor(&mut painter, 8, 8);

        // The tip and the white interior are painted.
        assert_eq!(painter.pixel(8, 8), Some(Color::BLACK));
        assert_eq!(painter.pixel(9, 10), Some(Color::WHITE));

        // The area to the right of the arrow stays transparent, so the cursor
        // never appears as a white rectangle on the wallpaper.
        assert_eq!(painter.pixel(9, 8), Some(background));
        assert_eq!(painter.pixel(18, 16), Some(background));
    }

    #[test]
    fn empty_icon_draws_a_placeholder() {
        let mut pixels = [0u32; 64 * 64];
        let mut canvas = Canvas::new(&mut pixels, 64, 64, 64).unwrap();
        let mut painter = Painter::new(&mut canvas);
        draw_icon(
            &mut painter,
            &Icon::EMPTY,
            Rect::new(8, 8, 32, 32),
            Color::BLUE,
        );
        assert_ne!(pixels[22 * 64 + 22], 0);
    }
}
