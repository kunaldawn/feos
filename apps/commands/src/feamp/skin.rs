//! FeAmp's look: the classic three-window layout (player, equaliser,
//! playlist), drawn from code. Nothing here is a bitmap from anywhere else:
//! the bevels, LED digits, sliders and stripes are all rectangles, laid out
//! where a 1998 media player kept them.
//!
//! Everything is drawn at 1x into a 275-pixel-wide buffer; the window shows it
//! doubled in double-size mode. [`hit`] answers what's under a point in the
//! same coordinates.

use core::fmt::Write;

use feos_gfx::{Canvas, Color, Rect};
use feos_text::Text;

use super::font;
use super::{MiniVis, Player, Status};
use crate::equalizer::{BANDS, RANGE_DB};

/// The width of every FeAmp window.
pub const WIDTH: i32 = 275;
/// The player's height.
pub const MAIN_H: i32 = 116;
/// The player's height when shaded to its title bar.
pub const SHADE_H: i32 = 14;
/// The equaliser's height.
pub const EQ_H: i32 = 116;
/// The playlist's height.
pub const PL_H: i32 = 232;
/// Playlist rows.
pub const PL_ROW_H: i32 = 11;
/// The playlist's list, relative to its window.
pub const PL_LIST: (i32, i32, i32, i32) = (12, 20, 244, 176);
/// Rows the playlist shows.
pub const PL_ROWS: usize = (PL_LIST.3 / PL_ROW_H) as usize;
/// Bars in the player's small spectrum.
pub const VIS_BARS: usize = 19;

const fn rgb(value: u32) -> Color {
    Color::from_u32(value)
}

// The palette: blue-steel panels, gold title stripes, and green LCDs.
const BODY_TOP: Color = rgb(0x3d4058);
const BODY_BOTTOM: Color = rgb(0x24263a);
const EDGE_LIGHT: Color = rgb(0x7a7fa3);
const EDGE_DARK: Color = rgb(0x0c0d16);
const FACE_TOP: Color = rgb(0x5a5e7c);
const FACE_BOTTOM: Color = rgb(0x363950);
const FACE_PRESSED: Color = rgb(0x22243a);
const ICON: Color = rgb(0xdcdff0);
const ICON_DIM: Color = rgb(0x8a8ea8);
const LABEL: Color = rgb(0xb7bbd6);
const LCD: Color = rgb(0x000000);
const LCD_GREEN: Color = rgb(0x00e41c);
const LCD_GHOST: Color = rgb(0x0d2a12);
const LCD_DIM: Color = rgb(0x0f5c18);
const GOLD: Color = rgb(0xe8c35a);
const GOLD_DARK: Color = rgb(0x8f6a1f);
const GREY_STRIPE: Color = rgb(0x8d90a6);
const GREY_STRIPE_DARK: Color = rgb(0x4a4c60);
const TITLE_TEXT: Color = rgb(0xfff4d0);
const TITLE_TEXT_INACTIVE: Color = rgb(0xc4c6d4);
const GROOVE: Color = rgb(0x0e0f19);
const PL_TEXT: Color = rgb(0x00f000);
const PL_CURRENT: Color = rgb(0xffffff);
const PL_SELECTED: Color = rgb(0x1020c8);
const LED_ON: Color = rgb(0x3cff3c);
const LED_OFF: Color = rgb(0x1a3a1a);

/// Everything that can be clicked or dragged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Control {
    MainTitle,
    Menu,
    Minimize,
    Shade,
    Close,
    /// The letters down the left: O, A, I, D, V.
    Clutter(u8),
    Time,
    MiniVis,
    Volume,
    Balance,
    EqToggle,
    PlToggle,
    Seek,
    Prev,
    Play,
    Pause,
    Stop,
    Next,
    Eject,
    Shuffle,
    Repeat,
    Logo,
    EqTitle,
    EqClose,
    EqOn,
    EqAuto,
    EqPresets,
    Preamp,
    Band(u8),
    PlTitle,
    PlClose,
    PlRows,
    PlScroll,
    PlAdd,
    PlRem,
    PlSel,
    PlMisc,
    PlList,
}

/// Where each window is, for the windows showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout {
    /// The player's height: [`MAIN_H`], or [`SHADE_H`] when shaded.
    pub main_h: i32,
    /// The equaliser's top, if it shows.
    pub eq_y: Option<i32>,
    /// The playlist's top, if it shows.
    pub pl_y: Option<i32>,
    /// The whole height.
    pub height: i32,
}

impl Layout {
    /// Stacks the windows that show, top to bottom.
    pub fn new(shaded: bool, show_eq: bool, show_pl: bool) -> Self {
        let main_h = if shaded { SHADE_H } else { MAIN_H };
        let mut y = main_h;
        let eq_y = show_eq.then(|| {
            let at = y;
            y += EQ_H;
            at
        });
        let pl_y = show_pl.then(|| {
            let at = y;
            y += PL_H;
            at
        });
        Self {
            main_h,
            eq_y,
            pl_y,
            height: y,
        }
    }
}

fn inside(x: i32, y: i32, rect: (i32, i32, i32, i32)) -> bool {
    x >= rect.0 && y >= rect.1 && x < rect.0 + rect.2 && y < rect.1 + rect.3
}

// Where things are in the player.
pub const VOLUME: (i32, i32, i32, i32) = (107, 57, 68, 13);
pub const BALANCE: (i32, i32, i32, i32) = (177, 57, 38, 13);
pub const SEEK: (i32, i32, i32, i32) = (16, 72, 248, 10);
pub const SLIDER_THUMB: i32 = 14;
pub const SEEK_THUMB: i32 = 29;
/// The equaliser's sliders: x of the first band, spacing, and the column.
pub const EQ_SLIDER_TOP: i32 = 38;
pub const EQ_SLIDER_H: i32 = 63;
pub const EQ_THUMB: i32 = 11;
pub const PREAMP_X: i32 = 21;
const BAND_X: i32 = 78;
const BAND_STEP: i32 = 18;
const TRANSPORT: [(Control, i32); 5] = [
    (Control::Prev, 16),
    (Control::Play, 39),
    (Control::Pause, 62),
    (Control::Stop, 85),
    (Control::Next, 108),
];
const PL_BUTTONS: [(Control, &str, i32, i32); 5] = [
    (Control::PlAdd, "ADD", 11, 25),
    (Control::PlRem, "REM", 39, 25),
    (Control::PlSel, "SEL", 67, 25),
    (Control::PlMisc, "MISC", 95, 29),
    (Control::PlList, "LIST", 230, 32),
];

/// What's at (`x`, `y`), in 1x coordinates.
pub fn hit(layout: &Layout, x: i32, y: i32) -> Option<Control> {
    if !(0..WIDTH).contains(&x) || y < 0 {
        return None;
    }
    if y < layout.main_h {
        return hit_main(layout.main_h == SHADE_H, x, y);
    }
    if let Some(top) = layout.eq_y {
        if y >= top && y < top + EQ_H {
            return hit_eq(x, y - top);
        }
    }
    if let Some(top) = layout.pl_y {
        if y >= top && y < top + PL_H {
            return hit_pl(x, y - top);
        }
    }
    None
}

fn title_buttons(x: i32, y: i32) -> Option<Control> {
    if !(3..12).contains(&y) {
        return None;
    }
    match x {
        6..=14 => Some(Control::Menu),
        244..=252 => Some(Control::Minimize),
        254..=262 => Some(Control::Shade),
        264..=272 => Some(Control::Close),
        _ => None,
    }
}

fn hit_main(shaded: bool, x: i32, y: i32) -> Option<Control> {
    if y < 14 {
        return Some(title_buttons(x, y).unwrap_or(Control::MainTitle));
    }
    if shaded {
        return Some(Control::MainTitle);
    }
    if inside(x, y, (10, 22, 8, 40)) {
        return Some(Control::Clutter(((y - 22) / 8) as u8));
    }
    if inside(x, y, (26, 22, 76, 20)) {
        return Some(Control::Time);
    }
    if inside(x, y, (25, 44, 76, 17)) {
        return Some(Control::MiniVis);
    }
    for (control, rect) in [
        (Control::Volume, VOLUME),
        (Control::Balance, BALANCE),
        (Control::EqToggle, (219, 58, 23, 12)),
        (Control::PlToggle, (242, 58, 23, 12)),
        (Control::Seek, SEEK),
        (Control::Eject, (136, 89, 22, 16)),
        (Control::Shuffle, (161, 89, 50, 15)),
        (Control::Repeat, (211, 89, 30, 15)),
        (Control::Logo, (243, 88, 26, 19)),
    ] {
        if inside(x, y, rect) {
            return Some(control);
        }
    }
    TRANSPORT
        .iter()
        .find(|(_, left)| inside(x, y, (*left, 88, 23, 18)))
        .map(|(control, _)| *control)
}

fn hit_eq(x: i32, y: i32) -> Option<Control> {
    if y < 14 {
        return Some(if (264..=272).contains(&x) && (3..12).contains(&y) {
            Control::EqClose
        } else {
            Control::EqTitle
        });
    }
    for (control, rect) in [
        (Control::EqOn, (14, 18, 25, 12)),
        (Control::EqAuto, (40, 18, 33, 12)),
        (Control::EqPresets, (217, 18, 44, 12)),
        (Control::Preamp, (PREAMP_X - 2, EQ_SLIDER_TOP, 18, EQ_SLIDER_H)),
    ] {
        if inside(x, y, rect) {
            return Some(control);
        }
    }
    (0..BANDS as i32)
        .find(|band| inside(x, y, (BAND_X + band * BAND_STEP - 2, EQ_SLIDER_TOP, 18, EQ_SLIDER_H)))
        .map(|band| Control::Band(band as u8))
}

fn hit_pl(x: i32, y: i32) -> Option<Control> {
    if y < 14 {
        return Some(if (264..=272).contains(&x) && (3..12).contains(&y) {
            Control::PlClose
        } else {
            Control::PlTitle
        });
    }
    if inside(x, y, PL_LIST) {
        return Some(Control::PlRows);
    }
    if inside(x, y, (258, PL_LIST.1, 10, PL_LIST.3)) {
        return Some(Control::PlScroll);
    }
    PL_BUTTONS
        .iter()
        .find(|(_, _, left, width)| inside(x, y, (*left, 204, *width, 18)))
        .map(|(control, ..)| *control)
}

/// A slider's position from a point along it, 0 to 1.
pub fn slider_fraction(start: i32, length: i32, thumb: i32, at: i32) -> f32 {
    ((at - start - thumb / 2) as f32 / (length - thumb).max(1) as f32).clamp(0.0, 1.0)
}

// Drawing --------------------------------------------------------------------

fn fill(canvas: &mut Canvas<'_>, x: i32, y: i32, w: i32, h: i32, color: Color) {
    if w > 0 && h > 0 {
        canvas.fill_rect(Rect::new(x, y, w as u32, h as u32), color);
    }
}

fn gradient(canvas: &mut Canvas<'_>, x: i32, y: i32, w: i32, h: i32, top: Color, bottom: Color) {
    for row in 0..h {
        let t = if h > 1 { (row * 255 / (h - 1)) as u8 } else { 0 };
        fill(canvas, x, y + row, w, 1, top.lerp(bottom, t));
    }
}

/// A raised (or, `sunken`, pressed-in) edge.
fn bevel(canvas: &mut Canvas<'_>, x: i32, y: i32, w: i32, h: i32, sunken: bool) {
    let (light, dark) = if sunken { (EDGE_DARK, EDGE_LIGHT) } else { (EDGE_LIGHT, EDGE_DARK) };
    fill(canvas, x, y, w, 1, light);
    fill(canvas, x, y, 1, h, light);
    fill(canvas, x, y + h - 1, w, 1, dark);
    fill(canvas, x + w - 1, y, 1, h, dark);
}

fn button(canvas: &mut Canvas<'_>, x: i32, y: i32, w: i32, h: i32, pressed: bool) {
    if pressed {
        fill(canvas, x, y, w, h, FACE_PRESSED);
    } else {
        gradient(canvas, x, y, w, h, FACE_TOP, FACE_BOTTOM);
    }
    bevel(canvas, x, y, w, h, pressed);
}

/// A panel's brushed-steel body: a gradient with faint scan lines.
fn body(canvas: &mut Canvas<'_>, x: i32, y: i32, w: i32, h: i32) {
    gradient(canvas, x, y, w, h, BODY_TOP, BODY_BOTTOM);
    for row in (1..h).step_by(2) {
        let line = canvas.pixel(x + 1, y + row).unwrap_or(BODY_BOTTOM);
        fill(canvas, x, y + row, w, 1, line.lerp(Color::BLACK, 24));
    }
    bevel(canvas, x, y, w, h, false);
}

/// A black display, set into the panel.
fn lcd(canvas: &mut Canvas<'_>, x: i32, y: i32, w: i32, h: i32) {
    fill(canvas, x, y, w, h, LCD);
    bevel(canvas, x - 1, y - 1, w + 2, h + 2, true);
}

/// A title bar with gold stripes either side of its name, when it's focused.
fn title_bar(canvas: &mut Canvas<'_>, y: i32, name: &str, active: bool, buttons: &[Control], pressed: Option<Control>) {
    gradient(canvas, 1, y + 1, WIDTH - 2, 12, rgb(0x2e3048), rgb(0x1a1b2a));
    let (stripe, stripe_dark, text) = if active {
        (GOLD, GOLD_DARK, TITLE_TEXT)
    } else {
        (GREY_STRIPE, GREY_STRIPE_DARK, TITLE_TEXT_INACTIVE)
    };
    let text_width = font::width(name);
    let text_x = (WIDTH - text_width) / 2;
    let stripes_start = 20;
    let stripes_end = 240;
    for row in [4, 6, 8, 10] {
        let color = if row == 4 || row == 8 { stripe } else { stripe_dark };
        fill(canvas, stripes_start, y + row, text_x - 6 - stripes_start, 1, color);
        fill(canvas, text_x + text_width + 5, y + row, stripes_end - text_x - text_width - 5, 1, color);
    }
    font::draw(canvas, text_x, y + 4, name, text, WIDTH);
    for control in buttons {
        let (bx, glyph) = match control {
            Control::Menu => (6, 0),
            Control::Minimize => (244, 1),
            Control::Shade => (254, 2),
            _ => (264, 3),
        };
        let down = pressed == Some(*control);
        button(canvas, bx, y + 3, 9, 9, down);
        let color = if active { ICON } else { ICON_DIM };
        match glyph {
            0 => {
                fill(canvas, bx + 2, y + 5, 5, 1, color);
                fill(canvas, bx + 2, y + 7, 5, 1, color);
                fill(canvas, bx + 2, y + 9, 5, 1, color);
            }
            1 => fill(canvas, bx + 2, y + 9, 5, 1, color),
            2 => {
                fill(canvas, bx + 2, y + 5, 5, 1, color);
                fill(canvas, bx + 2, y + 9, 5, 1, color);
                fill(canvas, bx + 2, y + 5, 1, 5, color);
                fill(canvas, bx + 6, y + 5, 1, 5, color);
            }
            _ => {
                for step in 0..5 {
                    canvas.put_pixel(bx + 2 + step, y + 5 + step, color);
                    canvas.put_pixel(bx + 6 - step, y + 5 + step, color);
                }
            }
        }
    }
}

/// A seven-segment digit, 9 by 13, with its unlit segments faintly showing.
fn led_digit(canvas: &mut Canvas<'_>, x: i32, y: i32, digit: Option<u8>, minus: bool) {
    // a b c d e f g, as on a calculator.
    const SEGMENTS: [u8; 10] = [
        0b111_1110, 0b011_0000, 0b110_1101, 0b111_1001, 0b011_0011, 0b101_1011, 0b101_1111, 0b111_0000,
        0b111_1111, 0b111_1011,
    ];
    let lit = match (digit, minus) {
        (_, true) => 0b000_0001,
        (Some(d), _) => SEGMENTS[d as usize % 10],
        (None, _) => 0,
    };
    // The sign's place shows nothing unless there's a sign.
    let ghost = if digit.is_none() && !minus && x < 45 { LCD } else { LCD_GHOST };
    let segment = |canvas: &mut Canvas<'_>, bit: u8, rect: (i32, i32, i32, i32)| {
        let color = if lit & (1 << (6 - bit)) != 0 { LCD_GREEN } else { ghost };
        fill(canvas, x + rect.0, y + rect.1, rect.2, rect.3, color);
    };
    segment(canvas, 0, (1, 0, 7, 2)); // a
    segment(canvas, 1, (7, 1, 2, 5)); // b
    segment(canvas, 2, (7, 7, 2, 5)); // c
    segment(canvas, 3, (1, 11, 7, 2)); // d
    segment(canvas, 4, (0, 7, 2, 5)); // e
    segment(canvas, 5, (0, 1, 2, 5)); // f
    segment(canvas, 6, (1, 6, 7, 1)); // g
}

/// Writes `m:ss`.
pub fn write_time<const N: usize>(out: &mut Text<N>, ms: u64) {
    let seconds = ms / 1000;
    let _ = write!(out, "{}:{:02}", seconds / 60, seconds % 60);
}

/// Green at the bottom of a range, through yellow, to red at the top.
fn level_color(fraction: f32) -> Color {
    let t = fraction.clamp(0.0, 1.0);
    if t < 0.5 {
        rgb(0x22d022).lerp(rgb(0xe8e030), (t * 2.0 * 255.0) as u8)
    } else {
        rgb(0xe8e030).lerp(rgb(0xef3a18), ((t - 0.5) * 2.0 * 255.0) as u8)
    }
}

fn icon_triangle_right(canvas: &mut Canvas<'_>, x: i32, y: i32, height: i32, color: Color) {
    for column in 0..(height + 1) / 2 {
        fill(canvas, x + column, y + column, 1, height - 2 * column, color);
    }
}

fn icon_triangle_left(canvas: &mut Canvas<'_>, x: i32, y: i32, height: i32, color: Color) {
    let width = (height + 1) / 2;
    for column in 0..width {
        fill(canvas, x + width - 1 - column, y + column, 1, height - 2 * column, color);
    }
}

/// Draws the whole of FeAmp, at 1x, into `canvas`.
pub fn draw(player: &Player, canvas: &mut Canvas<'_>, now_ms: u64) {
    let layout = player.layout();
    draw_main(player, canvas, layout.main_h == SHADE_H, now_ms);
    if let Some(top) = layout.eq_y {
        draw_eq(player, canvas, top);
    }
    if let Some(top) = layout.pl_y {
        draw_playlist(player, canvas, top);
    }
}

fn draw_main(player: &Player, canvas: &mut Canvas<'_>, shaded: bool, now_ms: u64) {
    let pressed = player.pressed;
    let height = if shaded { SHADE_H } else { MAIN_H };
    body(canvas, 0, 0, WIDTH, height);
    title_bar(
        canvas,
        0,
        if shaded { "" } else { "FEAMP" },
        player.focused,
        &[Control::Menu, Control::Minimize, Control::Shade, Control::Close],
        pressed,
    );
    if shaded {
        // The title and time, small, in the bar.
        lcd(canvas, 20, 3, 150, 9);
        let mut text = Text::<96>::new();
        player.marquee_text(&mut text);
        let offset = player.marquee_offset(font::width(text.as_str()), 148);
        draw_marquee(canvas, 21, 4, 148, text.as_str(), offset);
        lcd(canvas, 196, 3, 34, 9);
        let mut time = Text::<16>::new();
        if player.status != Status::Stopped {
            write_time(&mut time, player.position_ms);
        }
        font::draw(canvas, 198, 4, time.as_str(), LCD_GREEN, 230);
        return;
    }

    // Clutter bar.
    fill(canvas, 10, 22, 8, 40, rgb(0x15161f));
    for (index, letter) in ['O', 'A', 'I', 'D', 'V'].iter().enumerate() {
        let lit = match index {
            3 => player.double,
            4 => player.vis_open,
            _ => pressed == Some(Control::Clutter(index as u8)),
        };
        font::draw_char(canvas, 12, 22 + index as i32 * 8, *letter, if lit { GOLD } else { ICON_DIM }, 18);
    }

    // The time display, with its status light and the small visualiser.
    lcd(canvas, 25, 21, 77, 41);
    let status_color = LCD_GREEN;
    match player.status {
        Status::Playing => icon_triangle_right(canvas, 29, 26, 9, status_color),
        Status::Paused => {
            fill(canvas, 29, 27, 3, 8, status_color);
            fill(canvas, 34, 27, 3, 8, status_color);
        }
        Status::Stopped => fill(canvas, 29, 27, 8, 8, status_color),
    }
    let blink_off = player.status == Status::Paused && (now_ms / 500) % 2 == 1;
    let shown = if player.status == Status::Stopped || blink_off {
        None
    } else {
        Some(if player.remaining {
            player.duration_ms().saturating_sub(player.position_ms)
        } else {
            player.position_ms
        })
    };
    let seconds = shown.map(|ms| ms / 1000);
    let minutes = seconds.map(|s| (s / 60).min(99));
    let minus = player.remaining && shown.is_some();
    if minus {
        fill(canvas, 41, 31, 6, 1, LCD_GREEN);
    }
    led_digit(canvas, 51, 25, minutes.map(|m| (m / 10) as u8), false);
    led_digit(canvas, 62, 25, minutes.map(|m| (m % 10) as u8), false);
    let colon = if shown.is_some() { LCD_GREEN } else { LCD_GHOST };
    fill(canvas, 73, 28, 2, 2, colon);
    fill(canvas, 73, 34, 2, 2, colon);
    led_digit(canvas, 77, 25, seconds.map(|s| ((s % 60) / 10) as u8), false);
    led_digit(canvas, 88, 25, seconds.map(|s| (s % 10) as u8), false);
    draw_mini_vis(player, canvas, 25, 44);

    // Song title, and what kind of file it is.
    lcd(canvas, 111, 22, 154, 11);
    let mut text = Text::<96>::new();
    player.marquee_text(&mut text);
    let offset = player.marquee_offset(font::width(text.as_str()), 152);
    draw_marquee(canvas, 112, 24, 152, text.as_str(), offset);
    lcd(canvas, 111, 39, 19, 9);
    lcd(canvas, 161, 39, 13, 9);
    let mut kbps = Text::<8>::new();
    let mut khz = Text::<8>::new();
    if let Some(info) = player.info.filter(|_| player.status != Status::Stopped) {
        let _ = write!(kbps, "{:>3}", player.bitrate_kbps().min(999));
        let _ = write!(khz, "{:>2}", (info.sample_rate / 1000).min(99));
    }
    font::draw(canvas, 112, 40, kbps.as_str(), LCD_GREEN, 130);
    font::draw(canvas, 162, 40, khz.as_str(), LCD_GREEN, 174);
    font::draw(canvas, 133, 40, "kbps", LABEL, WIDTH);
    font::draw(canvas, 177, 40, "khz", LABEL, WIDTH);
    let channels = player.info.filter(|_| player.status != Status::Stopped).map_or(0, |info| info.channels);
    font::draw(canvas, 205, 40, "mono", if channels == 1 { LCD_GREEN } else { LCD_DIM }, WIDTH);
    font::draw(canvas, 232, 40, "stereo", if channels >= 2 { LCD_GREEN } else { LCD_DIM }, WIDTH);

    // Volume and balance.
    let volume = player.volume_fraction();
    draw_hslider(canvas, VOLUME, volume, level_color(volume), pressed == Some(Control::Volume));
    let balance = player.eq.balance;
    let off_center = balance.abs();
    draw_hslider(
        canvas,
        BALANCE,
        balance * 0.5 + 0.5,
        rgb(0x22d022).lerp(rgb(0xef3a18), (off_center * 255.0) as u8),
        pressed == Some(Control::Balance),
    );
    toggle(canvas, 219, 58, 23, 12, "EQ", player.show_eq, pressed == Some(Control::EqToggle));
    toggle(canvas, 242, 58, 23, 12, "PL", player.show_pl, pressed == Some(Control::PlToggle));

    // The seek bar.
    let (sx, sy, sw, sh) = SEEK;
    fill(canvas, sx, sy, sw, sh, GROOVE);
    bevel(canvas, sx, sy, sw, sh, true);
    let duration = player.duration_ms();
    if player.status != Status::Stopped && duration > 0 {
        let fraction = player
            .seek_preview
            .unwrap_or((player.position_ms as f32 / duration as f32).min(1.0));
        let thumb_x = sx + ((sw - SEEK_THUMB) as f32 * fraction) as i32;
        fill(canvas, sx + 1, sy + 4, thumb_x - sx, 2, LCD_DIM);
        button(canvas, thumb_x, sy, SEEK_THUMB, sh, pressed == Some(Control::Seek));
        fill(canvas, thumb_x + 9, sy + 3, 11, 1, ICON_DIM);
        fill(canvas, thumb_x + 9, sy + 6, 11, 1, ICON_DIM);
    }

    // Transport buttons.
    for (control, left) in TRANSPORT {
        let down = pressed == Some(control);
        button(canvas, left, 88, 23, 18, down);
        let (ix, iy) = (left + 7 + down as i32, 92 + down as i32);
        match control {
            Control::Prev => {
                fill(canvas, ix - 1, iy, 2, 9, ICON);
                icon_triangle_left(canvas, ix + 2, iy, 9, ICON);
                icon_triangle_left(canvas, ix + 6, iy, 9, ICON);
            }
            Control::Play => icon_triangle_right(canvas, ix + 2, iy, 9, ICON),
            Control::Pause => {
                fill(canvas, ix + 1, iy + 1, 3, 8, ICON);
                fill(canvas, ix + 6, iy + 1, 3, 8, ICON);
            }
            Control::Stop => fill(canvas, ix + 1, iy + 1, 8, 8, ICON),
            _ => {
                icon_triangle_right(canvas, ix - 1, iy, 9, ICON);
                icon_triangle_right(canvas, ix + 3, iy, 9, ICON);
                fill(canvas, ix + 8, iy, 2, 9, ICON);
            }
        }
    }
    let eject_down = pressed == Some(Control::Eject);
    button(canvas, 136, 89, 22, 16, eject_down);
    for row in 0..5 {
        fill(canvas, 147 - row - 1, 93 + row, row * 2 + 2, 1, ICON);
    }
    fill(canvas, 142, 100, 10, 2, ICON);
    toggle(canvas, 161, 89, 50, 15, "SHUFFLE", player.shuffle, pressed == Some(Control::Shuffle));
    toggle(canvas, 211, 89, 30, 15, "REP", player.repeat, pressed == Some(Control::Repeat));
    draw_logo(canvas, 243, 88, player.status == Status::Playing && (now_ms / 120).is_multiple_of(8));
}

/// A toggle button: a light, then its name.
fn toggle(canvas: &mut Canvas<'_>, x: i32, y: i32, w: i32, h: i32, label: &str, on: bool, pressed: bool) {
    button(canvas, x, y, w, h, pressed);
    fill(canvas, x + 3, y + h / 2 - 1, 3, 3, if on { LED_ON } else { LED_OFF });
    let text_x = x + 7 + (w - 8 - font::width(label)).max(0) / 2;
    font::draw(canvas, text_x, y + (h - 7) / 2, label, if on { ICON } else { ICON_DIM }, x + w - 1);
}

fn draw_hslider(canvas: &mut Canvas<'_>, rect: (i32, i32, i32, i32), fraction: f32, color: Color, pressed: bool) {
    let (x, y, w, h) = rect;
    fill(canvas, x, y + 3, w, h - 6, GROOVE);
    bevel(canvas, x, y + 3, w, h - 6, true);
    let thumb_x = x + ((w - SLIDER_THUMB) as f32 * fraction.clamp(0.0, 1.0)) as i32;
    fill(canvas, x + 1, y + 5, thumb_x - x, h - 10, color);
    button(canvas, thumb_x, y, SLIDER_THUMB, h, pressed);
    fill(canvas, thumb_x + 4, y + 6, 6, 1, ICON_DIM);
}

/// The FeAmp mark: a lightning bolt, for iron that sparks.
fn draw_logo(canvas: &mut Canvas<'_>, x: i32, y: i32, flash: bool) {
    fill(canvas, x, y, 26, 19, rgb(0x1a1b28));
    bevel(canvas, x, y, 26, 19, true);
    let bolt = if flash { rgb(0xffffff) } else { GOLD };
    let rows: [(i32, i32); 15] = [
        (13, 4), (12, 4), (11, 4), (10, 4), (9, 4), (8, 9), (9, 7), (10, 5),
        (9, 4), (8, 4), (7, 4), (6, 3), (5, 3), (5, 2), (5, 1),
    ];
    for (row, (left, width)) in rows.iter().enumerate() {
        fill(canvas, x + left + 1, y + 2 + row as i32, *width, 1, bolt);
    }
    // A spark where it lands.
    let spark = if flash { GOLD } else { ICON_DIM };
    canvas.put_pixel(x + 4, y + 16, spark);
    canvas.put_pixel(x + 8, y + 16, spark);
    canvas.put_pixel(x + 3, y + 14, spark);
    canvas.put_pixel(x + 9, y + 14, spark);
    font::draw(canvas, x + 14, y + 10, "Fe", ICON, x + 26);
}

/// Scrolling text in a box `width` wide, starting `offset` pixels in.
fn draw_marquee(canvas: &mut Canvas<'_>, x: i32, y: i32, width: i32, text: &str, offset: i32) {
    let text_width = font::width(text);
    if text_width <= width {
        font::draw(canvas, x, y, text, LCD_GREEN, x + width);
        return;
    }
    // Two copies, so the end runs into the start again.
    let mut sub = [0u32; 160 * 7];
    let Some(mut strip) = Canvas::new(&mut sub, 160, 7, 160) else {
        return;
    };
    let start = -offset;
    font::draw(&mut strip, start, 0, text, LCD_GREEN, 160);
    font::draw(&mut strip, start + text_width, 0, text, LCD_GREEN, 160);
    for row in 0..7 {
        for column in 0..width.min(160) {
            let pixel = sub[(row * 160 + column) as usize];
            if pixel != 0 {
                canvas.put_pixel(x + column, y + row, Color::from_u32(pixel));
            }
        }
    }
}

fn draw_mini_vis(player: &Player, canvas: &mut Canvas<'_>, x: i32, y: i32) {
    match player.vis_mode {
        MiniVis::Off => {}
        MiniVis::Spectrum => {
            for (bar, (&height, &peak)) in player.bars.iter().zip(&player.peaks).enumerate() {
                let bx = x + 1 + bar as i32 * 4;
                let height = height.clamp(0.0, 16.0) as i32;
                for row in 0..height {
                    let color = level_color(row as f32 / 15.0);
                    fill(canvas, bx, y + 16 - row, 3, 1, color);
                }
                let peak = peak.clamp(0.0, 16.0) as i32;
                if peak > 0 {
                    fill(canvas, bx, y + 16 - peak, 3, 1, rgb(0xb4b4c8));
                }
            }
        }
        MiniVis::Scope => {
            let mut last = None;
            for (column, value) in player.scope_line.iter().enumerate() {
                let py = y + 8 - (value * 8.0).clamp(-8.0, 7.0) as i32;
                let px = x + column as i32;
                let color = level_color(value.abs() * 1.5);
                match last {
                    Some(previous) => {
                        let (top, bottom) = if previous < py { (previous, py) } else { (py, previous) };
                        fill(canvas, px, top, 1, (bottom - top).max(1), color);
                    }
                    None => canvas.put_pixel(px, py, color),
                }
                last = Some(py);
            }
        }
    }
}

fn draw_eq(player: &Player, canvas: &mut Canvas<'_>, top: i32) {
    let pressed = player.pressed;
    body(canvas, 0, top, WIDTH, EQ_H);
    title_bar(canvas, top, "FEAMP EQUALIZER", player.focused, &[Control::EqClose], pressed);
    toggle(canvas, 14, top + 18, 25, 12, "ON", player.eq.enabled, pressed == Some(Control::EqOn));
    toggle(canvas, 40, top + 18, 33, 12, "AUTO", player.eq_auto, pressed == Some(Control::EqAuto));
    button(canvas, 217, top + 18, 44, 12, pressed == Some(Control::EqPresets));
    font::draw(canvas, 219, top + 21, "PRESETS", ICON, 260);

    // The curve.
    let (gx, gy, gw, gh) = (86, top + 17, 113, 19);
    lcd(canvas, gx, gy, gw, gh);
    fill(canvas, gx, gy + gh / 2, gw, 1, LCD_GHOST);
    let preamp_y = gy + gh / 2 - (player.eq.preamp_db / RANGE_DB * (gh / 2 - 1) as f32) as i32;
    fill(canvas, gx, preamp_y, gw, 1, LCD_DIM);
    let mut previous = None;
    for column in 0..gw {
        let db = curve_at(&player.eq.bands_db, column as f32 / (gw - 1) as f32);
        let py = gy + gh / 2 - (db / RANGE_DB * (gh / 2 - 1) as f32) as i32;
        let color = level_color(0.5 + db / RANGE_DB * 0.5);
        match previous {
            Some(last) => {
                let (a, b): (i32, i32) = if last < py { (last, py) } else { (py, last) };
                fill(canvas, gx + column, a, 1, (b - a).max(1), color);
            }
            None => canvas.put_pixel(gx + column, py, color),
        }
        previous = Some(py);
    }

    // Labels.
    font::draw_tiny(canvas, 52, top + EQ_SLIDER_TOP + 3, "+12db", LABEL);
    font::draw_tiny(canvas, 56, top + EQ_SLIDER_TOP + 29, "+0db", LABEL);
    font::draw_tiny(canvas, 52, top + EQ_SLIDER_TOP + 55, "-12db", LABEL);
    font::draw(canvas, 13, top + 104, "PREAMP", LABEL, 50);
    const NAMES: [&str; BANDS] = ["60", "170", "310", "600", "1K", "3K", "6K", "12K", "14K", "16K"];
    draw_vslider(canvas, PREAMP_X, top + EQ_SLIDER_TOP, player.eq.preamp_db, pressed == Some(Control::Preamp));
    for (band, name) in NAMES.iter().enumerate() {
        let x = BAND_X + band as i32 * BAND_STEP;
        draw_vslider(canvas, x, top + EQ_SLIDER_TOP, player.eq.bands_db[band], pressed == Some(Control::Band(band as u8)));
        font::draw_tiny(canvas, x + 8 - font::tiny_width(name) / 2, top + 105, name, LABEL);
    }
}

/// The equaliser's curve at `t` (0 to 1 across the bands): a Catmull-Rom spline.
pub fn curve_at(bands: &[f32; BANDS], t: f32) -> f32 {
    let position = t.clamp(0.0, 1.0) * (BANDS - 1) as f32;
    let index = (position as usize).min(BANDS - 2);
    let f = position - index as f32;
    let at = |i: isize| bands[i.clamp(0, BANDS as isize - 1) as usize];
    let (p0, p1, p2, p3) = (at(index as isize - 1), at(index as isize), at(index as isize + 1), at(index as isize + 2));
    let value = 0.5
        * (2.0 * p1
            + (-p0 + p2) * f
            + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * f * f
            + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * f * f * f);
    value.clamp(-RANGE_DB, RANGE_DB)
}

fn draw_vslider(canvas: &mut Canvas<'_>, x: i32, y: i32, db: f32, pressed: bool) {
    let groove_x = x + 5;
    fill(canvas, groove_x, y, 5, EQ_SLIDER_H, GROOVE);
    bevel(canvas, groove_x, y, 5, EQ_SLIDER_H, true);
    let fraction = 0.5 - db / RANGE_DB * 0.5; // 0 at the top
    let thumb_y = y + ((EQ_SLIDER_H - EQ_THUMB) as f32 * fraction) as i32;
    // The bar lights from the bottom to the thumb, coloured by height.
    for row in (thumb_y + EQ_THUMB / 2)..(y + EQ_SLIDER_H - 1) {
        let height = 1.0 - (row - y) as f32 / EQ_SLIDER_H as f32;
        fill(canvas, groove_x + 1, row, 3, 1, level_color(height));
    }
    fill(canvas, groove_x - 1, y + EQ_SLIDER_H / 2, 7, 1, LCD_DIM);
    button(canvas, x + 1, thumb_y, 13, EQ_THUMB, pressed);
    fill(canvas, x + 4, thumb_y + 5, 7, 1, ICON_DIM);
}

fn draw_playlist(player: &Player, canvas: &mut Canvas<'_>, top: i32) {
    let pressed = player.pressed;
    body(canvas, 0, top, WIDTH, PL_H);
    title_bar(canvas, top, "FEAMP PLAYLIST", player.focused, &[Control::PlClose], pressed);
    let (lx, ly, lw, lh) = PL_LIST;
    lcd(canvas, lx, top + ly, lw, lh);
    for row in 0..PL_ROWS {
        let index = player.scroll + row;
        let Some(track) = player.tracks.get(index) else {
            break;
        };
        let y = top + ly + row as i32 * PL_ROW_H;
        if player.cursor == index {
            fill(canvas, lx, y, lw, PL_ROW_H, PL_SELECTED);
        }
        let color = if player.current == Some(index) { PL_CURRENT } else { PL_TEXT };
        let mut duration = Text::<12>::new();
        if track.duration_ms > 0 {
            write_time(&mut duration, track.duration_ms);
        }
        let duration_x = lx + lw - 2 - font::width(duration.as_str());
        let mut line = Text::<128>::new();
        let _ = write!(line, "{}. {}", index + 1, track.name.as_str());
        let end = font::draw(canvas, lx + 2, y + 2, line.as_str(), color, duration_x - 4);
        if end >= duration_x - 4 {
            // Clipped: a hint that there's more.
            fill(canvas, duration_x - 8, y + 8, 1, 1, color);
            fill(canvas, duration_x - 6, y + 8, 1, 1, color);
        }
        font::draw(canvas, duration_x, y + 2, duration.as_str(), color, WIDTH);
    }
    if player.tracks.is_empty() {
        font::draw(canvas, lx + 6, top + ly + 6, "No music. Press ADD to scan", PL_TEXT, lx + lw);
        font::draw(canvas, lx + 6, top + ly + 17, "the disk, or drop files here.", PL_TEXT, lx + lw);
    }

    // The scroll bar.
    let (bx, by, bh) = (259, top + ly, lh);
    fill(canvas, bx, by, 8, bh, GROOVE);
    bevel(canvas, bx, by, 8, bh, true);
    let total = player.tracks.len();
    let thumb_h = 18;
    let thumb_y = if total > PL_ROWS {
        by + ((bh - thumb_h) as usize * player.scroll / (total - PL_ROWS)) as i32
    } else {
        by
    };
    button(canvas, bx, thumb_y, 8, thumb_h, pressed == Some(Control::PlScroll));

    // Buttons and the totals display.
    for (control, label, left, width) in PL_BUTTONS {
        button(canvas, left, top + 204, width, 18, pressed == Some(control));
        font::draw(canvas, left + (width - font::width(label)) / 2 + 1, top + 210, label, ICON, left + width);
    }
    lcd(canvas, 128, top + 204, 96, 9);
    let mut totals = Text::<32>::new();
    if player.status != Status::Stopped {
        write_time(&mut totals, player.position_ms);
    } else {
        let _ = write!(totals, "-:--");
    }
    let _ = write!(totals, "/");
    write_time(&mut totals, player.total_ms());
    font::draw(canvas, 130, top + 205, totals.as_str(), LCD_GREEN, 224);
    let mut count = Text::<16>::new();
    let _ = write!(count, "{} files", total);
    font::draw(canvas, 130, top + 215, count.as_str(), LABEL, 224);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_stacks_the_windows_that_show() {
        assert_eq!(Layout::new(false, true, true).height, MAIN_H + EQ_H + PL_H);
        let shaded = Layout::new(true, false, true);
        assert_eq!(shaded.pl_y, Some(SHADE_H));
        assert_eq!(shaded.eq_y, None);
        assert_eq!(Layout::new(false, false, false).height, MAIN_H);
    }

    #[test]
    fn hits_land_on_the_controls() {
        let layout = Layout::new(false, true, true);
        assert_eq!(hit(&layout, 50, 95), Some(Control::Play));
        assert_eq!(hit(&layout, 268, 7), Some(Control::Close));
        assert_eq!(hit(&layout, 100, 5), Some(Control::MainTitle));
        assert_eq!(hit(&layout, 12, 23), Some(Control::Clutter(0)));
        assert_eq!(hit(&layout, 12, 57), Some(Control::Clutter(4)));
        assert_eq!(hit(&layout, 85, MAIN_H + 60), Some(Control::Band(0)));
        assert_eq!(hit(&layout, 250, MAIN_H + 60), Some(Control::Band(9)));
        assert_eq!(hit(&layout, 100, MAIN_H + EQ_H + 50), Some(Control::PlRows));
        assert_eq!(hit(&layout, 20, MAIN_H + EQ_H + 210), Some(Control::PlAdd));
        assert_eq!(hit(&layout, 300, 50), None);
    }

    #[test]
    fn sliders_reach_both_ends() {
        let (x, _, w, _) = VOLUME;
        assert_eq!(slider_fraction(x, w, SLIDER_THUMB, x), 0.0);
        assert_eq!(slider_fraction(x, w, SLIDER_THUMB, x + w), 1.0);
    }

    #[test]
    fn the_curve_passes_through_the_bands() {
        let bands = [12.0, -12.0, 0.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0, 6.0];
        assert_eq!(curve_at(&bands, 0.0), 12.0);
        assert!((curve_at(&bands, 1.0 / 9.0) + 12.0).abs() < 0.01);
        assert!((curve_at(&bands, 1.0) - 6.0).abs() < 0.01);
    }
}
