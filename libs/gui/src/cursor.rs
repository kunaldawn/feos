//! Pointer cursors shared by the desktop and full-screen applications.

use feos_gfx::{Color, Point, Rect};

use crate::painter::Painter;

/// The shape of a software pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CursorShape {
    /// The normal arrow.
    #[default]
    Arrow,
    /// Left-right resize arrow for the left and right window edges.
    SizeHorizontal,
    /// Up-down resize arrow for the top and bottom window edges.
    SizeVertical,
    /// Top-left/bottom-right resize arrow.
    SizeNwse,
    /// Top-right/bottom-left resize arrow.
    SizeNesw,
}

/// The screen area `shape` can touch with its hot spot at `(x, y)`.
///
/// A caller that keeps the cursor out of its composed frame restores this
/// rectangle before drawing the cursor somewhere else.
pub fn cursor_bounds(x: i32, y: i32, shape: CursorShape) -> Rect {
    match shape {
        CursorShape::Arrow => Rect::new(x, y, 16, 24),
        // The resize cursors are centred on the hot spot and reach 11 pixels
        // along each diagonal or axis, plus their outline.
        _ => Rect::new(x - 14, y - 14, 29, 29),
    }
}

/// Draws `shape` with its hotspot at `(x, y)`.
///
/// The arrow uses the classic bitmap; resize shapes are centred on the
/// pointer.
pub fn draw_cursor_shape(painter: &mut Painter<'_, '_>, x: i32, y: i32, shape: CursorShape) {
    match shape {
        CursorShape::Arrow => draw_cursor(painter, x, y),
        CursorShape::SizeHorizontal => draw_size_horizontal(painter, x, y),
        CursorShape::SizeVertical => draw_size_vertical(painter, x, y),
        CursorShape::SizeNwse => draw_size_nwse(painter, x, y),
        CursorShape::SizeNesw => draw_size_nesw(painter, x, y),
    }
}

/// Draws the classic XP arrow pointer at `x`/`y`.
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

/// Draws a filled arrowhead with a black outline and white centre.
fn draw_arrowhead(painter: &mut Painter<'_, '_>, outer: [Point; 3], inner: [Point; 3]) {
    painter.fill_polygon(&outer, Color::BLACK);
    painter.fill_polygon(&inner, Color::WHITE);
}

fn draw_size_horizontal(painter: &mut Painter<'_, '_>, x: i32, y: i32) {
    painter.fill_rect(Rect::new(x - 6, y - 1, 12, 3), Color::BLACK);
    painter.fill_rect(Rect::new(x - 6, y, 12, 1), Color::WHITE);
    draw_arrowhead(
        painter,
        [
            Point::new(x - 10, y),
            Point::new(x - 5, y - 5),
            Point::new(x - 5, y + 5),
        ],
        [
            Point::new(x - 9, y),
            Point::new(x - 5, y - 3),
            Point::new(x - 5, y + 3),
        ],
    );
    draw_arrowhead(
        painter,
        [
            Point::new(x + 10, y),
            Point::new(x + 5, y - 5),
            Point::new(x + 5, y + 5),
        ],
        [
            Point::new(x + 9, y),
            Point::new(x + 5, y - 3),
            Point::new(x + 5, y + 3),
        ],
    );
}

fn draw_size_vertical(painter: &mut Painter<'_, '_>, x: i32, y: i32) {
    painter.fill_rect(Rect::new(x - 1, y - 6, 3, 12), Color::BLACK);
    painter.fill_rect(Rect::new(x, y - 6, 1, 12), Color::WHITE);
    draw_arrowhead(
        painter,
        [
            Point::new(x, y - 10),
            Point::new(x - 5, y - 5),
            Point::new(x + 5, y - 5),
        ],
        [
            Point::new(x, y - 9),
            Point::new(x - 3, y - 5),
            Point::new(x + 3, y - 5),
        ],
    );
    draw_arrowhead(
        painter,
        [
            Point::new(x, y + 10),
            Point::new(x - 5, y + 5),
            Point::new(x + 5, y + 5),
        ],
        [
            Point::new(x, y + 9),
            Point::new(x - 3, y + 5),
            Point::new(x + 3, y + 5),
        ],
    );
}

fn draw_size_nwse(painter: &mut Painter<'_, '_>, x: i32, y: i32) {
    // A three-pixel black shaft with a white centre line.
    for offset in -1..=1 {
        painter.line(x - 6 + offset, y - 6, x + 6 + offset, y + 6, Color::BLACK);
        painter.line(x - 6, y - 6 + offset, x + 6, y + 6 + offset, Color::BLACK);
    }
    painter.line(x - 5, y - 5, x + 5, y + 5, Color::WHITE);

    draw_arrowhead(
        painter,
        [
            Point::new(x - 9, y - 9),
            Point::new(x - 3, y - 8),
            Point::new(x - 8, y - 3),
        ],
        [
            Point::new(x - 8, y - 8),
            Point::new(x - 4, y - 7),
            Point::new(x - 7, y - 4),
        ],
    );
    draw_arrowhead(
        painter,
        [
            Point::new(x + 9, y + 9),
            Point::new(x + 3, y + 8),
            Point::new(x + 8, y + 3),
        ],
        [
            Point::new(x + 8, y + 8),
            Point::new(x + 4, y + 7),
            Point::new(x + 7, y + 4),
        ],
    );
}

fn draw_size_nesw(painter: &mut Painter<'_, '_>, x: i32, y: i32) {
    for offset in -1..=1 {
        painter.line(x + 6 + offset, y - 6, x - 6 + offset, y + 6, Color::BLACK);
        painter.line(x + 6, y - 6 + offset, x - 6, y + 6 + offset, Color::BLACK);
    }
    painter.line(x + 5, y - 5, x - 5, y + 5, Color::WHITE);

    draw_arrowhead(
        painter,
        [
            Point::new(x + 9, y - 9),
            Point::new(x + 3, y - 8),
            Point::new(x + 8, y - 3),
        ],
        [
            Point::new(x + 8, y - 8),
            Point::new(x + 4, y - 7),
            Point::new(x + 7, y - 4),
        ],
    );
    draw_arrowhead(
        painter,
        [
            Point::new(x - 9, y + 9),
            Point::new(x - 3, y + 8),
            Point::new(x - 8, y + 3),
        ],
        [
            Point::new(x - 8, y + 8),
            Point::new(x - 4, y + 7),
            Point::new(x - 7, y + 4),
        ],
    );
}
