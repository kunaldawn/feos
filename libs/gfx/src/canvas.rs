//! The drawing surface.

use crate::color::Color;
use crate::font;
use crate::geometry::{Rect, Size};
#[cfg(test)]
use crate::geometry::Point;

/// A 2D drawing surface backed by a 32-bit XRGB8888 pixel buffer.
///
/// `stride` is the number of pixels between the start of two consecutive
/// scanlines, which may be larger than `width` for padded framebuffers.
///
/// A canvas borrows its buffer mutably, so it is impossible to create two
/// canvases over the same pixels at the same time.
pub struct Canvas<'a> {
    pixels: &'a mut [u32],
    width: usize,
    height: usize,
    stride: usize,
}

impl<'a> Canvas<'a> {
    /// Creates a canvas over `pixels`.
    ///
    /// Returns `None` if the dimensions are zero, if `stride < width`, or if
    /// the buffer is too small for `stride * height` pixels.
    pub fn new(pixels: &'a mut [u32], width: usize, height: usize, stride: usize) -> Option<Self> {
        if width == 0 || height == 0 || stride < width {
            return None;
        }
        // The last row only needs `width` pixels, not a full stride. This lets
        // sub-canvases start at a non-zero x without requiring padding past
        // the end of their own rows.
        let required = stride.checked_mul(height - 1)?.checked_add(width)?;
        if pixels.len() < required {
            return None;
        }

        Some(Self {
            pixels,
            width,
            height,
            stride,
        })
    }

    pub const fn width(&self) -> usize {
        self.width
    }

    pub const fn height(&self) -> usize {
        self.height
    }

    pub const fn stride(&self) -> usize {
        self.stride
    }

    pub const fn size(&self) -> Size {
        Size::new(self.width as u32, self.height as u32)
    }

    /// The complete backing buffer (including any stride padding).
    pub fn pixels(&self) -> &[u32] {
        self.pixels
    }

    /// Mutable access to the complete backing buffer.
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        self.pixels
    }

    /// Creates a drawing surface for `rect` inside this canvas.
    ///
    /// The returned canvas has its own origin at `(0, 0)` and shares the
    /// parent's stride, so it can be used to host a window client without
    /// copying pixels. Drawing is clipped to the requested rectangle.
    pub fn sub_canvas(&mut self, rect: Rect) -> Option<Canvas<'_>> {
        let x0 = rect.x.max(0) as i64;
        let y0 = rect.y.max(0) as i64;
        let x1 = rect.right().min(self.width as i64);
        let y1 = rect.bottom().min(self.height as i64);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }

        let x = x0 as usize;
        let y = y0 as usize;
        let width = (x1 - x0) as usize;
        let height = (y1 - y0) as usize;
        let start = y * self.stride + x;
        let len = self.stride * (height - 1) + width;
        if start + len > self.pixels.len() {
            return None;
        }
        Canvas::new(
            &mut self.pixels[start..start + len],
            width,
            height,
            self.stride,
        )
    }

    /// Copies the overlapping part of `source` into this canvas.
    ///
    /// A fast whole-buffer copy is used when the layouts match; otherwise the
    /// rows are copied with clipping. This is the present step for an
    /// off-screen composition buffer.
    pub fn copy_from(&mut self, source: &Canvas<'_>) {
        let width = (self.width as u32).min(source.width as u32) as usize;
        let height = (self.height as u32).min(source.height as u32) as usize;
        for row in 0..height {
            let source_start = row * source.stride;
            let destination_start = row * self.stride;
            self.pixels[destination_start..destination_start + width]
                .copy_from_slice(&source.pixels[source_start..source_start + width]);
        }
    }

    /// Fills the whole canvas with `color`.
    pub fn clear(&mut self, color: Color) {
        let raw = color.raw();
        for y in 0..self.height {
            let row = y * self.stride;
            self.pixels[row..row + self.width].fill(raw);
        }
    }

    /// Writes a single pixel, ignoring coordinates outside the canvas.
    pub fn put_pixel(&mut self, x: i32, y: i32, color: Color) {
        if x < 0 || y < 0 {
            return;
        }

        let (x, y) = (x as usize, y as usize);
        if x < self.width && y < self.height {
            self.pixels[y * self.stride + x] = color.raw();
        }
    }

    /// Reads a single pixel. Returns `None` outside the canvas.
    pub fn pixel(&self, x: i32, y: i32) -> Option<Color> {
        if x < 0 || y < 0 {
            return None;
        }

        let (x, y) = (x as usize, y as usize);
        if x < self.width && y < self.height {
            Some(Color::from_u32(self.pixels[y * self.stride + x]))
        } else {
            None
        }
    }

    /// Fills `rect`, clipped to the canvas.
    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        if rect.is_empty() {
            return;
        }

        let x0 = rect.x.max(0).min(self.width as i32) as usize;
        let y0 = rect.y.max(0).min(self.height as i32) as usize;
        let x1 = rect.right().clamp(0, self.width as i64) as usize;
        let y1 = rect.bottom().clamp(0, self.height as i64) as usize;

        if x0 >= x1 || y0 >= y1 {
            return;
        }

        let raw = color.raw();
        for y in y0..y1 {
            let row = y * self.stride;
            self.pixels[row + x0..row + x1].fill(raw);
        }
    }

    /// Draws a rectangle outline of the given thickness (clamped to at least
    /// one pixel).
    pub fn stroke_rect(&mut self, rect: Rect, thickness: u32, color: Color) {
        if rect.is_empty() {
            return;
        }

        let t = thickness.max(1).min(rect.width).min(rect.height);

        self.fill_rect(Rect::new(rect.x, rect.y, rect.width, t), color);
        self.fill_rect(
            Rect::new(rect.x, rect.y + (rect.height - t) as i32, rect.width, t),
            color,
        );
        self.fill_rect(Rect::new(rect.x, rect.y, t, rect.height), color);
        self.fill_rect(
            Rect::new(rect.x + (rect.width - t) as i32, rect.y, t, rect.height),
            color,
        );
    }

    /// Draws a horizontal line from `x0` to `x1` (inclusive).
    pub fn hline(&mut self, x0: i32, x1: i32, y: i32, color: Color) {
        if y < 0 || y >= self.height as i32 {
            return;
        }

        let (a, b) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
        if b < 0 || a >= self.width as i32 {
            return;
        }

        let start = a.max(0) as usize;
        let end = b.min(self.width as i32 - 1) as usize;
        let row = y as usize * self.stride;
        self.pixels[row + start..=row + end].fill(color.raw());
    }

    /// Draws a vertical line from `y0` to `y1` (inclusive).
    pub fn vline(&mut self, x: i32, y0: i32, y1: i32, color: Color) {
        if x < 0 || x >= self.width as i32 {
            return;
        }

        let (a, b) = if y0 <= y1 { (y0, y1) } else { (y1, y0) };
        if b < 0 || a >= self.height as i32 {
            return;
        }

        let start = a.max(0) as usize;
        let end = b.min(self.height as i32 - 1) as usize;
        let x = x as usize;
        let raw = color.raw();
        for y in start..=end {
            self.pixels[y * self.stride + x] = raw;
        }
    }

    /// Draws a line with Bresenham's algorithm. Both endpoints are included.
    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: Color) {
        let clip = Rect::new(0, 0, self.width as u32, self.height as u32);
        self.line_clipped(x0, y0, x1, y1, clip, color);
    }

    /// Draws the part of a line that falls inside `clip`.
    ///
    /// The range of steps that can be visible is computed up front, so a very
    /// long or far off-screen line costs only as much as its visible part.
    /// The pixels are the same ones an unclipped walk would visit.
    pub fn line_clipped(
        &mut self,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        clip: Rect,
        color: Color,
    ) {
        let clip = clip.clipped_to_canvas(self.width, self.height);
        if clip.is_empty() {
            return;
        }
        let (min_x, max_x) = (clip.x as i64, clip.right() - 1);
        let (min_y, max_y) = (clip.y as i64, clip.bottom() - 1);
        let (x0, y0, x1, y1) = (x0 as i64, y0 as i64, x1 as i64, y1 as i64);
        if x0.max(x1) < min_x || x0.min(x1) > max_x || y0.max(y1) < min_y || y0.min(y1) > max_y {
            return;
        }

        let dx = (x1 - x0).abs();
        let dy = (y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let raw = color.raw();

        if dx == 0 && dy == 0 {
            self.pixels[y0 as usize * self.stride + x0 as usize] = raw;
            return;
        }

        // Walk the major axis; the minor coordinate is a rounded multiple of
        // the slope, which is what Bresenham's error term computes.
        let (major, minor, major_start, minor_start, major_step, minor_step, major_range) =
            if dx >= dy {
                (dx, dy, x0, y0, sx, sy, (min_x, max_x))
            } else {
                (dy, dx, y0, x0, sy, sx, (min_y, max_y))
            };
        let (low, high) = major_range;
        let (first, last) = if major_step > 0 {
            ((low - major_start).max(0), (high - major_start).min(major))
        } else {
            ((major_start - high).max(0), (major_start - low).min(major))
        };

        let mut step = first;
        while step <= last {
            let offset = ((2 * step as i128 * minor as i128 + major as i128) / (2 * major as i128)) as i64;
            let along = major_start + major_step * step;
            let across = minor_start + minor_step * offset;
            let (x, y) = if dx >= dy {
                (along, across)
            } else {
                (across, along)
            };
            if x >= min_x && x <= max_x && y >= min_y && y <= max_y {
                self.pixels[y as usize * self.stride + x as usize] = raw;
            }
            step += 1;
        }
    }

    /// Draws a one-pixel circle outline using the midpoint algorithm.
    pub fn circle(&mut self, cx: i32, cy: i32, radius: u32, color: Color) {
        let r = radius.min(i32::MAX as u32) as i32;
        if r == 0 {
            self.put_pixel(cx, cy, color);
            return;
        }

        let mut x = r;
        let mut y = 0;
        let mut err = 1 - r;

        while x >= y {
            self.plot_circle(cx, cy, x, y, color);
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
    }

    fn plot_circle(&mut self, cx: i32, cy: i32, x: i32, y: i32, color: Color) {
        for (dx, dy) in [
            (x, y),
            (y, x),
            (-y, x),
            (-x, y),
            (-x, -y),
            (-y, -x),
            (y, -x),
            (x, -y),
        ] {
            self.put_pixel(cx.saturating_add(dx), cy.saturating_add(dy), color);
        }
    }

    /// Fills a circle.
    pub fn fill_circle(&mut self, cx: i32, cy: i32, radius: u32, color: Color) {
        if radius == 0 {
            self.put_pixel(cx, cy, color);
            return;
        }

        let r = radius.min(i32::MAX as u32) as i64;
        let cx = cx as i64;
        let cy = cy as i64;

        for dy in -r..=r {
            let dx = isqrt64((r * r - dy * dy) as u64) as i64;
            let x0 = (cx - dx).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            let x1 = (cx + dx).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            let y = (cy + dy).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            self.hline(x0, x1, y, color);
        }
    }

    /// Fills `rect` with a vertical gradient from `top` to `bottom`.
    pub fn fill_vertical_gradient(&mut self, rect: Rect, top: Color, bottom: Color) {
        let clip = Rect::new(0, 0, self.width as u32, self.height as u32);
        self.fill_vertical_gradient_clipped(rect, clip, top, bottom);
    }

    /// Fills the part of a vertical gradient that falls inside `clip`.
    ///
    /// The colour of a row depends only on its position in `rect`, so a
    /// clipped redraw matches the same rows of a full one, and rows outside
    /// `clip` cost nothing.
    pub fn fill_vertical_gradient_clipped(
        &mut self,
        rect: Rect,
        clip: Rect,
        top: Color,
        bottom: Color,
    ) {
        if rect.is_empty() {
            return;
        }
        let clip = clip.clipped_to_canvas(self.width, self.height);
        let x0 = (rect.x as i64).max(clip.x as i64);
        let x1 = rect.right().min(clip.right());
        let y0 = (rect.y as i64).max(clip.y as i64);
        let y1 = rect.bottom().min(clip.bottom());
        if x0 >= x1 || y0 >= y1 {
            return;
        }

        let denominator = rect.height.saturating_sub(1).max(1) as u64;
        for y in y0..y1 {
            let row = (y - rect.y as i64) as u64;
            let t = ((row * 255) / denominator) as u8;
            let raw = top.lerp(bottom, t).raw();
            let start = y as usize * self.stride;
            self.pixels[start + x0 as usize..start + x1 as usize].fill(raw);
        }
    }

    /// Blends `color` over `rect` with constant `alpha`, clipped to the canvas.
    pub fn blend_rect(&mut self, rect: Rect, color: Color, alpha: u8) {
        if alpha == 0 || rect.is_empty() {
            return;
        }
        let x0 = rect.x.max(0).min(self.width as i32) as usize;
        let y0 = rect.y.max(0).min(self.height as i32) as usize;
        let x1 = rect.right().clamp(0, self.width as i64) as usize;
        let y1 = rect.bottom().clamp(0, self.height as i64) as usize;
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        if alpha == 255 {
            self.fill_rect(rect, color);
            return;
        }

        // One channel multiply per pixel, written against the row slice so the
        // loop has no per-pixel bounds or clip checks.
        let inv = 255 - alpha as u32;
        let t = alpha as u32;
        let (r, g, b) = (
            color.r() as u32 * t,
            color.g() as u32 * t,
            color.b() as u32 * t,
        );
        for y in y0..y1 {
            let start = y * self.stride;
            for pixel in &mut self.pixels[start + x0..start + x1] {
                let old = *pixel;
                let nr = (((old >> 16) & 0xff) * inv + r) / 255;
                let ng = (((old >> 8) & 0xff) * inv + g) / 255;
                let nb = ((old & 0xff) * inv + b) / 255;
                *pixel = (nr << 16) | (ng << 8) | nb;
            }
        }
    }

    /// Draws one character with an 8x8 bitmap font. `scale` magnifies each
    /// font pixel into a `scale * scale` square (clamped to 1..=64).
    pub fn draw_char(&mut self, x: i32, y: i32, ch: char, color: Color, scale: u32) {
        let clip = Rect::new(0, 0, self.width as u32, self.height as u32);
        self.draw_char_clipped(x, y, ch, color, scale, clip);
    }

    /// Draws the part of one character that falls inside `clip`, one run of
    /// set bits at a time.
    pub fn draw_char_clipped(
        &mut self,
        x: i32,
        y: i32,
        ch: char,
        color: Color,
        scale: u32,
        clip: Rect,
    ) {
        let scale = scale.clamp(1, 64);
        let clip = clip.clipped_to_canvas(self.width, self.height);
        if clip.is_empty() {
            return;
        }
        let step = scale as i64;
        let (clip_x0, clip_x1) = (clip.x as i64, clip.right());
        let (clip_y0, clip_y1) = (clip.y as i64, clip.bottom());
        let (x, y) = (x as i64, y as i64);
        if x >= clip_x1
            || x + font::GLYPH_WIDTH as i64 * step <= clip_x0
            || y >= clip_y1
            || y + font::GLYPH_HEIGHT as i64 * step <= clip_y0
        {
            return;
        }

        let raw = color.raw();
        let glyph = font::glyph(ch);
        for (row, bits) in glyph.iter().enumerate() {
            let top = y + row as i64 * step;
            let y0 = top.max(clip_y0);
            let y1 = (top + step).min(clip_y1);
            if y0 >= y1 || *bits == 0 {
                continue;
            }
            let mut col = 0usize;
            while col < font::GLYPH_WIDTH {
                if bits & (1 << col) == 0 {
                    col += 1;
                    continue;
                }
                let start = col;
                while col < font::GLYPH_WIDTH && bits & (1 << col) != 0 {
                    col += 1;
                }
                let x0 = (x + start as i64 * step).max(clip_x0);
                let x1 = (x + col as i64 * step).min(clip_x1);
                if x0 < x1 {
                    for py in y0..y1 {
                        let base = py as usize * self.stride;
                        self.pixels[base + x0 as usize..base + x1 as usize].fill(raw);
                    }
                }
            }
        }
    }

    /// Draws text with an 8x8 bitmap font. Supports `\n`, `\r` and `\t`.
    /// Returns the y coordinate just below the last line.
    pub fn draw_text(&mut self, x: i32, y: i32, text: &str, color: Color, scale: u32) -> i32 {
        let scale = scale.clamp(1, 64);
        let advance = font::GLYPH_WIDTH as i32 * scale as i32;
        let line_height = font::GLYPH_HEIGHT as i32 * scale as i32;

        let mut cx = x;
        let mut cy = y;
        for ch in text.chars() {
            match ch {
                '\n' => {
                    cx = x;
                    cy = cy.saturating_add(line_height);
                }
                '\r' => {}
                '\t' => cx = cx.saturating_add(advance.saturating_mul(4)),
                _ => {
                    self.draw_char(cx, cy, ch, color, scale);
                    cx = cx.saturating_add(advance);
                }
            }
        }

        cy.saturating_add(line_height)
    }

    /// Width in pixels of the widest line in `text`.
    pub fn text_width(text: &str, scale: u32) -> u32 {
        let scale = scale.clamp(1, 64);
        text.lines()
            .map(|line| (line.chars().count() as u32).saturating_mul(8 * scale))
            .max()
            .unwrap_or(0)
    }

    /// Height in pixels of `text`, including newlines.
    pub fn text_height(text: &str, scale: u32) -> u32 {
        let scale = scale.clamp(1, 64);
        let lines = text.bytes().filter(|&byte| byte == b'\n').count() as u32 + 1;
        lines.saturating_mul(8 * scale)
    }

    /// Copies a packed `width * height` XRGB8888 image to `(x, y)`, clipping
    /// at the canvas edges.
    pub fn blit(&mut self, x: i32, y: i32, width: u32, height: u32, pixels: &[u32]) {
        let clip = Rect::new(0, 0, self.width as u32, self.height as u32);
        self.blit_clipped(x, y, width, height, pixels, clip);
    }

    /// Copies the part of a packed image that falls inside `clip`, one row
    /// slice at a time.
    pub fn blit_clipped(
        &mut self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        pixels: &[u32],
        clip: Rect,
    ) {
        let source_width = width as usize;
        let source_height = height as usize;
        let Some(required) = source_width.checked_mul(source_height) else {
            return;
        };
        if required == 0 || pixels.len() < required {
            return;
        }

        let clip = clip.clipped_to_canvas(self.width, self.height);
        let x0 = (x as i64).max(clip.x as i64);
        let y0 = (y as i64).max(clip.y as i64);
        let x1 = (x as i64 + width as i64).min(clip.right());
        let y1 = (y as i64 + height as i64).min(clip.bottom());
        if x0 >= x1 || y0 >= y1 {
            return;
        }

        let columns = (x1 - x0) as usize;
        let skip = (x0 - x as i64) as usize;
        for py in y0..y1 {
            let row = (py - y as i64) as usize;
            let source = row * source_width + skip;
            let target = py as usize * self.stride + x0 as usize;
            self.pixels[target..target + columns]
                .copy_from_slice(&pixels[source..source + columns]);
        }
    }

    /// Copies `rect` from `source` to the same position in this canvas.
    ///
    /// The rectangle is clipped to both canvases. This is the partial present
    /// step for an off-screen buffer, and the restore step for a cached layer.
    pub fn copy_rect_from(&mut self, source: &Canvas<'_>, rect: Rect) {
        self.copy_rect_from_pixels(source.pixels, source.width, source.height, source.stride, rect);
    }

    /// Copies `rect` from a raw `width * height` buffer with `stride` pixels
    /// per row to the same position in this canvas.
    pub fn copy_rect_from_pixels(
        &mut self,
        pixels: &[u32],
        width: usize,
        height: usize,
        stride: usize,
        rect: Rect,
    ) {
        let x0 = rect.x.max(0) as i64;
        let y0 = rect.y.max(0) as i64;
        let x1 = rect.right().min(self.width as i64).min(width as i64);
        let y1 = rect.bottom().min(self.height as i64).min(height as i64);
        if x0 >= x1 || y0 >= y1 {
            return;
        }

        let columns = (x1 - x0) as usize;
        for y in y0 as usize..y1 as usize {
            let from = y * stride + x0 as usize;
            let to = y * self.stride + x0 as usize;
            if from + columns > pixels.len() {
                return;
            }
            self.pixels[to..to + columns].copy_from_slice(&pixels[from..from + columns]);
        }
    }
}

/// Integer square root (floor).
fn isqrt64(n: u64) -> u64 {
    if n < 2 {
        return n;
    }

    let mut x = n;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas(pixels: &mut [u32], width: usize, height: usize) -> Canvas<'_> {
        Canvas::new(pixels, width, height, width).unwrap()
    }

    #[test]
    fn rejects_invalid_buffers() {
        let mut pixels = [0u32; 4];
        assert!(Canvas::new(&mut pixels, 0, 1, 0).is_none());
        assert!(Canvas::new(&mut pixels, 3, 2, 2).is_none());
        assert!(Canvas::new(&mut pixels, 3, 2, 3).is_none());
        assert!(Canvas::new(&mut pixels, 2, 2, 2).is_some());
    }

    #[test]
    fn sub_canvas_draws_inside_the_parent() {
        let mut pixels = [0u32; 8 * 6];
        let mut parent = canvas(&mut pixels, 8, 6);
        parent.fill_rect(Rect::new(0, 0, 8, 6), Color::BLACK);
        {
            let mut child = parent.sub_canvas(Rect::new(2, 1, 3, 2)).unwrap();
            child.fill_rect(Rect::new(0, 0, 3, 2), Color::WHITE);
        }
        assert_eq!(parent.pixel(2, 1), Some(Color::WHITE));
        assert_eq!(parent.pixel(4, 2), Some(Color::WHITE));
        assert_eq!(parent.pixel(1, 1), Some(Color::BLACK));
        assert_eq!(parent.pixel(5, 2), Some(Color::BLACK));
    }

    #[test]
    fn sub_canvas_works_at_the_bottom_right_edge() {
        let mut pixels = [0u32; 8 * 6];
        let mut parent = canvas(&mut pixels, 8, 6);
        parent.fill_rect(Rect::new(0, 0, 8, 6), Color::BLACK);
        // The requested rectangle extends past both edges; the visible part
        // must still be usable.
        let mut child = parent.sub_canvas(Rect::new(5, 4, 10, 10)).unwrap();
        child.fill_rect(Rect::new(0, 0, 3, 2), Color::WHITE);
        assert_eq!(parent.pixel(5, 4), Some(Color::WHITE));
        assert_eq!(parent.pixel(7, 5), Some(Color::WHITE));
    }

    #[test]
    fn offscreen_copy_matches_source() {
        let mut source_pixels = [0u32; 4 * 3];
        let mut destination_pixels = [0u32; 4 * 3];
        {
            let mut source = canvas(&mut source_pixels, 4, 3);
            source.clear(Color::BLUE);
            source.put_pixel(1, 1, Color::RED);
        }
        {
            let source = canvas(&mut source_pixels, 4, 3);
            let mut destination = canvas(&mut destination_pixels, 4, 3);
            destination.copy_from(&source);
        }
        assert_eq!(destination_pixels[0], Color::BLUE.raw());
        assert_eq!(destination_pixels[5], Color::RED.raw());
    }

    #[test]
    fn pixels_are_clipped() {
        let mut pixels = [0u32; 4 * 4];
        let mut canvas = canvas(&mut pixels, 4, 4);

        canvas.put_pixel(1, 2, Color::RED);
        canvas.put_pixel(-1, 0, Color::GREEN);
        canvas.put_pixel(4, 0, Color::GREEN);

        assert_eq!(canvas.pixel(1, 2), Some(Color::RED));
        assert_eq!(canvas.pixel(-1, 0), None);
        assert_eq!(canvas.pixel(4, 0), None);
    }

    #[test]
    fn fill_rect_is_clipped() {
        let mut pixels = [0u32; 6 * 6];
        let mut canvas = canvas(&mut pixels, 6, 6);

        canvas.fill_rect(Rect::new(-1, -1, 2, 2), Color::RED);
        assert_eq!(canvas.pixel(0, 0), Some(Color::RED));
        assert_eq!(canvas.pixel(1, 0), Some(Color::BLACK));

        canvas.clear(Color::BLACK);
        canvas.fill_rect(Rect::new(4, 4, 10, 10), Color::BLUE);
        assert_eq!(canvas.pixel(4, 4), Some(Color::BLUE));
        assert_eq!(canvas.pixel(5, 5), Some(Color::BLUE));
        assert_eq!(canvas.pixel(3, 5), Some(Color::BLACK));
    }

    #[test]
    fn line_hits_both_endpoints() {
        let mut pixels = [0u32; 10 * 10];
        let mut canvas = canvas(&mut pixels, 10, 10);

        canvas.line(0, 0, 9, 9, Color::WHITE);
        assert_eq!(canvas.pixel(0, 0), Some(Color::WHITE));
        assert_eq!(canvas.pixel(9, 9), Some(Color::WHITE));
    }

    #[test]
    fn filled_circle_is_round_and_clipped() {
        let mut pixels = [0u32; 11 * 11];
        let mut canvas = canvas(&mut pixels, 11, 11);

        canvas.fill_circle(5, 5, 4, Color::RED);
        assert_eq!(canvas.pixel(5, 5), Some(Color::RED));
        assert_eq!(canvas.pixel(1, 5), Some(Color::RED));
        assert_eq!(canvas.pixel(9, 5), Some(Color::RED));
        assert_eq!(canvas.pixel(0, 5), Some(Color::BLACK));
        assert_eq!(canvas.pixel(1, 1), Some(Color::BLACK));
    }

    #[test]
    fn circle_outline_is_symmetric() {
        let mut pixels = [0u32; 21 * 21];
        let mut canvas = canvas(&mut pixels, 21, 21);

        canvas.circle(10, 10, 8, Color::RED);
        for (x, y) in [(2, 10), (18, 10), (10, 2), (10, 18)] {
            assert_eq!(canvas.pixel(x, y), Some(Color::RED), "({x}, {y})");
        }
        assert_eq!(canvas.pixel(10, 10), Some(Color::BLACK));
    }

    #[test]
    fn text_layout_and_rendering() {
        assert_eq!(Canvas::text_width("AB", 1), 16);
        assert_eq!(Canvas::text_width("AB", 2), 32);
        assert_eq!(Canvas::text_height("A\nB", 1), 16);

        let mut pixels = [0u32; 16 * 16];
        let mut canvas = canvas(&mut pixels, 16, 16);
        canvas.draw_text(0, 0, "A", Color::WHITE, 1);
        assert!(pixels.iter().any(|&pixel| pixel == Color::WHITE.raw()));
    }

    #[test]
    fn gradient_reaches_both_ends() {
        let mut pixels = [0u32; 3];
        let mut canvas = canvas(&mut pixels, 1, 3);
        canvas.fill_vertical_gradient(Rect::new(0, 0, 1, 3), Color::BLACK, Color::WHITE);

        assert_eq!(canvas.pixel(0, 0), Some(Color::BLACK));
        assert_eq!(canvas.pixel(0, 2), Some(Color::WHITE));
    }

    #[test]
    fn blit_clips_and_copies() {
        let image = [
            Color::RED.raw(),
            Color::GREEN.raw(),
            Color::BLUE.raw(),
            Color::WHITE.raw(),
        ];
        let mut pixels = [0u32; 4 * 4];
        let mut canvas = canvas(&mut pixels, 4, 4);

        canvas.blit(-1, -1, 2, 2, &image);
        assert_eq!(canvas.pixel(0, 0), Some(Color::WHITE));
        assert_eq!(canvas.pixel(1, 0), Some(Color::BLACK));
    }

    #[test]
    fn line_includes_both_endpoints_in_every_octant() {
        for &(x1, y1) in &[
            (39, 5),
            (39, 25),
            (0, 25),
            (25, 39),
            (5, 0),
            (0, 0),
            (39, 39),
            (20, 39),
        ] {
            let mut pixels = [0u32; 40 * 40];
            let mut surface = canvas(&mut pixels, 40, 40);
            surface.line(20, 20, x1, y1, Color::WHITE);
            assert_eq!(surface.pixel(20, 20), Some(Color::WHITE));
            assert_eq!(surface.pixel(x1, y1), Some(Color::WHITE));
        }
    }

    #[test]
    fn a_far_off_screen_line_is_clipped_without_walking_it() {
        let mut pixels = [0u32; 16 * 16];
        let mut surface = canvas(&mut pixels, 16, 16);
        // A walk over this range would take minutes in a dev build.
        surface.line(i32::MIN / 2, 8, i32::MAX / 2, 8, Color::WHITE);
        assert_eq!(surface.pixel(0, 8), Some(Color::WHITE));
        assert_eq!(surface.pixel(15, 8), Some(Color::WHITE));
        assert_eq!(surface.pixel(8, 7), Some(Color::BLACK));
        surface.line(i32::MIN, i32::MIN, i32::MAX, i32::MAX, Color::RED);
    }

    #[test]
    fn clipped_line_matches_the_visible_part_of_an_unclipped_one() {
        let mut full = [0u32; 32 * 32];
        let mut surface = canvas(&mut full, 32, 32);
        surface.line(1, 2, 30, 17, Color::WHITE);
        let mut clipped = [0u32; 32 * 32];
        let mut other = canvas(&mut clipped, 32, 32);
        let clip = Rect::new(8, 4, 10, 8);
        other.line_clipped(1, 2, 30, 17, clip, Color::WHITE);
        for y in 0..32i32 {
            for x in 0..32i32 {
                let expected = if clip.contains(Point::new(x, y)) {
                    full[y as usize * 32 + x as usize]
                } else {
                    0
                };
                assert_eq!(clipped[y as usize * 32 + x as usize], expected, "{x},{y}");
            }
        }
    }

    #[test]
    fn clipped_gradient_matches_the_same_rows_of_a_full_one() {
        let rect = Rect::new(-4, -8, 40, 64);
        let (top, bottom) = (Color::rgb(0, 10, 200), Color::rgb(250, 40, 0));
        let mut full = [0u32; 32 * 32];
        canvas(&mut full, 32, 32).fill_vertical_gradient(rect, top, bottom);
        let mut part = [0u32; 32 * 32];
        let clip = Rect::new(5, 5, 7, 9);
        canvas(&mut part, 32, 32).fill_vertical_gradient_clipped(rect, clip, top, bottom);
        for y in 0..32i32 {
            for x in 0..32i32 {
                let index = y as usize * 32 + x as usize;
                if clip.contains(Point::new(x, y)) {
                    assert_eq!(part[index], full[index]);
                } else {
                    assert_eq!(part[index], 0);
                }
            }
        }
    }

    #[test]
    fn blit_clips_on_every_side_and_keeps_source_alignment() {
        let source: [u32; 16] = core::array::from_fn(|index| index as u32 + 1);
        let mut pixels = [0u32; 4 * 4];
        canvas(&mut pixels, 4, 4).blit(-2, -2, 4, 4, &source);
        assert_eq!(pixels[0], source[2 * 4 + 2]);
        assert_eq!(pixels[1], source[2 * 4 + 3]);
        assert_eq!(pixels[4], source[3 * 4 + 2]);
        assert_eq!(pixels[2], 0);

        let mut pixels = [0u32; 4 * 4];
        canvas(&mut pixels, 4, 4).blit(3, 3, 4, 4, &source);
        assert_eq!(pixels[15], source[0]);
        assert_eq!(pixels[14], 0);

        // An image larger than its slice, or one that overflows, is ignored.
        let mut pixels = [0u32; 4 * 4];
        canvas(&mut pixels, 4, 4).blit(0, 0, u32::MAX, u32::MAX, &source);
        assert!(pixels.iter().all(|&pixel| pixel == 0));
    }

    #[test]
    fn copy_rect_copies_only_the_requested_region() {
        let mut source_pixels = [7u32; 8 * 8];
        let source = canvas(&mut source_pixels, 8, 8);
        let mut target_pixels = [0u32; 8 * 8];
        let mut target = canvas(&mut target_pixels, 8, 8);
        target.copy_rect_from(&source, Rect::new(6, 6, 100, 100));
        assert_eq!(target.pixel(6, 6).map(|c| c.raw()), Some(7));
        assert_eq!(target.pixel(7, 7).map(|c| c.raw()), Some(7));
        assert_eq!(target.pixel(5, 6).map(|c| c.raw()), Some(0));
        target.copy_rect_from(&source, Rect::new(-3, -3, 4, 4));
        assert_eq!(target.pixel(0, 0).map(|c| c.raw()), Some(7));
        assert_eq!(target.pixel(1, 1).map(|c| c.raw()), Some(0));
    }

    #[test]
    fn blend_rect_matches_per_pixel_lerp() {
        let mut pixels = [0u32; 4 * 4];
        let mut surface = canvas(&mut pixels, 4, 4);
        let base = Color::rgb(200, 100, 50);
        let over = Color::rgb(10, 20, 30);
        surface.fill_rect(Rect::new(0, 0, 4, 4), base);
        surface.blend_rect(Rect::new(-1, -1, 3, 3), over, 90);
        assert_eq!(surface.pixel(0, 0), Some(base.lerp(over, 90)));
        assert_eq!(surface.pixel(1, 1), Some(base.lerp(over, 90)));
        assert_eq!(surface.pixel(2, 2), Some(base));
    }

    #[test]
    fn span_glyphs_match_a_pixel_by_pixel_scale_up() {
        for scale in [1u32, 3] {
            let mut pixels = [0u32; 32 * 32];
            let mut surface = canvas(&mut pixels, 32, 32);
            surface.draw_char(2, 3, 'R', Color::WHITE, scale);
            let glyph = font::glyph('R');
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..font::GLYPH_WIDTH {
                    let expected = if bits & (1 << col) != 0 {
                        Color::WHITE
                    } else {
                        Color::BLACK
                    };
                    let px = 2 + col as i32 * scale as i32;
                    let py = 3 + row as i32 * scale as i32;
                    assert_eq!(surface.pixel(px, py), Some(expected));
                }
            }
        }
    }
}
