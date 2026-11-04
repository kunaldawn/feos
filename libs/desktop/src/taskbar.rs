//! The classic-style taskbar: Start button, quick launch, task buttons and tray.

use feos_bundle::AppCatalog;
use feos_gfx::{Color, Rect};
use feos_gui::{Align, Id, RectExt, Sense, Ui, VerticalAlign};

use crate::config::{TaskbarConfig, TaskbarPosition};
use crate::paint::{draw_icon, draw_start_flag};
use crate::theme::DesktopTheme;
use crate::window::WindowManager;

/// Geometry of the taskbar and its sections.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TaskbarLayout {
    /// The complete bar.
    pub bar: Rect,
    /// Start button.
    pub start: Rect,
    /// Quick-launch strip.
    pub quick: Rect,
    /// Task-button area.
    pub tasks: Rect,
    /// Notification area.
    pub tray: Rect,
}

impl TaskbarLayout {
    /// The area left for windows.
    pub fn work_area(&self, canvas: Rect, position: TaskbarPosition) -> Rect {
        match position {
            TaskbarPosition::Bottom => {
                let height = (self.bar.y - canvas.y).max(0) as u32;
                Rect::new(canvas.x, canvas.y, canvas.width, height)
            }
            TaskbarPosition::Top => {
                let y = self.bar.bottom_i32();
                let height = (canvas.bottom_i32() - y).max(0) as u32;
                Rect::new(canvas.x, y, canvas.width, height)
            }
        }
    }
}

/// Results from one taskbar frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TaskbarResponse {
    /// The Start button was clicked.
    pub start_clicked: bool,
    /// A quick-launch icon was clicked (catalog index).
    pub quick_clicked: Option<usize>,
    /// A task button was clicked (window index).
    pub task_clicked: Option<usize>,
    /// The "show desktop" button was clicked.
    pub desktop_clicked: bool,
    /// The clock was clicked.
    pub tray_clicked: bool,
    /// The bell in the notification area was clicked.
    pub bell_clicked: bool,
    /// A task button was right-clicked.
    pub task_menu: Option<usize>,
}

/// Computes the taskbar layout for `canvas`.
pub fn layout(canvas: Rect, config: &TaskbarConfig) -> TaskbarLayout {
    let height = config.height.clamp(28, canvas.height.max(28));
    let y = match config.position {
        TaskbarPosition::Bottom => canvas.bottom_i32() - height as i32,
        TaskbarPosition::Top => canvas.y,
    };
    let bar = Rect::new(canvas.x, y, canvas.width, height);

    let start_width = (config.start_label.len() as u32 * 16 + 72).clamp(96, 190);
    let start = Rect::new(bar.x + 2, bar.y + 2, start_width, height.saturating_sub(4));

    let quick_width = if config.show_quick_launch {
        (18 + config.quick_launch_count.min(6) as u32 * 30
            + if config.show_desktop { 26 } else { 0 })
        .min(240)
    } else {
        0
    };
    let quick = Rect::new(
        start.right_i32() + 4,
        bar.y + 2,
        quick_width,
        height.saturating_sub(4),
    );

    let tray_width = 56 + if config.show_clock { 76 } else { 0 };
    let tray = Rect::new(
        bar.right_i32() - tray_width,
        bar.y,
        tray_width.min(bar.width as i32).max(0) as u32,
        bar.height,
    );

    let tasks_x = if quick_width > 0 {
        quick.right_i32() + 4
    } else {
        start.right_i32() + 4
    };
    let tasks_width = (tray.x - tasks_x - 4).max(0) as u32;
    let tasks = Rect::new(tasks_x, bar.y + 2, tasks_width, height.saturating_sub(4));

    TaskbarLayout {
        bar,
        start,
        quick,
        tasks,
        tray,
    }
}

/// The width and gap of each task button for `count` open windows.
fn task_button_size(layout: &TaskbarLayout, count: usize) -> (i32, i32) {
    let gap = 3;
    let available = layout.tasks.width as i32;
    // Like XP, buttons shrink as more windows open instead of overflowing
    // past the notification area.
    let width = ((available - gap * (count as i32 - 1)) / count as i32).clamp(32, 168);
    (width.max(1), gap)
}

/// The taskbar button for the window at `index`, if it has one.
///
/// The desktop uses this to tell a foreground app which part of the taskbar
/// represents its window, so a click there can suspend the app and minimise
/// the window instead of being swallowed by the app's own input loop.
pub fn task_button_rect<const W: usize>(
    layout: &TaskbarLayout,
    windows: &WindowManager<W>,
    index: usize,
) -> Option<Rect> {
    let window = windows.get(index)?;
    if !window.open || layout.tasks.width == 0 {
        return None;
    }
    let count = windows.count();
    if count == 0 {
        return None;
    }

    let (width, gap) = task_button_size(layout, count);
    let mut ordinal = 0i32;
    for slot in 0..W {
        let Some(candidate) = windows.get(slot) else {
            break;
        };
        if !candidate.open {
            continue;
        }
        if slot == index {
            let x = layout.tasks.x + ordinal * (width + gap);
            return Some(Rect::new(
                x,
                layout.tasks.y,
                width as u32,
                layout.tasks.height,
            ));
        }
        ordinal += 1;
    }
    None
}

/// The notification bell inside the tray.
pub fn bell_rect(layout: &TaskbarLayout) -> Rect {
    let size = layout.tray.height.saturating_sub(8).min(24);
    Rect::new(
        layout.tray.x + 6,
        layout.tray.y + (layout.tray.height as i32 - size as i32) / 2,
        size,
        size,
    )
}

fn draw_bell<const N: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    rect: Rect,
    theme: &DesktopTheme,
    unread: usize,
) {
    let cx = rect.x + rect.width as i32 / 2;
    let top = rect.y + 4;
    let body = [
        feos_gfx::Point::new(cx - 3, top + 2),
        feos_gfx::Point::new(cx + 3, top + 2),
        feos_gfx::Point::new(cx + 6, top + 11),
        feos_gfx::Point::new(cx - 6, top + 11),
    ];
    ui.painter.fill_polygon(&body, theme.tray_text);
    ui.painter
        .fill_rect(Rect::new(cx - 1, top, 2, 2), theme.tray_text);
    ui.painter
        .fill_rect(Rect::new(cx - 2, top + 13, 4, 2), theme.tray_text);
    if unread > 0 {
        ui.painter.fill_circle(cx + 6, top + 2, 4, theme.close_button);
    }
}

/// Draws the taskbar and returns interaction results.
pub fn render<const N: usize, const W: usize, const A: usize>(
    ui: &mut Ui<'_, '_, '_, N>,
    theme: &DesktopTheme,
    config: &TaskbarConfig,
    layout: &TaskbarLayout,
    windows: &WindowManager<W>,
    catalog: &AppCatalog<A>,
    clock: &str,
    unread: usize,
) -> TaskbarResponse {
    let mut response = TaskbarResponse::default();
    let bar = layout.bar;
    if bar.is_empty() {
        return response;
    }

    ui.painter
        .fill_vertical_gradient(bar, theme.taskbar_top, theme.taskbar_bottom);
    let highlight_y = match config.position {
        TaskbarPosition::Bottom => bar.y,
        TaskbarPosition::Top => bar.bottom_i32() - 1,
    };
    ui.painter.fill_rect(
        Rect::new(bar.x, highlight_y, bar.width, 1),
        theme.taskbar_highlight,
    );

    // Start button -------------------------------------------------------
    let start_id = Id::from_name("desktop.taskbar.start");
    let start = ui.interact(start_id, layout.start, Sense::click());
    let start_color = if start.hovered {
        theme.start_top.lerp(Color::WHITE, 30)
    } else {
        theme.start_top
    };
    ui.painter
        .fill_rounded_rect(layout.start, 5, theme.start_bottom);
    ui.painter.fill_rounded_rect(layout.start, 5, start_color);
    ui.painter
        .stroke_rounded_rect(layout.start, 5, 1, theme.start_border);
    let flag_size = (layout.start.height.saturating_sub(12)).min(20) as i32;
    draw_start_flag(
        &mut ui.painter,
        Rect::new(
            layout.start.x + 9,
            layout.start.y + (layout.start.height as i32 - flag_size) / 2,
            flag_size as u32,
            flag_size as u32,
        ),
    );
    ui.painter.draw_text_ellipsis(
        Rect::new(
            layout.start.x + flag_size + 15,
            layout.start.y,
            layout.start.width.saturating_sub((flag_size + 20) as u32),
            layout.start.height,
        ),
        config.start_label.as_str(),
        theme.start_text,
        2,
        Align::Start,
        VerticalAlign::Middle,
    );
    if start.clicked {
        response.start_clicked = true;
    }

    // Quick launch -------------------------------------------------------
    if config.show_quick_launch && layout.quick.width > 0 {
        ui.painter.fill_rect(layout.quick, theme.quick_bg);
        ui.painter.fill_rect(
            Rect::new(layout.quick.x, layout.quick.y, 1, layout.quick.height),
            theme.taskbar_highlight,
        );

        let mut x = layout.quick.x + 4;
        if config.show_desktop {
            let show_id = Id::from_name("desktop.taskbar.show_desktop");
            let show_rect = Rect::new(
                x,
                layout.quick.y + 3,
                layout.quick.height.saturating_sub(6),
                layout.quick.height.saturating_sub(6),
            );
            let show = ui.interact(show_id, show_rect, Sense::click());
            if show.hovered {
                ui.painter
                    .fill_rounded_rect(show_rect, 3, theme.task_button_hover);
            }
            let monitor = Rect::new(
                show_rect.x + 4,
                show_rect.y + 3,
                show_rect.width.saturating_sub(8),
                show_rect.height.saturating_sub(10),
            );
            ui.painter.stroke_rect(monitor, 1, theme.tray_text);
            ui.painter.fill_rect(
                Rect::new(
                    monitor.x + 3,
                    monitor.y + 3,
                    monitor.width.saturating_sub(6),
                    monitor.height.saturating_sub(6),
                ),
                theme.tray_text,
            );
            ui.painter.fill_rect(
                Rect::new(
                    monitor.x + monitor.width as i32 / 2 - 4,
                    monitor.bottom_i32() + 1,
                    8,
                    2,
                ),
                theme.tray_text,
            );
            if show.clicked {
                response.desktop_clicked = true;
            }
            x = show_rect.right_i32() + 4;
        }

        let quick_apps = catalog.take(config.quick_launch_count as usize);
        let icon_size = layout.quick.height.saturating_sub(8) as i32;
        for (index, app) in quick_apps.iter().enumerate() {
            let rect = Rect::new(x, layout.quick.y + 4, icon_size as u32, icon_size as u32);
            let id = Id::from_name("desktop.taskbar.quick").child(index as u64 + 1);
            let quick = ui.interact(id, rect, Sense::click());
            if quick.hovered {
                ui.painter
                    .fill_rounded_rect(rect, 3, theme.task_button_hover);
            }
            draw_icon(&mut ui.painter, &app.icon, rect, theme.caption_button);
            if quick.clicked {
                response.quick_clicked = Some(index);
            }
            x += icon_size + 4;
        }
    }

    // Task buttons -------------------------------------------------------
    let open_count = windows.count();
    if open_count > 0 && layout.tasks.width > 0 {
        let (width, gap) = task_button_size(layout, open_count);
        let clip = ui.painter.push_clip(layout.tasks);
        let mut x = layout.tasks.x;
        for index in 0..W {
            let Some(window) = windows.get(index) else {
                continue;
            };
            if !window.open {
                continue;
            }
            let rect = Rect::new(x, layout.tasks.y, width.max(1) as u32, layout.tasks.height);
            let id = Id::from_name("desktop.taskbar.task").child(index as u64 + 1);
            let task = ui.interact(id, rect, Sense::click());
            let focused = windows.is_focused(index);
            let fill = if focused {
                theme.task_button_active
            } else if task.hovered {
                theme.task_button_hover
            } else {
                theme.task_button
            };
            ui.painter.fill_rounded_rect(rect, 3, fill);
            ui.painter
                .stroke_rounded_rect(rect, 3, 1, theme.task_border);

            let icon_size = (rect.height.saturating_sub(8)).min(22);
            if let Some(app) = catalog.get(window.app) {
                draw_icon(
                    &mut ui.painter,
                    &app.icon,
                    Rect::new(
                        rect.x + 5,
                        rect.y + (rect.height as i32 - icon_size as i32) / 2,
                        icon_size,
                        icon_size,
                    ),
                    theme.caption_button,
                );
            }
            let text_x = rect.x + icon_size as i32 + 10;
            ui.painter.draw_text_ellipsis(
                Rect::new(
                    text_x,
                    rect.y,
                    (rect.right_i32() - text_x - 4).max(0) as u32,
                    rect.height,
                ),
                window.title.as_str(),
                if window.state == crate::window::WindowState::Minimized {
                    theme.task_text.lerp(theme.taskbar_bottom, 80)
                } else {
                    theme.task_text
                },
                1,
                Align::Start,
                VerticalAlign::Middle,
            );
            if task.clicked {
                response.task_clicked = Some(index);
            }
            if task.secondary_clicked {
                response.task_menu = Some(index);
            }
            x += width + gap;
        }
        ui.painter.restore_clip(clip);
    }

    // Notification area --------------------------------------------------
    // The whole area opens the clock, except the bell, which is registered
    // after it and so takes the press.
    let tray_id = Id::from_name("desktop.taskbar.tray");
    let tray = ui.interact(tray_id, layout.tray, Sense::click());
    let bell = bell_rect(layout);
    let bell_hit = ui.interact(Id::from_name("desktop.taskbar.bell"), bell, Sense::click());
    ui.painter.fill_rect(layout.tray, theme.tray_bg);
    ui.painter.fill_rect(
        Rect::new(layout.tray.x, layout.tray.y, 1, layout.tray.height),
        theme.taskbar_highlight,
    );

    if bell_hit.hovered {
        ui.painter
            .fill_rounded_rect(bell, 3, theme.task_button_hover);
    }
    draw_bell(ui, bell, theme, unread);
    if bell_hit.clicked {
        response.bell_clicked = true;
    }

    if config.show_clock {
        ui.painter.draw_text_ellipsis(
            Rect::new(
                layout.tray.right_i32() - 76,
                layout.tray.y,
                72,
                layout.tray.height,
            ),
            clock,
            theme.tray_text,
            1,
            Align::End,
            VerticalAlign::Middle,
        );
    }
    if tray.clicked && !bell_hit.clicked {
        response.tray_clicked = true;
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bottom_taskbar_leaves_work_area_above_it() {
        let config = TaskbarConfig::default();
        let canvas = Rect::new(0, 0, 800, 600);
        let layout = layout(canvas, &config);
        assert_eq!(layout.bar.y, 560);
        let work = layout.work_area(canvas, config.position);
        assert_eq!(work.height, 560);
        assert_eq!(work.y, 0);
    }

    #[test]
    fn top_taskbar_leaves_work_area_below_it() {
        let mut config = TaskbarConfig::default();
        config.position = TaskbarPosition::Top;
        config.height = 48;
        let canvas = Rect::new(0, 0, 800, 600);
        let layout = layout(canvas, &config);
        assert_eq!(layout.bar.y, 0);
        let work = layout.work_area(canvas, config.position);
        assert_eq!(work.y, 48);
        assert_eq!(work.height, 552);
    }

    #[test]
    fn task_buttons_are_easy_to_locate() {
        use crate::window::WindowManager;

        let config = TaskbarConfig::default();
        let canvas = Rect::new(0, 0, 800, 600);
        let layout = layout(canvas, &config);
        let mut windows = WindowManager::<4>::new();
        let work = layout.work_area(canvas, config.position);
        let first = windows.open(0, "One", work, 300, 200, 10).unwrap();
        let second = windows.open(1, "Two", work, 300, 200, 10).unwrap();

        let first_rect = task_button_rect(&layout, &windows, first).unwrap();
        let second_rect = task_button_rect(&layout, &windows, second).unwrap();
        assert_eq!(first_rect.x, layout.tasks.x);
        assert!(second_rect.x > first_rect.x);
        assert_eq!(first_rect.y, layout.tasks.y);
        assert_eq!(task_button_rect(&layout, &windows, 3), None);
    }

    #[test]
    fn sections_do_not_overlap() {
        let config = TaskbarConfig::default();
        let canvas = Rect::new(0, 0, 800, 600);
        let layout = layout(canvas, &config);
        assert!(layout.start.right_i32() <= layout.quick.x);
        assert!(layout.quick.right_i32() <= layout.tasks.x);
        assert!(layout.tasks.right_i32() <= layout.tray.x);
    }
}
