//! The Windows XP-style Start menu.

use feos_bundle::AppCatalog;
use feos_gfx::{Color, Rect};
use feos_gui::{paint_icon, Align, Icon as GuiIcon, Id, RectExt, Sense, Ui, VerticalAlign};

use crate::config::{StartMenuConfig, TaskbarPosition};
use crate::paint::draw_icon;
use crate::taskbar::TaskbarLayout;
use crate::theme::DesktopTheme;

/// Number of pinned applications shown before "All Programs".
pub const PINNED_APPS: usize = 5;

/// Commands available from the right-hand column and footer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum StartMenuCommand {
    /// No command.
    #[default]
    None,
    /// Open the "My Computer" view.
    MyComputer,
    /// Open the control panel.
    ControlPanel,
    /// Open help.
    Help,
    /// Open the run dialog.
    Run,
    /// Open search.
    Search,
    /// Open desktop settings.
    Settings,
    /// End the desktop session.
    LogOff,
    /// Power off the machine.
    ShutDown,
}

/// Persistent Start-menu state.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StartMenuState {
    /// Whether the menu is visible.
    pub open: bool,
    /// Highlighted application (absolute catalog index).
    pub highlighted: usize,
    /// Whether the all-programs grid is shown.
    pub all_programs: bool,
}

impl StartMenuState {
    /// Creates a closed menu.
    pub const fn new() -> Self {
        Self {
            open: false,
            highlighted: 0,
            all_programs: false,
        }
    }

    /// Opens the menu and highlights the first app.
    pub fn show(&mut self) {
        self.open = true;
        self.highlighted = 0;
        self.all_programs = false;
    }

    /// Closes the menu.
    pub fn close(&mut self) {
        self.open = false;
        self.all_programs = false;
    }

    /// Toggles visibility.
    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.show();
        }
    }

    /// Moves the highlight by `delta`, clamped to the visible apps.
    pub fn move_highlight(&mut self, delta: i32, app_count: usize, all_programs: bool) {
        let limit = if all_programs {
            app_count
        } else {
            app_count.min(PINNED_APPS)
        };
        if limit == 0 {
            self.highlighted = 0;
            return;
        }
        let current = self.highlighted.min(limit - 1) as i32;
        self.highlighted = (current + delta).clamp(0, limit as i32 - 1) as usize;
    }

    /// Moves the highlight in the all-programs grid.
    pub fn move_grid(&mut self, dx: i32, dy: i32, app_count: usize, columns: usize) {
        if app_count == 0 {
            self.highlighted = 0;
            return;
        }
        let columns = columns.max(1) as i32;
        let current = self.highlighted.min(app_count - 1) as i32;
        let next = (current + dx + dy * columns).clamp(0, app_count as i32 - 1);
        self.highlighted = next as usize;
    }

    /// The app that Enter should launch, if any.
    pub fn selected(&self, app_count: usize) -> Option<usize> {
        if app_count == 0 {
            None
        } else {
            Some(self.highlighted.min(app_count - 1))
        }
    }
}

/// Results from one Start-menu frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StartMenuResponse {
    /// Catalog index of an app the user launched.
    pub launched: Option<usize>,
    /// A right-column or footer command.
    pub command: StartMenuCommand,
    /// The menu panel rectangle (useful for outside-click detection).
    pub panel: Rect,
    /// The menu closed itself (Escape or a launch).
    pub closed: bool,
}

/// Draws the Start menu when `state.open` is set.
pub fn render<const N: usize, const A: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    config: &StartMenuConfig,
    taskbar: &TaskbarLayout,
    position: TaskbarPosition,
    catalog: &AppCatalog<A>,
    state: &mut StartMenuState,
) -> StartMenuResponse {
    let mut response = StartMenuResponse::default();
    if !state.open {
        return response;
    }

    let canvas = ui.canvas_rect();
    let max_height = match position {
        TaskbarPosition::Bottom => (taskbar.bar.y - canvas.y - 8).max(120) as u32,
        TaskbarPosition::Top => {
            (canvas.bottom_i32() - taskbar.bar.bottom_i32() - 8).max(120) as u32
        }
    };
    let width = config.width.min(canvas.width.saturating_sub(8)).max(260);
    let height = config.height.min(max_height).max(220);
    let x = (taskbar.bar.x + 2).min(canvas.right_i32() - width as i32 - 2);
    let y = match position {
        TaskbarPosition::Bottom => taskbar.bar.y - height as i32 - 2,
        TaskbarPosition::Top => taskbar.bar.bottom_i32() + 2,
    };
    let panel = Rect::new(x.max(canvas.x + 2), y, width, height);
    response.panel = panel;

    ui.painter.shadow(panel, 8, 10, 130);
    ui.painter.fill_rounded_rect(panel, 8, theme.menu_bg);
    ui.painter
        .stroke_rounded_rect(panel, 8, 1, theme.menu_border);

    // Header ------------------------------------------------------------
    let header_height = 58u32;
    let header = Rect::new(panel.x + 1, panel.y + 1, panel.width - 2, header_height);
    ui.painter.canvas_mut().fill_vertical_gradient(
        header,
        theme.menu_header_top,
        theme.menu_header_bottom,
    );
    ui.painter.fill_circle(
        header.x + 28,
        header.y + 28,
        17,
        theme.start_bottom.lerp(Color::WHITE, 40),
    );
    ui.painter.draw_text_centered(
        Rect::new(header.x + 11, header.y + 11, 34, 34),
        "FE",
        theme.start_text,
        2,
    );
    ui.painter.draw_text(
        header.x + 54,
        header.y + 14,
        "feos user",
        theme.menu_header_text,
        2,
    );
    ui.painter.draw_text(
        header.x + 54,
        header.y + 36,
        "Desktop session",
        theme.menu_header_text.lerp(theme.menu_header_bottom, 120),
        1,
    );

    let two_columns = config.columns >= 2 && panel.width >= 340;
    let left = if two_columns {
        Rect::new(
            panel.x + 1,
            panel.y + header_height as i32,
            panel.width * 58 / 100,
            panel.height - header_height - 2,
        )
    } else {
        Rect::new(
            panel.x + 1,
            panel.y + header_height as i32,
            panel.width - 2,
            panel.height - header_height - 2,
        )
    };
    let right = Rect::new(
        left.right_i32(),
        left.y,
        (panel.right_i32() - left.right_i32() - 1).max(0) as u32,
        left.height,
    );
    if two_columns {
        ui.painter
            .fill_rect(right, theme.menu_bg.lerp(theme.menu_separator, 80));
        ui.painter.fill_rect(
            Rect::new(right.x, right.y, 1, right.height),
            theme.menu_separator,
        );
    }

    let footer_height = 42u32;
    let footer = Rect::new(
        panel.x + 1,
        panel.bottom_i32() - footer_height as i32,
        panel.width - 2,
        footer_height,
    );
    ui.painter.canvas_mut().fill_vertical_gradient(
        footer,
        theme.menu_header_bottom,
        theme.menu_header_top,
    );

    // Application list / grid -------------------------------------------
    let app_area = Rect::new(
        left.x + 4,
        left.y + 4,
        left.width.saturating_sub(8),
        (footer.y - left.y - 8).max(0) as u32,
    );
    let app_count = catalog.len();

    if state.all_programs {
        let back_height = 30u32;
        let grid_area = Rect::new(
            app_area.x,
            app_area.y,
            app_area.width,
            app_area.height.saturating_sub(back_height + 4),
        );
        let columns = 4u32;
        let cell_width = (grid_area.width / columns).max(1);
        let cell_height = 64u32;
        for index in 0..app_count {
            let column = index as u32 % columns;
            let row = index as u32 / columns;
            let cell = Rect::new(
                grid_area.x + (column * cell_width) as i32 + 2,
                grid_area.y + (row * cell_height) as i32 + 2,
                cell_width.saturating_sub(4),
                cell_height.saturating_sub(4),
            );
            if cell.y + cell.height as i32 > grid_area.bottom_i32() {
                break;
            }
            let id = Id::from_name("desktop.start.grid").child(index as u64 + 1);
            let hit = ui.interact(id, cell, Sense::click());
            if hit.hovered || state.highlighted == index {
                ui.painter.fill_rounded_rect(cell, 4, theme.menu_hover);
            }
            if let Some(app) = catalog.get(index) {
                draw_icon(
                    &mut ui.painter,
                    &app.icon,
                    Rect::new(cell.x + (cell.width as i32 - 32) / 2, cell.y + 6, 32, 32),
                    theme.caption_button,
                );
                ui.painter.draw_text_ellipsis(
                    Rect::new(cell.x + 3, cell.y + 40, cell.width.saturating_sub(6), 18),
                    app.name(),
                    if hit.hovered || state.highlighted == index {
                        theme.menu_hover_text
                    } else {
                        theme.menu_text
                    },
                    1,
                    Align::Center,
                    VerticalAlign::Middle,
                );
            }
            if hit.clicked {
                response.launched = Some(index);
            }
        }

        let back = Rect::new(
            app_area.x,
            app_area.bottom_i32() - back_height as i32,
            app_area.width,
            back_height,
        );
        let id = Id::from_name("desktop.start.back");
        let hit = ui.interact(id, back, Sense::click());
        if hit.hovered {
            ui.painter.fill_rounded_rect(back, 3, theme.menu_hover);
        }
        paint_icon(
            &mut ui.painter,
            Rect::new(back.x + 6, back.y + 7, 16, 16),
            GuiIcon::ChevronLeft,
            theme.menu_text,
        );
        ui.painter.draw_text_ellipsis(
            Rect::new(
                back.x + 28,
                back.y,
                back.width.saturating_sub(32),
                back.height,
            ),
            "Back",
            theme.menu_text,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        if hit.clicked {
            state.all_programs = false;
        }
    } else {
        let show_all_height = 30u32;
        let list_area = Rect::new(
            app_area.x,
            app_area.y,
            app_area.width,
            app_area.height.saturating_sub(show_all_height + 4),
        );
        let row_height = if config.show_descriptions {
            44u32
        } else {
            32u32
        };
        let rows = (list_area.height / row_height).max(1) as usize;
        let visible = app_count.min(PINNED_APPS).min(rows);
        for index in 0..visible {
            let row = Rect::new(
                list_area.x,
                list_area.y + (index as u32 * row_height) as i32,
                list_area.width,
                row_height,
            );
            let id = Id::from_name("desktop.start.app").child(index as u64 + 1);
            let hit = ui.interact(id, row, Sense::click());
            if hit.hovered || state.highlighted == index {
                ui.painter.fill_rounded_rect(row, 3, theme.menu_hover);
            }
            if let Some(app) = catalog.get(index) {
                draw_icon(
                    &mut ui.painter,
                    &app.icon,
                    Rect::new(row.x + 4, row.y + 5, 26, 26),
                    theme.caption_button,
                );
                let text_color = if hit.hovered || state.highlighted == index {
                    theme.menu_hover_text
                } else {
                    theme.menu_text
                };
                ui.painter.draw_text_ellipsis(
                    Rect::new(row.x + 36, row.y + 3, row.width.saturating_sub(40), 20),
                    app.name(),
                    text_color,
                    1,
                    Align::Start,
                    VerticalAlign::Middle,
                );
                if config.show_descriptions {
                    ui.painter.draw_text_ellipsis(
                        Rect::new(row.x + 36, row.y + 22, row.width.saturating_sub(40), 16),
                        app.manifest.description.as_str(),
                        if hit.hovered || state.highlighted == index {
                            theme.menu_hover_text.lerp(theme.menu_hover, 70)
                        } else {
                            theme.menu_muted
                        },
                        1,
                        Align::Start,
                        VerticalAlign::Middle,
                    );
                }
            }
            if hit.clicked {
                response.launched = Some(index);
            }
        }

        if app_count == 0 {
            ui.painter.draw_text(
                list_area.x + 8,
                list_area.y + 10,
                "No applications found",
                theme.menu_muted,
                1,
            );
        }

        let all = Rect::new(
            app_area.x,
            app_area.bottom_i32() - show_all_height as i32,
            app_area.width,
            show_all_height,
        );
        let id = Id::from_name("desktop.start.all_programs");
        let hit = ui.interact(id, all, Sense::click());
        if hit.hovered {
            ui.painter.fill_rounded_rect(all, 3, theme.menu_hover);
        }
        paint_icon(
            &mut ui.painter,
            Rect::new(all.x + 6, all.y + 7, 16, 16),
            GuiIcon::Grid,
            theme.menu_text,
        );
        ui.painter.draw_text_ellipsis(
            Rect::new(all.x + 28, all.y, all.width.saturating_sub(32), all.height),
            "All Programs",
            theme.menu_text,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
        if hit.clicked {
            state.all_programs = true;
            state.highlighted = 0;
        }
    }

    // Right column -------------------------------------------------------
    if two_columns {
        let entries = [
            (GuiIcon::Home, "My Computer", StartMenuCommand::MyComputer),
            (
                GuiIcon::Folder,
                "My Documents",
                StartMenuCommand::MyComputer,
            ),
            (
                GuiIcon::Settings,
                "Control Panel",
                StartMenuCommand::ControlPanel,
            ),
            (GuiIcon::Help, "Help and Support", StartMenuCommand::Help),
            (GuiIcon::Search, "Search", StartMenuCommand::Search),
            (GuiIcon::Terminal, "Run...", StartMenuCommand::Run),
            (
                GuiIcon::Sliders,
                "Desktop Settings",
                StartMenuCommand::Settings,
            ),
        ];
        let mut y = right.y + 8;
        for (index, (icon, label, command)) in entries.iter().enumerate() {
            let row = Rect::new(right.x + 6, y, right.width.saturating_sub(12), 32);
            let id = Id::from_name("desktop.start.command").child(index as u64 + 1);
            let hit = ui.interact(id, row, Sense::click());
            if hit.hovered {
                ui.painter.fill_rounded_rect(row, 3, theme.menu_hover);
            }
            paint_icon(
                &mut ui.painter,
                Rect::new(row.x + 5, row.y + 8, 16, 16),
                *icon,
                if hit.hovered {
                    theme.menu_hover_text
                } else {
                    theme.menu_text
                },
            );
            ui.painter.draw_text_ellipsis(
                Rect::new(row.x + 30, row.y, row.width.saturating_sub(34), row.height),
                label,
                if hit.hovered {
                    theme.menu_hover_text
                } else {
                    theme.menu_text
                },
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
            if hit.clicked {
                response.command = *command;
            }
            y += 34;
        }

        ui.painter.fill_rect(
            Rect::new(right.x + 10, y + 4, right.width.saturating_sub(20), 1),
            theme.menu_separator,
        );
        y += 12;
        let user = Rect::new(right.x + 6, y, right.width.saturating_sub(12), 32);
        ui.painter.fill_circle(
            user.x + 12,
            user.y + 16,
            10,
            theme.menu_header_bottom.lerp(Color::WHITE, 40),
        );
        ui.painter.draw_text_centered(
            Rect::new(user.x + 4, user.y + 8, 16, 16),
            "U",
            theme.menu_header_text,
            1,
        );
        ui.painter.draw_text_ellipsis(
            Rect::new(
                user.x + 28,
                user.y,
                user.width.saturating_sub(30),
                user.height,
            ),
            "feos user",
            theme.menu_text,
            1,
            Align::Start,
            VerticalAlign::Middle,
        );
    }

    // Footer -------------------------------------------------------------
    let logoff = Rect::new(
        footer.right_i32() - 214,
        footer.y + 7,
        96,
        footer.height.saturating_sub(14),
    );
    let shutdown = Rect::new(
        footer.right_i32() - 112,
        footer.y + 7,
        104,
        footer.height.saturating_sub(14),
    );
    let logoff_id = Id::from_name("desktop.start.logoff");
    let shutdown_id = Id::from_name("desktop.start.shutdown");
    let logoff_hit = ui.interact(logoff_id, logoff, Sense::click());
    let shutdown_hit = ui.interact(shutdown_id, shutdown, Sense::click());
    for (rect, hit, label, color, command) in [
        (
            logoff,
            logoff_hit,
            "Log Off",
            theme.caption_button,
            StartMenuCommand::LogOff,
        ),
        (
            shutdown,
            shutdown_hit,
            "Turn Off",
            theme.close_button,
            StartMenuCommand::ShutDown,
        ),
    ] {
        ui.painter.fill_rounded_rect(
            rect,
            4,
            if hit.hovered {
                color.lerp(Color::WHITE, 40)
            } else {
                color
            },
        );
        ui.painter
            .stroke_rounded_rect(rect, 4, 1, theme.menu_border);
        ui.painter
            .draw_text_centered(rect, label, theme.menu_header_text, 1);
        if hit.clicked {
            response.command = command;
        }
    }

    if response.launched.is_some() || response.command != StartMenuCommand::None {
        state.close();
        response.closed = true;
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_is_clamped_to_visible_apps() {
        let mut state = StartMenuState::new();
        state.move_highlight(10, 9, false);
        assert_eq!(state.highlighted, PINNED_APPS - 1);
        state.move_highlight(-10, 9, false);
        assert_eq!(state.highlighted, 0);
        state.move_highlight(4, 9, true);
        assert_eq!(state.highlighted, 4);
        state.move_highlight(100, 9, true);
        assert_eq!(state.highlighted, 8);
    }

    #[test]
    fn grid_navigation_wraps_by_row() {
        let mut state = StartMenuState::new();
        state.move_grid(1, 0, 9, 4);
        assert_eq!(state.highlighted, 1);
        state.move_grid(0, 1, 9, 4);
        assert_eq!(state.highlighted, 5);
        state.move_grid(-10, -10, 9, 4);
        assert_eq!(state.highlighted, 0);
    }

    #[test]
    fn selected_uses_the_highlighted_index() {
        let mut state = StartMenuState::new();
        state.highlighted = 3;
        assert_eq!(state.selected(2), Some(1));
        assert_eq!(state.selected(0), None);
    }
}
