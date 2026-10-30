//! Additional chart types: pie/donut, scatter, radar, heatmaps, funnels,
//! bullet graphs, treemaps and candlesticks.
//!
//! The basic line/area/bar charts live in [`crate::chart`]. These components
//! follow the same rules: caller-owned data, no allocation and immediate-mode
//! painting into a clipped `Painter`.

use feos_gfx::{Color, Point, Rect};

use crate::geometry::{Align, RectExt, VerticalAlign};
use crate::math::{cos, sin};
use crate::response::Response;
use crate::text::format_f32;
use crate::ui::Ui;

/// One slice of a pie or donut chart.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PieSlice<'a> {
    /// Relative value. Slices are normalised against the sum of all values.
    pub value: f32,
    /// Fill colour.
    pub color: Color,
    /// Optional legend label.
    pub label: &'a str,
}

impl<'a> PieSlice<'a> {
    pub const fn new(value: f32, color: Color) -> Self {
        Self {
            value,
            color,
            label: "",
        }
    }

    pub const fn labeled(value: f32, color: Color, label: &'a str) -> Self {
        Self {
            value,
            color,
            label,
        }
    }
}

/// One point in a [`Ui::scatter_chart`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScatterPoint {
    pub x: f32,
    pub y: f32,
}

impl ScatterPoint {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// One series in a [`Ui::radar_chart`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadarSeries<'a> {
    /// Series name, reserved for legends.
    pub label: &'a str,
    /// Values in `0..=1`, one per axis.
    pub values: &'a [f32],
    /// Line and fill colour.
    pub color: Color,
}

impl<'a> RadarSeries<'a> {
    pub const fn new(values: &'a [f32], color: Color) -> Self {
        Self {
            label: "",
            values,
            color,
        }
    }

    pub const fn labeled(label: &'a str, values: &'a [f32], color: Color) -> Self {
        Self {
            label,
            values,
            color,
        }
    }
}

/// One stage of a [`Ui::funnel_chart`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FunnelStage<'a> {
    pub label: &'a str,
    pub value: f32,
    pub color: Color,
}

impl<'a> FunnelStage<'a> {
    pub const fn new(label: &'a str, value: f32, color: Color) -> Self {
        Self {
            label,
            value,
            color,
        }
    }
}

/// One tile of a [`Ui::treemap_chart`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TreemapItem<'a> {
    pub label: &'a str,
    pub value: f32,
    pub color: Color,
}

impl<'a> TreemapItem<'a> {
    pub const fn new(label: &'a str, value: f32, color: Color) -> Self {
        Self {
            label,
            value,
            color,
        }
    }
}

/// One OHLC candle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candle {
    pub open: f32,
    pub high: f32,
    pub low: f32,
    pub close: f32,
}

impl Candle {
    pub const fn new(open: f32, high: f32, low: f32, close: f32) -> Self {
        Self {
            open,
            high,
            low,
            close,
        }
    }
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A pie chart. Draw a legend on the right when slices have labels.
    pub fn pie_chart(&mut self, rect: Rect, slices: &[PieSlice<'_>]) -> Response {
        self.pie_or_donut(rect, slices, 0.0)
    }

    /// A donut chart. Draw a legend on the right when slices have labels.
    pub fn donut_chart(&mut self, rect: Rect, slices: &[PieSlice<'_>]) -> Response {
        self.pie_or_donut(rect, slices, 0.55)
    }

    fn pie_or_donut(&mut self, rect: Rect, slices: &[PieSlice<'_>], hole: f32) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let mut response = Response::empty(id, rect);
        if rect.is_empty() {
            return response;
        }
        let has_labels = slices.iter().any(|slice| !slice.label.is_empty());
        let legend_width = if has_labels {
            150.min(rect.width as i32 / 2)
        } else {
            0
        };
        let chart = Rect::new(
            rect.x,
            rect.y,
            rect.width.saturating_sub(legend_width as u32),
            rect.height,
        );
        let radius = (chart.width.min(chart.height) as i32 / 2 - 4).max(1);
        let center = chart.center();
        let total: f32 = slices.iter().map(|slice| slice.value.max(0.0)).sum::<f32>();
        if total <= 0.0 {
            self.painter
                .circle(center.x, center.y, radius as u32, palette.border);
        } else {
            let mut angle = -core::f32::consts::PI / 2.0;
            for slice in slices {
                let value = slice.value.max(0.0);
                if value <= 0.0 {
                    continue;
                }
                let sweep = value / total * core::f32::consts::PI * 2.0;
                self.fill_ring_sector(center, radius, hole, angle, angle + sweep, slice.color);
                angle += sweep;
            }
        }
        if has_labels {
            let legend = Rect::new(
                rect.right_i32() - legend_width + 8,
                rect.y + 4,
                (legend_width - 12).max(0) as u32,
                rect.height,
            );
            self.pie_legend(legend, slices, total);
        }
        response.changed = false;
        let _ = metrics;
        response
    }

    fn pie_legend(&mut self, rect: Rect, slices: &[PieSlice<'_>], total: f32) {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let row = (metrics.small_scale as i32 * 11 + 6).max(18);
        let mut y = rect.y;
        for slice in slices {
            if y + row > rect.bottom_i32() {
                break;
            }
            self.painter.fill_rounded_rect(
                Rect::new(rect.x, y + (row - 8) / 2, 8, 8),
                2,
                slice.color,
            );
            let mut percent = [0u8; 16];
            let value = if total > 0.0 {
                slice.value.max(0.0) / total * 100.0
            } else {
                0.0
            };
            let label = format_f32(value, 0, &mut percent);
            self.painter.draw_text_ellipsis(
                Rect::new(rect.x + 14, y, rect.width.saturating_sub(52), row as u32),
                slice.label,
                palette.text,
                metrics.small_scale,
                Align::Start,
                VerticalAlign::Middle,
            );
            self.painter.draw_text_ellipsis(
                Rect::new(rect.right_i32() - 40, y, 40, row as u32),
                label,
                palette.text_muted,
                metrics.small_scale,
                Align::End,
                VerticalAlign::Middle,
            );
            y += row;
        }
    }

    fn fill_ring_sector(
        &mut self,
        center: Point,
        radius: i32,
        hole: f32,
        start: f32,
        end: f32,
        color: Color,
    ) {
        const STEPS: usize = 24;
        let inner = (radius as f32 * hole.clamp(0.0, 0.95)) as i32;
        let mut points = [Point::new(0, 0); (STEPS + 1) * 2];
        let mut index = 0;
        for step in 0..=STEPS {
            let angle = start + (end - start) * step as f32 / STEPS as f32;
            points[index] = Point::new(
                center.x + (cos(angle) * radius as f32) as i32,
                center.y + (sin(angle) * radius as f32) as i32,
            );
            index += 1;
        }
        if inner > 0 {
            for step in (0..=STEPS).rev() {
                let angle = start + (end - start) * step as f32 / STEPS as f32;
                points[index] = Point::new(
                    center.x + (cos(angle) * inner as f32) as i32,
                    center.y + (sin(angle) * inner as f32) as i32,
                );
                index += 1;
            }
        } else {
            points[index] = center;
            index += 1;
        }
        self.painter.fill_polygon(&points[..index], color);
    }

    /// A scatter plot with caller-supplied axis ranges.
    pub fn scatter_chart(
        &mut self,
        rect: Rect,
        points: &[ScatterPoint],
        x_range: (f32, f32),
        y_range: (f32, f32),
        color: Color,
    ) -> Response {
        let palette = self.theme.palette;
        let id = self.next_id();
        let response = Response::empty(id, rect);
        if rect.is_empty() {
            return response;
        }
        let pad = 10;
        let plot = Rect::new(
            rect.x + pad,
            rect.y + pad,
            rect.width.saturating_sub((pad * 2) as u32),
            rect.height.saturating_sub((pad * 2) as u32),
        );
        self.painter.stroke_rect(plot, 1, palette.chart_grid);
        let (x0, x1) = normal_range(x_range);
        let (y0, y1) = normal_range(y_range);
        for point in points {
            let fx = ((point.x - x0) / (x1 - x0)).clamp(0.0, 1.0);
            let fy = ((point.y - y0) / (y1 - y0)).clamp(0.0, 1.0);
            let x = plot.x + (plot.width as f32 * fx) as i32;
            let y = plot.bottom_i32() - (plot.height as f32 * fy) as i32;
            self.painter.fill_circle(x, y, 3, color);
            self.painter
                .circle(x, y, 4, color.lerp(palette.surface, 100));
        }
        response
    }

    /// A radar/spider chart. Values are normalised to `0..=1`.
    pub fn radar_chart(
        &mut self,
        rect: Rect,
        axes: &[&str],
        series: &[RadarSeries<'_>],
    ) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let response = Response::empty(id, rect);
        if rect.is_empty() || axes.len() < 3 {
            return response;
        }
        let center = rect.center();
        let radius = (rect.width.min(rect.height) as i32 / 2 - 18).max(4);
        let count = axes.len();
        for ring in 1..=4 {
            let scale = ring as f32 / 4.0;
            let mut points = [Point::new(0, 0); 12];
            for (index, axis) in (0..count.min(points.len())).enumerate() {
                let angle = -core::f32::consts::PI / 2.0
                    + axis as f32 * core::f32::consts::PI * 2.0 / count as f32;
                points[index] = Point::new(
                    center.x + (cos(angle) * radius as f32 * scale) as i32,
                    center.y + (sin(angle) * radius as f32 * scale) as i32,
                );
            }
            for index in 0..count.min(12) {
                let a = points[index];
                let b = points[(index + 1) % count.min(12)];
                self.painter.line(a.x, a.y, b.x, b.y, palette.chart_grid);
            }
        }
        for (axis, label) in axes.iter().enumerate().take(12) {
            let angle = -core::f32::consts::PI / 2.0
                + axis as f32 * core::f32::consts::PI * 2.0 / count as f32;
            let x = center.x + (cos(angle) * radius as f32) as i32;
            let y = center.y + (sin(angle) * radius as f32) as i32;
            self.painter
                .line(center.x, center.y, x, y, palette.chart_grid);
            self.painter.draw_text_ellipsis(
                Rect::new(x - 30, y - 8, 60, 16),
                label,
                palette.text_muted,
                metrics.small_scale,
                Align::Center,
                VerticalAlign::Middle,
            );
        }
        for item in series {
            let mut points = [Point::new(0, 0); 12];
            let mut index = 0;
            for axis in 0..count.min(12) {
                let value = item
                    .values
                    .get(axis)
                    .copied()
                    .unwrap_or(0.0)
                    .clamp(0.0, 1.0);
                let angle = -core::f32::consts::PI / 2.0
                    + axis as f32 * core::f32::consts::PI * 2.0 / count as f32;
                points[index] = Point::new(
                    center.x + (cos(angle) * radius as f32 * value) as i32,
                    center.y + (sin(angle) * radius as f32 * value) as i32,
                );
                index += 1;
            }
            self.painter
                .fill_polygon(&points[..index], item.color.lerp(palette.surface, 70));
            for point in points.iter().take(index) {
                self.painter.fill_circle(point.x, point.y, 3, item.color);
            }
            for index in 0..count.min(12) {
                let a = points[index];
                let b = points[(index + 1) % count.min(12)];
                self.painter.line(a.x, a.y, b.x, b.y, item.color);
            }
        }
        response
    }

    /// A heatmap. `values` are row-major and `min..max` maps to the two
    /// endpoint colours.
    #[allow(clippy::too_many_arguments)]
    pub fn heatmap(
        &mut self,
        rect: Rect,
        columns: u32,
        values: &[f32],
        min: f32,
        max: f32,
        low: Color,
        high: Color,
    ) -> Response {
        let palette = self.theme.palette;
        let id = self.next_id();
        let response = Response::empty(id, rect);
        if rect.is_empty() || columns == 0 || values.is_empty() {
            return response;
        }
        let rows = values.len().div_ceil(columns.max(1) as usize) as u32;
        let cell_w = rect.width as i32 / columns as i32;
        let cell_h = rect.height as i32 / rows.max(1) as i32;
        let range = (max - min).abs().max(f32::EPSILON);
        for (index, value) in values.iter().enumerate() {
            let column = index as u32 % columns;
            let row = index as u32 / columns;
            let t = ((value - min) / range).clamp(0.0, 1.0);
            let cell = Rect::new(
                rect.x + column as i32 * cell_w,
                rect.y + row as i32 * cell_h,
                cell_w.max(0) as u32,
                cell_h.max(0) as u32,
            );
            self.painter
                .fill_rect(cell.inset(1), low.lerp(high, (t * 255.0) as u8));
        }
        self.painter.stroke_rect(rect, 1, palette.border);
        response
    }

    /// A funnel chart. Values are normalised against the largest stage.
    pub fn funnel_chart(&mut self, rect: Rect, stages: &[FunnelStage<'_>]) -> Response {
        let palette = self.theme.palette;
        let id = self.next_id();
        let response = Response::empty(id, rect);
        if rect.is_empty() || stages.is_empty() {
            return response;
        }
        let max = stages
            .iter()
            .map(|stage| stage.value.max(0.0))
            .fold(0.0, f32::max)
            .max(f32::EPSILON);
        let band = rect.height as i32 / stages.len() as i32;
        for (index, stage) in stages.iter().enumerate() {
            let top_width = (rect.width as f32 * (stage.value.max(0.0) / max)) as i32;
            let next = stages
                .get(index + 1)
                .map(|next| (rect.width as f32 * (next.value.max(0.0) / max)) as i32)
                .unwrap_or(top_width / 2);
            let y0 = rect.y + index as i32 * band;
            let y1 = y0 + band;
            let cx = rect.center().x;
            let points = [
                Point::new(cx - top_width / 2, y0 + 1),
                Point::new(cx + top_width / 2, y0 + 1),
                Point::new(cx + next / 2, y1 - 1),
                Point::new(cx - next / 2, y1 - 1),
            ];
            self.painter.fill_polygon(&points, stage.color);
            self.painter.draw_text_ellipsis(
                Rect::new(rect.x, y0, rect.width, band as u32),
                stage.label,
                palette.background,
                self.theme.metrics.small_scale,
                Align::Center,
                VerticalAlign::Middle,
            );
        }
        response
    }

    /// A bullet graph: one value bar, a target marker and a labelled scale.
    pub fn bullet_chart(
        &mut self,
        rect: Rect,
        value: f32,
        target: f32,
        max: f32,
        label: &str,
    ) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let response = Response::empty(id, rect);
        if rect.is_empty() {
            return response;
        }
        let max = max.abs().max(f32::EPSILON);
        let label_height = self.painter.line_height(metrics.small_scale);
        self.painter.draw_text_ellipsis(
            Rect::new(rect.x, rect.y, rect.width, label_height as u32),
            label,
            palette.text,
            metrics.small_scale,
            Align::Start,
            VerticalAlign::Top,
        );
        let bar = Rect::new(
            rect.x,
            rect.y + label_height + 4,
            rect.width,
            (rect.height as i32 - label_height - 8).max(6) as u32,
        );
        self.painter
            .fill_rounded_rect(bar, 3, palette.surface_active);
        let fraction = (value / max).clamp(0.0, 1.0);
        self.painter.fill_rounded_rect(
            Rect::new(
                bar.x,
                bar.y,
                (bar.width as f32 * fraction) as u32,
                bar.height,
            ),
            3,
            palette.accent,
        );
        let target_x = bar.x + (bar.width as f32 * (target / max).clamp(0.0, 1.0)) as i32;
        self.painter.fill_rect(
            Rect::new(target_x - 1, bar.y - 2, 3, bar.height + 4),
            palette.text,
        );
        response
    }

    /// A simple proportional treemap. Items are laid out as vertical strips
    /// from left to right, which keeps the component deterministic and
    /// allocation-free.
    pub fn treemap_chart(&mut self, rect: Rect, items: &[TreemapItem<'_>]) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let id = self.next_id();
        let response = Response::empty(id, rect);
        if rect.is_empty() || items.is_empty() {
            return response;
        }
        let total: f32 = items.iter().map(|item| item.value.max(0.0)).sum();
        if total <= 0.0 {
            return response;
        }
        let mut x = rect.x;
        for (index, item) in items.iter().enumerate() {
            let last = index + 1 == items.len();
            let width = if last {
                rect.right_i32() - x
            } else {
                (rect.width as f32 * (item.value.max(0.0) / total)) as i32
            };
            let tile = Rect::new(x, rect.y, width.max(0) as u32, rect.height);
            if tile.width > 2 {
                self.painter.fill_rect(tile.inset(1), item.color);
                self.painter.draw_text_ellipsis(
                    Rect::new(
                        tile.x + 4,
                        tile.y + 4,
                        tile.width.saturating_sub(8),
                        metrics.small_scale * 10,
                    ),
                    item.label,
                    palette.background,
                    metrics.small_scale,
                    Align::Start,
                    VerticalAlign::Top,
                );
            }
            x += width + 1;
        }
        response
    }

    /// An OHLC candlestick chart.
    pub fn candlestick_chart(&mut self, rect: Rect, candles: &[Candle]) -> Response {
        let palette = self.theme.palette;
        let id = self.next_id();
        let response = Response::empty(id, rect);
        if rect.is_empty() || candles.is_empty() {
            return response;
        }
        let mut low = f32::INFINITY;
        let mut high = f32::NEG_INFINITY;
        for candle in candles {
            low = low.min(candle.low);
            high = high.max(candle.high);
        }
        let range = (high - low).max(f32::EPSILON);
        let slot = rect.width as i32 / candles.len() as i32;
        let body_width = (slot / 2).max(1);
        let y_of =
            |value: f32| rect.bottom_i32() - ((value - low) / range * rect.height as f32) as i32;
        for (index, candle) in candles.iter().enumerate() {
            let cx = rect.x + index as i32 * slot + slot / 2;
            let color = if candle.close >= candle.open {
                palette.success
            } else {
                palette.danger
            };
            self.painter
                .line(cx, y_of(candle.high), cx, y_of(candle.low), color);
            let top = y_of(candle.open.max(candle.close));
            let bottom = y_of(candle.open.min(candle.close));
            self.painter.fill_rect(
                Rect::new(
                    cx - body_width / 2,
                    top,
                    body_width.max(1) as u32,
                    (bottom - top).max(1) as u32,
                ),
                color,
            );
        }
        response
    }
}

fn normal_range(range: (f32, f32)) -> (f32, f32) {
    let (a, b) = range;
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}
