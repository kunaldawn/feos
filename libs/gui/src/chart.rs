//! Line, area, bar and sparkline charts.
//!
//! Charts take slices of points; they never allocate and never retain data.
//! Values outside the configured range are clipped to the plot area.

use feos_gfx::{Color, Rect};

use crate::geometry::{Align, RectExt, VerticalAlign};
use crate::text::format_f32;
use crate::ui::Ui;

/// A point in data space.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ChartPoint {
    pub x: f32,
    pub y: f32,
}

impl ChartPoint {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// How a series is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeriesKind {
    Line,
    Area,
    Bars,
}

/// One data series.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Series<'a> {
    pub points: &'a [ChartPoint],
    pub color: Color,
    pub kind: SeriesKind,
}

impl<'a> Series<'a> {
    /// A line series.
    pub const fn line(points: &'a [ChartPoint], color: Color) -> Self {
        Self {
            points,
            color,
            kind: SeriesKind::Line,
        }
    }

    /// A filled area series.
    pub const fn area(points: &'a [ChartPoint], color: Color) -> Self {
        Self {
            points,
            color,
            kind: SeriesKind::Area,
        }
    }

    /// A bar series.
    pub const fn bars(points: &'a [ChartPoint], color: Color) -> Self {
        Self {
            points,
            color,
            kind: SeriesKind::Bars,
        }
    }

    /// Changes the series kind.
    pub const fn with_kind(mut self, kind: SeriesKind) -> Self {
        self.kind = kind;
        self
    }
}

/// A chart description.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chart<'a> {
    pub series: &'a [Series<'a>],
    pub y_min: f32,
    pub y_max: f32,
    pub x_min: f32,
    pub x_max: f32,
    pub grid_lines: u8,
    pub show_axes: bool,
    pub show_labels: bool,
}

impl<'a> Chart<'a> {
    /// Creates a chart with a 0..=1 range.
    pub const fn new(series: &'a [Series<'a>]) -> Self {
        Self {
            series,
            y_min: 0.0,
            y_max: 1.0,
            x_min: 0.0,
            x_max: 1.0,
            grid_lines: 4,
            show_axes: true,
            show_labels: true,
        }
    }

    /// Sets the Y range.
    pub const fn y_range(mut self, min: f32, max: f32) -> Self {
        self.y_min = min;
        self.y_max = max;
        self
    }

    /// Sets the X range.
    pub const fn x_range(mut self, min: f32, max: f32) -> Self {
        self.x_min = min;
        self.x_max = max;
        self
    }

    /// Sets the number of grid lines per axis.
    pub const fn grid(mut self, lines: u8) -> Self {
        self.grid_lines = lines;
        self
    }

    /// Shows or hides axes and labels.
    pub const fn axes(mut self, show: bool) -> Self {
        self.show_axes = show;
        self.show_labels = show;
        self
    }
}

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// Draws a chart into `rect`.
    pub fn chart(&mut self, rect: Rect, chart: &Chart<'_>) {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        self.painter
            .fill_rounded_rect(rect, metrics.radius, palette.surface);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            metrics.border_width,
            palette.border,
        );

        let padding = if chart.show_labels {
            RectExt::inset_each(rect, 44, 12, 12, 22)
        } else {
            rect.inset(10)
        };
        let plot = padding;
        if plot.is_empty() {
            return;
        }

        let y_min = chart.y_min;
        let y_max = chart.y_max;
        let x_min = chart.x_min;
        let x_max = chart.x_max;

        let old_clip = self.painter.push_clip(plot);
        let grid = chart.grid_lines.max(1) as i32;
        for index in 0..=grid {
            let y = plot.y + plot.height as i32 * index / grid;
            self.painter
                .fill_rect(Rect::new(plot.x, y, plot.width, 1), palette.chart_grid);
        }
        for index in 0..=grid {
            let x = plot.x + plot.width as i32 * index / grid;
            self.painter
                .fill_rect(Rect::new(x, plot.y, 1, plot.height), palette.chart_grid);
        }

        for series in chart.series {
            if series.points.is_empty() {
                continue;
            }
            match series.kind {
                SeriesKind::Bars => {
                    let count = series.points.len();
                    let slot = plot.width as f32 / count as f32;
                    let bar_width = (slot - 4.0).max(1.0);
                    for point in series.points {
                        let x = map_x(point.x, x_min, x_max, plot) as f32;
                        let y = map_y(point.y, y_min, y_max, plot);
                        let top = y.min(plot.bottom_i32());
                        let height = (plot.bottom_i32() - top).max(0) as u32;
                        let bar =
                            Rect::new((x - bar_width / 2.0) as i32, top, bar_width as u32, height);
                        self.painter.fill_rounded_rect(bar, 2, series.color);
                    }
                }
                SeriesKind::Area => {
                    for pair in series.points.windows(2) {
                        let x0 = map_x(pair[0].x, x_min, x_max, plot);
                        let x1 = map_x(pair[1].x, x_min, x_max, plot);
                        let y0 = map_y(pair[0].y, y_min, y_max, plot);
                        let y1 = map_y(pair[1].y, y_min, y_max, plot);
                        if x1 >= x0 {
                            for x in x0..=x1 {
                                let t = if x1 == x0 {
                                    0.0
                                } else {
                                    (x - x0) as f32 / (x1 - x0) as f32
                                };
                                let y = (y0 as f32 + (y1 - y0) as f32 * t) as i32;
                                self.painter.fill_rect_alpha(
                                    Rect::new(x, y, 1, (plot.bottom_i32() - y).max(0) as u32),
                                    series.color,
                                    64,
                                );
                            }
                        }
                    }
                    for pair in series.points.windows(2) {
                        self.painter.line(
                            map_x(pair[0].x, x_min, x_max, plot),
                            map_y(pair[0].y, y_min, y_max, plot),
                            map_x(pair[1].x, x_min, x_max, plot),
                            map_y(pair[1].y, y_min, y_max, plot),
                            series.color,
                        );
                    }
                }
                SeriesKind::Line => {
                    for pair in series.points.windows(2) {
                        self.painter.line(
                            map_x(pair[0].x, x_min, x_max, plot),
                            map_y(pair[0].y, y_min, y_max, plot),
                            map_x(pair[1].x, x_min, x_max, plot),
                            map_y(pair[1].y, y_min, y_max, plot),
                            series.color,
                        );
                    }
                }
            }
        }
        self.painter.restore_clip(old_clip);

        if chart.show_axes {
            self.painter.fill_rect(
                Rect::new(plot.x, plot.bottom_i32() - 1, plot.width, 1),
                palette.chart_axis,
            );
            self.painter.fill_rect(
                Rect::new(plot.x, plot.y, 1, plot.height),
                palette.chart_axis,
            );
        }

        if chart.show_labels {
            let scale = metrics.small_scale;
            for index in 0..=grid {
                let (value, line) = y_tick(y_min, y_max, plot, index, grid);
                let mut buffer = [0u8; 24];
                let label = format_f32(value, 1, &mut buffer);
                let y = line - 4;
                self.painter.draw_text_aligned(
                    Rect::new(rect.x + 4, y, 36, 8),
                    label,
                    palette.chart_axis,
                    scale,
                    Align::End,
                    VerticalAlign::Top,
                );
            }

            let mut buffer = [0u8; 24];
            let label = format_f32(x_min, 1, &mut buffer);
            self.painter.draw_text(
                plot.x,
                plot.bottom_i32() + 6,
                label,
                palette.chart_axis,
                scale,
            );
            let mut buffer = [0u8; 24];
            let label = format_f32(x_max, 1, &mut buffer);
            let width = self.painter.text_width(label, scale);
            self.painter.draw_text(
                plot.right_i32() - width,
                plot.bottom_i32() + 6,
                label,
                palette.chart_axis,
                scale,
            );
        }
    }

    /// Draws a simple line chart from evenly spaced Y values.
    pub fn line_chart(&mut self, rect: Rect, values: &[f32], color: Color) {
        if values.is_empty() {
            return;
        }
        let (min, max) = value_range(values);
        let count = values.len();
        let old_clip = self.painter.push_clip(rect);
        let mut previous: Option<(i32, i32)> = None;
        for (index, value) in values.iter().enumerate() {
            let x = rect.x
                + if count > 1 {
                    rect.width as i32 * index as i32 / (count as i32 - 1)
                } else {
                    rect.width as i32 / 2
                };
            let y = map_y(*value, min, max, rect);
            if let Some((px, py)) = previous {
                self.painter.line(px, py, x, y, color);
            }
            previous = Some((x, y));
        }
        self.painter.restore_clip(old_clip);
    }

    /// Draws a bar chart from evenly spaced Y values.
    pub fn bar_chart(&mut self, rect: Rect, values: &[f32], color: Color) {
        if values.is_empty() {
            return;
        }
        let (min, max) = value_range(values);
        let old_clip = self.painter.push_clip(rect);
        let slot = rect.width as f32 / values.len() as f32;
        let bar_width = (slot - 3.0).max(1.0);
        for (index, value) in values.iter().enumerate() {
            let x = rect.x as f32 + slot * (index as f32 + 0.5);
            let y = map_y(*value, min, max, rect);
            let top = y.min(rect.bottom_i32());
            let bar = Rect::new(
                (x - bar_width / 2.0) as i32,
                top,
                bar_width as u32,
                (rect.bottom_i32() - top).max(0) as u32,
            );
            self.painter.fill_rounded_rect(bar, 2, color);
        }
        self.painter.restore_clip(old_clip);
    }

    /// Draws a compact line without axes or background.
    pub fn sparkline(&mut self, rect: Rect, values: &[f32], color: Color) {
        if values.len() < 2 {
            return;
        }
        let (min, max) = value_range(values);
        let old_clip = self.painter.push_clip(rect);
        let count = values.len();
        let mut previous = (rect.x, map_y(values[0], min, max, rect));
        for (index, value) in values.iter().enumerate().skip(1) {
            let x = rect.x + rect.width as i32 * index as i32 / (count as i32 - 1);
            let y = map_y(*value, min, max, rect);
            self.painter.line(previous.0, previous.1, x, y, color);
            previous = (x, y);
        }
        self.painter.restore_clip(old_clip);
    }
}

fn value_range(values: &[f32]) -> (f32, f32) {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for value in values {
        if value.is_finite() {
            min = min.min(*value);
            max = max.max(*value);
        }
    }
    if !min.is_finite() || !max.is_finite() {
        return (0.0, 1.0);
    }
    if (max - min).abs() < f32::EPSILON {
        return (min - 1.0, max + 1.0);
    }
    (min, max)
}

/// The value and pixel row of the y-axis grid line `index` of `grid`, counted
/// from the top of `plot`. The top line is `max` and the bottom one `min`, the
/// same way round as [`map_y`] plots values.
fn y_tick(min: f32, max: f32, plot: Rect, index: i32, grid: i32) -> (f32, i32) {
    let grid = grid.max(1);
    let value = max - (max - min) * index as f32 / grid as f32;
    (value, plot.y + plot.height as i32 * index / grid)
}

fn map_x(value: f32, min: f32, max: f32, rect: Rect) -> i32 {
    if !value.is_finite() {
        return rect.x;
    }
    let range = max - min;
    let fraction = if range.abs() < f32::EPSILON {
        0.0
    } else {
        (value - min) / range
    }
    .clamp(0.0, 1.0);
    rect.x + (fraction * rect.width as f32) as i32
}

fn map_y(value: f32, min: f32, max: f32, rect: Rect) -> i32 {
    if !value.is_finite() {
        return rect.bottom_i32();
    }
    let range = max - min;
    let fraction = if range.abs() < f32::EPSILON {
        0.0
    } else {
        (value - min) / range
    }
    .clamp(0.0, 1.0);
    rect.bottom_i32() - (fraction * rect.height as f32) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn y_axis_labels_increase_upward_like_the_plotted_values() {
        let plot = Rect::new(10, 20, 200, 160);
        let (top, top_y) = y_tick(0.0, 100.0, plot, 0, 4);
        let (bottom, bottom_y) = y_tick(0.0, 100.0, plot, 4, 4);
        assert_eq!(top, 100.0);
        assert_eq!(bottom, 0.0);
        assert!(top_y < bottom_y);
        for index in 0..=4 {
            let (value, y) = y_tick(0.0, 70.0, plot, index, 4);
            assert!((map_y(value, 0.0, 70.0, plot) - y).abs() <= 1, "tick {index}");
        }
    }
}
