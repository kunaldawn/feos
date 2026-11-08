//! Pointer cursors shared by the desktop and full-screen applications.
//!
//! The pictures are the Windows XP set in `assets/cursors` (see its README and
//! `LICENSING.md`): the static `xp-default` cursors, and the animated busy and
//! working-in-background hourglasses from `animated`. Only the files named in
//! [`CursorShape::file`] are embedded. They're decoded each time they're drawn,
//! into buffers on the stack: a 32x32 cursor is 4 KiB of pixels, and drawing
//! one allocates nothing.
//!
//! Old cursors invert the screen under some pixels (the text beam and the
//! cross do, so they show on black and white alike); those are drawn inverted
//! here too.

use feos_gfx::{Color, Point, Rect};
use feos_image::ani::Animation;
use feos_image::cur::{self, CursorInfo};

use crate::painter::Painter;

/// The largest cursor drawn, in pixels on a side. Every embedded one is 32.
pub const CURSOR_MAX: u32 = 32;

const RGBA: usize = (CURSOR_MAX * CURSOR_MAX * 4) as usize;
const INVERT: usize = (CURSOR_MAX * CURSOR_MAX / 8) as usize;

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
    /// The text beam, over text that can be typed in or selected.
    Text,
    /// The pointing hand, over a link.
    Link,
    /// The four-way arrow, for moving something.
    Move,
    /// The arrow with a question mark.
    Help,
    /// The thin cross, for precise picking.
    Precision,
    /// The slashed circle: what's under the pointer won't take it.
    Unavailable,
    /// The turning hourglass: busy, so the pointer can't be used. Animated.
    Busy,
    /// The arrow with a turning hourglass: working, but the pointer still
    /// works. Animated.
    Working,
}

impl CursorShape {
    /// Every shape, for tests and galleries.
    pub const ALL: [CursorShape; 13] = [
        CursorShape::Arrow,
        CursorShape::SizeHorizontal,
        CursorShape::SizeVertical,
        CursorShape::SizeNwse,
        CursorShape::SizeNesw,
        CursorShape::Text,
        CursorShape::Link,
        CursorShape::Move,
        CursorShape::Help,
        CursorShape::Precision,
        CursorShape::Unavailable,
        CursorShape::Busy,
        CursorShape::Working,
    ];

    /// The embedded file: a `.cur`, or an `.ani` for the animated shapes.
    pub fn file(self) -> &'static [u8] {
        macro_rules! cursor {
            ($path:literal) => {
                include_bytes!(concat!("../../../assets/cursors/", $path))
            };
        }
        match self {
            CursorShape::Arrow => cursor!("xp-default/arrow.cur"),
            CursorShape::SizeHorizontal => cursor!("xp-default/size-we.cur"),
            CursorShape::SizeVertical => cursor!("xp-default/size-ns.cur"),
            CursorShape::SizeNwse => cursor!("xp-default/size-nwse.cur"),
            CursorShape::SizeNesw => cursor!("xp-default/size-nesw.cur"),
            CursorShape::Text => cursor!("xp-default/text.cur"),
            CursorShape::Link => cursor!("xp-default/link.cur"),
            CursorShape::Move => cursor!("xp-default/move.cur"),
            CursorShape::Help => cursor!("xp-default/help.cur"),
            CursorShape::Precision => cursor!("xp-default/precision.cur"),
            CursorShape::Unavailable => cursor!("xp-default/unavailable.cur"),
            CursorShape::Busy => cursor!("animated/busy.ani"),
            CursorShape::Working => cursor!("animated/working.ani"),
        }
    }

    /// Whether the shape changes over time.
    pub const fn animated(self) -> bool {
        matches!(self, CursorShape::Busy | CursorShape::Working)
    }

    /// The image of this shape `elapsed_ms` after it appeared, as a cursor
    /// file, and how long until the next image (`None` when it never changes).
    fn image_at(self, elapsed_ms: u64) -> Option<(&'static [u8], Option<u64>)> {
        let file = self.file();
        if cur::is_magic(file) {
            return Some((file, None));
        }
        let animation = Animation::parse(file).ok()?;
        let (step, left) = animation.step_at(elapsed_ms);
        let frame = animation.frame(animation.frame_at_step(step)).ok()?;
        Some((frame, Some(left)))
    }

    /// Size and hot spot of the image shown `elapsed_ms` after the shape appeared.
    fn info_at(self, elapsed_ms: u64) -> Option<CursorInfo> {
        let (file, _) = self.image_at(elapsed_ms)?;
        let info = cur::info(file).ok()?;
        (info.width <= CURSOR_MAX && info.height <= CURSOR_MAX).then_some(info)
    }
}

/// How long until an animated `shape`, shown for `elapsed_ms`, changes; `None`
/// for a shape that doesn't animate.
pub fn cursor_frame_left(shape: CursorShape, elapsed_ms: u64) -> Option<u64> {
    if !shape.animated() {
        return None;
    }
    shape.image_at(elapsed_ms).and_then(|(_, left)| left)
}

/// The screen area `shape` can touch with its hot spot at `(x, y)`.
///
/// A caller that keeps the cursor out of its composed frame restores this
/// rectangle before drawing the cursor somewhere else. It covers every frame
/// of an animated shape: all are the same size, with the same hot spot.
pub fn cursor_bounds(x: i32, y: i32, shape: CursorShape) -> Rect {
    match shape.info_at(0) {
        Some(info) => Rect::new(
            x - info.hotspot_x as i32,
            y - info.hotspot_y as i32,
            info.width,
            info.height,
        ),
        None => Rect::new(x, y, CLASSIC_WIDTH, CLASSIC.len() as u32),
    }
}

/// Draws `shape` with its hot spot at `(x, y)`, as it first appears.
pub fn draw_cursor_shape(painter: &mut Painter<'_, '_>, x: i32, y: i32, shape: CursorShape) {
    draw_cursor_shape_at(painter, x, y, shape, 0);
}

/// Draws `shape` with its hot spot at `(x, y)`, as it looks `elapsed_ms` after
/// it appeared.
pub fn draw_cursor_shape_at(
    painter: &mut Painter<'_, '_>,
    x: i32,
    y: i32,
    shape: CursorShape,
    elapsed_ms: u64,
) {
    if !draw_image(painter, x, y, shape, elapsed_ms) {
        draw_classic_arrow(painter, x, y);
    }
}

/// Draws the arrow pointer at `x`/`y`.
pub fn draw_cursor(painter: &mut Painter<'_, '_>, x: i32, y: i32) {
    draw_cursor_shape(painter, x, y, CursorShape::Arrow);
}

fn draw_image(
    painter: &mut Painter<'_, '_>,
    x: i32,
    y: i32,
    shape: CursorShape,
    elapsed_ms: u64,
) -> bool {
    let Some((file, _)) = shape.image_at(elapsed_ms) else {
        return false;
    };
    let mut rgba = [0u8; RGBA];
    let mut invert = [0u8; INVERT];
    let Ok(info) = cur::info(file) else {
        return false;
    };
    if info.width > CURSOR_MAX || info.height > CURSOR_MAX {
        return false;
    }
    if cur::decode(file, &mut rgba, &mut invert, &mut []).is_err() {
        return false;
    }
    let origin = Point::new(x - info.hotspot_x as i32, y - info.hotspot_y as i32);
    let bounds = Rect::new(origin.x, origin.y, info.width, info.height);
    if !painter.visible(bounds) {
        return true;
    }
    let width = info.width as usize;
    for row in 0..info.height as usize {
        for column in 0..width {
            let index = row * width + column;
            let (px, py) = (origin.x + column as i32, origin.y + row as i32);
            if cur::inverts(&invert, index) {
                if let Some(under) = painter.pixel(px, py) {
                    painter.put_pixel(px, py, Color::from_u32(!under.raw() & 0x00ff_ffff));
                }
                continue;
            }
            let at = index * 4;
            let alpha = rgba[at + 3];
            if alpha != 0 {
                painter.blend_pixel(px, py, Color::rgb(rgba[at], rgba[at + 1], rgba[at + 2]), alpha);
            }
        }
    }
    true
}

const CLASSIC_WIDTH: u32 = 12;
const CLASSIC: [&str; 19] = [
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

/// The arrow drawn from a bitmap in the code, in case an embedded cursor
/// wouldn't decode, which only a broken build could arrange.
fn draw_classic_arrow(painter: &mut Painter<'_, '_>, x: i32, y: i32) {
    for (row, line) in CLASSIC.iter().enumerate() {
        for (column, pixel) in line.bytes().enumerate() {
            let color = match pixel {
                b'X' => Color::BLACK,
                b'.' => Color::WHITE,
                _ => continue,
            };
            painter.put_pixel(x + column as i32, y + row as i32, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feos_gfx::Canvas;

    const BACKGROUND: u32 = 0x0030_6090;

    fn draw(shape: CursorShape, elapsed_ms: u64) -> std::vec::Vec<u32> {
        let mut pixels = std::vec![BACKGROUND; 96 * 96];
        let mut canvas = Canvas::new(&mut pixels, 96, 96, 96).unwrap();
        let mut painter = Painter::new(&mut canvas);
        draw_cursor_shape_at(&mut painter, 48, 48, shape, elapsed_ms);
        pixels
    }

    #[test]
    fn every_shape_decodes_and_draws_inside_its_bounds() {
        for shape in CursorShape::ALL {
            let info = shape.info_at(0).unwrap_or_else(|| panic!("{shape:?} won't decode"));
            assert_eq!((info.width, info.height), (32, 32), "{shape:?}");
            let bounds = cursor_bounds(48, 48, shape);
            assert!(bounds.contains(Point::new(48, 48)), "{shape:?} hot spot");
            let pixels = draw(shape, 0);
            let mut painted = 0;
            for (index, &pixel) in pixels.iter().enumerate() {
                if pixel != BACKGROUND {
                    painted += 1;
                    let point = Point::new((index % 96) as i32, (index / 96) as i32);
                    assert!(bounds.contains(point), "{shape:?} painted outside at {point:?}");
                }
            }
            assert!(painted > 20, "{shape:?} painted {painted} pixels");
        }
    }

    #[test]
    fn the_arrow_tip_is_the_hot_spot() {
        let pixels = draw(CursorShape::Arrow, 0);
        assert_eq!(pixels[48 * 96 + 48] & 0xff_ffff, 0);
        assert_eq!(pixels[47 * 96 + 48], BACKGROUND);
        assert_eq!(pixels[48 * 96 + 47], BACKGROUND);
        // The area to the right of the arrow's tip stays clear.
        assert_eq!(pixels[48 * 96 + 50], BACKGROUND);
    }

    #[test]
    fn the_text_beam_inverts_what_is_under_it() {
        let pixels = draw(CursorShape::Text, 0);
        assert!(pixels.iter().any(|&pixel| pixel == !BACKGROUND & 0xff_ffff));
    }

    #[test]
    fn busy_and_working_animate_and_static_shapes_do_not() {
        for shape in [CursorShape::Busy, CursorShape::Working] {
            let left = cursor_frame_left(shape, 0).unwrap();
            assert!(left > 0 && left < 2000);
            let first = draw(shape, 0);
            let changed = (1..40).any(|step| draw(shape, step * 100) != first);
            assert!(changed, "{shape:?} never changes");
        }
        assert_eq!(cursor_frame_left(CursorShape::Arrow, 0), None);
        assert_eq!(cursor_frame_left(CursorShape::Text, 500), None);
    }

    #[test]
    fn drawing_near_the_edges_is_clipped() {
        let mut pixels = [BACKGROUND; 16 * 16];
        let mut canvas = Canvas::new(&mut pixels, 16, 16, 16).unwrap();
        let mut painter = Painter::new(&mut canvas);
        for shape in CursorShape::ALL {
            for (x, y) in [(0, 0), (15, 15), (-5, 8), (20, 20)] {
                draw_cursor_shape(&mut painter, x, y, shape);
            }
        }
    }
}
