//! The volume panel that opens from the tray's speaker.

use feos_gfx::Rect;
use feos_gui::{Id, RectExt, Sense, Ui};

use crate::taskbar::{speaker_rect, TaskbarLayout};
use crate::theme::DesktopTheme;

const PANEL_WIDTH: u32 = 196;
const PANEL_HEIGHT: u32 = 68;
const SLIDER_MARGIN: i32 = 10;

/// The sound settings as the desktop last saw them. The mixer is shared by the
/// whole system, so the tests, which run two desktops side by side, keep theirs
/// here instead of letting one reach the other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sound {
    pub(crate) level: u8,
    pub(crate) muted: bool,
}

impl Sound {
    pub(crate) const FULL: Self = Self {
        level: feos_mixer::MAX_VOLUME,
        muted: false,
    };

    /// Catches up with the mixer, which an app may have changed.
    pub(crate) fn sync(&mut self) {
        #[cfg(not(test))]
        {
            self.level = feos_mixer::volume();
            self.muted = feos_mixer::muted();
        }
    }

    /// Sets the volume, by way of a wheel or slider.
    pub(crate) fn set_level(&mut self, level: i32) {
        self.level = level.clamp(0, feos_mixer::MAX_VOLUME as i32) as u8;
        #[cfg(not(test))]
        feos_mixer::set_volume(self.level);
    }

    /// Mutes or unmutes.
    pub(crate) fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        #[cfg(not(test))]
        feos_mixer::set_muted(muted);
    }
}

/// The volume a press at `x` asks for, on a slider starting at `left`.
pub(crate) fn level_at(x: i32, left: i32, width: u32) -> u8 {
    let max = feos_mixer::MAX_VOLUME as i64;
    let offset = (x - left).clamp(0, width as i32) as i64;
    ((offset * max + width.max(1) as i64 / 2) / width.max(1) as i64) as u8
}

/// Draws the panel and applies what the pointer does to it. Returns its area.
pub(crate) fn render_panel<const N: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    taskbar: &TaskbarLayout,
    work_area: Rect,
    sound: &mut Sound,
) -> Rect {
    let speaker = speaker_rect(taskbar);
    let left = (speaker.x + speaker.width as i32 - PANEL_WIDTH as i32)
        .min(work_area.right_i32() - PANEL_WIDTH as i32)
        .max(work_area.x);
    let panel = Rect::new(
        left,
        (work_area.bottom_i32() - PANEL_HEIGHT as i32 - 4).max(work_area.y),
        PANEL_WIDTH,
        PANEL_HEIGHT,
    );
    ui.painter.shadow(panel, 6, 8, 120);
    ui.painter.fill_rounded_rect(panel, 4, theme.menu_bg);
    ui.painter
        .stroke_rounded_rect(panel, 4, 1, theme.menu_border);

    let muted = sound.muted;
    let slider = Rect::new(
        panel.x + SLIDER_MARGIN,
        panel.y + 26,
        PANEL_WIDTH - 2 * SLIDER_MARGIN as u32,
        12,
    );
    let hit = ui.interact(Id::from_name("desktop.volume.slider"), slider, Sense::click());
    if hit.pressed {
        sound.set_level(level_at(ui.pointer().x, slider.x, slider.width) as i32);
        sound.set_muted(false);
    }
    let wheel = ui.wheel();
    if wheel != 0 {
        let change = if wheel > 0 { 5 } else { -5 };
        sound.set_level(sound.level as i32 + change);
    }

    let mut label = feos_text::Text::<24>::new();
    let _ = core::fmt::Write::write_fmt(&mut label, format_args!("Volume: {}%", sound.level));
    ui.painter
        .draw_text(panel.x + SLIDER_MARGIN, panel.y + 8, label.as_str(), theme.menu_text, 1);

    let bar = Rect::new(slider.x, slider.y + 4, slider.width, 4);
    ui.painter.fill_rect(bar, theme.menu_border);
    let fill = slider.width * sound.level as u32 / feos_mixer::MAX_VOLUME as u32;
    ui.painter
        .fill_rect(Rect::new(bar.x, bar.y, fill, bar.height), theme.menu_hover);
    ui.painter
        .fill_rect(Rect::new(bar.x + fill as i32 - 2, slider.y, 4, slider.height), theme.menu_text);

    let mute = Rect::new(panel.x + SLIDER_MARGIN, panel.y + 44, 60, 18);
    let mute_hit = ui.interact(Id::from_name("desktop.volume.mute"), mute, Sense::click());
    if mute_hit.hovered {
        ui.painter.fill_rounded_rect(mute, 3, theme.menu_hover);
    }
    ui.painter.stroke_rounded_rect(mute, 3, 1, theme.menu_border);
    ui.painter
        .draw_text_centered(mute, if muted { "Unmute" } else { "Mute" }, theme.menu_text, 1);
    if mute_hit.clicked {
        sound.set_muted(!muted);
    }
    panel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slider_maps_its_ends_and_clamps_outside_them() {
        assert_eq!(level_at(10, 10, 100), 0);
        assert_eq!(level_at(110, 10, 100), 100);
        assert_eq!(level_at(60, 10, 100), 50);
        assert_eq!(level_at(-50, 10, 100), 0);
        assert_eq!(level_at(500, 10, 100), 100);
    }

    #[test]
    fn a_zero_width_slider_does_not_divide_by_zero() {
        assert_eq!(level_at(5, 5, 0), 0);
    }
}
