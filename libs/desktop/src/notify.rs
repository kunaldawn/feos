//! Desktop notifications: balloon toasts and the tray's history list.

use feos_app::shell::{Level, Notification};
use feos_gfx::Rect;
use feos_gui::{Align, Id, RectExt, Sense, Ui, VerticalAlign};
use feos_text::Text;

use crate::taskbar::TaskbarLayout;
use crate::theme::DesktopTheme;

/// Number of toasts visible at once.
pub const MAX_TOASTS: usize = 3;
/// Number of notifications the history keeps.
pub const HISTORY: usize = 6;
/// How long a toast stays on screen.
pub const TOAST_MS: u64 = 5_000;

const TOAST_WIDTH: u32 = 320;
const TOAST_HEIGHT: u32 = 54;
const TOAST_GAP: i32 = 6;

/// One remembered notification.
#[derive(Clone, Copy, Debug)]
struct Entry {
    title: Text<32>,
    body: Text<96>,
    level: Level,
    /// Uptime at which its toast disappears; 0 once dismissed.
    expires_ms: u64,
    used: bool,
}

impl Entry {
    const EMPTY: Self = Self {
        title: Text::EMPTY,
        body: Text::EMPTY,
        level: Level::Info,
        expires_ms: 0,
        used: false,
    };
}

/// Notification state owned by the desktop.
#[derive(Clone, Copy, Debug)]
pub struct Notifications {
    /// Newest first.
    entries: [Entry; HISTORY],
    /// Notifications that arrived since the history was last opened.
    unread: usize,
}

impl Notifications {
    /// An empty history.
    pub const fn new() -> Self {
        Self {
            entries: [Entry::EMPTY; HISTORY],
            unread: 0,
        }
    }

    /// Adds a notification and shows its toast.
    pub fn push(&mut self, notification: Notification, now_ms: u64) {
        self.entries.copy_within(0..HISTORY - 1, 1);
        self.entries[0] = Entry {
            title: notification.title,
            body: notification.body,
            level: notification.level,
            expires_ms: now_ms.saturating_add(TOAST_MS).max(1),
            used: true,
        };
        self.unread = self.unread.saturating_add(1);
    }

    /// Number of notifications not yet seen in the history list.
    pub const fn unread(&self) -> usize {
        self.unread
    }

    /// Marks the history as seen.
    pub fn mark_read(&mut self) {
        self.unread = 0;
    }

    /// Hides toasts whose time is up. Returns `true` when any went away.
    pub fn expire(&mut self, now_ms: u64) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            if entry.used && entry.expires_ms != 0 && now_ms >= entry.expires_ms {
                entry.expires_ms = 0;
                changed = true;
            }
        }
        changed
    }

    /// Hides every toast.
    pub fn dismiss_all(&mut self) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            if entry.expires_ms != 0 {
                entry.expires_ms = 0;
                changed = true;
            }
        }
        changed
    }

    fn visible(&self) -> impl Iterator<Item = (usize, &Entry)> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.used && entry.expires_ms != 0)
            .take(MAX_TOASTS)
    }

    /// Number of toasts on screen.
    pub fn visible_count(&self) -> usize {
        self.visible().count()
    }

    /// The rectangle of the toast stack, empty when no toast is visible.
    pub fn area(&self, taskbar: &TaskbarLayout, work_area: Rect) -> Rect {
        let count = self.visible_count() as u32;
        if count == 0 {
            return Rect::new(0, 0, 0, 0);
        }
        let height = count * TOAST_HEIGHT + (count - 1) * TOAST_GAP as u32;
        let right = taskbar.tray.right_i32().min(work_area.right_i32());
        Rect::new(
            (right - TOAST_WIDTH as i32 - 4).max(work_area.x),
            (work_area.bottom_i32() - height as i32 - 4).max(work_area.y),
            TOAST_WIDTH.min(work_area.width),
            height.min(work_area.height),
        )
    }

    /// Draws the toast stack and returns `true` when one was dismissed.
    pub fn render_toasts<const N: usize>(
        &mut self,
        ui: &mut Ui<'_, '_, '_, N>,
        theme: &DesktopTheme,
        taskbar: &TaskbarLayout,
        work_area: Rect,
    ) -> bool {
        let area = self.area(taskbar, work_area);
        if area.is_empty() {
            return false;
        }

        let mut y = area.y;
        let mut dismissed = None;
        for (index, entry) in self.visible() {
            let rect = Rect::new(area.x, y, area.width, TOAST_HEIGHT);
            let id = Id::from_name("desktop.toast").child(index as u64 + 1);
            let hit = ui.interact(id, rect, Sense::click());
            draw_entry(ui, theme, rect, entry);
            if hit.clicked {
                dismissed = Some(index);
            }
            y += TOAST_HEIGHT as i32 + TOAST_GAP;
        }
        if let Some(index) = dismissed {
            self.entries[index].expires_ms = 0;
            return true;
        }
        false
    }

    /// Draws the history list above the tray.
    pub fn render_history<const N: usize>(
        &self,
        ui: &mut Ui<'_, '_, '_, N>,
        theme: &DesktopTheme,
        taskbar: &TaskbarLayout,
        work_area: Rect,
    ) -> Rect {
        let rows = self
            .entries
            .iter()
            .filter(|entry| entry.used)
            .count()
            .max(1) as u32;
        let height = rows * (TOAST_HEIGHT + 2) + 30;
        let right = taskbar.tray.right_i32().min(work_area.right_i32());
        let panel = Rect::new(
            (right - TOAST_WIDTH as i32 - 8).max(work_area.x),
            (work_area.bottom_i32() - height as i32 - 4).max(work_area.y),
            (TOAST_WIDTH + 8).min(work_area.width),
            height.min(work_area.height),
        );

        ui.painter.shadow(panel, 6, 8, 120);
        ui.painter.fill_rounded_rect(panel, 4, theme.menu_bg);
        ui.painter
            .stroke_rounded_rect(panel, 4, 1, theme.menu_border);
        ui.painter.draw_text_ellipsis(
            Rect::new(
                panel.x + 8,
                panel.y + 2,
                panel.width.saturating_sub(16),
                24,
            ),
            "Notifications",
            theme.menu_text,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );

        let mut y = panel.y + 28;
        let mut shown = 0;
        for entry in self.entries.iter().filter(|entry| entry.used) {
            draw_entry(
                ui,
                theme,
                Rect::new(panel.x + 4, y, panel.width.saturating_sub(8), TOAST_HEIGHT),
                entry,
            );
            y += TOAST_HEIGHT as i32 + 2;
            shown += 1;
        }
        if shown == 0 {
            ui.painter.draw_text_ellipsis(
                Rect::new(panel.x + 8, y, panel.width.saturating_sub(16), TOAST_HEIGHT),
                "Nothing to report",
                theme.menu_muted,
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
        }
        panel
    }
}

impl Default for Notifications {
    fn default() -> Self {
        Self::new()
    }
}

fn draw_entry<const N: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    rect: Rect,
    entry: &Entry,
) {
    let accent = match entry.level {
        Level::Info => theme.caption_button,
        Level::Error => theme.close_button,
    };
    ui.painter.fill_rounded_rect(rect, 4, theme.menu_bg);
    ui.painter
        .stroke_rounded_rect(rect, 4, 1, theme.menu_border);
    ui.painter
        .fill_rect(Rect::new(rect.x + 1, rect.y + 1, 5, rect.height - 2), accent);
    let picture = match entry.level {
        Level::Info => feos_iconset::Stock::Info,
        Level::Error => feos_iconset::Stock::Error,
    };
    feos_iconset::draw(&mut ui.painter, picture, Rect::new(rect.x + 12, rect.y + 7, 16, 16));
    ui.painter.draw_text_ellipsis(
        Rect::new(rect.x + 34, rect.y + 6, rect.width.saturating_sub(42), 18),
        &feos_text::display_name(entry.title.as_str()),
        theme.menu_text,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
    ui.painter.draw_text_ellipsis(
        Rect::new(
            rect.x + 14,
            rect.y + 28,
            rect.width.saturating_sub(22),
            18,
        ),
        &feos_text::display_name(entry.body.as_str()),
        theme.menu_muted,
        1,
        Align::Start,
        VerticalAlign::Middle,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(title: &str) -> Notification {
        Notification {
            title: Text::from_str(title),
            body: Text::from_str("detail"),
            level: Level::Info,
        }
    }

    #[test]
    fn toasts_expire_and_the_history_remembers_them() {
        let mut notifications = Notifications::new();
        notifications.push(note("one"), 1000);
        assert_eq!(notifications.visible_count(), 1);
        assert_eq!(notifications.unread(), 1);
        assert!(!notifications.expire(1000 + TOAST_MS - 1));
        assert!(notifications.expire(1000 + TOAST_MS));
        assert_eq!(notifications.visible_count(), 0);
        assert_eq!(notifications.unread(), 1);
        notifications.mark_read();
        assert_eq!(notifications.unread(), 0);
    }

    #[test]
    fn the_history_drops_its_oldest_entry() {
        let mut notifications = Notifications::new();
        for index in 0..HISTORY + 3 {
            notifications.push(note(if index == 0 { "first" } else { "later" }), 0);
        }
        assert!(notifications
            .entries
            .iter()
            .all(|entry| entry.title.as_str() == "later"));
        assert_eq!(notifications.visible_count(), MAX_TOASTS);
    }
}
