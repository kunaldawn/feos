//! The desktop window manager.
//!
//! Windows are stored in a fixed-size table and identified by their slot
//! index. Every window has a z-order value, a normal rectangle and a saved
//! rectangle for maximise/restore. The manager owns no pixels and no
//! allocation; the renderer reads it and draws in z-order.

use feos_app::App;
use feos_gfx::{Point, Rect};
use feos_gui::RectExt;
use feos_text::Text;

/// Default maximum number of open windows.
pub const MAX_WINDOWS: usize = 8;

/// Maximum title length.
pub const TITLE_MAX: usize = 48;

/// Smallest width a resized window may have.
pub const MIN_WINDOW_WIDTH: u32 = 240;

/// Smallest height a resized window may have.
pub const MIN_WINDOW_HEIGHT: u32 = 160;

/// [`DesktopWindow::app`] for a window whose bundle is no longer installed.
pub const NO_APP: usize = usize::MAX;

/// Height of the managed title bar.
pub const TITLE_HEIGHT: u32 = 28;

/// The content rectangle of a managed window.
pub fn content_rect(rect: Rect) -> Rect {
    Rect::new(
        rect.x + 1,
        rect.y + TITLE_HEIGHT as i32 + 1,
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(TITLE_HEIGHT + 2),
    )
}

/// How a window is currently displayed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowState {
    /// Normal floating window.
    Normal,
    /// Hidden behind the taskbar button.
    Minimized,
    /// Filling the work area.
    Maximized,
}

/// The edge or corner being dragged to resize a window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResizeEdge {
    /// Top edge.
    Top,
    /// Bottom edge.
    Bottom,
    /// Left edge.
    Left,
    /// Right edge.
    Right,
    /// Top-left corner.
    TopLeft,
    /// Top-right corner.
    TopRight,
    /// Bottom-left corner.
    BottomLeft,
    /// Bottom-right corner.
    BottomRight,
}

impl ResizeEdge {
    /// Returns `true` when the edge moves the left side of the window.
    pub const fn moves_left(self) -> bool {
        matches!(self, Self::Left | Self::TopLeft | Self::BottomLeft)
    }

    /// Returns `true` when the edge moves the right side of the window.
    pub const fn moves_right(self) -> bool {
        matches!(self, Self::Right | Self::TopRight | Self::BottomRight)
    }

    /// Returns `true` when the edge moves the top side of the window.
    pub const fn moves_top(self) -> bool {
        matches!(self, Self::Top | Self::TopLeft | Self::TopRight)
    }

    /// Returns `true` when the edge moves the bottom side of the window.
    pub const fn moves_bottom(self) -> bool {
        matches!(self, Self::Bottom | Self::BottomLeft | Self::BottomRight)
    }
}

/// Why a window has no running application to show.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum WindowStatus {
    /// Nothing went wrong.
    #[default]
    Ready,
    /// The manifest's entry point is not in the shell registry.
    UnknownEntry,
    /// The kernel refused to create a process.
    SpawnFailed,
    /// The app could not open its window.
    OpenFailed,
}

/// One managed window.
#[derive(Clone, Copy)]
pub struct DesktopWindow {
    /// Catalog index of the application this window belongs to.
    ///
    /// [`bundle`](Self::bundle) is the stable identity: a catalog rescan
    /// recomputes this index from it, and sets it to [`NO_APP`] when the
    /// bundle has gone away.
    pub app: usize,
    /// Manifest id of the application this window belongs to.
    pub bundle: Text<32>,
    /// Arguments the application was opened with.
    pub args: Text<96>,
    /// Why the window has nothing to show, if so.
    pub status: WindowStatus,
    /// The title bar has moved since the current press began, so its release
    /// is the end of a drag and not half of a double click.
    pub title_moved: bool,
    /// Minimized by "Show Desktop", which may restore it again.
    pub hidden_by_show_desktop: bool,
    /// Uptime of the app's last frame, for per-window animation periods.
    pub last_frame_ms: u64,
    /// Time spent drawing this window's app, in microseconds.
    pub cpu_us: u64,
    /// The status its app's task died with, if it did.
    pub failed: Option<i32>,
    /// Window title.
    pub title: Text<TITLE_MAX>,
    /// Current rectangle.
    pub rect: Rect,
    /// Rectangle restored when leaving maximised state.
    pub restore: Rect,
    /// Display state.
    pub state: WindowState,
    /// State restored when the window leaves the minimized state.
    pub restore_state: WindowState,
    /// Whether a foreground app was suspended by clicking its taskbar button.
    pub suspended: bool,
    /// The windowed application hosted by this window, if any.
    pub runtime: Option<&'static dyn App>,
    /// Whether [`App::open_window`] has been called for this window.
    pub app_started: bool,
    /// Whether the slot is in use.
    pub open: bool,
    /// Z-order (larger is closer to the viewer).
    pub z: u32,
    /// PID of a background process launched from this window, if any.
    pub pid: Option<u32>,
    /// Whether the app has been launched in the background.
    pub launched: bool,
    /// The pointer the hosted app asked for over its content in its last frame.
    pub cursor: feos_gui::CursorShape,
    /// The app draws its own title bar and border: the desktop draws no frame,
    /// and the whole rectangle is content.
    pub frameless: bool,
}

impl DesktopWindow {
    /// An unused slot.
    pub const EMPTY: Self = Self {
        app: 0,
        bundle: Text::EMPTY,
        args: Text::EMPTY,
        status: WindowStatus::Ready,
        title_moved: false,
        hidden_by_show_desktop: false,
        last_frame_ms: 0,
    cpu_us: 0,
    failed: None,
        title: Text::EMPTY,
        rect: Rect::new(0, 0, 0, 0),
        restore: Rect::new(0, 0, 0, 0),
        state: WindowState::Normal,
        restore_state: WindowState::Normal,
        suspended: false,
        runtime: None,
        app_started: false,
        open: false,
        z: 0,
        pid: None,
        launched: false,
        cursor: feos_gui::CursorShape::Arrow,
        frameless: false,
    };

    /// Where the app draws: inside the frame, or everything if it has none.
    pub fn content(&self) -> Rect {
        if self.frameless {
            self.rect
        } else {
            content_rect(self.rect)
        }
    }
}

/// A fixed-capacity window manager.
pub struct WindowManager<const N: usize = MAX_WINDOWS> {
    windows: [DesktopWindow; N],
    next_z: u32,
    focused: Option<usize>,
}

impl<const N: usize> WindowManager<N> {
    /// Creates an empty manager.
    pub const fn new() -> Self {
        Self {
            windows: [DesktopWindow::EMPTY; N],
            next_z: 1,
            focused: None,
        }
    }

    /// Opens a window for `app`, or focuses an existing one.
    ///
    /// The initial rectangle is cascaded from the top-left of `work_area` and
    /// clamped so the window is always reachable.
    pub fn open(
        &mut self,
        app: usize,
        title: &str,
        work_area: Rect,
        default_width: u32,
        default_height: u32,
        cascade: i32,
    ) -> Option<usize> {
        if let Some(index) = self
            .windows
            .iter()
            .position(|window| window.open && window.app == app)
        {
            if self.windows[index].state == WindowState::Minimized {
                self.windows[index].state = self.windows[index].restore_state;
            }
            self.windows[index].suspended = false;
            self.focus(index);
            return Some(index);
        }
        self.open_new(app, title, work_area, default_width, default_height, cascade)
    }

    /// Opens a window for `app`, even if it already has one.
    pub fn open_new(
        &mut self,
        app: usize,
        title: &str,
        work_area: Rect,
        default_width: u32,
        default_height: u32,
        cascade: i32,
    ) -> Option<usize> {
        let index = self.windows.iter().position(|window| !window.open)?;
        let offset = (self.next_z as i32 % 6) * cascade.max(0);
        // Keep the wallpaper's icon column visible for normal windows. The
        // user can still maximise or resize over it afterwards.
        let icon_margin = if work_area.width >= 900 { 160 } else { 48 };
        let width = default_width
            .min(work_area.width.saturating_sub(icon_margin).max(1))
            .max(1);
        let height = default_height.min(work_area.height.max(1)).max(1);
        let max_x = (work_area.right_i32() - width as i32).max(work_area.x);
        let max_y = (work_area.bottom_i32() - height as i32).max(work_area.y);
        // Leave the wallpaper's icon column visible, the way XP keeps the
        // left edge of the desktop usable when a window opens.
        let x = (work_area.x + icon_margin as i32 + offset).min(max_x);
        let y = (work_area.y + 24 + offset).min(max_y);
        let rect = Rect::new(x, y, width, height);

        let window = &mut self.windows[index];
        window.app = app;
        window.bundle.clear();
        window.args.clear();
        window.status = WindowStatus::Ready;
        window.title_moved = false;
        window.hidden_by_show_desktop = false;
        window.last_frame_ms = 0;
        window.title.set(title);
        window.rect = rect;
        window.restore = rect;
        window.state = WindowState::Normal;
        window.restore_state = WindowState::Normal;
        window.suspended = false;
        window.runtime = None;
        window.app_started = false;
        window.open = true;
        window.pid = None;
        window.launched = false;
        window.frameless = false;
        self.focus(index);
        Some(index)
    }

    /// The open window showing catalog entry `app`, if any.
    pub fn get_open_for(&self, app: usize) -> Option<usize> {
        self.windows
            .iter()
            .position(|window| window.open && window.app == app)
    }

    /// Number of open windows.
    pub fn count(&self) -> usize {
        self.windows.iter().filter(|window| window.open).count()
    }

    /// Returns `true` when no windows are open.
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// Returns the window at `index`, even when it is closed.
    pub fn get(&self, index: usize) -> Option<&DesktopWindow> {
        self.windows.get(index)
    }

    /// Returns a mutable window at `index`.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut DesktopWindow> {
        self.windows.get_mut(index)
    }

    /// The focused slot, if any.
    pub const fn focused(&self) -> Option<usize> {
        self.focused
    }

    /// Returns `true` when `index` is the focused window.
    pub fn is_focused(&self, index: usize) -> bool {
        self.focused == Some(index)
    }

    /// The focused window, if any.
    pub fn focused_window(&self) -> Option<&DesktopWindow> {
        self.focused
            .and_then(|index| self.windows.get(index))
            .filter(|window| window.open)
    }

    /// Brings a window to the front.
    pub fn focus(&mut self, index: usize) -> bool {
        if !self.windows.get(index).is_some_and(|window| window.open) {
            return false;
        }
        let z = self.next_z;
        self.next_z = self.next_z.wrapping_add(1).max(1);
        self.windows[index].z = z;
        self.focused = Some(index);
        true
    }

    /// Clears keyboard focus, as clicking the wallpaper does in XP.
    pub fn clear_focus(&mut self) {
        self.focused = None;
    }

    /// Focuses the next visible window in slot order.
    ///
    /// Minimized windows are skipped: they cannot take keyboard input, and
    /// focusing one would leave every key going nowhere.
    pub fn focus_next(&mut self) -> Option<usize> {
        let start = self.focused.map(|index| index + 1).unwrap_or(0);
        for step in 0..N {
            let index = (start + step) % N.max(1);
            let window = &self.windows[index];
            if window.open && window.state != WindowState::Minimized {
                self.focus(index);
                return Some(index);
            }
        }
        self.focused = None;
        None
    }

    /// Switches to the window that has gone longest without focus, restoring
    /// it if it is minimized. Repeating this visits every open window.
    pub fn cycle_forward(&mut self) -> Option<usize> {
        let index = self
            .windows
            .iter()
            .enumerate()
            .filter(|(index, window)| window.open && self.focused != Some(*index))
            .min_by_key(|(_, window)| window.z)
            .map(|(index, _)| index)?;
        if self.windows[index].state == WindowState::Minimized {
            self.windows[index].state = self.windows[index].restore_state;
        }
        self.focus(index);
        Some(index)
    }

    /// Undoes [`cycle_forward`](Self::cycle_forward): sends the focused window
    /// to the back and focuses whichever window is now on top.
    pub fn cycle_backward(&mut self) -> Option<usize> {
        if let Some(index) = self.focused {
            let lowest = self
                .windows
                .iter()
                .filter(|window| window.open)
                .map(|window| window.z)
                .min()
                .unwrap_or(1);
            if lowest <= 1 {
                self.renumber();
            }
            let lowest = self
                .windows
                .iter()
                .filter(|window| window.open)
                .map(|window| window.z)
                .min()
                .unwrap_or(2);
            self.windows[index].z = lowest.saturating_sub(1);
            self.focused = None;
        }
        let index = self.topmost_open(None)?;
        self.focus(index);
        Some(index)
    }

    /// Spreads z values out so a window can be placed below the lowest one.
    fn renumber(&mut self) {
        let mut order = [0usize; N];
        let mut count = 0;
        for (index, window) in self.windows.iter().enumerate() {
            if !window.open {
                continue;
            }
            let mut position = count;
            while position > 0 && self.windows[order[position - 1]].z > window.z {
                order[position] = order[position - 1];
                position -= 1;
            }
            order[position] = index;
            count += 1;
        }
        for (rank, index) in order[..count].iter().enumerate() {
            self.windows[*index].z = rank as u32 + 2;
        }
        self.next_z = count as u32 + 2;
    }

    /// Closes a window.
    pub fn close(&mut self, index: usize) -> bool {
        let Some(window) = self.windows.get_mut(index) else {
            return false;
        };
        if !window.open {
            return false;
        }
        window.open = false;
        window.pid = None;
        window.launched = false;
        window.suspended = false;
        window.runtime = None;
        window.app_started = false;
        if self.focused == Some(index) {
            self.focused = self.topmost_open(None);
        }
        true
    }

    /// Minimises a window.
    pub fn minimize(&mut self, index: usize) -> bool {
        let Some(window) = self.windows.get_mut(index) else {
            return false;
        };
        if !window.open || window.state == WindowState::Minimized {
            return false;
        }
        // Remember whether the window was maximised so restoring it from the
        // taskbar returns it to the same size, exactly like XP.
        window.restore_state = window.state;
        window.state = WindowState::Minimized;
        if self.focused == Some(index) {
            self.focused = self.topmost_open(Some(index));
        }
        true
    }

    /// Restores a minimised window and focuses it.
    pub fn restore(&mut self, index: usize) -> bool {
        let Some(window) = self.windows.get_mut(index) else {
            return false;
        };
        if !window.open || window.state != WindowState::Minimized {
            return false;
        }
        window.state = window.restore_state;
        self.focus(index);
        true
    }

    /// Minimises a normal window or restores a minimised one.
    pub fn toggle_minimize(&mut self, index: usize) -> bool {
        let minimized = self
            .windows
            .get(index)
            .is_some_and(|window| window.state == WindowState::Minimized);
        if minimized {
            self.restore(index)
        } else {
            self.minimize(index)
        }
    }

    /// Toggles maximised state, saving or restoring the normal rectangle.
    pub fn toggle_maximize(&mut self, index: usize, work_area: Rect) -> bool {
        let Some(window) = self.windows.get_mut(index) else {
            return false;
        };
        if !window.open {
            return false;
        }
        if window.state == WindowState::Minimized {
            window.state = window.restore_state;
        }
        if window.state == WindowState::Maximized {
            window.rect = window.restore;
            window.state = WindowState::Normal;
            window.restore_state = WindowState::Normal;
        } else {
            window.restore = window.rect;
            window.rect = work_area;
            window.state = WindowState::Maximized;
            window.restore_state = WindowState::Maximized;
        }
        self.focus(index);
        true
    }

    /// Fits every window to a new work area, after the screen changed size. A maximised
    /// one takes all of it, and the rest stay reachable and no larger than it is.
    pub fn fit_to(&mut self, work_area: Rect) {
        let fit = |rect: Rect| {
            let width = rect.width.min(work_area.width).max(1);
            let height = rect.height.min(work_area.height).max(1);
            let max_x = (work_area.right_i32() - width as i32).max(work_area.x);
            let max_y = (work_area.bottom_i32() - height as i32).max(work_area.y);
            Rect::new(
                rect.x.clamp(work_area.x, max_x),
                rect.y.clamp(work_area.y, max_y),
                width,
                height,
            )
        };
        for window in self.windows.iter_mut().filter(|window| window.open) {
            window.restore = fit(window.restore);
            let maximized = window.state == WindowState::Maximized
                || (window.state == WindowState::Minimized
                    && window.restore_state == WindowState::Maximized);
            window.rect = if maximized { work_area } else { fit(window.rect) };
        }
    }

    /// Moves a normal window, keeping at least part of it visible.
    pub fn move_by(&mut self, index: usize, dx: i32, dy: i32, work_area: Rect) -> bool {
        let Some(window) = self.windows.get_mut(index) else {
            return false;
        };
        if !window.open || window.state != WindowState::Normal {
            return false;
        }
        // On a very small work area these bounds can cross; keeping the title
        // bar reachable matters more than the other edge then.
        let min_x = work_area.x - window.rect.width as i32 + 48;
        let max_x = (work_area.right_i32() - 48).max(min_x);
        let min_y = work_area.y;
        let max_y = (work_area.bottom_i32() - 28).max(min_y);
        window.rect.x = window.rect.x.saturating_add(dx).clamp(min_x, max_x);
        window.rect.y = window.rect.y.saturating_add(dy).clamp(min_y, max_y);
        window.restore = window.rect;
        true
    }

    /// Resizes a normal window from its lower-right corner.
    pub fn resize_by(&mut self, index: usize, dw: i32, dh: i32, work_area: Rect) -> bool {
        self.resize_from(index, ResizeEdge::BottomRight, dw, dh, work_area)
    }

    /// Resizes a normal window by dragging one edge or corner.
    ///
    /// The window is kept inside `work_area` (so it never covers the taskbar)
    /// and never shrinks below [`MIN_WINDOW_WIDTH`] by [`MIN_WINDOW_HEIGHT`].
    pub fn resize_from(
        &mut self,
        index: usize,
        edge: ResizeEdge,
        dx: i32,
        dy: i32,
        work_area: Rect,
    ) -> bool {
        let Some(window) = self.windows.get_mut(index) else {
            return false;
        };
        if !window.open || window.state != WindowState::Normal {
            return false;
        }

        // Hosted apps lay themselves out against a minimum content size, so
        // keep their desktop frames large enough to avoid clipping their UI.
        let (min_width, min_height) = if window.runtime.is_some() {
            ((720 + 2), (480 + TITLE_HEIGHT as i32 + 2))
        } else {
            (MIN_WINDOW_WIDTH as i32, MIN_WINDOW_HEIGHT as i32)
        };
        let mut left = window.rect.x;
        let mut top = window.rect.y;
        let mut right = window.rect.right_i32();
        let mut bottom = window.rect.bottom_i32();

        if edge.moves_left() {
            let min_left = work_area.x.min(right - min_width);
            left = left.saturating_add(dx).clamp(min_left, right - min_width);
        } else if edge.moves_right() {
            let max_right = work_area.right_i32().max(left + min_width);
            right = right.saturating_add(dx).clamp(left + min_width, max_right);
        }

        if edge.moves_top() {
            let min_top = work_area.y.min(bottom - min_height);
            top = top.saturating_add(dy).clamp(min_top, bottom - min_height);
        } else if edge.moves_bottom() {
            let max_bottom = work_area.bottom_i32().max(top + min_height);
            bottom = bottom
                .saturating_add(dy)
                .clamp(top + min_height, max_bottom);
        }

        window.rect = Rect::new(
            left,
            top,
            (right - left).max(1) as u32,
            (bottom - top).max(1) as u32,
        );
        window.restore = window.rect;
        true
    }

    /// Recomputes every window's catalog index after a rescan, using `find` to
    /// turn its bundle id into the new index.
    pub fn remap_apps(&mut self, mut find: impl FnMut(&str) -> Option<usize>) {
        for window in &mut self.windows {
            if window.open {
                window.app = find(window.bundle.as_str()).unwrap_or(NO_APP);
            }
        }
    }

    /// Returns the topmost window containing `point`, if any.
    pub fn topmost_at(&self, point: Point) -> Option<usize> {
        self.windows
            .iter()
            .enumerate()
            .filter(|(_, window)| window.open && window.state != WindowState::Minimized)
            .filter(|(_, window)| window.rect.contains(point))
            .max_by_key(|(_, window)| window.z)
            .map(|(index, _)| index)
    }

    /// Returns the topmost window whose content area contains `point`.
    ///
    /// Title bars and the resize border belong to the desktop window manager,
    /// so app pointer events are routed through this narrower test. The inset
    /// matches the transparent resize border drawn by the desktop.
    pub fn topmost_content_at(&self, point: Point) -> Option<usize> {
        const BORDER: i32 = 4;
        self.windows
            .iter()
            .enumerate()
            .filter(|(_, window)| window.open && window.state != WindowState::Minimized)
            .filter(|(_, window)| {
                if window.frameless {
                    return window.rect.contains(point);
                }
                content_rect(window.rect)
                    .inset_each(BORDER, 0, BORDER, BORDER)
                    .contains(point)
            })
            .max_by_key(|(_, window)| window.z)
            .map(|(index, _)| index)
    }

    /// Fills `out` with open window indices in ascending z-order and returns
    /// the number written.
    pub fn z_order(&self, out: &mut [usize; N]) -> usize {
        let mut count = 0;
        for (index, window) in self.windows.iter().enumerate() {
            if !window.open || window.state == WindowState::Minimized {
                continue;
            }
            let mut position = count;
            while position > 0 && self.windows[out[position - 1]].z > window.z {
                out[position] = out[position - 1];
                position -= 1;
            }
            out[position] = index;
            count += 1;
        }
        count
    }

    fn topmost_open(&self, exclude: Option<usize>) -> Option<usize> {
        self.windows
            .iter()
            .enumerate()
            .filter(|(index, window)| {
                window.open && window.state != WindowState::Minimized && Some(*index) != exclude
            })
            .max_by_key(|(_, window)| window.z)
            .map(|(index, _)| index)
    }
}

impl<const N: usize> Default for WindowManager<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod fit_tests {
    use super::*;

    #[test]
    fn windows_follow_the_screen_size() {
        let mut windows = WindowManager::<4>::new();
        let large = Rect::new(0, 0, 1600, 1160);
        let normal = windows.open_new(1, "normal", large, 500, 300, 0).unwrap();
        let wide = windows.open_new(2, "wide", large, 1500, 1000, 0).unwrap();
        let maximized = windows.open_new(3, "max", large, 500, 300, 0).unwrap();
        assert!(windows.toggle_maximize(maximized, large));
        assert_eq!(windows.get(maximized).unwrap().rect, large);

        let small = Rect::new(0, 0, 800, 560);
        windows.fit_to(small);
        assert_eq!(windows.get(maximized).unwrap().rect, small);
        let rect = windows.get(wide).unwrap().rect;
        assert!(rect.width <= small.width && rect.height <= small.height);
        assert!(rect.x >= 0 && rect.y >= 0);
        let rect = windows.get(normal).unwrap().rect;
        assert!(rect.x + rect.width as i32 <= 800 && rect.y + rect.height as i32 <= 560);
        // Its restored size is on the screen too.
        assert!(windows.toggle_maximize(maximized, small));
        let rect = windows.get(maximized).unwrap().rect;
        assert!(rect.x + rect.width as i32 <= 800);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: Rect = Rect::new(0, 0, 800, 600);

    #[test]
    fn opens_focuses_and_closes() {
        let mut wm = WindowManager::<4>::new();
        let first = wm.open(0, "One", WORK, 400, 300, 20).unwrap();
        let second = wm.open(1, "Two", WORK, 400, 300, 20).unwrap();
        assert_eq!(wm.count(), 2);
        assert_eq!(wm.focused(), Some(second));
        assert!(wm.get(first).unwrap().rect.x >= WORK.x);

        wm.focus(first);
        assert_eq!(wm.focused(), Some(first));
        assert_eq!(
            wm.topmost_at(Point::new(
                wm.get(first).unwrap().rect.x + 4,
                wm.get(first).unwrap().rect.y + 4
            )),
            Some(first)
        );

        assert!(wm.close(first));
        assert_eq!(wm.count(), 1);
        assert_eq!(wm.focused(), Some(second));
    }

    #[test]
    fn opening_the_same_app_focuses_it() {
        let mut wm = WindowManager::<4>::new();
        let first = wm.open(7, "Seven", WORK, 300, 200, 10).unwrap();
        wm.open(1, "Other", WORK, 300, 200, 10).unwrap();
        let again = wm.open(7, "Seven", WORK, 300, 200, 10).unwrap();
        assert_eq!(first, again);
        assert_eq!(wm.count(), 2);
        assert_eq!(wm.focused(), Some(first));
    }

    #[test]
    fn minimizes_and_restores() {
        let mut wm = WindowManager::<4>::new();
        let index = wm.open(0, "One", WORK, 300, 200, 10).unwrap();
        assert!(wm.minimize(index));
        assert_eq!(wm.get(index).unwrap().state, WindowState::Minimized);
        assert_eq!(wm.focused(), None);
        assert!(wm.restore(index));
        assert_eq!(wm.focused(), Some(index));
        assert!(wm.toggle_minimize(index));
        assert_eq!(wm.get(index).unwrap().state, WindowState::Minimized);
    }

    #[test]
    fn a_frameless_window_is_all_content() {
        let mut wm = WindowManager::<4>::new();
        let index = wm.open(0, "Amp", WORK, 275, 116, 10).unwrap();
        let rect = wm.get(index).unwrap().rect;
        // With a frame, the top edge is the title bar's.
        assert_eq!(wm.topmost_content_at(Point::new(rect.x + 10, rect.y + 2)), None);
        wm.get_mut(index).unwrap().frameless = true;
        assert_eq!(wm.get(index).unwrap().content(), rect);
        assert_eq!(wm.topmost_content_at(Point::new(rect.x + 10, rect.y + 2)), Some(index));
        assert_eq!(wm.topmost_content_at(Point::new(rect.x, rect.y)), Some(index));
    }

    #[test]
    fn maximizes_and_restores_the_saved_rect() {
        let mut wm = WindowManager::<4>::new();
        let index = wm.open(0, "One", WORK, 300, 200, 10).unwrap();
        let before = wm.get(index).unwrap().rect;
        assert!(wm.toggle_maximize(index, WORK));
        assert_eq!(wm.get(index).unwrap().rect, WORK);
        assert!(wm.toggle_maximize(index, WORK));
        assert_eq!(wm.get(index).unwrap().rect, before);
    }

    #[test]
    fn cycles_focus_and_orders_z() {
        let mut wm = WindowManager::<4>::new();
        let a = wm.open(0, "A", WORK, 300, 200, 10).unwrap();
        let b = wm.open(1, "B", WORK, 300, 200, 10).unwrap();
        assert_eq!(wm.focus_next(), Some(a));
        assert_eq!(wm.focus_next(), Some(b));

        let mut order = [0usize; 4];
        let count = wm.z_order(&mut order);
        assert_eq!(count, 2);
        assert_eq!(order[..count], [a, b]);
    }

    #[test]
    fn moving_clamps_to_work_area() {
        let mut wm = WindowManager::<4>::new();
        let index = wm.open(0, "One", WORK, 300, 200, 10).unwrap();
        assert!(wm.move_by(index, 10000, 10000, WORK));
        let rect = wm.get(index).unwrap().rect;
        assert!(rect.x < WORK.right_i32());
        assert!(rect.y < WORK.bottom_i32());
        assert!(wm.resize_by(index, 10000, 10000, WORK));
        assert!(wm.get(index).unwrap().rect.width <= WORK.width);
    }

    #[test]
    fn content_hit_test_excludes_the_title_bar() {
        let mut wm = WindowManager::<4>::new();
        let index = wm.open(0, "One", WORK, 400, 300, 10).unwrap();
        let rect = Rect::new(10, 10, 400, 300);
        wm.get_mut(index).unwrap().rect = rect;

        assert_eq!(wm.topmost_content_at(Point::new(60, 60)), Some(index));
        assert_eq!(wm.topmost_content_at(Point::new(60, rect.y + 2)), None);
        assert_eq!(wm.topmost_at(Point::new(60, rect.y + 2)), Some(index));
    }

    #[test]
    fn resizes_from_every_edge_and_corner() {
        let mut wm = WindowManager::<4>::new();
        let index = wm.open(0, "One", WORK, 400, 300, 10).unwrap();
        wm.get_mut(index).unwrap().rect = Rect::new(100, 100, 400, 300);
        let before = wm.get(index).unwrap().rect;

        assert!(wm.resize_from(index, ResizeEdge::Right, 40, 0, WORK));
        assert_eq!(wm.get(index).unwrap().rect.width, before.width + 40);
        assert_eq!(wm.get(index).unwrap().rect.x, before.x);

        assert!(wm.resize_from(index, ResizeEdge::Bottom, 0, 30, WORK));
        assert_eq!(wm.get(index).unwrap().rect.height, before.height + 30);

        assert!(wm.resize_from(index, ResizeEdge::Left, 20, 0, WORK));
        let rect = wm.get(index).unwrap().rect;
        assert_eq!(rect.x, before.x + 20);
        assert_eq!(rect.width, before.width + 20);

        assert!(wm.resize_from(index, ResizeEdge::Top, 0, 10, WORK));
        let rect = wm.get(index).unwrap().rect;
        assert_eq!(rect.y, before.y + 10);
        assert_eq!(rect.height, before.height + 20);

        // Dragging too far in either direction clamps to the minimum size.
        assert!(wm.resize_from(index, ResizeEdge::Left, 10000, 0, WORK));
        assert_eq!(wm.get(index).unwrap().rect.width, MIN_WINDOW_WIDTH);
        assert!(wm.resize_from(index, ResizeEdge::Top, 0, 10000, WORK));
        assert_eq!(wm.get(index).unwrap().rect.height, MIN_WINDOW_HEIGHT);

        // And to the work area.
        assert!(wm.resize_from(index, ResizeEdge::Right, 10000, 0, WORK));
        assert!(wm.resize_from(index, ResizeEdge::Bottom, 0, 10000, WORK));
        let rect = wm.get(index).unwrap().rect;
        assert_eq!(rect.right_i32(), WORK.right_i32());
        assert_eq!(rect.bottom_i32(), WORK.bottom_i32());
    }

    #[test]
    fn a_minimized_maximized_window_restores_maximized() {
        let mut wm = WindowManager::<4>::new();
        let index = wm.open(0, "One", WORK, 300, 200, 10).unwrap();
        assert!(wm.toggle_maximize(index, WORK));
        assert_eq!(wm.get(index).unwrap().state, WindowState::Maximized);

        assert!(wm.minimize(index));
        assert_eq!(wm.get(index).unwrap().state, WindowState::Minimized);
        assert!(wm.restore(index));
        assert_eq!(wm.get(index).unwrap().state, WindowState::Maximized);
        assert_eq!(wm.get(index).unwrap().rect, WORK);
    }

    #[test]
    fn tab_focus_skips_minimized_windows() {
        let mut wm = WindowManager::<4>::new();
        let a = wm.open(0, "A", WORK, 300, 200, 10).unwrap();
        let b = wm.open(1, "B", WORK, 300, 200, 10).unwrap();
        wm.minimize(a);
        assert_eq!(wm.focus_next(), Some(b));
        assert_eq!(wm.focus_next(), Some(b));
        wm.minimize(b);
        assert_eq!(wm.focus_next(), None);
        assert_eq!(wm.focused(), None);
    }

    #[test]
    fn cycling_forward_visits_every_window_and_restores_minimized_ones() {
        let mut wm = WindowManager::<4>::new();
        let a = wm.open(0, "A", WORK, 300, 200, 10).unwrap();
        let b = wm.open(1, "B", WORK, 300, 200, 10).unwrap();
        let c = wm.open(2, "C", WORK, 300, 200, 10).unwrap();
        wm.minimize(b);
        wm.focus(c);
        assert_eq!(wm.cycle_forward(), Some(a));
        assert_eq!(wm.cycle_forward(), Some(b));
        assert_eq!(wm.get(b).unwrap().state, WindowState::Normal);
        assert_eq!(wm.cycle_forward(), Some(c));
        assert_eq!(wm.cycle_forward(), Some(a));
    }

    #[test]
    fn cycling_backward_sends_the_focused_window_to_the_back() {
        let mut wm = WindowManager::<4>::new();
        let a = wm.open(0, "A", WORK, 300, 200, 10).unwrap();
        let b = wm.open(1, "B", WORK, 300, 200, 10).unwrap();
        let c = wm.open(2, "C", WORK, 300, 200, 10).unwrap();
        assert_eq!(wm.cycle_backward(), Some(b));
        assert_eq!(wm.cycle_backward(), Some(a));
        let mut order = [0usize; 4];
        let count = wm.z_order(&mut order);
        assert_eq!(&order[..count], &[b, c, a]);
    }

    #[test]
    fn a_rescan_moves_windows_with_their_bundle() {
        let mut wm = WindowManager::<4>::new();
        let index = wm.open(3, "Keep", WORK, 300, 200, 10).unwrap();
        wm.get_mut(index).unwrap().bundle.set("keep");
        let gone = wm.open(4, "Gone", WORK, 300, 200, 10).unwrap();
        wm.get_mut(gone).unwrap().bundle.set("gone");
        wm.remap_apps(|id| (id == "keep").then_some(1));
        assert_eq!(wm.get(index).unwrap().app, 1);
        assert_eq!(wm.get(gone).unwrap().app, NO_APP);
    }

    #[test]
    fn moving_on_a_tiny_work_area_does_not_panic() {
        let tiny = Rect::new(0, 0, 20, 10);
        let mut wm = WindowManager::<4>::new();
        let index = wm.open(0, "One", tiny, 300, 200, 10).unwrap();
        assert!(wm.move_by(index, 5, 5, tiny));
        assert!(wm.move_by(index, -500, -500, tiny));
    }
}
