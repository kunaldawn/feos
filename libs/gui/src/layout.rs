//! The linear layout engine.
//!
//! feos-gui deliberately avoids a full constraint solver. Widgets ask the
//! current frame for the next cell in a row, column or grid, and the frame
//! returns its rectangle. This is small, deterministic and allocation-free,
//! and it maps naturally onto an immediate-mode API.

use feos_gfx::Rect;

use crate::geometry::RectExt;

/// Maximum nesting depth for rows, columns and grids.
pub const MAX_LAYOUT_DEPTH: usize = 16;

/// The direction widgets flow in a layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Row,
    Column,
    Grid,
}

#[derive(Clone, Copy, Debug)]
struct Frame {
    rect: Rect,
    cursor: i32,
    kind: Kind,
    gap: i32,
    columns: u32,
    cell_width: i32,
    cell_height: i32,
    index: u32,
}

impl Frame {
    const fn column(rect: Rect, gap: i32) -> Self {
        Self {
            rect,
            cursor: rect.y,
            kind: Kind::Column,
            gap,
            columns: 1,
            cell_width: 0,
            cell_height: 0,
            index: 0,
        }
    }

    const fn row(rect: Rect, gap: i32) -> Self {
        Self {
            rect,
            cursor: rect.x,
            kind: Kind::Row,
            gap,
            columns: 1,
            cell_width: 0,
            cell_height: 0,
            index: 0,
        }
    }

    fn grid(rect: Rect, columns: u32, cell_height: i32, gap: i32) -> Self {
        let columns = columns.max(1);
        let cell_width = (rect.width as i32 - gap * (columns as i32 - 1)) / columns as i32;
        Self {
            rect,
            cursor: rect.y,
            kind: Kind::Grid,
            gap,
            columns,
            cell_width: cell_width.max(0),
            cell_height: cell_height.max(0),
            index: 0,
        }
    }

    fn next(&mut self, content_width: i32, content_height: i32) -> Rect {
        match self.kind {
            Kind::Column => {
                let height = content_height.max(0);
                let rect = Rect::new(self.rect.x, self.cursor, self.rect.width, height as u32);
                self.cursor = self.cursor.saturating_add(height).saturating_add(self.gap);
                rect
            }
            Kind::Row => {
                let width = content_width.max(0);
                let rect = Rect::new(self.cursor, self.rect.y, width as u32, self.rect.height);
                self.cursor = self.cursor.saturating_add(width).saturating_add(self.gap);
                rect
            }
            Kind::Grid => {
                let column = (self.index % self.columns) as i32;
                let row = (self.index / self.columns) as i32;
                self.index = self.index.saturating_add(1);
                Rect::new(
                    self.rect
                        .x
                        .saturating_add(column * (self.cell_width + self.gap)),
                    self.rect
                        .y
                        .saturating_add(row * (self.cell_height + self.gap)),
                    self.cell_width.max(0) as u32,
                    self.cell_height.max(0) as u32,
                )
            }
        }
    }

    fn next_sized(&mut self, width: i32, height: i32) -> Rect {
        match self.kind {
            Kind::Column => {
                let width = width.max(0).min(self.rect.width as i32);
                let height = height.max(0);
                let rect = Rect::new(self.rect.x, self.cursor, width as u32, height as u32);
                self.cursor = self.cursor.saturating_add(height).saturating_add(self.gap);
                rect
            }
            Kind::Row | Kind::Grid => self.next(width, height),
        }
    }

    fn used(&self) -> Rect {
        match self.kind {
            Kind::Column => {
                let height = (self.cursor - self.gap - self.rect.y).max(0);
                Rect::new(self.rect.x, self.rect.y, self.rect.width, height as u32)
            }
            Kind::Row => {
                let width = (self.cursor - self.gap - self.rect.x).max(0);
                Rect::new(self.rect.x, self.rect.y, width as u32, self.rect.height)
            }
            Kind::Grid => {
                let rows = if self.index == 0 {
                    0
                } else {
                    self.index.div_ceil(self.columns)
                };
                let height = (rows as i32 * (self.cell_height + self.gap) - self.gap).max(0);
                Rect::new(self.rect.x, self.rect.y, self.rect.width, height as u32)
            }
        }
    }

    fn axis(&self) -> Axis {
        match self.kind {
            Kind::Row => Axis::Horizontal,
            Kind::Column | Kind::Grid => Axis::Vertical,
        }
    }

    fn remaining(&self) -> (i32, i32) {
        match self.kind {
            Kind::Column => (
                self.rect.width as i32,
                (self.rect.bottom_i32() - self.cursor).max(0),
            ),
            Kind::Row => (
                (self.rect.right_i32() - self.cursor).max(0),
                self.rect.height as i32,
            ),
            Kind::Grid => (self.cell_width, self.cell_height),
        }
    }
}

/// A stack of active layout frames.
///
/// The UI owns one; apps normally use [`Ui::column`](crate::Ui::column),
/// [`Ui::row`](crate::Ui::row) and [`Ui::grid`](crate::Ui::grid) instead of
/// touching this directly.
pub struct LayoutStack {
    frames: [Frame; MAX_LAYOUT_DEPTH],
    depth: usize,
}

impl LayoutStack {
    /// Creates a stack with a default full-canvas column.
    pub fn new(rect: Rect, gap: i32) -> Self {
        let mut stack = Self {
            frames: [Frame::column(Rect::new(0, 0, 0, 0), gap); MAX_LAYOUT_DEPTH],
            depth: 1,
        };
        stack.frames[0] = Frame::column(rect, gap);
        stack
    }

    /// Resets the stack to a default column filling `rect`.
    pub fn reset(&mut self, rect: Rect, gap: i32) {
        self.depth = 1;
        self.frames[0] = Frame::column(rect, gap);
    }

    /// Pushes a column frame.
    pub fn push_column(&mut self, rect: Rect, gap: i32) {
        if self.depth < MAX_LAYOUT_DEPTH {
            self.frames[self.depth] = Frame::column(rect, gap);
            self.depth += 1;
        }
    }

    /// Pushes a row frame.
    pub fn push_row(&mut self, rect: Rect, gap: i32) {
        if self.depth < MAX_LAYOUT_DEPTH {
            self.frames[self.depth] = Frame::row(rect, gap);
            self.depth += 1;
        }
    }

    /// Pushes a grid frame.
    pub fn push_grid(&mut self, rect: Rect, columns: u32, cell_height: i32, gap: i32) {
        if self.depth < MAX_LAYOUT_DEPTH {
            self.frames[self.depth] = Frame::grid(rect, columns, cell_height, gap);
            self.depth += 1;
        }
    }

    /// Pops the current frame and returns the area it consumed.
    pub fn pop(&mut self) -> Rect {
        if self.depth <= 1 {
            return self.frames[0].used();
        }

        let used = self.frames[self.depth - 1].used();
        self.depth -= 1;
        used
    }

    /// Returns the next widget rectangle in the current frame.
    pub fn next(&mut self, content_width: i32, content_height: i32) -> Rect {
        if self.depth == 0 {
            return Rect::new(
                0,
                0,
                content_width.max(0) as u32,
                content_height.max(0) as u32,
            );
        }
        self.frames[self.depth - 1].next(content_width, content_height)
    }

    /// Returns the next cell with an explicit width even in a column.
    ///
    /// Normal column widgets fill the frame width; `next_sized` is for widgets
    /// such as `button_sized` that need an exact width.
    pub fn next_sized(&mut self, width: i32, height: i32) -> Rect {
        if self.depth == 0 {
            return Rect::new(0, 0, width.max(0) as u32, height.max(0) as u32);
        }
        self.frames[self.depth - 1].next_sized(width, height)
    }

    /// The current frame's axis.
    pub fn axis(&self) -> Axis {
        if self.depth == 0 {
            Axis::Vertical
        } else {
            self.frames[self.depth - 1].axis()
        }
    }

    /// Remaining width and height in the current frame.
    pub fn remaining(&self) -> (i32, i32) {
        if self.depth == 0 {
            (0, 0)
        } else {
            self.frames[self.depth - 1].remaining()
        }
    }

    /// Current nesting depth.
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// The rectangle of the current frame.
    pub fn rect(&self) -> Rect {
        if self.depth == 0 {
            Rect::new(0, 0, 0, 0)
        } else {
            self.frames[self.depth - 1].rect
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_stacks_widgets_vertically() {
        let mut stack = LayoutStack::new(Rect::new(10, 20, 100, 100), 5);
        assert_eq!(stack.next(0, 10), Rect::new(10, 20, 100, 10));
        assert_eq!(stack.next(0, 20), Rect::new(10, 35, 100, 20));
        assert_eq!(stack.axis(), Axis::Vertical);
    }

    #[test]
    fn row_uses_intrinsic_widths() {
        let mut stack = LayoutStack::new(Rect::new(0, 0, 100, 20), 4);
        stack.push_row(Rect::new(0, 0, 100, 20), 4);
        assert_eq!(stack.next(30, 0), Rect::new(0, 0, 30, 20));
        assert_eq!(stack.next(20, 0), Rect::new(34, 0, 20, 20));
        stack.pop();
    }

    #[test]
    fn grid_wraps_into_rows() {
        let mut stack = LayoutStack::new(Rect::new(0, 0, 100, 100), 5);
        stack.push_grid(Rect::new(0, 0, 100, 100), 2, 20, 10);
        assert_eq!(stack.next(0, 0), Rect::new(0, 0, 45, 20));
        assert_eq!(stack.next(0, 0), Rect::new(55, 0, 45, 20));
        assert_eq!(stack.next(0, 0), Rect::new(0, 30, 45, 20));
    }

    #[test]
    fn pop_reports_used_extent() {
        let mut stack = LayoutStack::new(Rect::new(0, 0, 100, 100), 4);
        stack.push_column(Rect::new(0, 0, 100, 100), 4);
        stack.next(0, 10);
        stack.next(0, 20);
        assert_eq!(stack.pop(), Rect::new(0, 0, 100, 34));
    }
}
