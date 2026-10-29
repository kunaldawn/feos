//! The drawing surface.

use crate::color::Color;
use crate::font;
use crate::geometry::{Rect, Size};

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
        if pixels.len() < stride.checked_mul(height)? {
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
    pub fn line(&mut self, mut x0: i32, mut y0: i32, x1: i32, y1: i32, color: Color) {
        let dx = (x1 - x0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let dy = -(y1 - y0).abs();
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;

        loop {
            self.put_pixel(x0, y0, color);
            if x0 == x1 && y0 == y1 {
                break;
            }

            let e2 = 2 * err;
            if e2 >= dy {
                if x0 == x1 {
                    break;
                }
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                if y0 == y1 {
                    break;
                }
                err += dx;
                y0 += sy;
            }
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
        if rect.is_empty() {
            return;
        }

        let denominator = rect.height.saturating_sub(1).max(1) as u64;
        for row in 0..rect.height {
            let t = ((row as u64 * 255) / denominator) as u8;
            self.fill_rect(
                Rect::new(rect.x, rect.y.saturating_add(row as i32), rect.width, 1),
                top.lerp(bottom, t),
            );
        }
    }

    /// Draws one character with an 8x8 bitmap font. `scale` magnifies each
    /// font pixel into a `scale * scale` square (clamped to 1..=64).
    pub fn draw_char(&mut self, x: i32, y: i32, ch: char, color: Color, scale: u32) {
        let scale = scale.clamp(1, 64);
        let glyph = font::glyph(ch);

        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..font::GLYPH_WIDTH {
                if bits & (1 << col) != 0 {
                    let px = x.saturating_add(col as i32 * scale as i32);
                    let py = y.saturating_add(row as i32 * scale as i32);
                    self.fill_rect(Rect::new(px, py, scale, scale), color);
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
        let width = width as usize;
        let height = height as usize;
        if width == 0 || height == 0 || pixels.len() < width * height {
            return;
        }

        for row in 0..height {
            let py = y as i64 + row as i64;
            if py < 0 || py >= self.height as i64 {
                continue;
            }

            for col in 0..width {
                let px = x as i64 + col as i64;
                if px < 0 || px >= self.width as i64 {
                    continue;
                }

                self.pixels[py as usize * self.stride + px as usize] = pixels[row * width + col];
            }
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
}
