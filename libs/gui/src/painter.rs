//! A clipped, themed drawing surface built on [`feos_gfx::Canvas`].
//!
//! Every operation is intersected with the painter's current clip rectangle,
//! so widgets can draw without knowing whether they are inside a window, a
//! scroll area or a partially off-screen panel.

use feos_gfx::{font, Canvas, Color, Point, Rect};

use crate::geometry::{Align, RectExt, VerticalAlign};

/// A clipped 2D drawing surface.
///
/// Two rectangles limit drawing. The *logical* clip is what widgets push and
/// pop, and it also decides what the pointer can hit. The *paint mask* limits
/// only pixels: the desktop sets it to the damaged part of the screen, so a
/// frame runs all of its hit testing while repainting a small region.
pub struct Painter<'a, 'buf> {
    canvas: &'a mut Canvas<'buf>,
    /// `logical` intersected with `mask`: what drawing actually honours.
    clip: Rect,
    logical: Rect,
    mask: Rect,
}

impl<'a, 'buf> Painter<'a, 'buf> {
    /// Creates a painter covering the whole canvas.
    pub fn new(canvas: &'a mut Canvas<'buf>) -> Self {
        let clip = Rect::new(0, 0, canvas.width() as u32, canvas.height() as u32);
        Self {
            canvas,
            clip,
            logical: clip,
            mask: clip,
        }
    }

    /// The underlying canvas.
    pub fn canvas(&self) -> &Canvas<'buf> {
        self.canvas
    }

    /// Mutable access to the underlying canvas.
    ///
    /// Drawing through the canvas bypasses the clip; prefer the painter's
    /// methods inside widgets.
    pub fn canvas_mut(&mut self) -> &mut Canvas<'buf> {
        self.canvas
    }

    /// The canvas rectangle.
    pub fn canvas_rect(&self) -> Rect {
        Rect::new(
            0,
            0,
            self.canvas.width() as u32,
            self.canvas.height() as u32,
        )
    }

    /// The canvas size.
    pub fn size(&self) -> (u32, u32) {
        (self.canvas.width() as u32, self.canvas.height() as u32)
    }

    /// The current clip rectangle.
    // The logical clip is the one callers see: `clip` is it with the mask applied.
    #[allow(clippy::misnamed_getters)]
    pub const fn clip(&self) -> Rect {
        self.logical
    }

    /// Limits pixel output to `rect` without changing the logical clip, so
    /// [`clip_contains`](Self::clip_contains) and hit testing are unaffected.
    pub fn set_paint_mask(&mut self, rect: Rect) {
        self.mask = rect;
        self.clip = self.logical.clipped_to(rect);
    }

    /// The current paint mask.
    pub const fn paint_mask(&self) -> Rect {
        self.mask
    }

    /// Intersects the current clip with `rect` and returns the previous clip.
    ///
    /// Pair every call with [`restore_clip`](Painter::restore_clip).
    pub fn push_clip(&mut self, rect: Rect) -> Rect {
        let previous = self.logical;
        self.logical = self.logical.clipped_to(rect);
        self.clip = self.logical.clipped_to(self.mask);
        previous
    }

    /// Restores a clip returned by [`push_clip`](Painter::push_clip).
    pub fn restore_clip(&mut self, clip: Rect) {
        self.logical = clip;
        self.clip = self.logical.clipped_to(self.mask);
    }

    /// Resets the clip to the whole canvas.
    pub fn reset_clip(&mut self) {
        self.logical = self.canvas_rect();
        self.clip = self.logical.clipped_to(self.mask);
    }

    /// Returns `true` when `point` is inside the current clip.
    pub fn clip_contains(&self, point: Point) -> bool {
        self.logical.contains(point)
    }

    /// Returns `true` when `rect` overlaps the clip.
    pub fn visible(&self, rect: Rect) -> bool {
        !rect.clipped_to(self.clip).is_empty()
    }

    /// Fills a rectangle, clipped to the painter's clip.
    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        let clipped = rect.clipped_to(self.clip);
        if !clipped.is_empty() {
            self.canvas.fill_rect(clipped, color);
        }
    }

    /// Draws a rectangle outline.
    pub fn stroke_rect(&mut self, rect: Rect, thickness: i32, color: Color) {
        if rect.is_empty() {
            return;
        }
        let thickness = thickness.max(1);
        let t = thickness.min(rect.width as i32).min(rect.height as i32);
        self.fill_rect(Rect::new(rect.x, rect.y, rect.width, t as u32), color);
        self.fill_rect(
            Rect::new(
                rect.x,
                rect.y + rect.height as i32 - t,
                rect.width,
                t as u32,
            ),
            color,
        );
        self.fill_rect(Rect::new(rect.x, rect.y, t as u32, rect.height), color);
        self.fill_rect(
            Rect::new(
                rect.x + rect.width as i32 - t,
                rect.y,
                t as u32,
                rect.height,
            ),
            color,
        );
    }

    /// Fills a rounded rectangle.
    pub fn fill_rounded_rect(&mut self, rect: Rect, radius: u32, color: Color) {
        if rect.is_empty() {
            return;
        }
        let max_radius = (rect.width.min(rect.height) / 2) as i32;
        let radius = (radius as i32).clamp(0, max_radius);
        if radius == 0 {
            self.fill_rect(rect, color);
            return;
        }

        for row in 0..rect.height as i32 {
            let dy = if row < radius {
                radius - row
            } else if row >= rect.height as i32 - radius {
                row - (rect.height as i32 - radius) + 1
            } else {
                0
            };
            let inset = corner_inset(radius, dy);
            self.fill_rect(
                Rect::new(
                    rect.x.saturating_add(inset),
                    rect.y.saturating_add(row),
                    rect.width.saturating_sub((inset * 2) as u32),
                    1,
                ),
                color,
            );
        }
    }

    /// Fills a rounded rectangle with a constant alpha (0..=255).
    pub fn fill_rounded_rect_alpha(&mut self, rect: Rect, radius: u32, color: Color, alpha: u8) {
        if rect.is_empty() || alpha == 0 {
            return;
        }
        if alpha == 255 {
            self.fill_rounded_rect(rect, radius, color);
            return;
        }

        let clipped = rect.clipped_to(self.clip);
        if clipped.is_empty() {
            return;
        }

        let max_radius = (rect.width.min(rect.height) / 2) as i32;
        let radius = (radius as i32).clamp(0, max_radius);
        for y in clipped.y..clipped.bottom_i32() {
            let row = y - rect.y;
            let dy = if row < radius {
                radius - row
            } else if row >= rect.height as i32 - radius {
                row - (rect.height as i32 - radius) + 1
            } else {
                0
            };
            let inset = corner_inset(radius, dy);
            let x0 = (rect.x + inset).max(clipped.x);
            let x1 = (rect.right_i32() - inset).min(clipped.right_i32());
            for x in x0..x1 {
                self.blend_pixel(x, y, color, alpha);
            }
        }
    }

    /// Draws a rounded rectangle outline.
    pub fn stroke_rounded_rect(&mut self, rect: Rect, radius: u32, thickness: i32, color: Color) {
        if rect.is_empty() {
            return;
        }
        let thickness = thickness.max(1);
        let max_radius = (rect.width.min(rect.height) / 2) as i32;
        let radius = (radius as i32).clamp(0, max_radius);

        for row in 0..rect.height as i32 {
            let dy = if row < radius {
                radius - row
            } else if row >= rect.height as i32 - radius {
                row - (rect.height as i32 - radius) + 1
            } else {
                0
            };
            let inset = corner_inset(radius, dy);
            let left = inset;
            let right = rect.width as i32 - inset - thickness;
            if right < left {
                continue;
            }

            if row < thickness || row >= rect.height as i32 - thickness {
                self.fill_rect(
                    Rect::new(
                        rect.x + left,
                        rect.y + row,
                        (right - left + thickness).max(0) as u32,
                        1,
                    ),
                    color,
                );
            } else {
                self.fill_rect(
                    Rect::new(rect.x + left, rect.y + row, thickness as u32, 1),
                    color,
                );
                self.fill_rect(
                    Rect::new(rect.x + right, rect.y + row, thickness as u32, 1),
                    color,
                );
            }
        }
    }

    /// Draws a soft drop shadow beneath `rect`.
    ///
    /// Only the visible ring outside `rect` is blended: filling the whole
    /// grown rectangle would touch every pixel under the panel even though
    /// the caller immediately covers it again. On a 1024x768 window that
    /// difference is millions of alpha blends per frame.
    pub fn shadow(&mut self, rect: Rect, _radius: u32, spread: i32, alpha: u8) {
        let spread = spread.max(0);
        if spread == 0 || alpha == 0 || rect.is_empty() {
            return;
        }
        for layer in (1..=3).rev() {
            let grow = spread * layer / 3;
            if grow == 0 {
                continue;
            }
            let shifted = rect.inset(-grow).translate(0, 2 + layer);
            let alpha = alpha / (layer as u8 + 1);
            if alpha == 0 {
                continue;
            }

            let left = (rect.x - shifted.x).max(0) as u32;
            let right = (shifted.right_i32() - rect.right_i32()).max(0) as u32;
            let top = (rect.y - shifted.y).max(0) as u32;
            let bottom = (shifted.bottom_i32() - rect.bottom_i32()).max(0) as u32;

            // Bottom band spans the full shadow width.
            if bottom > 0 {
                self.fill_rect_alpha(
                    Rect::new(shifted.x, rect.bottom_i32(), shifted.width, bottom),
                    Color::BLACK,
                    alpha,
                );
            }
            // Top band only exists when the shadow is not fully shifted down.
            if top > 0 {
                self.fill_rect_alpha(
                    Rect::new(shifted.x, shifted.y, shifted.width, top),
                    Color::BLACK,
                    alpha,
                );
            }
            // Left and right bands cover the remaining vertical edges.
            if left > 0 {
                self.fill_rect_alpha(
                    Rect::new(shifted.x, rect.y, left, rect.height),
                    Color::BLACK,
                    alpha,
                );
            }
            if right > 0 {
                self.fill_rect_alpha(
                    Rect::new(rect.right_i32(), rect.y, right, rect.height),
                    Color::BLACK,
                    alpha,
                );
            }
        }
    }

    /// Fills a rectangle with constant alpha.
    pub fn fill_rect_alpha(&mut self, rect: Rect, color: Color, alpha: u8) {
        if alpha == 0 {
            return;
        }
        let clipped = rect.clipped_to(self.clip);
        if clipped.is_empty() {
            return;
        }
        self.canvas.blend_rect(clipped, color, alpha);
    }

    /// Fills `rect` with a vertical gradient, clipped to the painter's clip.
    pub fn fill_vertical_gradient(&mut self, rect: Rect, top: Color, bottom: Color) {
        self.canvas
            .fill_vertical_gradient_clipped(rect, self.clip, top, bottom);
    }

    /// Copies a packed `width * height` image to `(x, y)`, clipped.
    pub fn blit(&mut self, x: i32, y: i32, width: u32, height: u32, pixels: &[u32]) {
        self.canvas
            .blit_clipped(x, y, width, height, pixels, self.clip);
    }

    /// Copies the clipped part of `rect` from a canvas-sized cached layer.
    pub fn restore_layer(&mut self, rect: Rect, pixels: &[u32], width: usize, height: usize) {
        let clipped = rect.clipped_to(self.clip);
        if !clipped.is_empty() {
            self.canvas
                .copy_rect_from_pixels(pixels, width, height, width, clipped);
        }
    }

    /// Blends one pixel.
    pub fn blend_pixel(&mut self, x: i32, y: i32, color: Color, alpha: u8) {
        if alpha == 0 || !self.clip.contains(Point::new(x, y)) {
            return;
        }
        if alpha == 255 {
            self.canvas.put_pixel(x, y, color);
            return;
        }
        if let Some(existing) = self.canvas.pixel(x, y) {
            self.canvas.put_pixel(x, y, existing.lerp(color, alpha));
        }
    }

    /// Draws one pixel, respecting the clip.
    pub fn put_pixel(&mut self, x: i32, y: i32, color: Color) {
        if self.clip.contains(Point::new(x, y)) {
            self.canvas.put_pixel(x, y, color);
        }
    }

    /// Reads a pixel, respecting the clip.
    pub fn pixel(&self, x: i32, y: i32) -> Option<Color> {
        if self.clip.contains(Point::new(x, y)) {
            self.canvas.pixel(x, y)
        } else {
            None
        }
    }

    /// Draws a filled polygon using an even-odd scanline fill.
    ///
    /// The polygon is implicitly closed. Self-intersecting polygons follow the
    /// even-odd rule. The scanline intersection buffer is fixed, so extremely
    /// complex polygons degrade to the first 128 crossings rather than
    /// allocating.
    pub fn fill_polygon(&mut self, points: &[Point], color: Color) {
        if points.len() < 3 {
            return;
        }
        let clip = self.clip;
        let mut min_y = i32::MAX;
        let mut max_y = i32::MIN;
        for point in points {
            min_y = min_y.min(point.y);
            max_y = max_y.max(point.y);
        }
        min_y = min_y.max(clip.y);
        max_y = max_y.min(clip.bottom_i32() - 1);
        let mut crossings = [0i32; 128];
        for y in min_y..=max_y {
            let scan = y as f32 + 0.5;
            let mut count = 0usize;
            for index in 0..points.len() {
                let a = points[index];
                let b = points[(index + 1) % points.len()];
                let ay = a.y as f32;
                let by = b.y as f32;
                if (ay <= scan && by > scan) || (by <= scan && ay > scan) {
                    let t = (scan - ay) / (by - ay);
                    let x = a.x as f32 + t * (b.x - a.x) as f32;
                    if count < crossings.len() {
                        crossings[count] = if x >= 0.0 { x as i32 } else { x as i32 - 1 };
                        count += 1;
                    }
                }
            }
            if count < 2 {
                continue;
            }
            let xs = &mut crossings[..count];
            // Insertion sort keeps this allocation-free and the crossing
            // counts are small for UI-style polygons.
            for i in 1..xs.len() {
                let value = xs[i];
                let mut j = i;
                while j > 0 && xs[j - 1] > value {
                    xs[j] = xs[j - 1];
                    j -= 1;
                }
                xs[j] = value;
            }
            let mut index = 0;
            while index + 1 < count {
                let x0 = xs[index].max(clip.x);
                let x1 = xs[index + 1].min(clip.right_i32());
                if x1 > x0 {
                    self.fill_rect(Rect::new(x0, y, (x1 - x0) as u32, 1), color);
                }
                index += 2;
            }
        }
    }

    /// Draws a horizontal line, clipped.
    pub fn hline(&mut self, x0: i32, x1: i32, y: i32, color: Color) {
        if x0 <= x1 {
            self.fill_rect(Rect::new(x0, y, (x1 - x0 + 1) as u32, 1), color);
        } else {
            self.fill_rect(Rect::new(x1, y, (x0 - x1 + 1) as u32, 1), color);
        }
    }

    /// Draws a vertical line, clipped.
    pub fn vline(&mut self, x: i32, y0: i32, y1: i32, color: Color) {
        if y0 <= y1 {
            self.fill_rect(Rect::new(x, y0, 1, (y1 - y0 + 1) as u32), color);
        } else {
            self.fill_rect(Rect::new(x, y1, 1, (y0 - y1 + 1) as u32), color);
        }
    }

    /// Draws a Bresenham line, clipped.
    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: Color) {
        self.canvas.line_clipped(x0, y0, x1, y1, self.clip, color);
    }

    /// Draws a one-pixel circle outline.
    pub fn circle(&mut self, cx: i32, cy: i32, radius: u32, color: Color) {
        let mut x = (radius.min(i32::MAX as u32)) as i32;
        if x == 0 {
            self.put_pixel(cx, cy, color);
            return;
        }
        let mut y = 0;
        let mut err = 1 - x;
        while x >= y {
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
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
    }

    /// Fills a circle, clipped row by row.
    pub fn fill_circle(&mut self, cx: i32, cy: i32, radius: u32, color: Color) {
        let radius = radius.min(i32::MAX as u32) as i64;
        if radius == 0 {
            self.put_pixel(cx, cy, color);
            return;
        }
        for dy in -radius..=radius {
            let dx = isqrt64((radius * radius - dy * dy) as u64) as i32;
            self.fill_rect(
                Rect::new(
                    cx.saturating_sub(dx),
                    (cy as i64 + dy).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                    (dx as u32) * 2 + 1,
                    1,
                ),
                color,
            );
        }
    }

    /// Draws one bitmap character. `scale` is clamped to 1..=16.
    pub fn draw_char(&mut self, x: i32, y: i32, ch: char, color: Color, scale: u32) {
        self.canvas
            .draw_char_clipped(x, y, ch, color, scale.clamp(1, 16), self.clip);
    }

    /// Draws text with newline, carriage-return and tab support.
    ///
    /// Returns the y coordinate immediately below the last line.
    pub fn draw_text(&mut self, x: i32, y: i32, text: &str, color: Color, scale: u32) -> i32 {
        let scale = scale.clamp(1, 16);
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
                '\t' => cx = cx.saturating_add(advance * 4),
                _ => {
                    self.draw_char(cx, cy, ch, color, scale);
                    cx = cx.saturating_add(advance);
                }
            }
        }
        cy.saturating_add(line_height)
    }

    /// Draws single-line text aligned inside `rect`.
    pub fn draw_text_aligned(
        &mut self,
        rect: Rect,
        text: &str,
        color: Color,
        scale: u32,
        align: Align,
        vertical: VerticalAlign,
    ) {
        let width = self.text_width(text, scale);
        let height = self.text_height(text, scale);
        let x = match align {
            Align::Start => rect.x,
            Align::Center => rect.x + (rect.width as i32 - width) / 2,
            Align::End => rect.x + rect.width as i32 - width,
        };
        let y = match vertical {
            VerticalAlign::Top => rect.y,
            VerticalAlign::Middle => rect.y + (rect.height as i32 - height) / 2,
            VerticalAlign::Bottom => rect.y + rect.height as i32 - height,
        };
        self.draw_text(x, y, text, color, scale);
    }

    /// Draws text centered horizontally and vertically.
    pub fn draw_text_centered(&mut self, rect: Rect, text: &str, color: Color, scale: u32) {
        self.draw_text_aligned(
            rect,
            text,
            color,
            scale,
            Align::Center,
            VerticalAlign::Middle,
        );
    }

    /// Draws a single line, truncating with `...` when it does not fit.
    pub fn draw_text_ellipsis(
        &mut self,
        rect: Rect,
        text: &str,
        color: Color,
        scale: u32,
        align: Align,
        vertical: VerticalAlign,
    ) {
        let line = text.split('\n').next().unwrap_or("");
        let width = self.text_width(line, scale);
        if width <= rect.width as i32 {
            self.draw_text_aligned(rect, line, color, scale, align, vertical);
            return;
        }

        let scale = scale.clamp(1, 16);
        let advance = font::GLYPH_WIDTH as i32 * scale as i32;
        let ellipsis_width = advance * 3;
        let budget = (rect.width as i32 - ellipsis_width).max(0);
        let mut used = 0;
        let mut end = 0;
        for (index, ch) in line.char_indices() {
            let char_width = if ch == '\t' { advance * 4 } else { advance };
            if used + char_width > budget {
                break;
            }
            used += char_width;
            end = index + ch.len_utf8();
        }

        let total = used + ellipsis_width;
        let x = match align {
            Align::Start => rect.x,
            Align::Center => rect.x + (rect.width as i32 - total) / 2,
            Align::End => rect.x + rect.width as i32 - total,
        };
        let height = font::GLYPH_HEIGHT as i32 * scale as i32;
        let y = match vertical {
            VerticalAlign::Top => rect.y,
            VerticalAlign::Middle => rect.y + (rect.height as i32 - height) / 2,
            VerticalAlign::Bottom => rect.y + rect.height as i32 - height,
        };
        self.draw_text(x, y, &line[..end], color, scale);
        self.draw_text(x + used, y, "...", color, scale);
    }

    /// Draws word-wrapped text and returns the used height.
    ///
    /// Wrapping is left-aligned. Single-line alignment is available through
    /// [`draw_text_aligned`](Painter::draw_text_aligned).
    pub fn draw_text_wrapped(&mut self, rect: Rect, text: &str, color: Color, scale: u32) -> i32 {
        let scale = scale.clamp(1, 16);
        let line_height = font::GLYPH_HEIGHT as i32 * scale as i32;
        let advance = font::GLYPH_WIDTH as i32 * scale as i32;
        let mut y = rect.y;

        for paragraph in text.split('\n') {
            let mut x = rect.x;
            let mut has_word = false;
            for word in paragraph.split_whitespace() {
                let word_width = self.text_width(word, scale);
                let needed = if has_word {
                    advance + word_width
                } else {
                    word_width
                };
                if has_word && x + needed > rect.right_i32() {
                    y = y.saturating_add(line_height);
                    x = rect.x;
                    has_word = false;
                }
                if has_word {
                    x += advance;
                }
                self.draw_text(x, y, word, color, scale);
                x += word_width;
                has_word = true;
            }
            y = y.saturating_add(line_height);
        }

        (y - rect.y).max(0)
    }

    /// Width of the widest line in `text`, in pixels.
    pub fn text_width(&self, text: &str, scale: u32) -> i32 {
        let scale = scale.clamp(1, 16);
        let advance = font::GLYPH_WIDTH as i32 * scale as i32;
        text.lines()
            .map(|line| {
                line.chars()
                    .map(|ch| if ch == '\t' { advance * 4 } else { advance })
                    .sum::<i32>()
            })
            .max()
            .unwrap_or(0)
    }

    /// Height of `text`, including newlines, in pixels.
    pub fn text_height(&self, text: &str, scale: u32) -> i32 {
        let scale = scale.clamp(1, 16);
        let line_height = font::GLYPH_HEIGHT as i32 * scale as i32;
        let lines = text.bytes().filter(|byte| *byte == b'\n').count() as i32 + 1;
        lines * line_height
    }

    /// Height of `text` when wrapped to `width` pixels.
    pub fn wrapped_text_height(&self, width: i32, text: &str, scale: u32) -> i32 {
        let scale = scale.clamp(1, 16);
        let line_height = font::GLYPH_HEIGHT as i32 * scale as i32;
        let advance = font::GLYPH_WIDTH as i32 * scale as i32;
        let width = width.max(1);
        let mut lines = 0;

        for paragraph in text.split('\n') {
            lines += 1;
            let mut x = 0;
            let mut has_word = false;
            for word in paragraph.split_whitespace() {
                let word_width = self.text_width(word, scale);
                let needed = if has_word {
                    advance + word_width
                } else {
                    word_width
                };
                if has_word && x + needed > width {
                    lines += 1;
                    x = 0;
                    has_word = false;
                }
                if has_word {
                    x += advance;
                }
                x += word_width;
                has_word = true;
            }
        }

        lines * line_height
    }

    /// A single line's height in pixels.
    pub fn line_height(&self, scale: u32) -> i32 {
        font::GLYPH_HEIGHT as i32 * scale.clamp(1, 16) as i32
    }
}

fn corner_inset(radius: i32, dy: i32) -> i32 {
    if radius <= 0 {
        return 0;
    }
    let radius = radius as i64;
    let dy = dy.clamp(0, radius as i32) as i64;
    let dx = isqrt64((radius * radius - dy * dy).max(0) as u64);
    (radius - dx as i64).clamp(0, radius) as i32
}

fn isqrt64(value: u64) -> u64 {
    if value < 2 {
        return value;
    }
    let mut x = value;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = (x + value / x) / 2;
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
    fn clipping_contains_all_drawing() {
        let mut pixels = [0u32; 20 * 20];
        let mut canvas = canvas(&mut pixels, 20, 20);
        {
            let mut painter = Painter::new(&mut canvas);
            let old = painter.push_clip(Rect::new(5, 5, 5, 5));
            painter.fill_rect(Rect::new(0, 0, 20, 20), Color::RED);
            painter.line(0, 0, 19, 19, Color::GREEN);
            painter.restore_clip(old);
        }

        assert_eq!(canvas.pixel(6, 5), Some(Color::RED));
        assert_eq!(canvas.pixel(4, 5), Some(Color::BLACK));
        assert_eq!(canvas.pixel(10, 5), Some(Color::BLACK));

        {
            let mut painter = Painter::new(&mut canvas);
            let old = painter.push_clip(Rect::new(1, 1, 5, 5));
            painter.draw_text(0, 0, "XX", Color::WHITE, 2);
            painter.restore_clip(old);
        }
        assert_eq!(canvas.pixel(0, 0), Some(Color::BLACK));
        assert_eq!(canvas.pixel(6, 5), Some(Color::RED));
    }

    #[test]
    fn rounded_rect_removes_corners() {
        let mut pixels = [0u32; 20 * 20];
        let mut canvas = canvas(&mut pixels, 20, 20);
        let mut painter = Painter::new(&mut canvas);
        painter.fill_rounded_rect(Rect::new(0, 0, 20, 20), 6, Color::RED);

        assert_eq!(canvas.pixel(10, 10), Some(Color::RED));
        assert_eq!(canvas.pixel(0, 0), Some(Color::BLACK));
        assert_eq!(canvas.pixel(19, 0), Some(Color::BLACK));
        assert_eq!(canvas.pixel(0, 19), Some(Color::BLACK));
        assert_eq!(canvas.pixel(10, 0), Some(Color::RED));
    }

    #[test]
    fn text_measurement_and_ellipsis() {
        let mut pixels = [0u32; 64 * 20];
        let mut canvas = canvas(&mut pixels, 64, 20);
        let mut painter = Painter::new(&mut canvas);
        assert_eq!(painter.text_width("AB", 1), 16);
        assert_eq!(painter.text_height("A\nB", 2), 32);

        painter.draw_text_ellipsis(
            Rect::new(0, 0, 32, 8),
            "abcdefgh",
            Color::WHITE,
            1,
            Align::Start,
            VerticalAlign::Top,
        );
        assert!(pixels.iter().any(|pixel| *pixel == Color::WHITE.raw()));
    }

    #[test]
    fn shadow_is_translucent() {
        let mut pixels = [0u32; 30 * 30];
        let mut canvas = canvas(&mut pixels, 30, 30);
        canvas.clear(Color::WHITE);
        let mut painter = Painter::new(&mut canvas);
        painter.shadow(Rect::new(6, 6, 18, 18), 4, 4, 160);
        assert_ne!(canvas.pixel(15, 27), Some(Color::WHITE));
        // The shadow only paints the visible ring; the panel covers the
        // interior, so blending it would be wasted work.
        assert_eq!(canvas.pixel(15, 15), Some(Color::WHITE));
    }

    #[test]
    fn polygons_fill_their_interior_and_respect_clipping() {
        let mut pixels = [0u32; 20 * 20];
        let mut canvas = canvas(&mut pixels, 20, 20);
        let mut painter = Painter::new(&mut canvas);
        painter.fill_polygon(
            &[Point::new(2, 2), Point::new(18, 2), Point::new(10, 18)],
            Color::RED,
        );
        assert_eq!(canvas.pixel(10, 6), Some(Color::RED));
        assert_eq!(canvas.pixel(3, 16), Some(Color::BLACK));
        assert_eq!(canvas.pixel(1, 1), Some(Color::BLACK));
    }
}
