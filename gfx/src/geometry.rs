//! Basic geometry types used by the drawing API.

use core::cmp::{max, min};

/// A point in canvas coordinates.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A width/height pair in pixels.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// An axis-aligned rectangle, with its top-left corner at `(x, y)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub const fn size(self) -> Size {
        Size::new(self.width, self.height)
    }

    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Right edge, exclusive (as an `i64` so it cannot overflow).
    pub const fn right(self) -> i64 {
        self.x as i64 + self.width as i64
    }

    /// Bottom edge, exclusive (as an `i64` so it cannot overflow).
    pub const fn bottom(self) -> i64 {
        self.y as i64 + self.height as i64
    }

    pub fn contains(self, point: Point) -> bool {
        (point.x as i64) >= self.x as i64
            && (point.x as i64) < self.right()
            && (point.y as i64) >= self.y as i64
            && (point.y as i64) < self.bottom()
    }

    /// Returns the overlapping region, or `None` if the rectangles are
    /// disjoint.
    pub fn intersect(self, other: Self) -> Option<Self> {
        let x0 = max(self.x, other.x);
        let y0 = max(self.y, other.y);
        let x1 = min(self.right(), other.right());
        let y1 = min(self.bottom(), other.bottom());

        if x1 <= x0 as i64 || y1 <= y0 as i64 {
            None
        } else {
            Some(Self::new(
                x0,
                y0,
                (x1 - x0 as i64) as u32,
                (y1 - y0 as i64) as u32,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersection_clips_to_overlap() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, -5, 10, 10);
        assert_eq!(a.intersect(b), Some(Rect::new(5, 0, 5, 5)));
    }

    #[test]
    fn disjoint_rects_do_not_intersect() {
        assert_eq!(
            Rect::new(0, 0, 4, 4).intersect(Rect::new(4, 4, 4, 4)),
            None
        );
    }

    #[test]
    fn contains_uses_exclusive_edges() {
        let rect = Rect::new(0, 0, 10, 10);
        assert!(rect.contains(Point::new(0, 0)));
        assert!(rect.contains(Point::new(9, 9)));
        assert!(!rect.contains(Point::new(10, 9)));
    }
}
