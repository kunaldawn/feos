//! Splash screen and graphics test card drawn with `feos-gfx`.

use feos_gfx::{Canvas, Color, Rect};

const ACCENT: Color = Color::rgb(92, 200, 255);
const ACCENT_DIM: Color = Color::rgb(40, 88, 150);
const TEXT_DIM: Color = Color::rgb(150, 168, 205);

/// Draws the boot splash: a gradient background, a logo mark and the title.
pub fn splash(canvas: &mut Canvas) {
    let width = canvas.width() as i32;
    let height = canvas.height() as i32;

    canvas.fill_vertical_gradient(
        Rect::new(0, 0, width as u32, height as u32),
        Color::rgb(7, 10, 24),
        Color::rgb(42, 18, 78),
    );
    draw_grid(canvas, 64, Color::rgb(21, 27, 52));

    let cx = width / 2;
    let cy = height / 2 - 150;

    canvas.circle(cx, cy, 118, ACCENT_DIM);
    canvas.circle(cx, cy, 96, Color::rgb(70, 140, 220));
    canvas.fill_circle(cx, cy, 72, Color::rgb(62, 150, 255));

    canvas.fill_rect(Rect::new(cx - 28, cy - 28, 56, 56), Color::rgb(10, 18, 36));
    canvas.stroke_rect(
        Rect::new(cx - 38, cy - 38, 76, 76),
        3,
        Color::rgb(190, 228, 255),
    );
    canvas.fill_rect(Rect::new(cx - 12, cy - 12, 24, 24), ACCENT);

    centered_text(canvas, cy + 168, "feos", Color::WHITE, 10);
    centered_text(
        canvas,
        cy + 278,
        "aarch64 graphics on QEMU ramfb",
        Color::rgb(174, 192, 232),
        2,
    );

    let dots = [
        Color::RED,
        Color::ORANGE,
        Color::YELLOW,
        Color::GREEN,
        Color::CYAN,
        Color::BLUE,
        Color::MAGENTA,
        Color::WHITE,
    ];
    for (index, color) in dots.iter().enumerate() {
        let x = cx - 210 + index as i32 * 60;
        canvas.fill_circle(x, cy + 330, 12, *color);
    }

    canvas.draw_text(
        32,
        height - 44,
        "starting the desktop session ...",
        Color::rgb(100, 115, 150),
        1,
    );
}

/// Draws a test card that exercises every primitive in the library.
pub fn demo(canvas: &mut Canvas) {
    let width = canvas.width() as i32;
    let height = canvas.height() as i32;

    canvas.fill_vertical_gradient(
        Rect::new(0, 0, width as u32, height as u32),
        Color::rgb(8, 11, 24),
        Color::rgb(50, 22, 78),
    );
    draw_grid(canvas, 32, Color::rgb(22, 28, 48));

    // Header.
    canvas.draw_text(40, 30, "feos graphics", Color::WHITE, 4);
    canvas.draw_text(40, 78, "feos-gfx test card - ramfb - XRGB8888", TEXT_DIM, 1);

    // Sun.
    let sun_x = width - 164;
    let sun_y = 128;
    canvas.circle(sun_x, sun_y, 118, Color::rgb(66, 52, 34));
    canvas.circle(sun_x, sun_y, 98, Color::rgb(120, 88, 48));
    canvas.circle(sun_x, sun_y, 78, Color::rgb(255, 210, 110));
    canvas.fill_circle(sun_x, sun_y, 58, Color::rgb(255, 186, 64));

    // Shape panel.
    canvas.stroke_rect(Rect::new(40, 220, 300, 180), 2, ACCENT_DIM);
    canvas.fill_rect(Rect::new(64, 248, 90, 100), ACCENT);
    canvas.fill_circle(280, 298, 52, Color::rgb(255, 92, 160));
    canvas.circle(280, 298, 70, Color::rgb(255, 164, 200));
    canvas.draw_text(
        40,
        412,
        "fill_rect / fill_circle / stroke_rect",
        TEXT_DIM,
        1,
    );

    // Text samples.
    canvas.draw_text(600, 248, "8x8 bitmap font", Color::WHITE, 2);
    canvas.draw_text(
        600,
        292,
        "The quick brown fox",
        Color::rgb(190, 205, 235),
        1,
    );
    canvas.draw_text(
        600,
        312,
        "jumps over the lazy dog",
        Color::rgb(190, 205, 235),
        1,
    );
    canvas.draw_text(
        600,
        332,
        "0123456789 !@#$%^&*()",
        Color::rgb(120, 220, 180),
        1,
    );

    // Checkerboard: one-square-per-pixel proof.
    for row in 0..4 {
        for col in 0..12 {
            let color = if (row + col) % 2 == 0 {
                Color::rgb(34, 46, 78)
            } else {
                Color::rgb(70, 100, 160)
            };
            canvas.fill_rect(Rect::new(600 + col * 24, 372 + row * 24, 24, 24), color);
        }
    }

    // Line fan.
    for index in 0..8 {
        canvas.line(
            380,
            618,
            520 + index * 36,
            478 + index * 14,
            Color::rgb((120 + index * 12) as u8, 170, 255),
        );
    }
    canvas.draw_text(380, 628, "Bresenham lines", TEXT_DIM, 1);

    // Colour bars.
    let bars = [
        ("red", Color::RED),
        ("orange", Color::ORANGE),
        ("yellow", Color::YELLOW),
        ("green", Color::GREEN),
        ("cyan", Color::CYAN),
        ("blue", Color::BLUE),
        ("magenta", Color::MAGENTA),
        ("white", Color::WHITE),
    ];
    let bar_width = 104;
    let gap = 10;
    let bar_y = height - 110;
    for (index, (label, color)) in bars.iter().enumerate() {
        let x = 24 + index as i32 * (bar_width + gap);
        canvas.fill_rect(Rect::new(x, bar_y, bar_width as u32, 48), *color);

        let label_width = Canvas::text_width(label, 1) as i32;
        canvas.draw_text(
            x + (bar_width - label_width) / 2,
            bar_y + 56,
            label,
            TEXT_DIM,
            1,
        );
    }
}

fn draw_grid(canvas: &mut Canvas, step: i32, color: Color) {
    let width = canvas.width() as i32;
    let height = canvas.height() as i32;

    let mut x = 0;
    while x < width {
        canvas.vline(x, 0, height - 1, color);
        x += step;
    }

    let mut y = 0;
    while y < height {
        canvas.hline(0, width - 1, y, color);
        y += step;
    }
}

fn centered_text(canvas: &mut Canvas, y: i32, text: &str, color: Color, scale: u32) {
    let width = Canvas::text_width(text, scale) as i32;
    let x = (canvas.width() as i32 - width) / 2;
    canvas.draw_text(x, y, text, color, scale);
}
