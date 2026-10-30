//! Geometry helpers used by the layout engine, painter and widgets.

use feos_gfx::{Point, Rect, Size};

/// Horizontal alignment inside a rectangle.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}

/// Vertical alignment inside a rectangle.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum VerticalAlign {
    #[default]
    Top,
    Middle,
    Bottom,
}

/// Four-sided spacing, in pixels.
///
/// Padding may be negative when an element deliberately draws outside its
/// layout cell (shadows and focus rings do this internally).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Padding {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Padding {
    pub const ZERO: Self = Self::all(0);

    /// The same padding on all four sides.
    pub const fn all(value: i32) -> Self {
        Self {
            left: value,
            top: value,
            right: value,
            bottom: value,
        }
    }

    /// Horizontal and vertical padding.
    pub const fn symmetric(horizontal: i32, vertical: i32) -> Self {
        Self {
            left: horizontal,
            top: vertical,
            right: horizontal,
            bottom: vertical,
        }
    }

    /// Explicit padding for each side.
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Total horizontal padding.
    pub const fn horizontal(self) -> i32 {
        self.left.saturating_add(self.right)
    }

    /// Total vertical padding.
    pub const fn vertical(self) -> i32 {
        self.top.saturating_add(self.bottom)
    }

    /// Applies this padding to `rect`, clamping the result to non-negative
    /// dimensions.
    pub fn apply(self, rect: Rect) -> Rect {
        rect.inset_each(self.left, self.top, self.right, self.bottom)
    }
}

/// Convenience operations on [`Rect`].
///
/// The graphics crate deliberately keeps `Rect` small. The GUI layer needs
/// many derived rectangles, so those helpers live here.
pub trait RectExt {
    /// Removes `amount` from every side.
    fn inset(self, amount: i32) -> Rect;

    /// Removes `x` from the left/right and `y` from the top/bottom.
    fn inset_xy(self, x: i32, y: i32) -> Rect;

    /// Removes an independent amount from each side.
    fn inset_each(self, left: i32, top: i32, right: i32, bottom: i32) -> Rect;

    /// Moves the rectangle.
    fn translate(self, dx: i32, dy: i32) -> Rect;

    /// Returns the rectangle with a new top-left corner.
    fn at(self, x: i32, y: i32) -> Rect;

    /// Returns the rectangle with new dimensions (clamped to non-negative).
    fn sized(self, width: i32, height: i32) -> Rect;

    /// Centers this rectangle inside `outer`.
    fn center_in(self, outer: Rect) -> Rect;

    /// The rectangle's center point.
    fn center(self) -> Point;

    /// The right edge as a signed coordinate.
    fn right_i32(self) -> i32;

    /// The bottom edge as a signed coordinate.
    fn bottom_i32(self) -> i32;

    /// Intersects, returning an empty rectangle instead of `None`.
    fn clipped_to(self, other: Rect) -> Rect;

    /// The top `height` pixels and the rest.
    fn split_top(self, height: i32) -> (Rect, Rect);

    /// The bottom `height` pixels and the rest.
    fn split_bottom(self, height: i32) -> (Rect, Rect);

    /// The left `width` pixels and the rest.
    fn split_left(self, width: i32) -> (Rect, Rect);

    /// The right `width` pixels and the rest.
    fn split_right(self, width: i32) -> (Rect, Rect);
}

impl RectExt for Rect {
    fn inset(self, amount: i32) -> Rect {
        self.inset_each(amount, amount, amount, amount)
    }

    fn inset_xy(self, x: i32, y: i32) -> Rect {
        self.inset_each(x, y, x, y)
    }

    fn inset_each(self, left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        let width = (self.width as i64 - left as i64 - right as i64).max(0);
        let height = (self.height as i64 - top as i64 - bottom as i64).max(0);
        Rect::new(
            self.x.saturating_add(left),
            self.y.saturating_add(top),
            width as u32,
            height as u32,
        )
    }

    fn translate(self, dx: i32, dy: i32) -> Rect {
        Rect::new(
            self.x.saturating_add(dx),
            self.y.saturating_add(dy),
            self.width,
            self.height,
        )
    }

    fn at(self, x: i32, y: i32) -> Rect {
        Rect::new(x, y, self.width, self.height)
    }

    fn sized(self, width: i32, height: i32) -> Rect {
        Rect::new(self.x, self.y, width.max(0) as u32, height.max(0) as u32)
    }

    fn center_in(self, outer: Rect) -> Rect {
        self.at(
            outer.x + (outer.width as i32 - self.width as i32) / 2,
            outer.y + (outer.height as i32 - self.height as i32) / 2,
        )
    }

    fn center(self) -> Point {
        Point::new(
            self.x + self.width as i32 / 2,
            self.y + self.height as i32 / 2,
        )
    }

    fn right_i32(self) -> i32 {
        self.right().clamp(i32::MIN as i64, i32::MAX as i64) as i32
    }

    fn bottom_i32(self) -> i32 {
        self.bottom().clamp(i32::MIN as i64, i32::MAX as i64) as i32
    }

    fn clipped_to(self, other: Rect) -> Rect {
        self.intersect(other)
            .unwrap_or_else(|| Rect::new(0, 0, 0, 0))
    }

    fn split_top(self, height: i32) -> (Rect, Rect) {
        let height = height.clamp(0, self.height as i32);
        (
            Rect::new(self.x, self.y, self.width, height as u32),
            Rect::new(
                self.x,
                self.y.saturating_add(height),
                self.width,
                self.height.saturating_sub(height as u32),
            ),
        )
    }

    fn split_bottom(self, height: i32) -> (Rect, Rect) {
        let height = height.clamp(0, self.height as i32);
        let top = self.height.saturating_sub(height as u32);
        (
            Rect::new(
                self.x,
                self.y.saturating_add(top as i32),
                self.width,
                height as u32,
            ),
            Rect::new(self.x, self.y, self.width, top),
        )
    }

    fn split_left(self, width: i32) -> (Rect, Rect) {
        let width = width.clamp(0, self.width as i32);
        (
            Rect::new(self.x, self.y, width as u32, self.height),
            Rect::new(
                self.x.saturating_add(width),
                self.y,
                self.width.saturating_sub(width as u32),
                self.height,
            ),
        )
    }

    fn split_right(self, width: i32) -> (Rect, Rect) {
        let width = width.clamp(0, self.width as i32);
        let left = self.width.saturating_sub(width as u32);
        (
            Rect::new(
                self.x.saturating_add(left as i32),
                self.y,
                width as u32,
                self.height,
            ),
            Rect::new(self.x, self.y, left, self.height),
        )
    }
}

/// Returns the union of two rectangles, ignoring empty ones.
pub fn union(a: Rect, b: Rect) -> Rect {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }

    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = a.right().max(b.right());
    let bottom = a.bottom().max(b.bottom());
    Rect::new(x, y, (right - x as i64) as u32, (bottom - y as i64) as u32)
}

/// A width/height pair built from signed values, clamping negatives to zero.
pub fn size(width: i32, height: i32) -> Size {
    Size::new(width.max(0) as u32, height.max(0) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inset_clamps_at_zero() {
        let rect = Rect::new(10, 20, 5, 4);
        assert_eq!(rect.inset(10), Rect::new(20, 30, 0, 0));
        assert_eq!(rect.inset_each(1, 2, 1, 1), Rect::new(11, 22, 3, 1));
    }

    #[test]
    fn splits_are_complementary() {
        let rect = Rect::new(0, 0, 100, 40);
        let (top, bottom) = rect.split_top(12);
        assert_eq!(top, Rect::new(0, 0, 100, 12));
        assert_eq!(bottom, Rect::new(0, 12, 100, 28));
        assert_eq!(union(top, bottom), rect);
    }

    #[test]
    fn centering_accounts_for_size() {
        let child = Rect::new(0, 0, 20, 10);
        assert_eq!(
            child.center_in(Rect::new(0, 0, 100, 50)),
            Rect::new(40, 20, 20, 10)
        );
    }
}
